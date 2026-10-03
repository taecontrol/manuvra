//! Escalations: the payload a run publishes when it stops `uncertain` on a step or on final
//! verification, and the dispositions each one allows.

use super::artifacts::{PendingEscalation, RunArtifacts};
use super::capture::redacted_value;
use super::disposition::natural_condition_numeric_checks_satisfied;
use super::stops::{Stop, step_detail};
use crate::evidence::Redactor;
use crate::verification::DoneResult;
use crate::{judgment, policy};
use manuvra_chrome::Observation;
use manuvra_contract::{DispositionKind, DoneCondition, Escalation, StepVerdict, VerdictResult};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::time::{SystemTime, UNIX_EPOCH};

#[allow(clippy::too_many_arguments)]
pub(super) fn escalate(
    artifacts: &mut RunArtifacts,
    redactor: &Redactor,
    index: usize,
    step: &manuvra_contract::Step,
    done: DoneResult,
    judgments: Option<&judgment::Judgments>,
    pending: PendingEscalation,
    reason: &'static str,
) -> Stop {
    let id = format!("e_{}", artifacts.escalations.len() + 1);
    let stopped = stopped_at();
    let offered_candidate = pending.candidate.as_ref().map(policy::Candidate::offered);
    let mut candidates = offered_candidate.as_ref().map_or_else(
        || judgments.map(|value|json!({"operation":value.operation,"click_target":value.click_target,"type_target":value.type_target,"select_target":value.select_target,"type_value":value.type_value,"key":value.key})).unwrap_or_else(||json!({})),
        |candidate| json!([candidate]),
    );
    if reason == "target_below_gate"
        && let Some(judgments) = judgments
    {
        candidates["contenders"] = resolved_click_contenders(&pending.observation, judgments);
    }
    let latest = latest_observation(artifacts);
    let decision = judgments.and_then(|_| {
        artifacts
            .decisions
            .last()
            .map(|(name, _)| format!("decisions/{name}.json"))
    });
    let payload = redacted_value(
        &json!({"id":id,"phase":"step","step_id":step.id,"step":{"goal":step.goal,"done_when":step.done_when},"done":done,"observation":latest,"decision":decision,"gate_reason":reason,"candidates":candidates,"offered_candidate":offered_candidate,"permitted_mutations":["CLICK","TYPE_TEXT","PRESS_KEY"],"recent_actions":artifacts.trace.iter().rev().filter(|event|event.get("event").and_then(Value::as_str).is_some_and(|event|event.starts_with("action_"))).take(8).collect::<Vec<_>>(),"stopped_at":stopped}),
        redactor,
    );
    artifacts.escalations.push((id.clone(), payload));
    let dispositions = allowed_dispositions(step, &pending);
    artifacts.escalation = Some(Escalation {
        id: id.clone(),
        phase: "step".into(),
        step_id: Some(redactor.redact_export_text(&step.id)),
        expires_at: stopped,
        payload: format!("escalations/{id}.json"),
        dispositions,
    });
    artifacts.pending = Some(pending);
    artifacts.verdicts[index] = StepVerdict {
        id: redactor.redact_export_text(&step.id),
        result: VerdictResult::Unresolved,
        basis: None,
    };
    Stop::uncertain(reason, step_detail(redactor, step))
}

fn resolved_click_contenders(observation: &Observation, judgments: &judgment::Judgments) -> Value {
    judgments.click_target.probabilities.iter().filter_map(|(key, probability)| {
        let mut contender = match crate::policy::click_choice(observation, key)? {
            crate::policy::ClickChoice::Element(element) => json!({"role":element.role,"name":element.name,"container":element.container}),
            crate::policy::ClickChoice::Reveal { region, offset } => json!({"role":region.reveal_roles[offset],"name":region.reveals_on_hover[offset],"container":region.name,"revealed_by_hover":true}),
        };
        contender["key"] = json!(key);
        contender["probability"] = json!(probability);
        Some(contender)
    }).collect()
}

