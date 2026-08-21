use serde_json::Value;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::thread;
use std::time::{Duration, Instant};
use tempfile::TempDir;

const CLI: &str = env!("CARGO_BIN_EXE_manuvra");
const DAEMON: &str = env!("CARGO_BIN_EXE_manuvra-daemon");

struct Harness {
    _root: TempDir,
    temporary: PathBuf,
    config: PathBuf,
    diagnostics_config: Option<PathBuf>,
    evidence: Option<PathBuf>,
    daemon: Option<Child>,
}

impl Harness {
    fn new() -> Self {
        Self::from_diagnostics(None)
    }

    fn configured(diagnostics: Value) -> Self {
        Self::from_diagnostics(Some(diagnostics))
    }

    fn from_diagnostics(diagnostics: Option<Value>) -> Self {
        let root = tempfile::tempdir().unwrap();
        let temporary = root.path().join("tmp");
        let config = root.path().join("config");
        fs::create_dir_all(&temporary).unwrap();
        let evidence = diagnostics
            .as_ref()
            .map(|_| temporary.join("diagnostic-evidence.json"));
        let diagnostics_config = diagnostics.map(|mut diagnostics| {
            diagnostics["evidence_path"] =
                Value::String(evidence.as_ref().unwrap().to_str().unwrap().to_owned());
            let path = temporary.join("diagnostic-scenario.json");
            fs::write(&path, serde_json::to_vec(&diagnostics).unwrap()).unwrap();
            path
        });
        let mut harness = Self {
            _root: root,
            temporary,
            config,
            diagnostics_config,
            evidence,
            daemon: None,
        };
        harness.start_daemon();
        harness
    }

    fn start_daemon(&mut self) {
        let mut command = Command::new(DAEMON);
        command
            .env("MANUVRA_TMPDIR", &self.temporary)
            .env("MANUVRA_CONFIG_HOME", &self.config)
            .env("MANUVRA_TEST_FAKE_ADAPTER", "1")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        if let Some(config) = &self.diagnostics_config {
            command.env("MANUVRA_TEST_DIAGNOSTICS_CONFIG", config);
        }
        let child = command.spawn().unwrap();
        self.daemon = Some(child);
        wait_for_socket(&self.socket_path());
        self.wait_for_daemon();
    }

    fn stop_daemon(&mut self) {
        if let Some(mut daemon) = self.daemon.take() {
            daemon.kill().unwrap();
            daemon.wait().unwrap();
        }
    }

    fn wait_for_daemon(&self) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            if self.command(&["targets"]).status.success() {
                return;
            }
            thread::sleep(Duration::from_millis(5));
        }
        panic!("daemon did not complete runtime initialization");
    }

    fn command(&self, args: &[&str]) -> Output {
        Command::new(CLI)
            .args(args)
            .env("MANUVRA_TMPDIR", &self.temporary)
            .env("MANUVRA_CONFIG_HOME", &self.config)
            .env("MANUVRA_NO_AUTOSTART", "1")
            .output()
            .unwrap()
    }

    fn spawn_command(&self, args: &[&str]) -> Child {
        Command::new(CLI)
            .args(args)
            .env("MANUVRA_TMPDIR", &self.temporary)
            .env("MANUVRA_CONFIG_HOME", &self.config)
            .env("MANUVRA_NO_AUTOSTART", "1")
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap()
    }

    fn success(&self, args: &[&str]) -> Value {
        let output = self.command(args);
        assert!(
            output.status.success(),
            "stderr={} stdout={}",
            String::from_utf8_lossy(&output.stderr),
            String::from_utf8_lossy(&output.stdout)
        );
        assert!(output.stdout.len() <= 4096);
        assert_eq!(output.stdout.last(), Some(&b'\n'));
        serde_json::from_slice(&output.stdout).unwrap()
    }

    fn open(&self, target: &str, role: Option<&str>) -> String {
        let mut args = vec!["open", "--target", target];
        if let Some(role) = role {
            args.extend(["--role", role]);
        }
        self.success(&args)["session_id"]
            .as_str()
            .unwrap()
            .to_owned()
    }

    fn socket_path(&self) -> PathBuf {
        self.temporary.join("manuvra/runtime-v1/daemon.sock")
    }

    fn evidence(&self) -> Value {
        serde_json::from_slice(&fs::read(self.evidence.as_ref().unwrap()).unwrap()).unwrap()
    }
}

