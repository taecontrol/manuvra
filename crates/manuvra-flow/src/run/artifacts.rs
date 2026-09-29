//! What a run records as it goes: step and expectation verdicts, evidence files, the current
//! stop and escalation, and the pending state a disposition answers.

#[cfg(any(target_os = "linux", target_os = "macos"))]
use super::publish::RecordedEvidence;
use super::publish::{not_run_expectation_verdicts, not_run_step_verdicts};
use super::stops::Stop;
use crate::evidence::Redactor;
use crate::policy;
use crate::verification::DoneResult;
use manuvra_chrome::Observation;
#[cfg(any(target_os = "linux", target_os = "macos"))]
use manuvra_contract::Cleanup;
use manuvra_contract::{
    DoneCondition, Escalation, ExpectationVerdict, Job, StepVerdict, VerdictResult,
};
use serde_json::{Value, json};

#[derive(Clone)]
pub(super) struct RunArtifacts {
    pub(super) observations: Vec<(String, Value, Option<Vec<u8>>)>,
    pub(super) decisions: Vec<(String, Value)>,
    pub(super) steps: Vec<(String, Value)>,
    pub(super) escalations: Vec<(String, Value)>,
    pub(super) dispositions: Vec<(String, Value)>,
    pub(super) trace: Vec<Value>,
    pub(super) verdicts: Vec<StepVerdict>,
    pub(super) expectation_verdicts: Vec<ExpectationVerdict>,
    pub(super) verification: Option<Value>,
    pub(super) stop: Option<Stop>,
    pub(super) escalation: Option<Escalation>,
    pub(super) pending: Option<PendingEscalation>,
    pub(super) pending_verification: Option<PendingVerification>,
    pub(super) caller_assisted: bool,
    /// Set once a dispatched action's fact could not be journaled; the evidence stays incomplete.
    pub(super) evidence_incomplete: bool,
}

#[derive(Clone)]
pub(super) struct PendingEscalation {
    pub(super) done: DoneResult,
    pub(super) noul: Option<f64>,
    pub(super) candidate: Option<policy::Candidate>,
    pub(super) observation: Observation,
    pub(super) ambiguous_mutation: bool,
}

#[derive(Clone)]
pub(super) struct PendingVerification {
    pub(super) verdicts: Vec<ExpectationVerdict>,
    pub(super) observation: Observation,
}

impl PendingVerification {
    pub(super) fn attestable(&self) -> bool {
        self.verdicts
            .iter()
            .any(|verdict| verdict.result == VerdictResult::Unresolved)
            && self
                .verdicts
                .iter()
                .all(|verdict| verdict.result != VerdictResult::NotSatisfied)
            && self.verdicts.iter().all(|verdict| {
                verdict.result != VerdictResult::Unresolved
                    || verdict.noul.is_some_and(|noul| noul > 0.20)
            })
    }
}

impl RunArtifacts {
    pub(super) fn new(job: &Job, redactor: &Redactor) -> Self {
        Self {
            observations: Vec::new(),
            decisions: Vec::new(),
            steps: Vec::new(),
            escalations: Vec::new(),
            dispositions: Vec::new(),
            trace: Vec::new(),
            verdicts: not_run_step_verdicts(job, redactor),
            expectation_verdicts: not_run_expectation_verdicts(job, redactor),
            verification: None,
            stop: None,
            escalation: None,
            pending: None,
            pending_verification: None,
            caller_assisted: false,
            evidence_incomplete: false,
        }
    }

    /// The evidence recorded so far, published with a paused run's checkpoint.
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    pub(super) fn recorded(&self, cleanup: Cleanup) -> RecordedEvidence {
        RecordedEvidence {
            observations: self.observations.clone(),
            decisions: self.decisions.clone(),
            steps: self.steps.clone(),
            escalations: self.escalations.clone(),
            dispositions: self.dispositions.clone(),
            verification: self.verification.clone(),
            trace: self.trace.clone(),
            cleanup,
        }
    }

    /// The evidence of a run that ends here, published with its cleanup.
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    pub(super) fn into_recorded(self, cleanup: Cleanup) -> RecordedEvidence {
        RecordedEvidence {
            observations: self.observations,
            decisions: self.decisions,
            steps: self.steps,
            escalations: self.escalations,
            dispositions: self.dispositions,
            verification: self.verification,
            trace: self.trace,
            cleanup,
        }
    }
}

#[allow(clippy::too_many_arguments)]
pub(super) fn record_step(
    artifacts: &mut RunArtifacts,
    redactor: &Redactor,
    index: usize,
    step: &manuvra_contract::Step,
    done: DoneResult,
    mutations: u8,
    redaction_verified: bool,
    active_ms: u128,
    basis: &str,
) {
    let id = redactor.redact_export_text(&step.id);
    artifacts.steps.push((safe_name(index+1,&id),json!({"id":id,"done":done,"basis":basis,"mutation_limit_consumed":mutations,"redaction_verified":redaction_verified,"active_ms":active_ms})));
}

/// The basis a satisfied or failed step records: how its done condition was checked.
pub(super) fn done_basis(step: &manuvra_contract::Step) -> &'static str {
    match &step.done_when {
        DoneCondition::Structured(_) => "structured",
        DoneCondition::NaturalLanguage(_) => "natural_language",
    }
}

fn safe_name(index: usize, id: &str) -> String {
    let clean: String = id
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '-' | '_') {
                c
            } else {
                '_'
            }
        })
        .collect();
    format!("{index:04}-{clean}")
}
