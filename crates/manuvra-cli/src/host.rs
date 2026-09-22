#![cfg(any(target_os = "linux", target_os = "macos"))]

use crate::control_socket::{AuthenticatedListener, read_frame};
use crate::process::{HostBootstrap, IPC_VERSION, now_unix_ms, process_identity};
use crate::store::{self, RunControl};
use manuvra_contract::{DispositionRequest, SchemaVersion, VerdictResult};
use manuvra_flow::InputCancellation;
use manuvra_flow::run::{HostedControl, HostedEvent, HostedTermination};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::fs;
use std::io::{Read, Write};
use std::os::fd::FromRawFd;
use std::os::unix::net::UnixStream;
#[cfg(debug_assertions)]
use std::os::unix::process::CommandExt;
use std::path::Path;
#[cfg(debug_assertions)]
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

#[derive(Clone, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum Request {
    Ready {
        ipc_version: u16,
    },
    Status {
        ipc_version: u16,
    },
    Abort {
        ipc_version: u16,
        run_id: String,
        job_digest: String,
    },
    Deadline {
        ipc_version: u16,
        run_id: String,
        job_digest: String,
    },
    Resume {
        ipc_version: u16,
        run_id: String,
        job_digest: String,
        request_id: String,
        request_digest: String,
        request: DispositionRequest,
    },
}

#[derive(Serialize)]
struct Response<'a> {
    ipc_version: u16,
    run_id: &'a str,
    job_digest: &'a str,
    accepted: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    result: Option<&'a Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error_code: Option<&'a str>,
}

struct Control {
    state_root: std::path::PathBuf,
    run: Mutex<RunControl>,
    cancellation: InputCancellation,
    abort: AtomicBool,
    ready: AtomicBool,
    watchdog_lost: AtomicBool,
    resume: Mutex<ResumeAdmission>,
    pause_timeout_ms: u64,
}

#[derive(Default)]
struct ResumeAdmission {
    pending: Option<PendingResume>,
    awaiting_checkpoint: Option<ResumePublication>,
    consumed: BTreeMap<String, ResumeReceipt>,
}

struct PendingResume {
    request: DispositionRequest,
    receipt: ResumeReceipt,
}

#[derive(Clone)]
struct ResumeReceipt {
    request_id: String,
    request_digest: String,
}

struct ResumePublication {
    receipt: ResumeReceipt,
    response: Option<Value>,
}

impl HostedControl for Control {
    fn cancellation(&self) -> InputCancellation {
        self.cancellation.clone()
    }

    fn pause_deadline_unix_ms(&self) -> u64 {
        let mut run = self
            .run
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(deadline) = run.pause_deadline_unix_ms {
            return deadline;
        }
        let deadline = now_unix_ms()
            .saturating_add(self.pause_timeout_ms)
            .min(run.lifetime_deadline_unix_ms);
        run.pause_deadline_unix_ms = Some(deadline);
        deadline
    }

    fn publish_checkpoint(&self, result: &Value) -> Result<(), String> {
        let mut run = self
            .run
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        run.sequence = run.sequence.saturating_add(1);
        run.result = result.clone();
        update_pause_deadline(&mut run, result, self.pause_timeout_ms);
        publish_resume_response(self, &run, result)?;
        store::write_run_control(&self.state_root, &run)
    }

