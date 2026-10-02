//! Replay keys: the stable identity of an operation on an observation, so the ledger refuses the
//! same mutation twice and the same key press on an unsettled focus identity.

use super::{Candidate, Policy};
use crate::judgment::Operation;
use manuvra_chrome::{Element, FocusAnchor, HoverRegion, Observation};
use sha2::{Digest, Sha256};

impl Policy {
    pub(super) fn candidate_replay_key(
        &self,
        observation: &Observation,
        candidate: &Candidate,
    ) -> Option<String> {
        match candidate.operation {
            Operation::ScrollUp | Operation::ScrollDown => {
                Some(self.scroll_key(observation, candidate.operation))
            }
            Operation::Hover => candidate
                .hover_region(observation)
                .zip(candidate.hover_target.as_ref())
                .map(|(region, target)| hover_replay_key(observation, region, &target.reveal)),
            Operation::PressKey => {
                let state = candidate.focus_anchor.as_ref().map(focus_state);
                Some(self.key_digest(observation, candidate, state))
            }
            _ => candidate
                .target_index
                .and_then(|index| observation.elements.iter().find(|item| item.index == index))
                .map(|target| replay_key(observation, target, candidate)),
        }
    }

    fn scroll_key(&self, observation: &Observation, operation: Operation) -> String {
        let key = scroll_replay_key(observation, operation);
        match self.scroll_attempts.get(&key) {
            Some(ordinal) => format!("{key}:{ordinal}"),
            None => key,
        }
    }

    /// The focus identity of a key press, without the anchor state: while a press with this
    /// identity is unsettled, the same key on the same focus is refused.
    pub(super) fn key_identity(
        &self,
        observation: &Observation,
        candidate: &Candidate,
    ) -> Option<String> {
        (candidate.operation == Operation::PressKey)
            .then(|| self.key_digest(observation, candidate, None))
    }

    /// Key press replay keys carry the step ordinal, so the same key on the same focus is
    /// refused only within one step.
    fn key_digest(
        &self,
        observation: &Observation,
        candidate: &Candidate,
        state: Option<serde_json::Value>,
    ) -> String {
        let stable = serde_json::json!({
            "operation":candidate.operation,"key":candidate.key,"route":observation.route,
            "step":self.step,"focus":candidate.focus_anchor.as_ref().map(focus_identity),
            "state":state,
        });
        hex::encode(Sha256::digest(stable.to_string().as_bytes()))
    }
}

fn replay_key(observation: &Observation, target: &Element, candidate: &Candidate) -> String {
    let mut stable = serde_json::json!({
        "operation":candidate.operation,"target_role":target.role,
        "target_name":target.name.to_ascii_lowercase(),"dialog":target.in_dialog,
        "input_type":target.input_type,"value_name":candidate.value_name,
        "route":observation.route,
    });
    if let Some(container) = &target.container {
        stable["container"] = container.to_ascii_lowercase().into();
    }
    hex::encode(Sha256::digest(stable.to_string().as_bytes()))
}

/// The same region in an unchanged region list is one hover; any change to the list's names or
/// reveals makes it a new one.
fn hover_replay_key(observation: &Observation, region: &HoverRegion, reveal: &str) -> String {
    let listed: Vec<_> = observation
        .hover_regions
        .iter()
        .map(|item| serde_json::json!({"name":item.name,"reveals_on_hover":item.reveals_on_hover}))
        .collect();
    let stable = serde_json::json!({
        "operation":Operation::Hover,
        "route":observation.route,
        "region_name":region.name,
        "reveal":reveal,
        "reveals_on_hover":region.reveals_on_hover,
        "hover_regions":hex::encode(Sha256::digest(serde_json::Value::from(listed).to_string().as_bytes())),
    });
    hex::encode(Sha256::digest(stable.to_string().as_bytes()))
}

