//! Debug-build fault injection for the run host. Integration tests select a scenario through
//! `MANUVRA_TEST_HOST_FAULT` or `MANUVRA_TEST_HOST_BOOTSTRAP_FAULT` to stop the host at a crash
//! window, park it, or finish it against a fake browser. Release builds never compile it.

use super::Control;
use crate::process::HostBootstrap;
use manuvra_flow::run::{HostedControl, HostedEvent, HostedTermination};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::fs;
use std::io::Write;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::Mutex;
use std::time::Duration;

pub(super) fn bootstrap_fault(point: &str) {
    if std::env::var("MANUVRA_TEST_HOST_BOOTSTRAP_FAULT").as_deref() == Ok(point) {
        kill_fault_host();
    }
}

pub(super) fn run_fault_fixture(
    bootstrap: &HostBootstrap,
    control: &Control,
) -> Result<bool, String> {
    let Some(scenario) = std::env::var_os("MANUVRA_TEST_HOST_FAULT") else {
        return Ok(false);
    };
    let mut browser = spawn_fake_browser(&bootstrap.runtime_dir)?;
    fault_handler(&scenario.to_string_lossy())
        .and_then(|handler| handler(bootstrap, control, &mut browser))
}

type FaultHandler = fn(&HostBootstrap, &Control, &mut Child) -> Result<bool, String>;

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

fn fault_before_prepared(
    bootstrap: &HostBootstrap,
    _: &Control,
    browser: &mut Child,
) -> Result<bool, String> {
    run_fault_action_at(bootstrap, browser, FaultActionBoundary::BeforePrepared)
}

fn fault_after_prepared(
    bootstrap: &HostBootstrap,
    _: &Control,
    browser: &mut Child,
) -> Result<bool, String> {
    run_fault_action_at(bootstrap, browser, FaultActionBoundary::AfterPrepared)
}

fn fault_after_dispatch(
    bootstrap: &HostBootstrap,
    _: &Control,
    browser: &mut Child,
) -> Result<bool, String> {
    run_fault_action_at(bootstrap, browser, FaultActionBoundary::AfterDispatch)
}

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

fn fault_pause_abort(
    bootstrap: &HostBootstrap,
    control: &Control,
    browser: &mut Child,
) -> Result<bool, String> {
    finish_pause_abort(bootstrap, control, browser)
}

fn fault_watchdog_lost(
    bootstrap: &HostBootstrap,
    control: &Control,
    browser: &mut Child,
) -> Result<bool, String> {
    finish_watchdog_loss(bootstrap, control, browser)
}

fn fault_terminal_hang(
    bootstrap: &HostBootstrap,
    control: &Control,
    _: &mut Child,
) -> Result<bool, String> {
    publish_terminal_hang(bootstrap, control)?;
    park_forever()
}

fn publish_terminal_hang(bootstrap: &HostBootstrap, control: &Control) -> Result<(), String> {
    let mut result = fault_control_result(control);
    apply_terminal_hang_result(&mut result);
    finish_fault_evidence(bootstrap, &mut result)?;
    control.publish_checkpoint(&result)
}

fn fault_control_result(control: &Control) -> Value {
    control
        .run
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .result
        .clone()
}

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

fn park_forever() -> ! {
    loop {
        std::thread::park();
    }
}

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

struct FaultActionJournal {
    inner: manuvra_flow::actions::DurableJournal,
    boundary: FaultActionBoundary,
}

#[derive(Clone, Copy)]
enum FaultActionBoundary {
    BeforePrepared,
    AfterPrepared,
    AfterDispatch,
}

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

struct FaultActionPerformer<'a> {
    browser: Mutex<&'a mut Child>,
    runtime_dir: &'a Path,
    boundary: FaultActionBoundary,
}

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

fn fault_observation() -> manuvra_flow::Observation {
    manuvra_flow::Observation {
        document_id: "fault-document".into(),
        url: "http://127.0.0.1:4351/".into(),
        route: "/".into(),
        title: "Fault fixture".into(),
        dialogs: Vec::new(),
        focused: None,
        focus_anchor: None,
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
            container: None,
            shares_name: false,
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
        hover_regions: Vec::new(),
        hover_regions_truncated: false,
        hover_rules_unreadable: false,
    }
}

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
        key: choice("Escape"),
        hover_target: None,
        step_done: 0.0,
        usage: BTreeMap::new(),
        request_id: None,
        model: "fault-fixture".into(),
        request: Value::Null,
    }
}

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

fn close_fake_browser(browser: &mut Child) {
    drop(browser.stdin.take());
    let _ = browser.wait();
}

fn kill_fault_host() -> ! {
    unsafe {
        libc::kill(libc::getpid(), libc::SIGKILL);
    }
    std::process::abort()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::process::now_unix_ms;
    use crate::store;
    use manuvra_contract::SchemaVersion;
    use std::sync::Arc;
    use std::sync::atomic::Ordering;
    use tempfile::TempDir;

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
        close_fake_browser(&mut browser);
    }

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
        let control = super::super::make_control(
            &bootstrap,
            &bootstrap.runtime_dir.join("control.sock"),
            &redactor,
        )
        .unwrap();
        (temporary, bootstrap, control)
    }

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
}