    fn wait_while_paused(&self, escalation_id: &str) -> HostedEvent {
        loop {
            if let Some(termination) = self.paused_termination() {
                return HostedEvent::Termination(termination);
            }
            let mut admission = self
                .resume
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if admission
                .pending
                .as_ref()
                .is_some_and(|pending| pending.request.escalation_id == escalation_id)
            {
                let pending = admission.pending.take().expect("checked pending resume");
                admission.awaiting_checkpoint = Some(ResumePublication {
                    receipt: pending.receipt,
                    response: None,
                });
                return HostedEvent::Disposition(pending.request);
            }
            drop(admission);
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    fn termination(&self) -> Option<HostedTermination> {
        self.paused_termination()
    }
}

fn publish_resume_response(
    control: &Control,
    run: &RunControl,
    result: &Value,
) -> Result<(), String> {
    publish_resume_response_with(
        control,
        result,
        |receipt, result, exit_code| {
            store::sealed_control_record(
                &control.state_root,
                &receipt.request_id,
                &run.run_id,
                &receipt.request_digest,
                exit_code,
                result,
            )
        },
        |receipt, record| {
            store::finalize_control_request(&control.state_root, &receipt.request_id, record)
        },
    )
}

fn publish_resume_response_with(
    control: &Control,
    result: &Value,
    seal: impl FnOnce(&ResumeReceipt, Value, u8) -> Result<store::RequestRecord, String>,
    finalize: impl FnOnce(&ResumeReceipt, &store::RequestRecord) -> Result<(), String>,
) -> Result<(), String> {
    let mut admission = control
        .resume
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let Some(publication) = admission.awaiting_checkpoint.as_mut() else {
        return Ok(());
    };
    let frozen = publication.response.get_or_insert_with(|| result.clone());
    let exit_code = crate::result_exit_code(frozen)?;
    let record = seal(&publication.receipt, frozen.clone(), exit_code)?;
    finalize(&publication.receipt, &record)?;
    admission.awaiting_checkpoint = None;
    Ok(())
}

fn update_pause_deadline(run: &mut RunControl, result: &Value, pause_timeout_ms: u64) {
    let paused = result.get("state").and_then(Value::as_str) == Some("uncertain")
        && result.get("terminal").and_then(Value::as_bool) == Some(false);
    if !paused {
        run.pause_deadline_unix_ms = None;
    } else if run.pause_deadline_unix_ms.is_none() {
        run.pause_deadline_unix_ms = Some(
            now_unix_ms()
                .saturating_add(pause_timeout_ms)
                .min(run.lifetime_deadline_unix_ms),
        );
    }
}

impl Control {
    fn deadline_termination(&self) -> Option<HostedTermination> {
        let run = self
            .run
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        deadline_termination(
            now_unix_ms(),
            run.pause_deadline_unix_ms,
            run.lifetime_deadline_unix_ms,
        )
    }

    fn paused_termination(&self) -> Option<HostedTermination> {
        if self.watchdog_lost.load(Ordering::SeqCst) {
            return Some(HostedTermination::WatchdogLost);
        }
        if self.abort.load(Ordering::SeqCst) {
            return Some(HostedTermination::Aborted);
        }
        self.deadline_termination()
    }
}

fn deadline_termination(
    now: u64,
    pause_deadline: Option<u64>,
    lifetime_deadline: u64,
) -> Option<HostedTermination> {
    if now >= lifetime_deadline {
        Some(HostedTermination::LifetimeElapsed)
    } else if pause_deadline.is_some_and(|deadline| now >= deadline) {
        Some(HostedTermination::PauseDeadlineElapsed)
    } else {
        None
    }
}

pub fn main() -> Result<(), String> {
    let mut bootstrap: HostBootstrap = crate::process::read_bootstrap()?;
    bootstrap.liveness_fd = std::env::var("MANUVRA_LIVENESS_FD")
        .ok()
        .and_then(|value| value.parse().ok())
        .or(bootstrap.liveness_fd);
    bootstrap_fault("before_lock");
    let run_lock_fd = std::env::var("MANUVRA_RUN_LOCK_FD")
        .map_err(|_| "host inherited no run lock descriptor".to_owned())?
        .parse()
        .map_err(|_| "host inherited an invalid run lock descriptor".to_owned())?;
    let _lock = store::adopt_inherited_run_lock(
        &bootstrap.state_root,
        &bootstrap.intent.run_id,
        run_lock_fd,
    )?;
    bootstrap_fault("after_lock");
    run_bootstrap(bootstrap)
}

fn run_bootstrap(bootstrap: HostBootstrap) -> Result<(), String> {
    let redactor = manuvra_flow::evidence::Redactor::for_job_with_provider_key(
        &bootstrap.job,
        bootstrap.provider_key.as_deref(),
    )?;
    let socket = bootstrap.runtime_dir.join("control.sock");
    let listener = bind_private_socket(&socket)?;
    bootstrap_fault("after_socket");
    let control = make_control(&bootstrap, &socket, &redactor)?;
    publish_initial_control(&bootstrap, &control)?;
    bootstrap_fault("after_control");
    monitor_watchdog(bootstrap.liveness_fd, control.clone());
    let socket_stop = Arc::new(AtomicBool::new(false));
    serve(listener, control.clone(), socket_stop.clone());
    wait_for_client_readiness(&control);
    continue_host_run(bootstrap, redactor, control, socket, socket_stop)
}

#[cfg(debug_assertions)]
fn bootstrap_fault(point: &str) {
    if std::env::var("MANUVRA_TEST_HOST_BOOTSTRAP_FAULT").as_deref() == Ok(point) {
        kill_fault_host();
    }
}

#[cfg(not(debug_assertions))]
fn bootstrap_fault(_: &str) {}

fn wait_for_client_readiness(control: &Control) {
    while !control.ready.load(Ordering::SeqCst) && control.termination().is_none() {
        std::thread::sleep(Duration::from_millis(5));
    }
}

fn continue_host_run(
    bootstrap: HostBootstrap,
    redactor: manuvra_flow::evidence::Redactor,
    control: Arc<Control>,
    socket: std::path::PathBuf,
    socket_stop: Arc<AtomicBool>,
) -> Result<(), String> {
    if run_fault_fixture(&bootstrap, control.as_ref())? {
        socket_stop.store(true, Ordering::SeqCst);
        let _ = fs::remove_file(&socket);
        return Ok(());
    }
    let outcome = execute_flow(&bootstrap, &redactor, control.as_ref())?;
    finalize_flow(bootstrap, control.as_ref(), outcome, &socket, &socket_stop)
}

#[cfg(debug_assertions)]
fn run_fault_fixture(bootstrap: &HostBootstrap, control: &Control) -> Result<bool, String> {
    let Some(scenario) = std::env::var_os("MANUVRA_TEST_HOST_FAULT") else {
        return Ok(false);
    };
    let mut browser = spawn_fake_browser(&bootstrap.runtime_dir)?;
    fault_handler(&scenario.to_string_lossy())
        .and_then(|handler| handler(bootstrap, control, &mut browser))
}

#[cfg(debug_assertions)]
type FaultHandler = fn(&HostBootstrap, &Control, &mut Child) -> Result<bool, String>;

#[cfg(debug_assertions)]
fn fault_handler(name: &str) -> Result<FaultHandler, String> {
    const HANDLERS: [(&str, FaultHandler); 7] = [
        ("before_action_prepared", fault_before_prepared),
        ("after_action_prepared", fault_after_prepared),
        ("after_dispatch_before_receipt", fault_after_dispatch),
        ("pause_hang", fault_pause_hang),
        ("pause_abort", fault_pause_abort),
        ("watchdog_lost", fault_watchdog_lost),
        ("terminal_hang", fault_terminal_hang),
    ];
    HANDLERS
        .into_iter()
        .find(|(scenario, _)| *scenario == name)
        .map(|(_, handler)| handler)
        .ok_or_else(|| "unknown MANUVRA_TEST_HOST_FAULT scenario".into())
}

#[cfg(debug_assertions)]
fn fault_before_prepared(
    bootstrap: &HostBootstrap,
    _: &Control,
    browser: &mut Child,
) -> Result<bool, String> {
    run_fault_action_at(bootstrap, browser, FaultActionBoundary::BeforePrepared)
}

#[cfg(debug_assertions)]
fn fault_after_prepared(
    bootstrap: &HostBootstrap,
    _: &Control,
    browser: &mut Child,
) -> Result<bool, String> {
    run_fault_action_at(bootstrap, browser, FaultActionBoundary::AfterPrepared)
}

#[cfg(debug_assertions)]
fn fault_after_dispatch(
    bootstrap: &HostBootstrap,
    _: &Control,
    browser: &mut Child,
) -> Result<bool, String> {
    run_fault_action_at(bootstrap, browser, FaultActionBoundary::AfterDispatch)
}

#[cfg(debug_assertions)]
fn fault_pause_hang(
    bootstrap: &HostBootstrap,
    control: &Control,
    _: &mut Child,
) -> Result<bool, String> {
    publish_fault_pause(bootstrap, control).map(|()| {
        loop {
            std::thread::park();
        }
    })
}

#[cfg(debug_assertions)]
fn fault_pause_abort(
    bootstrap: &HostBootstrap,
    control: &Control,
    browser: &mut Child,
) -> Result<bool, String> {
    finish_pause_abort(bootstrap, control, browser)
}

#[cfg(debug_assertions)]
fn fault_watchdog_lost(
    bootstrap: &HostBootstrap,
    control: &Control,
    browser: &mut Child,
) -> Result<bool, String> {
    finish_watchdog_loss(bootstrap, control, browser)
}

#[cfg(debug_assertions)]
fn fault_terminal_hang(
    bootstrap: &HostBootstrap,
    control: &Control,
    _: &mut Child,
) -> Result<bool, String> {
    publish_terminal_hang(bootstrap, control)?;
    park_forever()
}

#[cfg(debug_assertions)]
fn publish_terminal_hang(bootstrap: &HostBootstrap, control: &Control) -> Result<(), String> {
    let mut result = fault_control_result(control);
    apply_terminal_hang_result(&mut result);
    finish_fault_evidence(bootstrap, &mut result)?;
    control.publish_checkpoint(&result)
}

#[cfg(debug_assertions)]
fn fault_control_result(control: &Control) -> Value {
    control
        .run
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .result
        .clone()
}

#[cfg(debug_assertions)]
fn apply_terminal_hang_result(result: &mut Value) {
    result["state"] = json!("blocked");
    result["terminal"] = json!(true);
    result["reason"] = json!({"code":"terminal_checkpoint_fixture"});
    result["evidence"]["complete"] = json!(true);
    result["cleanup"] = json!({
        "browser":"closure_pending",
        "profile":"removal_pending",
        "application_state":"caller_owned"
    });
}

#[cfg(debug_assertions)]
fn park_forever() -> ! {
    loop {
        std::thread::park();
    }
}

#[cfg(not(debug_assertions))]
fn run_fault_fixture(_bootstrap: &HostBootstrap, _control: &Control) -> Result<bool, String> {
    Ok(false)
}

#[cfg(debug_assertions)]
fn spawn_fake_browser(runtime_dir: &Path) -> Result<Child, String> {
    let log = runtime_dir.join("fake-browser.dispatch.log");
    let child_pid = runtime_dir.join("fake-browser-child.pid");
    let mut command = Command::new("sh");
    command
        .arg("-c")
        .arg("sleep 60 & child=$!; trap 'kill \"$child\" 2>/dev/null || true; wait \"$child\" 2>/dev/null || true' EXIT TERM; printf '%s' \"$child\" > \"$2\"; while IFS= read -r line; do printf '%s\\n' \"$line\" >> \"$1\"; done")
        .arg("fake-browser")
        .arg(log)
        .arg(child_pid)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    unsafe {
        command.pre_exec(|| {
            #[cfg(target_os = "linux")]
            if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL) == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let child = command.spawn().map_err(|error| error.to_string())?;
    fs::write(runtime_dir.join("fake-browser.pid"), child.id().to_string())
        .map_err(|error| error.to_string())?;
    let deadline = std::time::Instant::now() + Duration::from_secs(1);
    while !runtime_dir.join("fake-browser-child.pid").is_file() {
        if std::time::Instant::now() >= deadline {
            return Err("fake browser descendant did not start".into());
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    Ok(child)
}

#[cfg(debug_assertions)]
fn dispatch_fake_browser(browser: &mut Child, runtime_dir: &Path) -> Result<(), String> {
    browser
        .stdin
        .as_mut()
        .ok_or_else(|| "fake browser input pipe is unavailable".to_owned())?
        .write_all(b"dispatch click submit\n")
        .map_err(|error| error.to_string())?;
    let log = runtime_dir.join("fake-browser.dispatch.log");
    let deadline = std::time::Instant::now() + Duration::from_secs(1);
    while fs::read_to_string(&log)
        .map(|contents| !contents.contains("dispatch click submit"))
        .unwrap_or(true)
    {
        if std::time::Instant::now() >= deadline {
            return Err("fake browser did not persist dispatch".into());
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    Ok(())
}

#[cfg(debug_assertions)]
struct FaultActionJournal {
    inner: manuvra_flow::actions::DurableJournal,
    boundary: FaultActionBoundary,
}

#[cfg(debug_assertions)]
#[derive(Clone, Copy)]
enum FaultActionBoundary {
    BeforePrepared,
    AfterPrepared,
    AfterDispatch,
}

#[cfg(debug_assertions)]
impl FaultActionBoundary {
    fn before_append(self, prepared: bool) {
        if matches!((prepared, self), (true, Self::BeforePrepared)) {
            kill_fault_host();
        }
    }

    fn after_append(self, prepared: bool) {
        if matches!((prepared, self), (true, Self::AfterPrepared)) {
            kill_fault_host();
        }
    }

    fn after_dispatch(self) {
        if matches!(self, Self::AfterDispatch) {
            kill_fault_host();
        }
    }
}

#[cfg(debug_assertions)]
impl manuvra_flow::actions::ActionJournal for FaultActionJournal {
    fn append(&mut self, value: &Value) -> Result<(), String> {
        let prepared = value.get("event").and_then(Value::as_str) == Some("action_prepared");
        self.boundary.before_append(prepared);
        manuvra_flow::actions::ActionJournal::append(&mut self.inner, value)?;
        self.boundary.after_append(prepared);
        Ok(())
    }

    fn entries(&self) -> &[Value] {
        self.inner.entries()
    }
}

#[cfg(debug_assertions)]
struct FaultActionPerformer<'a> {
    browser: Mutex<&'a mut Child>,
    runtime_dir: &'a Path,
    boundary: FaultActionBoundary,
}

#[cfg(debug_assertions)]
impl manuvra_flow::actions::Performer for FaultActionPerformer<'_> {
    fn dispatch(
        &self,
        _: manuvra_flow::PreparedInput,
        _: &manuvra_flow::InputCancellation,
    ) -> Result<manuvra_flow::PerformFact, manuvra_flow::PerformError> {
        dispatch_fake_browser(
            &mut self
                .browser
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
            self.runtime_dir,
        )
        .map_err(manuvra_flow::PerformError::Uncertain)?;
        self.boundary.after_dispatch();
        Ok(manuvra_flow::PerformFact {
            readback: None,
            readback_matches: None,
            suboperations: vec![],
        })
    }
}

#[cfg(debug_assertions)]
fn run_fault_action_at(
    bootstrap: &HostBootstrap,
    browser: &mut Child,
    boundary: FaultActionBoundary,
) -> Result<bool, String> {
    let (observation, permit, inner) = fault_action_resources(bootstrap)?;
    let mut journal = FaultActionJournal { inner, boundary };
    let performer = FaultActionPerformer {
        browser: Mutex::new(browser),
        runtime_dir: &bootstrap.runtime_dir,
        boundary,
    };
    let values = manuvra_flow::values::Values::new(&bootstrap.job);
    let cancellation = manuvra_flow::InputCancellation::default();
    let _ = manuvra_flow::actions::perform(
        permit,
        &performer,
        &observation,
        &values,
        &mut journal,
        &cancellation,
    );
    Err("fault action boundary did not terminate the host".into())
}

#[cfg(debug_assertions)]
fn fault_action_resources(
    bootstrap: &HostBootstrap,
) -> Result<
    (
        manuvra_flow::Observation,
        manuvra_flow::policy::Permit,
        manuvra_flow::actions::DurableJournal,
    ),
    String,
> {
    let observation = fault_observation();
    fault_permit(bootstrap, &observation).and_then(|permit| {
        fault_action_journal(bootstrap).map(|journal| (observation, permit, journal))
    })
}

#[cfg(debug_assertions)]
fn fault_permit(
    bootstrap: &HostBootstrap,
    observation: &manuvra_flow::Observation,
) -> Result<manuvra_flow::policy::Permit, String> {
    let mut policy =
        manuvra_flow::policy::Policy::new(&bootstrap.job.options, "http://127.0.0.1:4351/");
    if let manuvra_flow::policy::Next::Mutate(permit) = policy.decide(
        &bootstrap.job.steps[0],
        observation,
        &fault_judgments(),
        manuvra_flow::verification::DoneResult::NotSatisfied,
        false,
        false,
    ) {
        Ok(*permit)
    } else {
        Err("fault action policy did not mint a permit".into())
    }
}

#[cfg(debug_assertions)]
fn fault_action_journal(
    bootstrap: &HostBootstrap,
) -> Result<manuvra_flow::actions::DurableJournal, String> {
    manuvra_flow::evidence::Redactor::for_job_with_provider_key(
        &bootstrap.job,
        bootstrap.provider_key.as_deref(),
    )
    .and_then(|redactor| {
        manuvra_flow::actions::DurableJournal::open(
            &bootstrap.intent.evidence_root,
            &bootstrap.intent.run_id,
            &redactor,
        )
    })
}

#[cfg(debug_assertions)]
fn fault_observation() -> manuvra_flow::Observation {
    manuvra_flow::Observation {
        document_id: "fault-document".into(),
        url: "http://127.0.0.1:4351/".into(),
        route: "/".into(),
        title: "Fault fixture".into(),
        dialogs: Vec::new(),
        focused: None,
        visible_text: "Submit".into(),
        covered_text: String::new(),
        dialog_texts: BTreeMap::new(),
        elements: vec![manuvra_flow::Element {
            index: 1,
            node_id: 1,
            context: "main".into(),
            role: "button".into(),
            name: "Submit".into(),
            input_type: None,
            value: String::new(),
            checked: None,
            selected: None,
            expanded: None,
            disabled: false,
            in_dialog: None,
            operations: vec!["CLICK".into()],
            select_options: vec![],
            rect: manuvra_flow::Rect {
                x: 1.0,
                y: 1.0,
                width: 10.0,
                height: 10.0,
            },
        }],
        viewport: manuvra_flow::ViewportState {
            width: 100,
            height: 100,
            scroll_x: 0.0,
            scroll_y: 0.0,
            document_height: 100.0,
        },
        coverage: manuvra_flow::Coverage::default(),
    }
}

#[cfg(debug_assertions)]
fn fault_judgments() -> manuvra_flow::judgment::Judgments {
    let choice = |selected: &str| manuvra_flow::judgment::ChoiceJudgment {
        choice: selected.into(),
        probabilities: BTreeMap::from([(selected.into(), 1.0)]),
        confidence: 1.0,
    };
    manuvra_flow::judgment::Judgments {
        operation: choice("CLICK"),
        click_target: choice("1"),
        type_target: choice("NO_TYPE_TEXT_TARGET"),
        select_target: choice("NO_SELECT_TARGET"),
        type_value: choice("NONE_FITS"),
        step_done: 0.0,
        usage: BTreeMap::new(),
        request_id: None,
        model: "fault-fixture".into(),
        request: Value::Null,
    }
}

#[cfg(debug_assertions)]
fn publish_fault_pause(bootstrap: &HostBootstrap, control: &Control) -> Result<(), String> {
    let deadline = control.pause_deadline_unix_ms();
    let mut result = control
        .run
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .result
        .clone();
    result["state"] = json!("uncertain");
    result["terminal"] = json!(false);
    result["reason"] = json!({"code":"debug_force_stop"});
    result["escalation"] = json!({"expires_at":deadline.to_string()});
    result["cleanup"] =
        json!({"browser":"alive","profile":"retained","application_state":"caller_owned"});
    result["evidence"] = json!({
        "manifest":bootstrap.intent.evidence_root.join(&bootstrap.intent.run_id).join("manifest.json"),
        "complete":true
    });
    let redactor = manuvra_flow::evidence::Redactor::for_job_with_provider_key(
        &bootstrap.job,
        bootstrap.provider_key.as_deref(),
    )?;
    let cleanup =
        json!({"browser":"alive","profile":"retained","application_state":"caller_owned"});
    manuvra_flow::evidence::publish(
        &bootstrap.intent.evidence_root,
        &bootstrap.intent.run_id,
        manuvra_flow::evidence::EvidenceBundle {
            complete: true,
            job: manuvra_flow::evidence::redacted_job(&bootstrap.job, &redactor)?,
            provenance: json!({"fixture":"hosted_pause"}),
            observations: Vec::new(),
            decisions: Vec::new(),
            steps: Vec::new(),
            escalations: Vec::new(),
            dispositions: Vec::new(),
            verification: None,
            trace: Vec::new(),
            cleanup,
            result: result.clone(),
        },
        &redactor,
    )?;
    control.publish_checkpoint(&result)
}

#[cfg(debug_assertions)]
fn finish_watchdog_loss(
    bootstrap: &HostBootstrap,
    control: &Control,
    browser: &mut Child,
) -> Result<bool, String> {
    if !matches!(
        control.wait_while_paused("e_fault"),
        HostedEvent::Termination(HostedTermination::WatchdogLost)
    ) {
        return Err("fault fixture ended without watchdog loss".into());
    }
    close_fake_browser(browser);
    let mut result = control
        .run
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .result
        .clone();
    result["state"] = json!("blocked");
    result["terminal"] = json!(true);
    result["reason"] = json!({"code":"watchdog_lost"});
    result["evidence"]["complete"] = json!(true);
    result["cleanup"] = json!({"browser":"closed","profile":"removal_unconfirmed","application_state":"caller_owned"});
    finish_fault_evidence(bootstrap, &mut result)?;
    control.publish_checkpoint(&result).map(|()| true)
}

#[cfg(debug_assertions)]
fn finish_pause_abort(
    bootstrap: &HostBootstrap,
    control: &Control,
    browser: &mut Child,
) -> Result<bool, String> {
    publish_fault_pause(bootstrap, control)?;
    if !matches!(
        control.wait_while_paused("e_fault"),
        HostedEvent::Termination(HostedTermination::Aborted)
    ) {
        return Err("fault fixture ended without caller abort".into());
    }
    close_fake_browser(browser);
    let mut result = control
        .run
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .result
        .clone();
    result["state"] = json!("aborted");
    result["terminal"] = json!(true);
    result["reason"] = json!({"code":"caller_aborted"});
    result["evidence"]["complete"] = json!(true);
    result["cleanup"] = json!({"browser":"closed","profile":"removal_unconfirmed","application_state":"caller_owned"});
    finish_fault_evidence(bootstrap, &mut result)?;
    control.publish_checkpoint(&result).map(|()| true)
}

#[cfg(debug_assertions)]
fn finish_fault_evidence(bootstrap: &HostBootstrap, result: &mut Value) -> Result<(), String> {
    let redactor = manuvra_flow::evidence::Redactor::for_job_with_provider_key(
        &bootstrap.job,
        bootstrap.provider_key.as_deref(),
    )?;
    let cleanup = result["cleanup"].clone();
    let manifest = bootstrap
        .intent
        .evidence_root
        .join(&bootstrap.intent.run_id)
        .join("manifest.json");
    let published = if manifest.is_file() {
        manuvra_flow::evidence::replace_result_cleanup(
            &bootstrap.intent.evidence_root,
            &bootstrap.intent.run_id,
            &cleanup,
            result,
            &redactor,
        )
    } else {
        manuvra_flow::evidence::publish(
            &bootstrap.intent.evidence_root,
            &bootstrap.intent.run_id,
            manuvra_flow::evidence::EvidenceBundle {
                complete: true,
                job: manuvra_flow::evidence::redacted_job(&bootstrap.job, &redactor)?,
                provenance: json!({"fixture":"watchdog_loss"}),
                observations: Vec::new(),
                decisions: Vec::new(),
                steps: Vec::new(),
                escalations: Vec::new(),
                dispositions: Vec::new(),
                verification: None,
                trace: Vec::new(),
                cleanup,
                result: result.clone(),
            },
            &redactor,
        )
        .map(|_| ())
    };
    if let Err(error) = published {
        result["evidence"]["complete"] = json!(false);
        return Err(error);
    }
    Ok(())
}

#[cfg(debug_assertions)]
fn close_fake_browser(browser: &mut Child) {
    drop(browser.stdin.take());
    let _ = browser.wait();
}

#[cfg(debug_assertions)]
fn kill_fault_host() -> ! {
    unsafe {
        libc::kill(libc::getpid(), libc::SIGKILL);
    }
    std::process::abort()
}

fn make_control(
    bootstrap: &HostBootstrap,
    socket: &Path,
    redactor: &manuvra_flow::evidence::Redactor,
) -> Result<Arc<Control>, String> {
    Ok(Arc::new(Control {
        state_root: bootstrap.state_root.clone(),
        run: Mutex::new(RunControl {
            schema_version: SchemaVersion,
            ipc_version: IPC_VERSION,
            sequence: 1,
            run_id: bootstrap.intent.run_id.clone(),
            request_id: bootstrap.intent.public_request_id.clone(),
            job_digest: bootstrap.intent.job_digest.clone(),
            evidence_root: bootstrap.intent.evidence_root.clone(),
            started_unix_ms: bootstrap.started_unix_ms,
            lifetime_deadline_unix_ms: bootstrap.lifetime_deadline_unix_ms,
            pause_deadline_unix_ms: None,
            host: Some(process_identity(std::process::id())?),
            watchdog: bootstrap.watchdog.clone(),
            socket: socket.to_path_buf(),
            result: running_result(bootstrap, redactor)?,
        }),
        cancellation: InputCancellation::default(),
        abort: AtomicBool::new(false),
        ready: AtomicBool::new(false),
        watchdog_lost: AtomicBool::new(false),
        resume: Mutex::new(ResumeAdmission::default()),
        pause_timeout_ms: bootstrap.pause_timeout_ms,
    }))
}

fn publish_initial_control(bootstrap: &HostBootstrap, control: &Control) -> Result<(), String> {
    let run = control
        .run
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    store::write_run_control(&bootstrap.state_root, &run)
}

fn execute_flow(
    bootstrap: &HostBootstrap,
    redactor: &manuvra_flow::evidence::Redactor,
    control: &Control,
) -> Result<manuvra_flow::FlowOutcome, String> {
    manuvra_flow::run::run_hosted(
        &bootstrap.job,
        manuvra_flow::FlowConfig {
            request_id: bootstrap.intent.public_request_id.clone(),
            run_id: bootstrap.intent.run_id.clone(),
            evidence_root: bootstrap.intent.evidence_root.clone(),
            browser: bootstrap.browser.clone(),
            headless: bootstrap.headless,
        },
        redactor,
        bootstrap.provider_key.clone(),
        control,
    )
}

fn finalize_flow(
    bootstrap: HostBootstrap,
    control: &Control,
    outcome: manuvra_flow::FlowOutcome,
    socket: &Path,
    socket_stop: &AtomicBool,
) -> Result<(), String> {
    if outcome.result.get("terminal").and_then(Value::as_bool) == Some(true) {
        control.publish_checkpoint(&outcome.result)?;
    }
    let record = store::RequestRecord {
        schema_version: SchemaVersion,
        public_request_id: bootstrap.intent.public_request_id,
        run_id: bootstrap.intent.run_id.clone(),
        job_digest: bootstrap.intent.job_digest,
        result_digest: None,
        exit_code: outcome.exit_code,
        result: outcome.result,
    };
    store::finalize_request(&bootstrap.state_root, &bootstrap.lookup_request_id, &record)?;
    socket_stop.store(true, Ordering::SeqCst);
    let _ = fs::remove_file(socket);
    Ok(())
}

fn running_result(
    bootstrap: &HostBootstrap,
    redactor: &manuvra_flow::evidence::Redactor,
) -> Result<Value, String> {
    let manifest = bootstrap
        .intent
        .evidence_root
        .join(&bootstrap.intent.run_id)
        .join("manifest.json");
    let manifest = if manifest.is_absolute() {
        manifest
    } else {
        std::env::current_dir()
            .map_err(|error| error.to_string())?
            .join(manifest)
    };
    Ok(json!({
        "schema_version": 1,
        "request_id": bootstrap.intent.public_request_id,
        "run_id": bootstrap.intent.run_id,
        "state": "running",
        "terminal": false,
        "reason": null,
        "verdict": {
            "overall": VerdictResult::Unresolved,
            "steps": bootstrap.job.steps.iter().enumerate().map(|(index,step)| json!({"id":redactor.redact_export_text(&step.id),"result":if index == 0 {"unresolved"} else {"not_run"}})).collect::<Vec<_>>(),
            "expectations": bootstrap.job.expectations.iter().map(|expectation| json!({"id":redactor.redact_export_text(&expectation.id),"result":"not_run","numeric_checks":[]})).collect::<Vec<_>>(),
            "caller_assisted": false
        },
        "evidence": {"manifest":manifest,"complete":false},
        "escalation": null,
        "cleanup": {"browser":"starting","profile":"starting","application_state":"caller_owned"}
    }))
}

fn bind_private_socket(path: &Path) -> Result<AuthenticatedListener, String> {
    let listener = AuthenticatedListener::bind(path)?;
    listener.set_nonblocking()?;
    Ok(listener)
}

fn serve(listener: AuthenticatedListener, control: Arc<Control>, stop: Arc<AtomicBool>) {
    std::thread::spawn(move || {
        while !stop.load(Ordering::SeqCst) {
            match listener.try_accept() {
                Ok(Some(stream)) => handle(stream, &control),
                Ok(None) => {
                    std::thread::sleep(Duration::from_millis(10));
                }
                Err(_) => break,
            }
        }
    });
}

fn handle(mut stream: UnixStream, control: &Control) {
    let request = read_frame(&mut stream);
    let effect = apply_request(control, &request);
    let run = control
        .run
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let response = Response {
        ipc_version: IPC_VERSION,
        run_id: &run.run_id,
        job_digest: &run.job_digest,
        accepted: effect.accepted,
        result: effect.accepted.then_some(&run.result),
        error_code: effect.error_code,
    };
    if send_response(&mut stream, &response) && effect.readiness && effect.accepted {
        control.ready.store(true, Ordering::SeqCst);
    }
}

struct RequestEffect {
    accepted: bool,
    readiness: bool,
    error_code: Option<&'static str>,
}

fn apply_request(control: &Control, request: &Result<Request, String>) -> RequestEffect {
    match request {
        Ok(Request::Ready {
            ipc_version: IPC_VERSION,
        }) => RequestEffect {
            accepted: true,
            readiness: true,
            error_code: None,
        },
        Ok(Request::Status {
            ipc_version: IPC_VERSION,
        }) => ordinary_effect(),
        Ok(Request::Abort {
            ipc_version: IPC_VERSION,
            run_id,
            job_digest,
        }) if request_identity_matches(control, run_id, job_digest) => {
            control.abort.store(true, Ordering::SeqCst);
            control.cancellation.cancel();
            ordinary_effect()
        }
        Ok(Request::Deadline {
            ipc_version: IPC_VERSION,
            run_id,
            job_digest,
        }) if request_identity_matches(control, run_id, job_digest)
            && control.deadline_termination().is_some() =>
        {
            control.cancellation.cancel();
            ordinary_effect()
        }
        Ok(Request::Resume {
            ipc_version: IPC_VERSION,
            run_id,
            job_digest,
            request_id,
            request_digest,
            request,
        }) if request_identity_matches(control, run_id, job_digest) => resume_effect(
            admit_disposition(control, request_id, request_digest, request.clone()),
        ),
        _ => RequestEffect {
            accepted: false,
            readiness: false,
            error_code: None,
        },
    }
}

fn request_identity_matches(control: &Control, run_id: &str, job_digest: &str) -> bool {
    let run = control
        .run
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    run.run_id == run_id && run.job_digest == job_digest
}

fn ordinary_effect() -> RequestEffect {
    RequestEffect {
        accepted: true,
        readiness: false,
        error_code: None,
    }
}

fn resume_effect(result: Result<(), &'static str>) -> RequestEffect {
    RequestEffect {
        accepted: result.is_ok(),
        readiness: false,
        error_code: result.err(),
    }
}

fn admit_disposition(
    control: &Control,
    request_id: &str,
    request_digest: &str,
    request: DispositionRequest,
) -> Result<(), &'static str> {
    let mut run = control
        .run
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let mut admission = control
        .resume
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some(repeated) = repeated_resume(&admission, request_id, request_digest, &request) {
        return repeated;
    }
    let current_id = run.result.pointer("/escalation/id").and_then(Value::as_str);
    let paused = run.result.get("state").and_then(Value::as_str) == Some("uncertain")
        && run.result.get("terminal").and_then(Value::as_bool) == Some(false);
    if !paused || current_id != Some(request.escalation_id.as_str()) {
        return Err("stale_escalation");
    }
    if admission.pending.is_some() {
        return Err("stale_escalation");
    }
    run.pause_deadline_unix_ms = None;
    if store::write_run_control(&control.state_root, &run).is_err() {
        return Err("resume_state_unavailable");
    }
    let receipt = ResumeReceipt {
        request_id: request_id.to_owned(),
        request_digest: request_digest.to_owned(),
    };
    admission
        .consumed
        .insert(request.escalation_id.clone(), receipt.clone());
    admission.pending = Some(PendingResume { request, receipt });
    Ok(())
}

fn repeated_resume(
    admission: &ResumeAdmission,
    request_id: &str,
    request_digest: &str,
    request: &DispositionRequest,
) -> Option<Result<(), &'static str>> {
    admission
        .consumed
        .get(&request.escalation_id)
        .map(|receipt| {
            (receipt.request_id == request_id && receipt.request_digest == request_digest)
                .then_some(())
                .ok_or("stale_escalation")
        })
}

fn send_response(stream: &mut UnixStream, response: &Response<'_>) -> bool {
    serde_json::to_writer(&mut *stream, response).is_ok() && stream.flush().is_ok()
}

fn monitor_watchdog(fd: Option<i32>, control: Arc<Control>) {
    let Some(fd) = fd else { return };
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
    if flags != -1 {
        unsafe {
            libc::fcntl(fd, libc::F_SETFD, flags | libc::FD_CLOEXEC);
        }
    }
    std::thread::spawn(move || {
        let mut pipe = unsafe { std::fs::File::from_raw_fd(fd) };
        let mut byte = [0_u8; 1];
        if pipe.read(&mut byte).unwrap_or(0) == 0 {
            control.watchdog_lost.store(true, Ordering::SeqCst);
            control.cancellation.cancel();
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::process::IPC_VERSION;
    use std::os::unix::fs::{PermissionsExt, symlink};
    use tempfile::TempDir;

    #[test]
    fn private_socket_rejects_symlink_substitution() {
        let temporary = TempDir::new().unwrap();
        let outside = temporary.path().join("outside");
        fs::write(&outside, b"outside").unwrap();
        let socket = temporary.path().join("control.sock");
        symlink(&outside, &socket).unwrap();
        assert!(bind_private_socket(&socket).is_err());
        assert_eq!(fs::read(&outside).unwrap(), b"outside");
    }

    #[test]
    fn private_socket_has_private_mode_and_accepts_only_current_user_peer() {
        let temporary = TempDir::new().unwrap();
        let socket = temporary.path().join("control.sock");
        let listener = bind_private_socket(&socket).unwrap();
        assert_eq!(
            fs::metadata(&socket).unwrap().permissions().mode() & 0o777,
            0o600
        );
        let client = UnixStream::connect(&socket).unwrap();
        assert!(listener.accept().is_ok());
        drop(client);
    }

    #[test]
    fn watchdog_deadline_request_requires_a_locally_confirmed_deadline() {
        let temporary = TempDir::new().unwrap();
        let cancellation = InputCancellation::default();
        let control = Control {
            state_root: temporary.path().join("state"),
            run: Mutex::new(RunControl {
                schema_version: SchemaVersion,
                ipc_version: IPC_VERSION,
                sequence: 1,
                run_id: "r".into(),
                request_id: "q".into(),
                job_digest: "d".into(),
                evidence_root: temporary.path().join("evidence"),
                started_unix_ms: 0,
                lifetime_deadline_unix_ms: u64::MAX,
                pause_deadline_unix_ms: None,
                host: None,
                watchdog: None,
                socket: temporary.path().join("socket"),
                result: json!({"state":"uncertain","terminal":false}),
            }),
            cancellation: cancellation.clone(),
            abort: AtomicBool::new(false),
            ready: AtomicBool::new(false),
            watchdog_lost: AtomicBool::new(false),
            resume: Mutex::new(ResumeAdmission::default()),
            pause_timeout_ms: 1_000,
        };

        assert!(
            !send_test_request(&control, "deadline")["accepted"]
                .as_bool()
                .unwrap()
        );
        assert!(!cancellation.is_cancelled());
        control.run.lock().unwrap().pause_deadline_unix_ms = Some(now_unix_ms().saturating_sub(1));
        assert!(
            send_test_request(&control, "deadline")["accepted"]
                .as_bool()
                .unwrap()
        );
        assert!(cancellation.is_cancelled());
        assert!(!control.abort.load(Ordering::SeqCst));
        assert_eq!(
            control.termination(),
            Some(HostedTermination::PauseDeadlineElapsed)
        );
        assert_eq!(send_test_request(&control, "abort")["accepted"], true);
        assert!(control.abort.load(Ordering::SeqCst));
    }

    fn send_test_request(control: &Control, kind: &str) -> Value {
        let run = control.run.lock().unwrap();
        let request = if matches!(kind, "abort" | "deadline") {
            json!({
                "kind":kind,
                "ipc_version":IPC_VERSION,
                "run_id":run.run_id,
                "job_digest":run.job_digest,
            })
        } else {
            json!({"kind":kind,"ipc_version":IPC_VERSION})
        };
        drop(run);
        send_test_value(control, request)
    }

    fn send_test_value(control: &Control, request: Value) -> Value {
        let (mut client, server) = UnixStream::pair().unwrap();
        crate::control_socket::write_frame(&mut client, &request).unwrap();
        handle(server, control);
        serde_json::from_reader(client).unwrap()
    }

    #[test]
    fn readiness_requires_a_socket_exchange_with_the_exact_ipc_version() {
        let temporary = TempDir::new().unwrap();
        let cancellation = InputCancellation::default();
        let control = Control {
            state_root: temporary.path().join("state"),
            run: Mutex::new(RunControl {
                schema_version: SchemaVersion,
                ipc_version: IPC_VERSION,
                sequence: 1,
                run_id: "ready-run".into(),
                request_id: "q".into(),
                job_digest: "d".into(),
                evidence_root: temporary.path().join("evidence"),
                started_unix_ms: 0,
                lifetime_deadline_unix_ms: u64::MAX,
                pause_deadline_unix_ms: None,
                host: None,
                watchdog: None,
                socket: temporary.path().join("socket"),
                result: json!({"state":"running","terminal":false}),
            }),
            cancellation,
            abort: AtomicBool::new(false),
            ready: AtomicBool::new(false),
            watchdog_lost: AtomicBool::new(false),
            resume: Mutex::new(ResumeAdmission::default()),
            pause_timeout_ms: 1_000,
        };
        let wrong = send_test_value(
            &control,
            json!({"kind":"ready","ipc_version":IPC_VERSION + 1}),
        );
        assert_eq!(wrong["accepted"], false);
        assert!(!control.ready.load(Ordering::SeqCst));
        let ready = send_test_request(&control, "ready");
        assert_eq!(ready["accepted"], true);
        assert_eq!(ready["ipc_version"], IPC_VERSION);
        assert_eq!(ready["run_id"], "ready-run");
        assert_eq!(ready["job_digest"], "d");
        assert!(control.ready.load(Ordering::SeqCst));
        for _ in 0..100 {
            assert_eq!(send_test_request(&control, "status")["accepted"], true);
        }

        let abort = serde_json::to_vec(&json!({
            "kind":"abort",
            "ipc_version":IPC_VERSION,
            "run_id":"ready-run",
            "job_digest":"d",
        }))
        .unwrap();
        let status = serde_json::to_vec(&json!({
            "kind":"status",
            "ipc_version":IPC_VERSION,
        }))
        .unwrap();
        let mut joined = abort;
        joined.push(b'\n');
        joined.extend(status);
        joined.push(b'\n');
        let (mut client, server) = UnixStream::pair().unwrap();
        client.write_all(&joined).unwrap();
        handle(server, &control);
        let rejected: Value = serde_json::from_reader(client).unwrap();
        assert_eq!(rejected["accepted"], false);
        assert!(!control.abort.load(Ordering::SeqCst));
    }

    #[test]
    fn disposition_admission_consumes_an_escalation_exactly_once() {
        let temporary = TempDir::new().unwrap();
        let control = Arc::new(Control {
            state_root: temporary.path().join("state"),
            run: Mutex::new(RunControl {
                schema_version: SchemaVersion,
                ipc_version: IPC_VERSION,
                sequence: 1,
                run_id: "r_resume".into(),
                request_id: "original".into(),
                job_digest: "digest".into(),
                evidence_root: temporary.path().join("evidence"),
                started_unix_ms: 0,
                lifetime_deadline_unix_ms: u64::MAX,
                pause_deadline_unix_ms: Some(u64::MAX),
                host: None,
                watchdog: None,
                socket: temporary.path().join("socket"),
                result: json!({"state":"uncertain","terminal":false,"escalation":{"id":"e_1"}}),
            }),
            cancellation: InputCancellation::default(),
            abort: AtomicBool::new(false),
            ready: AtomicBool::new(true),
            watchdog_lost: AtomicBool::new(false),
            resume: Mutex::new(ResumeAdmission::default()),
            pause_timeout_ms: 1_000,
        });
        let request = DispositionRequest {
            schema_version: SchemaVersion,
            escalation_id: "e_1".into(),
            disposition: manuvra_contract::Disposition::RetryObservation(
                manuvra_contract::RetryObservationDisposition {
                    kind: manuvra_contract::RetryObservationKind::RetryObservation,
                },
            ),
        };
        let wrong_resume = send_test_value(
            &control,
            json!({
                "kind":"resume",
                "ipc_version":IPC_VERSION,
                "run_id":"r_other",
                "job_digest":"digest",
                "request_id":"misdirected",
                "request_digest":"misdirected-digest",
                "request":request,
            }),
        );
        assert_eq!(wrong_resume["accepted"], false);
        assert!(control.resume.lock().unwrap().pending.is_none());
        assert!(control.resume.lock().unwrap().consumed.is_empty());
        let wrong_abort = send_test_value(
            &control,
            json!({
                "kind":"abort",
                "ipc_version":IPC_VERSION,
                "run_id":"r_resume",
                "job_digest":"wrong-digest",
            }),
        );
        assert_eq!(wrong_abort["accepted"], false);
        assert!(!control.abort.load(Ordering::SeqCst));
        assert!(!control.cancellation.is_cancelled());
        let barrier = Arc::new(std::sync::Barrier::new(8));
        let contenders = (0..8)
            .map(|index| {
                let control = Arc::clone(&control);
                let barrier = Arc::clone(&barrier);
                let request = request.clone();
                std::thread::spawn(move || {
                    barrier.wait();
                    admit_disposition(
                        &control,
                        &format!("resume-{index}"),
                        &format!("digest-{index}"),
                        request,
                    )
                })
            })
            .collect::<Vec<_>>();
        assert_eq!(
            contenders
                .into_iter()
                .map(|contender| contender.join().unwrap())
                .filter(Result::is_ok)
                .count(),
            1
        );
        let (accepted_id, accepted_digest) = {
            let admission = control.resume.lock().unwrap();
            let receipt = admission.consumed.get("e_1").unwrap();
            (receipt.request_id.clone(), receipt.request_digest.clone())
        };
        assert!(matches!(
            control.wait_while_paused("e_1"),
            HostedEvent::Disposition(_)
        ));
        let historical = json!({
            "schema_version":1,
            "request_id":"original",
            "run_id":"r_resume",
            "state":"running",
            "terminal":false,
            "evidence":{"complete":false,"manifest":"/tmp/history/manifest.json"},
            "escalation":null,
        });
        control.publish_checkpoint(&historical).unwrap();
        let immutable = match store::lookup_request(&control.state_root, &accepted_id).unwrap() {
            Some(store::RequestEntry::Complete(record)) => record,
            _ => panic!("accepted resume must publish an immutable response record"),
        };
        assert_eq!(immutable.result, historical);
        store::validate_control_record(&control.state_root, &immutable).unwrap();
        control
            .publish_checkpoint(&json!({
                "schema_version":1,
                "request_id":"original",
                "run_id":"r_resume",
                "state":"passed",
                "terminal":true,
                "evidence":{"complete":true,"manifest":"/tmp/later/manifest.json"},
                "escalation":null,
            }))
            .unwrap();
        let still_immutable =
            match store::lookup_request(&control.state_root, &accepted_id).unwrap() {
                Some(store::RequestEntry::Complete(record)) => record,
                _ => panic!("historical resume response disappeared"),
            };
        assert_eq!(still_immutable.result, historical);
        assert!(
            admit_disposition(&control, &accepted_id, &accepted_digest, request.clone()).is_ok()
        );
        let duplicate = send_test_value(
            &control,
            json!({
                "kind":"resume",
                "ipc_version":IPC_VERSION,
                "run_id":"r_resume",
                "job_digest":"digest",
                "request_id":accepted_id,
                "request_digest":accepted_digest,
                "request":request,
            }),
        );
        assert_eq!(duplicate["accepted"], true);
        assert_eq!(
            admit_disposition(&control, "late-resume", "late-digest", request.clone()),
            Err("stale_escalation")
        );
        let stale = DispositionRequest {
            schema_version: SchemaVersion,
            escalation_id: "e_0".into(),
            disposition: manuvra_contract::Disposition::Abort(manuvra_contract::AbortDisposition {
                kind: manuvra_contract::AbortKind::Abort,
            }),
        };
        assert_eq!(
            admit_disposition(&control, "stale", "stale-digest", stale),
            Err("stale_escalation")
        );
        assert_eq!(control.run.lock().unwrap().pause_deadline_unix_ms, None);
    }

    fn assert_resume_publication_recovers_first_response(fail_seal: bool) {
        let temporary = TempDir::new().unwrap();
        let receipt = ResumeReceipt {
            request_id: "resume-publication".into(),
            request_digest: "resume-digest".into(),
        };
        let mut consumed = BTreeMap::new();
        consumed.insert("e_1".into(), receipt.clone());
        let control = Control {
            state_root: temporary.path().join("state"),
            run: Mutex::new(RunControl {
                schema_version: SchemaVersion,
                ipc_version: IPC_VERSION,
                sequence: 1,
                run_id: "r_publication".into(),
                request_id: "original".into(),
                job_digest: "job-digest".into(),
                evidence_root: temporary.path().join("evidence"),
                started_unix_ms: 0,
                lifetime_deadline_unix_ms: u64::MAX,
                pause_deadline_unix_ms: None,
                host: None,
                watchdog: None,
                socket: temporary.path().join("socket"),
                result: json!({"state":"uncertain","terminal":false,"escalation":{"id":"e_1"}}),
            }),
            cancellation: InputCancellation::default(),
            abort: AtomicBool::new(false),
            ready: AtomicBool::new(true),
            watchdog_lost: AtomicBool::new(false),
            resume: Mutex::new(ResumeAdmission {
                pending: None,
                awaiting_checkpoint: Some(ResumePublication {
                    receipt: receipt.clone(),
                    response: None,
                }),
                consumed,
            }),
            pause_timeout_ms: 1_000,
        };
        let first = json!({
            "schema_version":1,
            "request_id":"original",
            "run_id":"r_publication",
            "state":"running",
            "terminal":false,
            "evidence":{"complete":false,"manifest":"/tmp/first/manifest.json"},
            "escalation":null,
        });
        let later = json!({
            "schema_version":1,
            "request_id":"original",
            "run_id":"r_publication",
            "state":"passed",
            "terminal":true,
            "evidence":{"complete":true,"manifest":"/tmp/later/manifest.json"},
            "escalation":null,
        });
        let failure = publish_resume_response_with(
            &control,
            &first,
            |receipt, result, exit_code| {
                if fail_seal {
                    Err("injected seal failure".into())
                } else {
                    store::sealed_control_record(
                        &control.state_root,
                        &receipt.request_id,
                        "r_publication",
                        &receipt.request_digest,
                        exit_code,
                        result,
                    )
                }
            },
            |receipt, record| {
                if fail_seal {
                    unreachable!("finalization cannot follow a seal failure")
                }
                let _ = (receipt, record);
                Err("injected finalize failure".into())
            },
        )
        .unwrap_err();
        assert!(failure.contains(if fail_seal { "seal" } else { "finalize" }));
        assert!(
            store::lookup_request(&control.state_root, &receipt.request_id)
                .unwrap()
                .is_none()
        );
        assert_eq!(
            control
                .resume
                .lock()
                .unwrap()
                .awaiting_checkpoint
                .as_ref()
                .and_then(|publication| publication.response.as_ref()),
            Some(&first)
        );

        control.publish_checkpoint(&later).unwrap();
        let immutable =
            match store::lookup_request(&control.state_root, &receipt.request_id).unwrap() {
                Some(store::RequestEntry::Complete(record)) => record,
                _ => panic!("the later checkpoint must finish the retained response"),
            };
        assert_eq!(immutable.result, first);
        store::validate_control_record(&control.state_root, &immutable).unwrap();
        assert!(control.resume.lock().unwrap().awaiting_checkpoint.is_none());

        let final_later = json!({"state":"failed","terminal":true});
        control.publish_checkpoint(&final_later).unwrap();
        let unchanged =
            match store::lookup_request(&control.state_root, &receipt.request_id).unwrap() {
                Some(store::RequestEntry::Complete(record)) => record,
                _ => panic!("the retained history must remain complete"),
            };
        assert_eq!(unchanged.result, first);
        let repeated = DispositionRequest {
            schema_version: SchemaVersion,
            escalation_id: "e_1".into(),
            disposition: manuvra_contract::Disposition::RetryObservation(
                manuvra_contract::RetryObservationDisposition {
                    kind: manuvra_contract::RetryObservationKind::RetryObservation,
                },
            ),
        };
        assert_eq!(
            admit_disposition(
                &control,
                &receipt.request_id,
                &receipt.request_digest,
                repeated
            ),
            Ok(())
        );
        assert!(control.resume.lock().unwrap().pending.is_none());
    }

    #[test]
    fn resume_publication_retains_first_response_when_sealing_fails() {
        assert_resume_publication_recovers_first_response(true);
    }

    #[test]
    fn resume_publication_retains_first_response_when_finalization_fails() {
        assert_resume_publication_recovers_first_response(false);
    }

    #[cfg(debug_assertions)]
    #[test]
    fn external_fake_browser_records_only_explicit_dispatch() {
        let temporary = TempDir::new().unwrap();
        let mut browser = spawn_fake_browser(temporary.path()).unwrap();
        let log = temporary.path().join("fake-browser.dispatch.log");
        assert!(!log.exists());
        dispatch_fake_browser(&mut browser, temporary.path()).unwrap();
        assert!(
            fs::read_to_string(log)
                .unwrap()
                .contains("dispatch click submit")
        );
        browser.kill().unwrap();
        browser.wait().unwrap();
    }

    #[cfg(debug_assertions)]
    fn debug_fault_runtime() -> (TempDir, HostBootstrap, Arc<Control>) {
        let temporary = tempfile::Builder::new()
            .prefix("host-pause-abort")
            .tempdir_in("/tmp")
            .unwrap();
        let evidence_root = temporary.path().join("evidence");
        let state_root = temporary.path().join("state");
        let runtime_dir = temporary.path().join("runtime");
        for directory in [&evidence_root, &state_root, &runtime_dir] {
            fs::create_dir_all(directory).unwrap();
        }
        let evidence_root = evidence_root.canonicalize().unwrap();
        let job = manuvra_contract::Job::parse(
            serde_json::to_vec(&json!({
                "schema_version":1,
                "target":{"kind":"browser","url":"http://127.0.0.1:4351/"},
                "context":{"journey":"pause abort","revision":"fixture","environment":"fixture","actor":"fixture","authority":"fixture"},
                "steps":[{"id":"submit","goal":"submit","done_when":[{"url_contains":"/done"}]}]
            }))
            .unwrap()
            .as_slice(),
        )
        .unwrap();
        let bootstrap = HostBootstrap {
            job,
            intent: store::RequestIntent {
                schema_version: SchemaVersion,
                public_request_id: "pause-abort".into(),
                run_id: "r_pause_abort_test".into(),
                job_digest: "digest".into(),
                evidence_root,
            },
            lookup_request_id: "pause-abort".into(),
            state_root,
            browser: None,
            headless: true,
            provider_key: None,
            runtime_dir,
            started_unix_ms: now_unix_ms(),
            lifetime_deadline_unix_ms: u64::MAX,
            pause_timeout_ms: 30_000,
            watchdog: None,
            liveness_fd: None,
        };
        let redactor = manuvra_flow::evidence::Redactor::for_job(&bootstrap.job).unwrap();
        let control = make_control(
            &bootstrap,
            &bootstrap.runtime_dir.join("control.sock"),
            &redactor,
        )
        .unwrap();
        (temporary, bootstrap, control)
    }

    #[cfg(debug_assertions)]
    #[test]
    fn pause_abort_fixture_publishes_and_replaces_complete_evidence() {
        let (_temporary, bootstrap, control) = debug_fault_runtime();
        control.abort.store(true, Ordering::SeqCst);
        let mut browser = spawn_fake_browser(&bootstrap.runtime_dir).unwrap();

        assert!(finish_pause_abort(&bootstrap, &control, &mut browser).unwrap());
        let result = &control.run.lock().unwrap().result;
        assert_eq!(result["state"], "aborted");
        assert_eq!(result["evidence"]["complete"], true);
        assert!(
            bootstrap
                .intent
                .evidence_root
                .join("r_pause_abort_test/manifest.json")
                .is_file()
        );
    }

    #[cfg(debug_assertions)]
    #[test]
    fn watchdog_loss_fixture_closes_browser_and_replaces_complete_evidence() {
        let (_temporary, bootstrap, control) = debug_fault_runtime();
        control.watchdog_lost.store(true, Ordering::SeqCst);
        let mut browser = spawn_fake_browser(&bootstrap.runtime_dir).unwrap();

        assert!(finish_watchdog_loss(&bootstrap, &control, &mut browser).unwrap());
        let result = &control.run.lock().unwrap().result;
        assert_eq!(result["state"], "blocked");
        assert_eq!(result["reason"]["code"], "watchdog_lost");
        assert_eq!(result["cleanup"]["browser"], "closed");
        assert_eq!(result["evidence"]["complete"], true);
    }

    #[test]
    fn watchdog_pipe_loss_cancels_input_and_marks_the_host() {
        let temporary = TempDir::new().unwrap();
        let mut descriptors = [-1_i32; 2];
        assert_eq!(unsafe { libc::pipe(descriptors.as_mut_ptr()) }, 0);
        let cancellation = InputCancellation::default();
        let control = Arc::new(Control {
            state_root: temporary.path().join("state"),
            run: Mutex::new(RunControl {
                schema_version: SchemaVersion,
                ipc_version: IPC_VERSION,
                sequence: 1,
                run_id: "r".into(),
                request_id: "q".into(),
                job_digest: "d".into(),
                evidence_root: temporary.path().join("evidence"),
                started_unix_ms: 0,
                lifetime_deadline_unix_ms: u64::MAX,
                pause_deadline_unix_ms: None,
                host: None,
                watchdog: None,
                socket: temporary.path().join("socket"),
                result: json!({"state":"running","terminal":false}),
            }),
            cancellation: cancellation.clone(),
            abort: AtomicBool::new(false),
            ready: AtomicBool::new(false),
            watchdog_lost: AtomicBool::new(false),
            resume: Mutex::new(ResumeAdmission::default()),
            pause_timeout_ms: 1_000,
        });
        monitor_watchdog(Some(descriptors[0]), control.clone());
        unsafe {
            libc::close(descriptors[1]);
        }
        let deadline = std::time::Instant::now() + Duration::from_secs(1);
        while !control.watchdog_lost.load(Ordering::SeqCst) {
            assert!(std::time::Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(cancellation.is_cancelled());
        assert_eq!(
            control.paused_termination(),
            Some(HostedTermination::WatchdogLost)
        );
        assert!(matches!(
            control.wait_while_paused("e"),
            HostedEvent::Termination(HostedTermination::WatchdogLost)
        ));

        control.watchdog_lost.store(false, Ordering::SeqCst);
        control.abort.store(true, Ordering::SeqCst);
        assert_eq!(
            control.paused_termination(),
            Some(HostedTermination::Aborted)
        );

        control.abort.store(false, Ordering::SeqCst);
        let mut run = control.run.lock().unwrap();
        run.lifetime_deadline_unix_ms = now_unix_ms().saturating_sub(1);
        assert_eq!(
            deadline_termination(
                now_unix_ms(),
                run.pause_deadline_unix_ms,
                run.lifetime_deadline_unix_ms
            ),
            Some(HostedTermination::LifetimeElapsed)
        );
        run.lifetime_deadline_unix_ms = u64::MAX;
        run.pause_deadline_unix_ms = Some(now_unix_ms().saturating_sub(1));
        assert_eq!(
            deadline_termination(
                now_unix_ms(),
                run.pause_deadline_unix_ms,
                run.lifetime_deadline_unix_ms
            ),
            Some(HostedTermination::PauseDeadlineElapsed)
        );
        assert_eq!(deadline_termination(1, None, u64::MAX), None);
    }
}
