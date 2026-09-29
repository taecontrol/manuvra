use crate::evidence::{self, EvidenceBundle, Redactor};
#[cfg(any(target_os = "linux", target_os = "macos", test))]
use crate::verification::{
    DoneResult, check_done, check_natural_done, natural_numeric_literals_satisfied, verify,
};
#[cfg(any(target_os = "linux", target_os = "macos", test))]
use crate::{actions, judgment, policy, values::Values};
#[cfg(any(target_os = "linux", target_os = "macos"))]
use manuvra_chrome::{BrowserConfig, OwnedBrowser};
#[cfg(any(target_os = "linux", target_os = "macos", test))]
use manuvra_chrome::{BrowserError, CapturedPage, Observation};
#[cfg(any(target_os = "linux", target_os = "macos", test))]
use manuvra_contract::DoneCondition;
use manuvra_contract::{
    Cleanup, Escalation, EvidenceRef, ExpectationVerdict, Job, Reason, RunResult, RunState,
    SchemaVersion, StepVerdict, Verdict, VerdictResult,
};
#[cfg(any(target_os = "linux", target_os = "macos", test))]
use manuvra_contract::{Disposition, DispositionKind, DispositionRequest};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
#[cfg(any(target_os = "linux", target_os = "macos"))]
use std::sync::OnceLock;
#[cfg(any(target_os = "linux", target_os = "macos", test))]
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

#[derive(Clone)]
pub struct FlowConfig {
    pub request_id: String,
    pub run_id: String,
    pub evidence_root: PathBuf,
    pub browser: Option<PathBuf>,
    pub headless: bool,
}

pub struct FlowOutcome {
    pub result: Value,
    pub exit_code: u8,
}

#[cfg(any(target_os = "linux", target_os = "macos", test))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HostedTermination {
    Aborted,
    PauseDeadlineElapsed,
    LifetimeElapsed,
    WatchdogLost,
}

#[cfg(any(target_os = "linux", target_os = "macos", test))]
#[derive(Debug, Clone)]
pub enum HostedEvent {
    Disposition(DispositionRequest),
    Termination(HostedTermination),
}

#[cfg(any(target_os = "linux", target_os = "macos", test))]
pub trait HostedControl {
    fn cancellation(&self) -> manuvra_chrome::InputCancellation;
    fn pause_deadline_unix_ms(&self) -> u64;
    fn publish_checkpoint(&self, result: &Value) -> Result<(), String>;
    fn wait_while_paused(&self, escalation_id: &str) -> HostedEvent;
    fn termination(&self) -> Option<HostedTermination>;
}