pub(super) fn allowed_dispositions(
    step: &manuvra_contract::Step,
    pending: &PendingEscalation,
) -> Vec<DispositionKind> {
    let mut dispositions = Vec::new();
    if pending.candidate.is_some() {
        dispositions.push(DispositionKind::Execute);
    }
    if matches!(step.done_when, DoneCondition::NaturalLanguage(_))
        && pending.done == DoneResult::Unknown
        && pending.noul.is_some_and(|noul| noul > 0.20)
        && !pending.ambiguous_mutation
        && pending.candidate.is_none()
        && natural_condition_numeric_checks_satisfied(step, pending)
    {
        dispositions.push(DispositionKind::Advance);
    }
    dispositions.extend([DispositionKind::RetryObservation, DispositionKind::Abort]);
    dispositions
}

pub(super) fn escalate_verification(
    artifacts: &mut RunArtifacts,
    redactor: &Redactor,
    reason: &'static str,
) -> Stop {
    let id = format!("e_{}", artifacts.escalations.len() + 1);
    let stopped = stopped_at();
    let observation = latest_observation(artifacts);
    let mut payload = redacted_value(
        &json!({
            "id":id,
            "phase":"verification",
            "observation":observation,
            "verification":"verification/final.json",
            "gate_reason":reason,
            "stopped_at":stopped,
        }),
        redactor,
    );
    // Verdicts already redact caller/page strings while preserving the typed protocol.
    payload["expectations"] = json!(artifacts.expectation_verdicts);
    artifacts.escalations.push((id.clone(), payload));
    artifacts.escalation = Some(Escalation {
        id: id.clone(),
        phase: "verification".into(),
        step_id: None,
        expires_at: stopped,
        payload: format!("escalations/{id}.json"),
        dispositions: verification_dispositions(artifacts),
    });
    Stop::uncertain(reason, BTreeMap::new())
}

fn verification_dispositions(artifacts: &RunArtifacts) -> Vec<DispositionKind> {
    let mut dispositions = Vec::new();
    if artifacts
        .pending_verification
        .as_ref()
        .is_some_and(|pending| pending.attestable())
    {
        dispositions.push(DispositionKind::Advance);
    }
    dispositions.extend([DispositionKind::RetryObservation, DispositionKind::Abort]);
    dispositions
}

/// When the run stopped on an escalation, in Unix milliseconds.
fn stopped_at() -> String {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .to_string()
}

/// The snapshot and screenshot of the latest observation, which an escalation rests on.
fn latest_observation(artifacts: &RunArtifacts) -> Value {
    artifacts
        .observations
        .last()
        .map(|(name, _, png)| {
            json!({
                "snapshot":format!("observations/{name}.json"),
                "screenshot":png.as_ref().map(|_|format!("observations/{name}.png")),
            })
        })
        .unwrap_or(Value::Null)
}

pub(super) fn natural_noul(
    step: &manuvra_contract::Step,
    judgments: &judgment::Judgments,
) -> Option<f64> {
    matches!(step.done_when, DoneCondition::NaturalLanguage(_)).then_some(judgments.step_done)
}

pub(super) fn reissue_escalation(
    artifacts: &mut RunArtifacts,
    redactor: &Redactor,
    index: usize,
    step: &manuvra_contract::Step,
    reason: &'static str,
) -> Stop {
    let pending = artifacts
        .pending
        .clone()
        .unwrap_or_else(|| PendingEscalation {
            done: DoneResult::Unknown,
            noul: None,
            candidate: None,
            observation: empty_observation(),
            ambiguous_mutation: true,
        });
    escalate(
        artifacts,
        redactor,
        index,
        step,
        pending.done,
        None,
        pending,
        reason,
    )
}

