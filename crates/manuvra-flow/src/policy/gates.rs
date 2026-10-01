//! The decision gates before authorization: done-first, the per-operation confidence gate, and
//! the key choice's validity and gate.

use super::surface::selected_surface;
use super::targeting::selected_target;
use super::{Next, PolicyStop};
use crate::judgment::{Judgments, Operation, selected_operation};
use crate::verification::DoneResult;
use manuvra_chrome::{Key, Observation};
use manuvra_contract::{DoneCondition, Step};

/// The operation confidence the chosen operation must reach, proportional to the cost of a wrong
/// choice; a key press's key choice must reach the same gate as the press. `HOVER` is
/// non-mutating, bounded by the fallback budget and the run-wide replay ledger, and revalidated
/// before dispatch, while acting on the control it reveals still needs 0.70.
fn gate(operation: Operation) -> f64 {
    match operation {
        Operation::Hover => 0.60,
        _ => 0.70,
    }
}

fn operation_gate(
    judgments: &Judgments,
    operation: Operation,
    already_reobserved: bool,
) -> Option<Next> {
    (judgments.operation.confidence < gate(operation)).then_some({
        if already_reobserved {
            Next::Stop(PolicyStop::Uncertain("operation_below_gate"))
        } else {
            Next::ReobserveOperation
        }
    })
}

pub(super) fn ready_operation(
    observation: &Observation,
    judgments: &Judgments,
    already_reobserved: bool,
) -> Result<Operation, Next> {
    let operation = selected_operation(judgments).ok();
    if let Some(surface) = selected_surface(observation, judgments, operation) {
        return Err(Next::Stop(PolicyStop::UnsupportedSurface(surface)));
    }
    let operation =
        operation.ok_or(Next::Stop(PolicyStop::Blocked("provider_invalid_response")))?;
    target_preflight(observation, judgments, operation, already_reobserved)?;
    if let Some(next) = operation_gate(judgments, operation, already_reobserved) {
        return Err(next);
    }
    if operation == Operation::PressKey
        && let Some(next) = key_preflight(judgments, already_reobserved)
    {
        return Err(next);
    }
    Ok(operation)
}

fn target_preflight(
    observation: &Observation,
    judgments: &Judgments,
    operation: Operation,
    already_reobserved: bool,
) -> Result<(), Next> {
    if operation != Operation::Click {
        return Ok(());
    }
    selected_target(observation, judgments, operation).map_err(Next::Stop)?;
    target_gate(observation, judgments, already_reobserved).map_or(Ok(()), Err)
}

fn target_gate(
    observation: &Observation,
    judgments: &Judgments,
    already_reobserved: bool,
) -> Option<Next> {
    (crate::contest::contested(observation) && judgments.click_target.confidence < 0.70).then_some(
        if already_reobserved {
            Next::Stop(PolicyStop::Uncertain("target_below_gate"))
        } else {
            Next::ReobserveOperation
        },
    )
}

fn key_preflight(judgments: &Judgments, already_reobserved: bool) -> Option<Next> {
    if Key::from_choice(&judgments.key.choice).is_none() {
        return Some(Next::Stop(PolicyStop::Blocked("provider_invalid_response")));
    }
    key_gate(judgments, already_reobserved)
}

fn key_gate(judgments: &Judgments, already_reobserved: bool) -> Option<Next> {
    (judgments.key.confidence < gate(Operation::PressKey)).then_some(if already_reobserved {
        Next::Stop(PolicyStop::Uncertain("key_below_gate"))
    } else {
        Next::ReobserveOperation
    })
}