impl Drop for Harness {
    fn drop(&mut self) {
        if let Some(mut daemon) = self.daemon.take() {
            let _ = daemon.kill();
            let _ = daemon.wait();
        }
    }
}

#[test]
fn redirected_setup_uses_bounded_json_with_the_fake_permission_owner() {
    let harness = Harness::new();

    let setup = harness.success(&["setup"]);

    manuvra_protocol::validate_command_result("system.setup", &setup).unwrap();
    for permission in ["accessibility", "screen_recording", "post_event"] {
        assert_eq!(setup["permissions"][permission]["granted"], true);
        assert_eq!(setup["permissions"][permission]["prompt_requested"], false);
        assert_eq!(setup["permissions"][permission]["settings_opened"], false);
    }
}

fn permission_fact(before: bool, granted: bool, settings_opened: bool) -> Value {
    serde_json::json!({
        "before_granted": before,
        "prompt_requested": !before,
        "settings_opened": settings_opened,
        "granted": granted,
        "freshly_granted": !before && granted,
        "residual": !granted
    })
}

#[test]
fn configured_diagnostics_fake_drives_public_setup_doctor_and_replay_without_external_effects() {
    let harness = Harness::configured(serde_json::json!({
        "permissions": {
            "accessibility": permission_fact(false, false, true),
            "screen_recording": permission_fact(true, true, false),
            "post_event": permission_fact(false, true, false)
        },
        "installation": {
            "installed": true,
            "bundle": "/opt/homebrew/opt/manuvra/libexec/Manuvra.app"
        },
        "doctor_warnings": ["future_permission_warning"]
    }));

    let first = harness.success(&["setup", "--request-id", "configured-j5"]);
    let replay = harness.success(&["setup", "--request-id", "configured-j5"]);

    assert_eq!(first, replay);
    assert_eq!(first["installation"]["installed"], true);
    assert_eq!(
        first["installation"]["bundle"],
        "/opt/homebrew/opt/manuvra/libexec/Manuvra.app"
    );
    assert_eq!(
        first["permissions"]["accessibility"]["before_granted"],
        false
    );
    assert_eq!(
        first["permissions"]["accessibility"]["settings_opened"],
        true
    );
    assert_eq!(first["permissions"]["post_event"]["freshly_granted"], true);
    let doctor = harness.success(&["doctor", "--json"]);
    assert_eq!(
        doctor["daemon"]["adapters"][0]["permissions"]["accessibility"],
        false
    );
    assert_eq!(
        doctor["daemon"]["adapters"][0]["permissions"]["post_event"],
        true
    );
    assert!(
        doctor["warnings"]
            .as_array()
            .unwrap()
            .contains(&Value::String("future_permission_warning".to_owned()))
    );

    let evidence = harness.evidence();
    assert_eq!(evidence["setup_invocations"], 1);
    assert_eq!(evidence["request_attempts"]["accessibility"], 1);
    assert_eq!(evidence["request_attempts"]["screen_recording"], 0);
    assert_eq!(evidence["request_attempts"]["post_event"], 1);
    assert_eq!(evidence["rechecks"]["accessibility"], 1);
    assert_eq!(evidence["rechecks"]["screen_recording"], 1);
    assert_eq!(evidence["rechecks"]["post_event"], 1);
    assert_eq!(evidence["pane_opens"]["accessibility"], 1);
    assert_eq!(evidence["pane_opens"]["screen_recording"], 0);
    assert_eq!(evidence["external_permission_api_calls"], 0);
    assert_eq!(evidence["external_open_process_calls"], 0);
}

