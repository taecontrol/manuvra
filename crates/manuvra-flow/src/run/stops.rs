//! How a run stops: the terminal fields of each stop and the mapping of policy, provider,
//! browser-control, and action failures to the stop that ends or pauses the run.

use super::artifacts::RunArtifacts;
use super::capture::Captured;
use crate::evidence::Redactor;
use crate::{actions, judgment, policy, values::Values};
use manuvra_contract::{Reason, RunState, VerdictResult};
use serde_json::{Value, json};
use std::collections::BTreeMap;

#[derive(Clone)]
pub(super) struct Stop {
    pub(super) state: RunState,
    pub(super) code: &'static str,
    exit_code: u8,
    pub(super) details: BTreeMap<String, Value>,
}

impl Stop {
    pub(super) fn blocked(code: &'static str, details: BTreeMap<String, Value>) -> Self {
        Self {
            state: RunState::Blocked,
            code,
            exit_code: 3,
            details,
        }
    }
    pub(super) fn uncertain(code: &'static str, details: BTreeMap<String, Value>) -> Self {
        Self {
            state: RunState::Uncertain,
            code,
            exit_code: 2,
            details,
        }
    }
    pub(super) fn failed(code: &'static str, details: BTreeMap<String, Value>) -> Self {
        Self {
            state: RunState::Failed,
            code,
            exit_code: 4,
            details,
        }
    }
    fn fields(self) -> (RunState, Option<Reason>, u8, VerdictResult) {
        let overall = if self.state == RunState::Failed {
            VerdictResult::NotSatisfied
        } else {
            VerdictResult::Unresolved
        };
        (
            self.state,
            Some(Reason {
                code: self.code.into(),
                details: self.details,
            }),
            self.exit_code,
            overall,
        )
    }
}

pub(super) fn terminal_fields(stop: Option<Stop>) -> (RunState, Option<Reason>, u8, VerdictResult) {
    stop.map_or(
        (RunState::Passed, None, 0, VerdictResult::Satisfied),
        Stop::fields,
    )
}

pub(super) fn step_detail(
    redactor: &Redactor,
    step: &manuvra_contract::Step,
) -> BTreeMap<String, Value> {
    BTreeMap::from([(
        "step_id".into(),
        json!(redactor.redact_export_text(&step.id)),
    )])
}

/// A policy stop that ends the run. Policy uncertainty is escalated where it arises, with the
/// observation and candidates a disposition needs, so an uncertain stop is never published
/// without an escalation.
pub(super) fn policy_stop(
    stop: policy::PolicyStop,
    redactor: &Redactor,
    step: &manuvra_contract::Step,
) -> Stop {
    match stop {
        policy::PolicyStop::Blocked(code) | policy::PolicyStop::Uncertain(code) => {
            Stop::blocked(code, step_detail(redactor, step))
        }
        policy::PolicyStop::UnsupportedSurface(surface) => {
            let mut details = step_detail(redactor, step);
            details.insert("surface".into(), json!(surface));
            Stop::blocked("unsupported_surface", details)
        }
    }
}

pub(super) fn provider_stop(
    error: &manuvra_jev::JevError,
    redactor: &Redactor,
    step: &manuvra_contract::Step,
) -> Stop {
    match error {
        manuvra_jev::JevError::InvalidResponse(_) | manuvra_jev::JevError::ModelChanged => {
            Stop::blocked("provider_invalid_response", step_detail(redactor, step))
        }
        manuvra_jev::JevError::Deadline => {
            Stop::blocked("budget_exhausted", step_detail(redactor, step))
        }
        _ => Stop::blocked("provider_unavailable", step_detail(redactor, step)),
    }
}

pub(super) fn value_not_provided(
    redactor: &Redactor,
    step: &manuvra_contract::Step,
    captured: &Captured,
    judgments: &judgment::Judgments,
    values: &Values<'_>,
) -> Stop {
    let field = judgments
        .type_target
        .choice
        .parse::<u64>()
        .ok()
        .and_then(|index| {
            captured
                .raw
                .elements
                .iter()
                .find(|element| element.index == index)
        })
        .map(|element| redactor.redact_export_text(&element.name));
    Stop::blocked(
        "value_not_provided",
        BTreeMap::from([
            (
                "step_id".into(),
                json!(redactor.redact_export_text(&step.id)),
            ),
            ("observed_field".into(), json!(field)),
            ("known_value_names".into(), json!(values.known_names())),
        ]),
    )
}

pub(super) fn control_stop(message: String) -> Stop {
    Stop::blocked(
        "browser_control_failed",
        BTreeMap::from([("message".into(), json!(message))]),
    )
}