fn scroll_replay_key(observation: &Observation, operation: Operation) -> String {
    let mut stable = serde_json::json!({
        "operation":operation,
        "route":observation.route,
        "scroll_y":observation.viewport.scroll_y,
        "document_height":observation.viewport.document_height,
    });
    if let super::scroll::ScrollRoute::Region(region) = super::scroll::route(observation, operation)
    {
        stable["scroll_target"] = serde_json::json!(region.target());
        stable["positions"] = super::scroll::positions(observation, region);
    }
    hex::encode(Sha256::digest(stable.to_string().as_bytes()))
}

fn focus_identity(anchor: &FocusAnchor) -> serde_json::Value {
    let mut identity = serde_json::json!({
        "role":anchor.role,
        "name":anchor.name.to_ascii_lowercase(),
        "dialog":anchor.in_dialog,
        "position":anchor.position,
    });
    if let Some(container) = &anchor.container {
        identity["container"] = container.to_ascii_lowercase().into();
    }
    identity
}

/// Anchor state a legitimate repeated key press changes, such as the active option of a
/// listbox. It distinguishes repeated presses only while no earlier press on the same focus
/// identity is unsettled.
fn focus_state(anchor: &FocusAnchor) -> serde_json::Value {
    serde_json::json!({
        "active_descendant":anchor.active_descendant,
        "expanded":anchor.expanded,
        "selected":anchor.selected,
        "checked":anchor.checked,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::judgment::Judgments;
    use crate::policy::tests::support::*;
    use crate::policy::{Next, PolicyStop};
    use crate::test_support::hover_region as region;
    use manuvra_contract::{JobOptions, Step};

    #[test]
    fn element_replay_without_a_container_keeps_the_legacy_digest() {
        let page = observation("CLICK", "button");
        let policy = Policy::new(&JobOptions::default(), "http://example.test/");
        let candidate = policy.caller_candidate(&page, &judgments("CLICK")).unwrap();
        assert_eq!(
            replay_key(&page, &page.elements[0], &candidate),
            "3e310faef2ed4aaa8b1bb628a34eca1114162ee58242d336a8f7a685360c7f47"
        );
    }

    #[test]
    fn element_replay_distinguishes_containers_and_forbids_each_repeat() {
        for labels in [
            [Some("Alpha"), Some("Bravo")],
            [None, None],
            [Some("Step 1: Account"), Some("Step 2: Details")],
        ] {
            let mut policy = Policy::new(&JobOptions::default(), "http://example.test/");
            let mut page = observation("CLICK", "button");
            page.elements[0].container = labels[0].map(str::to_owned);
            minted(decide_not_done(
                &mut policy,
                &step(),
                &page,
                &judgments("CLICK"),
                false,
            ));
            policy.begin_step();
            page.elements[0].container = labels[1].map(str::to_owned);
            let next = decide_not_done(&mut policy, &step(), &page, &judgments("CLICK"), false);
            if labels[0] == labels[1] {
                assert!(matches!(
                    next,
                    Next::Stop(PolicyStop::Uncertain("replay_forbidden"))
                ));
            } else {
                minted(next);
                assert!(matches!(
                    decide_not_done(&mut policy, &step(), &page, &judgments("CLICK"), false),
                    Next::Stop(PolicyStop::Uncertain("replay_forbidden"))
                ));
            }
        }
    }

    #[test]
    fn focus_identity_distinguishes_same_named_controls_in_different_containers() {
        let mut page = focused("Copy");
        let anchor = page.focus_anchor.as_mut().unwrap();
        let legacy = focus_identity(anchor);
        assert!(legacy.get("container").is_none());
        anchor.container = Some("Staging".into());
        let staging = focus_identity(anchor);
        assert_eq!(staging["container"], "staging");
        anchor.container = Some("Production".into());
        assert_ne!(focus_identity(anchor), staging);
    }

    #[test]
    fn region_scroll_replay_key_tracks_target_ancestors_and_window_positions() {
        let mut page = observation("CLICK", "button");
        let legacy = scroll_replay_key(&page, Operation::ScrollDown);
        page.scroll_regions=serde_json::from_value(serde_json::json!([
            {"node_id":1,"name":"Body","overlay":null,"parent_node_id":null,"can_scroll_up":false,"can_scroll_down":false,"scroll_top":700,"scroll_height":1000,"client_height":300,"rect":{"x":0,"y":0,"width":200,"height":300}},
            {"node_id":2,"name":"List","overlay":null,"parent_node_id":1,"can_scroll_up":false,"can_scroll_down":true,"scroll_top":0,"scroll_height":1000,"client_height":300,"rect":{"x":0,"y":0,"width":200,"height":300}}
        ])).unwrap();
        let key = scroll_replay_key(&page, Operation::ScrollDown);
        assert_ne!(key, legacy);
        assert_eq!(key, scroll_replay_key(&page, Operation::ScrollDown));
        for index in 0..2 {
            let mut changed = page.clone();
            changed.scroll_regions[index].scroll_top += 1.0;
            assert_ne!(key, scroll_replay_key(&changed, Operation::ScrollDown));
        }
        let mut changed = page.clone();
        changed.viewport.scroll_y = 1.0;
        assert_ne!(key, scroll_replay_key(&changed, Operation::ScrollDown));
        page.viewport.document_height = 1000.0;
        let with_regions = scroll_replay_key(&page, Operation::ScrollDown);
        page.scroll_regions.clear();
        assert_eq!(
            with_regions,
            scroll_replay_key(&page, Operation::ScrollDown)
        );
    }

    #[test]
    fn an_uncertain_scroll_does_not_reopen_an_observed_other_direction() {
        let mut page = observation("CLICK", "button");
        page.viewport.document_height = 2000.0;
        page.viewport.scroll_y = 100.0;
        let mut policy = Policy::new(&JobOptions::default(), &page.url);
        let down = minted(decide_not_done(
            &mut policy,
            &step(),
            &page,
            &judgments("SCROLL_DOWN"),
            false,
        ));
        policy.record_observed(&down.replay_key);
        let up = minted(decide_not_done(
            &mut policy,
            &step(),
            &page,
            &judgments("SCROLL_UP"),
            false,
        ));
        policy.record_uncertain_scroll(&up.replay_key);
        assert!(matches!(
            decide_not_done(
                &mut policy,
                &step(),
                &page,
                &judgments("SCROLL_DOWN"),
                false
            ),
            Next::Stop(PolicyStop::Uncertain("replay_forbidden"))
        ));
        let retried = minted(decide_not_done(
            &mut policy,
            &step(),
            &page,
            &judgments("SCROLL_UP"),
            false,
        ));
        assert_ne!(retried.replay_key, up.replay_key);
        policy.record_observed(&retried.replay_key);
        assert!(matches!(
            decide_not_done(&mut policy, &step(), &page, &judgments("SCROLL_UP"), false),
            Next::Stop(PolicyStop::Uncertain("replay_forbidden"))
        ));
    }

    /// How a minted key permit settles before the next beat.
    enum Settle {
        Observed,
        Unsettled,
        NotPerformed,
        Unused,
    }

    #[derive(Debug, PartialEq)]
    enum Expect {
        Minted,
        Forbidden,
        Exhausted,
    }

    /// One beat of a replay script.
    enum Beat {
        Press(Observation, &'static str, Settle, Expect),
        Caller(Observation, &'static str, Settle, Expect),
        Click(Observation, Expect),
        Wait(Observation),
        /// Releases the replay key of the permit minted at this position as not performed.
        Release(usize),
        BeginStep,
    }

    struct KeyScript {
        name: &'static str,
        options: JobOptions,
        mutation_limit: u8,
        beats: Vec<Beat>,
        actions: u16,
    }

    fn pressed(key: &str) -> Judgments {
        let mut press = judgments("PRESS_KEY");
        press.key = choice(key);
        press
    }

    fn settle(
        policy: &mut Policy,
        next: Next,
        settle: Settle,
        minted: &mut Vec<String>,
        label: &str,
    ) -> Expect {
        match next {
            Next::Mutate(permit) => {
                minted.push(permit.replay_key.clone());
                match settle {
                    Settle::Observed => policy.record_observed(&permit.replay_key),
                    Settle::Unsettled => {}
                    Settle::NotPerformed => policy.release_not_performed(&permit.replay_key),
                    Settle::Unused => {
                        policy.release_unused(permit);
                    }
                }
                Expect::Minted
            }
            Next::Stop(PolicyStop::Uncertain("replay_forbidden")) => Expect::Forbidden,
            Next::Stop(PolicyStop::Blocked("budget_exhausted")) => Expect::Exhausted,
            other => panic!("{label}: {other:?}"),
        }
    }

    fn play(script: KeyScript) {
        let mut policy = Policy::new(&script.options, "http://example.test/");
        let step = Step {
            mutation_limit: script.mutation_limit,
            ..step()
        };
        let mut minted: Vec<String> = Vec::new();
        for (position, beat) in script.beats.into_iter().enumerate() {
            let label = format!("{} beat {position}", script.name);
            let (next, how, expect) = match beat {
                Beat::Press(page, key, how, expect) => (
                    decide_not_done(&mut policy, &step, &page, &pressed(key), false),
                    how,
                    expect,
                ),
                Beat::Caller(page, key, how, expect) => {
                    let candidate = policy.caller_candidate(&page, &pressed(key)).unwrap();
                    let next = policy
                        .authorize_caller(&step, &page, &candidate)
                        .map_or_else(Next::Stop, |permit| Next::Mutate(Box::new(permit)));
                    (next, how, expect)
                }
                Beat::Click(page, expect) => (
                    decide_not_done(&mut policy, &step, &page, &judgments("CLICK"), false),
                    Settle::Observed,
                    expect,
                ),
                Beat::Wait(page) => {
                    let next =
                        decide_not_done(&mut policy, &step, &page, &judgments("WAIT"), false);
                    assert!(matches!(next, Next::Wait), "{label}");
                    continue;
                }
                Beat::Release(position) => {
                    policy.release_not_performed(&minted[position]);
                    continue;
                }
                Beat::BeginStep => {
                    policy.begin_step();
                    continue;
                }
            };
            assert_eq!(
                settle(&mut policy, next, how, &mut minted, &label),
                expect,
                "{label}"
            );
        }
        assert_eq!(policy.actions, script.actions, "{}", script.name);
    }

    fn anchored(name: &str, change: impl FnOnce(&mut FocusAnchor)) -> Observation {
        let mut page = focused(name);
        change(page.focus_anchor.as_mut().unwrap());
        page
    }

    fn listbox(active: Option<(&str, &str)>, change: impl FnOnce(&mut FocusAnchor)) -> Observation {
        anchored("Choose item", |anchor| {
            anchor.role = "combobox".into();
            anchor.active_descendant = active.map(|(id, name)| manuvra_chrome::ActiveDescendant {
                id: id.into(),
                role: "option".into(),
                name: name.into(),
                selected: Some(false),
                checked: None,
            });
            change(anchor);
        })
    }

    #[test]
    fn key_replay_follows_focus_identity_state_settlement_and_step() {
        use Beat::{BeginStep, Caller, Click, Press, Release, Wait};
        use Expect::{Exhausted, Forbidden, Minted};
        use Settle::{NotPerformed, Observed, Unsettled, Unused};
        let a = || focused("A");
        let beta = || Some(("beta", "Beta"));
        let checkbox = |checked: bool, expanded: Option<bool>| {
            anchored("Accept terms", |anchor| {
                anchor.role = "checkbox".into();
                anchor.checked = Some(checked);
                anchor.expanded = expanded;
            })
        };
        let roving = |position| {
            anchored("", |anchor| {
                anchor.role = "treeitem".into();
                anchor.position = Some(position);
            })
        };
        let menu = |expanded| anchored("Menu", |anchor| anchor.expanded = Some(expanded));
        let popover = || focused("Breakdown");
        let opened_popover = || anchored("Breakdown", |anchor| anchor.expanded = Some(true));
        let mut aria_click = observation("CLICK", "button");
        aria_click.focus_anchor = listbox(beta(), |anchor| {
            anchor.expanded = Some(false);
            anchor.checked = Some(true);
        })
        .focus_anchor;
        let scripts = [
            KeyScript {
                name: "caller authority shares the replay ledger",
                options: JobOptions::default(),
                mutation_limit: 2,
                beats: vec![
                    Caller(a(), "Escape", Observed, Minted),
                    Caller(a(), "Escape", Observed, Forbidden),
                    Press(a(), "Escape", Observed, Forbidden),
                ],
                actions: 1,
            },
            KeyScript {
                name: "presses consume the step limit and a release refunds it",
                options: JobOptions::default(),
                mutation_limit: 2,
                beats: vec![
                    Wait(a()),
                    Press(a(), "Escape", Unsettled, Minted),
                    Press(a(), "Escape", Unsettled, Forbidden),
                    Press(focused("B"), "Escape", Unsettled, Minted),
                    Press(focused("C"), "Escape", Unsettled, Exhausted),
                    Release(0),
                    Press(a(), "Escape", Unsettled, Minted),
                ],
                actions: 3,
            },
            KeyScript {
                name: "presses are bounded by the run's actions",
                options: JobOptions {
                    max_actions: Some(1),
                    ..JobOptions::default()
                },
                mutation_limit: 2,
                beats: vec![
                    Press(a(), "Escape", Unsettled, Minted),
                    Press(focused("B"), "Escape", Unsettled, Exhausted),
                ],
                actions: 1,
            },
            KeyScript {
                name: "active descendant and ARIA state distinguish presses, not clicks",
                options: JobOptions::default(),
                mutation_limit: 8,
                beats: vec![
                    Press(listbox(None, |_| {}), "ArrowDown", Observed, Minted),
                    Press(listbox(None, |_| {}), "ArrowDown", Observed, Forbidden),
                    Press(
                        listbox(Some(("alpha", "Alpha")), |_| {}),
                        "ArrowDown",
                        Observed,
                        Minted,
                    ),
                    Press(listbox(beta(), |_| {}), "ArrowDown", Observed, Minted),
                    Press(listbox(beta(), |_| {}), "ArrowDown", Observed, Forbidden),
                    Press(
                        listbox(beta(), |anchor| {
                            anchor.active_descendant.as_mut().unwrap().selected = Some(true)
                        }),
                        "ArrowDown",
                        Observed,
                        Minted,
                    ),
                    Press(
                        listbox(beta(), |anchor| anchor.expanded = Some(false)),
                        "ArrowDown",
                        Observed,
                        Minted,
                    ),
                    Press(
                        listbox(beta(), |anchor| {
                            anchor.expanded = Some(false);
                            anchor.checked = Some(true);
                        }),
                        "ArrowDown",
                        Observed,
                        Minted,
                    ),
                    Click(observation("CLICK", "button"), Minted),
                    Click(aria_click, Forbidden),
                ],
                actions: 7,
            },
            KeyScript {
                name: "unnamed roving items are distinguished by position",
                options: JobOptions::default(),
                mutation_limit: 8,
                beats: vec![
                    Press(roving(1), "ArrowDown", Observed, Minted),
                    Press(roving(2), "ArrowDown", Observed, Minted),
                    Press(roving(2), "ArrowDown", Observed, Forbidden),
                ],
                actions: 2,
            },
            KeyScript {
                name: "an unsettled press forbids the same key on the same focus whatever its state",
                options: JobOptions::default(),
                mutation_limit: 8,
                beats: vec![
                    Press(checkbox(false, None), "Space", Unsettled, Minted),
                    Press(checkbox(true, Some(true)), "Space", Unsettled, Forbidden),
                    Caller(checkbox(true, Some(true)), "Space", Unsettled, Forbidden),
                    Press(checkbox(true, Some(true)), "Tab", Observed, Minted),
                    Press(
                        anchored("Subscribe", |anchor| {
                            anchor.role = "checkbox".into();
                            anchor.checked = Some(true);
                            anchor.expanded = Some(true);
                        }),
                        "Space",
                        Observed,
                        Minted,
                    ),
                ],
                actions: 3,
            },
            KeyScript {
                name: "a press proven not performed releases its unsettled focus identity",
                options: JobOptions::default(),
                mutation_limit: 2,
                beats: vec![
                    Press(menu(false), "Enter", NotPerformed, Minted),
                    Press(menu(true), "Enter", Unused, Minted),
                    Press(menu(false), "Enter", Unsettled, Minted),
                ],
                actions: 2,
            },
            KeyScript {
                name: "key replay is scoped to the step",
                options: JobOptions::default(),
                mutation_limit: 8,
                beats: vec![
                    BeginStep,
                    Press(popover(), "Escape", Observed, Minted),
                    Press(popover(), "Escape", Observed, Forbidden),
                    Press(popover(), "Space", Unsettled, Minted),
                    Press(opened_popover(), "Space", Unsettled, Forbidden),
                    BeginStep,
                    Press(popover(), "Escape", Observed, Minted),
                    Press(opened_popover(), "Space", Observed, Minted),
                    Press(popover(), "Escape", Observed, Forbidden),
                ],
                actions: 4,
            },
        ];
        for script in scripts {
            play(script);
        }
    }

    #[test]
    fn replay_ledger_is_global_across_steps_for_the_same_submit_semantics() {
        let mut policy = Policy::new(&JobOptions::default(), "http://example.test/");
        let first_step = step();
        let mut later_step = step();
        later_step.id = "confirm-again".into();
        let first = decide_not_done(
            &mut policy,
            &first_step,
            &observation("CLICK", "button"),
            &judgments("CLICK"),
            false,
        );
        assert!(matches!(first, Next::Mutate(_)));
        policy.begin_step();
        let mut remounted = observation("CLICK", "button");
        remounted.document_id = "new-document-token".into();
        remounted.elements[0].node_id = 999;
        assert!(matches!(
            decide_not_done(
                &mut policy,
                &later_step,
                &remounted,
                &judgments("CLICK"),
                false
            ),
            Next::Stop(PolicyStop::Uncertain("replay_forbidden"))
        ));
    }

    #[test]
    fn hover_replay_is_refused_for_an_unchanged_region_list_even_in_a_later_step() {
        let mut policy = Policy::new(&JobOptions::default(), "http://example.test/");
        let later = Step {
            id: "later".into(),
            ..step()
        };
        minted(decide_not_done(
            &mut policy,
            &step(),
            &hover_page(),
            &hover(Some("R1_1")),
            false,
        ));
        assert!(matches!(
            decide_not_done(
                &mut policy,
                &step(),
                &hover_page(),
                &hover(Some("R1_1")),
                false
            ),
            Next::Stop(PolicyStop::Uncertain("replay_forbidden"))
        ));
        policy.begin_step();
        let mut remounted = hover_page();
        remounted.document_id = "new-document".into();
        remounted.hover_regions[0].node_id = 99;
        remounted.hover_regions[0].reveal_node_ids[0] = 99;
        assert!(matches!(
            decide_not_done(&mut policy, &later, &remounted, &hover(Some("R1_1")), false),
            Next::Stop(PolicyStop::Uncertain("replay_forbidden"))
        ));
        let mut changed = hover_page();
        changed.hover_regions.push(region(3, "Utilities", 43));
        minted(decide_not_done(
            &mut policy,
            &later,
            &changed,
            &hover(Some("R1_1")),
            false,
        ));
    }
    #[test]
    fn different_controls_in_one_region_have_distinct_hover_replay_keys() {
        let mut page = hover_page();
        page.hover_regions[0].reveals_on_hover.push("Delete".into());
        page.hover_regions[0].reveal_roles.push("button".into());
        page.hover_regions[0].reveal_node_ids.push(99);
        let mut policy = Policy::new(&JobOptions::default(), "http://example.test/");
        let first = minted(decide_not_done(
            &mut policy,
            &step(),
            &page,
            &hover(Some("R1_1")),
            false,
        ));
        policy.record_observed(&first.replay_key);
        let second = minted(decide_not_done(
            &mut policy,
            &step(),
            &page,
            &hover(Some("R1_2")),
            false,
        ));
        assert_ne!(first.replay_key, second.replay_key);
        assert_eq!(second.consume().0.hover_target.unwrap().reveal, "Delete");
    }
}
