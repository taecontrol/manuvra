//! The run result and the evidence bundle a run publishes, including runs that stop before
//! their first step.

#[cfg(any(target_os = "linux", target_os = "macos", test))]
use super::artifacts::RunArtifacts;
use super::{FlowConfig, FlowOutcome};
use crate::evidence::{self, EvidenceBundle, Redactor};
#[cfg(any(target_os = "linux", target_os = "macos"))]
use manuvra_chrome::BrowserError;
use manuvra_contract::{
    Cleanup, Escalation, EvidenceRef, ExpectationVerdict, Job, Reason, RunResult, RunState,
    SchemaVersion, StepVerdict, Verdict, VerdictResult,
};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::path::Path;

/// The evidence a run recorded while it drove the target, published with its job, provenance,
/// and result.
pub(super) struct RecordedEvidence {
    pub(super) observations: Vec<(String, Value, Option<Vec<u8>>)>,
    pub(super) decisions: Vec<(String, Value)>,
    pub(super) steps: Vec<(String, Value)>,
    pub(super) escalations: Vec<(String, Value)>,
    pub(super) dispositions: Vec<(String, Value)>,
    pub(super) verification: Option<Value>,
    pub(super) trace: Vec<Value>,
    pub(super) cleanup: Cleanup,
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
pub(super) fn publish_browser_error(
    job: &Job,
    config: FlowConfig,
    redactor: &Redactor,
    error: BrowserError,
) -> Result<FlowOutcome, String> {
    let display_mode = if config.headless {
        "headless"
    } else {
        "headed"
    };
    let cleanup = match &error {
        BrowserError::Unavailable | BrowserError::UnsupportedPlatform => not_started_cleanup(),
        _ => unconfirmed_cleanup(),
    };
    publish_browser_error_with_provenance(
        job,
        config,
        redactor,
        error,
        json!({"browser_path":null,"browser_version":null,"viewport":null,"display_mode":display_mode}),
        cleanup,
    )
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
pub(super) fn publish_browser_error_with_provenance(
    job: &Job,
    config: FlowConfig,
    redactor: &Redactor,
    error: BrowserError,
    provenance: Value,
    cleanup: Cleanup,
) -> Result<FlowOutcome, String> {
    let code = match error {
        BrowserError::UnsupportedPlatform => "unsupported_platform",
        BrowserError::Unavailable => "browser_unavailable",
        BrowserError::Launch(_) => "browser_launch_failed",
        BrowserError::Control(_) | BrowserError::InvalidObservation(_) => "browser_control_failed",
    };
    let details = BTreeMap::from([(
        "message".into(),
        json!(redactor.redact_external_text(&error.to_string())),
    )]);
    publish_blocked_before_steps(job, config, redactor, code, details, provenance, cleanup)
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
pub(super) fn publish_without_browser(
    job: &Job,
    config: FlowConfig,
    redactor: &Redactor,
    code: &str,
    details: BTreeMap<String, Value>,
) -> Result<FlowOutcome, String> {
    publish_blocked_before_steps(
        job,
        config,
        redactor,
        code,
        details,
        json!({"browser_path":null,"browser_version":null,"viewport":null,"display_mode":null}),
        not_started_cleanup(),
    )
}

/// A run blocked before its first step: every step is `not_run` and the trace holds only the stop.
fn publish_blocked_before_steps(
    job: &Job,
    config: FlowConfig,
    redactor: &Redactor,
    code: &str,
    details: BTreeMap<String, Value>,
    provenance: Value,
    cleanup: Cleanup,
) -> Result<FlowOutcome, String> {
    let result = run_result(
        job,
        &config,
        redactor,
        RunState::Blocked,
        Some(Reason {
            code: code.into(),
            details,
        }),
        VerdictResult::Unresolved,
        not_run_step_verdicts(job, redactor),
        None,
        cleanup.clone(),
    )?;
    let recorded = RecordedEvidence {
        observations: Vec::new(),
        decisions: Vec::new(),
        steps: Vec::new(),
        escalations: Vec::new(),
        dispositions: Vec::new(),
        verification: None,
        trace: vec![json!({"event":"stop","reason":code})],
        cleanup,
    };
    publish_bundle(job, config, redactor, provenance, recorded, result, 3)
}

fn not_started_cleanup() -> Cleanup {
    Cleanup {
        browser: "not_started".into(),
        profile: "not_created".into(),
        application_state: "caller_owned".into(),
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
pub(super) fn unconfirmed_cleanup() -> Cleanup {
    Cleanup {
        browser: "closure_unconfirmed".into(),
        profile: "removal_unconfirmed".into(),
        application_state: "caller_owned".into(),
    }
}

pub(super) fn not_run_step_verdicts(job: &Job, redactor: &Redactor) -> Vec<StepVerdict> {
    job.steps
        .iter()
        .map(|step| StepVerdict {
            id: redactor.redact_export_text(&step.id),
            result: VerdictResult::NotRun,
            basis: None,
        })
        .collect()
}

pub(super) fn not_run_expectation_verdicts(
    job: &Job,
    redactor: &Redactor,
) -> Vec<ExpectationVerdict> {
    job.expectations
        .iter()
        .map(|expectation| ExpectationVerdict {
            id: redactor.redact_export_text(expectation.id()),
            result: VerdictResult::NotRun,
            noul: None,
            numeric_checks: Vec::new(),
            assertion_checks: Vec::new(),
        })
        .collect()
}

#[allow(clippy::too_many_arguments)]
fn run_result(
    job: &Job,
    config: &FlowConfig,
    redactor: &Redactor,
    state: RunState,
    reason: Option<Reason>,
    overall: VerdictResult,
    steps: Vec<StepVerdict>,
    mut escalation: Option<Escalation>,
    cleanup: Cleanup,
) -> Result<Value, String> {
    let manifest = config
        .evidence_root
        .join(&config.run_id)
        .join("manifest.json");
    if let Some(escalation) = &mut escalation {
        let root =
            std::fs::canonicalize(&config.evidence_root).map_err(|error| error.to_string())?;
        escalation.payload = root
            .join(&config.run_id)
            .join(&escalation.payload)
            .to_string_lossy()
            .into_owned();
    }
    let result = RunResult {
        schema_version: SchemaVersion,
        request_id: redactor.redact_export_text(&config.request_id),
        run_id: config.run_id.clone(),
        state,
        terminal: true,
        reason,
        verdict: Verdict {
            overall,
            steps,
            expectations: not_run_expectation_verdicts(job, redactor),
            caller_assisted: false,
        },
        evidence: EvidenceRef {
            manifest: absolute_text(&manifest)?,
            complete: true,
        },
        escalation,
        cleanup,
    };
    serde_json::to_value(result).map_err(|e| e.to_string())
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[allow(clippy::too_many_arguments)]
pub(super) fn run_result_with_assistance(
    job: &Job,
    config: &FlowConfig,
    redactor: &Redactor,
    state: RunState,
    reason: Option<Reason>,
    overall: VerdictResult,
    steps: Vec<StepVerdict>,
    escalation: Option<Escalation>,
    cleanup: Cleanup,
    caller_assisted: bool,
) -> Result<Value, String> {
    run_result(
        job, config, redactor, state, reason, overall, steps, escalation, cleanup,
    )
    .map(|mut result| {
        result["verdict"]["caller_assisted"] = json!(caller_assisted);
        result
    })
}

#[cfg(any(target_os = "linux", target_os = "macos", test))]
pub(super) fn apply_artifact_verdict(
    result: &mut Value,
    artifacts: &RunArtifacts,
) -> Result<(), String> {
    result["verdict"]["expectations"] =
        serde_json::to_value(&artifacts.expectation_verdicts).unwrap_or(Value::Null);
    result["verdict"]["caller_assisted"] = json!(artifacts.caller_assisted);
    if artifacts.evidence_incomplete {
        result["evidence"]["complete"] = json!(false);
    }
    if passed_without_satisfied_evidence(result, artifacts) {
        return Err("passed result requires complete satisfied verification evidence".into());
    }
    Ok(())
}

#[cfg(any(target_os = "linux", target_os = "macos", test))]
fn passed_without_satisfied_evidence(result: &Value, artifacts: &RunArtifacts) -> bool {
    let passed = result.get("state").and_then(Value::as_str) == Some("passed");
    let complete = result
        .pointer("/evidence/complete")
        .and_then(Value::as_bool)
        == Some(true);
    let steps_satisfied = artifacts
        .verdicts
        .iter()
        .all(|verdict| verdict.result == VerdictResult::Satisfied);
    let expectations_satisfied = artifacts
        .expectation_verdicts
        .iter()
        .all(|verdict| verdict.result == VerdictResult::Satisfied);
    passed
        && (!complete
            || artifacts.verification.is_none()
            || !steps_satisfied
            || !expectations_satisfied)
}

pub(super) fn publish_bundle(
    job: &Job,
    config: FlowConfig,
    redactor: &Redactor,
    mut provenance: Value,
    recorded: RecordedEvidence,
    result: Value,
    exit_code: u8,
) -> Result<FlowOutcome, String> {
    redact_provenance(&mut provenance, redactor);
    let complete = result
        .pointer("/evidence/complete")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let bundle = EvidenceBundle {
        complete,
        job: evidence::redacted_job(job, redactor)?,
        provenance,
        observations: recorded.observations,
        decisions: recorded.decisions,
        steps: recorded.steps,
        escalations: recorded.escalations,
        dispositions: recorded.dispositions,
        verification: recorded.verification,
        trace: recorded.trace,
        cleanup: serde_json::to_value(recorded.cleanup).map_err(|e| e.to_string())?,
        result,
    };
    let result = evidence::publish(&config.evidence_root, &config.run_id, bundle, redactor)?;
    Ok(FlowOutcome { result, exit_code })
}

fn redact_provenance(provenance: &mut Value, redactor: &Redactor) {
    let Some(fields) = provenance.as_object_mut() else {
        return;
    };
    for name in ["browser_path", "browser_version"] {
        if let Some(Value::String(text)) = fields.get_mut(name) {
            *text = redactor.redact_external_text(text);
        }
    }
}

fn absolute_text(path: &Path) -> Result<String, String> {
    let parent = path
        .parent()
        .and_then(Path::parent)
        .ok_or_else(|| "invalid evidence path".to_owned())?;
    let root = std::fs::canonicalize(parent).map_err(|e| e.to_string())?;
    Ok(root
        .join(path.strip_prefix(parent).map_err(|e| e.to_string())?)
        .to_string_lossy()
        .into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::run::tests::support::*;
    use tempfile::TempDir;

    #[test]
    fn passed_result_requires_complete_satisfied_final_verification() {
        let job = expectation_job();
        let redactor = Redactor::for_job(&job).unwrap();
        let mut artifacts = RunArtifacts::new(&job, &redactor);
        let mut result = json!({
            "state":"passed",
            "evidence":{"complete":true},
            "verdict":{"expectations":[],"caller_assisted":false}
        });
        assert!(apply_artifact_verdict(&mut result, &artifacts).is_err());

        artifacts.verdicts[0].result = VerdictResult::Satisfied;
        artifacts.expectation_verdicts[0].result = VerdictResult::Satisfied;
        artifacts.verification = Some(json!({"phase":"verification"}));
        assert!(apply_artifact_verdict(&mut result, &artifacts).is_ok());

        result["evidence"]["complete"] = json!(false);
        assert!(apply_artifact_verdict(&mut result, &artifacts).is_err());
    }

    #[test]
    fn classified_text_cannot_corrupt_result_protocol_literals() {
        let mut job = job("Ready");
        for (name, value) in [
            ("passed_collision", "passed"),
            ("failed_collision", "failed"),
        ] {
            job.values.insert(
                name.into(),
                manuvra_contract::JobValue {
                    value: value.into(),
                    description: "classified collision".into(),
                    formats: None,
                    secret: true,
                },
            );
        }
        let redactor = Redactor::for_job(&job).unwrap();
        let temp = TempDir::new().unwrap();
        let config = FlowConfig {
            request_id: "request".into(),
            run_id: "r_protocol".into(),
            evidence_root: temp.path().to_path_buf(),
            browser: None,
            headless: true,
        };
        let cleanup = Cleanup {
            browser: "closed".into(),
            profile: "removed".into(),
            application_state: "caller_owned".into(),
        };
        for (state, overall, verdict, expected) in [
            (
                RunState::Passed,
                VerdictResult::Satisfied,
                VerdictResult::Satisfied,
                ("passed", "satisfied"),
            ),
            (
                RunState::Failed,
                VerdictResult::NotSatisfied,
                VerdictResult::NotSatisfied,
                ("failed", "not_satisfied"),
            ),
        ] {
            let result = run_result(
                &job,
                &config,
                &redactor,
                state,
                None,
                overall,
                vec![StepVerdict {
                    id: "ready".into(),
                    result: verdict,
                    basis: Some("structured".into()),
                }],
                None,
                cleanup.clone(),
            )
            .unwrap();
            assert_eq!(result["state"], expected.0);
            assert_eq!(result["verdict"]["overall"], expected.1);
        }
    }
}