/// The stop of an action whose failure ends the run. An action journal that cannot be written
/// before dispatch blocks it, a readback mismatch fails it, and a dispatched action whose fact
/// cannot be journaled blocks it with evidence that stays incomplete for the rest of the run: its
/// outcome is unrecorded, so it is neither retried nor reported as not performed.
pub(super) fn terminal_action_stop(
    stop: actions::ActionStop,
    artifacts: &mut RunArtifacts,
    redactor: &Redactor,
    step: &manuvra_contract::Step,
) -> Stop {
    match stop {
        actions::ActionStop::EvidenceUnavailable => {
            Stop::blocked("evidence_unavailable", step_detail(redactor, step))
        }
        actions::ActionStop::ReadbackMismatch => {
            Stop::failed("write_readback_mismatch", step_detail(redactor, step))
        }
        actions::ActionStop::IncompleteEvidence => {
            artifacts.evidence_incomplete = true;
            Stop::blocked(
                "evidence_incomplete_after_dispatch",
                step_detail(redactor, step),
            )
        }
        other => unreachable!("{other:?} does not end the run"),
    }
}

/// Final verification escalates its own uncertainty; a policy stop reached here ends the run.
pub(super) fn verification_policy_stop(stop: policy::PolicyStop) -> Stop {
    match stop {
        policy::PolicyStop::Blocked(code) | policy::PolicyStop::Uncertain(code) => {
            Stop::blocked(code, BTreeMap::new())
        }
        policy::PolicyStop::UnsupportedSurface(surface) => Stop::blocked(
            "unsupported_surface",
            BTreeMap::from([("surface".into(), json!(surface))]),
        ),
    }
}

pub(super) fn verification_provider_stop(error: &manuvra_jev::JevError) -> Stop {
    match error {
        manuvra_jev::JevError::InvalidResponse(_) | manuvra_jev::JevError::ModelChanged => {
            Stop::blocked("provider_invalid_response", BTreeMap::new())
        }
        manuvra_jev::JevError::Deadline => Stop::blocked("budget_exhausted", BTreeMap::new()),
        _ => Stop::blocked("provider_unavailable", BTreeMap::new()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::run::publish::apply_artifact_verdict;
    use crate::run::tests::support::*;

    #[test]
    fn verification_policy_and_provider_failures_block_the_run() {
        let exhausted = verification_policy_stop(policy::PolicyStop::Blocked("budget_exhausted"));
        assert_eq!(
            (exhausted.code, exhausted.state),
            ("budget_exhausted", RunState::Blocked)
        );
        let foreign = verification_policy_stop(policy::PolicyStop::Blocked("origin_not_allowed"));
        assert_eq!(
            (foreign.code, foreign.state),
            ("origin_not_allowed", RunState::Blocked)
        );
        let popup =
            verification_policy_stop(policy::PolicyStop::UnsupportedSurface("popup_or_new_tab"));
        assert_eq!(popup.code, "unsupported_surface");
        assert_eq!(popup.details["surface"], "popup_or_new_tab");
        for (error, code) in [
            (
                manuvra_jev::JevError::InvalidResponse("bad".into()),
                "provider_invalid_response",
            ),
            (
                manuvra_jev::JevError::ModelChanged,
                "provider_invalid_response",
            ),
            (manuvra_jev::JevError::Deadline, "budget_exhausted"),
            (manuvra_jev::JevError::Unavailable, "provider_unavailable"),
        ] {
            let stop = verification_provider_stop(&error);
            assert_eq!((stop.code, stop.state), (code, RunState::Blocked));
        }
    }

    #[test]
    fn unjournaled_action_facts_keep_every_later_result_incomplete() {
        let job = mutation_job();
        let redactor = Redactor::for_job(&job).unwrap();
        let mut artifacts = RunArtifacts::new(&job, &redactor);
        let stop = terminal_action_stop(
            actions::ActionStop::IncompleteEvidence,
            &mut artifacts,
            &redactor,
            &job.steps[0],
        );
        assert_eq!(
            (stop.code, stop.state),
            ("evidence_incomplete_after_dispatch", RunState::Blocked)
        );
        let mut aborted = json!({
            "state":"aborted",
            "evidence":{"complete":true},
            "verdict":{"expectations":[],"caller_assisted":false}
        });
        apply_artifact_verdict(&mut aborted, &artifacts).unwrap();
        assert_eq!(aborted["evidence"]["complete"], false);

        artifacts.verdicts[0].result = VerdictResult::Satisfied;
        artifacts.verification = Some(json!({"phase":"verification"}));
        let mut passed = json!({
            "state":"passed",
            "evidence":{"complete":true},
            "verdict":{"expectations":[],"caller_assisted":false}
        });
        assert!(apply_artifact_verdict(&mut passed, &artifacts).is_err());
    }
}