#[test]
fn configured_diagnostics_fake_reports_development_bundle_null() {
    let harness = Harness::configured(serde_json::json!({
        "permissions": {
            "accessibility": permission_fact(true, true, false),
            "screen_recording": permission_fact(false, false, true),
            "post_event": permission_fact(true, true, false)
        },
        "installation": {"installed": false, "bundle": null}
    }));

    let setup = harness.success(&["setup"]);

    assert_eq!(setup["installation"]["installed"], false);
    assert!(setup["installation"]["bundle"].is_null());
    assert_eq!(setup["permissions"]["screen_recording"]["residual"], true);
    assert_eq!(harness.evidence()["pane_opens"]["screen_recording"], 1);
}

#[test]
fn malformed_diagnostics_fake_config_prevents_daemon_startup() {
    let root = tempfile::tempdir().unwrap();
    let temporary = root.path().join("tmp");
    fs::create_dir_all(&temporary).unwrap();
    let config = temporary.join("malformed.json");
    fs::write(&config, br#"{"unexpected":true}"#).unwrap();

    let output = Command::new(DAEMON)
        .env("MANUVRA_TMPDIR", &temporary)
        .env("MANUVRA_CONFIG_HOME", root.path().join("config"))
        .env("MANUVRA_TEST_FAKE_ADAPTER", "1")
        .env("MANUVRA_TEST_DIAGNOSTICS_CONFIG", config)
        .output()
        .unwrap();

    assert_eq!(output.status.code(), Some(70));
    assert!(!temporary.join("manuvra/runtime-v1/daemon.sock").exists());
    assert!(String::from_utf8_lossy(&output.stderr).contains("invalid fake diagnostics config"));
}

fn wait_for_socket(path: &Path) {
    let deadline = Instant::now() + Duration::from_secs(3);
    while Instant::now() < deadline {
        if UnixStream::connect(path).is_ok() {
            return;
        }
        thread::sleep(Duration::from_millis(5));
    }
    panic!("socket did not become ready: {}", path.display());
}

#[test]
fn representative_cli_lifecycle_has_bounded_results_and_durable_export() {
    let harness = Harness::new();
    let socket_mode = fs::metadata(harness.socket_path())
        .unwrap()
        .permissions()
        .mode()
        & 0o777;
    let root_mode = fs::metadata(harness.socket_path().parent().unwrap())
        .unwrap()
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(socket_mode, 0o600);
    assert_eq!(root_mode, 0o700);

    let targets = harness.success(&["targets"]);
    let listed = targets["targets"].as_array().unwrap();
    assert_eq!(listed.len(), 2);
    let chrome = listed
        .iter()
        .find(|target| target["kind"] == "chrome")
        .expect("chrome target");
    let macos = listed
        .iter()
        .find(|target| target["kind"] == "macos")
        .expect("macos target");
    assert_eq!(chrome["owner"], "Chrome");
    assert_eq!(chrome["title"], "Fake Chrome");
    assert_eq!(macos["owner"], "Fake");
    assert_eq!(macos["title"], "Fake Target");
    let actor = harness.open("chrome_fake_1", None);
    let observer = harness.open("chrome_fake_1", Some("observer"));

    let screenshot = harness.success(&["observe", "screenshot", "--session", &actor]);
    let screenshot_path = PathBuf::from(screenshot["screenshot_path"].as_str().unwrap());
    let session_directory = screenshot_path.parent().unwrap().to_path_buf();
    assert!(screenshot_path.is_file());

    let click = harness.success(&[
        "click",
        "--session",
        &actor,
        "--role",
        "button",
        "--name",
        "Save",
    ]);
    assert_eq!(click["outcome"], "observed");
    assert_eq!(click["delivery"], "backend_confirmed");
    assert_eq!(click["effect_verification"], "not_asserted");
    assert!(Path::new(click["observation"]["screenshot_path"].as_str().unwrap()).is_file());

    let export_root = harness._root.path().join("export");
    let export = harness.success(&[
        "export",
        "--session",
        &actor,
        "--all",
        "--destination",
        export_root.to_str().unwrap(),
    ]);
    assert_eq!(export["verified"], true);
    assert!(export_root.join("manifest.json").is_file());
    let close = harness.success(&["close", "--session", &actor]);
    assert_eq!(close["artifacts_removed"], true);
    let exported_manifest: Value =
        serde_json::from_slice(&fs::read(export_root.join("manifest.json")).unwrap()).unwrap();
    assert_eq!(exported_manifest["lifetime"], "caller_owned");
    for artifact in exported_manifest["artifacts"].as_array().unwrap() {
        assert_eq!(artifact["lifetime"], "caller_owned");
        assert!(Path::new(artifact["absolute_path"].as_str().unwrap()).is_file());
    }
    assert!(!session_directory.exists());
    assert!(export_root.join("manifest.json").exists());
    harness.success(&["close", "--session", &observer]);
}

#[test]
fn competing_daemon_start_does_not_cleanup_live_session() {
    let harness = Harness::new();
    let actor = harness.open("chrome_fake_1", None);
    let screenshot = harness.success(&["observe", "screenshot", "--session", &actor]);
    let screenshot_path = PathBuf::from(screenshot["screenshot_path"].as_str().unwrap());
    assert!(screenshot_path.is_file());

    let status = Command::new(DAEMON)
        .env("MANUVRA_TMPDIR", &harness.temporary)
        .env("MANUVRA_CONFIG_HOME", &harness.config)
        .env("MANUVRA_TEST_FAKE_ADAPTER", "1")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .unwrap();

    assert!(
        !status.success(),
        "a competing daemon unexpectedly acquired authority"
    );
    assert!(
        screenshot_path.is_file(),
        "the losing daemon removed a live session artifact"
    );
    assert_eq!(
        harness.success(&["close", "--session", &actor])["closed"],
        true
    );
}

#[test]
fn daemon_crash_restart_cleans_session_and_releases_actor() {
    let mut harness = Harness::new();
    let actor = harness.open("chrome_fake_1", None);
    let screenshot = harness.success(&["observe", "screenshot", "--session", &actor]);
    let orphan = PathBuf::from(screenshot["screenshot_path"].as_str().unwrap())
        .parent()
        .unwrap()
        .to_path_buf();
    assert!(orphan.exists());
    harness.stop_daemon();
    harness.start_daemon();
    assert!(!orphan.exists());
    let replacement = harness.open("chrome_fake_1", None);
    assert!(replacement.starts_with("s_"));
}

#[test]
fn offline_discovery_does_not_require_daemon() {
    let mut harness = Harness::new();
    harness.stop_daemon();
    let list = harness.success(&["commands", "list", "--limit", "1"]);
    assert_eq!(list["commands"].as_array().unwrap().len(), 1);
    let schema = harness.success(&["commands", "schema", "action.click", "--side", "input"]);
    assert!(Path::new(schema["absolute_path"].as_str().unwrap()).is_file());
    let error = harness.success(&["commands", "errors", "foreground_required"]);
    assert_eq!(error["code"], "foreground_required");
}

#[test]
fn unknown_registry_identity_returns_catalogued_unknown_command() {
    let root = tempfile::tempdir().unwrap();
    for args in [
        ["commands", "get", "common.press"].as_slice(),
        ["commands", "schema", "common.press", "--side", "input"].as_slice(),
    ] {
        let output = Command::new(CLI)
            .args(args)
            .env("MANUVRA_TMPDIR", root.path().join("tmp"))
            .env("MANUVRA_CONFIG_HOME", root.path().join("config"))
            .env("MANUVRA_NO_AUTOSTART", "1")
            .output()
            .unwrap();
        assert_eq!(
            output.status.code(),
            Some(2),
            "stderr={}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(output.stdout.len() <= 4096);
        assert_eq!(output.stdout.last(), Some(&b'\n'));
        let result: Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(result["error"]["code"], "unknown_command");
        assert_eq!(result["error"]["recovery_command"], "manuvra commands list");
        assert_eq!(
            result["error"]["help_command"],
            "manuvra commands errors unknown_command"
        );
    }
}

#[test]
fn daemon_control_drains_busy_sessions_and_stops_without_killing_work() {
    let harness = Harness::new();
    let actor = harness.open("chrome_fake_1", None);
    let status = harness.success(&["daemon", "status"]);
    assert_eq!(status["running"], true);
    assert_eq!(status["admission"], "open");
    assert_eq!(status["active_sessions"][0]["session_id"], actor);

    let busy = harness.command(&["daemon", "stop"]);
    assert_eq!(busy.status.code(), Some(4));
    let busy: Value = serde_json::from_slice(&busy.stdout).unwrap();
    assert_eq!(busy["error"]["code"], "daemon_busy");
    assert!(busy["error"]["message"].as_str().unwrap().contains(&actor));

    let rejected = harness.command(&["open", "--target", "macos_fake_1"]);
    assert_eq!(rejected.status.code(), Some(4));
    let rejected: Value = serde_json::from_slice(&rejected.stdout).unwrap();
    assert_eq!(rejected["error"]["code"], "daemon_draining");

    assert_eq!(
        harness.success(&["close", "--session", &actor])["closed"],
        true
    );
    let stopped = harness.success(&["daemon", "stop"]);
    assert_eq!(stopped["stopped"], true);
    let deadline = Instant::now() + Duration::from_secs(3);
    while harness.socket_path().exists() && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(5));
    }
    assert!(!harness.socket_path().exists());
    assert_eq!(harness.success(&["daemon", "status"])["running"], false);
}

