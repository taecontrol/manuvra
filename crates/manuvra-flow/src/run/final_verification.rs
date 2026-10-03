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
    let report = match evaluate_final(job, &captured.raw, values, evaluator, policy) {
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
    observation: &Observation,
    values: &Values<'_>,
    evaluator: &impl manuvra_jev::Evaluator,
    policy: &mut policy::Policy,
) -> Result<crate::verification::VerificationReport, Stop> {
    let plan = prepare_verification(&job.expectations, observation, values);
    let deadline = if !plan.needs_provider() {
        Instant::now()
    } else {
        policy
            .record_model_call()
            .map_err(verification_policy_stop)?
    };
    plan.finish(&job.expectations, observation, values, evaluator, deadline)
        .map_err(|error| verification_provider_stop(&error))
}

pub(super) fn record_verification(
    report: crate::verification::VerificationReport,
    observation: Observation,
    redactor: &Redactor,
    artifacts: &mut RunArtifacts,
    uncertainty_reason: &'static str,
) -> VerificationProgress {
    artifacts.expectation_verdicts = redacted_expectation_verdicts(&report.verdicts, redactor);
    let mut record = redacted_value(&report.record, redactor);
    record["expectations"] = json!(artifacts.expectation_verdicts);
    artifacts.verification = Some(record);
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
    use manuvra_contract::VerdictResult;

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
}