/// Publishes the honest `unsupported_platform` run on a platform without a supported runtime.
/// Supported platforms run every job through [`run_hosted`].
#[cfg(not(any(target_os = "linux", target_os = "macos")))]
pub fn run(job: &Job, config: FlowConfig, redactor: &Redactor) -> Result<FlowOutcome, String> {
    publish_without_browser(
        job,
        config,
        redactor,
        "unsupported_platform",
        BTreeMap::new(),
    )
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
pub fn run_hosted(
    job: &Job,
    config: FlowConfig,
    redactor: &Redactor,
    provider_key: Option<String>,
    control: &dyn HostedControl,
) -> Result<FlowOutcome, String> {
    run_with_browser(job, config, redactor, provider_key, control)
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn run_with_browser(
    job: &Job,
    config: FlowConfig,
    redactor: &Redactor,
    provider_key: Option<String>,
    control: &dyn HostedControl,
) -> Result<FlowOutcome, String> {
    let started = StartedBrowser::launch(browser_config(job, &config), target_url(job))
        .and_then(StartedBrowser::navigate);
    match started {
        Ok(started) => finish_browser_run(
            job,
            config,
            redactor,
            started.browser,
            started.provenance,
            provider_key,
            control,
        ),
        Err(StartupFailure::Launch(error)) => publish_browser_error(job, config, redactor, error),
        Err(StartupFailure::AfterLaunch(failure)) => publish_browser_error_with_provenance(
            job,
            config,
            redactor,
            failure.error,
            failure.provenance,
            failure.cleanup,
        ),
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
struct StartedBrowser {
    browser: OwnedBrowser,
    target_url: String,
    provenance: Value,
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
enum StartupFailure {
    Launch(BrowserError),
    AfterLaunch(Box<AfterLaunchFailure>),
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
struct AfterLaunchFailure {
    error: BrowserError,
    provenance: Value,
    cleanup: Cleanup,
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
impl StartedBrowser {
    fn launch(config: BrowserConfig, target_url: &str) -> Result<Self, StartupFailure> {
        OwnedBrowser::launch(config)
            .map(|browser| {
                let provenance = serde_json::to_value(browser.provenance())
                    .expect("browser provenance contains only serializable fields");
                Self {
                    browser,
                    target_url: target_url.into(),
                    provenance,
                }
            })
            .map_err(StartupFailure::Launch)
    }
    fn navigate(mut self) -> Result<Self, StartupFailure> {
        self.browser
            .navigate(&self.target_url)
            .map_err(|error| self.after_launch_failure(error))?;
        Ok(self)
    }

    fn after_launch_failure(&mut self, error: BrowserError) -> StartupFailure {
        StartupFailure::AfterLaunch(Box::new(AfterLaunchFailure {
            error,
            provenance: self.provenance.clone(),
            cleanup: cleanup_started_browser(&mut self.browser),
        }))
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn cleanup_started_browser(browser: &mut OwnedBrowser) -> Cleanup {
    if browser.close().is_ok() {
        Cleanup {
            browser: "closed".into(),
            profile: "removed".into(),
            application_state: "caller_owned".into(),
        }
    } else {
        Cleanup {
            browser: "closure_unconfirmed".into(),
            profile: "removal_unconfirmed".into(),
            application_state: "caller_owned".into(),
        }
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn finish_browser_run(
    job: &Job,
    config: FlowConfig,
    redactor: &Redactor,
    mut browser: OwnedBrowser,
    provenance: Value,
    provider_key: Option<String>,
    control: &dyn HostedControl,
) -> Result<FlowOutcome, String> {
    let mut journal =
        actions::DurableJournal::open(&config.evidence_root, &config.run_id, redactor)?;
    finish_hosted_browser_run(
        job,
        config,
        redactor,
        &mut browser,
        provenance,
        &LazyEvaluator::new(provider_key),
        &mut journal,
        &control.cancellation(),
        control,
    )
}

/// The action journal of a hosted run, cleared once the run's evidence is published.
#[cfg(any(target_os = "linux", target_os = "macos"))]
trait RunJournal: actions::ActionJournal {
    fn clear(&mut self) -> Result<(), String>;
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
impl RunJournal for actions::DurableJournal {
    fn clear(&mut self) -> Result<(), String> {
        actions::DurableJournal::clear(self)
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[allow(clippy::too_many_arguments)]
fn finish_hosted_browser_run(
    job: &Job,
    config: FlowConfig,
    redactor: &Redactor,
    browser: &mut impl HostedBrowser,
    provenance: Value,
    evaluator: &impl manuvra_jev::Evaluator,
    journal: &mut impl RunJournal,
    cancellation: &manuvra_chrome::InputCancellation,
    control: &dyn HostedControl,
) -> Result<FlowOutcome, String> {
    let mut machine = HostedMachine::new(job, redactor);
    loop {
        if machine.artifacts.stop.is_none() {
            machine.drive(
                browser,
                evaluator,
                journal,
                cancellation,
                Some(&config),
                control,
            );
        }
        if let Some(termination) = control.termination() {
            return publish_active_hosted_stop(
                job,
                config,
                redactor,
                browser,
                provenance,
                machine.artifacts,
                journal,
                termination,
            );
        }
        let Some(escalation_id) = machine.paused_escalation_id() else {
            return finish_hosted_terminal(
                job, config, redactor, browser, provenance, machine, journal, control,
            );
        };
        if let Some(outcome) = handle_hosted_pause(
            job,
            &config,
            redactor,
            browser,
            provenance.clone(),
            evaluator,
            journal,
            cancellation,
            control,
            &mut machine,
            &escalation_id,
        )? {
            return Ok(outcome);
        }
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[allow(clippy::too_many_arguments)]
fn handle_hosted_pause(
    job: &Job,
    config: &FlowConfig,
    redactor: &Redactor,
    browser: &mut impl HostedBrowser,
    provenance: Value,
    evaluator: &impl manuvra_jev::Evaluator,
    journal: &mut impl RunJournal,
    cancellation: &manuvra_chrome::InputCancellation,
    control: &dyn HostedControl,
    machine: &mut HostedMachine<'_>,
    escalation_id: &str,
) -> Result<Option<FlowOutcome>, String> {
    set_hosted_escalation_deadline(&mut machine.artifacts, control.pause_deadline_unix_ms());
    let published = publish_pause_checkpoint(
        job,
        config,
        redactor,
        provenance.clone(),
        &machine.artifacts,
    )?;
    control.publish_checkpoint(&published.result)?;
    machine.policy.pause();
    match control.wait_while_paused(escalation_id) {
        HostedEvent::Termination(termination) => finalize_paused_termination(
            job,
            config,
            redactor,
            browser,
            provenance,
            journal,
            control,
            machine,
            termination,
        )
        .map(Some),
        HostedEvent::Disposition(request) => {
            machine.policy.resume();
            machine
                .apply(request, browser, evaluator, journal, cancellation)
                .map_or(Ok(None), |termination| {
                    finalize_paused_termination(
                        job,
                        config,
                        redactor,
                        browser,
                        provenance,
                        journal,
                        control,
                        machine,
                        termination,
                    )
                    .map(Some)
                })
        }
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[allow(clippy::too_many_arguments)]
fn finalize_paused_termination(
    job: &Job,
    config: &FlowConfig,
    redactor: &Redactor,
    browser: &mut impl HostedBrowser,
    provenance: Value,
    journal: &mut impl RunJournal,
    control: &dyn HostedControl,
    machine: &mut HostedMachine<'_>,
    termination: HostedTermination,
) -> Result<FlowOutcome, String> {
    let artifacts = std::mem::replace(&mut machine.artifacts, RunArtifacts::new(job, redactor));
    publish_active_hosted_stop(
        job,
        config.clone(),
        redactor,
        browser,
        provenance,
        artifacts,
        journal,
        termination,
    )
    .and_then(|outcome| publish_terminal_checkpoint(control, journal, outcome))
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[allow(clippy::too_many_arguments)]
fn finish_hosted_terminal(
    job: &Job,
    config: FlowConfig,
    redactor: &Redactor,
    browser: &mut impl HostedBrowser,
    provenance: Value,
    machine: HostedMachine<'_>,
    journal: &mut impl RunJournal,
    control: &dyn HostedControl,
) -> Result<FlowOutcome, String> {
    let cleanup = browser.cleanup_hosted();
    let (state, reason, exit_code, overall) = terminal_fields(machine.artifacts.stop.clone());
    let result = run_result_with_assistance(
        job,
        &config,
        redactor,
        state,
        reason,
        overall,
        machine.artifacts.verdicts.clone(),
        machine.artifacts.escalation.clone(),
        cleanup.clone(),
        machine.artifacts.caller_assisted,
    )?;
    let mut result = result;
    apply_artifact_verdict(&mut result, &machine.artifacts)?;
    let outcome = publish_bundle(
        job,
        config,
        redactor,
        provenance,
        machine.artifacts.observations,
        machine.artifacts.decisions,
        machine.artifacts.steps,
        machine.artifacts.escalations,
        machine.artifacts.dispositions,
        machine.artifacts.verification,
        machine.artifacts.trace,
        cleanup,
        result,
        exit_code,
    )?;
    control.publish_checkpoint(&outcome.result)?;
    let _ = journal.clear();
    Ok(outcome)
}

#[cfg(any(target_os = "linux", target_os = "macos", test))]
struct HostedMachine<'a> {
    job: &'a Job,
    redactor: &'a Redactor,
    values: Values<'a>,
    policy: policy::Policy,
    index: usize,
    verification_complete: bool,
    artifacts: RunArtifacts,
}

#[cfg(any(target_os = "linux", target_os = "macos", test))]
impl<'a> HostedMachine<'a> {
    fn new(job: &'a Job, redactor: &'a Redactor) -> Self {
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
    fn paused_escalation_id(&self) -> Option<String> {
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

    fn advance_step(&mut self) {
        self.index += 1;
        if self.index < self.job.steps.len() {
            self.policy.begin_step();
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn drive(
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

    fn apply(
        &mut self,
        request: DispositionRequest,
        browser: &impl DriveBrowser,
        evaluator: &impl manuvra_jev::Evaluator,
        journal: &mut impl actions::ActionJournal,
        cancellation: &manuvra_chrome::InputCancellation,
    ) -> Option<HostedTermination> {
        let name = format!("request_{:04}", self.artifacts.dispositions.len() + 1);
        self.artifacts
            .dispositions
            .push((name, redacted_value(&request, self.redactor)));
        match request.disposition {
            Disposition::Abort(_) => {
                self.artifacts.caller_assisted = true;
                Some(HostedTermination::Aborted)
            }
            Disposition::RetryObservation(_) => {
                self.artifacts.caller_assisted = true;
                self.clear_pause();
                None
            }
            Disposition::Advance(advance) => {
                if self.artifacts.pending_verification.is_some() {
                    self.apply_verification_advance(&advance.rationale, browser, evaluator);
                } else {
                    self.apply_advance(&advance.rationale, browser);
                }
                None
            }
            Disposition::Execute(execute) => {
                if self.artifacts.pending_verification.is_some() {
                    self.reissue_verification("execute_not_permitted");
                } else {
                    self.apply_execute(
                        &execute.candidate_id,
                        browser,
                        evaluator,
                        journal,
                        cancellation,
                    );
                }
                None
            }
        }
    }

    fn clear_pause(&mut self) {
        self.artifacts.stop = None;
        self.artifacts.escalation = None;
        self.artifacts.pending = None;
        self.artifacts.pending_verification = None;
    }

    fn apply_verification_advance(
        &mut self,
        rationale: &str,
        browser: &impl DriveBrowser,
        evaluator: &impl manuvra_jev::Evaluator,
    ) {
        let Some(pending) = self.artifacts.pending_verification.clone() else {
            return;
        };
        if rationale.trim().is_empty() || !pending.attestable() {
            self.reissue_verification("advance_not_permitted");
            return;
        }
        let observation = match capture_verification_advance(
            self.job,
            self.redactor,
            browser,
            &self.policy,
            &mut self.artifacts,
        ) {
            Ok(observation) => observation,
            Err(stop) => return self.end_with(stop),
        };
        let prior_hash = relevant_state_hash(&pending.observation);
        let current_hash = relevant_state_hash(&observation);
        let identity_unchanged = pending.observation.document_id == observation.document_id;
        let facts_unchanged = prior_hash == current_hash;
        self.artifacts.trace.push(json!({
            "event":"verification_disposition_check",
            "kind":"advance",
            "document_identity_unchanged":identity_unchanged,
            "relevant_facts_unchanged":facts_unchanged,
            "prior_state_hash":prior_hash,
            "current_state_hash":current_hash,
        }));
        if !(identity_unchanged && facts_unchanged) {
            self.recheck_changed_verification(observation, evaluator);
            return;
        }
        self.accept_verification_attestation(rationale);
    }

    fn accept_verification_attestation(&mut self, rationale: &str) {
        for verdict in &mut self.artifacts.expectation_verdicts {
            if verdict.result == VerdictResult::Unresolved {
                verdict.result = VerdictResult::Satisfied;
            }
        }
        if let Some(Value::Object(record)) = &mut self.artifacts.verification {
            record.insert("basis".into(), json!("caller_attestation"));
            record.insert(
                "rationale".into(),
                json!(self.redactor.redact_export_text(rationale)),
            );
            record.insert(
                "expectations".into(),
                serde_json::to_value(&self.artifacts.expectation_verdicts).unwrap_or(Value::Null),
            );
        }
        self.artifacts.caller_assisted = true;
        self.verification_complete = true;
        self.clear_pause();
    }

    fn recheck_changed_verification(
        &mut self,
        observation: Observation,
        evaluator: &impl manuvra_jev::Evaluator,
    ) {
        self.artifacts.stop = None;
        self.artifacts.escalation = None;
        self.artifacts.pending_verification = None;
        let report = match evaluate_final(
            self.job,
            &observation,
            &self.values,
            evaluator,
            &mut self.policy,
        ) {
            Ok(report) => report,
            Err(stop) => return self.end_with(stop),
        };
        match record_verification(
            report,
            observation,
            self.redactor,
            &mut self.artifacts,
            "verification_state_changed",
        ) {
            VerificationProgress::Complete => self.verification_complete = true,
            VerificationProgress::Stop(stop) => self.artifacts.stop = Some(stop),
        }
    }

    /// Ends the run with a terminal stop reached while answering a disposition: the pause is
    /// cleared, so no escalation is published with it and the run is never driven again.
    fn end_with(&mut self, stop: Stop) {
        self.clear_pause();
        self.artifacts.stop = Some(stop);
    }

    fn reissue_verification(&mut self, reason: &'static str) {
        self.artifacts.stop = Some(escalate_verification(
            &mut self.artifacts,
            self.redactor,
            reason,
        ));
    }

    fn apply_advance(&mut self, rationale: &str, browser: &impl DriveBrowser) {
        let Some(pending) = self.artifacts.pending.clone() else {
            self.reissue("stale_escalation", None);
            return;
        };
        let observation = match self.capture_for_disposition(browser, "advance_observation") {
            Ok(captured) => {
                self.artifacts.observations.push(captured.artifact);
                captured.raw
            }
            Err(stop) => return self.end_with(stop),
        };
        match self.check_advance(&pending, &observation, rationale) {
            Ok(()) => self.attest_step(),
            Err(reason) => self.reissue(reason, Some(pending)),
        }
    }

    /// An advance attests the escalated observation, so the fresh one must show the same document
    /// and the same relevant facts, and the escalation must permit an attestation.
    fn check_advance(
        &mut self,
        pending: &PendingEscalation,
        current: &Observation,
        rationale: &str,
    ) -> Result<(), &'static str> {
        let step = &self.job.steps[self.index];
        let prior_hash = relevant_state_hash(&pending.observation);
        let current_hash = relevant_state_hash(current);
        let identity_unchanged = pending.observation.document_id == current.document_id;
        let facts_unchanged = prior_hash == current_hash;
        let permitted = advance_permitted(step, pending, rationale);
        self.artifacts.trace.push(json!({
            "event":"disposition_check",
            "kind":"advance",
            "permitted":permitted,
            "document_identity_unchanged":identity_unchanged,
            "relevant_state_unchanged":facts_unchanged,
            "prior_state_hash":prior_hash,
            "current_state_hash":current_hash,
            "numeric_checks_satisfied":natural_condition_numeric_checks_satisfied(step, pending),
            "pending_ambiguous_mutation":pending.ambiguous_mutation,
            "pending_candidate":pending.candidate.is_some(),
        }));
        if !(identity_unchanged && facts_unchanged) {
            return Err("relevant_state_changed");
        }
        permitted.then_some(()).ok_or("advance_not_permitted")
    }

    fn attest_step(&mut self) {
        let step = &self.job.steps[self.index];
        self.artifacts.caller_assisted = true;
        self.artifacts.verdicts[self.index] = StepVerdict {
            id: self.redactor.redact_export_text(&step.id),
            result: VerdictResult::Satisfied,
            basis: Some("caller_attestation".into()),
        };
        record_step(
            &mut self.artifacts,
            self.redactor,
            self.index,
            step,
            DoneResult::Satisfied,
            self.policy.step_mutations(),
            true,
            self.policy.active_ms(),
            "caller_attestation",
        );
        self.advance_step();
        self.clear_pause();
    }

    fn apply_execute(
        &mut self,
        candidate_id: &str,
        browser: &impl DriveBrowser,
        evaluator: &impl manuvra_jev::Evaluator,
        journal: &mut impl actions::ActionJournal,
        cancellation: &manuvra_chrome::InputCancellation,
    ) {
        let (pending, candidate) = match self.offered_candidate(candidate_id) {
            Ok(value) => value,
            Err(reason) => {
                self.reissue(reason, self.artifacts.pending.clone());
                return;
            }
        };
        let ResumeObservation {
            captured,
            done,
            noul,
        } = match self.resume_observation(browser, evaluator) {
            Ok(value) => value,
            Err(stop) => return self.end_with(stop),
        };
        let step = &self.job.steps[self.index];
        record_capture(
            &mut self.artifacts,
            self.redactor,
            step,
            &captured,
            "resume_observation",
            done,
        );
        if done == DoneResult::Satisfied {
            self.complete_resumed_step();
            return;
        }
        if done == DoneResult::Unknown {
            self.reissue_unknown_done(pending, captured.raw, noul);
            return;
        }
        self.perform_caller_candidate(
            pending,
            candidate,
            captured,
            done,
            browser,
            journal,
            cancellation,
        );
    }

    fn reissue_unknown_done(
        &mut self,
        mut pending: PendingEscalation,
        observation: Observation,
        noul: Option<f64>,
    ) {
        pending.done = DoneResult::Unknown;
        pending.noul = noul;
        pending.candidate = None;
        pending.ambiguous_mutation = false;
        pending.observation = observation;
        let reason = match self.job.steps[self.index].done_when {
            DoneCondition::NaturalLanguage(_) => "done_uncertain",
            DoneCondition::Structured(_) => "done_unknown",
        };
        self.reissue(reason, Some(pending));
    }

    fn offered_candidate(
        &self,
        candidate_id: &str,
    ) -> Result<(PendingEscalation, policy::Candidate), &'static str> {
        let pending = self.artifacts.pending.clone().ok_or("stale_escalation")?;
        let candidate = pending
            .candidate
            .clone()
            .filter(|candidate| candidate.id == candidate_id)
            .ok_or("candidate_not_offered")?;
        Ok((pending, candidate))
    }

    /// A fresh observation for a disposition. Like an autonomous observation it must be on an
    /// allowed origin and verifiably redacted before a provider call or completion rests on it.
    fn capture_for_disposition(
        &mut self,
        browser: &impl DriveBrowser,
        event: &str,
    ) -> Result<Captured, Stop> {
        let step = &self.job.steps[self.index];
        let captured = capture_step(
            browser,
            self.redactor,
            self.index + 1,
            self.artifacts.observations.len() + 1,
        )
        .map_err(control_stop)?;
        self.policy
            .check_origin(&captured.raw)
            .map_err(|stop| policy_stop(stop, self.redactor, step))?;
        if !captured.redaction_verified {
            record_capture(
                &mut self.artifacts,
                self.redactor,
                step,
                &captured,
                event,
                DoneResult::Unknown,
            );
            return Err(Stop::blocked(
                "redaction_unverifiable",
                step_detail(self.redactor, step),
            ));
        }
        Ok(captured)
    }

    fn resume_observation(
        &mut self,
        browser: &impl DriveBrowser,
        evaluator: &impl manuvra_jev::Evaluator,
    ) -> Result<ResumeObservation, Stop> {
        let captured = self.capture_for_disposition(browser, "resume_observation")?;
        let step = &self.job.steps[self.index];
        let (done, noul) = match &step.done_when {
            DoneCondition::Structured(assertions) => (
                check_done(assertions, &captured.raw, &self.job.values),
                None,
            ),
            DoneCondition::NaturalLanguage(condition) => {
                self.judge_resume_done(condition, &captured, evaluator)?
            }
        };
        Ok(ResumeObservation {
            captured,
            done,
            noul,
        })
    }

    fn judge_resume_done(
        &mut self,
        condition: &str,
        captured: &Captured,
        evaluator: &impl manuvra_jev::Evaluator,
    ) -> Result<(DoneResult, Option<f64>), Stop> {
        let step = &self.job.steps[self.index];
        let deadline = self
            .policy
            .record_model_call()
            .map_err(|stop| policy_stop(stop, self.redactor, step))?;
        let judgments = judgment::judge(
            evaluator,
            step,
            &captured.raw,
            &self.artifacts.trace,
            &self.values,
            deadline,
        )
        .map_err(|error| provider_stop(&error, self.redactor, step))?;
        self.artifacts.decisions.push((
            format!("d_{:04}", self.artifacts.decisions.len() + 1),
            redacted_value(&judgments, self.redactor),
        ));
        Ok((
            check_natural_done(condition, &captured.raw, judgments.step_done),
            Some(judgments.step_done),
        ))
    }

    fn complete_resumed_step(&mut self) {
        let step = &self.job.steps[self.index];
        self.artifacts.caller_assisted = true;
        let basis = match step.done_when {
            DoneCondition::Structured(_) => "structured",
            DoneCondition::NaturalLanguage(_) => "natural_language",
        };
        self.artifacts.verdicts[self.index] = StepVerdict {
            id: self.redactor.redact_export_text(&step.id),
            result: VerdictResult::Satisfied,
            basis: Some(basis.into()),
        };
        record_step(
            &mut self.artifacts,
            self.redactor,
            self.index,
            step,
            DoneResult::Satisfied,
            self.policy.step_mutations(),
            true,
            self.policy.active_ms(),
            basis,
        );
        self.advance_step();
        self.clear_pause();
    }

    #[allow(clippy::too_many_arguments)]
    fn perform_caller_candidate(
        &mut self,
        pending: PendingEscalation,
        candidate: policy::Candidate,
        captured: Captured,
        done: DoneResult,
        browser: &impl DriveBrowser,
        journal: &mut impl actions::ActionJournal,
        cancellation: &manuvra_chrome::InputCancellation,
    ) {
        let step = &self.job.steps[self.index];
        let permit = match self
            .policy
            .authorize_caller(step, &captured.raw, &candidate)
        {
            Ok(permit) => permit,
            Err(policy::PolicyStop::Uncertain(reason)) => {
                self.reissue_without_candidate(reason, pending, captured.raw);
                return;
            }
            Err(stop) => return self.end_with(policy_stop(stop, self.redactor, step)),
        };
        let journal_start = journal.entries().len();
        let performed = actions::perform_with_basis(
            permit,
            browser,
            &captured.raw,
            &self.values,
            journal,
            cancellation,
            "caller_authority",
        );
        self.artifacts
            .trace
            .extend(journal.entries()[journal_start..].iter().cloned());
        self.artifacts.caller_assisted = true;
        self.clear_pause();
        self.settle_caller_action(performed, pending, captured.raw, done);
    }

    /// A proven non-effect releases the replay key and reissues without a candidate, a possibly
    /// performed action reissues as an ambiguous mutation, and an evidence or readback failure
    /// ends the run.
    fn settle_caller_action(
        &mut self,
        performed: Result<actions::ActionFact, actions::ActionStop>,
        pending: PendingEscalation,
        observation: Observation,
        done: DoneResult,
    ) {
        match performed {
            Ok(fact) => self.policy.record_observed(&fact.replay_key),
            Err(actions::ActionStop::Reobserve(replay_key)) => {
                self.policy.release_not_performed(&replay_key);
                self.reissue_without_candidate(
                    "candidate_revalidation_failed",
                    pending,
                    observation,
                );
            }
            Err(actions::ActionStop::InvalidPermit) => {
                self.reissue_without_candidate(
                    "candidate_revalidation_failed",
                    pending,
                    observation,
                );
            }
            Err(actions::ActionStop::Uncertain) => self.reissue_ambiguous(done, observation),
            Err(stop) => {
                let step = &self.job.steps[self.index];
                let stop = terminal_action_stop(stop, &mut self.artifacts, self.redactor, step);
                self.end_with(stop);
            }
        }
    }

    fn reissue_without_candidate(
        &mut self,
        reason: &'static str,
        mut pending: PendingEscalation,
        observation: Observation,
    ) {
        pending.candidate = None;
        pending.observation = observation;
        self.reissue(reason, Some(pending));
    }

    fn reissue_ambiguous(&mut self, done: DoneResult, observation: Observation) {
        let pending = PendingEscalation {
            done,
            noul: None,
            candidate: None,
            observation,
            ambiguous_mutation: true,
        };
        self.reissue("action_outcome_uncertain", Some(pending));
    }

    fn reissue(&mut self, reason: &'static str, pending: Option<PendingEscalation>) {
        let step = &self.job.steps[self.index];
        self.artifacts.pending = pending;
        self.artifacts.stop = Some(reissue_escalation(
            &mut self.artifacts,
            self.redactor,
            self.index,
            step,
            reason,
        ));
    }
}

#[cfg(any(target_os = "linux", target_os = "macos", test))]
struct ResumeObservation {
    captured: Captured,
    done: DoneResult,
    noul: Option<f64>,
}

/// The stop of an action whose failure ends the run. An action journal that cannot be written
/// before dispatch blocks it, a readback mismatch fails it, and a dispatched action whose fact
/// cannot be journaled blocks it with evidence that stays incomplete for the rest of the run: its
/// outcome is unrecorded, so it is neither retried nor reported as not performed.
#[cfg(any(target_os = "linux", target_os = "macos", test))]
fn terminal_action_stop(
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

#[cfg(any(target_os = "linux", target_os = "macos", test))]
fn advance_permitted(
    step: &manuvra_contract::Step,
    pending: &PendingEscalation,
    rationale: &str,
) -> bool {
    let DoneCondition::NaturalLanguage(condition) = &step.done_when else {
        return false;
    };
    if pending.done != DoneResult::Unknown || pending.noul.is_none_or(|noul| noul <= 0.20) {
        return false;
    }
    natural_numeric_literals_satisfied(condition, &pending.observation)
        && !pending.ambiguous_mutation
        && pending.candidate.is_none()
        && !rationale.trim().is_empty()
}

#[cfg(any(target_os = "linux", target_os = "macos", test))]
fn natural_condition_numeric_checks_satisfied(
    step: &manuvra_contract::Step,
    pending: &PendingEscalation,
) -> bool {
    let DoneCondition::NaturalLanguage(condition) = &step.done_when else {
        return false;
    };
    natural_numeric_literals_satisfied(condition, &pending.observation)
}

#[cfg(any(target_os = "linux", target_os = "macos", test))]
fn relevant_state_hash(observation: &Observation) -> String {
    use sha2::{Digest, Sha256};

    hex::encode(Sha256::digest(relevant_state(observation).to_string()))
}

/// The observed facts an attestation rests on. Hover regions are part of the observation the
/// escalation publishes and of the provider's view; they are included only when listed, so pages
/// without them hash exactly as before hover regions existed.
#[cfg(any(target_os = "linux", target_os = "macos", test))]
fn relevant_state(observation: &Observation) -> Value {
    let mut state = json!({
        "url": observation.url,
        "route": observation.route,
        "title": observation.title,
        "dialogs": observation.dialogs,
        "dialog_texts": observation.dialog_texts,
        "focused": observation.focused,
        "focus_anchor": observation.focus_anchor,
        "visible_text": observation.visible_text,
        "covered_text": observation.covered_text,
        "elements": observation.elements,
        "coverage": observation.coverage,
    });
    if !observation.hover_regions.is_empty() {
        state["hover_regions"] = json!(observation.hover_regions);
    }
    state
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[allow(clippy::too_many_arguments)]
fn publish_active_hosted_stop(
    job: &Job,
    config: FlowConfig,
    redactor: &Redactor,
    browser: &mut impl HostedBrowser,
    provenance: Value,
    artifacts: RunArtifacts,
    journal: &mut impl RunJournal,
    termination: HostedTermination,
) -> Result<FlowOutcome, String> {
    let cleanup = browser.cleanup_hosted();
    let (state, code, exit_code) = hosted_termination_fields(termination);
    let result = run_result_with_assistance(
        job,
        &config,
        redactor,
        state,
        Some(Reason {
            code: code.into(),
            details: BTreeMap::new(),
        }),
        VerdictResult::Unresolved,
        artifacts.verdicts.clone(),
        artifacts.escalation.clone(),
        cleanup.clone(),
        artifacts.caller_assisted,
    )?;
    let mut result = result;
    apply_artifact_verdict(&mut result, &artifacts)?;
    publish_bundle(
        job,
        config,
        redactor,
        provenance,
        artifacts.observations,
        artifacts.decisions,
        artifacts.steps,
        artifacts.escalations,
        artifacts.dispositions,
        artifacts.verification,
        artifacts.trace,
        cleanup,
        result,
        exit_code,
    )
    .inspect(|_| {
        let _ = journal.clear();
    })
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn publish_terminal_checkpoint(
    control: &dyn HostedControl,
    journal: &mut impl RunJournal,
    outcome: FlowOutcome,
) -> Result<FlowOutcome, String> {
    control.publish_checkpoint(&outcome.result).map(|()| {
        let _ = journal.clear();
        outcome
    })
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn set_hosted_escalation_deadline(artifacts: &mut RunArtifacts, deadline: u64) {
    if let Some(escalation) = &mut artifacts.escalation {
        escalation.expires_at = deadline.to_string();
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn publish_pause_checkpoint(
    job: &Job,
    config: &FlowConfig,
    redactor: &Redactor,
    provenance: Value,
    artifacts: &RunArtifacts,
) -> Result<FlowOutcome, String> {
    let (state, reason, exit_code, overall) = terminal_fields(artifacts.stop.clone());
    let retained = Cleanup {
        browser: "alive".into(),
        profile: "retained".into(),
        application_state: "caller_owned".into(),
    };
    let mut checkpoint = run_result_with_assistance(
        job,
        config,
        redactor,
        state,
        reason,
        overall,
        artifacts.verdicts.clone(),
        artifacts.escalation.clone(),
        retained.clone(),
        artifacts.caller_assisted,
    )?;
    apply_artifact_verdict(&mut checkpoint, artifacts)?;
    checkpoint["terminal"] = json!(false);
    publish_bundle(
        job,
        config.clone(),
        redactor,
        provenance,
        artifacts.observations.clone(),
        artifacts.decisions.clone(),
        artifacts.steps.clone(),
        artifacts.escalations.clone(),
        artifacts.dispositions.clone(),
        artifacts.verification.clone(),
        artifacts.trace.clone(),
        retained,
        checkpoint,
        exit_code,
    )
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn hosted_termination_fields(termination: HostedTermination) -> (RunState, &'static str, u8) {
    match termination {
        HostedTermination::Aborted => (RunState::Aborted, "caller_aborted", 5),
        HostedTermination::PauseDeadlineElapsed => {
            (RunState::Expired, "resume_deadline_elapsed", 5)
        }
        HostedTermination::LifetimeElapsed => (RunState::Expired, "lifetime_elapsed", 5),
        HostedTermination::WatchdogLost => (RunState::Blocked, "watchdog_lost", 3),
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn browser_config(job: &Job, config: &FlowConfig) -> BrowserConfig {
    let (width, height) = viewport(job);
    BrowserConfig {
        explicit_binary: config.browser.clone(),
        headless: config.headless,
        width,
        height,
        inherit_process_group: true,
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn viewport(job: &Job) -> (u16, u16) {
    job.options
        .viewport
        .as_ref()
        .map_or((1120, 780), |value| (value.width, value.height))
}

#[cfg(any(target_os = "linux", target_os = "macos", test))]
fn target_url(job: &Job) -> &str {
    match &job.target {
        manuvra_contract::Target::Browser { url } => url,
    }
}

#[cfg(any(target_os = "linux", target_os = "macos", test))]
fn terminal_fields(stop: Option<Stop>) -> (RunState, Option<Reason>, u8, VerdictResult) {
    stop.map_or(
        (RunState::Passed, None, 0, VerdictResult::Satisfied),
        Stop::fields,
    )
}

#[cfg(any(target_os = "linux", target_os = "macos", test))]
trait BrowserPage {
    fn capture_redacted_page(&self, sensitive: &[String]) -> Result<CapturedPage, BrowserError>;
    fn observe_page(&self) -> Result<Observation, BrowserError>;
}

#[cfg(any(target_os = "linux", target_os = "macos", test))]
trait DriveBrowser: BrowserPage + actions::Performer {}
#[cfg(any(target_os = "linux", target_os = "macos", test))]
impl<T: BrowserPage + actions::Performer> DriveBrowser for T {}

#[cfg(any(target_os = "linux", target_os = "macos"))]
trait HostedBrowser: DriveBrowser {
    fn cleanup_hosted(&mut self) -> Cleanup;
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
impl HostedBrowser for OwnedBrowser {
    fn cleanup_hosted(&mut self) -> Cleanup {
        cleanup_started_browser(self)
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
struct LazyEvaluator {
    client: OnceLock<Result<manuvra_jev::Client, manuvra_jev::JevError>>,
    provider_key: Option<String>,
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
impl LazyEvaluator {
    fn new(provider_key: Option<String>) -> Self {
        Self {
            client: OnceLock::new(),
            provider_key,
        }
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
impl manuvra_jev::Evaluator for LazyEvaluator {
    fn evaluate(
        &self,
        request: &Value,
        deadline: Instant,
    ) -> Result<manuvra_jev::Evaluation, manuvra_jev::JevError> {
        self.client
            .get_or_init(|| {
                self.provider_key
                    .clone()
                    .map_or_else(manuvra_jev::Client::from_environment, |key| {
                        manuvra_jev::Client::from_key(key)
                    })
            })
            .as_ref()
            .map_err(Clone::clone)?
            .evaluate(request, deadline)
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
impl BrowserPage for OwnedBrowser {
    fn capture_redacted_page(&self, sensitive: &[String]) -> Result<CapturedPage, BrowserError> {
        self.capture_redacted(sensitive)
    }

    fn observe_page(&self) -> Result<Observation, BrowserError> {
        self.observe()
    }
}

#[cfg(any(target_os = "linux", target_os = "macos", test))]
#[derive(Clone)]
struct RunArtifacts {
    observations: Vec<(String, Value, Option<Vec<u8>>)>,
    decisions: Vec<(String, Value)>,
    steps: Vec<(String, Value)>,
    escalations: Vec<(String, Value)>,
    dispositions: Vec<(String, Value)>,
    trace: Vec<Value>,
    verdicts: Vec<StepVerdict>,
    expectation_verdicts: Vec<ExpectationVerdict>,
    verification: Option<Value>,
    stop: Option<Stop>,
    escalation: Option<Escalation>,
    pending: Option<PendingEscalation>,
    pending_verification: Option<PendingVerification>,
    caller_assisted: bool,
    /// Set once a dispatched action's fact could not be journaled; the evidence stays incomplete.
    evidence_incomplete: bool,
}

#[cfg(any(target_os = "linux", target_os = "macos", test))]
#[derive(Clone)]
struct PendingEscalation {
    done: DoneResult,
    noul: Option<f64>,
    candidate: Option<policy::Candidate>,
    observation: Observation,
    ambiguous_mutation: bool,
}

#[cfg(any(target_os = "linux", target_os = "macos", test))]
#[derive(Clone)]
struct PendingVerification {
    verdicts: Vec<ExpectationVerdict>,
    observation: Observation,
}

#[cfg(any(target_os = "linux", target_os = "macos", test))]
impl PendingVerification {
    fn attestable(&self) -> bool {
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

#[cfg(any(target_os = "linux", target_os = "macos", test))]
impl RunArtifacts {
    fn new(job: &Job, redactor: &Redactor) -> Self {
        Self {
            observations: Vec::new(),
            decisions: Vec::new(),
            steps: Vec::new(),
            escalations: Vec::new(),
            dispositions: Vec::new(),
            trace: Vec::new(),
            verdicts: job
                .steps
                .iter()
                .map(|step| StepVerdict {
                    id: redactor.redact_export_text(&step.id),
                    result: VerdictResult::NotRun,
                    basis: None,
                })
                .collect(),
            expectation_verdicts: job
                .expectations
                .iter()
                .map(|expectation| ExpectationVerdict {
                    id: redactor.redact_export_text(&expectation.id),
                    result: VerdictResult::NotRun,
                    noul: None,
                    numeric_checks: Vec::new(),
                })
                .collect(),
            verification: None,
            stop: None,
            escalation: None,
            pending: None,
            pending_verification: None,
            caller_assisted: false,
            evidence_incomplete: false,
        }
    }
}

#[cfg(any(target_os = "linux", target_os = "macos", test))]
#[derive(Clone)]
struct Stop {
    state: RunState,
    code: &'static str,
    exit_code: u8,
    details: BTreeMap<String, Value>,
}

#[cfg(any(target_os = "linux", target_os = "macos", test))]
impl Stop {
    fn blocked(code: &'static str, details: BTreeMap<String, Value>) -> Self {
        Self {
            state: RunState::Blocked,
            code,
            exit_code: 3,
            details,
        }
    }
    fn uncertain(code: &'static str, details: BTreeMap<String, Value>) -> Self {
        Self {
            state: RunState::Uncertain,
            code,
            exit_code: 2,
            details,
        }
    }
    fn failed(code: &'static str, details: BTreeMap<String, Value>) -> Self {
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

#[cfg(any(target_os = "linux", target_os = "macos", test))]
enum VerificationProgress {
    Complete,
    Stop(Stop),
}

#[cfg(any(target_os = "linux", target_os = "macos", test))]
#[allow(clippy::too_many_arguments)]
fn verify_final(
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

#[cfg(any(target_os = "linux", target_os = "macos", test))]
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
    Ok(captured)
}

#[cfg(any(target_os = "linux", target_os = "macos", test))]
fn capture_verification_advance(
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

#[cfg(any(target_os = "linux", target_os = "macos", test))]
fn evaluate_final(
    job: &Job,
    observation: &Observation,
    values: &Values<'_>,
    evaluator: &impl manuvra_jev::Evaluator,
    policy: &mut policy::Policy,
) -> Result<crate::verification::VerificationReport, Stop> {
    let deadline = if job.expectations.is_empty() {
        Instant::now()
    } else {
        policy
            .record_model_call()
            .map_err(verification_policy_stop)?
    };
    verify(&job.expectations, observation, values, evaluator, deadline)
        .map_err(|error| verification_provider_stop(&error))
}

#[cfg(any(target_os = "linux", target_os = "macos", test))]
fn record_verification(
    report: crate::verification::VerificationReport,
    observation: Observation,
    redactor: &Redactor,
    artifacts: &mut RunArtifacts,
    uncertainty_reason: &'static str,
) -> VerificationProgress {
    artifacts.expectation_verdicts = redacted_expectation_verdicts(&report.verdicts, redactor);
    artifacts.verification = Some(redacted_value(&report.record, redactor));
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

#[cfg(any(target_os = "linux", target_os = "macos", test))]
fn redacted_expectation_verdicts(
    verdicts: &[ExpectationVerdict],
    redactor: &Redactor,
) -> Vec<ExpectationVerdict> {
    serde_json::from_value(redacted_value(&verdicts, redactor)).unwrap_or_else(|_| {
        verdicts
            .iter()
            .map(|verdict| ExpectationVerdict {
                id: redactor.redact_export_text(&verdict.id),
                result: verdict.result,
                noul: verdict.noul,
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
    })
}

/// Final verification escalates its own uncertainty; a policy stop reached here ends the run.
#[cfg(any(target_os = "linux", target_os = "macos", test))]
fn verification_policy_stop(stop: policy::PolicyStop) -> Stop {
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

#[cfg(any(target_os = "linux", target_os = "macos", test))]
fn verification_provider_stop(error: &manuvra_jev::JevError) -> Stop {
    match error {
        manuvra_jev::JevError::InvalidResponse(_) | manuvra_jev::JevError::ModelChanged => {
            Stop::blocked("provider_invalid_response", BTreeMap::new())
        }
        manuvra_jev::JevError::Deadline => Stop::blocked("budget_exhausted", BTreeMap::new()),
        _ => Stop::blocked("provider_unavailable", BTreeMap::new()),
    }
}

#[cfg(any(target_os = "linux", target_os = "macos", test))]
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

#[cfg(any(target_os = "linux", target_os = "macos", test))]
#[allow(clippy::too_many_arguments)]
fn evaluate_step(
    job: &Job,
    redactor: &Redactor,
    browser: &impl DriveBrowser,
    evaluator: &impl manuvra_jev::Evaluator,
    journal: &mut impl actions::ActionJournal,
    values: &Values<'_>,
    cancellation: &manuvra_chrome::InputCancellation,
    policy: &mut policy::Policy,
    index: usize,
    step: &manuvra_contract::Step,
    artifacts: &mut RunArtifacts,
    initial_mutations: u8,
    initial_observation_number: usize,
) -> Option<Stop> {
    StepDriver {
        job,
        redactor,
        browser,
        evaluator,
        journal,
        values,
        cancellation,
        policy,
        index,
        step,
        artifacts,
        observation_number: initial_observation_number,
        done_unknown_reobserved: false,
        operation_gate_reobserved: false,
        mutations: initial_mutations,
        awaiting_final_done_reobservation: false,
    }
    .drive()
}

#[cfg(any(target_os = "linux", target_os = "macos", test))]
struct StepDriver<'a> {
    job: &'a Job,
    redactor: &'a Redactor,
    browser: &'a dyn DriveBrowser,
    evaluator: &'a dyn manuvra_jev::Evaluator,
    journal: &'a mut dyn actions::ActionJournal,
    values: &'a Values<'a>,
    cancellation: &'a manuvra_chrome::InputCancellation,
    policy: &'a mut policy::Policy,
    index: usize,
    step: &'a manuvra_contract::Step,
    artifacts: &'a mut RunArtifacts,
    observation_number: usize,
    done_unknown_reobserved: bool,
    operation_gate_reobserved: bool,
    mutations: u8,
    awaiting_final_done_reobservation: bool,
}

#[cfg(any(target_os = "linux", target_os = "macos", test))]
impl StepDriver<'_> {
    fn drive(&mut self) -> Option<Stop> {
        loop {
            match self.drive_iteration() {
                StepProgress::Continue => {}
                StepProgress::Complete => return None,
                StepProgress::Stop(stop) => return Some(stop),
            }
        }
    }

    fn drive_iteration(&mut self) -> StepProgress {
        // Every earlier action has settled, so a cancelled run stops before observing again. The
        // hosted loop publishes the termination that cancelled it in place of this stop.
        if self.cancellation.is_cancelled() {
            return StepProgress::Stop(Stop::blocked(
                "run_cancelled",
                step_detail(self.redactor, self.step),
            ));
        }
        let captured = match self.capture_guarded() {
            Ok(value) => value,
            Err(stop) => return StepProgress::Stop(stop),
        };
        match &self.step.done_when {
            DoneCondition::Structured(assertions) => {
                let done = check_done(assertions, &captured.raw, &self.job.values);
                self.record(&captured, done);
                if let Err(stop) = self.policy.check_active() {
                    return StepProgress::Stop(policy_stop(stop, self.redactor, self.step));
                }
                self.done_progress(done, &captured)
            }
            DoneCondition::NaturalLanguage(condition) => {
                self.natural_done_progress(condition, &captured)
            }
        }
    }

    fn capture_guarded(&mut self) -> Result<Captured, Stop> {
        let captured = self.capture()?;
        self.policy
            .check_origin(&captured.raw)
            .map_err(|stop| policy_stop(stop, self.redactor, self.step))?;
        Ok(captured)
    }

    fn natural_done_progress(&mut self, condition: &str, captured: &Captured) -> StepProgress {
        let judgments = match self.natural_judgments(captured) {
            Ok(value) => value,
            Err(stop) => return StepProgress::Stop(stop),
        };
        let done = check_natural_done(condition, &captured.raw, judgments.step_done);
        self.record(captured, done);
        self.record_decision(&judgments);
        if done == DoneResult::NotSatisfied && self.mutations >= self.step.mutation_limit {
            return self.after_mutation_limit(done);
        }
        let next = self.policy.decide(
            self.step,
            &captured.raw,
            &judgments,
            done,
            self.done_unknown_reobserved,
            self.operation_gate_reobserved,
        );
        self.apply_next(done, captured, &judgments, next)
    }

    fn natural_judgments(&mut self, captured: &Captured) -> Result<judgment::Judgments, Stop> {
        if !captured.redaction_verified {
            self.record(captured, DoneResult::Unknown);
            return Err(Stop::blocked(
                "redaction_unverifiable",
                step_detail(self.redactor, self.step),
            ));
        }
        let deadline = self
            .policy
            .record_model_call()
            .map_err(|stop| policy_stop(stop, self.redactor, self.step))?;
        self.judge(captured, deadline)
    }

    fn after_mutation_limit(&mut self, done: DoneResult) -> StepProgress {
        if !self.awaiting_final_done_reobservation {
            self.awaiting_final_done_reobservation = true;
            self.pause_before_reobservation(Duration::from_millis(300));
            StepProgress::Continue
        } else {
            StepProgress::Stop(self.done_condition_failed(done))
        }
    }

    fn capture(&mut self) -> Result<Captured, Stop> {
        self.observation_number += 1;
        capture_step(
            self.browser,
            self.redactor,
            self.index + 1,
            self.observation_number,
        )
        .map_err(control_stop)
    }

    fn record(&mut self, captured: &Captured, done: DoneResult) {
        record_capture(
            self.artifacts,
            self.redactor,
            self.step,
            captured,
            if self.done_unknown_reobserved
                || self.operation_gate_reobserved
                || self.awaiting_final_done_reobservation
            {
                "reobservation"
            } else {
                "observation"
            },
            done,
        );
    }

    fn done_progress(&mut self, done: DoneResult, captured: &Captured) -> StepProgress {
        if !captured.redaction_verified {
            return StepProgress::Stop(Stop::blocked(
                "redaction_unverifiable",
                step_detail(self.redactor, self.step),
            ));
        }
        match done {
            DoneResult::Satisfied => {
                self.complete_step(done);
                StepProgress::Complete
            }
            DoneResult::Unknown => self.unknown_done(done, captured),
            DoneResult::NotSatisfied => self.not_done(done, captured),
        }
    }

    fn complete_step(&mut self, done: DoneResult) {
        self.artifacts.verdicts[self.index] = StepVerdict {
            id: self.redactor.redact_export_text(&self.step.id),
            result: VerdictResult::Satisfied,
            basis: Some(self.done_basis().into()),
        };
        self.record_step(done, true);
    }

    fn unknown_done(&mut self, done: DoneResult, captured: &Captured) -> StepProgress {
        if !self.done_unknown_reobserved {
            self.done_unknown_reobserved = true;
            self.pause_before_reobservation(Duration::from_millis(300));
            return StepProgress::Continue;
        }
        StepProgress::Stop(escalate(
            self.artifacts,
            self.redactor,
            self.index,
            self.step,
            done,
            None,
            PendingEscalation {
                done,
                noul: None,
                candidate: None,
                observation: captured.raw.clone(),
                ambiguous_mutation: false,
            },
            "done_unknown",
        ))
    }

    fn not_done(&mut self, done: DoneResult, captured: &Captured) -> StepProgress {
        if self.mutations >= self.step.mutation_limit {
            return self.after_mutation_limit(done);
        }
        let deadline = match self.policy.record_model_call() {
            Ok(deadline) => deadline,
            Err(stop) => return StepProgress::Stop(policy_stop(stop, self.redactor, self.step)),
        };
        let judgments = match self.judge(captured, deadline) {
            Ok(value) => value,
            Err(stop) => return StepProgress::Stop(stop),
        };
        self.record_decision(&judgments);
        self.apply_policy(done, captured, &judgments)
    }

    fn done_condition_failed(&mut self, done: DoneResult) -> Stop {
        self.artifacts.verdicts[self.index] = StepVerdict {
            id: self.redactor.redact_export_text(&self.step.id),
            result: VerdictResult::NotSatisfied,
            basis: Some(self.done_basis().into()),
        };
        self.record_step(done, true);
        Stop::failed(
            "done_condition_not_met",
            BTreeMap::from([
                (
                    "step_id".into(),
                    json!(self.redactor.redact_export_text(&self.step.id)),
                ),
                ("mutation_limit_consumed".into(), json!(true)),
            ]),
        )
    }

    fn judge(&self, captured: &Captured, deadline: Instant) -> Result<judgment::Judgments, Stop> {
        judgment::judge(
            self.evaluator,
            self.step,
            &captured.raw,
            &self.artifacts.trace,
            self.values,
            deadline,
        )
        .map_err(|error| provider_stop(&error, self.redactor, self.step))
    }

    fn record_decision(&mut self, judgments: &judgment::Judgments) {
        let name = format!("d_{:04}", self.artifacts.decisions.len() + 1);
        self.artifacts
            .decisions
            .push((name, redacted_value(judgments, self.redactor)));
    }

    fn apply_policy(
        &mut self,
        done: DoneResult,
        captured: &Captured,
        judgments: &judgment::Judgments,
    ) -> StepProgress {
        let next = self.policy.decide(
            self.step,
            &captured.raw,
            judgments,
            done,
            self.done_unknown_reobserved,
            self.operation_gate_reobserved,
        );
        self.apply_next(done, captured, judgments, next)
    }

    fn apply_next(
        &mut self,
        done: DoneResult,
        captured: &Captured,
        judgments: &judgment::Judgments,
        next: policy::Next,
    ) -> StepProgress {
        match next {
            policy::Next::Complete => {
                self.complete_step(done);
                StepProgress::Complete
            }
            policy::Next::ReobserveDone => {
                self.done_unknown_reobserved = true;
                self.pause_before_reobservation(Duration::from_millis(300));
                StepProgress::Continue
            }
            policy::Next::ReobserveOperation => {
                self.operation_gate_reobserved = true;
                self.pause_before_reobservation(Duration::from_millis(300));
                StepProgress::Continue
            }
            other => self.apply_after_reobserve(done, captured, judgments, other),
        }
    }

    fn apply_after_reobserve(
        &mut self,
        done: DoneResult,
        captured: &Captured,
        judgments: &judgment::Judgments,
        next: policy::Next,
    ) -> StepProgress {
        match next {
            policy::Next::Wait => {
                self.done_unknown_reobserved = false;
                self.operation_gate_reobserved = false;
                std::thread::sleep(Duration::from_millis(150));
                StepProgress::Continue
            }
            other => self.apply_terminal_next(done, captured, judgments, other),
        }
    }

    fn apply_terminal_next(
        &mut self,
        done: DoneResult,
        captured: &Captured,
        judgments: &judgment::Judgments,
        next: policy::Next,
    ) -> StepProgress {
        match next {
            policy::Next::Mutate(permit) if self.force_stop_before_first_mutation(&permit) => {
                let candidate = self.policy.release_unused(permit);
                StepProgress::Stop(escalate(
                    self.artifacts,
                    self.redactor,
                    self.index,
                    self.step,
                    done,
                    Some(judgments),
                    PendingEscalation {
                        done,
                        noul: natural_noul(self.step, judgments),
                        candidate: Some(candidate),
                        observation: captured.raw.clone(),
                        ambiguous_mutation: true,
                    },
                    "debug_forced_stop",
                ))
            }
            policy::Next::Mutate(permit) => self.mutate(done, captured, judgments, *permit),
            policy::Next::Stop(stop) => {
                StepProgress::Stop(self.policy_stop(done, captured, judgments, stop))
            }
            policy::Next::Complete
            | policy::Next::ReobserveDone
            | policy::Next::ReobserveOperation
            | policy::Next::Wait => {
                unreachable!("earlier next variants were handled")
            }
        }
    }

    /// The forced debug stop fires on the step's first mutating permit; scrolls and hovers before
    /// it are performed.
    fn force_stop_before_first_mutation(&self, permit: &policy::Permit) -> bool {
        permit.operation().mutates()
            && self.mutations == 0
            && self
                .job
                .options
                .debug
                .as_ref()
                .is_some_and(|debug| debug.force_stop_at_step == self.step.id)
    }

    fn mutate(
        &mut self,
        done: DoneResult,
        captured: &Captured,
        judgments: &judgment::Judgments,
        permit: policy::Permit,
    ) -> StepProgress {
        self.done_unknown_reobserved = false;
        self.operation_gate_reobserved = false;
        let journal_start = self.journal.entries().len();
        let result = actions::perform(
            permit,
            self.browser,
            &captured.raw,
            self.values,
            self.journal,
            self.cancellation,
        );
        self.artifacts
            .trace
            .extend(self.journal.entries()[journal_start..].iter().cloned());
        match result {
            Ok(fact) => {
                self.policy.record_observed(&fact.replay_key);
                if fact.operation.mutates() {
                    self.mutations += 1;
                }
                self.awaiting_final_done_reobservation = false;
                debug_assert_eq!(
                    self.artifacts
                        .trace
                        .last()
                        .and_then(|event| event.get("event")),
                    Some(&json!("action_fact"))
                );
                StepProgress::Continue
            }
            Err(actions::ActionStop::Reobserve(replay_key)) => {
                self.policy.release_not_performed(&replay_key);
                self.done_unknown_reobserved = false;
                self.operation_gate_reobserved = false;
                StepProgress::Continue
            }
            Err(stop) => StepProgress::Stop(self.action_stop(done, captured, judgments, stop)),
        }
    }

    fn action_stop(
        &mut self,
        done: DoneResult,
        captured: &Captured,
        judgments: &judgment::Judgments,
        stop: actions::ActionStop,
    ) -> Stop {
        match stop {
            actions::ActionStop::Uncertain => {
                self.escalate_action(done, captured, judgments, "action_outcome_uncertain", true)
            }
            actions::ActionStop::InvalidPermit => self.escalate_action(
                done,
                captured,
                judgments,
                "candidate_revalidation_failed",
                false,
            ),
            actions::ActionStop::Reobserve(_) => {
                unreachable!("not-performed actions reobserve before stop mapping")
            }
            other => terminal_action_stop(other, self.artifacts, self.redactor, self.step),
        }
    }

    /// An action whose outcome may have changed the page is escalated as an ambiguous mutation;
    /// a permit that could not be prepared was never journaled or dispatched.
    fn escalate_action(
        &mut self,
        done: DoneResult,
        captured: &Captured,
        judgments: &judgment::Judgments,
        reason: &'static str,
        ambiguous_mutation: bool,
    ) -> Stop {
        escalate(
            self.artifacts,
            self.redactor,
            self.index,
            self.step,
            done,
            Some(judgments),
            PendingEscalation {
                done,
                noul: natural_noul(self.step, judgments),
                candidate: None,
                observation: captured.raw.clone(),
                ambiguous_mutation,
            },
            reason,
        )
    }

    fn policy_stop(
        &mut self,
        done: DoneResult,
        captured: &Captured,
        judgments: &judgment::Judgments,
        stop: policy::PolicyStop,
    ) -> Stop {
        match stop {
            policy::PolicyStop::Uncertain(reason) => {
                let candidate = (reason == "operation_below_gate" || reason == "key_below_gate")
                    .then(|| self.policy.caller_candidate(&captured.raw, judgments).ok())
                    .flatten();
                escalate(
                    self.artifacts,
                    self.redactor,
                    self.index,
                    self.step,
                    done,
                    Some(judgments),
                    PendingEscalation {
                        done,
                        noul: natural_noul(self.step, judgments),
                        candidate,
                        observation: captured.raw.clone(),
                        ambiguous_mutation: false,
                    },
                    reason,
                )
            }
            policy::PolicyStop::Blocked("value_not_provided") => {
                value_not_provided(self.redactor, self.step, captured, judgments, self.values)
            }
            other => policy_stop(other, self.redactor, self.step),
        }
    }

    fn pause_before_reobservation(&mut self, delay: Duration) {
        std::thread::sleep(delay);
    }

    fn record_step(&mut self, done: DoneResult, redaction_verified: bool) {
        record_step(
            self.artifacts,
            self.redactor,
            self.index,
            self.step,
            done,
            self.mutations,
            redaction_verified,
            self.policy.active_ms(),
            self.done_basis(),
        );
    }

    fn done_basis(&self) -> &'static str {
        match &self.step.done_when {
            DoneCondition::Structured(_) => "structured",
            DoneCondition::NaturalLanguage(_) => "natural_language",
        }
    }
}

#[cfg(any(target_os = "linux", target_os = "macos", test))]
enum StepProgress {
    Continue,
    Complete,
    Stop(Stop),
}

#[cfg(any(target_os = "linux", target_os = "macos", test))]
fn provider_stop(
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

#[cfg(any(target_os = "linux", target_os = "macos", test))]
fn value_not_provided(
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

#[cfg(any(target_os = "linux", target_os = "macos", test))]
#[allow(clippy::too_many_arguments)]
fn record_step(
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

#[cfg(any(target_os = "linux", target_os = "macos", test))]
fn step_detail(redactor: &Redactor, step: &manuvra_contract::Step) -> BTreeMap<String, Value> {
    BTreeMap::from([(
        "step_id".into(),
        json!(redactor.redact_export_text(&step.id)),
    )])
}

/// A policy stop that ends the run. Policy uncertainty is escalated where it arises, with the
/// observation and candidates a disposition needs, so an uncertain stop is never published
/// without an escalation.
#[cfg(any(target_os = "linux", target_os = "macos", test))]
fn policy_stop(
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

#[cfg(any(target_os = "linux", target_os = "macos", test))]
#[allow(clippy::too_many_arguments)]
fn escalate(
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
    let stopped = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .to_string();
    let offered_candidate = pending.candidate.as_ref().map(policy::Candidate::offered);
    let candidates = offered_candidate.as_ref().map_or_else(
        || judgments.map(|value|json!({"operation":value.operation,"click_target":value.click_target,"type_target":value.type_target,"select_target":value.select_target,"type_value":value.type_value,"key":value.key})).unwrap_or_else(||json!({})),
        |candidate| json!([candidate]),
    );
    let latest=artifacts.observations.last().map(|(name,_,png)|json!({"snapshot":format!("observations/{name}.json"),"screenshot":png.as_ref().map(|_|format!("observations/{name}.png"))})).unwrap_or(Value::Null);
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

#[cfg(any(target_os = "linux", target_os = "macos", test))]
fn allowed_dispositions(
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

#[cfg(any(target_os = "linux", target_os = "macos", test))]
fn escalate_verification(
    artifacts: &mut RunArtifacts,
    redactor: &Redactor,
    reason: &'static str,
) -> Stop {
    let id = format!("e_{}", artifacts.escalations.len() + 1);
    let stopped = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .to_string();
    let observation = artifacts
        .observations
        .last()
        .map(|(name, _, png)| {
            json!({
                "snapshot":format!("observations/{name}.json"),
                "screenshot":png.as_ref().map(|_|format!("observations/{name}.png")),
            })
        })
        .unwrap_or(Value::Null);
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

#[cfg(any(target_os = "linux", target_os = "macos", test))]
fn natural_noul(step: &manuvra_contract::Step, judgments: &judgment::Judgments) -> Option<f64> {
    matches!(step.done_when, DoneCondition::NaturalLanguage(_)).then_some(judgments.step_done)
}

#[cfg(any(target_os = "linux", target_os = "macos", test))]
fn reissue_escalation(
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

#[cfg(any(target_os = "linux", target_os = "macos", test))]
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

#[cfg(any(target_os = "linux", target_os = "macos", test))]
fn redacted_value(value: &impl serde::Serialize, redactor: &Redactor) -> Value {
    let mut value = serde_json::to_value(value).unwrap_or(Value::Null);
    redactor.redact_export_value(&mut value);
    value
}

#[cfg(any(target_os = "linux", target_os = "macos", test))]
fn record_capture(
    artifacts: &mut RunArtifacts,
    redactor: &Redactor,
    step: &manuvra_contract::Step,
    captured: &Captured,
    event: &str,
    done: DoneResult,
) {
    artifacts.observations.push(captured.artifact.clone());
    artifacts
        .trace
        .push(json!({"event":event,"step_id":redactor.redact_export_text(&step.id),"done":done}));
}

#[cfg(any(target_os = "linux", target_os = "macos", test))]
fn control_stop(message: String) -> Stop {
    Stop::blocked(
        "browser_control_failed",
        BTreeMap::from([("message".into(), json!(message))]),
    )
}

#[cfg(any(target_os = "linux", target_os = "macos", test))]
struct Captured {
    raw: Observation,
    artifact: (String, Value, Option<Vec<u8>>),
    redaction_verified: bool,
}

#[cfg(any(target_os = "linux", target_os = "macos", test))]
fn capture_step(
    browser: &(impl BrowserPage + ?Sized),
    redactor: &Redactor,
    step: usize,
    attempt: usize,
) -> Result<Captured, String> {
    let name = format!("o_{step:04}_{attempt}");
    let sensitive = redactor.sensitive_values();
    match browser.capture_redacted_page(&sensitive) {
        Ok(captured) if captured.redaction.verifies(sensitive.len()) => {
            let observation = redacted_observation(&captured.observation, redactor)?;
            Ok(Captured {
                raw: captured.observation,
                artifact: (name, observation, Some(captured.screenshot.bytes)),
                redaction_verified: true,
            })
        }
        Ok(_) => withheld_capture(browser, redactor, name),
        Err(BrowserError::Control(message)) if message == "redaction_unverifiable" => {
            withheld_capture(browser, redactor, name)
        }
        Err(error) => Err(redactor.redact_external_text(&error.to_string())),
    }
}

#[cfg(any(target_os = "linux", target_os = "macos", test))]
fn withheld_capture(
    browser: &(impl BrowserPage + ?Sized),
    redactor: &Redactor,
    name: String,
) -> Result<Captured, String> {
    let raw = browser
        .observe_page()
        .map_err(|error| redactor.redact_external_text(&error.to_string()))?;
    let mut observation = redacted_observation(&raw, redactor)?;
    if let Value::Object(fields) = &mut observation {
        fields.insert(
            "screenshot".into(),
            json!({"withheld":"redaction_unverifiable"}),
        );
    }
    Ok(Captured {
        raw,
        artifact: (name, observation, None),
        redaction_verified: false,
    })
}

#[cfg(any(target_os = "linux", target_os = "macos", test))]
fn redacted_observation(raw: &Observation, redactor: &Redactor) -> Result<Value, String> {
    let redact = |text: &str| redactor.redact_external_text(text);
    let elements = raw
        .elements
        .iter()
        .map(|element| {
            json!({
                "index":element.index,
                "role":element.role,
                "name":redact(&element.name),
                "input_type":element.input_type,
                "value":redact(&element.value),
                "checked":element.checked,
                "selected":element.selected,
                "expanded":element.expanded,
                "disabled":element.disabled,
                "in_dialog":element.in_dialog.as_ref().map(|dialog|redact(dialog)),
                "operations":element.operations,
                "select_options":element.select_options.iter().map(|option|json!({
                    "label":redact(&option.label),
                    "value":redact(&option.value),
                    "disabled":option.disabled,
                    "selected":option.selected,
                })).collect::<Vec<_>>(),
                "rect":element.rect,
            })
        })
        .collect::<Vec<_>>();
    let mut exported = json!({
        "url":redact(&raw.url),
        "route":redact(&raw.route),
        "title":redact(&raw.title),
        "dialogs":raw.dialogs.iter().map(|dialog|redact(dialog)).collect::<Vec<_>>(),
        "focused":raw.focused,
        "focus_anchor":raw.focus_anchor.as_ref().map(|anchor|json!({
            "role":redact(&anchor.role),
            "name":redact(&anchor.name),
            "in_dialog":anchor.in_dialog.as_ref().map(|dialog|redact(dialog)),
            "covered":anchor.covered,
            "active_descendant":anchor.active_descendant.as_ref().map(|item|json!({
                "id":redact(&item.id),"role":redact(&item.role),"name":redact(&item.name),
                "selected":item.selected,"checked":item.checked,
            })),
            "expanded":anchor.expanded,"selected":anchor.selected,"checked":anchor.checked,
        })),
        "visible_text":redact(&raw.visible_text),
        "covered_text":redact(&raw.covered_text),
        "dialog_texts":raw.dialog_texts.iter().map(|(name,text)|(redact(name),redact(text))).collect::<BTreeMap<_,_>>(),
        "elements":elements,
        "viewport":raw.viewport,
        "coverage":raw.coverage,
    });
    if let Value::Object(fields) = &mut exported {
        fields.extend(exported_hover_regions(raw, redactor));
    }
    Ok(exported)
}

/// Hover regions appear in evidence only when present, with page text redacted and without the
/// internal dispatch identity.
#[cfg(any(target_os = "linux", target_os = "macos", test))]
fn exported_hover_regions(raw: &Observation, redactor: &Redactor) -> Vec<(String, Value)> {
    let redact = |text: &str| redactor.redact_external_text(text);
    let regions = raw
        .hover_regions
        .iter()
        .map(|region| {
            json!({
                "index":region.index,
                "name":redact(&region.name),
                "reveals_on_hover":region.reveals_on_hover.iter().map(|name|redact(name)).collect::<Vec<_>>(),
            })
        })
        .collect::<Vec<_>>();
    let mut fields = Vec::new();
    if !regions.is_empty() {
        fields.push(("hover_regions".to_owned(), Value::Array(regions)));
    }
    if raw.hover_regions_truncated {
        fields.push(("hover_regions_truncated".to_owned(), Value::Bool(true)));
    }
    fields
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn publish_browser_error(
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
        BrowserError::Unavailable | BrowserError::UnsupportedPlatform => Cleanup {
            browser: "not_started".into(),
            profile: "not_created".into(),
            application_state: "caller_owned".into(),
        },
        _ => Cleanup {
            browser: "closure_unconfirmed".into(),
            profile: "removal_unconfirmed".into(),
            application_state: "caller_owned".into(),
        },
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
fn publish_browser_error_with_provenance(
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
    let verdicts = job
        .steps
        .iter()
        .map(|step| StepVerdict {
            id: redactor.redact_export_text(&step.id),
            result: VerdictResult::NotRun,
            basis: None,
        })
        .collect();
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
        verdicts,
        None,
        cleanup.clone(),
    )?;
    publish_bundle(
        job,
        config,
        redactor,
        provenance,
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        None,
        vec![json!({"event":"stop","reason":code})],
        cleanup,
        result,
        3,
    )
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn publish_without_browser(
    job: &Job,
    config: FlowConfig,
    redactor: &Redactor,
    code: &str,
    details: BTreeMap<String, Value>,
) -> Result<FlowOutcome, String> {
    let cleanup = Cleanup {
        browser: "not_started".into(),
        profile: "not_created".into(),
        application_state: "caller_owned".into(),
    };
    let verdicts = job
        .steps
        .iter()
        .map(|step| StepVerdict {
            id: redactor.redact_export_text(&step.id),
            result: VerdictResult::NotRun,
            basis: None,
        })
        .collect();
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
        verdicts,
        None,
        cleanup.clone(),
    )?;
    publish_bundle(
        job,
        config,
        redactor,
        json!({"browser_path":null,"browser_version":null,"viewport":null,"display_mode":null}),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        None,
        vec![json!({"event":"stop","reason":code})],
        cleanup,
        result,
        3,
    )
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
            expectations: job
                .expectations
                .iter()
                .map(|expectation| ExpectationVerdict {
                    id: redactor.redact_export_text(&expectation.id),
                    result: VerdictResult::NotRun,
                    noul: None,
                    numeric_checks: Vec::new(),
                })
                .collect(),
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
fn run_result_with_assistance(
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
fn apply_artifact_verdict(result: &mut Value, artifacts: &RunArtifacts) -> Result<(), String> {
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

#[allow(clippy::too_many_arguments)]
fn publish_bundle(
    job: &Job,
    config: FlowConfig,
    redactor: &Redactor,
    mut provenance: Value,
    observations: Vec<(String, Value, Option<Vec<u8>>)>,
    decisions: Vec<(String, Value)>,
    steps: Vec<(String, Value)>,
    escalations: Vec<(String, Value)>,
    dispositions: Vec<(String, Value)>,
    verification: Option<Value>,
    trace: Vec<Value>,
    cleanup: Cleanup,
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
        observations,
        decisions,
        steps,
        escalations,
        dispositions,
        verification,
        trace,
        cleanup: serde_json::to_value(cleanup).map_err(|e| e.to_string())?,
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

#[cfg(any(target_os = "linux", target_os = "macos", test))]
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
    use manuvra_chrome::{
        Coverage, Element, FocusAnchor, PerformError, PerformFact, PreparedInput,
        PreparedOperation, Rect, RedactionProof, Screenshot, ViewportState,
    };
    use serde_json::json;
    use std::collections::{BTreeMap, VecDeque};
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    use std::io::{Read, Write};
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    use std::net::{TcpListener, TcpStream};
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    use std::sync::Arc;
    use std::sync::Mutex;
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    use std::sync::atomic::{AtomicBool, Ordering};
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    use std::thread;
    use tempfile::TempDir;

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    struct OriginFixture {
        start_port: u16,
        foreign_port: u16,
        stop: Arc<AtomicBool>,
        workers: Vec<thread::JoinHandle<()>>,
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    impl OriginFixture {
        fn start() -> Self {
            let start = TcpListener::bind("127.0.0.1:0").unwrap();
            let foreign = TcpListener::bind("127.0.0.1:0").unwrap();
            start.set_nonblocking(true).unwrap();
            foreign.set_nonblocking(true).unwrap();
            let start_port = start.local_addr().unwrap().port();
            let foreign_port = foreign.local_addr().unwrap().port();
            let stop = Arc::new(AtomicBool::new(false));
            let start_body = format!(
                "<!doctype html><title>Origin guard</title><a href=\"http://127.0.0.1:{foreign_port}/landing\">Leave origin</a>"
            );
            let foreign_body =
                "<!doctype html><title>Foreign origin</title><p>Committed foreign origin</p>"
                    .to_owned();
            let workers = vec![
                spawn_page_server(start, start_body, Arc::clone(&stop)),
                spawn_page_server(foreign, foreign_body, Arc::clone(&stop)),
            ];
            Self {
                start_port,
                foreign_port,
                stop,
                workers,
            }
        }

        fn start_url(&self) -> String {
            format!("http://127.0.0.1:{}/", self.start_port)
        }

        fn foreign_origin(&self) -> String {
            format!("http://127.0.0.1:{}", self.foreign_port)
        }
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    impl Drop for OriginFixture {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::Relaxed);
            let _ = TcpStream::connect(("127.0.0.1", self.start_port));
            let _ = TcpStream::connect(("127.0.0.1", self.foreign_port));
            for worker in self.workers.drain(..) {
                worker.join().unwrap();
            }
        }
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    fn spawn_page_server(
        listener: TcpListener,
        body: String,
        stop: Arc<AtomicBool>,
    ) -> thread::JoinHandle<()> {
        thread::spawn(move || {
            while !stop.load(Ordering::Relaxed) {
                match listener.accept() {
                    Ok((mut stream, _)) => serve_origin_fixture(&mut stream, &body),
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(5));
                    }
                    Err(error) => panic!("origin fixture failed: {error}"),
                }
            }
        })
    }

    /// Serves the synthetic hover-reveal page on a temporary local origin.
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    struct HoverRevealFixture {
        port: u16,
        stop: Arc<AtomicBool>,
        worker: Option<thread::JoinHandle<()>>,
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    impl HoverRevealFixture {
        fn start() -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            listener.set_nonblocking(true).unwrap();
            let port = listener.local_addr().unwrap().port();
            let stop = Arc::new(AtomicBool::new(false));
            let body = include_str!("../../../tests/browser/hover-reveal.html").to_owned();
            Self {
                port,
                worker: Some(spawn_page_server(listener, body, Arc::clone(&stop))),
                stop,
            }
        }

        fn url(&self) -> String {
            format!("http://127.0.0.1:{}/plan", self.port)
        }
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    impl Drop for HoverRevealFixture {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::Relaxed);
            let _ = TcpStream::connect(("127.0.0.1", self.port));
            if let Some(worker) = self.worker.take() {
                worker.join().unwrap();
            }
        }
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    fn serve_origin_fixture(stream: &mut TcpStream, body: &str) {
        let mut request = Vec::new();
        let mut buffer = [0_u8; 1024];
        while !request.windows(4).any(|window| window == b"\r\n\r\n") {
            let read = stream.read(&mut buffer).unwrap();
            if read == 0 {
                break;
            }
            request.extend_from_slice(&buffer[..read]);
        }
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            body.len(),
            body
        );
        stream.write_all(response.as_bytes()).unwrap();
    }

    /// The owned Chromium behind the hosted loop, recording the page it shows when the loop
    /// closes it.
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    struct LiveBrowser {
        browser: OwnedBrowser,
        final_page: Option<Observation>,
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    impl LiveBrowser {
        fn open(url: &str) -> Self {
            let browser = OwnedBrowser::launch(BrowserConfig {
                explicit_binary: None,
                headless: true,
                width: 1120,
                height: 780,
                inherit_process_group: false,
            })
            .unwrap();
            browser.navigate(url).unwrap();
            Self {
                browser,
                final_page: None,
            }
        }
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    impl BrowserPage for LiveBrowser {
        fn capture_redacted_page(
            &self,
            sensitive: &[String],
        ) -> Result<CapturedPage, BrowserError> {
            self.browser.capture_redacted_page(sensitive)
        }
        fn observe_page(&self) -> Result<Observation, BrowserError> {
            self.browser.observe_page()
        }
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    impl actions::Performer for LiveBrowser {
        fn dispatch(
            &self,
            input: PreparedInput,
            cancellation: &manuvra_chrome::InputCancellation,
        ) -> Result<PerformFact, PerformError> {
            self.browser.dispatch(input, cancellation)
        }
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    impl HostedBrowser for LiveBrowser {
        fn cleanup_hosted(&mut self) -> Cleanup {
            self.final_page = self.browser.observe().ok();
            cleanup_started_browser(&mut self.browser)
        }
    }

    /// Serves scripted captures in order and then its fallback page, answers each dispatch with
    /// the next scripted outcome or its repeated one, and records every prepared input. An
    /// unscripted dispatch fails the test.
    struct FakeBrowser {
        captures: Mutex<VecDeque<Result<CapturedPage, BrowserError>>>,
        fallback: Observation,
        outcomes: Mutex<VecDeque<Result<PerformFact, PerformError>>>,
        repeated_outcome: Option<Result<PerformFact, PerformError>>,
        inputs: Mutex<Vec<PreparedInput>>,
    }

    impl FakeBrowser {
        /// Serves the pages in order, then keeps serving the last one.
        fn new(pages: impl IntoIterator<Item = Observation>) -> Self {
            let pages: Vec<_> = pages.into_iter().collect();
            let fallback = pages.last().cloned().expect("scripted page");
            Self::capturing(pages.into_iter().map(captured), fallback)
        }

        fn capturing(
            captures: impl IntoIterator<Item = Result<CapturedPage, BrowserError>>,
            fallback: Observation,
        ) -> Self {
            Self {
                captures: Mutex::new(captures.into_iter().collect()),
                fallback,
                outcomes: Mutex::new(VecDeque::new()),
                repeated_outcome: None,
                inputs: Mutex::new(Vec::new()),
            }
        }

        fn dispatching(
            self,
            outcomes: impl IntoIterator<Item = Result<PerformFact, PerformError>>,
        ) -> Self {
            Self {
                outcomes: Mutex::new(outcomes.into_iter().collect()),
                ..self
            }
        }

        fn always_dispatching(self, outcome: Result<PerformFact, PerformError>) -> Self {
            Self {
                repeated_outcome: Some(outcome),
                ..self
            }
        }

        fn operations(&self) -> Vec<PreparedOperation> {
            self.inputs
                .lock()
                .unwrap()
                .iter()
                .map(|input| input.operation)
                .collect()
        }

        fn dispatched(&self) -> usize {
            self.inputs.lock().unwrap().len()
        }
    }

    impl BrowserPage for FakeBrowser {
        fn capture_redacted_page(
            &self,
            sensitive: &[String],
        ) -> Result<CapturedPage, BrowserError> {
            self.captures
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or_else(|| captured(self.fallback.clone()))
                .map(|mut page| {
                    page.redaction.sensitive_values_checked = sensitive.len();
                    page
                })
        }
        fn observe_page(&self) -> Result<Observation, BrowserError> {
            Ok(self.fallback.clone())
        }
    }

    impl actions::Performer for FakeBrowser {
        fn dispatch(
            &self,
            input: PreparedInput,
            _cancellation: &manuvra_chrome::InputCancellation,
        ) -> Result<PerformFact, PerformError> {
            self.inputs.lock().unwrap().push(input);
            let scripted = self.outcomes.lock().unwrap().pop_front();
            scripted
                .or_else(|| self.repeated_outcome.clone())
                .expect("unscripted dispatch")
        }
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    impl HostedBrowser for FakeBrowser {
        fn cleanup_hosted(&mut self) -> Cleanup {
            Cleanup {
                browser: "closed".into(),
                profile: "removed".into(),
                application_state: "caller_owned".into(),
            }
        }
    }

    fn captured(observation: Observation) -> Result<CapturedPage, BrowserError> {
        Ok(CapturedPage {
            observation,
            screenshot: Screenshot {
                bytes: b"fake-png".to_vec(),
                width: 1,
                height: 1,
            },
            redaction: RedactionProof {
                sensitive_values_checked: 0,
                matched_values: 0,
                mask_count: 0,
            },
        })
    }

    fn performed(suboperations: &[&str]) -> Result<PerformFact, PerformError> {
        Ok(PerformFact {
            readback: None,
            readback_matches: None,
            suboperations: suboperations.iter().map(|&name| name.to_owned()).collect(),
        })
    }

    fn typed(readback: &str) -> Result<PerformFact, PerformError> {
        Ok(PerformFact {
            readback: Some(readback.into()),
            readback_matches: None,
            suboperations: vec![],
        })
    }

    struct NoProvider;
    impl manuvra_jev::Evaluator for NoProvider {
        fn evaluate(
            &self,
            _request: &Value,
            _deadline: Instant,
        ) -> Result<manuvra_jev::Evaluation, manuvra_jev::JevError> {
            Err(manuvra_jev::JevError::Unavailable)
        }
    }

    /// One scripted provider answer: the chosen operation and its targets, value and key, and the
    /// Noul given to every Noul question.
    #[derive(Clone)]
    struct Turn {
        operation: &'static str,
        confidence: f64,
        click_target: &'static str,
        type_target: &'static str,
        select_target: &'static str,
        type_value: &'static str,
        key: &'static str,
        key_confidence: f64,
        hover_target: Option<&'static str>,
        noul: f64,
    }

    impl Turn {
        fn new(operation: &'static str) -> Self {
            Self {
                operation,
                confidence: 0.95,
                click_target: "NO_CLICK_TARGET",
                type_target: "NO_TYPE_TEXT_TARGET",
                select_target: "NO_SELECT_TARGET",
                type_value: "NONE_FITS",
                key: "Escape",
                key_confidence: 1.0,
                hover_target: None,
                noul: 0.01,
            }
        }

        fn click(target: &'static str) -> Self {
            Self {
                click_target: target,
                ..Self::new("CLICK")
            }
        }

        fn scroll_down() -> Self {
            Self::new("SCROLL_DOWN")
        }

        fn hover(region: &'static str) -> Self {
            Self {
                hover_target: Some(region),
                ..Self::new("HOVER")
            }
        }

        fn type_text() -> Self {
            Self {
                type_target: "1",
                type_value: "name",
                ..Self::new("TYPE_TEXT")
            }
        }

        fn select() -> Self {
            Self {
                select_target: "1",
                type_value: "name",
                ..Self::new("SELECT")
            }
        }

        fn key(key: &'static str) -> Self {
            Self {
                key,
                ..Self::new("PRESS_KEY")
            }
        }

        /// A final-verification answer: every expectation receives this Noul.
        fn verdict(noul: f64) -> Self {
            Self {
                noul,
                ..Self::new("WAIT")
            }
        }

        fn confidence(self, confidence: f64) -> Self {
            Self { confidence, ..self }
        }

        fn key_confidence(self, key_confidence: f64) -> Self {
            Self {
                key_confidence,
                ..self
            }
        }

        fn noul(self, noul: f64) -> Self {
            Self { noul, ..self }
        }

        fn answer(&self, id: &str, question: &Value) -> Option<manuvra_jev::Answer> {
            if question["type"] == "noul" {
                return Some(manuvra_jev::Answer::Noul { noul: self.noul });
            }
            let (selected, confidence) = match id {
                "operation" => (self.operation, self.confidence),
                "click_target" => (self.click_target, 1.0),
                "type_target" => (self.type_target, 1.0),
                "select_target" => (self.select_target, 1.0),
                "type_value" => (self.type_value, 1.0),
                "key" => (self.key, self.key_confidence),
                "hover_target" => (self.hover_target?, 1.0),
                other => panic!("unscripted question {other}"),
            };
            Some(manuvra_jev::Answer::Choice {
                choice: selected.into(),
                probabilities: BTreeMap::from([(selected.into(), confidence)]),
                confidence,
            })
        }
    }

    /// Answers each request with the next scripted turn, repeating the last one, and records
    /// every request it receives.
    struct ScriptedProvider {
        turns: Mutex<VecDeque<Turn>>,
        requests: Mutex<Vec<Value>>,
    }

    impl ScriptedProvider {
        fn new(turns: impl IntoIterator<Item = Turn>) -> Self {
            Self {
                turns: Mutex::new(turns.into_iter().collect()),
                requests: Mutex::new(Vec::new()),
            }
        }

        fn calls(&self) -> usize {
            self.requests.lock().unwrap().len()
        }
    }

    impl manuvra_jev::Evaluator for ScriptedProvider {
        fn evaluate(
            &self,
            request: &Value,
            _deadline: Instant,
        ) -> Result<manuvra_jev::Evaluation, manuvra_jev::JevError> {
            self.requests.lock().unwrap().push(request.clone());
            let turn = {
                let mut turns = self.turns.lock().unwrap();
                if turns.len() > 1 {
                    turns.pop_front()
                } else {
                    turns.front().cloned()
                }
                .expect("scripted turn")
            };
            let answers = request["questions"]
                .as_object()
                .expect("questions")
                .iter()
                .filter_map(|(id, question)| {
                    turn.answer(id, question).map(|answer| (id.clone(), answer))
                })
                .collect();
            Ok(manuvra_jev::Evaluation {
                answers,
                usage: BTreeMap::new(),
                request_id: Some("scripted".into()),
                model: "jev-1.13.0".into(),
            })
        }
    }

    /// Records every journal entry; an injected failure refuses the entry at that position.
    #[derive(Default)]
    struct MemoryJournal {
        entries: Vec<Value>,
        fail_at: Option<usize>,
    }

    impl MemoryJournal {
        fn failing_at(position: usize) -> Self {
            Self {
                entries: Vec::new(),
                fail_at: Some(position),
            }
        }

        fn prepared(&self) -> Vec<&Value> {
            self.entries
                .iter()
                .filter(|entry| entry["event"] == "action_prepared")
                .collect()
        }
    }

    impl actions::ActionJournal for MemoryJournal {
        fn append(&mut self, value: &Value) -> Result<(), String> {
            if self.fail_at == Some(self.entries.len()) {
                return Err("injected journal failure".into());
            }
            self.entries.push(value.clone());
            Ok(())
        }
        fn entries(&self) -> &[Value] {
            &self.entries
        }
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    impl RunJournal for MemoryJournal {
        fn clear(&mut self) -> Result<(), String> {
            Ok(())
        }
    }

    /// Answers each pause with the next scripted disposition and aborts once none remain. It
    /// never reports a termination by itself.
    #[derive(Default)]
    struct ScriptedControl {
        checkpoints: Mutex<Vec<Value>>,
        dispositions: Mutex<VecDeque<Disposition>>,
        cancellation: manuvra_chrome::InputCancellation,
    }

    impl ScriptedControl {
        fn answering(dispositions: impl IntoIterator<Item = Disposition>) -> Self {
            Self {
                dispositions: Mutex::new(dispositions.into_iter().collect()),
                ..Self::default()
            }
        }

        fn checkpoints(&self) -> Vec<Value> {
            self.checkpoints.lock().unwrap().clone()
        }
    }

    impl HostedControl for ScriptedControl {
        fn cancellation(&self) -> manuvra_chrome::InputCancellation {
            self.cancellation.clone()
        }

        fn pause_deadline_unix_ms(&self) -> u64 {
            u64::MAX
        }

        fn publish_checkpoint(&self, result: &Value) -> Result<(), String> {
            self.checkpoints.lock().unwrap().push(result.clone());
            Ok(())
        }

        fn wait_while_paused(&self, escalation_id: &str) -> HostedEvent {
            self.dispositions.lock().unwrap().pop_front().map_or(
                HostedEvent::Termination(HostedTermination::Aborted),
                |disposition| HostedEvent::Disposition(request(escalation_id, disposition)),
            )
        }

        fn termination(&self) -> Option<HostedTermination> {
            None
        }
    }

    fn request(escalation_id: &str, disposition: Disposition) -> DispositionRequest {
        DispositionRequest {
            schema_version: SchemaVersion,
            escalation_id: escalation_id.into(),
            disposition,
        }
    }

    fn execute(candidate_id: &str) -> Disposition {
        Disposition::Execute(manuvra_contract::ExecuteDisposition {
            kind: manuvra_contract::ExecuteKind::Execute,
            candidate_id: candidate_id.into(),
        })
    }

    fn advance(rationale: &str) -> Disposition {
        Disposition::Advance(manuvra_contract::AdvanceDisposition {
            kind: manuvra_contract::AdvanceKind::Advance,
            rationale: rationale.into(),
        })
    }

    fn retry() -> Disposition {
        Disposition::RetryObservation(manuvra_contract::RetryObservationDisposition {
            kind: manuvra_contract::RetryObservationKind::RetryObservation,
        })
    }

    fn abort() -> Disposition {
        Disposition::Abort(manuvra_contract::AbortDisposition {
            kind: manuvra_contract::AbortKind::Abort,
        })
    }

    /// An uncertain stop always carries the escalation a disposition answers; the hosted loop
    /// pauses only on one.
    fn assert_pause_invariant(machine: &HostedMachine<'_>) {
        if let Some(stop) = &machine.artifacts.stop
            && stop.state == RunState::Uncertain
        {
            assert!(
                machine.artifacts.escalation.is_some(),
                "uncertain stop {} has no escalation",
                stop.code
            );
        }
    }

    /// Drives the machine as the hosted loop does, without publishing checkpoints.
    fn drive(
        machine: &mut HostedMachine<'_>,
        browser: &FakeBrowser,
        provider: &impl manuvra_jev::Evaluator,
        journal: &mut MemoryJournal,
    ) {
        machine.drive(
            browser,
            provider,
            journal,
            &manuvra_chrome::InputCancellation::default(),
            None,
            &ScriptedControl::default(),
        );
        assert_pause_invariant(machine);
    }

    /// Answers the machine's current escalation with a disposition.
    fn dispose(
        machine: &mut HostedMachine<'_>,
        disposition: Disposition,
        browser: &FakeBrowser,
        provider: &impl manuvra_jev::Evaluator,
        journal: &mut MemoryJournal,
    ) -> Option<HostedTermination> {
        let escalation_id = machine
            .artifacts
            .escalation
            .as_ref()
            .map_or_else(|| "e_stale".to_owned(), |escalation| escalation.id.clone());
        let termination = machine.apply(
            request(&escalation_id, disposition),
            browser,
            provider,
            journal,
            &manuvra_chrome::InputCancellation::default(),
        );
        assert_pause_invariant(machine);
        termination
    }

    /// The artifacts of a run driven autonomously until it completes or first stops.
    fn driven(
        job: &Job,
        browser: &FakeBrowser,
        provider: &impl manuvra_jev::Evaluator,
        journal: &mut MemoryJournal,
    ) -> RunArtifacts {
        let redactor = Redactor::for_job(job).unwrap();
        let mut machine = HostedMachine::new(job, &redactor);
        drive(&mut machine, browser, provider, journal);
        machine.artifacts
    }

    /// A run of the hosted loop against fakes, with its published evidence.
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    struct LoopRun {
        evidence: TempDir,
        outcome: FlowOutcome,
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    impl LoopRun {
        fn state(&self) -> &str {
            self.outcome.result["state"].as_str().unwrap()
        }

        fn code(&self) -> &str {
            self.outcome.result["reason"]["code"]
                .as_str()
                .unwrap_or_default()
        }

        fn complete(&self) -> bool {
            self.outcome.result["evidence"]["complete"]
                .as_bool()
                .unwrap()
        }

        fn artifact(&self, relative: &str) -> std::path::PathBuf {
            self.evidence.path().join("r_loop").join(relative)
        }

        fn trace(&self) -> String {
            std::fs::read_to_string(self.artifact("trace.jsonl")).unwrap()
        }
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    fn run_loop(
        job: &Job,
        browser: &mut impl HostedBrowser,
        provider: &impl manuvra_jev::Evaluator,
        control: &ScriptedControl,
        journal: &mut MemoryJournal,
    ) -> LoopRun {
        let evidence = TempDir::new().unwrap();
        let redactor = Redactor::for_job(job).unwrap();
        let config = FlowConfig {
            request_id: "hosted-loop".into(),
            run_id: "r_loop".into(),
            evidence_root: evidence.path().to_path_buf(),
            browser: None,
            headless: true,
        };
        let outcome = finish_hosted_browser_run(
            job,
            config,
            &redactor,
            browser,
            json!({"fixture":"hosted-loop"}),
            provider,
            journal,
            &control.cancellation,
            control,
        )
        .unwrap();
        LoopRun { evidence, outcome }
    }

    fn observed(text: &str) -> Observation {
        Observation {
            document_id: "recorded-money-document".into(),
            url: "http://127.0.0.1:4351/".into(),
            route: "/".into(),
            title: "Money · Accounts".into(),
            dialogs: Vec::new(),
            focused: None,
            focus_anchor: None,
            visible_text: text.into(),
            covered_text: String::new(),
            dialog_texts: BTreeMap::new(),
            elements: Vec::new(),
            viewport: ViewportState {
                width: 1120,
                height: 780,
                scroll_x: 0.,
                scroll_y: 0.,
                document_height: 780.,
            },
            coverage: Coverage::default(),
            hover_regions: Vec::new(),
            hover_regions_truncated: false,
        }
    }

    fn foreign(mut observation: Observation) -> Observation {
        observation.url = "http://foreign.test/".into();
        observation
    }

    fn element(index: u64, node_id: u64, role: &str, name: &str, operation: &str) -> Element {
        Element {
            index,
            node_id,
            context: "main".into(),
            role: role.into(),
            name: name.into(),
            input_type: None,
            value: String::new(),
            checked: None,
            selected: None,
            expanded: None,
            disabled: false,
            in_dialog: None,
            operations: vec![operation.into()],
            select_options: vec![],
            rect: Rect {
                x: 10.0,
                y: 10.0,
                width: 20.0,
                height: 10.0,
            },
        }
    }

    fn button(index: u64, node_id: u64, name: &str) -> Element {
        Element {
            expanded: Some(false),
            ..element(index, node_id, "button", name, "CLICK")
        }
    }

    fn text_field(value: &str) -> Observation {
        let mut observation = observed("");
        observation.elements.push(Element {
            input_type: Some("text".into()),
            value: value.into(),
            rect: Rect {
                x: 1.0,
                y: 1.0,
                width: 20.0,
                height: 10.0,
            },
            ..element(1, 7, "textbox", "Name", "TYPE_TEXT")
        });
        observation
    }

    fn key_observation(name: &str) -> Observation {
        let mut observation = observed("");
        observation.focus_anchor = Some(FocusAnchor {
            node_id: match name {
                "First" => 1,
                "Second" => 2,
                _ => 3,
            },
            context: "main".into(),
            role: "button".into(),
            name: name.into(),
            in_dialog: None,
            covered: true,
            surface: None,
            active_descendant: None,
            expanded: None,
            selected: None,
            checked: None,
            position: None,
        });
        observation
    }

    fn save_button_focus() -> Observation {
        let mut observation = key_observation("Save");
        observation.elements.push(button(1, 3, "Save"));
        observation
    }

    fn anchor_state(role: &str, expanded: Option<bool>, checked: Option<bool>) -> Observation {
        let mut observation = key_observation("Save");
        let anchor = observation.focus_anchor.as_mut().unwrap();
        anchor.role = role.into();
        anchor.expanded = expanded;
        anchor.checked = checked;
        observation
    }

    /// The Groceries and Rent rows before any hover: their action buttons are hidden.
    fn plan_before_hover() -> Observation {
        let mut page = observed("Plan Groceries Rent");
        page.elements
            .push(button(1, 11, "Edit assigned amount for Groceries, $400.00"));
        page.hover_regions = vec![
            hover_region(1, "Groceries", 41),
            hover_region(2, "Rent", 42),
        ];
        page
    }

    /// After hovering Groceries: its action button is a candidate and its region is gone.
    fn plan_groceries_revealed() -> Observation {
        let mut page = plan_before_hover();
        page.elements.push(button(2, 41, "Actions for Groceries"));
        page.hover_regions = vec![hover_region(1, "Rent", 42)];
        page
    }

    fn plan_menu_open() -> Observation {
        let mut page = plan_groceries_revealed();
        page.visible_text = "Plan Groceries Rent Rename Move to group… Delete category…".into();
        page
    }

    fn parse_job(value: Value) -> Job {
        Job::parse(serde_json::to_vec(&value).unwrap().as_slice()).unwrap()
    }

    fn job(wanted: &str) -> Job {
        parse_job(json!({
            "schema_version":1,
            "target":{"kind":"browser","url":"http://127.0.0.1:4351/"},
            "context":{"journey":"observe","revision":"fixture","environment":"fake","actor":"synthetic","authority":"observe"},
            "steps":[{"id":"ready","goal":"observe","done_when":[{"text_visible":wanted}]}]
        }))
    }

    fn mutation_job() -> Job {
        parse_job(json!({
            "schema_version":1,
            "target":{"kind":"browser","url":"http://127.0.0.1:4351/"},
            "context":{"journey":"mutate","revision":"fixture","environment":"fake","actor":"synthetic","authority":"mutate"},
            "values":{"name":{"value":"Wanted","description":"account name"}},
            "steps":[{"id":"fill","goal":"fill the account name","done_when":[{"field":"Name","equals_value":"name"}],"mutation_limit":2}]
        }))
    }

    fn force_stop(mut job: Job) -> Job {
        job.options.debug = Some(manuvra_contract::DebugOptions {
            force_stop_at_step: job.steps[0].id.clone(),
        });
        job
    }

    fn natural(mut job: Job, condition: &str) -> Job {
        job.steps[0].done_when = DoneCondition::NaturalLanguage(condition.into());
        job
    }

    fn expectation_job() -> Job {
        parse_job(json!({
            "schema_version":1,
            "target":{"kind":"browser","url":"http://127.0.0.1:4351/"},
            "context":{"journey":"verify","revision":"fixture","environment":"fake","actor":"synthetic","authority":"observe"},
            "steps":[{"id":"ready","goal":"observe","done_when":[{"text_visible":"Ready"}]}],
            "expectations":[{"id":"balance","claim":"The final balance is 12.34."}]
        }))
    }

    fn click_job() -> Job {
        parse_job(json!({
            "schema_version":1,
            "target":{"kind":"browser","url":"http://127.0.0.1:4351/"},
            "context":{"journey":"open","revision":"fixture","environment":"fake","actor":"synthetic","authority":"click"},
            "steps":[{"id":"open","goal":"Open the actions menu for Groceries.","done_when":[{"text_visible":"Move to group"}]}]
        }))
    }

    fn key_activation_job() -> Job {
        let mut job = job("Activations: 1");
        job.steps[0].goal = "Press Enter to activate Save".into();
        job.steps[0].mutation_limit = 2;
        job
    }

    fn key_job(id: &str, goal: &str, done_when: Value, mutation_limit: u8) -> Job {
        parse_job(json!({
            "schema_version":1,"target":{"kind":"browser","url":"http://127.0.0.1:4351/"},
            "context":{"journey":"key","revision":"fixture","environment":"fake","actor":"synthetic","authority":"keys"},
            "steps":[{"id":id,"goal":goal,"done_when":done_when,"mutation_limit":mutation_limit}]
        }))
    }

    fn low_hover_turn() -> Turn {
        Turn::hover("R1").confidence(0.59)
    }

    /// A run paused on a below-gate `HOVER` escalation offering the Groceries region.
    fn hover_escalation<'a>(job: &'a Job, redactor: &'a Redactor) -> HostedMachine<'a> {
        let browser = FakeBrowser::new([plan_before_hover()]);
        let provider = ScriptedProvider::new([low_hover_turn()]);
        let mut machine = HostedMachine::new(job, redactor);
        drive(
            &mut machine,
            &browser,
            &provider,
            &mut MemoryJournal::default(),
        );
        assert_eq!(
            machine.artifacts.stop.as_ref().unwrap().code,
            "operation_below_gate"
        );
        machine
    }

    fn offered(machine: &HostedMachine<'_>) -> policy::Candidate {
        machine
            .artifacts
            .pending
            .as_ref()
            .and_then(|pending| pending.candidate.clone())
            .expect("offered candidate")
    }

    fn offered_hover(machine: &HostedMachine<'_>) -> policy::Candidate {
        let candidate = offered(machine);
        assert_eq!(candidate.operation, judgment::Operation::Hover);
        candidate
    }

    /// The code and state of the run's stop, and whether the reissued escalation still offers a
    /// candidate.
    fn paused(machine: &HostedMachine<'_>) -> (&'static str, RunState, bool) {
        let stop = machine.artifacts.stop.as_ref().unwrap();
        let offered = machine
            .artifacts
            .pending
            .as_ref()
            .is_some_and(|pending| pending.candidate.is_some());
        (stop.code, stop.state, offered)
    }

    fn offers_execute(machine: &HostedMachine<'_>) -> bool {
        machine
            .artifacts
            .escalation
            .as_ref()
            .unwrap()
            .dispositions
            .contains(&DispositionKind::Execute)
    }

    fn action_operations(trace: &[Value], event: &str) -> Vec<String> {
        trace
            .iter()
            .filter(|entry| entry["event"] == event)
            .map(|entry| {
                entry
                    .get("operation")
                    .or_else(|| entry["fact"].get("operation"))
                    .and_then(Value::as_str)
                    .unwrap()
                    .to_owned()
            })
            .collect()
    }

    fn trace_events(artifacts: &RunArtifacts) -> Vec<String> {
        artifacts
            .trace
            .iter()
            .map(|entry| {
                let event = entry["event"].as_str().unwrap();
                let operation = entry
                    .get("operation")
                    .or_else(|| entry.get("fact").and_then(|fact| fact.get("operation")));
                operation.map_or_else(
                    || event.to_owned(),
                    |operation| format!("{event}:{}", operation.as_str().unwrap()),
                )
            })
            .collect()
    }

    fn mutation_judgments(operation: &str, confidence: f64) -> judgment::Judgments {
        let choice = |selected: &str| judgment::ChoiceJudgment {
            choice: selected.into(),
            probabilities: BTreeMap::from([(selected.into(), 1.0)]),
            confidence,
        };
        judgment::Judgments {
            operation: choice(operation),
            click_target: choice("1"),
            type_target: choice("1"),
            select_target: choice("1"),
            type_value: choice("name"),
            key: choice("Escape"),
            hover_target: None,
            step_done: 0.0,
            usage: BTreeMap::new(),
            request_id: None,
            model: "jev-test".into(),
            request: Value::Null,
        }
    }

    use crate::test_support::hover_region;

    /// Plays a careful agent on a live page: for each step goal it clicks the wanted control when
    /// it is a visible candidate, otherwise hovers the region that reveals it, and judges a
    /// natural done condition from the wanted control's expanded state.
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    struct RowActionProvider {
        wanted: Vec<(&'static str, &'static str)>,
        choices: Mutex<Vec<(String, String, Value)>>,
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    impl RowActionProvider {
        fn new(wanted: Vec<(&'static str, &'static str)>) -> Self {
            Self {
                wanted,
                choices: Mutex::new(Vec::new()),
            }
        }

        fn wanted_control(&self, request: &Value) -> &'static str {
            let goal = request["state"]["current_step"]["goal"].as_str().unwrap();
            self.wanted
                .iter()
                .find(|(step_goal, _)| *step_goal == goal)
                .map(|(_, control)| *control)
                .expect("scripted goal")
        }

        fn choose(request: &Value, wanted: &str) -> (&'static str, String, String) {
            let key_where = |question: &str, matches: &dyn Fn(&Value) -> bool| {
                request["questions"][question]["criteria"]
                    .as_object()
                    .and_then(|criteria| {
                        criteria
                            .iter()
                            .find(|(_, criterion)| matches(criterion))
                            .map(|(key, _)| key.clone())
                    })
            };
            if let Some(key) = key_where("click_target", &|criterion| {
                criterion["name"] == wanted && criterion["disabled"] == false
            }) {
                return ("CLICK", key, "NO_HOVER_TARGET".into());
            }
            key_where("hover_target", &|criterion| {
                criterion["reveals_on_hover"]
                    .as_array()
                    .is_some_and(|reveals| reveals.iter().any(|name| name == wanted))
            })
            .map_or(("BLOCKED", String::new(), String::new()), |key| {
                ("HOVER", "NO_CLICK_TARGET".into(), key)
            })
        }

        fn calls(&self) -> usize {
            self.choices.lock().unwrap().len()
        }
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    impl manuvra_jev::Evaluator for RowActionProvider {
        fn evaluate(
            &self,
            request: &Value,
            _deadline: Instant,
        ) -> Result<manuvra_jev::Evaluation, manuvra_jev::JevError> {
            let wanted = self.wanted_control(request);
            let (operation, click, hover) = Self::choose(request, wanted);
            let expanded = request["state"]["page"]["elements"]
                .as_array()
                .unwrap()
                .iter()
                .any(|element| element["name"] == wanted && element["expanded"] == true);
            let target_name = if operation == "HOVER" { &hover } else { &click };
            self.choices.lock().unwrap().push((
                operation.to_owned(),
                target_name.clone(),
                request.clone(),
            ));
            let choice = |selected: &str| manuvra_jev::Answer::Choice {
                choice: selected.into(),
                probabilities: BTreeMap::from([(selected.into(), 0.9)]),
                confidence: 0.9,
            };
            let mut answers = BTreeMap::from([
                ("operation".into(), choice(operation)),
                ("click_target".into(), choice(&click)),
                ("type_target".into(), choice("NO_TYPE_TEXT_TARGET")),
                ("select_target".into(), choice("NO_SELECT_TARGET")),
                ("type_value".into(), choice("NONE_FITS")),
                ("key".into(), choice("Escape")),
                (
                    "step_done".into(),
                    manuvra_jev::Answer::Noul {
                        noul: if expanded { 0.95 } else { 0.05 },
                    },
                ),
            ]);
            if request["questions"].get("hover_target").is_some() {
                answers.insert("hover_target".into(), choice(&hover));
            }
            Ok(manuvra_jev::Evaluation {
                answers,
                usage: BTreeMap::new(),
                request_id: Some("row-action-script".into()),
                model: "jev-1.13.0".into(),
            })
        }
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    fn prepared_actions(journal: &MemoryJournal) -> Vec<String> {
        journal
            .prepared()
            .into_iter()
            .map(|entry| {
                let target = entry
                    .get("hover_target")
                    .unwrap_or(&entry["target"])
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                format!("{} {target}", entry["operation"].as_str().unwrap())
            })
            .collect()
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    #[ignore = "requires the local Chromium executable"]
    fn hosted_run_stops_after_committed_navigation_to_a_foreign_origin() {
        let fixture = OriginFixture::start();
        let start_url = fixture.start_url();
        let job = parse_job(json!({
            "schema_version":1,
            "target":{"kind":"browser","url":start_url},
            "context":{"journey":"origin guard","revision":"fixture","environment":"local Chromium","actor":"synthetic","authority":"navigate only"},
            "steps":[{"id":"leave","goal":"Open Leave origin.","done_when":[{"text_visible":"Never present"}]}]
        }));
        let mut browser = LiveBrowser::open(&start_url);
        let provider = RowActionProvider::new(vec![("Open Leave origin.", "Leave origin")]);
        let mut journal = MemoryJournal::default();

        let run = run_loop(
            &job,
            &mut browser,
            &provider,
            &ScriptedControl::default(),
            &mut journal,
        );

        assert_eq!((run.state(), run.code()), ("blocked", "origin_not_allowed"));
        assert!(run.outcome.result["escalation"].is_null());
        assert_eq!(provider.calls(), 1);
        assert_eq!(
            journal.entries.len(),
            2,
            "only one prepared mutation may run"
        );
        assert_eq!(journal.entries[0]["event"], "action_prepared");
        assert_eq!(journal.entries[1]["event"], "action_fact");
        assert_eq!(journal.entries[1]["fact"]["outcome"], "observed");
        let committed = browser.final_page.expect("page observed before cleanup");
        assert!(committed.url.starts_with(&fixture.foreign_origin()));
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    #[ignore = "requires the local Chromium executable"]
    fn hosted_run_hovers_row_actions_into_view_and_never_clicks_another_row() {
        let fixture = HoverRevealFixture::start();
        let url = fixture.url();
        let job = parse_job(json!({
            "schema_version":1,
            "target":{"kind":"browser","url":url},
            "context":{"journey":"hover-revealed row actions","revision":"fixture","environment":"local Chromium","actor":"synthetic","authority":"delete a synthetic category"},
            "steps":[
                {"id":"groceries-menu","goal":"Open the actions menu for the Groceries category.","done_when":[{"text_visible":"Move to group…"}]},
                {"id":"rent-menu","goal":"Open the actions menu for the Rent category.","done_when":"The actions menu for Rent is open."},
                {"id":"choose-delete","goal":"Choose Delete category… for Rent.","done_when":[{"dialog_open":"Delete category?"}]},
                {"id":"confirm","goal":"Confirm the deletion.","done_when":[{"dialog_closed":"Delete category?"},{"text_absent":"Rent"},{"text_visible":"Groceries"}]}
            ]
        }));
        let mut browser = LiveBrowser::open(&url);
        let provider = RowActionProvider::new(vec![
            (
                "Open the actions menu for the Groceries category.",
                "Actions for Groceries",
            ),
            (
                "Open the actions menu for the Rent category.",
                "Actions for Rent",
            ),
            ("Choose Delete category… for Rent.", "Delete category…"),
            ("Confirm the deletion.", "Delete"),
        ]);
        let mut journal = MemoryJournal::default();

        let run = run_loop(
            &job,
            &mut browser,
            &provider,
            &ScriptedControl::default(),
            &mut journal,
        );

        assert_eq!(run.state(), "passed", "{}", run.outcome.result["reason"]);
        assert_eq!(run.outcome.result["verdict"]["caller_assisted"], false);
        assert_eq!(
            prepared_actions(&journal),
            [
                "HOVER Groceries",
                "CLICK Actions for Groceries",
                "HOVER Rent",
                "CLICK Actions for Rent",
                "CLICK Delete category…",
                "CLICK Delete",
            ]
        );
        assert!(
            journal
                .entries
                .iter()
                .filter(|entry| entry["event"] == "action_fact")
                .all(|entry| entry["fact"]["outcome"] == "observed")
        );
        let mut step_files: Vec<_> = std::fs::read_dir(run.artifact("steps"))
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .collect();
        step_files.sort();
        let per_step: Vec<_> = step_files
            .iter()
            .map(|path| {
                let step: Value =
                    serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
                step["mutation_limit_consumed"].as_u64().unwrap()
            })
            .collect();
        assert_eq!(per_step, [1, 1, 1, 1]);

        let choices = provider.choices.lock().unwrap();
        let (_, _, before_rent_hover) = choices
            .iter()
            .find(|(operation, target, request)| {
                operation == "HOVER"
                    && request["state"]["current_step"]["goal"]
                        == "Open the actions menu for the Rent category."
                    && request["state"]["page"]["hover_regions"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .any(|region| region["key"] == *target && region["name"] == "Rent")
            })
            .expect("the Rent row was hovered");
        let visible: Vec<_> = before_rent_hover["state"]["page"]["elements"]
            .as_array()
            .unwrap()
            .iter()
            .map(|element| element["name"].as_str().unwrap())
            .collect();
        assert!(visible.contains(&"Actions for Groceries"));
        assert!(!visible.contains(&"Actions for Rent"));
        assert!(
            choices[0].2["state"]["page"]["elements"]
                .as_array()
                .unwrap()
                .iter()
                .all(|element| element["name"] != "Actions for Groceries")
        );
        for action in journal.prepared().into_iter().skip(2) {
            let name = action
                .get("hover_target")
                .unwrap_or(&action["target"])
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or_default();
            assert!(!name.contains("Groceries"), "{action}");
        }
        let remaining = browser.final_page.expect("page observed before cleanup");
        assert!(!remaining.visible_text.contains("Rent"));
        assert!(remaining.visible_text.contains("Groceries"));
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn hosted_loop_publishes_running_checkpoints_before_the_terminal_result() {
        let job = parse_job(json!({
            "schema_version":1,
            "target":{"kind":"browser","url":"http://example.test/ready"},
            "context":{"journey":"checkpoint","revision":"r","environment":"e","actor":"a","authority":"a"},
            "values":{},
            "steps":[
                {"id":"first","goal":"observe first","done_when":[{"url_contains":"/ready"}]},
                {"id":"second","goal":"observe second","done_when":[{"url_contains":"/ready"}]}
            ]
        }));
        let ready = Observation {
            url: "http://example.test/ready".into(),
            route: "/ready".into(),
            ..observed("ready")
        };
        let control = ScriptedControl::default();

        let run = run_loop(
            &job,
            &mut FakeBrowser::new([ready]),
            &NoProvider,
            &control,
            &mut MemoryJournal::default(),
        );

        assert_eq!(run.state(), "passed");
        let checkpoints = control.checkpoints();
        let states: Vec<_> = checkpoints
            .iter()
            .map(|checkpoint| checkpoint["state"].as_str().unwrap())
            .collect();
        assert_eq!(states, ["running", "running", "passed"]);
        assert_eq!(checkpoints[0]["verdict"]["steps"][0]["result"], "satisfied");
        assert_eq!(
            checkpoints[0]["verdict"]["steps"][1]["result"],
            "unresolved"
        );
        assert_eq!(checkpoints[0]["evidence"]["complete"], false);
        assert_eq!(checkpoints[2]["terminal"], true);
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn caller_execute_round_trip_publishes_assisted_terminal_evidence() {
        let job = force_stop(mutation_job());
        let mut browser = FakeBrowser::new([text_field(""), text_field(""), text_field("Wanted")])
            .dispatching([typed("Wanted")]);
        let control = ScriptedControl::answering([execute("c_1")]);

        let run = run_loop(
            &job,
            &mut browser,
            &ScriptedProvider::new([Turn::type_text()]),
            &control,
            &mut MemoryJournal::default(),
        );

        assert_eq!(run.state(), "passed");
        assert_eq!(run.outcome.result["verdict"]["caller_assisted"], true);
        assert!(run.artifact("dispositions/request_0001.json").is_file());
        let checkpoints = control.checkpoints();
        assert_eq!(checkpoints[0]["state"], "uncertain");
        assert_eq!(checkpoints[0]["terminal"], false);
        assert!(
            checkpoints
                .iter()
                .any(|checkpoint| checkpoint["verdict"]["caller_assisted"] == true)
        );
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn hosted_pause_termination_closes_the_browser_and_publishes_abort() {
        let job = force_stop(mutation_job());

        let run = run_loop(
            &job,
            &mut FakeBrowser::new([text_field("")]),
            &ScriptedProvider::new([Turn::type_text()]),
            &ScriptedControl::default(),
            &mut MemoryJournal::default(),
        );

        assert_eq!((run.state(), run.code()), ("aborted", "caller_aborted"));
        assert_eq!(run.outcome.result["cleanup"]["browser"], "closed");
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn abort_persists_the_disposition_and_reports_actions_already_sent() {
        let job = force_stop(click_job());
        let mut page = observed("Plan");
        page.elements.push(button(1, 7, "Actions for Groceries"));
        page.viewport.document_height = 2_000.0;
        let mut browser = FakeBrowser::new([page]).dispatching([performed(&["scroll_down"])]);

        let run = run_loop(
            &job,
            &mut browser,
            &ScriptedProvider::new([Turn::scroll_down(), Turn::click("1")]),
            &ScriptedControl::answering([abort()]),
            &mut MemoryJournal::default(),
        );

        assert_eq!((run.state(), run.code()), ("aborted", "caller_aborted"));
        assert_eq!(run.outcome.result["verdict"]["caller_assisted"], true);
        let trace = run.trace();
        assert!(trace.contains("action_prepared"));
        assert!(trace.contains("action_fact"));
        assert!(run.artifact("dispositions/request_0001.json").is_file());
    }

    /// One caller `execute` whose outcome must end the run: the loop publishes it and never
    /// drives the run again.
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    struct TerminalExecute {
        job: Job,
        resume: Result<CapturedPage, BrowserError>,
        outcome: Option<Result<PerformFact, PerformError>>,
        journal: MemoryJournal,
        stop: (&'static str, &'static str),
        complete: bool,
        dispatched: usize,
        provider_calls: usize,
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn caller_execute_failures_end_the_run_instead_of_driving_it_again() {
        let mut exhausted = mutation_job();
        exhausted.options.max_actions = Some(0);
        let mut judged = natural(mutation_job(), "The Name field holds the account name");
        judged.options.max_model_calls = Some(2);
        let cases = [
            TerminalExecute {
                job: mutation_job(),
                resume: captured(text_field("")),
                outcome: Some(typed("Wrong")),
                journal: MemoryJournal::default(),
                stop: ("failed", "write_readback_mismatch"),
                complete: true,
                dispatched: 1,
                provider_calls: 2,
            },
            TerminalExecute {
                job: mutation_job(),
                resume: captured(text_field("")),
                outcome: None,
                journal: MemoryJournal::failing_at(0),
                stop: ("blocked", "evidence_unavailable"),
                complete: true,
                dispatched: 0,
                provider_calls: 2,
            },
            TerminalExecute {
                job: mutation_job(),
                resume: captured(text_field("")),
                outcome: Some(typed("Wanted")),
                journal: MemoryJournal::failing_at(1),
                stop: ("blocked", "evidence_incomplete_after_dispatch"),
                complete: false,
                dispatched: 1,
                provider_calls: 2,
            },
            TerminalExecute {
                job: mutation_job(),
                resume: Err(BrowserError::Control("target closed".into())),
                outcome: None,
                journal: MemoryJournal::default(),
                stop: ("blocked", "browser_control_failed"),
                complete: true,
                dispatched: 0,
                provider_calls: 2,
            },
            TerminalExecute {
                job: mutation_job(),
                resume: captured(foreign(text_field("Wanted"))),
                outcome: None,
                journal: MemoryJournal::default(),
                stop: ("blocked", "origin_not_allowed"),
                complete: true,
                dispatched: 0,
                provider_calls: 2,
            },
            TerminalExecute {
                job: exhausted,
                resume: captured(text_field("")),
                outcome: None,
                journal: MemoryJournal::default(),
                stop: ("blocked", "budget_exhausted"),
                complete: true,
                dispatched: 0,
                provider_calls: 2,
            },
            TerminalExecute {
                job: judged,
                resume: captured(text_field("")),
                outcome: None,
                journal: MemoryJournal::default(),
                stop: ("blocked", "budget_exhausted"),
                complete: true,
                dispatched: 0,
                provider_calls: 2,
            },
        ];
        for mut case in cases {
            let mut browser = FakeBrowser::capturing(
                [
                    captured(text_field("")),
                    captured(text_field("")),
                    case.resume,
                ],
                text_field("Wanted"),
            )
            .dispatching(case.outcome);
            let provider = ScriptedProvider::new([Turn::type_text().confidence(0.5)]);

            let run = run_loop(
                &case.job,
                &mut browser,
                &provider,
                &ScriptedControl::answering([execute("c_1"), retry()]),
                &mut case.journal,
            );

            let label = case.stop.1;
            assert_eq!((run.state(), run.code()), case.stop, "{label}");
            assert_eq!(run.complete(), case.complete, "{label}");
            assert!(run.outcome.result["escalation"].is_null(), "{label}");
            assert_eq!(
                run.outcome.result["verdict"]["steps"][0]["result"], "unresolved",
                "{label}"
            );
            assert_eq!(browser.dispatched(), case.dispatched, "{label}");
            assert_eq!(provider.calls(), case.provider_calls, "{label}");
        }
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn verification_disposition_failures_end_the_run_instead_of_verifying_again() {
        let ready = observed("Ready");
        let attested = observed("Ready balance 12.34");
        let changed = observed("Ready balance 12.34 pending");
        for (final_capture, stop) in [
            (captured(changed), ("failed", "expectation_not_met")),
            (
                Err(BrowserError::Control("target closed".into())),
                ("blocked", "browser_control_failed"),
            ),
        ] {
            let mut browser = FakeBrowser::capturing(
                [
                    captured(ready.clone()),
                    captured(attested.clone()),
                    final_capture,
                ],
                attested.clone(),
            );
            let provider = ScriptedProvider::new([
                Turn::verdict(0.50),
                Turn::verdict(0.05),
                Turn::verdict(0.95),
            ]);

            let run = run_loop(
                &expectation_job(),
                &mut browser,
                &provider,
                &ScriptedControl::answering([advance("The balance is visible."), retry()]),
                &mut MemoryJournal::default(),
            );

            assert_eq!((run.state(), run.code()), stop);
            assert!(run.outcome.result["escalation"].is_null());
            assert!(provider.calls() <= 2, "{}", stop.1);
        }
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn select_of_a_value_the_page_does_not_offer_pauses_without_dispatch() {
        let job = parse_job(json!({
            "schema_version":1,
            "target":{"kind":"browser","url":"http://127.0.0.1:4351/"},
            "context":{"journey":"select","revision":"fixture","environment":"fake","actor":"synthetic","authority":"select"},
            "values":{"name":{"value":"Wanted","description":"account name"}},
            "steps":[{"id":"choose","goal":"choose the account","done_when":[{"field":"Account","equals_value":"name"}]}]
        }));
        let mut page = observed("");
        page.elements.push(Element {
            select_options: vec![manuvra_chrome::SelectOption {
                node_id: 9,
                label: "Other".into(),
                value: "other".into(),
                disabled: false,
                selected: true,
            }],
            ..element(1, 8, "combobox", "Account", "SELECT")
        });
        let mut browser = FakeBrowser::new([page]);
        let control = ScriptedControl::default();

        let run = run_loop(
            &job,
            &mut browser,
            &ScriptedProvider::new([Turn::select()]),
            &control,
            &mut MemoryJournal::default(),
        );

        assert_eq!((run.state(), run.code()), ("aborted", "caller_aborted"));
        let paused = &control.checkpoints()[0];
        assert_eq!(paused["state"], "uncertain");
        assert_eq!(paused["reason"]["code"], "select_option_unavailable");
        assert!(paused["escalation"].is_object());
        assert_eq!(browser.dispatched(), 0);
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn cancellation_without_a_termination_ends_the_run_blocked() {
        let control = ScriptedControl::default();
        control.cancellation.cancel();

        let run = run_loop(
            &job("Ready"),
            &mut FakeBrowser::new([observed("Ready")]),
            &NoProvider,
            &control,
            &mut MemoryJournal::default(),
        );

        assert_eq!((run.state(), run.code()), ("blocked", "run_cancelled"));
        assert!(run.outcome.result["escalation"].is_null());
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn autonomous_journal_failures_end_the_run_truthfully() {
        for (journal, code, complete, dispatched) in [
            (
                MemoryJournal::failing_at(0),
                "evidence_unavailable",
                true,
                0,
            ),
            (
                MemoryJournal::failing_at(1),
                "evidence_incomplete_after_dispatch",
                false,
                1,
            ),
        ] {
            let mut journal = journal;
            let mut browser = FakeBrowser::new([plan_before_hover(), plan_groceries_revealed()])
                .dispatching([performed(&["mouse_move"])]);

            let run = run_loop(
                &click_job(),
                &mut browser,
                &ScriptedProvider::new([Turn::hover("R1")]),
                &ScriptedControl::default(),
                &mut journal,
            );

            assert_eq!((run.state(), run.code()), ("blocked", code));
            assert_eq!(run.complete(), complete, "{code}");
            assert_eq!(browser.dispatched(), dispatched, "{code}");
            let manifest: Value = serde_json::from_str(
                &std::fs::read_to_string(run.artifact("manifest.json")).unwrap(),
            )
            .unwrap();
            assert_eq!(manifest["complete"], complete, "{code}");
        }
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn resume_observation_on_a_foreign_origin_never_completes_or_reaches_the_provider() {
        let structured = click_job();
        let judged = natural(click_job(), "The actions menu for Groceries is open");
        for job in [structured, judged] {
            let mut browser = FakeBrowser::new([
                plan_before_hover(),
                plan_before_hover(),
                foreign(plan_menu_open()),
            ]);
            let provider = ScriptedProvider::new([low_hover_turn()]);

            let run = run_loop(
                &job,
                &mut browser,
                &provider,
                &ScriptedControl::answering([execute("c_1")]),
                &mut MemoryJournal::default(),
            );

            assert_eq!((run.state(), run.code()), ("blocked", "origin_not_allowed"));
            assert_eq!(provider.calls(), 2);
            assert_eq!(browser.dispatched(), 0);
            assert_ne!(
                run.outcome.result["verdict"]["steps"][0]["result"],
                "satisfied"
            );
        }
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn final_verification_on_a_foreign_origin_never_reaches_the_provider() {
        let provider = ScriptedProvider::new([Turn::verdict(0.95)]);

        let run = run_loop(
            &expectation_job(),
            &mut FakeBrowser::new([observed("Ready"), foreign(observed("Ready balance 12.34"))]),
            &provider,
            &ScriptedControl::default(),
            &mut MemoryJournal::default(),
        );

        assert_eq!((run.state(), run.code()), ("blocked", "origin_not_allowed"));
        assert_eq!(provider.calls(), 0);
    }

    #[test]
    fn unverifiable_resume_redaction_blocks_before_the_provider() {
        let job = natural(click_job(), "The actions menu for Groceries is open");
        let redactor = Redactor::for_job(&job).unwrap();
        let provider = ScriptedProvider::new([low_hover_turn()]);
        let mut machine = HostedMachine::new(&job, &redactor);
        let before = FakeBrowser::new([plan_before_hover()]);
        drive(
            &mut machine,
            &before,
            &provider,
            &mut MemoryJournal::default(),
        );
        let candidate = offered_hover(&machine);
        let unverifiable = FakeBrowser::capturing(
            [Err(BrowserError::Control("redaction_unverifiable".into()))],
            plan_menu_open(),
        );

        dispose(
            &mut machine,
            execute(&candidate.id),
            &unverifiable,
            &provider,
            &mut MemoryJournal::default(),
        );

        let stop = machine.artifacts.stop.as_ref().unwrap();
        assert_eq!(
            (stop.code, stop.state),
            ("redaction_unverifiable", RunState::Blocked)
        );
        assert!(machine.artifacts.escalation.is_none());
        assert_eq!(provider.calls(), 2);
        assert_eq!(unverifiable.dispatched(), 0);
        let (_, withheld, screenshot) = machine.artifacts.observations.last().unwrap();
        assert_eq!(withheld["screenshot"]["withheld"], "redaction_unverifiable");
        assert!(screenshot.is_none());
    }

    #[test]
    fn caller_execute_press_key_offers_anchor_and_dispatches_once() {
        let job = force_stop(key_activation_job());
        let redactor = Redactor::for_job(&job).unwrap();
        let focused = key_observation("Save");
        let mut activated = focused.clone();
        activated.visible_text = "Activations: 1".into();
        let browser = FakeBrowser::new([focused.clone(), focused, activated])
            .dispatching([performed(&["key_down", "key_up"])]);
        let provider = ScriptedProvider::new([Turn::key("Enter")]);
        let mut machine = HostedMachine::new(&job, &redactor);
        let mut journal = MemoryJournal::default();
        drive(&mut machine, &browser, &provider, &mut journal);
        let offered = &machine.artifacts.escalations[0].1["offered_candidate"];
        assert_eq!(offered["operation"], "PRESS_KEY");
        assert_eq!(offered["key"], "Enter");
        assert_eq!(offered["focus_anchor"]["name"], "Save");
        assert!(
            machine.artifacts.escalations[0].1["permitted_mutations"]
                .as_array()
                .unwrap()
                .contains(&json!("PRESS_KEY"))
        );
        let candidate_id = offered["id"].as_str().unwrap().to_owned();

        dispose(
            &mut machine,
            execute(&candidate_id),
            &browser,
            &provider,
            &mut journal,
        );
        drive(&mut machine, &browser, &provider, &mut journal);

        assert_eq!(machine.index, 1);
        assert_eq!(
            machine.artifacts.verdicts[0].result,
            VerdictResult::Satisfied
        );
        let prepared = journal.prepared();
        assert_eq!(prepared.len(), 1);
        assert_eq!(prepared[0]["key"], "Enter");
        assert_eq!(provider.calls(), 1);
    }

    #[test]
    fn caller_execute_revalidates_the_offered_candidate_without_dispatch() {
        let key_job = force_stop(key_activation_job());
        let element_job = force_stop(mutation_job());
        let mut remounted_field = text_field("");
        remounted_field.elements[0].node_id = 99;
        for (job, pages, turn) in [
            (
                key_job,
                [key_observation("Save"), key_observation("Other")],
                Turn::key("Enter"),
            ),
            (
                element_job,
                [text_field(""), remounted_field],
                Turn::type_text(),
            ),
        ] {
            let redactor = Redactor::for_job(&job).unwrap();
            let browser = FakeBrowser::new(pages);
            let provider = ScriptedProvider::new([turn]);
            let mut machine = HostedMachine::new(&job, &redactor);
            let mut journal = MemoryJournal::default();
            drive(&mut machine, &browser, &provider, &mut journal);
            let candidate = offered(&machine);

            dispose(
                &mut machine,
                execute(&candidate.id),
                &browser,
                &provider,
                &mut journal,
            );

            assert_eq!(
                paused(&machine),
                ("candidate_revalidation_failed", RunState::Uncertain, false)
            );
            assert!(!offers_execute(&machine));
            assert!(!machine.artifacts.caller_assisted);
            assert!(journal.entries.is_empty());
            assert_eq!(browser.dispatched(), 0);
        }

        let job = click_job();
        let redactor = Redactor::for_job(&job).unwrap();
        let mut remounted = plan_before_hover();
        remounted.hover_regions[0].node_id = 99;
        let mut renamed = plan_before_hover();
        renamed.hover_regions[0].name = "Groceries and dining".into();
        let mut revealing_other = plan_before_hover();
        revealing_other.hover_regions[0].reveals_on_hover = vec!["Rename Groceries".into()];
        let mut navigated = plan_before_hover();
        navigated.document_id = "another-document".into();
        for changed in [
            plan_groceries_revealed(),
            remounted,
            renamed,
            revealing_other,
            navigated,
        ] {
            let mut machine = hover_escalation(&job, &redactor);
            let candidate = offered_hover(&machine);
            let browser = FakeBrowser::new([changed]);
            dispose(
                &mut machine,
                execute(&candidate.id),
                &browser,
                &NoProvider,
                &mut MemoryJournal::default(),
            );
            assert_eq!(
                paused(&machine),
                ("candidate_revalidation_failed", RunState::Uncertain, false)
            );
            assert!(!offers_execute(&machine));
            assert!(!machine.artifacts.caller_assisted);
            assert_eq!(browser.dispatched(), 0);
        }

        let mut machine = hover_escalation(&job, &redactor);
        let candidate = offered_hover(&machine);
        let _reserved = machine
            .policy
            .authorize_caller(&job.steps[0], &plan_before_hover(), &candidate)
            .unwrap();
        let browser = FakeBrowser::new([plan_before_hover()]);
        dispose(
            &mut machine,
            execute(&candidate.id),
            &browser,
            &NoProvider,
            &mut MemoryJournal::default(),
        );
        assert_eq!(
            paused(&machine),
            ("replay_forbidden", RunState::Uncertain, false)
        );
        assert_eq!(browser.dispatched(), 0);
    }

    /// A run paused on a candidate for `operation`, with the pages its drive and execute see.
    fn caller_candidate_run(operation: &str) -> (Job, Vec<Observation>, Turn) {
        match operation {
            "CLICK" => (
                force_stop(key_activation_job()),
                vec![save_button_focus(); 2],
                Turn::click("1"),
            ),
            "PRESS_KEY" => (
                force_stop(key_activation_job()),
                vec![save_button_focus(); 2],
                Turn::key("Enter"),
            ),
            _ => (click_job(), vec![plan_before_hover(); 3], low_hover_turn()),
        }
    }

    #[test]
    fn caller_dispatch_proven_not_performed_releases_replay_and_reissues_without_a_candidate() {
        for operation in ["CLICK", "PRESS_KEY", "HOVER"] {
            for failure in [
                PerformError::Rejected("focus_changed".into()),
                PerformError::NotPerformed("cancelled before transport send".into()),
            ] {
                let (job, pages, turn) = caller_candidate_run(operation);
                let redactor = Redactor::for_job(&job).unwrap();
                let page = pages[0].clone();
                let browser = FakeBrowser::new(pages).dispatching([Err(failure)]);
                let provider = ScriptedProvider::new([turn]);
                let mut machine = HostedMachine::new(&job, &redactor);
                let mut journal = MemoryJournal::default();
                drive(&mut machine, &browser, &provider, &mut journal);
                let candidate = offered(&machine);
                let calls = provider.calls();
                assert_eq!(
                    machine.artifacts.escalations[0].1["offered_candidate"]["operation"],
                    operation
                );

                dispose(
                    &mut machine,
                    execute(&candidate.id),
                    &browser,
                    &provider,
                    &mut journal,
                );

                assert_eq!(
                    paused(&machine),
                    ("candidate_revalidation_failed", RunState::Uncertain, false),
                    "{operation}"
                );
                let reissued = &machine.artifacts.escalations.last().unwrap().1;
                assert_eq!(reissued["gate_reason"], "candidate_revalidation_failed");
                assert!(reissued["offered_candidate"].is_null());
                assert!(!offers_execute(&machine));
                assert!(machine.artifacts.caller_assisted);
                let prepared = journal.prepared();
                assert_eq!(prepared.len(), 1, "{operation}");
                assert_eq!(prepared[0]["operation"], operation);
                assert_eq!(prepared[0]["basis"], "caller_authority");
                let fact = journal
                    .entries
                    .iter()
                    .find(|event| event["event"] == "action_fact")
                    .unwrap();
                assert_eq!(fact["fact"]["outcome"], "not_performed");
                assert_eq!(machine.index, 0);
                assert_eq!(
                    machine.artifacts.verdicts[0].result,
                    VerdictResult::Unresolved
                );
                assert_eq!(provider.calls(), calls, "{operation}");
                assert_eq!(machine.policy.step_mutations(), 0);
                let permit = machine
                    .policy
                    .authorize_caller(&job.steps[0], &page, &candidate)
                    .expect("a proven non-effect releases its replay entry");
                machine.policy.release_unused(Box::new(permit));
            }
        }
    }

    #[test]
    fn possibly_performed_dispatch_escalates_as_an_ambiguous_action_and_keeps_the_replay_key() {
        let job = click_job();
        let redactor = Redactor::for_job(&job).unwrap();
        let mut machine = hover_escalation(&job, &redactor);
        let candidate = offered_hover(&machine);
        let browser = FakeBrowser::new([plan_before_hover()])
            .dispatching([Err(PerformError::Uncertain("sent without answer".into()))]);
        dispose(
            &mut machine,
            execute(&candidate.id),
            &browser,
            &NoProvider,
            &mut MemoryJournal::default(),
        );
        assert_eq!(
            paused(&machine),
            ("action_outcome_uncertain", RunState::Uncertain, false)
        );
        assert!(
            machine
                .artifacts
                .pending
                .as_ref()
                .unwrap()
                .ambiguous_mutation
        );
        assert!(machine.artifacts.caller_assisted);
        assert_eq!(browser.dispatched(), 1);
        assert!(matches!(
            machine
                .policy
                .authorize_caller(&job.steps[0], &plan_before_hover(), &candidate),
            Err(policy::PolicyStop::Uncertain("replay_forbidden"))
        ));

        let browser = FakeBrowser::new([plan_before_hover()])
            .dispatching([Err(PerformError::Uncertain("sent without answer".into()))]);
        let artifacts = driven(
            &job,
            &browser,
            &ScriptedProvider::new([Turn::hover("R1")]),
            &mut MemoryJournal::default(),
        );
        assert_eq!(
            artifacts.stop.as_ref().unwrap().code,
            "action_outcome_uncertain"
        );
        assert_eq!(artifacts.escalations[0].1["offered_candidate"], Value::Null);
        assert!(artifacts.pending.as_ref().unwrap().ambiguous_mutation);
        assert_eq!(browser.dispatched(), 1);
    }

    #[test]
    fn uncertain_key_press_cannot_be_executed_or_replayed_after_retry_observation() {
        let lost_key_up = || PerformError::Uncertain("lost key response".into());
        for (key, before, after) in [
            ("Enter", key_observation("Save"), key_observation("Save")),
            (
                "Enter",
                anchor_state("button", Some(false), None),
                anchor_state("button", Some(true), None),
            ),
            (
                "Space",
                anchor_state("checkbox", None, Some(false)),
                anchor_state("checkbox", None, Some(true)),
            ),
        ] {
            let job = key_activation_job();
            let redactor = Redactor::for_job(&job).unwrap();
            let browser = FakeBrowser::new([before, after]).dispatching([Err(lost_key_up())]);
            let provider = ScriptedProvider::new([Turn::key(key)]);
            let mut machine = HostedMachine::new(&job, &redactor);
            let mut journal = MemoryJournal::default();
            drive(&mut machine, &browser, &provider, &mut journal);
            assert_eq!(
                paused(&machine),
                ("action_outcome_uncertain", RunState::Uncertain, false)
            );
            assert!(!offers_execute(&machine));

            dispose(&mut machine, retry(), &browser, &provider, &mut journal);
            drive(&mut machine, &browser, &provider, &mut journal);

            assert_eq!(
                machine.artifacts.stop.as_ref().unwrap().code,
                "replay_forbidden",
                "{key}"
            );
            assert_eq!(provider.calls(), 2);
            assert_eq!(journal.prepared().len(), 1, "{key}");
            let refused = &machine.artifacts.escalations[1].1;
            assert_eq!(refused["gate_reason"], "replay_forbidden");
            assert_eq!(refused["candidates"]["operation"]["choice"], "PRESS_KEY");
            assert_eq!(refused["candidates"]["key"]["choice"], key);
            let prepared = refused["recent_actions"]
                .as_array()
                .unwrap()
                .iter()
                .find(|event| event["event"] == "action_prepared")
                .unwrap();
            assert_eq!(prepared["operation"], "PRESS_KEY");
            assert_eq!(prepared["key"], key);
            assert_eq!(prepared["focus_anchor"]["name"], "Save");
        }
    }

    #[test]
    fn autonomous_arrow_sequence_selects_observed_beta() {
        let job = key_job(
            "choose",
            "Press the arrow keys and Enter to choose Beta",
            json!([{"text_visible":"Selected: Beta"}]),
            3,
        );
        let mut before = key_observation("Choose item");
        before.focus_anchor.as_mut().unwrap().role = "combobox".into();
        let descendant = |id: &str, name: &str, selected: bool| {
            let mut page = before.clone();
            page.focus_anchor.as_mut().unwrap().active_descendant =
                Some(manuvra_chrome::ActiveDescendant {
                    id: id.into(),
                    role: "option".into(),
                    name: name.into(),
                    selected: Some(selected),
                    checked: None,
                });
            page
        };
        let alpha = descendant("alpha", "Alpha", false);
        let beta = descendant("beta", "Beta", false);
        let mut selected = descendant("beta", "Beta", true);
        selected.visible_text = "Selected: Beta".into();
        let browser = FakeBrowser::new([before.clone(), alpha, beta, selected])
            .always_dispatching(performed(&["key_down", "key_up"]));
        let provider = ScriptedProvider::new([
            Turn::key("ArrowDown"),
            Turn::key("ArrowDown"),
            Turn::key("Enter"),
        ]);

        let artifacts = driven(&job, &browser, &provider, &mut MemoryJournal::default());

        assert!(artifacts.stop.is_none());
        assert_eq!(artifacts.verdicts[0].result, VerdictResult::Satisfied);
        assert_eq!(provider.calls(), 3);
        let keys: Vec<_> = artifacts
            .trace
            .iter()
            .filter(|event| event["event"] == "action_prepared")
            .map(|event| event["key"].as_str().unwrap())
            .collect();
        assert_eq!(keys, ["ArrowDown", "ArrowDown", "Enter"]);
    }

    #[test]
    fn autonomous_escape_records_anchor_and_observes_restored_focus() {
        let job = key_job(
            "close",
            "Press Escape to close the breakdown popover",
            json!([{"dialog_closed":"Breakdown"}]),
            1,
        );
        let mut before = key_observation("First");
        before.dialogs.push("Breakdown".into());
        before.focus_anchor.as_mut().unwrap().in_dialog = Some("Breakdown".into());
        let browser = FakeBrowser::new([before, key_observation("Open breakdown")])
            .dispatching([performed(&["key_down", "key_up"])]);
        let provider = ScriptedProvider::new([Turn::key("Escape")]);

        let artifacts = driven(&job, &browser, &provider, &mut MemoryJournal::default());

        assert!(artifacts.stop.is_none());
        assert_eq!(artifacts.verdicts[0].result, VerdictResult::Satisfied);
        assert_eq!(provider.calls(), 1);
        let prepared = artifacts
            .trace
            .iter()
            .find(|value| value["event"] == "action_prepared")
            .unwrap();
        let fact = artifacts
            .trace
            .iter()
            .find(|value| value["event"] == "action_fact")
            .unwrap();
        assert_eq!(prepared["operation"], "PRESS_KEY");
        assert_eq!(prepared["key"], "Escape");
        assert_eq!(prepared["focus_anchor"]["name"], "First");
        assert_eq!(fact["fact"]["key"], "Escape");
        assert!(
            artifacts
                .observations
                .iter()
                .any(|(_, value, _)| value["focus_anchor"]["name"] == "Open breakdown")
        );
    }

    #[test]
    fn autonomous_tab_reaches_focus_condition_within_mutation_limit() {
        let job = key_job(
            "move",
            "Press Tab to focus Third",
            json!([{"focused":"Third"}]),
            2,
        );
        let browser = FakeBrowser::new([
            key_observation("First"),
            key_observation("Second"),
            key_observation("Third"),
        ])
        .always_dispatching(performed(&["key_down", "key_up"]));
        let provider = ScriptedProvider::new([Turn::key("Tab")]);

        let artifacts = driven(&job, &browser, &provider, &mut MemoryJournal::default());

        assert!(artifacts.stop.is_none());
        assert_eq!(artifacts.verdicts[0].result, VerdictResult::Satisfied);
        assert_eq!(provider.calls(), 2);
        assert_eq!(action_operations(&artifacts.trace, "action_fact").len(), 2);
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

    /// A run paused on uncertain final verification of the expectation job.
    fn verification_escalation<'a>(
        job: &'a Job,
        redactor: &'a Redactor,
        browser: &FakeBrowser,
        provider: &ScriptedProvider,
    ) -> HostedMachine<'a> {
        let mut machine = HostedMachine::new(job, redactor);
        drive(
            &mut machine,
            browser,
            provider,
            &mut MemoryJournal::default(),
        );
        let escalation = machine.artifacts.escalation.clone().unwrap();
        assert_eq!(escalation.phase, "verification");
        machine
    }

    #[test]
    fn verification_uncertainty_allows_attestation_but_refuses_execute() {
        let job = expectation_job();
        let redactor = Redactor::for_job(&job).unwrap();
        let browser = FakeBrowser::new([observed("Ready"), observed("Ready balance 12.34")]);
        let provider = ScriptedProvider::new([Turn::verdict(0.50)]);
        let mut machine = verification_escalation(&job, &redactor, &browser, &provider);
        let escalation = machine.artifacts.escalation.clone().unwrap();
        assert_eq!(escalation.step_id, None);
        assert_eq!(
            escalation.dispositions,
            [
                DispositionKind::Advance,
                DispositionKind::RetryObservation,
                DispositionKind::Abort,
            ]
        );
        let mut journal = MemoryJournal::default();

        dispose(
            &mut machine,
            execute("not-offered"),
            &browser,
            &provider,
            &mut journal,
        );
        assert_eq!(
            machine.artifacts.stop.as_ref().unwrap().code,
            "execute_not_permitted"
        );
        assert!(!machine.verification_complete);

        dispose(
            &mut machine,
            advance("Observed the final account facts directly."),
            &browser,
            &provider,
            &mut journal,
        );
        assert!(machine.verification_complete);
        assert!(machine.artifacts.stop.is_none());
        assert!(machine.artifacts.caller_assisted);
        assert_eq!(
            machine.artifacts.expectation_verdicts[0].result,
            VerdictResult::Satisfied
        );
        assert_eq!(
            machine.artifacts.verification.as_ref().unwrap()["basis"],
            "caller_attestation"
        );
    }

    #[test]
    fn verification_attestation_rechecks_changed_facts_identity_and_focus() {
        let mut scoped = expectation_job();
        scoped.expectations[0].exact_literals = vec![manuvra_contract::ExactLiteral {
            literal: "12.34".into(),
            within_text: Some("Wallet".into()),
        }];
        let wallet = observed("Ready\nWallet balance 12.34");
        let mut reserve = wallet.clone();
        reserve.visible_text = "Ready\nWallet balance 12.34\nWallet reserve 12.34".into();
        let balance = observed("Ready balance 12.34");
        let mut remounted = balance.clone();
        remounted.document_id = "remounted-document".into();
        let mut unfocused = text_field("");
        unfocused.visible_text = "Ready balance 12.34".into();
        let mut focused = unfocused.clone();
        focused.focused = Some(1);
        for (job, attested, current, identity_unchanged, facts_unchanged) in [
            (scoped, wallet, reserve, true, false),
            (expectation_job(), balance, remounted, false, true),
            (expectation_job(), unfocused, focused, true, false),
        ] {
            let redactor = Redactor::for_job(&job).unwrap();
            let browser = FakeBrowser::new([observed("Ready"), attested, current]);
            let provider = ScriptedProvider::new([Turn::verdict(0.50)]);
            let mut machine = verification_escalation(&job, &redactor, &browser, &provider);

            dispose(
                &mut machine,
                advance("Caller attests the prior final facts."),
                &browser,
                &provider,
                &mut MemoryJournal::default(),
            );

            assert!(!machine.verification_complete);
            assert!(!machine.artifacts.caller_assisted);
            assert_eq!(machine.artifacts.observations.len(), 3);
            assert_eq!(
                machine.artifacts.stop.as_ref().unwrap().code,
                "verification_state_changed"
            );
            assert_eq!(
                machine.artifacts.expectation_verdicts[0].result,
                VerdictResult::Unresolved
            );
            assert_eq!(provider.calls(), 2);
            let check = machine
                .artifacts
                .trace
                .iter()
                .find(|entry| entry["event"] == "verification_disposition_check")
                .unwrap();
            assert_eq!(check["document_identity_unchanged"], identity_unchanged);
            assert_eq!(check["relevant_facts_unchanged"], facts_unchanged);
        }
    }

    #[test]
    fn retry_observation_rechecks_final_expectations_without_resetting_the_run() {
        let job = expectation_job();
        let redactor = Redactor::for_job(&job).unwrap();
        let browser = FakeBrowser::new([observed("Ready"), observed("Ready balance 12.34")]);
        let provider = ScriptedProvider::new([Turn::verdict(0.50), Turn::verdict(0.95)]);
        let mut machine = verification_escalation(&job, &redactor, &browser, &provider);
        let mut journal = MemoryJournal::default();

        dispose(&mut machine, retry(), &browser, &provider, &mut journal);
        drive(&mut machine, &browser, &provider, &mut journal);

        assert!(machine.verification_complete);
        assert!(machine.artifacts.stop.is_none());
        assert_eq!(machine.artifacts.observations.len(), 3);
        assert_eq!(machine.artifacts.expectation_verdicts[0].noul, Some(0.95));
    }

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

    /// One way a step reaches the forced debug stop: the pages and provider turns of the run, the
    /// dispatches that precede the stop, and whether the caller first executes an offered hover.
    struct ForcedStop {
        job: Job,
        pages: Vec<Observation>,
        turns: Vec<Turn>,
        dispatches: Vec<Result<PerformFact, PerformError>>,
        execute_first: bool,
        dispatched: Vec<PreparedOperation>,
        offered: (&'static str, &'static str),
    }

    #[test]
    fn debug_force_stop_fires_on_the_first_mutation_after_scrolls_and_hovers() {
        let mut scrollable = observed("Plan");
        scrollable
            .elements
            .push(button(1, 7, "Actions for Groceries"));
        scrollable.viewport.document_height = 2_000.0;
        let cases = [
            ForcedStop {
                job: force_stop(mutation_job()),
                pages: vec![text_field("")],
                turns: vec![Turn::type_text()],
                dispatches: vec![],
                execute_first: false,
                dispatched: vec![],
                offered: ("TYPE_TEXT", "Name"),
            },
            ForcedStop {
                job: force_stop(click_job()),
                pages: vec![scrollable],
                turns: vec![Turn::scroll_down(), Turn::click("1")],
                dispatches: vec![performed(&["scroll_down"])],
                execute_first: false,
                dispatched: vec![PreparedOperation::ScrollDown],
                offered: ("CLICK", "Actions for Groceries"),
            },
            ForcedStop {
                job: force_stop(click_job()),
                pages: vec![plan_before_hover(), plan_groceries_revealed()],
                turns: vec![Turn::hover("R1"), Turn::click("2")],
                dispatches: vec![performed(&["mouse_move"])],
                execute_first: false,
                dispatched: vec![PreparedOperation::Hover],
                offered: ("CLICK", "Actions for Groceries"),
            },
            ForcedStop {
                job: force_stop(click_job()),
                pages: vec![
                    plan_before_hover(),
                    plan_before_hover(),
                    plan_before_hover(),
                    plan_groceries_revealed(),
                ],
                turns: vec![low_hover_turn(), low_hover_turn(), Turn::click("2")],
                dispatches: vec![performed(&["mouse_move"])],
                execute_first: true,
                dispatched: vec![PreparedOperation::Hover],
                offered: ("CLICK", "Actions for Groceries"),
            },
        ];
        for case in cases {
            let redactor = Redactor::for_job(&case.job).unwrap();
            let browser = FakeBrowser::new(case.pages).dispatching(case.dispatches);
            let provider = ScriptedProvider::new(case.turns);
            let mut machine = HostedMachine::new(&case.job, &redactor);
            let mut journal = MemoryJournal::default();
            drive(&mut machine, &browser, &provider, &mut journal);
            if case.execute_first {
                let candidate = offered_hover(&machine);
                dispose(
                    &mut machine,
                    execute(&candidate.id),
                    &browser,
                    &provider,
                    &mut journal,
                );
                assert!(machine.artifacts.stop.is_none());
                drive(&mut machine, &browser, &provider, &mut journal);
            }

            let label = case.offered.0;
            assert_eq!(
                paused(&machine),
                ("debug_forced_stop", RunState::Uncertain, true),
                "{label}"
            );
            assert_eq!(browser.operations(), case.dispatched, "{label}");
            let escalation = &machine.artifacts.escalations.last().unwrap().1;
            assert_eq!(escalation["offered_candidate"]["operation"], case.offered.0);
            assert_eq!(
                escalation["offered_candidate"]["target_name"],
                case.offered.1
            );
            assert_eq!(
                escalation["permitted_mutations"],
                json!(["CLICK", "TYPE_TEXT", "PRESS_KEY"])
            );
            let exported = escalation.to_string();
            for identity in ["document_id", "node_id", "target_index"] {
                assert!(!exported.contains(identity), "{label} {identity}");
            }
        }
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
    fn hover_then_click_satisfies_the_step_in_two_actions_with_one_mutation() {
        let browser = FakeBrowser::new([
            plan_before_hover(),
            plan_groceries_revealed(),
            plan_menu_open(),
        ])
        .dispatching([
            performed(&["mouse_move"]),
            performed(&["mouse_press", "mouse_release"]),
        ]);
        let provider = ScriptedProvider::new([Turn::hover("R1"), Turn::click("2")]);

        let artifacts = driven(
            &click_job(),
            &browser,
            &provider,
            &mut MemoryJournal::default(),
        );

        assert!(
            artifacts.stop.is_none(),
            "{:?}",
            artifacts.stop.map(|stop| stop.code)
        );
        assert_eq!(artifacts.verdicts[0].result, VerdictResult::Satisfied);
        assert_eq!(artifacts.steps[0].1["mutation_limit_consumed"], 1);
        assert_eq!(
            trace_events(&artifacts),
            [
                "observation",
                "action_prepared:HOVER",
                "action_fact:HOVER",
                "observation",
                "action_prepared:CLICK",
                "action_fact:CLICK",
                "observation",
                "final_verification_observation",
            ]
        );
        let groceries =
            json!({"index":1,"name":"Groceries","reveals_on_hover":["Actions for Groceries"]});
        assert_eq!(artifacts.trace[1]["hover_target"], groceries);
        assert_eq!(artifacts.trace[2]["fact"]["hover_target"], groceries);
        assert_eq!(artifacts.trace[2]["fact"]["outcome"], "observed");
        assert_eq!(
            artifacts.trace[2]["fact"]["suboperations"],
            json!(["mouse_move"])
        );
        assert_eq!(
            artifacts.observations[1].1["elements"][1]["name"],
            "Actions for Groceries"
        );
        let inputs = browser.inputs.lock().unwrap();
        assert_eq!(
            inputs
                .iter()
                .map(|input| (input.operation, input.node_id))
                .collect::<Vec<_>>(),
            [
                (PreparedOperation::Hover, 41),
                (PreparedOperation::Click, 41)
            ]
        );
        let requests = provider.requests.lock().unwrap();
        assert!(requests[0]["questions"]["operation"]["criteria"]["HOVER"].is_string());
        assert_eq!(
            requests[1]["questions"]["click_target"]["criteria"]["2"]["name"],
            "Actions for Groceries"
        );
        assert!(
            requests[1]["state"]["recent_actions"]
                .as_array()
                .unwrap()
                .iter()
                .any(|action| action.as_str().unwrap().contains(
                    r#""hover_target":{"index":1,"name":"Groceries","reveals_on_hover":["Actions for Groceries"]}"#
                ))
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

    #[test]
    fn executed_hover_runs_under_caller_authority_and_the_step_continues_autonomously() {
        let job = click_job();
        let redactor = Redactor::for_job(&job).unwrap();
        let mut machine = hover_escalation(&job, &redactor);
        let candidate = offered_hover(&machine);
        let browser = FakeBrowser::new([
            plan_before_hover(),
            plan_groceries_revealed(),
            plan_menu_open(),
        ])
        .dispatching([performed(&["mouse_move"]), performed(&["mouse_release"])]);
        let provider = ScriptedProvider::new([Turn::click("2")]);
        let mut journal = MemoryJournal::default();

        dispose(
            &mut machine,
            execute(&candidate.id),
            &browser,
            &provider,
            &mut journal,
        );

        assert!(machine.artifacts.stop.is_none());
        assert!(machine.artifacts.caller_assisted);
        assert_eq!(machine.policy.step_mutations(), 0);
        drive(&mut machine, &browser, &provider, &mut journal);

        assert!(
            machine.artifacts.stop.is_none(),
            "{:?}",
            machine.artifacts.stop.as_ref().map(|stop| stop.code)
        );
        assert_eq!(machine.index, 1);
        assert_eq!(
            machine.artifacts.verdicts[0].result,
            VerdictResult::Satisfied
        );
        assert_eq!(machine.artifacts.steps[0].1["mutation_limit_consumed"], 1);
        let prepared: Vec<_> = journal
            .prepared()
            .into_iter()
            .map(|event| (event["operation"].clone(), event["basis"].clone()))
            .collect();
        assert_eq!(
            prepared,
            [
                (json!("HOVER"), json!("caller_authority")),
                (json!("CLICK"), json!("autonomous")),
            ]
        );
        assert_eq!(
            journal.entries[0]["hover_target"],
            json!({"index":1,"name":"Groceries","reveals_on_hover":["Actions for Groceries"]})
        );
        let inputs = browser.inputs.lock().unwrap();
        assert_eq!(
            inputs
                .iter()
                .map(|input| (input.operation, input.node_id))
                .collect::<Vec<_>>(),
            [
                (PreparedOperation::Hover, 41),
                (PreparedOperation::Click, 41)
            ]
        );
        assert_eq!(
            provider.calls(),
            1,
            "an executed hover is not re-asked through the operation gate"
        );
        assert!(
            machine
                .artifacts
                .trace
                .iter()
                .any(|event| event["event"] == "action_fact"
                    && event["fact"]["operation"] == "HOVER"
                    && event["fact"]["outcome"] == "observed")
        );
    }

    #[test]
    fn executing_a_hover_requires_the_pending_escalation_and_its_offered_candidate() {
        let job = click_job();
        let redactor = Redactor::for_job(&job).unwrap();
        let mut machine = hover_escalation(&job, &redactor);
        let candidate = offered_hover(&machine);
        let browser =
            FakeBrowser::new([plan_before_hover()]).dispatching([performed(&["mouse_move"])]);
        let mut journal = MemoryJournal::default();

        dispose(
            &mut machine,
            execute("c_9"),
            &browser,
            &NoProvider,
            &mut journal,
        );
        assert_eq!(
            paused(&machine),
            ("candidate_not_offered", RunState::Uncertain, true)
        );
        assert!(offers_execute(&machine));

        dispose(
            &mut machine,
            execute(&candidate.id),
            &browser,
            &NoProvider,
            &mut journal,
        );
        assert!(machine.artifacts.stop.is_none());
        dispose(
            &mut machine,
            execute(&candidate.id),
            &browser,
            &NoProvider,
            &mut journal,
        );
        assert_eq!(
            paused(&machine),
            ("stale_escalation", RunState::Uncertain, false)
        );
        assert_eq!(browser.operations(), [PreparedOperation::Hover]);
    }

    #[test]
    fn executing_a_hover_completes_or_reissues_on_the_fresh_done_result() {
        let job = click_job();
        let redactor = Redactor::for_job(&job).unwrap();
        let mut machine = hover_escalation(&job, &redactor);
        let candidate = offered_hover(&machine);
        let done = FakeBrowser::new([plan_menu_open()]);
        dispose(
            &mut machine,
            execute(&candidate.id),
            &done,
            &NoProvider,
            &mut MemoryJournal::default(),
        );
        assert_eq!(machine.index, 1);
        assert!(machine.artifacts.stop.is_none());
        assert!(machine.artifacts.caller_assisted);
        assert_eq!(machine.artifacts.steps[0].1["done"], "satisfied");
        assert_eq!(done.dispatched(), 0);

        let mut machine = hover_escalation(&job, &redactor);
        let mut partial = plan_before_hover();
        partial.coverage.viewport_complete = false;
        let unknown = FakeBrowser::new([partial]);
        dispose(
            &mut machine,
            execute(&candidate.id),
            &unknown,
            &NoProvider,
            &mut MemoryJournal::default(),
        );
        assert_eq!(
            paused(&machine),
            ("done_unknown", RunState::Uncertain, false)
        );
        assert_eq!(machine.index, 0);
        assert_eq!(unknown.dispatched(), 0);
    }

    #[test]
    fn autonomous_dispatch_proven_not_performed_is_reobserved_and_retried_without_a_mutation() {
        for failure in [
            PerformError::Rejected("covered".into()),
            PerformError::NotPerformed("not queued".into()),
        ] {
            let browser = FakeBrowser::new([
                plan_before_hover(),
                plan_before_hover(),
                plan_groceries_revealed(),
                plan_menu_open(),
            ])
            .dispatching([
                Err(failure),
                performed(&["mouse_move"]),
                performed(&["mouse_release"]),
            ]);
            let provider =
                ScriptedProvider::new([Turn::hover("R1"), Turn::hover("R1"), Turn::click("2")]);

            let artifacts = driven(
                &click_job(),
                &browser,
                &provider,
                &mut MemoryJournal::default(),
            );

            assert!(
                artifacts.stop.is_none(),
                "{:?}",
                artifacts.stop.map(|stop| stop.code)
            );
            let outcomes: Vec<_> = artifacts
                .trace
                .iter()
                .filter(|entry| entry["event"] == "action_fact")
                .map(|entry| entry["fact"]["outcome"].as_str().unwrap().to_owned())
                .collect();
            assert_eq!(outcomes, ["not_performed", "observed", "observed"]);
            assert_eq!(artifacts.steps[0].1["mutation_limit_consumed"], 1);
        }
    }

    #[test]
    fn a_permit_that_cannot_be_prepared_escalates_without_journal_or_dispatch() {
        let job = click_job();
        let redactor = Redactor::for_job(&job).unwrap();
        let browser = FakeBrowser::new([plan_before_hover()]);
        let mut journal = MemoryJournal::default();
        let values = Values::new(&job);
        let cancellation = manuvra_chrome::InputCancellation::default();
        let mut policy = policy::Policy::new(&job.options, "http://127.0.0.1:4351/");
        let mut artifacts = RunArtifacts::new(&job, &redactor);
        let judgments = judgment::Judgments {
            hover_target: Some(judgment::ChoiceJudgment {
                choice: "R1".into(),
                probabilities: BTreeMap::from([("R1".into(), 1.0)]),
                confidence: 1.0,
            }),
            ..mutation_judgments("HOVER", 1.0)
        };
        let policy::Next::Mutate(permit) = policy.decide(
            &job.steps[0],
            &plan_before_hover(),
            &judgments,
            DoneResult::NotSatisfied,
            false,
            false,
        ) else {
            panic!("hover permit");
        };
        let mut remounted = plan_before_hover();
        remounted.hover_regions[0].node_id = 99;
        let captured = Captured {
            raw: remounted,
            artifact: ("observation".into(), Value::Null, None),
            redaction_verified: true,
        };
        let mut driver = StepDriver {
            job: &job,
            redactor: &redactor,
            browser: &browser,
            evaluator: &NoProvider,
            journal: &mut journal,
            values: &values,
            cancellation: &cancellation,
            policy: &mut policy,
            index: 0,
            step: &job.steps[0],
            artifacts: &mut artifacts,
            observation_number: 0,
            done_unknown_reobserved: false,
            operation_gate_reobserved: false,
            mutations: 0,
            awaiting_final_done_reobservation: false,
        };

        let progress = driver.mutate(DoneResult::NotSatisfied, &captured, &judgments, *permit);

        let StepProgress::Stop(stop) = progress else {
            panic!("an unprepared permit stops the step");
        };
        assert_eq!(
            (stop.code, stop.state),
            ("candidate_revalidation_failed", RunState::Uncertain)
        );
        assert!(artifacts.escalation.is_some());
        assert!(!artifacts.pending.as_ref().unwrap().ambiguous_mutation);
        assert_eq!(browser.dispatched(), 0);
        assert!(journal.entries.is_empty());
    }

    #[test]
    fn exported_observation_keeps_public_indices_but_omits_browser_identity() {
        let mut job = mutation_job();
        job.values.get_mut("name").unwrap().secret = true;
        let redactor = Redactor::for_job(&job).unwrap();
        let mut observation = text_field("");
        observation.document_id = "internal-document-token".into();
        observation.elements[0].node_id = 981_723;
        observation.elements[0].context = "main/shadow:981723".into();
        observation.focus_anchor = Some(FocusAnchor {
            node_id: 981_723,
            context: "main/shadow:981723".into(),
            role: "textbox".into(),
            name: "Wanted".into(),
            in_dialog: None,
            covered: true,
            surface: None,
            active_descendant: None,
            expanded: None,
            selected: None,
            checked: None,
            position: None,
        });
        let exported = redacted_observation(&observation, &redactor).unwrap();
        let text = exported.to_string();

        assert_eq!(exported["elements"][0]["index"], 1);
        assert_eq!(exported["elements"][0]["name"], "Name");
        assert!(!text.contains("internal-document-token"));
        assert!(!text.contains("981723"));
        assert!(!text.contains("document_id"));
        assert!(!text.contains("node_id"));
        assert!(!text.contains("main/shadow"));
        assert!(!text.contains("Wanted"));
        assert!(
            exported["focus_anchor"]["name"]
                .as_str()
                .unwrap()
                .contains("<masked:")
        );
        assert!(exported["elements"][0].get("context").is_none());
    }

    fn richly_observed() -> Observation {
        let mut observation = text_field("Wanted");
        observation.visible_text = "Ready Wanted".into();
        observation.covered_text = "Behind Wanted".into();
        observation.dialogs = vec!["Confirm Wanted".into()];
        observation.dialog_texts =
            BTreeMap::from([("Confirm Wanted".into(), "Keep Wanted?".into())]);
        observation.focused = Some(1);
        observation.elements.push(Element {
            index: 2,
            node_id: 8,
            context: "main/frame:3".into(),
            role: "combobox".into(),
            name: "Account".into(),
            input_type: None,
            value: "wanted-id".into(),
            checked: Some(false),
            selected: None,
            expanded: Some(true),
            disabled: false,
            in_dialog: Some("Confirm Wanted".into()),
            operations: vec!["SELECT".into()],
            select_options: vec![manuvra_chrome::SelectOption {
                node_id: 9,
                label: "Wanted".into(),
                value: "wanted-id".into(),
                disabled: false,
                selected: true,
            }],
            rect: Rect {
                x: 2.5,
                y: 30.0,
                width: 120.0,
                height: 24.0,
            },
        });
        observation.coverage.viewport_complete = false;
        observation.coverage.gaps = vec!["canvas".into(), "visible_text_truncated".into()];
        observation
    }

    #[test]
    fn exported_observation_without_hover_regions_is_unchanged() {
        let mut job = mutation_job();
        job.values.get_mut("name").unwrap().secret = true;
        let redactor = Redactor::for_job(&job).unwrap();

        let exported = redacted_observation(&richly_observed(), &redactor)
            .unwrap()
            .to_string();

        let golden = r#"{"coverage":{"gaps":["canvas","visible_text_truncated"],"open_shadow_roots":true,"same_origin_frames":true,"slots":true,"viewport_complete":false},"covered_text":"Behind {m}","dialog_texts":{"Confirm {m}":"Keep {m}?"},"dialogs":["Confirm {m}"],"elements":[{"checked":null,"disabled":false,"expanded":null,"in_dialog":null,"index":1,"input_type":"text","name":"Name","operations":["TYPE_TEXT"],"rect":{"height":10.0,"width":20.0,"x":1.0,"y":1.0},"role":"textbox","select_options":[],"selected":null,"value":"{m}"},{"checked":false,"disabled":false,"expanded":true,"in_dialog":"Confirm {m}","index":2,"input_type":null,"name":"Account","operations":["SELECT"],"rect":{"height":24.0,"width":120.0,"x":2.5,"y":30.0},"role":"combobox","select_options":[{"disabled":false,"label":"{m}","selected":true,"value":"wanted-id"}],"selected":null,"value":"wanted-id"}],"focus_anchor":null,"focused":1,"route":"/","title":"Money · Accounts","url":"http://127.0.0.1:4351/","viewport":{"document_height":780.0,"height":780,"scroll_x":0.0,"scroll_y":0.0,"width":1120},"visible_text":"Ready {m}"}"#;
        assert_eq!(
            exported,
            golden.replace("{m}", "\u{e000}<masked:1>\u{e000}")
        );
    }

    #[test]
    fn persisted_observation_lists_redacted_hover_regions_without_browser_identity() {
        let mut job = job("Ready");
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
        let mut observation = observed("Ready");
        observation.hover_regions = vec![
            manuvra_chrome::HoverRegion {
                index: 1,
                name: "Groceries".into(),
                reveals_on_hover: vec!["Actions for Groceries".into()],
                node_id: 981_723,
            },
            manuvra_chrome::HoverRegion {
                index: 2,
                name: "Rent".into(),
                reveals_on_hover: vec!["Actions for Rent".into(), "Pin Rent".into()],
                node_id: 981_724,
            },
        ];
        observation.hover_regions_truncated = true;

        let artifacts = driven(
            &job,
            &FakeBrowser::new([observation]),
            &NoProvider,
            &mut MemoryJournal::default(),
        );

        assert!(artifacts.stop.is_none());
        let persisted = &artifacts.observations[0].1;
        let masked = "\u{e000}<masked:1>\u{e000}";
        assert_eq!(
            persisted["hover_regions"],
            json!([
                {"index":1,"name":masked,"reveals_on_hover":[format!("Actions for {masked}")]},
                {"index":2,"name":"Rent","reveals_on_hover":["Actions for Rent","Pin Rent"]}
            ])
        );
        assert_eq!(persisted["hover_regions_truncated"], true);
        assert!(
            persisted["coverage"]["viewport_complete"]
                .as_bool()
                .unwrap()
        );
        let text = persisted.to_string();
        assert!(!text.contains("Groceries"));
        assert!(!text.contains("node_id"));
        assert!(!text.contains("98172"));
        assert!(!redactor.contains_export_leak(text.as_bytes()));
    }

    #[test]
    fn caller_execute_reobserves_done_first_then_dispatches_once_without_operation_reask() {
        let job = force_stop(mutation_job());
        let redactor = Redactor::for_job(&job).unwrap();
        let browser = FakeBrowser::new([text_field(""), text_field(""), text_field("Wanted")])
            .dispatching([typed("Wanted")]);
        let provider = ScriptedProvider::new([Turn::type_text()]);
        let mut journal = MemoryJournal::default();
        let mut machine = HostedMachine::new(&job, &redactor);
        drive(&mut machine, &browser, &provider, &mut journal);
        let candidate = offered(&machine);

        dispose(
            &mut machine,
            execute(&candidate.id),
            &browser,
            &provider,
            &mut journal,
        );
        drive(&mut machine, &browser, &provider, &mut journal);

        assert_eq!(machine.index, 1);
        assert!(machine.artifacts.caller_assisted);
        assert_eq!(
            provider.calls(),
            1,
            "resume execute must not re-ask the operation gate"
        );
        let prepared = journal.prepared();
        assert_eq!(prepared.len(), 1);
        assert_eq!(prepared[0]["basis"], "caller_authority");
    }

    #[test]
    fn retry_observation_preserves_model_budget_and_replay_ledger() {
        let mut job = force_stop(mutation_job());
        job.options.max_model_calls = Some(1);
        let redactor = Redactor::for_job(&job).unwrap();
        let observation = text_field("");
        let browser = FakeBrowser::new([observation.clone()]);
        let provider = ScriptedProvider::new([Turn::type_text()]);
        let mut machine = HostedMachine::new(&job, &redactor);
        let mut journal = MemoryJournal::default();
        drive(&mut machine, &browser, &provider, &mut journal);
        let candidate = offered(&machine);
        let _reserved = machine
            .policy
            .authorize_caller(&job.steps[0], &observation, &candidate)
            .unwrap();

        dispose(&mut machine, retry(), &browser, &NoProvider, &mut journal);

        assert!(machine.artifacts.stop.is_none());
        assert!(matches!(
            machine.policy.record_model_call(),
            Err(policy::PolicyStop::Blocked("budget_exhausted"))
        ));
        assert!(matches!(
            machine
                .policy
                .authorize_caller(&job.steps[0], &observation, &candidate),
            Err(policy::PolicyStop::Uncertain("replay_forbidden"))
        ));
    }

    #[test]
    fn resume_done_judgment_records_the_natural_language_decision() {
        let job = natural(mutation_job(), "The form is complete");
        let redactor = Redactor::for_job(&job).unwrap();
        let mut machine = HostedMachine::new(&job, &redactor);
        let captured = Captured {
            raw: text_field(""),
            artifact: ("o_resume".into(), json!({}), None),
            redaction_verified: true,
        };
        let Ok((done, noul)) = machine.judge_resume_done(
            "The form is complete",
            &captured,
            &ScriptedProvider::new([Turn::type_text()]),
        ) else {
            panic!("natural-language resume judgment should succeed");
        };
        assert_eq!(done, DoneResult::NotSatisfied);
        assert_eq!(noul, Some(0.01));
        assert_eq!(machine.artifacts.decisions.len(), 1);
    }

    #[test]
    fn resumed_unknown_done_reissues_the_condition_specific_reason() {
        for (job, reason) in [
            (
                natural(mutation_job(), "The form is complete"),
                "done_uncertain",
            ),
            (mutation_job(), "done_unknown"),
        ] {
            let redactor = Redactor::for_job(&job).unwrap();
            let mut machine = HostedMachine::new(&job, &redactor);
            let observation = text_field("");
            machine.reissue_unknown_done(
                PendingEscalation {
                    done: DoneResult::NotSatisfied,
                    noul: None,
                    candidate: None,
                    observation: observation.clone(),
                    ambiguous_mutation: false,
                },
                observation,
                Some(0.5),
            );
            assert_eq!(machine.artifacts.stop.as_ref().unwrap().code, reason);
            assert!(
                machine
                    .artifacts
                    .pending
                    .as_ref()
                    .unwrap()
                    .candidate
                    .is_none()
            );
        }
    }

    fn attestable(observation: Observation) -> PendingEscalation {
        PendingEscalation {
            done: DoneResult::Unknown,
            noul: Some(0.5),
            candidate: None,
            observation,
            ambiguous_mutation: false,
        }
    }

    #[test]
    fn advance_requires_natural_uncertainty_without_pending_mutation_and_unchanged_state() {
        let job = natural(job("unused"), "The journey is complete");
        let redactor = Redactor::for_job(&job).unwrap();
        let observation = observed("unchanged");
        let browser = FakeBrowser::new([observation.clone()]);
        let mut machine = HostedMachine::new(&job, &redactor);
        machine.artifacts.pending = Some(attestable(observation.clone()));

        machine.apply_advance("caller verified the condition", &browser);

        assert_eq!(machine.index, 1);
        assert_eq!(
            machine.artifacts.verdicts[0].basis.as_deref(),
            Some("caller_attestation")
        );
        assert_eq!(machine.artifacts.observations.len(), 1);
        let advance_check = machine
            .artifacts
            .trace
            .iter()
            .find(|entry| entry["event"] == "disposition_check")
            .unwrap();
        assert_eq!(advance_check["document_identity_unchanged"], true);
        assert_eq!(advance_check["relevant_state_unchanged"], true);
        assert_eq!(advance_check["numeric_checks_satisfied"], true);
        assert_eq!(
            advance_check["prior_state_hash"],
            advance_check["current_state_hash"]
        );

        let allowed = attestable(observed("unchanged"));
        let mut low_noul = allowed.clone();
        low_noul.noul = Some(0.20);
        assert!(!advance_permitted(&job.steps[0], &low_noul, "attested"));
        let mut not_satisfied = allowed.clone();
        not_satisfied.done = DoneResult::NotSatisfied;
        assert!(!advance_permitted(
            &job.steps[0],
            &not_satisfied,
            "attested"
        ));
        let mut ambiguous = allowed.clone();
        ambiguous.ambiguous_mutation = true;
        assert!(!advance_permitted(&job.steps[0], &ambiguous, "attested"));
        assert!(!advance_permitted(&job.steps[0], &allowed, ""));

        let numeric_job = natural(job.clone(), "The balance is 12.34");
        assert!(!advance_permitted(
            &numeric_job.steps[0],
            &allowed,
            "attested"
        ));
        assert!(
            !allowed_dispositions(&numeric_job.steps[0], &allowed)
                .contains(&DispositionKind::Advance)
        );

        let structured_job = mutation_job();
        let structured_redactor = Redactor::for_job(&structured_job).unwrap();
        let mut structured = HostedMachine::new(&structured_job, &structured_redactor);
        structured.artifacts.pending = Some(PendingEscalation {
            done: DoneResult::NotSatisfied,
            ..attestable(observation)
        });
        structured.apply_advance("not allowed", &browser);
        assert_eq!(structured.index, 0);
        assert_eq!(
            structured.artifacts.stop.as_ref().unwrap().code,
            "advance_not_permitted"
        );
        assert!(!structured.artifacts.caller_assisted);
        assert!(!advance_permitted(
            &structured_job.steps[0],
            &allowed,
            "attested"
        ));
    }

    #[test]
    fn advance_is_refused_when_the_document_or_a_hover_region_changed() {
        let job = natural(job("unused"), "Groceries is archived");
        let redactor = Redactor::for_job(&job).unwrap();
        let attested = plan_before_hover();
        let mut revealing = attested.clone();
        revealing.hover_regions[0].reveals_on_hover = vec!["Unarchive Groceries".into()];
        assert_eq!(revealing.visible_text, attested.visible_text);
        let mut remounted = attested.clone();
        remounted.document_id = "remounted-document".into();
        for (current, identity_unchanged, facts_unchanged) in
            [(revealing, true, false), (remounted, false, true)]
        {
            let mut machine = HostedMachine::new(&job, &redactor);
            machine.artifacts.pending = Some(attestable(attested.clone()));

            machine.apply_advance(
                "caller verified the condition",
                &FakeBrowser::new([current]),
            );

            assert_eq!(machine.index, 0);
            assert_eq!(
                paused(&machine),
                ("relevant_state_changed", RunState::Uncertain, false)
            );
            assert!(!machine.artifacts.caller_assisted);
            assert_eq!(machine.artifacts.observations.len(), 1);
            let check = machine
                .artifacts
                .trace
                .iter()
                .find(|entry| entry["event"] == "disposition_check")
                .unwrap();
            assert_eq!(check["document_identity_unchanged"], identity_unchanged);
            assert_eq!(check["relevant_state_unchanged"], facts_unchanged);
        }
    }

    #[test]
    fn advance_observation_is_guarded_like_every_other_observation() {
        let job = natural(job("unused"), "The journey is complete");
        let redactor = Redactor::for_job(&job).unwrap();
        for (browser, code) in [
            (
                FakeBrowser::new([foreign(observed("unchanged"))]),
                "origin_not_allowed",
            ),
            (
                FakeBrowser::capturing(
                    [Err(BrowserError::Control("redaction_unverifiable".into()))],
                    observed("unchanged"),
                ),
                "redaction_unverifiable",
            ),
            (
                FakeBrowser::capturing(
                    [Err(BrowserError::Control("target closed".into()))],
                    observed("unchanged"),
                ),
                "browser_control_failed",
            ),
        ] {
            let mut machine = HostedMachine::new(&job, &redactor);
            machine.artifacts.pending = Some(attestable(observed("unchanged")));

            machine.apply_advance("caller verified the condition", &browser);

            let stop = machine.artifacts.stop.as_ref().unwrap();
            assert_eq!((stop.code, stop.state), (code, RunState::Blocked));
            assert!(machine.artifacts.escalation.is_none());
            assert_eq!(machine.index, 0);
            assert!(!machine.artifacts.caller_assisted);
        }
    }

    #[test]
    fn relevant_state_without_hover_regions_hashes_as_before_hover_regions_existed() {
        assert_eq!(
            relevant_state_hash(&observed("unchanged")),
            "d6eaad215cf80aaf06c0b26ea36d0d650848ac1cfb5ac198d8283893fcc8ab4d"
        );
    }

    #[test]
    fn natural_done_uncertainty_reobserves_once_and_never_dispatches() {
        let job = natural(
            mutation_job(),
            "El campo Name contiene el valor proporcionado",
        );
        let browser = FakeBrowser::new([text_field("Wanted")]);
        let provider = ScriptedProvider::new([Turn::type_text().noul(0.50)]);

        let artifacts = driven(&job, &browser, &provider, &mut MemoryJournal::default());

        assert_eq!(artifacts.stop.as_ref().unwrap().code, "done_uncertain");
        assert_eq!(artifacts.observations.len(), 2);
        assert_eq!(artifacts.decisions.len(), 2);
        assert_eq!(artifacts.verdicts[0].result, VerdictResult::Unresolved);
        assert_eq!(browser.dispatched(), 0);
        assert_eq!(provider.calls(), 2);
    }

    /// Runs `check` on a step driver over the empty Name field of the mutation job.
    fn with_step_driver(check: impl FnOnce(&mut StepDriver<'_>, &Captured)) -> RunArtifacts {
        let job = mutation_job();
        let redactor = Redactor::for_job(&job).unwrap();
        let captured = Captured {
            raw: text_field(""),
            artifact: ("observation".into(), Value::Null, None),
            redaction_verified: true,
        };
        let browser = FakeBrowser::new([text_field("")]);
        let mut journal = MemoryJournal::default();
        let values = Values::new(&job);
        let cancellation = manuvra_chrome::InputCancellation::default();
        let mut policy = policy::Policy::new(&job.options, "http://127.0.0.1:4351/");
        let mut artifacts = RunArtifacts::new(&job, &redactor);
        let mut driver = StepDriver {
            job: &job,
            redactor: &redactor,
            browser: &browser,
            evaluator: &NoProvider,
            journal: &mut journal,
            values: &values,
            cancellation: &cancellation,
            policy: &mut policy,
            index: 0,
            step: &job.steps[0],
            artifacts: &mut artifacts,
            observation_number: 0,
            done_unknown_reobserved: false,
            operation_gate_reobserved: false,
            mutations: 0,
            awaiting_final_done_reobservation: false,
        };
        check(&mut driver, &captured);
        artifacts
    }

    #[test]
    fn step_driver_escalates_uncertainty_and_ends_the_run_on_every_other_stop() {
        let done = DoneResult::NotSatisfied;
        let typed_answer = mutation_judgments("TYPE_TEXT", 1.0);
        with_step_driver(|driver, captured| {
            for next in [policy::Next::ReobserveOperation, policy::Next::Wait] {
                assert!(matches!(
                    driver.apply_next(done, captured, &typed_answer, next),
                    StepProgress::Continue
                ));
            }
            let blocked = driver.apply_after_reobserve(
                done,
                captured,
                &typed_answer,
                policy::Next::Stop(policy::PolicyStop::Blocked("operation_blocked")),
            );
            assert!(matches!(
                blocked,
                StepProgress::Stop(Stop {
                    code: "operation_blocked",
                    state: RunState::Blocked,
                    ..
                })
            ));
        });

        let artifacts = with_step_driver(|driver, captured| {
            let stop = driver.policy_stop(
                done,
                captured,
                &typed_answer,
                policy::PolicyStop::Uncertain("operation_below_gate"),
            );
            assert_eq!(
                (stop.code, stop.state),
                ("operation_below_gate", RunState::Uncertain)
            );
        });
        assert!(artifacts.escalation.is_some());
        assert!(artifacts.pending.as_ref().unwrap().candidate.is_some());

        with_step_driver(|driver, captured| {
            let stop = driver.policy_stop(
                done,
                captured,
                &typed_answer,
                policy::PolicyStop::Blocked("value_not_provided"),
            );
            assert_eq!(
                (stop.code, stop.state),
                ("value_not_provided", RunState::Blocked)
            );
            assert_eq!(stop.details["observed_field"], "Name");
            assert_eq!(stop.details["known_value_names"], json!(["name"]));
        });

        for (failure, code, state, incomplete) in [
            (
                actions::ActionStop::EvidenceUnavailable,
                "evidence_unavailable",
                RunState::Blocked,
                false,
            ),
            (
                actions::ActionStop::ReadbackMismatch,
                "write_readback_mismatch",
                RunState::Failed,
                false,
            ),
            (
                actions::ActionStop::IncompleteEvidence,
                "evidence_incomplete_after_dispatch",
                RunState::Blocked,
                true,
            ),
            (
                actions::ActionStop::InvalidPermit,
                "candidate_revalidation_failed",
                RunState::Uncertain,
                false,
            ),
            (
                actions::ActionStop::Uncertain,
                "action_outcome_uncertain",
                RunState::Uncertain,
                false,
            ),
        ] {
            let artifacts = with_step_driver(|driver, captured| {
                let stop = driver.action_stop(done, captured, &typed_answer, failure);
                assert_eq!((stop.code, stop.state), (code, state));
            });
            assert_eq!(
                artifacts.escalation.is_some(),
                state == RunState::Uncertain,
                "{code}"
            );
            assert_eq!(artifacts.evidence_incomplete, incomplete, "{code}");
        }
        let unescalated = super::policy_stop(
            policy::PolicyStop::Uncertain("operation_below_gate"),
            &Redactor::for_job(&mutation_job()).unwrap(),
            &mutation_job().steps[0],
        );
        assert_eq!(unescalated.state, RunState::Blocked);
    }

    #[test]
    fn structured_done_passes_or_blocks_when_the_provider_is_unavailable() {
        let passed = driven(
            &job("Ready"),
            &FakeBrowser::new([observed("Ready")]),
            &NoProvider,
            &mut MemoryJournal::default(),
        );
        assert!(passed.stop.is_none());
        assert_eq!(passed.verdicts[0].result, VerdictResult::Satisfied);

        let blocked = driven(
            &job("Ready"),
            &FakeBrowser::new([observed("Not yet")]),
            &NoProvider,
            &mut MemoryJournal::default(),
        );
        assert_eq!(blocked.observations.len(), 1);
        let (state, reason, exit_code, overall) = terminal_fields(blocked.stop);
        assert_eq!(state, RunState::Blocked);
        assert_eq!(reason.unwrap().code, "provider_unavailable");
        assert_eq!(exit_code, 3);
        assert_eq!(overall, VerdictResult::Unresolved);
    }

    #[test]
    fn mutation_limit_waits_for_one_final_reobservation_without_a_second_judgment() {
        let mut job = mutation_job();
        job.steps[0].mutation_limit = 1;
        let browser = FakeBrowser::new([text_field(""), text_field(""), text_field("Wanted")])
            .dispatching([typed("Wanted")]);
        let provider = ScriptedProvider::new([Turn::type_text()]);

        let artifacts = driven(&job, &browser, &provider, &mut MemoryJournal::default());

        assert!(artifacts.stop.is_none());
        assert_eq!(artifacts.observations.len(), 4);
        assert_eq!(artifacts.decisions.len(), 1);
        assert_eq!(provider.calls(), 1);
        assert_eq!(artifacts.verdicts[0].result, VerdictResult::Satisfied);
        assert_eq!(
            trace_events(&artifacts)
                .iter()
                .filter(|event| event.starts_with("action_"))
                .collect::<Vec<_>>(),
            ["action_prepared:TYPE_TEXT", "action_fact:TYPE_TEXT"]
        );
    }

    #[test]
    fn done_unknown_and_low_operation_gate_each_receive_their_own_reobservation() {
        let browser = FakeBrowser::new([
            observed(""),
            text_field(""),
            text_field(""),
            text_field("Wanted"),
        ])
        .dispatching([typed("Wanted")]);
        let provider = ScriptedProvider::new([
            Turn::type_text().confidence(0.69),
            Turn::type_text().confidence(0.95),
        ]);

        let artifacts = driven(
            &mutation_job(),
            &browser,
            &provider,
            &mut MemoryJournal::default(),
        );

        assert!(artifacts.stop.is_none());
        assert_eq!(artifacts.observations.len(), 5);
        assert_eq!(artifacts.decisions.len(), 2);
        assert_eq!(artifacts.verdicts[0].result, VerdictResult::Satisfied);
        assert_eq!(provider.calls(), 2);
        assert_eq!(browser.dispatched(), 1);
    }

    #[test]
    fn unverifiable_masking_and_control_failure_stop_the_step() {
        for (failure, code) in [
            ("redaction_unverifiable", "redaction_unverifiable"),
            ("marker", "browser_control_failed"),
        ] {
            let browser = FakeBrowser::capturing(
                [Err(BrowserError::Control(failure.into()))],
                observed("Ready"),
            );
            let artifacts = driven(
                &job("Ready"),
                &browser,
                &NoProvider,
                &mut MemoryJournal::default(),
            );
            let stop = artifacts.stop.unwrap();
            assert_eq!((stop.code, stop.state), (code, RunState::Blocked));
        }
    }

    #[test]
    fn classified_values_with_json_escapes_are_redacted_before_persistence() {
        let mut job = mutation_job();
        job.values.insert(
            "escaped".into(),
            manuvra_contract::JobValue {
                value: r#"Se"cr\et"#.into(),
                description: "classified value with JSON escapes".into(),
                formats: None,
                secret: true,
            },
        );
        let redactor = Redactor::for_job(&job).unwrap();
        let value = redacted_value(
            &json!({"rationale":r#"typed Se"cr\et"#,"nested":[{r#"Se"cr\et"#:true}]}),
            &redactor,
        );
        let exported = value.to_string();
        assert!(!exported.contains(r#"Se\"cr\\et"#), "{exported}");
        assert!(!redactor.contains_export_leak(exported.as_bytes()));
        assert!(value["rationale"].as_str().unwrap().starts_with("typed "));
        assert!(
            redactor.contains_export_leak(json!({"leaked":r#"Se"cr\et"#}).to_string().as_bytes())
        );
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

    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    #[test]
    fn unsupported_platform_publishes_a_complete_blocked_run() {
        let job = job("Ready");
        let redactor = Redactor::for_job(&job).unwrap();
        let temp = TempDir::new().unwrap();
        let outcome = run(
            &job,
            FlowConfig {
                request_id: "unsupported".into(),
                run_id: "r_unsupported".into(),
                evidence_root: temp.path().to_path_buf(),
                browser: None,
                headless: true,
            },
            &redactor,
        )
        .unwrap();
        assert_eq!(outcome.exit_code, 3);
        assert_eq!(outcome.result["reason"]["code"], "unsupported_platform");
        assert!(temp.path().join("r_unsupported/manifest.json").is_file());
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn hosted_missing_browser_uses_the_production_browser_error_publisher() {
        let job = job("Ready");
        let redactor = Redactor::for_job(&job).unwrap();
        let temp = TempDir::new().unwrap();
        let outcome = run_hosted(
            &job,
            FlowConfig {
                request_id: "hosted-unavailable".into(),
                run_id: "r_hosted_unavailable".into(),
                evidence_root: temp.path().to_path_buf(),
                browser: Some(temp.path().join("missing-browser")),
                headless: true,
            },
            &redactor,
            None,
            &ScriptedControl::default(),
        )
        .unwrap();
        assert_eq!(outcome.exit_code, 3);
        assert_eq!(outcome.result["reason"]["code"], "browser_unavailable");
        assert!(
            temp.path()
                .join("r_hosted_unavailable/manifest.json")
                .is_file()
        );
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn hosted_termination_has_closed_truthful_result_fields() {
        assert_eq!(
            hosted_termination_fields(HostedTermination::Aborted),
            (RunState::Aborted, "caller_aborted", 5)
        );
        assert_eq!(
            hosted_termination_fields(HostedTermination::PauseDeadlineElapsed),
            (RunState::Expired, "resume_deadline_elapsed", 5)
        );
        assert_eq!(
            hosted_termination_fields(HostedTermination::LifetimeElapsed),
            (RunState::Expired, "lifetime_elapsed", 5)
        );
        assert_eq!(
            hosted_termination_fields(HostedTermination::WatchdogLost),
            (RunState::Blocked, "watchdog_lost", 3)
        );
    }
}