#[test]
fn typed_precondition_error_sets_shell_class_without_partial_effects() {
    let harness = Harness::new();
    let actor = harness.open("macos_fake_1", None);
    let output = harness.command(&["press", "--session", &actor, "--key", "Enter"]);
    assert_eq!(output.status.code(), Some(4));
    assert!(output.stdout.len() <= 4096);
    let result: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(result["error"]["code"], "foreground_required");
    assert_eq!(result["outcome"], "not_performed");
    assert_eq!(result["delivery"], "not_dispatched");
    assert!(result["observation"]["screenshot_path"].is_null());
}

#[test]
fn documented_cancel_request_id_reaches_the_original_public_invocation() {
    let harness = Harness::new();
    let actor = harness.open("chrome_fake_1", None);
    let blocked = harness.spawn_command(&[
        "raw",
        "cdp",
        "--session",
        &actor,
        "--intent",
        "action",
        "--method",
        "Fake.block",
        "--params",
        "{}",
        "--request-id",
        "qa_cancel_target",
        "--timeout-ms",
        "1000",
    ]);
    thread::sleep(Duration::from_millis(40));
    let cancellation = harness.command(&[
        "cancel",
        "--session",
        &actor,
        "--request-id",
        "qa_cancel_target",
    ]);
    let _ = harness.command(&["close", "--session", &actor, "--cancel-running"]);
    let terminal = blocked.wait_with_output().unwrap();

    assert_eq!(cancellation.status.code(), Some(0));
    let acknowledgement: Value = serde_json::from_slice(&cancellation.stdout).unwrap();
    assert_eq!(acknowledgement["disposition"], "cancellation_requested");
    assert_eq!(terminal.status.code(), Some(6));
    let terminal: Value = serde_json::from_slice(&terminal.stdout).unwrap();
    assert_eq!(terminal["error"]["code"], "cancelled");
}

#[test]
fn public_deadline_returns_the_typed_terminal_result_before_transport_closes() {
    let harness = Harness::new();
    let actor = harness.open("chrome_fake_1", None);
    let output = harness.command(&[
        "raw",
        "cdp",
        "--session",
        &actor,
        "--intent",
        "action",
        "--method",
        "Fake.block",
        "--params",
        "{}",
        "--timeout-ms",
        "100",
    ]);

    assert_eq!(output.status.code(), Some(6));
    let terminal: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(terminal["error"]["code"], "timed_out");
    assert_eq!(terminal["outcome"], "uncertain");
}
