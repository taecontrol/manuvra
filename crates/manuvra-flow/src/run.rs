use crate::evidence::{self, EvidenceBundle, Redactor};
#[cfg(any(target_os = "linux", test))]
use crate::verification::{
    DoneResult, check_done, check_natural_done, natural_numeric_literals_satisfied, verify,
};
#[cfg(any(target_os = "linux", test))]
use crate::{actions, judgment, policy, values::Values};
#[cfg(target_os = "linux")]
use manuvra_chrome::{BrowserConfig, OwnedBrowser};
#[cfg(any(target_os = "linux", test))]
use manuvra_chrome::{BrowserError, CapturedPage, Observation};
#[cfg(any(target_os = "linux", test))]
use manuvra_contract::DoneCondition;
use manuvra_contract::{
    Cleanup, Escalation, EvidenceRef, ExpectationVerdict, Job, Reason, RunResult, RunState,
    SchemaVersion, StepVerdict, Verdict, VerdictResult,
};
#[cfg(any(target_os = "linux", test))]
use manuvra_contract::{Disposition, DispositionKind, DispositionRequest};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
#[cfg(target_os = "linux")]
use std::sync::OnceLock;
#[cfg(any(target_os = "linux", test))]
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

#[cfg(any(target_os = "linux", test))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HostedTermination {
    Aborted,
    PauseDeadlineElapsed,
    LifetimeElapsed,
    WatchdogLost,
}

#[cfg(any(target_os = "linux", test))]
#[derive(Debug, Clone)]
pub enum HostedEvent {
    Disposition(DispositionRequest),
    Termination(HostedTermination),
}

#[cfg(any(target_os = "linux", test))]
pub trait HostedControl {
    fn cancellation(&self) -> manuvra_chrome::InputCancellation;
    fn pause_deadline_unix_ms(&self) -> u64;
    fn publish_checkpoint(&self, result: &Value) -> Result<(), String>;
    fn wait_while_paused(&self, escalation_id: &str) -> HostedEvent;
    fn termination(&self) -> Option<HostedTermination>;
}

pub fn run(job: &Job, config: FlowConfig, redactor: &Redactor) -> Result<FlowOutcome, String> {
    #[cfg(not(target_os = "linux"))]
    {
        publish_without_browser(
            job,
            config,
            redactor,
            "unsupported_platform",
            BTreeMap::new(),
        )
    }
    #[cfg(target_os = "linux")]
    run_linux(job, config, redactor)
}

#[cfg(target_os = "linux")]
pub fn run_hosted(
    job: &Job,
    config: FlowConfig,
    redactor: &Redactor,
    provider_key: Option<String>,
    control: &dyn HostedControl,
) -> Result<FlowOutcome, String> {
    run_linux_with(job, config, redactor, provider_key, Some(control))
}

#[cfg(target_os = "linux")]
fn run_linux(job: &Job, config: FlowConfig, redactor: &Redactor) -> Result<FlowOutcome, String> {
    run_linux_with(job, config, redactor, None, None)
}

