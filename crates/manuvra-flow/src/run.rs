use crate::evidence::{self, EvidenceBundle, Redactor};
#[cfg(any(target_os = "linux", test))]
use crate::verification::{DoneResult, check_done, check_natural_done};
#[cfg(any(target_os = "linux", test))]
use crate::{actions, judgment, policy, values::Values};
#[cfg(target_os = "linux")]
use manuvra_chrome::{BrowserConfig, OwnedBrowser};
#[cfg(any(target_os = "linux", test))]
use manuvra_chrome::{BrowserError, CapturedPage, Observation};
#[cfg(any(target_os = "linux", test))]
use manuvra_contract::DispositionKind;
#[cfg(any(target_os = "linux", test))]
use manuvra_contract::DoneCondition;
use manuvra_contract::{
    Cleanup, Escalation, EvidenceRef, ExpectationVerdict, Job, Reason, RunResult, RunState,
    SchemaVersion, StepVerdict, Verdict, VerdictResult,
};
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
pub trait HostedControl {
    fn cancellation(&self) -> manuvra_chrome::InputCancellation;
    fn pause_deadline_unix_ms(&self) -> u64;
    fn publish_checkpoint(&self, result: &Value) -> Result<(), String>;
    fn wait_while_paused(&self) -> HostedTermination;
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
            let mut artifacts = drive_steps(
                job,
                redactor,
                &browser,
                &evaluator,
                &mut journal,
                &cancellation,
                Some(&config),
                control,
            );
            if let Some(termination) = control.and_then(HostedControl::termination) {
                return publish_active_hosted_stop(
                    job,
                    config,
                    redactor,
                    &mut browser,
                    provenance,
                    artifacts,
                    &mut journal,
                    termination,
                );
            }
            if artifacts
                .stop
                .as_ref()
                .is_some_and(|stop| stop.state == RunState::Uncertain)
                && let Some(control) = control
            {
                return pause_hosted_run(
                    job,
                    config,
                    redactor,
                    &mut browser,
                    provenance,
                    artifacts,
                    &mut journal,
                    control,
                );
            }
            let cleanup = cleanup_browser(&mut browser, redactor, &mut artifacts.stop);
            let (state, reason, exit_code, overall) = terminal_fields(artifacts.stop);
            run_result(
                job,
                &config,
                redactor,
                state,
                reason,
                overall,
                artifacts.verdicts,
                artifacts.escalation.clone(),
                cleanup.clone(),
            )
            .and_then(|result| {
                publish_bundle(
                    job,
                    config,
                    redactor,
                    provenance,
                    artifacts.observations,
                    artifacts.decisions,
                    artifacts.steps,
                    artifacts.escalations,
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
fn publish_active_hosted_stop(
    job: &Job,
    config: FlowConfig,
    redactor: &Redactor,
    browser: &mut OwnedBrowser,
    provenance: Value,
    artifacts: RunArtifacts,
    journal: &mut actions::DurableJournal,
    termination: HostedTermination,
) -> Result<FlowOutcome, String> {
    let cleanup = cleanup_started_browser(browser);
    let (state, code, exit_code) = hosted_termination_fields(termination);
    let result = run_result(
        job,
        &config,
        redactor,
        state,
        Some(Reason {
            code: code.into(),
            details: BTreeMap::new(),
        }),
        VerdictResult::Unresolved,
        artifacts.verdicts,
        artifacts.escalation,
        cleanup.clone(),
    )?;
    publish_bundle(
        job,
        config,
        redactor,
        provenance,
        artifacts.observations,
        artifacts.decisions,
        artifacts.steps,
        artifacts.escalations,
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
#[allow(clippy::too_many_arguments)]
fn pause_hosted_run(
    job: &Job,
    config: FlowConfig,
    redactor: &Redactor,
    browser: &mut OwnedBrowser,
    provenance: Value,
    mut artifacts: RunArtifacts,
    journal: &mut actions::DurableJournal,
    control: &dyn HostedControl,
) -> Result<FlowOutcome, String> {
    set_hosted_escalation_deadline(&mut artifacts, control.pause_deadline_unix_ms());
    publish_pause_checkpoint(job, &config, redactor, provenance, &artifacts).and_then(|published| {
        continue_paused_run(
            job, &config, redactor, browser, artifacts, journal, control, published,
        )
    })
}

#[cfg(target_os = "linux")]
#[allow(clippy::too_many_arguments)]
fn continue_paused_run(
    job: &Job,
    config: &FlowConfig,
    redactor: &Redactor,
    browser: &mut OwnedBrowser,
    artifacts: RunArtifacts,
    journal: &mut actions::DurableJournal,
    control: &dyn HostedControl,
    published: FlowOutcome,
) -> Result<FlowOutcome, String> {
    control.publish_checkpoint(&published.result)?;
    let termination = control.wait_while_paused();
    let cleanup = cleanup_started_browser(browser);
    finalize_hosted_stop(job, config, redactor, artifacts, cleanup, termination)
        .and_then(|outcome| publish_terminal_checkpoint(control, journal, outcome))
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
    let mut checkpoint = run_result(
        job,
        config,
        redactor,
        state,
        reason,
        overall,
        artifacts.verdicts.clone(),
        artifacts.escalation.clone(),
        retained.clone(),
    )?;
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
        artifacts.trace.clone(),
        retained,
        checkpoint,
        exit_code,
    )
}

#[cfg(target_os = "linux")]
fn finalize_hosted_stop(
    job: &Job,
    config: &FlowConfig,
    redactor: &Redactor,
    artifacts: RunArtifacts,
    cleanup: Cleanup,
    termination: HostedTermination,
) -> Result<FlowOutcome, String> {
    let (state, code, final_exit) = hosted_termination_fields(termination);
    hosted_terminal_result(
        job, config, redactor, artifacts, cleanup, state, code, final_exit,
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
#[allow(clippy::too_many_arguments)]
fn hosted_terminal_result(
    job: &Job,
    config: &FlowConfig,
    redactor: &Redactor,
    artifacts: RunArtifacts,
    cleanup: Cleanup,
    state: RunState,
    code: &str,
    final_exit: u8,
) -> Result<FlowOutcome, String> {
    let final_result = run_result(
        job,
        config,
        redactor,
        state,
        Some(Reason {
            code: code.into(),
            details: BTreeMap::new(),
        }),
        VerdictResult::Unresolved,
        artifacts.verdicts,
        artifacts.escalation,
        cleanup.clone(),
    )?;
    replace_hosted_tail(config, &cleanup, &final_result, redactor).map(|()| FlowOutcome {
        result: final_result,
        exit_code: final_exit,
    })
}

#[cfg(target_os = "linux")]
fn replace_hosted_tail(
    config: &FlowConfig,
    cleanup: &Cleanup,
    final_result: &Value,
    redactor: &Redactor,
) -> Result<(), String> {
    evidence::replace_result_cleanup(
        &config.evidence_root,
        &config.run_id,
        cleanup,
        final_result,
        redactor,
    )
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
    trace: Vec<Value>,
    verdicts: Vec<StepVerdict>,
    stop: Option<Stop>,
    escalation: Option<Escalation>,
}

#[cfg(any(target_os = "linux", test))]
impl RunArtifacts {
    fn new(job: &Job, redactor: &Redactor) -> Self {
        Self {
            observations: Vec::new(),
            decisions: Vec::new(),
            steps: Vec::new(),
            escalations: Vec::new(),
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
            stop: None,
            escalation: None,
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
    artifacts
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
            "caller_assisted":false
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
        observation_number: 0,
        done_unknown_reobserved: false,
        operation_gate_reobserved: false,
        mutations: 0,
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
        let captured = match self.capture() {
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
            DoneResult::Unknown => self.unknown_done(done),
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

    fn unknown_done(&mut self, done: DoneResult) -> StepProgress {
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
                drop(permit);
                StepProgress::Stop(escalate(
                    self.artifacts,
                    self.redactor,
                    self.index,
                    self.step,
                    done,
                    Some(judgments),
                    "debug_forced_stop",
                ))
            }
            policy::Next::Mutate(permit) => self.mutate(done, captured, judgments, permit),
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
        result
            .map(|_fact| {
                self.mutations += 1;
                self.awaiting_final_done_reobservation = false;
                debug_assert_eq!(
                    self.artifacts
                        .trace
                        .last()
                        .and_then(|event| event.get("event")),
                    Some(&json!("action_fact"))
                );
                StepProgress::Continue
            })
            .unwrap_or_else(|stop| StepProgress::Stop(self.action_stop(done, judgments, stop)))
    }

    fn action_stop(
        &mut self,
        done: DoneResult,
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
                "action_outcome_uncertain",
            ),
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
            debug_assert_eq!(stop, actions::ActionStop::InvalidPermit);
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
            policy::PolicyStop::Uncertain(reason) => escalate(
                self.artifacts,
                self.redactor,
                self.index,
                self.step,
                done,
                Some(judgments),
                reason,
            ),
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
    }
}

#[cfg(any(target_os = "linux", test))]
fn escalate(
    artifacts: &mut RunArtifacts,
    redactor: &Redactor,
    index: usize,
    step: &manuvra_contract::Step,
    done: DoneResult,
    judgments: Option<&judgment::Judgments>,
    reason: &'static str,
) -> Stop {
    let id = "e_1".to_owned();
    let stopped = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .to_string();
    let candidates=judgments.map(|value|json!({"operation":value.operation,"click_target":value.click_target,"type_target":value.type_target,"type_value":value.type_value})).unwrap_or_else(||json!({}));
    let latest=artifacts.observations.last().map(|(name,_,png)|json!({"snapshot":format!("observations/{name}.json"),"screenshot":png.as_ref().map(|_|format!("observations/{name}.png"))})).unwrap_or(Value::Null);
    let decision = judgments.and_then(|_| {
        artifacts
            .decisions
            .last()
            .map(|(name, _)| format!("decisions/{name}.json"))
    });
    let payload = redacted_value(
        &json!({"id":id,"phase":"step","step_id":step.id,"step":{"goal":step.goal,"done_when":step.done_when},"done":done,"observation":latest,"decision":decision,"gate_reason":reason,"candidates":candidates,"permitted_mutations":["CLICK","TYPE_TEXT"],"recent_actions":artifacts.trace.iter().rev().filter(|event|event.get("event").and_then(Value::as_str).is_some_and(|event|event.starts_with("action_"))).take(8).collect::<Vec<_>>(),"stopped_at":stopped}),
        redactor,
    );
    artifacts.escalations.push((id.clone(), payload));
    artifacts.escalation = Some(Escalation {
        id,
        phase: "step".into(),
        step_id: Some(redactor.redact_export_text(&step.id)),
        expires_at: stopped,
        payload: "escalations/e_1.json".into(),
        dispositions: Vec::<DispositionKind>::new(),
    });
    artifacts.verdicts[index] = StepVerdict {
        id: redactor.redact_export_text(&step.id),
        result: VerdictResult::Unresolved,
        basis: None,
    };
    Stop::uncertain(reason, step_detail(redactor, step))
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
    let mut output = raw.clone();
    for text in [
        &mut output.document_id,
        &mut output.url,
        &mut output.route,
        &mut output.title,
        &mut output.visible_text,
        &mut output.covered_text,
    ] {
        *text = redactor.redact_external_text(text);
    }
    output.dialogs.iter_mut().for_each(|text| {
        *text = redactor.redact_external_text(text);
    });
    output.dialog_texts = output
        .dialog_texts
        .into_iter()
        .map(|(name, text)| {
            (
                redactor.redact_external_text(&name),
                redactor.redact_external_text(&text),
            )
        })
        .collect();
    for element in &mut output.elements {
        element.name = redactor.redact_external_text(&element.name);
        element.value = redactor.redact_external_text(&element.value);
        if let Some(dialog) = &mut element.in_dialog {
            *dialog = redactor.redact_external_text(dialog);
        }
    }
    serde_json::to_value(output).map_err(|error| error.to_string())
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
    use std::sync::Mutex;
    use tempfile::TempDir;

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
                .expect("scripted capture")
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
                    ("type_value".into(), choice("name")),
                    ("step_done".into(), manuvra_jev::Answer::Noul { noul: 0.01 }),
                ]),
                usage: BTreeMap::new(),
                request_id: Some("recorded-fake".into()),
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
                    ("type_value".into(), choice("name")),
                    ("step_done".into(), manuvra_jev::Answer::Noul { noul }),
                ]),
                usage: BTreeMap::new(),
                request_id: Some("recorded-natural-sequence".into()),
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

        fn wait_while_paused(&self) -> HostedTermination {
            HostedTermination::Aborted
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
            assert_eq!(driver.action_stop(done, &typed, stop).code, code);
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
        assert_eq!(artifacts.observations.len(), 3);
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
        assert_eq!(artifacts.observations.len(), 4);
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
