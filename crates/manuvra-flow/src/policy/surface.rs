//! Unsupported surfaces: file inputs, canvas controls, cross-origin frames, closed shadow roots,
//! and popups, where the policy stops instead of acting.

use super::targeting::{selected_element, selected_hover_region};
use crate::judgment::{Judgments, Operation, selected_operation};
use manuvra_chrome::{FocusAnchor, FocusSurface, Key, Observation};

pub(super) fn selected_surface(
    observation: &Observation,
    judgments: &Judgments,
    operation: Option<Operation>,
) -> Option<&'static str> {
    if operation == Some(Operation::PressKey) {
        key_surface(observation, Key::from_choice(&judgments.key.choice))
    } else {
        unsupported_surface(observation, judgments)
    }
}

/// A selected element or listed hover region is a target on the observed surface, so a surface
/// gap elsewhere on the page does not stop it.
fn unsupported_surface(observation: &Observation, judgments: &Judgments) -> Option<&'static str> {
    let operation = selected_operation(judgments).ok();
    let element =
        operation.and_then(|operation| selected_element(observation, judgments, operation));
    if element.is_some_and(|element| element.input_type.as_deref() == Some("file")) {
        return Some("file_input");
    }
    let targeted = element.is_some() || hover_region_selected(observation, judgments, operation);
    (!targeted)
        .then(|| named_surface_gap(&observation.coverage.gaps))
        .flatten()
}

fn hover_region_selected(
    observation: &Observation,
    judgments: &Judgments,
    operation: Option<Operation>,
) -> bool {
    operation == Some(Operation::Hover) && selected_hover_region(observation, judgments).is_some()
}

pub(super) fn key_surface(observation: &Observation, key: Option<Key>) -> Option<&'static str> {
    let anchor = observation.focus_anchor.as_ref()?;
    anchor_surface(anchor).or_else(|| {
        (matches!(key, Some(Key::Enter | Key::Space)) && focused_file_input(observation, anchor))
            .then_some("file_input")
    })
}

fn focused_file_input(observation: &Observation, anchor: &FocusAnchor) -> bool {
    observation.elements.iter().any(|element| {
        element.node_id == anchor.node_id
            && element.context == anchor.context
            && element.input_type.as_deref() == Some("file")
    })
}

fn anchor_surface(anchor: &FocusAnchor) -> Option<&'static str> {
    match anchor.surface {
        Some(FocusSurface::Canvas) => Some("canvas_control"),
        Some(FocusSurface::CrossOriginFrame) => Some("cross_origin_frame"),
        Some(FocusSurface::ClosedShadowRoot) => Some("closed_shadow_root"),
        None if !anchor.covered => Some("closed_shadow_root"),
        None => None,
    }
}