#[cfg(target_os = "linux")]
fn run_linux_with(
    job: &Job,
    config: FlowConfig,
    redactor: &Redactor,
    provider_key: Option<String>,
    control: Option<&dyn HostedControl>,
) -> Result<FlowOutcome, String> {
    let started = StartedBrowser::launch(
        browser_config(job, &config, control.is_some()),
        target_url(job),
    )
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

#[cfg(target_os = "linux")]
struct StartedBrowser {
    browser: OwnedBrowser,
    target_url: String,
    provenance: Value,
}

#[cfg(target_os = "linux")]
enum StartupFailure {
    Launch(BrowserError),
    AfterLaunch(Box<AfterLaunchFailure>),
}

#[cfg(target_os = "linux")]
struct AfterLaunchFailure {
    error: BrowserError,
    provenance: Value,
    cleanup: Cleanup,
}

#[cfg(target_os = "linux")]
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

#[cfg(target_os = "linux")]
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

#[cfg(target_os = "linux")]
fn finish_browser_run(
    job: &Job,
    config: FlowConfig,
    redactor: &Redactor,
    mut browser: OwnedBrowser,
    provenance: Value,
    provider_key: Option<String>,
    control: Option<&dyn HostedControl>,
) -> Result<FlowOutcome, String> {
    actions::DurableJournal::open(&config.evidence_root, &config.run_id, redactor).and_then(
        |mut journal| {
            let evaluator = LazyEvaluator::new(provider_key);
            let cancellation = control.map(HostedControl::cancellation).unwrap_or_default();
            if let Some(control) = control {
                return finish_hosted_browser_run(
                    job,
                    config,
                    redactor,
                    &mut browser,
                    provenance,
                    &evaluator,
                    &mut journal,
                    &cancellation,
                    control,
                );
            }
            let mut artifacts = drive_steps(
                job,
                redactor,
                &browser,
                &evaluator,
                &mut journal,
                &cancellation,
                Some(&config),
                None,
            );
            let cleanup = cleanup_browser(&mut browser, redactor, &mut artifacts.stop);
            let (state, reason, exit_code, overall) = terminal_fields(artifacts.stop.clone());
            run_result(
                job,
                &config,
                redactor,
                state,
                reason,
                overall,
                artifacts.verdicts.clone(),
                artifacts.escalation.clone(),
                cleanup.clone(),
            )
            .and_then(|mut result| {
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
            })
            .inspect(|_| {
                let _ = journal.remove();
            })
        },
    )
}

#[cfg(target_os = "linux")]
#[allow(clippy::too_many_arguments)]
fn finish_hosted_browser_run(
    job: &Job,
    config: FlowConfig,
    redactor: &Redactor,
    browser: &mut impl HostedBrowser,
    provenance: Value,
    evaluator: &impl manuvra_jev::Evaluator,
    journal: &mut actions::DurableJournal,
    cancellation: &manuvra_chrome::InputCancellation,
    control: &dyn HostedControl,
) -> Result<FlowOutcome, String> {
    let mut machine = HostedMachine::new(job, redactor);
    loop {
        if !machine.is_paused() {
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
        if machine.is_uncertain() {
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
            )? {
                return Ok(outcome);
            }
            continue;
        }
        return finish_hosted_terminal(
            job, config, redactor, browser, provenance, machine, journal, control,
        );
    }
}

#[cfg(target_os = "linux")]
#[allow(clippy::too_many_arguments)]
fn handle_hosted_pause(
    job: &Job,
    config: &FlowConfig,
    redactor: &Redactor,
    browser: &mut impl HostedBrowser,
    provenance: Value,
    evaluator: &impl manuvra_jev::Evaluator,
    journal: &mut actions::DurableJournal,
    cancellation: &manuvra_chrome::InputCancellation,
    control: &dyn HostedControl,
    machine: &mut HostedMachine<'_>,
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
    let escalation_id = machine.escalation_id()?;
    machine.policy.pause();
    match control.wait_while_paused(&escalation_id) {
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

#[cfg(target_os = "linux")]
#[allow(clippy::too_many_arguments)]
fn finalize_paused_termination(
    job: &Job,
    config: &FlowConfig,
    redactor: &Redactor,
    browser: &mut impl HostedBrowser,
    provenance: Value,
    journal: &mut actions::DurableJournal,
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

#[cfg(target_os = "linux")]
#[allow(clippy::too_many_arguments)]
fn finish_hosted_terminal(
    job: &Job,
    config: FlowConfig,
    redactor: &Redactor,
    browser: &mut impl HostedBrowser,
    provenance: Value,
    machine: HostedMachine<'_>,
    journal: &mut actions::DurableJournal,
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

#[cfg(any(target_os = "linux", test))]
struct HostedMachine<'a> {
    job: &'a Job,
    redactor: &'a Redactor,
    values: Values<'a>,
    policy: policy::Policy,
    index: usize,
    verification_complete: bool,
    artifacts: RunArtifacts,
}

#[cfg(any(target_os = "linux", test))]
impl<'a> HostedMachine<'a> {
    fn new(job: &'a Job, redactor: &'a Redactor) -> Self {
        let mut policy = policy::Policy::new(&job.options, target_url_for_policy(job));
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

    #[cfg(target_os = "linux")]
    fn is_uncertain(&self) -> bool {
        self.artifacts
            .stop
            .as_ref()
            .is_some_and(|stop| stop.state == RunState::Uncertain)
    }

    #[cfg(target_os = "linux")]
    fn is_paused(&self) -> bool {
        self.is_uncertain() && self.artifacts.escalation.is_some()
    }

    #[cfg(target_os = "linux")]
    fn escalation_id(&self) -> Result<String, String> {
        self.artifacts
            .escalation
            .as_ref()
            .map(|value| value.id.clone())
            .ok_or_else(|| "uncertain run has no escalation".to_owned())
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
            self.index += 1;
            if self.index < self.job.steps.len() {
                self.policy.begin_step();
            }
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
                    let _ = execute;
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
            &mut self.artifacts,
        ) {
            Ok(observation) => observation,
            Err(stop) => {
                self.finish_verification_with_stop(stop);
                return;
            }
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
            Err(stop) => {
                self.finish_verification_with_stop(stop);
                return;
            }
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

    fn finish_verification_with_stop(&mut self, stop: Stop) {
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
        let step = &self.job.steps[self.index];
        let permitted = advance_permitted(step, &pending, rationale);
        let prior_hash = relevant_state_hash(&pending.observation);
        let fresh = browser.observe_page().ok();
        let current_hash = fresh.as_ref().map(relevant_state_hash);
        let unchanged = current_hash.as_deref() == Some(prior_hash.as_str());
        self.artifacts.trace.push(json!({
            "event":"disposition_check",
            "kind":"advance",
            "permitted":permitted,
            "relevant_state_unchanged":unchanged,
            "prior_state_hash":prior_hash,
            "current_state_hash":current_hash,
            "numeric_checks_satisfied":natural_condition_numeric_checks_satisfied(step, &pending),
            "pending_ambiguous_mutation":pending.ambiguous_mutation,
            "pending_candidate":pending.candidate.is_some(),
        }));
        if !permitted || !unchanged {
            self.reissue(
                if !unchanged {
                    "relevant_state_changed"
                } else {
                    "advance_not_permitted"
                },
                Some(pending),
            );
            return;
        }
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
        self.index += 1;
        if self.index < self.job.steps.len() {
            self.policy.begin_step();
        }
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
            Err(stop) => {
                self.artifacts.stop = Some(stop);
                return;
            }
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
            self.complete_resumed_step(captured.redaction_verified);
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

    fn resume_observation(
        &mut self,
        browser: &impl DriveBrowser,
        evaluator: &impl manuvra_jev::Evaluator,
    ) -> Result<ResumeObservation, Stop> {
        let captured = capture_step(
            browser,
            self.redactor,
            self.index + 1,
            self.artifacts.observations.len() + 1,
        )
        .map_err(control_stop)?;
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

    fn complete_resumed_step(&mut self, redaction_verified: bool) {
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
            redaction_verified,
            self.policy.active_ms(),
            basis,
        );
        self.index += 1;
        if self.index < self.job.steps.len() {
            self.policy.begin_step();
        }
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
                let mut stale = pending;
                stale.candidate = None;
                stale.observation = captured.raw;
                self.reissue(reason, Some(stale));
                return;
            }
            Err(stop) => {
                self.artifacts.stop = Some(policy_stop(stop, self.redactor, step));
                return;
            }
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
        if let Err(stop) = performed {
            self.record_resumed_action_stop(stop, done, captured.raw);
        }
    }

    fn record_resumed_action_stop(
        &mut self,
        stop: actions::ActionStop,
        done: DoneResult,
        observation: Observation,
    ) {
        let step = &self.job.steps[self.index];
        let mapped = resumed_action_stop(stop, self.redactor, step);
        if mapped.state != RunState::Uncertain {
            self.artifacts.stop = Some(mapped);
            return;
        }
        self.artifacts.pending = Some(PendingEscalation {
            done,
            noul: None,
            candidate: None,
            observation,
            ambiguous_mutation: true,
        });
        self.reissue(mapped.code, self.artifacts.pending.clone());
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

#[cfg(any(target_os = "linux", test))]
struct ResumeObservation {
    captured: Captured,
    done: DoneResult,
    noul: Option<f64>,
}

#[cfg(any(target_os = "linux", test))]
fn resumed_action_stop(
    stop: actions::ActionStop,
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
        actions::ActionStop::IncompleteEvidence => Stop::uncertain(
            "evidence_incomplete_after_dispatch",
            step_detail(redactor, step),
        ),
        _ => Stop::uncertain("action_outcome_uncertain", step_detail(redactor, step)),
    }
}

#[cfg(any(target_os = "linux", test))]
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

#[cfg(any(target_os = "linux", test))]
fn natural_condition_numeric_checks_satisfied(
    step: &manuvra_contract::Step,
    pending: &PendingEscalation,
) -> bool {
    let DoneCondition::NaturalLanguage(condition) = &step.done_when else {
        return false;
    };
    natural_numeric_literals_satisfied(condition, &pending.observation)
}

#[cfg(any(target_os = "linux", test))]
fn relevant_state_hash(observation: &Observation) -> String {
    use sha2::{Digest, Sha256};

    hex::encode(Sha256::digest(relevant_state(observation).to_string()))
}

#[cfg(any(target_os = "linux", test))]
fn relevant_state(observation: &Observation) -> Value {
    json!({
        "url": observation.url,
        "route": observation.route,
        "title": observation.title,
        "dialogs": observation.dialogs,
        "dialog_texts": observation.dialog_texts,
        "focused": observation.focused,
        "visible_text": observation.visible_text,
        "covered_text": observation.covered_text,
        "elements": observation.elements,
        "coverage": observation.coverage,
    })
}

#[cfg(target_os = "linux")]
#[allow(clippy::too_many_arguments)]
fn publish_active_hosted_stop(
    job: &Job,
    config: FlowConfig,
    redactor: &Redactor,
    browser: &mut impl HostedBrowser,
    provenance: Value,
    artifacts: RunArtifacts,
    journal: &mut actions::DurableJournal,
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

#[cfg(target_os = "linux")]
fn publish_terminal_checkpoint(
    control: &dyn HostedControl,
    journal: &mut actions::DurableJournal,
    outcome: FlowOutcome,
) -> Result<FlowOutcome, String> {
    control.publish_checkpoint(&outcome.result).map(|()| {
        let _ = journal.clear();
        outcome
    })
}

#[cfg(target_os = "linux")]
fn set_hosted_escalation_deadline(artifacts: &mut RunArtifacts, deadline: u64) {
    if let Some(escalation) = &mut artifacts.escalation {
        escalation.expires_at = deadline.to_string();
    }
}

#[cfg(target_os = "linux")]
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

#[cfg(target_os = "linux")]
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

#[cfg(target_os = "linux")]
fn browser_config(job: &Job, config: &FlowConfig, hosted: bool) -> BrowserConfig {
    let (width, height) = viewport(job);
    BrowserConfig {
        explicit_binary: config.browser.clone(),
        headless: config.headless,
        width,
        height,
        inherit_process_group: hosted,
    }
}

#[cfg(target_os = "linux")]
fn viewport(job: &Job) -> (u16, u16) {
    job.options
        .viewport
        .as_ref()
        .map_or((1120, 780), |value| (value.width, value.height))
}

#[cfg(target_os = "linux")]
fn target_url(job: &Job) -> &str {
    match &job.target {
        manuvra_contract::Target::Browser { url } => url,
    }
}

#[cfg(target_os = "linux")]
fn cleanup_browser(
    browser: &mut OwnedBrowser,
    redactor: &Redactor,
    stop: &mut Option<Stop>,
) -> Cleanup {
    if let Err(error) = browser.close() {
        *stop = Some(Stop::blocked(
            "cleanup_failed",
            BTreeMap::from([(
                "message".into(),
                json!(redactor.redact_external_text(&error.to_string())),
            )]),
        ));
        return Cleanup {
            browser: "closure_unconfirmed".into(),
            profile: "removal_unconfirmed".into(),
            application_state: "caller_owned".into(),
        };
    }
    Cleanup {
        browser: "closed".into(),
        profile: "removed".into(),
        application_state: "caller_owned".into(),
    }
}

#[cfg(any(target_os = "linux", test))]
fn terminal_fields(stop: Option<Stop>) -> (RunState, Option<Reason>, u8, VerdictResult) {
    stop.map_or(
        (RunState::Passed, None, 0, VerdictResult::Satisfied),
        Stop::fields,
    )
}

#[cfg(any(target_os = "linux", test))]
trait BrowserPage {
    fn capture_redacted_page(&self, sensitive: &[String]) -> Result<CapturedPage, BrowserError>;
    fn observe_page(&self) -> Result<Observation, BrowserError>;
}

#[cfg(any(target_os = "linux", test))]
trait DriveBrowser: BrowserPage + actions::Performer {}
#[cfg(any(target_os = "linux", test))]
impl<T: BrowserPage + actions::Performer> DriveBrowser for T {}

#[cfg(target_os = "linux")]
trait HostedBrowser: DriveBrowser {
    fn cleanup_hosted(&mut self) -> Cleanup;
}

#[cfg(target_os = "linux")]
impl HostedBrowser for OwnedBrowser {
    fn cleanup_hosted(&mut self) -> Cleanup {
        cleanup_started_browser(self)
    }
}

#[cfg(target_os = "linux")]
struct LazyEvaluator {
    client: OnceLock<Result<manuvra_jev::Client, manuvra_jev::JevError>>,
    provider_key: Option<String>,
}

#[cfg(target_os = "linux")]
impl LazyEvaluator {
    fn new(provider_key: Option<String>) -> Self {
        Self {
            client: OnceLock::new(),
            provider_key,
        }
    }
}

#[cfg(target_os = "linux")]
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

#[cfg(target_os = "linux")]
impl BrowserPage for OwnedBrowser {
    fn capture_redacted_page(&self, sensitive: &[String]) -> Result<CapturedPage, BrowserError> {
        self.capture_redacted(sensitive)
    }

    fn observe_page(&self) -> Result<Observation, BrowserError> {
        self.observe()
    }
}

#[cfg(any(target_os = "linux", test))]
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
}

#[cfg(any(target_os = "linux", test))]
#[derive(Clone)]
struct PendingEscalation {
    done: DoneResult,
    noul: Option<f64>,
    candidate: Option<policy::Candidate>,
    observation: Observation,
    ambiguous_mutation: bool,
}

#[cfg(any(target_os = "linux", test))]
#[derive(Clone)]
struct PendingVerification {
    verdicts: Vec<ExpectationVerdict>,
    observation: Observation,
}

#[cfg(any(target_os = "linux", test))]
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

#[cfg(any(target_os = "linux", test))]
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
        }
    }
}

#[cfg(any(target_os = "linux", test))]
#[derive(Clone)]
struct Stop {
    state: RunState,
    code: &'static str,
    exit_code: u8,
    details: BTreeMap<String, Value>,
}

#[cfg(any(target_os = "linux", test))]
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

#[cfg(any(target_os = "linux", test))]
#[allow(clippy::too_many_arguments)]
fn drive_steps(
    job: &Job,
    redactor: &Redactor,
    browser: &impl DriveBrowser,
    evaluator: &impl manuvra_jev::Evaluator,
    journal: &mut impl actions::ActionJournal,
    cancellation: &manuvra_chrome::InputCancellation,
    config: Option<&FlowConfig>,
    control: Option<&dyn HostedControl>,
) -> RunArtifacts {
    let mut artifacts = RunArtifacts::new(job, redactor);
    let mut policy = policy::Policy::new(&job.options, target_url_for_policy(job));
    let values = Values::new(job);
    for (index, step) in job.steps.iter().enumerate() {
        policy.begin_step();
        let outcome = evaluate_step(
            job,
            redactor,
            browser,
            evaluator,
            journal,
            &values,
            cancellation,
            &mut policy,
            index,
            step,
            &mut artifacts,
            0,
            0,
        );
        if let Some(stop) = outcome {
            artifacts.stop = Some(stop);
            break;
        }
        if let (Some(config), Some(control)) = (config, control)
            && let Err(error) =
                publish_active_checkpoint(job, config, redactor, &mut artifacts, index, control)
        {
            artifacts.stop = Some(Stop::blocked(
                "evidence_unavailable",
                BTreeMap::from([(
                    "message".into(),
                    json!(redactor.redact_external_text(&error)),
                )]),
            ));
            break;
        }
    }
    if artifacts.stop.is_none()
        && let VerificationProgress::Stop(stop) = verify_final(
            job,
            redactor,
            browser,
            evaluator,
            &values,
            &mut policy,
            &mut artifacts,
        )
    {
        artifacts.stop = Some(stop);
    }
    artifacts
}

#[cfg(any(target_os = "linux", test))]
enum VerificationProgress {
    Complete,
    Stop(Stop),
}

#[cfg(any(target_os = "linux", test))]
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
    let captured = match capture_final(job, redactor, browser, artifacts) {
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

#[cfg(any(target_os = "linux", test))]
fn capture_final(
    job: &Job,
    redactor: &Redactor,
    browser: &impl DriveBrowser,
    artifacts: &mut RunArtifacts,
) -> Result<Captured, Stop> {
    let captured = capture_step(
        browser,
        redactor,
        job.steps.len() + 1,
        artifacts.observations.len() + 1,
    )
    .map_err(control_stop)?;
    artifacts.observations.push(captured.artifact.clone());
    artifacts.trace.push(json!({
        "event":"final_verification_observation",
        "redaction_verified":captured.redaction_verified,
    }));
    Ok(captured)
}

#[cfg(any(target_os = "linux", test))]
fn capture_verification_advance(
    job: &Job,
    redactor: &Redactor,
    browser: &impl DriveBrowser,
    artifacts: &mut RunArtifacts,
) -> Result<Observation, Stop> {
    let captured = capture_final(job, redactor, browser, artifacts)?;
    if captured.redaction_verified {
        Ok(captured.raw)
    } else {
        Err(Stop::blocked("redaction_unverifiable", BTreeMap::new()))
    }
}

#[cfg(any(target_os = "linux", test))]
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

#[cfg(any(target_os = "linux", test))]
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

#[cfg(any(target_os = "linux", test))]
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

#[cfg(any(target_os = "linux", test))]
fn verification_policy_stop(stop: policy::PolicyStop) -> Stop {
    match stop {
        policy::PolicyStop::Blocked(code) => Stop::blocked(code, BTreeMap::new()),
        policy::PolicyStop::Uncertain(code) => Stop::uncertain(code, BTreeMap::new()),
        policy::PolicyStop::UnsupportedSurface(surface) => Stop::blocked(
            "unsupported_surface",
            BTreeMap::from([("surface".into(), json!(surface))]),
        ),
    }
}

#[cfg(any(target_os = "linux", test))]
fn verification_provider_stop(error: &manuvra_jev::JevError) -> Stop {
    match error {
        manuvra_jev::JevError::InvalidResponse(_) | manuvra_jev::JevError::ModelChanged => {
            Stop::blocked("provider_invalid_response", BTreeMap::new())
        }
        manuvra_jev::JevError::Deadline => Stop::blocked("budget_exhausted", BTreeMap::new()),
        _ => Stop::blocked("provider_unavailable", BTreeMap::new()),
    }
}

#[cfg(any(target_os = "linux", test))]
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

#[cfg(any(target_os = "linux", test))]
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

#[cfg(any(target_os = "linux", test))]
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

#[cfg(any(target_os = "linux", test))]
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
        if self.cancellation.is_cancelled() {
            return StepProgress::Stop(Stop::uncertain(
                "action_outcome_uncertain",
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
            policy::Next::Mutate(permit) if self.force_stop_before_first_mutation() => {
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

    fn force_stop_before_first_mutation(&self) -> bool {
        self.mutations == 0
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
                if !matches!(
                    fact.operation,
                    judgment::Operation::ScrollUp | judgment::Operation::ScrollDown
                ) {
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
            actions::ActionStop::Uncertain => escalate(
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
                    ambiguous_mutation: true,
                },
                "action_outcome_uncertain",
            ),
            actions::ActionStop::Reobserve(_) => {
                unreachable!("not-performed actions reobserve before stop mapping")
            }
            other => self.simple_action_stop(other),
        }
    }

    fn simple_action_stop(&self, stop: actions::ActionStop) -> Stop {
        match stop {
            actions::ActionStop::EvidenceUnavailable => Stop::blocked(
                "evidence_unavailable",
                step_detail(self.redactor, self.step),
            ),
            other => self.post_dispatch_action_stop(other),
        }
    }

    fn post_dispatch_action_stop(&self, stop: actions::ActionStop) -> Stop {
        match stop {
            actions::ActionStop::ReadbackMismatch => Stop::failed(
                "write_readback_mismatch",
                step_detail(self.redactor, self.step),
            ),
            other => self.incomplete_action_stop(other),
        }
    }

    fn incomplete_action_stop(&self, stop: actions::ActionStop) -> Stop {
        if stop == actions::ActionStop::IncompleteEvidence {
            Stop::uncertain(
                "evidence_incomplete_after_dispatch",
                step_detail(self.redactor, self.step),
            )
        } else {
            debug_assert!(matches!(stop, actions::ActionStop::InvalidPermit));
            Stop::uncertain(
                "candidate_revalidation_failed",
                step_detail(self.redactor, self.step),
            )
        }
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
                let candidate = (reason == "operation_below_gate")
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

#[cfg(any(target_os = "linux", test))]
enum StepProgress {
    Continue,
    Complete,
    Stop(Stop),
}

#[cfg(any(target_os = "linux", test))]
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

#[cfg(any(target_os = "linux", test))]
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

#[cfg(any(target_os = "linux", test))]
fn target_url_for_policy(job: &Job) -> &str {
    match &job.target {
        manuvra_contract::Target::Browser { url } => url,
    }
}

#[cfg(any(target_os = "linux", test))]
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

#[cfg(any(target_os = "linux", test))]
fn step_detail(redactor: &Redactor, step: &manuvra_contract::Step) -> BTreeMap<String, Value> {
    BTreeMap::from([(
        "step_id".into(),
        json!(redactor.redact_export_text(&step.id)),
    )])
}

#[cfg(any(target_os = "linux", test))]
fn policy_stop(
    stop: policy::PolicyStop,
    redactor: &Redactor,
    step: &manuvra_contract::Step,
) -> Stop {
    match stop {
        policy::PolicyStop::Blocked(code) => Stop::blocked(code, step_detail(redactor, step)),
        policy::PolicyStop::Uncertain(code) => Stop::uncertain(code, step_detail(redactor, step)),
        policy::PolicyStop::UnsupportedSurface(surface) => {
            let mut details = step_detail(redactor, step);
            details.insert("surface".into(), json!(surface));
            Stop::blocked("unsupported_surface", details)
        }
    }
}

#[cfg(any(target_os = "linux", test))]
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
        || judgments.map(|value|json!({"operation":value.operation,"click_target":value.click_target,"type_target":value.type_target,"select_target":value.select_target,"type_value":value.type_value})).unwrap_or_else(||json!({})),
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
        &json!({"id":id,"phase":"step","step_id":step.id,"step":{"goal":step.goal,"done_when":step.done_when},"done":done,"observation":latest,"decision":decision,"gate_reason":reason,"candidates":candidates,"offered_candidate":offered_candidate,"permitted_mutations":["CLICK","TYPE_TEXT"],"recent_actions":artifacts.trace.iter().rev().filter(|event|event.get("event").and_then(Value::as_str).is_some_and(|event|event.starts_with("action_"))).take(8).collect::<Vec<_>>(),"stopped_at":stopped}),
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

#[cfg(any(target_os = "linux", test))]
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

#[cfg(any(target_os = "linux", test))]
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

#[cfg(any(target_os = "linux", test))]
fn natural_noul(step: &manuvra_contract::Step, judgments: &judgment::Judgments) -> Option<f64> {
    matches!(step.done_when, DoneCondition::NaturalLanguage(_)).then_some(judgments.step_done)
}

#[cfg(any(target_os = "linux", test))]
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

#[cfg(any(target_os = "linux", test))]
fn empty_observation() -> Observation {
    Observation {
        document_id: String::new(),
        url: String::new(),
        route: String::new(),
        title: String::new(),
        dialogs: Vec::new(),
        focused: None,
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
    }
}

#[cfg(any(target_os = "linux", test))]
fn redacted_value(value: &impl serde::Serialize, redactor: &Redactor) -> Value {
    let text = serde_json::to_string(value).unwrap_or_else(|_| "null".into());
    serde_json::from_str(&redactor.redact_export_text(&text)).unwrap_or(Value::Null)
}

#[cfg(any(target_os = "linux", test))]
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

#[cfg(any(target_os = "linux", test))]
fn control_stop(message: String) -> Stop {
    Stop::blocked(
        "browser_control_failed",
        BTreeMap::from([("message".into(), json!(message))]),
    )
}

#[cfg(any(target_os = "linux", test))]
struct Captured {
    raw: Observation,
    artifact: (String, Value, Option<Vec<u8>>),
    redaction_verified: bool,
}

#[cfg(any(target_os = "linux", test))]
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

#[cfg(any(target_os = "linux", test))]
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

#[cfg(any(target_os = "linux", test))]
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
    Ok(json!({
        "url":redact(&raw.url),
        "route":redact(&raw.route),
        "title":redact(&raw.title),
        "dialogs":raw.dialogs.iter().map(|dialog|redact(dialog)).collect::<Vec<_>>(),
        "focused":raw.focused,
        "visible_text":redact(&raw.visible_text),
        "covered_text":redact(&raw.covered_text),
        "dialog_texts":raw.dialog_texts.iter().map(|(name,text)|(redact(name),redact(text))).collect::<BTreeMap<_,_>>(),
        "elements":elements,
        "viewport":raw.viewport,
        "coverage":raw.coverage,
    }))
}

#[cfg(target_os = "linux")]
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

#[cfg(target_os = "linux")]
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

#[cfg(not(target_os = "linux"))]
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
    let evidence_complete = reason
        .as_ref()
        .is_none_or(|reason| reason.code != "evidence_incomplete_after_dispatch");
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
            complete: evidence_complete,
        },
        escalation,
        cleanup,
    };
    serde_json::to_value(result).map_err(|e| e.to_string())
}

#[cfg(target_os = "linux")]
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

#[cfg(any(target_os = "linux", test))]
fn apply_artifact_verdict(result: &mut Value, artifacts: &RunArtifacts) -> Result<(), String> {
    result["verdict"]["expectations"] =
        serde_json::to_value(&artifacts.expectation_verdicts).unwrap_or(Value::Null);
    result["verdict"]["caller_assisted"] = json!(artifacts.caller_assisted);
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
    if passed
        && (!complete
            || artifacts.verification.is_none()
            || !steps_satisfied
            || !expectations_satisfied)
    {
        return Err("passed result requires complete satisfied verification evidence".into());
    }
    Ok(())
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

#[cfg(any(target_os = "linux", test))]
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
    use manuvra_chrome::{Coverage, Element, Rect, RedactionProof, Screenshot, ViewportState};
    use serde_json::json;
    use std::collections::{BTreeMap, VecDeque};
    #[cfg(target_os = "linux")]
    use std::io::{Read, Write};
    #[cfg(target_os = "linux")]
    use std::net::{TcpListener, TcpStream};
    #[cfg(target_os = "linux")]
    use std::sync::Arc;
    use std::sync::Mutex;
    #[cfg(target_os = "linux")]
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    #[cfg(target_os = "linux")]
    use std::thread;
    use tempfile::TempDir;

    #[cfg(target_os = "linux")]
    struct OriginFixture {
        start_port: u16,
        foreign_port: u16,
        stop: Arc<AtomicBool>,
        workers: Vec<thread::JoinHandle<()>>,
    }

    #[cfg(target_os = "linux")]
    impl OriginFixture {
        fn start() -> Self {
            let start = TcpListener::bind("127.0.0.1:0").unwrap();
            let foreign = TcpListener::bind("127.0.0.1:0").unwrap();
            start.set_nonblocking(true).unwrap();
            foreign.set_nonblocking(true).unwrap();
            let start_port = start.local_addr().unwrap().port();
            let foreign_port = foreign.local_addr().unwrap().port();
            let stop = Arc::new(AtomicBool::new(false));
            let worker = |listener: TcpListener, body: String, stop: Arc<AtomicBool>| {
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
            };
            let start_body = format!(
                "<!doctype html><title>Origin guard</title><a href=\"http://127.0.0.1:{foreign_port}/landing\">Leave origin</a>"
            );
            let foreign_body =
                "<!doctype html><title>Foreign origin</title><p>Committed foreign origin</p>"
                    .to_owned();
            let workers = vec![
                worker(start, start_body, Arc::clone(&stop)),
                worker(foreign, foreign_body, Arc::clone(&stop)),
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

    #[cfg(target_os = "linux")]
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

    #[cfg(target_os = "linux")]
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

    struct FakeBrowser {
        captures: Mutex<VecDeque<Result<CapturedPage, BrowserError>>>,
        fallback: Observation,
        dispatch_result: Option<Result<manuvra_chrome::PerformFact, manuvra_chrome::PerformError>>,
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
                .unwrap_or_else(|| observation_page(self.fallback.clone()))
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
            _input: manuvra_chrome::PreparedInput,
            _cancellation: &manuvra_chrome::InputCancellation,
        ) -> Result<manuvra_chrome::PerformFact, manuvra_chrome::PerformError> {
            self.dispatch_result.clone().unwrap_or_else(|| {
                Err(manuvra_chrome::PerformError::NotPerformed(
                    "fake browser".into(),
                ))
            })
        }
    }

    #[cfg(target_os = "linux")]
    impl HostedBrowser for FakeBrowser {
        fn cleanup_hosted(&mut self) -> Cleanup {
            Cleanup {
                browser: "closed".into(),
                profile: "removed".into(),
                application_state: "caller_owned".into(),
            }
        }
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

    struct TypeTextProvider(std::sync::atomic::AtomicUsize);

    impl manuvra_jev::Evaluator for TypeTextProvider {
        fn evaluate(
            &self,
            _request: &Value,
            _deadline: Instant,
        ) -> Result<manuvra_jev::Evaluation, manuvra_jev::JevError> {
            self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let choice = |selected: &str| manuvra_jev::Answer::Choice {
                choice: selected.into(),
                probabilities: BTreeMap::from([(selected.into(), 1.0)]),
                confidence: 1.0,
            };
            Ok(manuvra_jev::Evaluation {
                answers: BTreeMap::from([
                    ("operation".into(), choice("TYPE_TEXT")),
                    ("click_target".into(), choice("NO_CLICK_TARGET")),
                    ("type_target".into(), choice("1")),
                    ("select_target".into(), choice("NO_SELECT_TARGET")),
                    ("type_value".into(), choice("name")),
                    ("step_done".into(), manuvra_jev::Answer::Noul { noul: 0.01 }),
                ]),
                usage: BTreeMap::new(),
                request_id: Some("recorded-fake".into()),
                model: "jev-1.13.0".into(),
            })
        }
    }

    #[cfg(target_os = "linux")]
    struct ClickProvider(AtomicUsize);

    #[cfg(target_os = "linux")]
    impl manuvra_jev::Evaluator for ClickProvider {
        fn evaluate(
            &self,
            request: &Value,
            _deadline: Instant,
        ) -> Result<manuvra_jev::Evaluation, manuvra_jev::JevError> {
            self.0.fetch_add(1, Ordering::SeqCst);
            let first = |question: &str| {
                request["questions"][question]["criteria"]
                    .as_object()
                    .unwrap()
                    .keys()
                    .next()
                    .unwrap()
                    .clone()
            };
            let choice = |selected: String| manuvra_jev::Answer::Choice {
                probabilities: BTreeMap::from([(selected.clone(), 1.0)]),
                choice: selected,
                confidence: 1.0,
            };
            Ok(manuvra_jev::Evaluation {
                answers: BTreeMap::from([
                    ("operation".into(), choice("CLICK".into())),
                    ("click_target".into(), choice(first("click_target"))),
                    ("type_target".into(), choice(first("type_target"))),
                    ("select_target".into(), choice(first("select_target"))),
                    ("type_value".into(), choice(first("type_value"))),
                    ("step_done".into(), manuvra_jev::Answer::Noul { noul: 0.01 }),
                ]),
                usage: BTreeMap::new(),
                request_id: Some("origin-guard-fixture".into()),
                model: "jev-1.13.0".into(),
            })
        }
    }

    struct ConfidenceSequenceProvider(Mutex<VecDeque<f64>>);

    impl manuvra_jev::Evaluator for ConfidenceSequenceProvider {
        fn evaluate(
            &self,
            _request: &Value,
            _deadline: Instant,
        ) -> Result<manuvra_jev::Evaluation, manuvra_jev::JevError> {
            let confidence = self.0.lock().unwrap().pop_front().expect("confidence");
            let choice = |selected: &str, confidence: f64| manuvra_jev::Answer::Choice {
                choice: selected.into(),
                probabilities: BTreeMap::from([(selected.into(), 1.0)]),
                confidence,
            };
            Ok(manuvra_jev::Evaluation {
                answers: BTreeMap::from([
                    ("operation".into(), choice("TYPE_TEXT", confidence)),
                    ("click_target".into(), choice("NO_CLICK_TARGET", 1.0)),
                    ("type_target".into(), choice("1", 1.0)),
                    ("select_target".into(), choice("NO_SELECT_TARGET", 1.0)),
                    ("type_value".into(), choice("name", 1.0)),
                    ("step_done".into(), manuvra_jev::Answer::Noul { noul: 0.01 }),
                ]),
                usage: BTreeMap::new(),
                request_id: Some("recorded-sequence".into()),
                model: "jev-1.13.0".into(),
            })
        }
    }

    struct NaturalDoneSequenceProvider(Mutex<VecDeque<f64>>);

    impl manuvra_jev::Evaluator for NaturalDoneSequenceProvider {
        fn evaluate(
            &self,
            _request: &Value,
            _deadline: Instant,
        ) -> Result<manuvra_jev::Evaluation, manuvra_jev::JevError> {
            let noul = self.0.lock().unwrap().pop_front().expect("done Noul");
            let choice = |selected: &str| manuvra_jev::Answer::Choice {
                choice: selected.into(),
                probabilities: BTreeMap::from([(selected.into(), 1.0)]),
                confidence: 0.95,
            };
            Ok(manuvra_jev::Evaluation {
                answers: BTreeMap::from([
                    ("operation".into(), choice("TYPE_TEXT")),
                    ("click_target".into(), choice("NO_CLICK_TARGET")),
                    ("type_target".into(), choice("1")),
                    ("select_target".into(), choice("NO_SELECT_TARGET")),
                    ("type_value".into(), choice("name")),
                    ("step_done".into(), manuvra_jev::Answer::Noul { noul }),
                ]),
                usage: BTreeMap::new(),
                request_id: Some("recorded-natural-sequence".into()),
                model: "jev-1.13.0".into(),
            })
        }
    }

    struct ExpectationSequenceProvider {
        nouls: Mutex<VecDeque<f64>>,
        requests: Mutex<Vec<Value>>,
    }

    impl manuvra_jev::Evaluator for ExpectationSequenceProvider {
        fn evaluate(
            &self,
            request: &Value,
            _deadline: Instant,
        ) -> Result<manuvra_jev::Evaluation, manuvra_jev::JevError> {
            self.requests.lock().unwrap().push(request.clone());
            let noul = self.nouls.lock().unwrap().pop_front().expect("Noul");
            let answers = request["questions"]
                .as_object()
                .unwrap()
                .keys()
                .map(|id| (id.clone(), manuvra_jev::Answer::Noul { noul }))
                .collect();
            Ok(manuvra_jev::Evaluation {
                answers,
                usage: BTreeMap::new(),
                request_id: Some("expectation-sequence".into()),
                model: "jev-1.13.0".into(),
            })
        }
    }

    #[derive(Default)]
    struct MemoryJournal(Vec<Value>);
    impl actions::ActionJournal for MemoryJournal {
        fn append(&mut self, value: &Value) -> Result<(), String> {
            self.0.push(value.clone());
            Ok(())
        }
        fn entries(&self) -> &[Value] {
            &self.0
        }
    }

    #[derive(Default)]
    struct RecordingHostedControl(Mutex<Vec<Value>>);

    impl HostedControl for RecordingHostedControl {
        fn cancellation(&self) -> manuvra_chrome::InputCancellation {
            manuvra_chrome::InputCancellation::default()
        }

        fn pause_deadline_unix_ms(&self) -> u64 {
            u64::MAX
        }

        fn publish_checkpoint(&self, result: &Value) -> Result<(), String> {
            self.0.lock().unwrap().push(result.clone());
            Ok(())
        }

        fn wait_while_paused(&self, _: &str) -> HostedEvent {
            HostedEvent::Termination(HostedTermination::Aborted)
        }

        fn termination(&self) -> Option<HostedTermination> {
            None
        }
    }

    #[cfg(target_os = "linux")]
    struct DispositionHostedControl {
        checkpoints: Mutex<Vec<Value>>,
        request: Mutex<Option<DispositionRequest>>,
    }

    #[cfg(target_os = "linux")]
    impl HostedControl for DispositionHostedControl {
        fn cancellation(&self) -> manuvra_chrome::InputCancellation {
            manuvra_chrome::InputCancellation::default()
        }
        fn pause_deadline_unix_ms(&self) -> u64 {
            u64::MAX
        }
        fn publish_checkpoint(&self, result: &Value) -> Result<(), String> {
            self.checkpoints.lock().unwrap().push(result.clone());
            Ok(())
        }
        fn wait_while_paused(&self, _: &str) -> HostedEvent {
            HostedEvent::Disposition(self.request.lock().unwrap().take().unwrap())
        }
        fn termination(&self) -> Option<HostedTermination> {
            None
        }
    }

    fn drive_fake(job: &Job, redactor: &Redactor, browser: &FakeBrowser) -> RunArtifacts {
        drive_steps(
            job,
            redactor,
            browser,
            &NoProvider,
            &mut MemoryJournal::default(),
            &manuvra_chrome::InputCancellation::default(),
            None,
            None,
        )
    }

    #[cfg(target_os = "linux")]
    #[test]
    #[ignore = "requires the local Chromium executable"]
    fn production_driver_stops_after_committed_navigation_to_a_foreign_origin() {
        let fixture = OriginFixture::start();
        let start_url = fixture.start_url();
        let job = Job::parse(
            serde_json::to_vec(&json!({
                "schema_version":1,
                "target":{"kind":"browser","url":start_url},
                "context":{"journey":"origin guard","revision":"fixture","environment":"local Chromium","actor":"synthetic","authority":"navigate only"},
                "steps":[{"id":"leave","goal":"Open Leave origin.","done_when":[{"text_visible":"Never present"}]}]
            }))
            .unwrap()
            .as_slice(),
        )
        .unwrap();
        let redactor = Redactor::for_job(&job).unwrap();
        let mut browser = OwnedBrowser::launch(BrowserConfig {
            explicit_binary: None,
            headless: true,
            width: 1120,
            height: 780,
            inherit_process_group: false,
        })
        .unwrap();
        browser.navigate(&start_url).unwrap();
        let provider = ClickProvider(AtomicUsize::new(0));
        let mut journal = MemoryJournal::default();

        let artifacts = drive_steps(
            &job,
            &redactor,
            &browser,
            &provider,
            &mut journal,
            &manuvra_chrome::InputCancellation::default(),
            None,
            None,
        );

        let stop = artifacts.stop.expect("foreign origin must stop the run");
        assert_eq!(stop.state, RunState::Blocked);
        assert_eq!(
            stop.code, "origin_not_allowed",
            "unexpected stop details: {:?}",
            stop.details
        );
        assert_eq!(provider.0.load(Ordering::SeqCst), 1);
        assert_eq!(journal.0.len(), 2, "only one prepared mutation may run");
        assert_eq!(journal.0[0]["event"], "action_prepared");
        assert_eq!(journal.0[1]["event"], "action_fact");
        assert_eq!(journal.0[1]["fact"]["outcome"], "observed");
        let committed = browser.observe().unwrap();
        assert!(committed.url.starts_with(&fixture.foreign_origin()));
        browser.close().unwrap();
    }

    #[test]
    fn completed_steps_publish_running_checkpoints_with_the_next_step_unresolved() {
        let job = Job::parse(
            serde_json::to_vec(&json!({
                "schema_version":1,
                "target":{"kind":"browser","url":"http://example.test/ready"},
                "context":{"journey":"checkpoint","revision":"r","environment":"e","actor":"a","authority":"a"},
                "values":{},
                "steps":[
                    {"id":"first","goal":"observe first","done_when":[{"url_contains":"/ready"}]},
                    {"id":"second","goal":"observe second","done_when":[{"url_contains":"/ready"}]}
                ]
            }))
            .unwrap()
            .as_slice(),
        )
        .unwrap();
        let redactor = Redactor::for_job(&job).unwrap();
        let observation = Observation {
            url: "http://example.test/ready".into(),
            route: "/ready".into(),
            ..observed("ready")
        };
        let browser = FakeBrowser {
            captures: Mutex::new(VecDeque::from([
                observation_page(observation.clone()),
                observation_page(observation.clone()),
            ])),
            fallback: observation,
            dispatch_result: None,
        };
        let temporary = tempfile::tempdir().unwrap();
        let config = FlowConfig {
            request_id: "checkpoint-request".into(),
            run_id: "r_checkpoint".into(),
            evidence_root: temporary.path().to_path_buf(),
            browser: None,
            headless: true,
        };
        let control = RecordingHostedControl::default();
        let artifacts = drive_steps(
            &job,
            &redactor,
            &browser,
            &NoProvider,
            &mut MemoryJournal::default(),
            &manuvra_chrome::InputCancellation::default(),
            Some(&config),
            Some(&control),
        );
        assert!(artifacts.stop.is_none());
        let checkpoints = control.0.lock().unwrap();
        assert_eq!(checkpoints.len(), 2);
        assert_eq!(checkpoints[0]["verdict"]["steps"][0]["result"], "satisfied");
        assert_eq!(
            checkpoints[0]["verdict"]["steps"][1]["result"],
            "unresolved"
        );
        assert_eq!(checkpoints[0]["evidence"]["complete"], false);
    }

    fn observed(text: &str) -> Observation {
        Observation {
            document_id: "recorded-money-document".into(),
            url: "http://127.0.0.1:4351/".into(),
            route: "/".into(),
            title: "Money · Accounts".into(),
            dialogs: Vec::new(),
            focused: None,
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
        }
    }

    fn page(text: &str) -> Result<CapturedPage, BrowserError> {
        Ok(CapturedPage {
            observation: observed(text),
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

    fn observation_page(observation: Observation) -> Result<CapturedPage, BrowserError> {
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

    fn job(wanted: &str) -> Job {
        Job::parse(serde_json::to_string(&json!({"schema_version":1,"target":{"kind":"browser","url":"http://127.0.0.1:4351/"},"context":{"journey":"observe","revision":"fixture","environment":"fake","actor":"synthetic","authority":"observe"},"steps":[{"id":"ready","goal":"observe","done_when":[{"text_visible":wanted}]}]})).unwrap().as_bytes()).unwrap()
    }

    fn mutation_job() -> Job {
        Job::parse(serde_json::to_vec(&json!({
            "schema_version":1,
            "target":{"kind":"browser","url":"http://127.0.0.1:4351/"},
            "context":{"journey":"mutate","revision":"fixture","environment":"fake","actor":"synthetic","authority":"mutate"},
            "values":{"name":{"value":"Wanted","description":"account name"}},
            "steps":[{"id":"fill","goal":"fill the account name","done_when":[{"field":"Name","equals_value":"name"}],"mutation_limit":2}]
        })).unwrap().as_slice()).unwrap()
    }

    fn expectation_job() -> Job {
        Job::parse(
            serde_json::to_vec(&json!({
                "schema_version":1,
                "target":{"kind":"browser","url":"http://127.0.0.1:4351/"},
                "context":{"journey":"verify","revision":"fixture","environment":"fake","actor":"synthetic","authority":"observe"},
                "steps":[{"id":"ready","goal":"observe","done_when":[{"text_visible":"Ready"}]}],
                "expectations":[{"id":"balance","claim":"The final balance is 12.34."}]
            }))
            .unwrap()
            .as_slice(),
        )
        .unwrap()
    }

    fn text_field(value: &str) -> Observation {
        let mut observation = observed("");
        observation.elements.push(Element {
            index: 1,
            node_id: 7,
            context: "main".into(),
            role: "textbox".into(),
            name: "Name".into(),
            input_type: Some("text".into()),
            value: value.into(),
            checked: None,
            selected: None,
            expanded: None,
            disabled: false,
            in_dialog: None,
            operations: vec!["TYPE_TEXT".into()],
            select_options: vec![],
            rect: Rect {
                x: 1.0,
                y: 1.0,
                width: 20.0,
                height: 10.0,
            },
        });
        observation
    }

    #[test]
    fn final_verification_uses_a_fresh_observation_and_fails_missing_literals() {
        let job = expectation_job();
        let redactor = Redactor::for_job(&job).unwrap();
        let browser = FakeBrowser {
            captures: Mutex::new(VecDeque::from([
                page("Ready"),
                page("Ready balance 112.34"),
            ])),
            fallback: observed("Ready balance 112.34"),
            dispatch_result: None,
        };
        let provider = ExpectationSequenceProvider {
            nouls: Mutex::new(VecDeque::from([0.95])),
            requests: Mutex::new(Vec::new()),
        };
        let artifacts = drive_steps(
            &job,
            &redactor,
            &browser,
            &provider,
            &mut MemoryJournal::default(),
            &manuvra_chrome::InputCancellation::default(),
            None,
            None,
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
    fn verification_uncertainty_allows_attestation_but_refuses_execute() {
        let job = expectation_job();
        let redactor = Redactor::for_job(&job).unwrap();
        let final_page = observed("Ready balance 12.34");
        let browser = FakeBrowser {
            captures: Mutex::new(VecDeque::from([
                page("Ready"),
                observation_page(final_page.clone()),
            ])),
            fallback: final_page,
            dispatch_result: None,
        };
        let provider = ExpectationSequenceProvider {
            nouls: Mutex::new(VecDeque::from([0.50])),
            requests: Mutex::new(Vec::new()),
        };
        let control = RecordingHostedControl::default();
        let cancellation = manuvra_chrome::InputCancellation::default();
        let mut journal = MemoryJournal::default();
        let mut machine = HostedMachine::new(&job, &redactor);
        machine.drive(
            &browser,
            &provider,
            &mut journal,
            &cancellation,
            None,
            &control,
        );
        let escalation = machine.artifacts.escalation.clone().unwrap();
        assert_eq!(escalation.phase, "verification");
        assert_eq!(escalation.step_id, None);
        assert_eq!(
            escalation.dispositions,
            [
                DispositionKind::Advance,
                DispositionKind::RetryObservation,
                DispositionKind::Abort,
            ]
        );
        machine.apply(
            DispositionRequest {
                schema_version: SchemaVersion,
                escalation_id: escalation.id.clone(),
                disposition: Disposition::Execute(manuvra_contract::ExecuteDisposition {
                    kind: manuvra_contract::ExecuteKind::Execute,
                    candidate_id: "not-offered".into(),
                }),
            },
            &browser,
            &provider,
            &mut journal,
            &cancellation,
        );
        assert_eq!(
            machine.artifacts.stop.as_ref().unwrap().code,
            "execute_not_permitted"
        );
        assert!(!machine.verification_complete);
        let escalation_id = machine.artifacts.escalation.as_ref().unwrap().id.clone();
        machine.apply(
            DispositionRequest {
                schema_version: SchemaVersion,
                escalation_id,
                disposition: Disposition::Advance(manuvra_contract::AdvanceDisposition {
                    kind: manuvra_contract::AdvanceKind::Advance,
                    rationale: "Observed the final account facts directly.".into(),
                }),
            },
            &browser,
            &provider,
            &mut journal,
            &cancellation,
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
    fn verification_attestation_rechecks_changed_facts_and_numeric_scopes() {
        let mut job = expectation_job();
        job.expectations[0].exact_literals = vec![manuvra_contract::ExactLiteral {
            literal: "12.34".into(),
            within_text: Some("Wallet".into()),
        }];
        let redactor = Redactor::for_job(&job).unwrap();
        let initial = observed("Ready\nWallet balance 12.34");
        let changed = observed("Ready\nWallet balance 12.34\nWallet reserve 12.34");
        let browser = FakeBrowser {
            captures: Mutex::new(VecDeque::from([
                page("Ready"),
                observation_page(initial),
                observation_page(changed),
            ])),
            fallback: observed("Ready\nWallet balance 12.34\nWallet reserve 12.34"),
            dispatch_result: None,
        };
        let provider = ExpectationSequenceProvider {
            nouls: Mutex::new(VecDeque::from([0.50, 0.50])),
            requests: Mutex::new(Vec::new()),
        };
        let control = RecordingHostedControl::default();
        let cancellation = manuvra_chrome::InputCancellation::default();
        let mut journal = MemoryJournal::default();
        let mut machine = HostedMachine::new(&job, &redactor);
        machine.drive(
            &browser,
            &provider,
            &mut journal,
            &cancellation,
            None,
            &control,
        );
        let escalation = machine.artifacts.escalation.clone().unwrap();
        machine.apply(
            DispositionRequest {
                schema_version: SchemaVersion,
                escalation_id: escalation.id,
                disposition: Disposition::Advance(manuvra_contract::AdvanceDisposition {
                    kind: manuvra_contract::AdvanceKind::Advance,
                    rationale: "Caller attests the prior final facts.".into(),
                }),
            },
            &browser,
            &provider,
            &mut journal,
            &cancellation,
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
        assert_eq!(provider.requests.lock().unwrap().len(), 2);
        let check = machine
            .artifacts
            .trace
            .iter()
            .find(|entry| entry["event"] == "verification_disposition_check")
            .unwrap();
        assert_eq!(check["document_identity_unchanged"], true);
        assert_eq!(check["relevant_facts_unchanged"], false);
    }

    #[test]
    fn verification_attestation_rechecks_document_identity() {
        let job = expectation_job();
        let redactor = Redactor::for_job(&job).unwrap();
        let initial = observed("Ready balance 12.34");
        let mut remounted = initial.clone();
        remounted.document_id = "remounted-document".into();
        let browser = FakeBrowser {
            captures: Mutex::new(VecDeque::from([
                page("Ready"),
                observation_page(initial),
                observation_page(remounted.clone()),
            ])),
            fallback: remounted,
            dispatch_result: None,
        };
        let provider = ExpectationSequenceProvider {
            nouls: Mutex::new(VecDeque::from([0.50, 0.50])),
            requests: Mutex::new(Vec::new()),
        };
        let control = RecordingHostedControl::default();
        let cancellation = manuvra_chrome::InputCancellation::default();
        let mut journal = MemoryJournal::default();
        let mut machine = HostedMachine::new(&job, &redactor);
        machine.drive(
            &browser,
            &provider,
            &mut journal,
            &cancellation,
            None,
            &control,
        );
        let escalation_id = machine.artifacts.escalation.as_ref().unwrap().id.clone();
        machine.apply(
            DispositionRequest {
                schema_version: SchemaVersion,
                escalation_id,
                disposition: Disposition::Advance(manuvra_contract::AdvanceDisposition {
                    kind: manuvra_contract::AdvanceKind::Advance,
                    rationale: "Caller attests the prior final facts.".into(),
                }),
            },
            &browser,
            &provider,
            &mut journal,
            &cancellation,
        );
        assert!(!machine.verification_complete);
        assert_eq!(
            machine.artifacts.stop.as_ref().unwrap().code,
            "verification_state_changed"
        );
        let check = machine
            .artifacts
            .trace
            .iter()
            .find(|entry| entry["event"] == "verification_disposition_check")
            .unwrap();
        assert_eq!(check["document_identity_unchanged"], false);
        assert_eq!(check["relevant_facts_unchanged"], true);
    }

    #[test]
    fn verification_attestation_rechecks_focus_used_by_provider() {
        let job = expectation_job();
        let redactor = Redactor::for_job(&job).unwrap();
        let mut initial = text_field("");
        initial.visible_text = "Ready balance 12.34".into();
        let mut focus_changed = initial.clone();
        focus_changed.focused = Some(1);
        let browser = FakeBrowser {
            captures: Mutex::new(VecDeque::from([
                page("Ready"),
                observation_page(initial),
                observation_page(focus_changed.clone()),
            ])),
            fallback: focus_changed,
            dispatch_result: None,
        };
        let provider = ExpectationSequenceProvider {
            nouls: Mutex::new(VecDeque::from([0.50, 0.50])),
            requests: Mutex::new(Vec::new()),
        };
        let control = RecordingHostedControl::default();
        let cancellation = manuvra_chrome::InputCancellation::default();
        let mut journal = MemoryJournal::default();
        let mut machine = HostedMachine::new(&job, &redactor);
        machine.drive(
            &browser,
            &provider,
            &mut journal,
            &cancellation,
            None,
            &control,
        );
        let escalation_id = machine.artifacts.escalation.as_ref().unwrap().id.clone();
        machine.apply(
            DispositionRequest {
                schema_version: SchemaVersion,
                escalation_id,
                disposition: Disposition::Advance(manuvra_contract::AdvanceDisposition {
                    kind: manuvra_contract::AdvanceKind::Advance,
                    rationale: "Caller attests the prior final facts.".into(),
                }),
            },
            &browser,
            &provider,
            &mut journal,
            &cancellation,
        );
        assert!(!machine.verification_complete);
        assert!(!machine.artifacts.caller_assisted);
        assert_eq!(
            machine.artifacts.stop.as_ref().unwrap().code,
            "verification_state_changed"
        );
        assert_eq!(provider.requests.lock().unwrap().len(), 2);
        let check = machine
            .artifacts
            .trace
            .iter()
            .find(|entry| entry["event"] == "verification_disposition_check")
            .unwrap();
        assert_eq!(check["document_identity_unchanged"], true);
        assert_eq!(check["relevant_facts_unchanged"], false);
    }

    #[test]
    fn retry_observation_rechecks_final_expectations_without_resetting_the_run() {
        let job = expectation_job();
        let redactor = Redactor::for_job(&job).unwrap();
        let final_page = observed("Ready balance 12.34");
        let browser = FakeBrowser {
            captures: Mutex::new(VecDeque::from([
                page("Ready"),
                observation_page(final_page.clone()),
                observation_page(final_page.clone()),
            ])),
            fallback: final_page,
            dispatch_result: None,
        };
        let provider = ExpectationSequenceProvider {
            nouls: Mutex::new(VecDeque::from([0.50, 0.95])),
            requests: Mutex::new(Vec::new()),
        };
        let control = RecordingHostedControl::default();
        let cancellation = manuvra_chrome::InputCancellation::default();
        let mut journal = MemoryJournal::default();
        let mut machine = HostedMachine::new(&job, &redactor);
        machine.drive(
            &browser,
            &provider,
            &mut journal,
            &cancellation,
            None,
            &control,
        );
        let escalation_id = machine.artifacts.escalation.as_ref().unwrap().id.clone();
        machine.apply(
            DispositionRequest {
                schema_version: SchemaVersion,
                escalation_id,
                disposition: Disposition::RetryObservation(
                    manuvra_contract::RetryObservationDisposition {
                        kind: manuvra_contract::RetryObservationKind::RetryObservation,
                    },
                ),
            },
            &browser,
            &provider,
            &mut journal,
            &cancellation,
        );
        machine.drive(
            &browser,
            &provider,
            &mut journal,
            &cancellation,
            None,
            &control,
        );
        assert!(machine.verification_complete);
        assert!(machine.artifacts.stop.is_none());
        assert_eq!(machine.artifacts.observations.len(), 3);
        assert_eq!(machine.artifacts.expectation_verdicts[0].noul, Some(0.95));
    }

    #[test]
    fn verification_maps_every_policy_and_provider_failure_truthfully() {
        for (stop, code, state) in [
            (
                policy::PolicyStop::Blocked("budget_exhausted"),
                "budget_exhausted",
                RunState::Blocked,
            ),
            (
                policy::PolicyStop::Uncertain("policy_uncertain"),
                "policy_uncertain",
                RunState::Uncertain,
            ),
        ] {
            let mapped = verification_policy_stop(stop);
            assert_eq!(mapped.code, code);
            assert_eq!(mapped.state, state);
        }
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
            assert_eq!(verification_provider_stop(&error).code, code);
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
    fn debug_force_stop_publishes_uncertainty_before_first_dispatch() {
        let mut job = mutation_job();
        job.options.debug = Some(manuvra_contract::DebugOptions {
            force_stop_at_step: "fill".into(),
        });
        let redactor = Redactor::for_job(&job).unwrap();
        let mut observation = observed("");
        observation.elements.push(Element {
            index: 1,
            node_id: 7,
            context: "main".into(),
            role: "textbox".into(),
            name: "Name".into(),
            input_type: Some("text".into()),
            value: String::new(),
            checked: None,
            selected: None,
            expanded: None,
            disabled: false,
            in_dialog: None,
            operations: vec!["TYPE_TEXT".into()],
            select_options: vec![],
            rect: Rect {
                x: 1.0,
                y: 1.0,
                width: 20.0,
                height: 10.0,
            },
        });
        let browser = FakeBrowser {
            captures: Mutex::new(VecDeque::from([observation_page(observation.clone())])),
            fallback: observation,
            dispatch_result: None,
        };
        let provider = TypeTextProvider(std::sync::atomic::AtomicUsize::new(0));
        let mut journal = MemoryJournal::default();
        let artifacts = drive_steps(
            &job,
            &redactor,
            &browser,
            &provider,
            &mut journal,
            &manuvra_chrome::InputCancellation::default(),
            None,
            None,
        );
        assert_eq!(artifacts.stop.unwrap().code, "debug_forced_stop");
        assert!(journal.0.is_empty());
        assert!(artifacts.escalation.is_some());
        assert_eq!(artifacts.escalations[0].1["offered_candidate"]["id"], "c_1");
        let exported = artifacts.escalations[0].1.to_string();
        assert!(!exported.contains("document_id"));
        assert!(!exported.contains("node_id"));
        assert!(!exported.contains("target_index"));
        assert_eq!(
            artifacts.escalations[0].1["offered_candidate"]["target_name"],
            "Name"
        );
    }

    #[test]
    fn exported_observation_keeps_public_indices_but_omits_browser_identity() {
        let job = mutation_job();
        let redactor = Redactor::for_job(&job).unwrap();
        let mut observation = text_field("");
        observation.document_id = "internal-document-token".into();
        observation.elements[0].node_id = 981_723;
        observation.elements[0].context = "main/shadow:981723".into();
        let exported = redacted_observation(&observation, &redactor).unwrap();
        let text = exported.to_string();

        assert_eq!(exported["elements"][0]["index"], 1);
        assert_eq!(exported["elements"][0]["name"], "Name");
        assert!(!text.contains("internal-document-token"));
        assert!(!text.contains("981723"));
        assert!(!text.contains("document_id"));
        assert!(!text.contains("node_id"));
        assert!(!text.contains("main/shadow"));
        assert!(exported["elements"][0].get("context").is_none());
    }

    #[test]
    fn caller_execute_reobserves_done_first_then_dispatches_once_without_operation_reask() {
        let mut job = mutation_job();
        job.options.debug = Some(manuvra_contract::DebugOptions {
            force_stop_at_step: "fill".into(),
        });
        let redactor = Redactor::for_job(&job).unwrap();
        let empty = text_field("");
        let filled = text_field("Wanted");
        let browser = FakeBrowser {
            captures: Mutex::new(VecDeque::from([
                observation_page(empty.clone()),
                observation_page(empty.clone()),
                observation_page(filled),
            ])),
            fallback: empty,
            dispatch_result: Some(Ok(manuvra_chrome::PerformFact {
                readback: Some("Wanted".into()),
                readback_matches: None,
                suboperations: vec![],
            })),
        };
        let evaluator = TypeTextProvider(std::sync::atomic::AtomicUsize::new(0));
        let control = RecordingHostedControl::default();
        let cancellation = manuvra_chrome::InputCancellation::default();
        let mut journal = MemoryJournal::default();
        let mut machine = HostedMachine::new(&job, &redactor);
        machine.drive(
            &browser,
            &evaluator,
            &mut journal,
            &cancellation,
            None,
            &control,
        );
        let candidate_id = machine
            .artifacts
            .pending
            .as_ref()
            .and_then(|pending| pending.candidate.as_ref())
            .unwrap()
            .id
            .clone();
        machine.apply(
            DispositionRequest {
                schema_version: SchemaVersion,
                escalation_id: "e_1".into(),
                disposition: Disposition::Execute(manuvra_contract::ExecuteDisposition {
                    kind: manuvra_contract::ExecuteKind::Execute,
                    candidate_id,
                }),
            },
            &browser,
            &evaluator,
            &mut journal,
            &cancellation,
        );
        machine.drive(
            &browser,
            &evaluator,
            &mut journal,
            &cancellation,
            None,
            &control,
        );

        assert_eq!(machine.index, 1);
        assert!(machine.artifacts.caller_assisted);
        assert_eq!(
            evaluator.0.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "resume execute must not re-ask the operation gate"
        );
        assert_eq!(
            journal
                .0
                .iter()
                .filter(|event| event["event"] == "action_prepared")
                .count(),
            1
        );
        assert!(journal.0.iter().all(|event| {
            event["event"] != "action_prepared" || event["basis"] == "caller_authority"
        }));
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn hosted_flow_round_trip_publishes_assisted_terminal_evidence() {
        let temporary = TempDir::new().unwrap();
        let mut job = mutation_job();
        job.options.debug = Some(manuvra_contract::DebugOptions {
            force_stop_at_step: "fill".into(),
        });
        let redactor = Redactor::for_job(&job).unwrap();
        let empty = text_field("");
        let mut browser = FakeBrowser {
            captures: Mutex::new(VecDeque::from([
                observation_page(empty.clone()),
                observation_page(empty),
                observation_page(text_field("Wanted")),
            ])),
            fallback: observed(""),
            dispatch_result: Some(Ok(manuvra_chrome::PerformFact {
                readback: Some("Wanted".into()),
                readback_matches: None,
                suboperations: vec![],
            })),
        };
        let control = DispositionHostedControl {
            checkpoints: Mutex::new(Vec::new()),
            request: Mutex::new(Some(DispositionRequest {
                schema_version: SchemaVersion,
                escalation_id: "e_1".into(),
                disposition: Disposition::Execute(manuvra_contract::ExecuteDisposition {
                    kind: manuvra_contract::ExecuteKind::Execute,
                    candidate_id: "c_1".into(),
                }),
            })),
        };
        let config = FlowConfig {
            request_id: "resume-round-trip".into(),
            run_id: "r_roundtrip".into(),
            evidence_root: temporary.path().join("evidence"),
            browser: None,
            headless: true,
        };
        std::fs::create_dir_all(&config.evidence_root).unwrap();
        let mut journal =
            actions::DurableJournal::open(&config.evidence_root, &config.run_id, &redactor)
                .unwrap();
        let outcome = finish_hosted_browser_run(
            &job,
            config.clone(),
            &redactor,
            &mut browser,
            json!({"fixture":"resume"}),
            &TypeTextProvider(std::sync::atomic::AtomicUsize::new(0)),
            &mut journal,
            &manuvra_chrome::InputCancellation::default(),
            &control,
        )
        .unwrap();
        assert_eq!(outcome.result["state"], "passed");
        assert_eq!(outcome.result["verdict"]["caller_assisted"], true);
        assert!(
            config
                .evidence_root
                .join("r_roundtrip/dispositions/request_0001.json")
                .is_file()
        );
        assert!(control.checkpoints.lock().unwrap().len() >= 2);
        assert!(
            control
                .checkpoints
                .lock()
                .unwrap()
                .iter()
                .any(|checkpoint| checkpoint["verdict"]["caller_assisted"] == true)
        );
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn hosted_pause_termination_closes_the_browser_and_publishes_abort() {
        let temporary = TempDir::new().unwrap();
        let mut job = mutation_job();
        job.options.debug = Some(manuvra_contract::DebugOptions {
            force_stop_at_step: "fill".into(),
        });
        let redactor = Redactor::for_job(&job).unwrap();
        let observation = text_field("");
        let mut browser = FakeBrowser {
            captures: Mutex::new(VecDeque::from([observation_page(observation.clone())])),
            fallback: observation,
            dispatch_result: None,
        };
        let config = FlowConfig {
            request_id: "pause-abort".into(),
            run_id: "r_pause_abort".into(),
            evidence_root: temporary.path().join("evidence"),
            browser: None,
            headless: true,
        };
        std::fs::create_dir_all(&config.evidence_root).unwrap();
        let mut journal =
            actions::DurableJournal::open(&config.evidence_root, &config.run_id, &redactor)
                .unwrap();
        let control = RecordingHostedControl::default();
        let outcome = finish_hosted_browser_run(
            &job,
            config,
            &redactor,
            &mut browser,
            json!({"fixture":"pause-abort"}),
            &TypeTextProvider(std::sync::atomic::AtomicUsize::new(0)),
            &mut journal,
            &manuvra_chrome::InputCancellation::default(),
            &control,
        )
        .unwrap();
        assert_eq!(outcome.result["state"], "aborted");
        assert_eq!(outcome.result["reason"]["code"], "caller_aborted");
        assert_eq!(outcome.result["cleanup"]["browser"], "closed");
    }

    #[test]
    fn retry_observation_preserves_model_budget_and_replay_ledger() {
        let mut job = mutation_job();
        job.options.max_model_calls = Some(1);
        job.options.debug = Some(manuvra_contract::DebugOptions {
            force_stop_at_step: "fill".into(),
        });
        let redactor = Redactor::for_job(&job).unwrap();
        let observation = text_field("");
        let browser = FakeBrowser {
            captures: Mutex::new(VecDeque::from([observation_page(observation.clone())])),
            fallback: observation.clone(),
            dispatch_result: None,
        };
        let control = RecordingHostedControl::default();
        let mut machine = HostedMachine::new(&job, &redactor);
        let mut journal = MemoryJournal::default();
        machine.drive(
            &browser,
            &TypeTextProvider(std::sync::atomic::AtomicUsize::new(0)),
            &mut journal,
            &manuvra_chrome::InputCancellation::default(),
            None,
            &control,
        );
        let candidate = machine
            .artifacts
            .pending
            .as_ref()
            .and_then(|pending| pending.candidate.clone())
            .unwrap();
        let _reserved = machine
            .policy
            .authorize_caller(&job.steps[0], &observation, &candidate)
            .unwrap();

        machine.apply(
            DispositionRequest {
                schema_version: SchemaVersion,
                escalation_id: "e_1".into(),
                disposition: Disposition::RetryObservation(
                    manuvra_contract::RetryObservationDisposition {
                        kind: manuvra_contract::RetryObservationKind::RetryObservation,
                    },
                ),
            },
            &browser,
            &NoProvider,
            &mut journal,
            &manuvra_chrome::InputCancellation::default(),
        );

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
    #[cfg(target_os = "linux")]
    fn abort_persists_disposition_and_reports_actions_already_sent() {
        let temporary = TempDir::new().unwrap();
        let job = mutation_job();
        let redactor = Redactor::for_job(&job).unwrap();
        let config = FlowConfig {
            request_id: "abort-resume".into(),
            run_id: "r_abort_resume".into(),
            evidence_root: temporary.path().join("evidence"),
            browser: None,
            headless: true,
        };
        std::fs::create_dir_all(&config.evidence_root).unwrap();
        let mut machine = HostedMachine::new(&job, &redactor);
        machine.artifacts.stop = Some(Stop::uncertain("caller_review", BTreeMap::new()));
        machine.artifacts.trace.extend([
            json!({"event":"action_prepared","action_sequence":1,"outcome":"not_performed"}),
            json!({"event":"action_fact","action_sequence":1,"outcome":"confirmed"}),
        ]);
        publish_pause_checkpoint(
            &job,
            &config,
            &redactor,
            json!({"fixture":"abort"}),
            &machine.artifacts,
        )
        .unwrap();
        let browser_observation = text_field("");
        let mut browser = FakeBrowser {
            captures: Mutex::new(VecDeque::new()),
            fallback: browser_observation,
            dispatch_result: None,
        };
        let control = RecordingHostedControl::default();
        let mut journal =
            actions::DurableJournal::open(&config.evidence_root, &config.run_id, &redactor)
                .unwrap();
        let termination = machine
            .apply(
                DispositionRequest {
                    schema_version: SchemaVersion,
                    escalation_id: "e_1".into(),
                    disposition: Disposition::Abort(manuvra_contract::AbortDisposition {
                        kind: manuvra_contract::AbortKind::Abort,
                    }),
                },
                &browser,
                &NoProvider,
                &mut journal,
                &manuvra_chrome::InputCancellation::default(),
            )
            .unwrap();
        let outcome = finalize_paused_termination(
            &job,
            &config,
            &redactor,
            &mut browser,
            json!({"fixture":"abort"}),
            &mut journal,
            &control,
            &mut machine,
            termination,
        )
        .unwrap();
        assert_eq!(outcome.result["state"], "aborted");
        assert_eq!(outcome.result["verdict"]["caller_assisted"], true);
        let trace =
            std::fs::read_to_string(config.evidence_root.join("r_abort_resume/trace.jsonl"))
                .unwrap();
        assert!(trace.contains("action_prepared"));
        assert!(trace.contains("action_fact"));
        assert!(
            config
                .evidence_root
                .join("r_abort_resume/dispositions/request_0001.json")
                .is_file()
        );
    }

    #[test]
    fn resume_done_judgment_records_the_natural_language_decision() {
        let mut job = mutation_job();
        job.steps[0].done_when = DoneCondition::NaturalLanguage("The form is complete".into());
        let redactor = Redactor::for_job(&job).unwrap();
        let mut machine = HostedMachine::new(&job, &redactor);
        let captured = Captured {
            raw: text_field(""),
            artifact: ("o_resume".into(), json!({}), None),
            redaction_verified: true,
        };
        let (done, noul) = match machine.judge_resume_done(
            "The form is complete",
            &captured,
            &TypeTextProvider(std::sync::atomic::AtomicUsize::new(0)),
        ) {
            Ok(value) => value,
            Err(_) => panic!("natural-language resume judgment should succeed"),
        };
        assert_eq!(done, DoneResult::NotSatisfied);
        assert_eq!(noul, Some(0.01));
        assert_eq!(machine.artifacts.decisions.len(), 1);
    }

    #[test]
    fn resumed_unknown_done_reissues_the_condition_specific_reason() {
        for (natural, reason) in [(true, "done_uncertain"), (false, "done_unknown")] {
            let mut job = mutation_job();
            if natural {
                job.steps[0].done_when =
                    DoneCondition::NaturalLanguage("The form is complete".into());
            }
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
                natural.then_some(0.5),
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

    #[test]
    fn resumed_action_failures_preserve_truthful_terminal_classification() {
        let job = mutation_job();
        let redactor = Redactor::for_job(&job).unwrap();
        let step = &job.steps[0];
        for (failure, state, code) in [
            (
                actions::ActionStop::EvidenceUnavailable,
                RunState::Blocked,
                "evidence_unavailable",
            ),
            (
                actions::ActionStop::ReadbackMismatch,
                RunState::Failed,
                "write_readback_mismatch",
            ),
            (
                actions::ActionStop::IncompleteEvidence,
                RunState::Uncertain,
                "evidence_incomplete_after_dispatch",
            ),
            (
                actions::ActionStop::Uncertain,
                RunState::Uncertain,
                "action_outcome_uncertain",
            ),
            (
                actions::ActionStop::InvalidPermit,
                RunState::Uncertain,
                "action_outcome_uncertain",
            ),
        ] {
            let stop = resumed_action_stop(failure, &redactor, step);
            assert_eq!(stop.state, state);
            assert_eq!(stop.code, code);
        }
    }

    #[test]
    fn advance_requires_natural_uncertainty_without_pending_mutation_and_unchanged_state() {
        let mut job = job("unused");
        job.steps[0].done_when = DoneCondition::NaturalLanguage("The journey is complete".into());
        let redactor = Redactor::for_job(&job).unwrap();
        let observation = observed("unchanged");
        let browser = FakeBrowser {
            captures: Mutex::new(VecDeque::new()),
            fallback: observation.clone(),
            dispatch_result: None,
        };
        let mut machine = HostedMachine::new(&job, &redactor);
        machine.artifacts.pending = Some(PendingEscalation {
            done: DoneResult::Unknown,
            noul: Some(0.5),
            candidate: None,
            observation: observation.clone(),
            ambiguous_mutation: false,
        });
        machine.apply_advance("caller verified the condition", &browser);
        assert_eq!(machine.index, 1);
        assert_eq!(
            machine.artifacts.verdicts[0].basis.as_deref(),
            Some("caller_attestation")
        );
        let advance_check = machine
            .artifacts
            .trace
            .iter()
            .find(|entry| entry["event"] == "disposition_check")
            .unwrap();
        assert_eq!(advance_check["relevant_state_unchanged"], true);
        assert_eq!(advance_check["numeric_checks_satisfied"], true);
        assert_eq!(
            advance_check["prior_state_hash"],
            advance_check["current_state_hash"]
        );

        let allowed = PendingEscalation {
            done: DoneResult::Unknown,
            noul: Some(0.5),
            candidate: None,
            observation: observed("unchanged"),
            ambiguous_mutation: false,
        };
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

        let mut numeric_job = job.clone();
        numeric_job.steps[0].done_when =
            DoneCondition::NaturalLanguage("The balance is 12.34".into());
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
            noul: None,
            candidate: None,
            observation,
            ambiguous_mutation: false,
        });
        structured.apply_advance("not allowed", &browser);
        assert_eq!(structured.index, 0);
        assert_eq!(
            structured.artifacts.stop.as_ref().unwrap().code,
            "advance_not_permitted"
        );
        assert!(!structured.artifacts.caller_assisted);
        let mut structured_unknown = allowed;
        structured_unknown.done = DoneResult::Unknown;
        assert!(!advance_permitted(
            &structured_job.steps[0],
            &structured_unknown,
            "attested"
        ));
    }

    #[test]
    fn execute_does_not_dispatch_when_done_or_when_the_target_is_stale() {
        let job = mutation_job();
        let redactor = Redactor::for_job(&job).unwrap();
        let empty = text_field("");
        let judgments = judgment::Judgments {
            operation: judgment::ChoiceJudgment {
                choice: "TYPE_TEXT".into(),
                probabilities: BTreeMap::from([("TYPE_TEXT".into(), 1.0)]),
                confidence: 0.5,
            },
            click_target: judgment::ChoiceJudgment {
                choice: "NO_CLICK_TARGET".into(),
                probabilities: BTreeMap::from([("NO_CLICK_TARGET".into(), 1.0)]),
                confidence: 1.0,
            },
            type_target: judgment::ChoiceJudgment {
                choice: "1".into(),
                probabilities: BTreeMap::from([("1".into(), 1.0)]),
                confidence: 1.0,
            },
            select_target: judgment::ChoiceJudgment {
                choice: "NO_SELECT_TARGET".into(),
                probabilities: BTreeMap::from([("NO_SELECT_TARGET".into(), 1.0)]),
                confidence: 1.0,
            },
            type_value: judgment::ChoiceJudgment {
                choice: "name".into(),
                probabilities: BTreeMap::from([("name".into(), 1.0)]),
                confidence: 1.0,
            },
            step_done: 0.0,
            usage: BTreeMap::new(),
            request_id: None,
            model: "fixture".into(),
            request: Value::Null,
        };
        let mut machine = HostedMachine::new(&job, &redactor);
        let candidate = machine.policy.caller_candidate(&empty, &judgments).unwrap();
        machine.artifacts.pending = Some(PendingEscalation {
            done: DoneResult::NotSatisfied,
            noul: None,
            candidate: Some(candidate.clone()),
            observation: empty.clone(),
            ambiguous_mutation: false,
        });
        let done_browser = FakeBrowser {
            captures: Mutex::new(VecDeque::from([observation_page(text_field("Wanted"))])),
            fallback: empty.clone(),
            dispatch_result: Some(Err(manuvra_chrome::PerformError::Uncertain(
                "must not dispatch".into(),
            ))),
        };
        let mut journal = MemoryJournal::default();
        machine.apply_execute(
            &candidate.id,
            &done_browser,
            &NoProvider,
            &mut journal,
            &manuvra_chrome::InputCancellation::default(),
        );
        assert_eq!(machine.index, 1);
        assert!(journal.0.is_empty());
        assert_eq!(machine.artifacts.steps.len(), 1);
        assert_eq!(machine.artifacts.steps[0].1["done"], "satisfied");

        let mut stale_machine = HostedMachine::new(&job, &redactor);
        stale_machine.artifacts.pending = Some(PendingEscalation {
            done: DoneResult::NotSatisfied,
            noul: None,
            candidate: Some(candidate.clone()),
            observation: empty.clone(),
            ambiguous_mutation: false,
        });
        let mut remounted = empty.clone();
        remounted.elements[0].node_id = 99;
        let stale_browser = FakeBrowser {
            captures: Mutex::new(VecDeque::from([observation_page(remounted)])),
            fallback: empty,
            dispatch_result: None,
        };
        stale_machine.apply_execute(
            &candidate.id,
            &stale_browser,
            &NoProvider,
            &mut journal,
            &manuvra_chrome::InputCancellation::default(),
        );
        assert_eq!(stale_machine.index, 0);
        assert_eq!(
            stale_machine.artifacts.stop.as_ref().unwrap().code,
            "candidate_revalidation_failed"
        );
        assert!(!stale_machine.artifacts.caller_assisted);
        assert!(journal.0.is_empty());
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
            step_done: 0.0,
            usage: BTreeMap::new(),
            request_id: None,
            model: "jev-test".into(),
            request: Value::Null,
        }
    }

    #[test]
    fn natural_done_uncertainty_reobserves_once_and_never_dispatches() {
        let mut job = mutation_job();
        job.steps[0].done_when =
            DoneCondition::NaturalLanguage("El campo Name contiene el valor proporcionado".into());
        let redactor = Redactor::for_job(&job).unwrap();
        let mut observation = observed("");
        observation.elements.push(Element {
            index: 1,
            node_id: 7,
            context: "main".into(),
            role: "textbox".into(),
            name: "Name".into(),
            input_type: Some("text".into()),
            value: "Wanted".into(),
            checked: None,
            selected: None,
            expanded: None,
            disabled: false,
            in_dialog: None,
            operations: vec!["TYPE_TEXT".into()],
            select_options: vec![],
            rect: Rect {
                x: 1.0,
                y: 1.0,
                width: 20.0,
                height: 10.0,
            },
        });
        let browser = FakeBrowser {
            captures: Mutex::new(VecDeque::from([
                observation_page(observation.clone()),
                observation_page(observation.clone()),
            ])),
            fallback: observation,
            dispatch_result: None,
        };
        let provider = NaturalDoneSequenceProvider(Mutex::new(VecDeque::from([0.50, 0.50])));
        let mut journal = MemoryJournal::default();
        let artifacts = drive_steps(
            &job,
            &redactor,
            &browser,
            &provider,
            &mut journal,
            &manuvra_chrome::InputCancellation::default(),
            None,
            None,
        );
        assert_eq!(artifacts.stop.as_ref().unwrap().code, "done_uncertain");
        assert_eq!(artifacts.observations.len(), 2);
        assert_eq!(artifacts.decisions.len(), 2);
        assert_eq!(artifacts.verdicts[0].result, VerdictResult::Unresolved);
        assert!(
            artifacts
                .trace
                .iter()
                .all(|event| event["event"] != "action_prepared")
        );
        assert!(provider.0.lock().unwrap().is_empty());
    }

    #[test]
    fn step_driver_routes_every_policy_and_action_stop_truthfully() {
        let job = mutation_job();
        let redactor = Redactor::for_job(&job).unwrap();
        let mut observation = observed("");
        observation.elements.push(Element {
            index: 1,
            node_id: 7,
            context: "main".into(),
            role: "textbox".into(),
            name: "Name".into(),
            input_type: Some("text".into()),
            value: String::new(),
            checked: None,
            selected: None,
            expanded: None,
            disabled: false,
            in_dialog: None,
            operations: vec!["TYPE_TEXT".into()],
            select_options: vec![],
            rect: Rect {
                x: 1.0,
                y: 1.0,
                width: 20.0,
                height: 10.0,
            },
        });
        let captured = Captured {
            raw: observation.clone(),
            artifact: ("observation".into(), Value::Null, None),
            redaction_verified: true,
        };
        let browser = FakeBrowser {
            captures: Mutex::new(VecDeque::new()),
            fallback: observation.clone(),
            dispatch_result: None,
        };
        let evaluator = NoProvider;
        let mut journal = MemoryJournal::default();
        let values = Values::new(&job);
        let cancellation = manuvra_chrome::InputCancellation::default();
        let mut policy = policy::Policy::new(&job.options, "http://127.0.0.1:4351/");
        let mut artifacts = RunArtifacts::new(&job, &redactor);
        let mut driver = StepDriver {
            job: &job,
            redactor: &redactor,
            browser: &browser,
            evaluator: &evaluator,
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
        let done = DoneResult::NotSatisfied;
        let typed = mutation_judgments("TYPE_TEXT", 1.0);
        assert!(matches!(
            driver.apply_next(done, &captured, &typed, policy::Next::ReobserveOperation),
            StepProgress::Continue
        ));
        assert!(matches!(
            driver.apply_next(done, &captured, &typed, policy::Next::Wait),
            StepProgress::Continue
        ));
        let next = driver
            .policy
            .decide(driver.step, &captured.raw, &typed, done, false, false);
        assert!(matches!(
            driver.apply_terminal_next(done, &captured, &typed, next),
            StepProgress::Stop(_)
        ));
        assert!(matches!(
            driver.apply_after_reobserve(
                done,
                &captured,
                &typed,
                policy::Next::Stop(policy::PolicyStop::Blocked("operation_blocked"))
            ),
            StepProgress::Stop(_)
        ));
        assert_eq!(
            driver
                .policy_stop(
                    done,
                    &captured,
                    &typed,
                    policy::PolicyStop::Uncertain("target_below_gate")
                )
                .code,
            "target_below_gate"
        );
        assert_eq!(
            driver
                .policy_stop(
                    done,
                    &captured,
                    &typed,
                    policy::PolicyStop::Blocked("value_not_provided")
                )
                .code,
            "value_not_provided"
        );
        for (stop, code) in [
            (
                actions::ActionStop::EvidenceUnavailable,
                "evidence_unavailable",
            ),
            (
                actions::ActionStop::ReadbackMismatch,
                "write_readback_mismatch",
            ),
            (
                actions::ActionStop::IncompleteEvidence,
                "evidence_incomplete_after_dispatch",
            ),
            (
                actions::ActionStop::InvalidPermit,
                "candidate_revalidation_failed",
            ),
        ] {
            assert_eq!(driver.action_stop(done, &captured, &typed, stop).code, code);
        }
        assert_eq!(
            super::policy_stop(
                policy::PolicyStop::Uncertain("operation_below_gate"),
                &redactor,
                &job.steps[0]
            )
            .code,
            "operation_below_gate"
        );
    }

    #[test]
    fn scripted_browser_proves_satisfied_and_failed_done_paths() {
        let yes = job("Ready");
        let redactor = Redactor::for_job(&yes).unwrap();
        let fake = FakeBrowser {
            captures: Mutex::new(VecDeque::from([page("Ready")])),
            fallback: observed("Ready"),
            dispatch_result: None,
        };
        let passed = drive_fake(&yes, &redactor, &fake);
        assert!(passed.stop.is_none());
        assert_eq!(passed.verdicts[0].result, VerdictResult::Satisfied);
        let no = job("Ready");
        let fake = FakeBrowser {
            captures: Mutex::new(VecDeque::from([page("Not yet"), page("Still not")])),
            fallback: observed("Still not"),
            dispatch_result: None,
        };
        let failed = drive_fake(&no, &redactor, &fake);
        assert_eq!(failed.stop.as_ref().unwrap().code, "provider_unavailable");
        assert_eq!(failed.observations.len(), 1);
        let (state, reason, exit_code, overall) = terminal_fields(failed.stop);
        assert_eq!(state, RunState::Blocked);
        assert_eq!(reason.unwrap().code, "provider_unavailable");
        assert_eq!(exit_code, 3);
        assert_eq!(overall, VerdictResult::Unresolved);
    }

    #[test]
    fn mutation_limit_waits_for_one_final_reobservation_without_a_second_judgment() {
        let mut job = mutation_job();
        job.steps[0].mutation_limit = 1;
        let redactor = Redactor::for_job(&job).unwrap();
        let mut before = observed("");
        before.elements.push(Element {
            index: 1,
            node_id: 7,
            context: "main".into(),
            role: "textbox".into(),
            name: "Name".into(),
            input_type: Some("text".into()),
            value: String::new(),
            checked: None,
            selected: None,
            expanded: None,
            disabled: false,
            in_dialog: None,
            operations: vec!["TYPE_TEXT".into()],
            select_options: vec![],
            rect: Rect {
                x: 1.0,
                y: 1.0,
                width: 20.0,
                height: 10.0,
            },
        });
        let delayed = before.clone();
        let mut completed = before.clone();
        completed.elements[0].value = "Wanted".into();
        let browser = FakeBrowser {
            captures: Mutex::new(VecDeque::from([
                observation_page(before),
                observation_page(delayed),
                observation_page(completed.clone()),
            ])),
            fallback: completed,
            dispatch_result: Some(Ok(manuvra_chrome::PerformFact {
                readback: Some("Wanted".into()),
                readback_matches: None,
                suboperations: vec![],
            })),
        };
        let provider = TypeTextProvider(std::sync::atomic::AtomicUsize::new(0));
        let mut journal = MemoryJournal::default();
        let artifacts = drive_steps(
            &job,
            &redactor,
            &browser,
            &provider,
            &mut journal,
            &manuvra_chrome::InputCancellation::default(),
            None,
            None,
        );
        assert!(artifacts.stop.is_none());
        assert_eq!(artifacts.observations.len(), 4);
        assert_eq!(artifacts.decisions.len(), 1);
        assert_eq!(provider.0.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert_eq!(artifacts.verdicts[0].result, VerdictResult::Satisfied);
        let action_events: Vec<_> = artifacts
            .trace
            .iter()
            .filter_map(|event| event.get("event").and_then(Value::as_str))
            .filter(|event| event.starts_with("action_"))
            .collect();
        assert_eq!(action_events, ["action_prepared", "action_fact"]);
    }

    #[test]
    fn done_unknown_and_low_operation_gate_each_receive_their_own_reobservation() {
        let job = mutation_job();
        let redactor = Redactor::for_job(&job).unwrap();
        let unknown = observed("");
        let mut not_satisfied = observed("");
        not_satisfied.elements.push(Element {
            index: 1,
            node_id: 7,
            context: "main".into(),
            role: "textbox".into(),
            name: "Name".into(),
            input_type: Some("text".into()),
            value: String::new(),
            checked: None,
            selected: None,
            expanded: None,
            disabled: false,
            in_dialog: None,
            operations: vec!["TYPE_TEXT".into()],
            select_options: vec![],
            rect: Rect {
                x: 1.0,
                y: 1.0,
                width: 20.0,
                height: 10.0,
            },
        });
        let mut completed = not_satisfied.clone();
        completed.elements[0].value = "Wanted".into();
        let browser = FakeBrowser {
            captures: Mutex::new(VecDeque::from([
                observation_page(unknown),
                observation_page(not_satisfied.clone()),
                observation_page(not_satisfied),
                observation_page(completed.clone()),
            ])),
            fallback: completed,
            dispatch_result: Some(Ok(manuvra_chrome::PerformFact {
                readback: Some("Wanted".into()),
                readback_matches: None,
                suboperations: vec![],
            })),
        };
        let provider = ConfidenceSequenceProvider(Mutex::new(VecDeque::from([0.69, 0.95])));
        let mut journal = MemoryJournal::default();
        let artifacts = drive_steps(
            &job,
            &redactor,
            &browser,
            &provider,
            &mut journal,
            &manuvra_chrome::InputCancellation::default(),
            None,
            None,
        );
        assert!(artifacts.stop.is_none());
        assert_eq!(artifacts.observations.len(), 5);
        assert_eq!(artifacts.decisions.len(), 2);
        assert_eq!(artifacts.verdicts[0].result, VerdictResult::Satisfied);
        assert_eq!(provider.0.lock().unwrap().len(), 0);
        assert_eq!(
            artifacts
                .trace
                .iter()
                .filter(|event| event["event"] == "action_prepared")
                .count(),
            1
        );
    }

    #[test]
    fn scripted_browser_turns_unverifiable_masking_and_control_failure_into_stops() {
        let job = job("Ready");
        let redactor = Redactor::for_job(&job).unwrap();
        let masked = FakeBrowser {
            captures: Mutex::new(VecDeque::from([Err(BrowserError::Control(
                "redaction_unverifiable".into(),
            ))])),
            fallback: observed("Ready"),
            dispatch_result: None,
        };
        assert_eq!(
            drive_fake(&job, &redactor, &masked).stop.unwrap().code,
            "redaction_unverifiable"
        );
        let broken = FakeBrowser {
            captures: Mutex::new(VecDeque::from([Err(BrowserError::Control(
                "marker".into(),
            ))])),
            fallback: observed("Ready"),
            dispatch_result: None,
        };
        assert_eq!(
            drive_fake(&job, &redactor, &broken).stop.unwrap().code,
            "browser_control_failed"
        );
    }

    #[test]
    fn classified_text_cannot_corrupt_result_protocol_literals() {
        let mut job = job("Ready");
        job.values.insert(
            "passed_collision".into(),
            manuvra_contract::JobValue {
                value: "passed".into(),
                description: "classified collision".into(),
                formats: None,
                secret: true,
            },
        );
        job.values.insert(
            "failed_collision".into(),
            manuvra_contract::JobValue {
                value: "failed".into(),
                description: "classified collision".into(),
                formats: None,
                secret: true,
            },
        );
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
        let result = run_result(
            &job,
            &config,
            &redactor,
            RunState::Passed,
            None,
            VerdictResult::Satisfied,
            vec![StepVerdict {
                id: "ready".into(),
                result: VerdictResult::Satisfied,
                basis: Some("structured".into()),
            }],
            None,
            cleanup,
        )
        .unwrap();
        assert_eq!(result["state"], "passed");
        assert_eq!(result["verdict"]["overall"], "satisfied");
        let failed = run_result(
            &job,
            &config,
            &redactor,
            RunState::Failed,
            None,
            VerdictResult::NotSatisfied,
            vec![StepVerdict {
                id: "ready".into(),
                result: VerdictResult::NotSatisfied,
                basis: Some("structured".into()),
            }],
            None,
            Cleanup {
                browser: "closed".into(),
                profile: "removed".into(),
                application_state: "caller_owned".into(),
            },
        )
        .unwrap();
        assert_eq!(failed["state"], "failed");
        assert_eq!(failed["verdict"]["overall"], "not_satisfied");
    }

    #[test]
    fn unavailable_explicit_browser_publishes_a_complete_blocked_run() {
        let job = job("Ready");
        let redactor = Redactor::for_job(&job).unwrap();
        let temp = TempDir::new().unwrap();
        let outcome = run(
            &job,
            FlowConfig {
                request_id: "fake-unavailable".into(),
                run_id: "r_fake".into(),
                evidence_root: temp.path().to_path_buf(),
                browser: Some(temp.path().join("missing-browser")),
                headless: true,
            },
            &redactor,
        )
        .unwrap();
        assert_eq!(outcome.exit_code, 3);
        assert_eq!(outcome.result["reason"]["code"], "browser_unavailable");
        assert!(temp.path().join("r_fake/manifest.json").is_file());
    }

    #[cfg(target_os = "linux")]
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
