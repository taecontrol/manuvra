//! Final verification: the job's expectations judged against a fresh, allowed, verifiably
//! redacted observation after the last step.

use super::artifacts::{PendingVerification, RunArtifacts};
use super::capture::{Captured, DriveBrowser, capture_step, final_assertions, redacted_value};
use super::escalation::escalate_verification;
use super::stops::{Stop, control_stop, verification_policy_stop, verification_provider_stop};
use crate::evidence::Redactor;
use crate::verification::{DoneResult, prepare_verification};
use crate::{policy, values::Values};
use manuvra_chrome::Observation;
use manuvra_contract::{ExpectationVerdict, Job};
use serde_json::json;
use std::collections::BTreeMap;
use std::time::Instant;

pub(super) enum VerificationProgress {
    Complete,
    Stop(Stop),
}

#[allow(clippy::too_many_arguments)]
pub(super) fn verify_final(
    job: &Job,
    redactor: &Redactor,
    browser: &impl DriveBrowser,
    evaluator: &impl manuvra_jev::Evaluator,
    values: &Values<'_>,
    policy: &mut policy::Policy,
    artifacts: &mut RunArtifacts,
) -> VerificationProgress {
    let captured = match capture_final(job, redactor, browser, policy, artifacts) {
        Ok(captured) => captured,
        Err(stop) => return VerificationProgress::Stop(stop),
    };
    if !captured.redaction_verified {
        return VerificationProgress::Stop(Stop::blocked(
            "redaction_unverifiable",
            BTreeMap::new(),
        ));
    }
    let report = match evaluate_final(
        job,
        redactor,
        &captured.raw,
        values,
        evaluator,
        policy,
        artifacts,
    ) {
        Ok(report) => report,
        Err(stop) => return VerificationProgress::Stop(stop),
    };
    record_verification(
        report,
        captured.raw,
        redactor,
        artifacts,
        "verification_uncertain",
    )
}

/// The final observation, on an allowed origin before any expectation is judged on it.
fn capture_final(
    job: &Job,
    redactor: &Redactor,
    browser: &impl DriveBrowser,
    policy: &policy::Policy,
    artifacts: &mut RunArtifacts,
) -> Result<Captured, Stop> {
    let captured = capture_step(
        browser,
        redactor,
        &final_assertions(job),
        job.steps.len() + 1,
        artifacts.observations.len() + 1,
    )
    .map_err(control_stop)?;
    policy
        .check_origin(&captured.raw)
        .map_err(verification_policy_stop)?;
    artifacts.observations.push(captured.artifact.clone());
    artifacts.trace.push(json!({
        "event":"final_verification_observation",
        "redaction_verified":captured.redaction_verified,
    }));
    policy.check_active().map_err(verification_policy_stop)?;
    Ok(captured)
}

pub(super) fn capture_verification_advance(
    job: &Job,
    redactor: &Redactor,
    browser: &impl DriveBrowser,
    policy: &policy::Policy,
    artifacts: &mut RunArtifacts,
) -> Result<Observation, Stop> {
    let captured = capture_final(job, redactor, browser, policy, artifacts)?;
    if captured.redaction_verified {
        Ok(captured.raw)
    } else {
        Err(Stop::blocked("redaction_unverifiable", BTreeMap::new()))
    }
}

pub(super) fn evaluate_final(
    job: &Job,
    redactor: &Redactor,
    observation: &Observation,
    values: &Values<'_>,
    evaluator: &impl manuvra_jev::Evaluator,
    policy: &mut policy::Policy,
    artifacts: &mut RunArtifacts,
) -> Result<crate::verification::VerificationReport, Stop> {
    let plan = prepare_verification(&job.expectations, observation, values);
    let deadline = if !plan.needs_provider() {
        Instant::now()
    } else {
        store_verification(&plan.preliminary_report(), redactor, artifacts);
        policy
            .record_model_call()
            .map_err(verification_policy_stop)?
    };
    plan.finish(&job.expectations, observation, values, evaluator, deadline)
        .map_err(|error| {
            let stop = verification_provider_stop(&error);
            if let Some(record) = &mut artifacts.verification {
                record["provider"] = json!({
                    "attempted":true,"failed":true,
                    "reason":{"code":stop.code,"details":redacted_value(&json!(stop.details), redactor)}
                });
            }
            stop
        })
}