pub fn done_first(step: &Step, done: DoneResult, already_reobserved: bool) -> Option<Next> {
    match done {
        DoneResult::Satisfied => Some(Next::Complete),
        DoneResult::Unknown if already_reobserved => {
            let reason = match &step.done_when {
                DoneCondition::NaturalLanguage(_) => "done_uncertain",
                DoneCondition::Structured(_) => "done_unknown",
            };
            Some(Next::Stop(PolicyStop::Uncertain(reason)))
        }
        DoneResult::Unknown => Some(Next::ReobserveDone),
        DoneResult::NotSatisfied => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::policy::Policy;
    use crate::policy::tests::support::*;
    use manuvra_contract::JobOptions;
    use serde_json::Value;
    use std::collections::BTreeMap;

    #[test]
    fn contested_click_target_gate_precedes_operation_gate_and_preserves_invalid_targets() {
        let mut page = observation("CLICK", "button");
        page.elements[0].shares_name = true;
        let mut click = judgments("CLICK");
        click.click_target.confidence = 0.69;
        click.operation.confidence = 0.40;
        let mut policy = Policy::new(&JobOptions::default(), "http://example.test/");
        assert!(matches!(
            decide_not_done(&mut policy, &step(), &page, &click, false),
            Next::ReobserveOperation
        ));
        assert!(matches!(
            decide_not_done(&mut policy, &step(), &page, &click, true),
            Next::Stop(PolicyStop::Uncertain("target_below_gate"))
        ));
        click.click_target.choice = "999".into();
        assert!(matches!(
            decide_not_done(&mut policy, &step(), &page, &click, true),
            Next::Stop(PolicyStop::Blocked("provider_invalid_response"))
        ));
        click.click_target.choice = "1".into();
        click.click_target.confidence = 0.70;
        assert!(matches!(
            decide_not_done(&mut policy, &step(), &page, &click, true),
            Next::Stop(PolicyStop::Uncertain("operation_below_gate"))
        ));
        click.operation.confidence = 0.70;
        minted(decide_not_done(&mut policy, &step(), &page, &click, true));
    }

    #[test]
    fn target_gate_leaves_uncontested_clicks_and_contested_type_and_select_ungated() {
        let mut click = judgments("CLICK");
        click.click_target.confidence = 0.01;
        let mut policy = Policy::new(&JobOptions::default(), "http://example.test/");
        minted(decide_not_done(
            &mut policy,
            &step(),
            &observation("CLICK", "button"),
            &click,
            false,
        ));
        let mut page = observation("TYPE_TEXT", "textbox");
        page.elements[0].shares_name = true;
        let mut typed = judgments("TYPE_TEXT");
        typed.type_target.confidence = 0.01;
        let mut policy = Policy::new(&JobOptions::default(), "http://example.test/");
        minted(decide_not_done(&mut policy, &step(), &page, &typed, false));
        let mut page = native_select(&[("Savings", "savings-id", false)]);
        page.elements[0].shares_name = true;
        let mut selected = judgments("SELECT");
        selected.select_target.confidence = 0.01;
        let mut policy = Policy::new(&JobOptions::default(), "http://example.test/")
            .with_provided_values(&provided("Savings"));
        minted(decide_not_done(
            &mut policy,
            &step(),
            &page,
            &selected,
            false,
        ));
    }

    #[test]
    fn key_gate_reobserves_once_then_stops_with_key_below_gate() {
        let mut policy = Policy::new(&JobOptions::default(), "http://example.test/");
        let mut key = judgments("PRESS_KEY");
        key.key.confidence = 0.69;
        assert!(matches!(
            decide_not_done(&mut policy, &step(), &focused("A"), &key, false),
            Next::ReobserveOperation
        ));
        assert!(matches!(
            decide_not_done(&mut policy, &step(), &focused("A"), &key, true),
            Next::Stop(PolicyStop::Uncertain("key_below_gate"))
        ));
        assert_eq!(policy.actions, 0);
    }

    #[test]
    fn operation_gate_precedes_key_validity_and_key_validity_precedes_the_key_gate() {
        let mut policy = Policy::new(&JobOptions::default(), "http://example.test/");
        let mut key = judgments("PRESS_KEY");
        key.operation.confidence = 0.69;
        key.key.confidence = 0.69;
        key.key.choice = "Control+W".into();
        assert!(matches!(
            decide_not_done(&mut policy, &step(), &focused("A"), &key, false),
            Next::ReobserveOperation
        ));
        assert!(matches!(
            decide_not_done(&mut policy, &step(), &focused("A"), &key, true),
            Next::Stop(PolicyStop::Uncertain("operation_below_gate"))
        ));
        key.operation.confidence = 1.0;
        assert!(matches!(
            decide_not_done(&mut policy, &step(), &focused("A"), &key, false),
            Next::Stop(PolicyStop::Blocked("provider_invalid_response"))
        ));
    }

    #[test]
    fn keys_outside_the_closed_roster_are_invalid_responses() {
        let mut policy = Policy::new(&JobOptions::default(), "http://example.test/");
        let mut key = judgments("PRESS_KEY");
        for choice in ["Control+W", "F5", "escape", ""] {
            key.key.choice = choice.into();
            assert!(matches!(
                decide_not_done(&mut policy, &step(), &focused("A"), &key, false),
                Next::Stop(PolicyStop::Blocked("provider_invalid_response"))
            ));
            assert_eq!(
                policy.caller_candidate(&focused("A"), &key),
                Err(PolicyStop::Blocked("provider_invalid_response"))
            );
        }
        assert_eq!(policy.actions, 0);
    }

    #[test]
    fn operation_and_key_confidence_are_gated_without_gating_targets_or_values() {
        let mut policy = Policy::new(&JobOptions::default(), "http://example.test/");
        let mut click = judgments("CLICK");
        click.type_target.confidence = 0.01;
        assert!(matches!(
            decide_not_done(
                &mut policy,
                &step(),
                &observation("CLICK", "button"),
                &click,
                false
            ),
            Next::Mutate(_)
        ));
        let mut policy = Policy::new(&JobOptions::default(), "http://example.test/");
        click.click_target.confidence = 0.69;
        assert!(matches!(
            decide_not_done(
                &mut policy,
                &step(),
                &observation("CLICK", "button"),
                &click,
                false
            ),
            Next::Mutate(_)
        ));
        let mut typed = judgments("TYPE_TEXT");
        typed.type_value.confidence = 0.01;
        let mut policy = Policy::new(&JobOptions::default(), "http://example.test/");
        assert!(matches!(
            decide_not_done(
                &mut policy,
                &step(),
                &observation("TYPE_TEXT", "textbox"),
                &typed,
                false
            ),
            Next::Mutate(_)
        ));

        let mut key = judgments("PRESS_KEY");
        key.key.confidence = 0.69;
        assert!(matches!(
            decide_not_done(
                &mut Policy::new(&JobOptions::default(), "http://example.test/"),
                &step(),
                &focused("A"),
                &key,
                false
            ),
            Next::ReobserveOperation
        ));
        for (operation, role) in [("CLICK", "button"), ("TYPE_TEXT", "textbox")] {
            let mut other = judgments(operation);
            other.key.confidence = 0.01;
            other.key.choice = "Control+W".into();
            assert!(matches!(
                decide_not_done(
                    &mut Policy::new(&JobOptions::default(), "http://example.test/"),
                    &step(),
                    &observation(operation, role),
                    &other,
                    false
                ),
                Next::Mutate(_)
            ));
        }
    }

    #[test]
    fn operation_gate_reobserves_once_and_none_fits_never_mints_a_permit() {
        let mut low = judgments("CLICK");
        low.operation.confidence = 0.69;
        let mut policy = Policy::new(&JobOptions::default(), "http://example.test/");
        assert!(matches!(
            decide_not_done(
                &mut policy,
                &step(),
                &observation("CLICK", "button"),
                &low,
                false
            ),
            Next::ReobserveOperation
        ));
        assert!(matches!(
            decide_not_done(
                &mut policy,
                &step(),
                &observation("CLICK", "button"),
                &low,
                true
            ),
            Next::Stop(PolicyStop::Uncertain("operation_below_gate"))
        ));
        let mut none = judgments("TYPE_TEXT");
        none.type_value = choice("NONE_FITS");
        let mut policy = Policy::new(&JobOptions::default(), "http://example.test/");
        assert!(matches!(
            decide_not_done(
                &mut policy,
                &step(),
                &observation("TYPE_TEXT", "textbox"),
                &none,
                false
            ),
            Next::Stop(PolicyStop::Blocked("value_not_provided"))
        ));
    }

    #[test]
    fn done_first_prevents_a_confident_operation_from_dispatching() {
        let mut judgment = judgments("CLICK");
        judgment.operation.confidence = 0.95;
        judgment.step_done = 0.50;
        let mut policy = Policy::new(&JobOptions::default(), "http://example.test/");
        assert!(matches!(
            policy.decide(
                &natural_step(),
                &observation("CLICK", "button"),
                &judgment,
                DoneResult::Unknown,
                false,
                false
            ),
            Next::ReobserveDone
        ));
        assert_eq!(policy.actions, 0);
        assert!(matches!(
            policy.decide(
                &natural_step(),
                &observation("CLICK", "button"),
                &judgment,
                DoneResult::Unknown,
                true,
                false
            ),
            Next::Stop(PolicyStop::Uncertain("done_uncertain"))
        ));
        assert_eq!(policy.actions, 0);
    }

    #[test]
    fn recorded_spanish_done_judgments_stop_instead_of_advancing() {
        let fixture: Value = serde_json::from_str(include_str!(
            "../../tests/fixtures/recorded-spanish-done-judgments.json"
        ))
        .unwrap();
        for record in fixture["records"].as_array().unwrap() {
            assert_eq!(record["first"]["step"], 2);
            assert_eq!(record["first"]["call"], 2);
            assert_eq!(record["first"]["retry"], false);
            assert!(record["first"]["structured_done"].is_null());
            assert_eq!(record["retry"]["step"], 2);
            assert_eq!(record["retry"]["call"], 3);
            assert_eq!(record["retry"]["retry"], true);
            assert!(record["retry"]["structured_done"].is_null());
            let replay = |decision: &Value| {
                let mut judgment = judgments(decision["operation"].as_str().unwrap());
                judgment.step_done = decision["step_done"].as_f64().unwrap();
                judgment.operation.confidence = decision["confidence"].as_f64().unwrap();
                judgment
            };
            let mut policy = Policy::new(&JobOptions::default(), "http://example.test/");
            assert!(matches!(
                policy.decide(
                    &natural_step(),
                    &observation("TYPE_TEXT", "textbox"),
                    &replay(&record["first"]),
                    DoneResult::Unknown,
                    false,
                    false
                ),
                Next::ReobserveDone
            ));
            assert!(matches!(
                policy.decide(
                    &natural_step(),
                    &observation("TYPE_TEXT", "textbox"),
                    &replay(&record["retry"]),
                    DoneResult::Unknown,
                    true,
                    false
                ),
                Next::Stop(PolicyStop::Uncertain("done_uncertain"))
            ));
            assert_eq!(policy.actions, 0, "record {}", record["source"]);
        }
    }

    #[test]
    fn recorded_money_decisions_replay_done_first_and_gate_consumption() {
        let fixture: Value = serde_json::from_str(include_str!(
            "../../tests/fixtures/recorded-policy-decisions.json"
        ))
        .unwrap();
        let records = fixture["records"].as_array().unwrap();
        let from_record = |record: &Value| {
            let operation = record["operation"].as_str().unwrap();
            let confidence = record["confidence"].as_f64().unwrap();
            let mut replay = judgments(operation);
            replay.operation.confidence = confidence;
            replay.operation.probabilities = BTreeMap::from([(operation.into(), confidence)]);
            if let Some(target) = record.get("target").and_then(Value::as_str) {
                replay.click_target = choice(target);
            }
            replay
        };

        let mut policy = Policy::new(&JobOptions::default(), "http://example.test/");
        assert_eq!(records[0]["structured_done"], true);
        // The recorded speculative operation is deliberately not passed to policy:
        // production consumes the authoritative structured result first.
        assert_eq!(policy.actions, 0);

        assert!(matches!(
            decide_not_done(
                &mut policy,
                &step(),
                &observation("CLICK", "button"),
                &from_record(&records[1]),
                records[1]["retry"].as_bool().unwrap()
            ),
            Next::ReobserveOperation
        ));
        assert!(matches!(
            decide_not_done(
                &mut policy,
                &step(),
                &observation("CLICK", "button"),
                &from_record(&records[2]),
                records[2]["retry"].as_bool().unwrap()
            ),
            Next::Stop(PolicyStop::Uncertain("operation_below_gate"))
        ));

        let mut recorded_page = observation("CLICK", records[3]["target_role"].as_str().unwrap());
        recorded_page.elements[0].index = records[3]["target"].as_str().unwrap().parse().unwrap();
        recorded_page.elements[0].name = records[3]["target_name"].as_str().unwrap().into();
        let mut fresh_policy = Policy::new(&JobOptions::default(), "http://example.test/");
        assert!(matches!(
            decide_not_done(
                &mut fresh_policy,
                &step(),
                &recorded_page,
                &from_record(&records[3]),
                false
            ),
            Next::Mutate(_)
        ));
    }

    #[test]
    fn hover_follows_done_first_and_the_operation_gate() {
        let mut policy = Policy::new(&JobOptions::default(), "http://example.test/");
        let selected = hover(Some("R1"));
        let decide = |policy: &mut Policy, done, done_reobserved, judgment: &Judgments, gate| {
            policy.decide(
                &natural_step(),
                &hover_page(),
                judgment,
                done,
                done_reobserved,
                gate,
            )
        };
        assert!(matches!(
            decide(&mut policy, DoneResult::Satisfied, false, &selected, false),
            Next::Complete
        ));
        assert!(matches!(
            decide(&mut policy, DoneResult::Unknown, false, &selected, false),
            Next::ReobserveDone
        ));
        assert!(matches!(
            decide(&mut policy, DoneResult::Unknown, true, &selected, false),
            Next::Stop(PolicyStop::Uncertain("done_uncertain"))
        ));
        let mut low = selected.clone();
        low.operation.confidence = 0.59;
        assert!(matches!(
            decide(&mut policy, DoneResult::NotSatisfied, false, &low, false),
            Next::ReobserveOperation
        ));
        assert!(matches!(
            decide(&mut policy, DoneResult::NotSatisfied, false, &low, true),
            Next::Stop(PolicyStop::Uncertain("operation_below_gate"))
        ));
        assert_eq!(policy.actions, 0);
    }

    #[test]
    fn hover_clears_a_lower_operation_gate_than_every_other_operation() {
        let gated = |operation: &str, confidence: f64, page: &Observation| {
            let mut judgment = judgments(operation);
            judgment.hover_target = Some(choice("R1"));
            judgment.operation.confidence = confidence;
            let mut policy = Policy::new(&JobOptions::default(), "http://example.test/");
            [false, true].map(|reobserved| {
                decide_not_done(&mut policy, &step(), page, &judgment, reobserved)
            })
        };
        for confidence in [0.60, 0.65] {
            let [first, _] = gated("HOVER", confidence, &hover_page());
            assert_eq!(minted(first).operation(), Operation::Hover, "{confidence}");
        }
        let [first, second] = gated("HOVER", 0.59, &hover_page());
        assert!(matches!(first, Next::ReobserveOperation));
        assert!(matches!(
            second,
            Next::Stop(PolicyStop::Uncertain("operation_below_gate"))
        ));
        let others = [
            "CLICK",
            "TYPE_TEXT",
            "SELECT",
            "PRESS_KEY",
            "SCROLL_UP",
            "SCROLL_DOWN",
            "WAIT",
            "BLOCKED",
        ];
        for operation in others {
            let [first, second] = gated(operation, 0.65, &hover_page());
            assert!(matches!(first, Next::ReobserveOperation), "{operation}");
            assert!(
                matches!(
                    second,
                    Next::Stop(PolicyStop::Uncertain("operation_below_gate"))
                ),
                "{operation}"
            );
        }
        let [first, _] = gated("CLICK", 0.70, &observation("CLICK", "button"));
        assert_eq!(minted(first).operation(), Operation::Click);
    }
}