fn named_surface_gap(gaps: &[String]) -> Option<&'static str> {
    [
        ("cross_origin_frame", "cross_origin_frame"),
        ("closed_shadow_root", "closed_shadow_root"),
        ("canvas", "canvas_control"),
        ("popup", "popup_or_new_tab"),
    ]
    .into_iter()
    .find_map(|(gap, surface)| gaps.iter().any(|actual| actual == gap).then_some(surface))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::policy::tests::support::*;
    use crate::policy::{Next, Policy, PolicyStop};
    use manuvra_contract::JobOptions;

    #[test]
    fn activation_keys_on_a_focused_file_input_are_an_unsupported_surface() {
        let file = file_input_focus();
        for activation in ["Enter", "Space"] {
            let mut key = judgments("PRESS_KEY");
            key.key = choice(activation);
            let mut policy = Policy::new(&JobOptions::default(), "http://example.test/");
            assert!(matches!(
                decide_not_done(&mut policy, &step(), &file, &key, false),
                Next::Stop(PolicyStop::UnsupportedSurface("file_input"))
            ));
            let candidate = policy.caller_candidate(&file, &key).unwrap();
            assert_eq!(
                policy.authorize_caller(&step(), &file, &candidate).err(),
                Some(PolicyStop::UnsupportedSurface("file_input"))
            );
            assert_eq!(policy.actions, 0);
        }
        for other in [
            "Escape",
            "Tab",
            "Shift+Tab",
            "ArrowUp",
            "ArrowDown",
            "ArrowLeft",
            "ArrowRight",
            "Home",
            "End",
        ] {
            let mut key = judgments("PRESS_KEY");
            key.key = choice(other);
            assert!(matches!(
                decide_not_done(
                    &mut Policy::new(&JobOptions::default(), "http://example.test/"),
                    &step(),
                    &file,
                    &key,
                    false
                ),
                Next::Mutate(_)
            ));
        }
        let mut other_context = file_input_focus();
        other_context.focus_anchor.as_mut().unwrap().context = "main/frame:2".into();
        let mut enter = judgments("PRESS_KEY");
        enter.key = choice("Enter");
        assert!(matches!(
            decide_not_done(
                &mut Policy::new(&JobOptions::default(), "http://example.test/"),
                &step(),
                &other_context,
                &enter,
                false
            ),
            Next::Mutate(_)
        ));
    }

    #[test]
    fn unsupported_key_focus_stops_before_the_confidence_gates() {
        let mut canvas = focused("Drawing");
        canvas.focus_anchor.as_mut().unwrap().surface = Some(FocusSurface::Canvas);
        let mut low_operation = judgments("PRESS_KEY");
        low_operation.operation.confidence = 0.69;
        let mut low_key = judgments("PRESS_KEY");
        low_key.key.confidence = 0.69;
        let mut enter = low_key.clone();
        enter.key.choice = "Enter".into();
        for (observed, key) in [
            (&canvas, &low_operation),
            (&canvas, &low_key),
            (&file_input_focus(), &enter),
        ] {
            let mut policy = Policy::new(&JobOptions::default(), "http://example.test/");
            assert!(matches!(
                decide_not_done(&mut policy, &step(), observed, key, false),
                Next::Stop(PolicyStop::UnsupportedSurface(_))
            ));
        }
    }

    #[test]
    fn key_surface_checks_only_the_focus_anchor() {
        for gap in ["canvas", "cross_origin_frame", "closed_shadow_root"] {
            let mut observed = focused("A");
            observed.coverage.gaps.push(gap.into());
            assert!(
                matches!(
                    decide_not_done(
                        &mut Policy::new(&JobOptions::default(), "http://example.test/"),
                        &step(),
                        &observed,
                        &judgments("PRESS_KEY"),
                        false
                    ),
                    Next::Mutate(_)
                ),
                "{gap}"
            );
        }
        let mut popup = focused("A");
        popup.coverage.gaps.push("popup".into());
        assert!(matches!(
            decide_not_done(
                &mut Policy::new(&JobOptions::default(), "http://example.test/"),
                &step(),
                &popup,
                &judgments("PRESS_KEY"),
                false
            ),
            Next::Stop(PolicyStop::UnsupportedSurface("popup_or_new_tab"))
        ));
        let mut policy = Policy::new(&JobOptions::default(), "http://example.test/");
        let mut unsafe_focus = focused("frame");
        unsafe_focus.focus_anchor.as_mut().unwrap().role = "iframe".into();
        unsafe_focus.focus_anchor.as_mut().unwrap().covered = false;
        unsafe_focus.focus_anchor.as_mut().unwrap().surface = Some(FocusSurface::CrossOriginFrame);
        unsafe_focus.coverage.gaps.push("cross_origin_frame".into());
        assert!(matches!(
            decide_not_done(
                &mut policy,
                &step(),
                &unsafe_focus,
                &judgments("PRESS_KEY"),
                false
            ),
            Next::Stop(PolicyStop::UnsupportedSurface("cross_origin_frame"))
        ));
        for (surface, reason) in [
            (FocusSurface::Canvas, "canvas_control"),
            (FocusSurface::ClosedShadowRoot, "closed_shadow_root"),
        ] {
            let mut anchor = focused("A");
            anchor.focus_anchor.as_mut().unwrap().surface = Some(surface);
            assert!(matches!(
                decide_not_done(&mut policy, &step(), &anchor, &judgments("PRESS_KEY"), false),
                Next::Stop(PolicyStop::UnsupportedSurface(actual)) if actual == reason
            ));
        }
        let mut uncovered = focused("A");
        uncovered.focus_anchor.as_mut().unwrap().covered = false;
        assert!(matches!(
            decide_not_done(
                &mut policy,
                &step(),
                &uncovered,
                &judgments("PRESS_KEY"),
                false
            ),
            Next::Stop(PolicyStop::UnsupportedSurface("closed_shadow_root"))
        ));
        let mut no_focus = focused("A");
        no_focus.focus_anchor = None;
        assert!(matches!(
            decide_not_done(
                &mut Policy::new(&JobOptions::default(), "http://example.test/"),
                &step(),
                &no_focus,
                &judgments("PRESS_KEY"),
                false
            ),
            Next::Mutate(_)
        ));
    }

    #[test]
    fn unsupported_surfaces_are_named_and_popup_is_stopped_immediately() {
        let mut file = observation("TYPE_TEXT", "textbox");
        file.elements[0].input_type = Some("file".into());
        let mut policy = Policy::new(&JobOptions::default(), "http://example.test/");
        assert!(matches!(
            decide_not_done(&mut policy, &step(), &file, &judgments("TYPE_TEXT"), false),
            Next::Stop(PolicyStop::UnsupportedSurface("file_input"))
        ));

        for (gap, expected) in [
            ("cross_origin_frame", "cross_origin_frame"),
            ("closed_shadow_root", "closed_shadow_root"),
            ("canvas", "canvas_control"),
        ] {
            let mut observation = observation("CLICK", "button");
            observation.elements.clear();
            observation.coverage.gaps = vec![gap.into()];
            let mut policy = Policy::new(&JobOptions::default(), "http://example.test/");
            assert!(matches!(
                decide_not_done(
                    &mut policy,
                    &step(),
                    &observation,
                    &judgments("CLICK"),
                    false
                ),
                Next::Stop(PolicyStop::UnsupportedSurface(surface)) if surface == expected
            ));
        }

        let mut popup = observation("CLICK", "button");
        popup.coverage.gaps = vec!["popup".into()];
        let policy = Policy::new(&JobOptions::default(), "http://example.test/");
        assert_eq!(
            policy.check_origin(&popup),
            Err(PolicyStop::UnsupportedSurface("popup_or_new_tab"))
        );
    }

    #[test]
    fn a_listed_hover_target_is_not_stopped_by_a_canvas_gap() {
        let mut canvas = hover_page();
        canvas.elements.clear();
        canvas.coverage.gaps = vec!["canvas".into()];
        let mut policy = Policy::new(&JobOptions::default(), "http://example.test/");
        minted(decide_not_done(
            &mut policy,
            &step(),
            &canvas,
            &hover(Some("R1")),
            false,
        ));
        for unlisted in [None, Some("R9")] {
            let mut policy = Policy::new(&JobOptions::default(), "http://example.test/");
            assert!(matches!(
                decide_not_done(&mut policy, &step(), &canvas, &hover(unlisted), false),
                Next::Stop(PolicyStop::UnsupportedSurface("canvas_control"))
            ));
        }
        let mut click = judgments("CLICK");
        click.hover_target = Some(choice("R1"));
        let mut policy = Policy::new(&JobOptions::default(), "http://example.test/");
        assert!(matches!(
            decide_not_done(&mut policy, &step(), &canvas, &click, false),
            Next::Stop(PolicyStop::UnsupportedSurface("canvas_control"))
        ));
    }
}