fn store_verification(
    report: &crate::verification::VerificationReport,
    redactor: &Redactor,
    artifacts: &mut RunArtifacts,
) {
    artifacts.expectation_verdicts = redacted_expectation_verdicts(&report.verdicts, redactor);
    let mut record = redacted_value(&report.record, redactor);
    record["expectations"] = json!(artifacts.expectation_verdicts);
    artifacts.verification = Some(record);
}

pub(super) fn record_verification(
    report: crate::verification::VerificationReport,
    observation: Observation,
    redactor: &Redactor,
    artifacts: &mut RunArtifacts,
    uncertainty_reason: &'static str,
) -> VerificationProgress {
    store_verification(&report, redactor, artifacts);
    match report.outcome {
        DoneResult::Satisfied => VerificationProgress::Complete,
        DoneResult::NotSatisfied => {
            VerificationProgress::Stop(Stop::failed("expectation_not_met", BTreeMap::new()))
        }
        DoneResult::Unknown => {
            artifacts.pending_verification = Some(PendingVerification {
                verdicts: artifacts.expectation_verdicts.clone(),
                observation,
            });
            VerificationProgress::Stop(escalate_verification(
                artifacts,
                redactor,
                uncertainty_reason,
            ))
        }
    }
}

fn redacted_expectation_verdicts(
    verdicts: &[ExpectationVerdict],
    redactor: &Redactor,
) -> Vec<ExpectationVerdict> {
    verdicts
        .iter()
        .map(|verdict| ExpectationVerdict {
            id: redactor.redact_export_text(&verdict.id),
            result: verdict.result,
            noul: verdict.noul,
            assertion_checks: crate::evidence::redacted_assertion_checks(
                &verdict.assertion_checks,
                redactor,
            ),
            numeric_checks: verdict
                .numeric_checks
                .iter()
                .map(|check| manuvra_contract::NumericCheck {
                    literal: redactor.redact_export_text(&check.literal),
                    present: check.present,
                    within_text: check
                        .within_text
                        .as_ref()
                        .map(|text| redactor.redact_export_text(text)),
                })
                .collect(),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::run::tests::support::*;
    use crate::{policy::Policy, values::Values};
    use manuvra_contract::{RunState, VerdictResult};

    #[test]
    fn final_verification_uses_a_fresh_observation_and_fails_missing_literals() {
        let browser = FakeBrowser::new([observed("Ready"), observed("Ready balance 112.34")]);
        let provider = ScriptedProvider::new([Turn::verdict(0.95)]);

        let artifacts = driven(
            &expectation_job(),
            &browser,
            &provider,
            &mut MemoryJournal::default(),
        );

        assert_eq!(artifacts.observations.len(), 2);
        assert_eq!(artifacts.stop.unwrap().code, "expectation_not_met");
        assert_eq!(
            artifacts.expectation_verdicts[0].result,
            VerdictResult::NotSatisfied
        );
        assert_eq!(artifacts.expectation_verdicts[0].noul, Some(0.95));
        assert!(!artifacts.expectation_verdicts[0].numeric_checks[0].present);
        assert!(artifacts.verification.is_some());
    }

    #[test]
    fn structured_final_and_advance_stop_when_the_active_budget_is_expired() {
        let mut wire = serde_json::to_value(expectation_job()).unwrap();
        wire["expectations"] = json!([{"id":"ready","assertions":[{"text_visible":"Ready"}]}]);
        let mut job = parse_job(wire);
        // Represent an already-expired policy deterministically, without sleeping during capture.
        job.options.active_timeout_ms = Some(0);
        let redactor = Redactor::for_job(&job).unwrap();
        let browser = FakeBrowser::new([observed("Ready")]);
        let values = Values::new(&job);
        let mut policy = Policy::new(&job.options, "http://127.0.0.1:4351/");
        let mut artifacts = RunArtifacts::new(&job, &redactor);
        let result = verify_final(
            &job,
            &redactor,
            &browser,
            &NoProvider,
            &values,
            &mut policy,
            &mut artifacts,
        );
        let VerificationProgress::Stop(stop) = result else {
            panic!("an expired final observation cannot complete verification");
        };
        assert_eq!(stop.code, "budget_exhausted");
        let stop = capture_verification_advance(&job, &redactor, &browser, &policy, &mut artifacts)
            .expect_err("advance cannot attest an observation after the active budget expires");
        assert_eq!(stop.code, "budget_exhausted");
        assert_eq!(browser.dispatched(), 0);
    }

    fn mixed_color_job(max_model_calls: u16) -> Job {
        let mut wire = serde_json::to_value(expectation_job()).unwrap();
        wire["expectations"] = json!([
            {"id":"color","assertions":[{"color":{"target":{"text":"Amount"},"equals":"#b91c1c","tolerance":1}}]},
            {"id":"natural","claim":"Ready is present"}
        ]);
        wire["values"] =
            json!({"amount":{"value":"Amount","description":"classified subject","secret":true}});
        wire["options"]["max_model_calls"] = json!(max_model_calls);
        parse_job(wire)
    }

    fn color_page(red: u8) -> Observation {
        let mut value = serde_json::to_value(observed("Ready")).unwrap();
        value["colors"] = json!([{
            "node_id":1,"context":"main","text":"Amount","name":null,"role":null,
            "in_dialog":null,"container":null,"dialog_node_id":null,"container_node_id":null,
            "channel":"accessible","color":{"raw":"fixture computed color","rgba":[red,28,28,255]}
        }]);
        value["colors_complete"] = json!(true);
        value["color_scopes"] = json!([]);
        serde_json::from_value(value).unwrap()
    }

    fn assert_preliminary_evidence(artifacts: &RunArtifacts, red: u8, budget: bool) {
        assert_eq!(
            artifacts.expectation_verdicts[0].result,
            VerdictResult::Satisfied
        );
        assert_eq!(
            artifacts.expectation_verdicts[1].result,
            VerdictResult::NotRun
        );
        assert!(artifacts.expectation_verdicts[1].noul.is_none());
        let record = artifacts
            .verification
            .as_ref()
            .expect("computed checks are recorded");
        assert_eq!(
            record["expectations"][0]["assertion_checks"][0]["color"]["target"]["rgba"],
            json!([red, 28, 28, 255])
        );
        assert!(!record.to_string().contains("Amount"));
        if budget {
            assert!(record["provider"].is_null());
        } else {
            assert_eq!(record["provider"]["attempted"], true);
            assert_eq!(record["provider"]["failed"], true);
            assert_eq!(record["provider"]["reason"]["code"], "provider_unavailable");
        }
        assert!(artifacts.escalation.is_none());
        assert!(!artifacts.caller_assisted);
    }

    fn initial_failure_retains_preliminary_evidence(budget: bool) {
        let job = mixed_color_job(1);
        let redactor = Redactor::for_job_with_provider_key(&job, None).unwrap();
        let browser = FakeBrowser::new([color_page(185)]);
        let values = Values::new(&job);
        let mut policy = Policy::new(&job.options, "http://127.0.0.1:4351/");
        if budget {
            policy.record_model_call().unwrap();
        }
        let mut artifacts = RunArtifacts::new(&job, &redactor);
        let VerificationProgress::Stop(stop) = verify_final(
            &job,
            &redactor,
            &browser,
            &NoProvider,
            &values,
            &mut policy,
            &mut artifacts,
        ) else {
            panic!("a natural expectation cannot complete without its provider result");
        };
        assert_eq!(stop.state, RunState::Blocked);
        assert_eq!(
            stop.code,
            if budget {
                "budget_exhausted"
            } else {
                "provider_unavailable"
            }
        );
        assert_preliminary_evidence(&artifacts, 185, budget);
        assert_eq!(browser.dispatched(), 0);
    }

    #[test]
    fn initial_provider_failure_retains_preliminary_color_evidence() {
        initial_failure_retains_preliminary_evidence(false);
    }

    #[test]
    fn initial_budget_failure_retains_preliminary_color_evidence() {
        initial_failure_retains_preliminary_evidence(true);
    }

    fn changed_advance_failure_retains_preliminary_evidence(budget: bool) {
        let job = mixed_color_job(if budget { 1 } else { 2 });
        let redactor = Redactor::for_job_with_provider_key(&job, None).unwrap();
        let browser = FakeBrowser::new([color_page(185), color_page(185), color_page(184)]);
        let provider = ScriptedProvider::new([Turn::verdict(0.5)]);
        let mut machine = super::super::machine::HostedMachine::new(&job, &redactor);
        let mut journal = MemoryJournal::default();
        drive(&mut machine, &browser, &provider, &mut journal);
        assert!(machine.paused_escalation_id().is_some());
        dispose(
            &mut machine,
            advance("Ready is present"),
            &browser,
            &NoProvider,
            &mut journal,
        );
        let stop = machine.artifacts.stop.as_ref().unwrap();
        assert_eq!(stop.state, RunState::Blocked);
        assert_eq!(
            stop.code,
            if budget {
                "budget_exhausted"
            } else {
                "provider_unavailable"
            }
        );
        assert_preliminary_evidence(&machine.artifacts, 184, budget);
        assert!(!machine.verification_complete);
        assert_eq!(provider.calls(), 1);
        assert_eq!(browser.dispatched(), 0);
    }

    #[test]
    fn changed_advance_provider_failure_retains_preliminary_color_evidence() {
        changed_advance_failure_retains_preliminary_evidence(false);
    }

    #[test]
    fn changed_advance_budget_failure_retains_preliminary_color_evidence() {
        changed_advance_failure_retains_preliminary_evidence(true);
    }

    struct ErrorProvider(manuvra_jev::JevError);

    impl manuvra_jev::Evaluator for ErrorProvider {
        fn evaluate(
            &self,
            _: &serde_json::Value,
            _: Instant,
        ) -> Result<manuvra_jev::Evaluation, manuvra_jev::JevError> {
            Err(self.0.clone())
        }
    }

    #[test]
    fn preliminary_provider_error_evidence_matches_the_terminal_stop() {
        for error in [
            manuvra_jev::JevError::Deadline,
            manuvra_jev::JevError::ModelChanged,
            manuvra_jev::JevError::InvalidResponse("Amount is classified".into()),
        ] {
            let job = mixed_color_job(2);
            let redactor = Redactor::for_job_with_provider_key(&job, None).unwrap();
            let browser = FakeBrowser::new([color_page(185)]);
            let values = Values::new(&job);
            let mut policy = Policy::new(&job.options, "http://127.0.0.1:4351/");
            let mut artifacts = RunArtifacts::new(&job, &redactor);
            let VerificationProgress::Stop(stop) = verify_final(
                &job,
                &redactor,
                &browser,
                &ErrorProvider(error.clone()),
                &values,
                &mut policy,
                &mut artifacts,
            ) else {
                panic!("failed provider result cannot complete verification");
            };
            assert_eq!(stop.state, RunState::Blocked);
            let record = artifacts.verification.as_ref().unwrap();
            assert_eq!(record["provider"]["reason"]["code"], stop.code, "{error}");
            assert_eq!(record["provider"]["attempted"], true);
            assert_eq!(record["provider"]["failed"], true);
            assert_eq!(
                artifacts.expectation_verdicts[0].result,
                VerdictResult::Satisfied
            );
            assert_eq!(
                artifacts.expectation_verdicts[1].result,
                VerdictResult::NotRun
            );
            assert!(!record.to_string().contains("Amount"));
            assert!(!artifacts.caller_assisted);
            assert!(artifacts.escalation.is_none());
            assert_eq!(browser.dispatched(), 0);
        }
    }
}