fn empty_observation() -> Observation {
    Observation {
        document_id: String::new(),
        url: String::new(),
        route: String::new(),
        title: String::new(),
        dialogs: Vec::new(),
        focused: None,
        focus_anchor: None,
        visible_text: String::new(),
        covered_text: String::new(),
        painted_text: None,
        colors: Vec::new(),
        colors_complete: false,
        color_scopes: Vec::new(),
        dialog_texts: BTreeMap::new(),
        elements: Vec::new(),
        viewport: manuvra_chrome::ViewportState {
            width: 0,
            height: 0,
            scroll_x: 0.0,
            scroll_y: 0.0,
            document_height: 0.0,
        },
        coverage: manuvra_chrome::Coverage::default(),
        hover_regions: Vec::new(),
        scroll_regions: Vec::new(),
        overlay: None,
        scroll_regions_truncated: false,
        hover_regions_truncated: false,
        hover_rules_unreadable: false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::run::tests::support::*;

    #[test]
    fn target_below_gate_reobserves_once_resolves_contenders_and_refuses_execute() {
        let mut job = click_job();
        job.values.insert(
            "row".into(),
            serde_json::from_value(
                json!({"value":"private-row","description":"row","secret":true}),
            )
            .unwrap(),
        );
        let redactor = Redactor::for_job(&job).unwrap();
        let mut page = observed("Plan");
        let mut target = button(1, 7, "Actions for Groceries");
        target.shares_name = true;
        target.container = Some("private-row".into());
        page.elements.push(target);
        let browser = FakeBrowser::new([page]);
        let provider =
            ScriptedProvider::new([Turn::click("1").target_confidence(0.69).confidence(0.4)]);
        let mut machine = super::super::machine::HostedMachine::new(&job, &redactor);
        let mut journal = MemoryJournal::default();
        drive(&mut machine, &browser, &provider, &mut journal);
        assert_eq!(
            machine.artifacts.stop.as_ref().unwrap().code,
            "target_below_gate"
        );
        assert_eq!(provider.calls(), 2);
        assert_eq!(browser.dispatched(), 0);
        assert_eq!(
            machine.artifacts.escalation.as_ref().unwrap().dispositions,
            [DispositionKind::RetryObservation, DispositionKind::Abort]
        );
        assert!(
            machine
                .artifacts
                .pending
                .as_ref()
                .unwrap()
                .candidate
                .is_none()
        );
        let payload = &machine.artifacts.escalations[0].1;
        assert_eq!(payload["offered_candidate"], Value::Null);
        assert_eq!(payload["candidates"]["contenders"][0]["role"], "button");
        assert_eq!(
            payload["candidates"]["contenders"][0]["name"],
            "Actions for Groceries"
        );
        assert!(
            payload["candidates"]["contenders"][0]["container"]
                .as_str()
                .unwrap()
                .contains("<masked:")
        );
        assert!(!payload.to_string().contains("private-row"));
        assert!(
            machine
                .apply(
                    request("e_1", execute("c_1")),
                    &browser,
                    &provider,
                    &mut journal,
                    &manuvra_chrome::InputCancellation::default()
                )
                .is_none()
        );
        assert_eq!(
            machine.artifacts.stop.as_ref().unwrap().code,
            "candidate_not_offered"
        );
        assert_eq!(browser.dispatched(), 0);
    }

    #[test]
    fn low_key_confidence_offers_a_focus_bound_candidate_without_dispatch() {
        let job = key_job(
            "move",
            "Press Tab to focus Second",
            json!([{"focused":"Second"}]),
            1,
        );
        let browser = FakeBrowser::new([key_observation("First")]);
        let provider = ScriptedProvider::new([Turn::key("Tab").key_confidence(0.69)]);

        let artifacts = driven(&job, &browser, &provider, &mut MemoryJournal::default());

        assert_eq!(artifacts.stop.as_ref().unwrap().code, "key_below_gate");
        assert_eq!(provider.calls(), 2);
        assert_eq!(browser.dispatched(), 0);
        let offered = &artifacts.escalations[0].1["offered_candidate"];
        assert_eq!(offered["operation"], "PRESS_KEY");
        assert_eq!(offered["key"], "Tab");
        assert_eq!(offered["focus_anchor"]["name"], "First");
        assert_eq!(offered["focus_anchor"]["role"], "button");
        let exported = offered.to_string();
        assert!(!exported.contains("node_id"));
        assert!(!exported.contains("context"));
    }

    #[test]
    fn below_gate_escalations_permit_only_click_type_text_and_press_key_mutations() {
        let mut page = observed("Plan");
        page.elements.push(button(1, 7, "Actions for Groceries"));
        let artifacts = driven(
            &click_job(),
            &FakeBrowser::new([page]),
            &ScriptedProvider::new([Turn::click("1").confidence(0.5)]),
            &mut MemoryJournal::default(),
        );
        assert_eq!(
            artifacts.stop.as_ref().unwrap().code,
            "operation_below_gate"
        );
        assert_eq!(
            artifacts.escalations[0].1["permitted_mutations"],
            json!(["CLICK", "TYPE_TEXT", "PRESS_KEY"])
        );
    }

    #[test]
    fn hover_below_the_gate_reobserves_once_then_offers_one_hover_candidate() {
        let browser = FakeBrowser::new([plan_before_hover()]);

        let artifacts = driven(
            &click_job(),
            &browser,
            &ScriptedProvider::new([low_hover_turn()]),
            &mut MemoryJournal::default(),
        );

        let stop = artifacts.stop.as_ref().unwrap();
        assert_eq!(stop.code, "operation_below_gate");
        assert_eq!(artifacts.observations.len(), 2);
        assert_eq!(browser.dispatched(), 0);
        let escalation = &artifacts.escalations[0].1;
        let offered = json!({
            "id":"c_1","operation":"HOVER","target_name":null,"target_role":null,
            "target_dialog":null,"target_input_type":null,"value_name":null,
            "hover_target":{"name":"Groceries","reveals_on_hover":["Actions for Groceries"],"reveal":"Actions for Groceries"}
        });
        assert_eq!(escalation["offered_candidate"], offered);
        assert_eq!(escalation["candidates"], json!([offered]));
        let exported = escalation.to_string();
        for identity in ["node_id", "document_id", "target_index", "\"index\""] {
            assert!(!exported.contains(identity), "{identity}");
        }
        assert_eq!(
            artifacts.escalation.as_ref().unwrap().dispositions,
            [
                DispositionKind::Execute,
                DispositionKind::RetryObservation,
                DispositionKind::Abort,
            ]
        );
    }

    #[test]
    fn offered_hover_region_text_is_redacted_in_the_escalation() {
        let mut job = click_job();
        job.values.insert(
            "category".into(),
            manuvra_contract::JobValue {
                value: "Groceries".into(),
                description: "classified category".into(),
                formats: None,
                secret: true,
            },
        );
        let redactor = Redactor::for_job(&job).unwrap();
        let machine = hover_escalation(&job, &redactor);

        let payload = &machine.artifacts.escalations[0].1;
        assert_eq!(payload["offered_candidate"]["operation"], "HOVER");
        let exported = payload.to_string();
        assert!(!exported.contains("Groceries"));
        assert!(!redactor.contains_export_leak(exported.as_bytes()));
    }
    #[test]
    fn resolved_contenders_decode_reveal_keys_alongside_visible_targets() {
        let mut page = plan_before_hover();
        let mut answer = mutation_judgments("CLICK", 1.0);
        answer.click_target.probabilities =
            BTreeMap::from([("R2_1".into(), 0.4), ("1".into(), 0.6)]);
        page.hover_regions[1].reveal_roles[0] = "menuitem".into();
        let contenders = resolved_click_contenders(&page, &answer);
        assert_eq!(
            contenders[1],
            json!({"key":"R2_1","probability":0.4,"role":"menuitem","name":"Actions for Rent","container":"Rent","revealed_by_hover":true})
        );
        assert_eq!(contenders[0]["key"], "1");
    }
    #[test]
    fn an_absent_item_never_offers_a_click_under_caller_authority() {
        let job = click_job();
        let redactor = Redactor::for_job(&job).unwrap();
        let mut page = observed("Alpha");
        let mut target = button(1, 7, "Edit");
        target.container = Some("Alpha".into());
        page.elements.push(target);
        let browser = FakeBrowser::new([page]);
        let provider = ScriptedProvider::new([Turn::click("NO_CLICK_TARGET").confidence(0.4)]);
        let mut machine = super::super::machine::HostedMachine::new(&job, &redactor);
        drive(
            &mut machine,
            &browser,
            &provider,
            &mut MemoryJournal::default(),
        );
        assert_eq!(
            machine.artifacts.stop.as_ref().unwrap().code,
            "click_target_unavailable"
        );
        assert_eq!(browser.dispatched(), 0);
        assert_eq!(
            machine.artifacts.escalation.as_ref().unwrap().dispositions,
            [DispositionKind::RetryObservation, DispositionKind::Abort]
        );
        assert!(
            machine
                .artifacts
                .pending
                .as_ref()
                .unwrap()
                .candidate
                .is_none()
        );
        assert_eq!(
            machine.artifacts.escalations[0].1["offered_candidate"],
            Value::Null
        );
    }
}
