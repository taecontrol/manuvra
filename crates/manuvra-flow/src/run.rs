use crate::evidence::{self, EvidenceBundle, Redactor};
#[cfg(any(target_os = "linux", test))]
use crate::verification::{DoneResult, check_done};
#[cfg(target_os = "linux")]
use manuvra_chrome::{BrowserConfig, OwnedBrowser};
#[cfg(any(target_os = "linux", test))]
use manuvra_chrome::{BrowserError, CapturedPage, Observation};
#[cfg(any(target_os = "linux", test))]
use manuvra_contract::DoneCondition;
use manuvra_contract::{
    Cleanup, EvidenceRef, ExpectationVerdict, Job, Reason, RunResult, RunState, SchemaVersion,
    StepVerdict, Verdict, VerdictResult,
};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

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
fn run_linux(job: &Job, config: FlowConfig, redactor: &Redactor) -> Result<FlowOutcome, String> {
    let started = StartedBrowser::launch(browser_config(job, &config), target_url(job))
        .and_then(StartedBrowser::navigate);
    match started {
        Ok(started) => {
            finish_browser_run(job, config, redactor, started.browser, started.provenance)
        }
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
) -> Result<FlowOutcome, String> {
    let mut artifacts = drive_steps(job, redactor, &browser);
    let cleanup = cleanup_browser(&mut browser, redactor, &mut artifacts.stop);
    let (state, reason, exit_code, overall) = terminal_fields(artifacts.stop);
    let result = run_result(
        job,
        &config,
        redactor,
        state,
        reason,
        overall,
        artifacts.verdicts,
        cleanup.clone(),
    )?;
    publish_bundle(
        job,
        config,
        redactor,
        provenance,
        artifacts.observations,
        artifacts.steps,
        artifacts.trace,
        cleanup,
        result,
        exit_code,
    )
}

#[cfg(target_os = "linux")]
fn browser_config(job: &Job, config: &FlowConfig) -> BrowserConfig {
    let (width, height) = viewport(job);
    BrowserConfig {
        explicit_binary: config.browser.clone(),
        headless: config.headless,
        width,
        height,
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
struct RunArtifacts {
    observations: Vec<(String, Value, Option<Vec<u8>>)>,
    steps: Vec<(String, Value)>,
    trace: Vec<Value>,
    verdicts: Vec<StepVerdict>,
    stop: Option<Stop>,
}

#[cfg(any(target_os = "linux", test))]
impl RunArtifacts {
    fn new(job: &Job, redactor: &Redactor) -> Self {
        Self {
            observations: Vec::new(),
            steps: Vec::new(),
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
        }
    }
}

#[cfg(any(target_os = "linux", test))]
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
fn drive_steps(job: &Job, redactor: &Redactor, browser: &impl BrowserPage) -> RunArtifacts {
    let mut artifacts = RunArtifacts::new(job, redactor);
    for (index, step) in job.steps.iter().enumerate() {
        let outcome = evaluate_step(job, redactor, browser, index, step, &mut artifacts);
        if let Some(stop) = outcome {
            artifacts.stop = Some(stop);
            break;
        }
    }
    artifacts
}

#[cfg(any(target_os = "linux", test))]
fn evaluate_step(
    job: &Job,
    redactor: &Redactor,
    browser: &impl BrowserPage,
    index: usize,
    step: &manuvra_contract::Step,
    artifacts: &mut RunArtifacts,
) -> Option<Stop> {
    let DoneCondition::Structured(assertions) = &step.done_when else {
        unreachable!("admission rejected natural language")
    };
    let first = match capture_step(browser, redactor, index + 1, 1) {
        Ok(value) => value,
        Err(message) => return Some(control_stop(message)),
    };
    let mut done = check_done(assertions, &first.raw, &job.values);
    let mut redaction_verified = first.redaction_verified;
    record_capture(artifacts, redactor, step, &first, "observation", done);
    if done != DoneResult::Satisfied {
        let second = match capture_step(browser, redactor, index + 1, 2) {
            Ok(value) => value,
            Err(message) => return Some(control_stop(message)),
        };
        done = check_done(assertions, &second.raw, &job.values);
        redaction_verified &= second.redaction_verified;
        record_capture(artifacts, redactor, step, &second, "reobservation", done);
    }
    let exported_step_id = redactor.redact_export_text(&step.id);
    artifacts.steps.push((safe_name(index+1,&exported_step_id),json!({"id":exported_step_id,"done":done,"basis":"structured","mutation_limit_consumed":done==DoneResult::NotSatisfied,"redaction_verified":redaction_verified})));
    if !redaction_verified {
        return Some(Stop::blocked(
            "redaction_unverifiable",
            BTreeMap::from([(
                "step_id".into(),
                json!(redactor.redact_export_text(&step.id)),
            )]),
        ));
    }
    apply_done(redactor, index, step, done, &mut artifacts.verdicts)
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
fn apply_done(
    redactor: &Redactor,
    index: usize,
    step: &manuvra_contract::Step,
    done: DoneResult,
    verdicts: &mut [StepVerdict],
) -> Option<Stop> {
    let (result, stop) = match done {
        DoneResult::Satisfied => (VerdictResult::Satisfied, None),
        DoneResult::NotSatisfied => (
            VerdictResult::NotSatisfied,
            Some(Stop {
                state: RunState::Failed,
                code: "done_condition_not_met",
                exit_code: 4,
                details: BTreeMap::from([
                    (
                        "step_id".into(),
                        json!(redactor.redact_export_text(&step.id)),
                    ),
                    ("mutation_limit_consumed".into(), json!(true)),
                ]),
            }),
        ),
        DoneResult::Unknown => (
            VerdictResult::Unresolved,
            Some(Stop::blocked(
                "observation_unknown",
                BTreeMap::from([(
                    "step_id".into(),
                    json!(redactor.redact_export_text(&step.id)),
                )]),
            )),
        ),
    };
    verdicts[index] = StepVerdict {
        id: redactor.redact_export_text(&step.id),
        result,
        basis: Some("structured".into()),
    };
    stop
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
    browser: &impl BrowserPage,
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
    browser: &impl BrowserPage,
    redactor: &Redactor,
    name: String,
) -> Result<Captured, String> {
    let raw = browser.observe_page().map_err(|e| e.to_string())?;
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
        cleanup.clone(),
    )?;
    publish_bundle(
        job,
        config,
        redactor,
        provenance,
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
        cleanup.clone(),
    )?;
    publish_bundle(
        job,
        config,
        redactor,
        json!({"browser_path":null,"browser_version":null,"viewport":null,"display_mode":null}),
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
    cleanup: Cleanup,
) -> Result<Value, String> {
    let manifest = config
        .evidence_root
        .join(&config.run_id)
        .join("manifest.json");
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
        escalation: None,
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
    steps: Vec<(String, Value)>,
    trace: Vec<Value>,
    cleanup: Cleanup,
    result: Value,
    exit_code: u8,
) -> Result<FlowOutcome, String> {
    redact_provenance(&mut provenance, redactor);
    let bundle = EvidenceBundle {
        job: evidence::redacted_job(job, redactor)?,
        provenance,
        observations,
        steps,
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
    use manuvra_chrome::{Coverage, RedactionProof, Screenshot, ViewportState};
    use serde_json::json;
    use std::collections::{BTreeMap, VecDeque};
    use std::sync::Mutex;
    use tempfile::TempDir;

    struct FakeBrowser {
        captures: Mutex<VecDeque<Result<CapturedPage, BrowserError>>>,
        fallback: Observation,
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

    fn job(wanted: &str) -> Job {
        Job::parse(serde_json::to_string(&json!({"schema_version":1,"target":{"kind":"browser","url":"http://127.0.0.1:4351/"},"context":{"journey":"observe","revision":"fixture","environment":"fake","actor":"synthetic","authority":"observe"},"steps":[{"id":"ready","goal":"observe","done_when":[{"text_visible":wanted}]}]})).unwrap().as_bytes()).unwrap()
    }

    #[test]
    fn scripted_browser_proves_satisfied_and_failed_done_paths() {
        let yes = job("Ready");
        let redactor = Redactor::for_job(&yes).unwrap();
        let fake = FakeBrowser {
            captures: Mutex::new(VecDeque::from([page("Ready")])),
            fallback: observed("Ready"),
        };
        let passed = drive_steps(&yes, &redactor, &fake);
        assert!(passed.stop.is_none());
        assert_eq!(passed.verdicts[0].result, VerdictResult::Satisfied);
        let no = job("Ready");
        let fake = FakeBrowser {
            captures: Mutex::new(VecDeque::from([page("Not yet"), page("Still not")])),
            fallback: observed("Still not"),
        };
        let failed = drive_steps(&no, &redactor, &fake);
        assert_eq!(failed.stop.as_ref().unwrap().code, "done_condition_not_met");
        assert_eq!(failed.observations.len(), 2);
        let (state, reason, exit_code, overall) = terminal_fields(failed.stop);
        assert_eq!(state, RunState::Failed);
        assert_eq!(reason.unwrap().code, "done_condition_not_met");
        assert_eq!(exit_code, 4);
        assert_eq!(overall, VerdictResult::NotSatisfied);
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
        };
        assert_eq!(
            drive_steps(&job, &redactor, &masked).stop.unwrap().code,
            "redaction_unverifiable"
        );
        let broken = FakeBrowser {
            captures: Mutex::new(VecDeque::from([Err(BrowserError::Control(
                "marker".into(),
            ))])),
            fallback: observed("Ready"),
        };
        assert_eq!(
            drive_steps(&job, &redactor, &broken).stop.unwrap().code,
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
}
