//! The hosted machine: the run's state across autonomous driving and caller dispositions. It
//! drives each step in order, publishes a running checkpoint after each one, and then verifies
//! the final expectations.

use super::artifacts::RunArtifacts;
use super::capture::DriveBrowser;
use super::final_verification::{VerificationProgress, verify_final};
use super::step_driver::evaluate_step;
use super::stops::Stop;
use super::{FlowConfig, HostedControl, target_url};
use crate::evidence::Redactor;
use crate::{actions, policy, values::Values};
use manuvra_contract::{Job, RunState, VerdictResult};
use serde_json::json;
use std::collections::BTreeMap;

pub(super) struct HostedMachine<'a> {
    pub(super) job: &'a Job,
    pub(super) redactor: &'a Redactor,
    pub(super) values: Values<'a>,
    pub(super) policy: policy::Policy,
    pub(super) index: usize,
    pub(super) verification_complete: bool,
    pub(super) artifacts: RunArtifacts,
}

impl<'a> HostedMachine<'a> {
    pub(super) fn new(job: &'a Job, redactor: &'a Redactor) -> Self {
        let mut policy =
            policy::Policy::new(&job.options, target_url(job)).with_provided_values(&job.values);
        policy.begin_step();
        Self {
            job,
            redactor,
            values: Values::new(job),
            policy,
            index: 0,
            verification_complete: false,
            artifacts: RunArtifacts::new(job, redactor),
        }
    }

    /// The escalation a disposition must answer, while the run is stopped uncertain on one. Any
    /// other stop is terminal.
    pub(super) fn paused_escalation_id(&self) -> Option<String> {
        let uncertain = self
            .artifacts
            .stop
            .as_ref()
            .is_some_and(|stop| stop.state == RunState::Uncertain);
        uncertain
            .then_some(self.artifacts.escalation.as_ref())
            .flatten()
            .map(|escalation| escalation.id.clone())
    }

    pub(super) fn advance_step(&mut self) {
        self.index += 1;
        if self.index < self.job.steps.len() {
            self.policy.begin_step();
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub(super) fn drive(
        &mut self,
        browser: &impl DriveBrowser,
        evaluator: &impl manuvra_jev::Evaluator,
        journal: &mut impl actions::ActionJournal,
        cancellation: &manuvra_chrome::InputCancellation,
        config: Option<&FlowConfig>,
        control: &dyn HostedControl,
    ) {
        self.artifacts.stop = None;
        self.artifacts.escalation = None;
        self.artifacts.pending = None;
        while self.index < self.job.steps.len() {
            let step = &self.job.steps[self.index];
            let mutations = self.policy.step_mutations();
            let observation_number = self.artifacts.observations.len();
            let outcome = evaluate_step(
                self.job,
                self.redactor,
                browser,
                evaluator,
                journal,
                &self.values,
                cancellation,
                &mut self.policy,
                self.index,
                step,
                &mut self.artifacts,
                mutations,
                observation_number,
            );
            if let Some(stop) = outcome {
                self.artifacts.stop = Some(stop);
                return;
            }
            if let Some(config) = config
                && let Err(error) = publish_active_checkpoint(
                    self.job,
                    config,
                    self.redactor,
                    &mut self.artifacts,
                    self.index,
                    control,
                )
            {
                self.artifacts.stop = Some(Stop::blocked(
                    "evidence_unavailable",
                    BTreeMap::from([(
                        "message".into(),
                        json!(self.redactor.redact_external_text(&error)),
                    )]),
                ));
                return;
            }
            self.advance_step();
        }
        self.drive_final_verification(browser, evaluator);
    }

    fn drive_final_verification(
        &mut self,
        browser: &impl DriveBrowser,
        evaluator: &impl manuvra_jev::Evaluator,
    ) {
        if self.verification_complete || self.artifacts.stop.is_some() {
            return;
        }
        match verify_final(
            self.job,
            self.redactor,
            browser,
            evaluator,
            &self.values,
            &mut self.policy,
            &mut self.artifacts,
        ) {
            VerificationProgress::Complete => self.verification_complete = true,
            VerificationProgress::Stop(stop) => self.artifacts.stop = Some(stop),
        }
    }
}

fn publish_active_checkpoint(
    job: &Job,
    config: &FlowConfig,
    redactor: &Redactor,
    artifacts: &mut RunArtifacts,
    completed_index: usize,
    control: &dyn HostedControl,
) -> Result<(), String> {
    if let Some(next) = artifacts.verdicts.get_mut(completed_index + 1) {
        next.result = VerdictResult::Unresolved;
        next.basis = None;
    }
    let manifest = config
        .evidence_root
        .join(&config.run_id)
        .join("manifest.json");
    control.publish_checkpoint(&json!({
        "schema_version":1,
        "request_id":config.request_id,
        "run_id":config.run_id,
        "state":"running",
        "terminal":false,
        "reason":null,
        "verdict":{
            "overall":VerdictResult::Unresolved,
            "steps":artifacts.verdicts,
            "expectations":job.expectations.iter().map(|expectation| json!({"id":redactor.redact_export_text(&expectation.id),"result":"not_run","numeric_checks":[]})).collect::<Vec<_>>(),
            "caller_assisted":artifacts.caller_assisted
        },
        "evidence":{"manifest":manifest,"complete":false},
        "escalation":null,
        "cleanup":{"browser":"alive","profile":"retained","application_state":"caller_owned"}
    }))
}
