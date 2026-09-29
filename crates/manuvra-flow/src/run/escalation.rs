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
    let candidates = offered_candidate.as_ref().map_or_else(
        || judgments.map(|value|json!({"operation":value.operation,"click_target":value.click_target,"type_target":value.type_target,"select_target":value.select_target,"type_value":value.type_value,"key":value.key})).unwrap_or_else(||json!({})),
        |candidate| json!([candidate]),
    );
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
    let payload = redacted_value(
        &json!({
            "id":id,
            "phase":"verification",
            "expectations":artifacts.expectation_verdicts,
            "observation":observation,
            "verification":"verification/final.json",
            "gate_reason":reason,
            "stopped_at":stopped,
        }),
        redactor,
    );
    artifacts.escalations.push((id.clone(), payload));
    artifacts.escalation = Some(Escalation {
        id: id.clone(),
        phase: "verification".into(),
        step_id: None,
        expires_at: stopped,
        payload: format!("escalations/{id}.json"),
        dispositions: vec![
            DispositionKind::Advance,
            DispositionKind::RetryObservation,
            DispositionKind::Abort,
        ],
    });
    Stop::uncertain(reason, BTreeMap::new())
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
        hover_regions_truncated: false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::run::tests::support::*;

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
            "hover_target":{"name":"Groceries","reveals_on_hover":["Actions for Groceries"]}
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
}
