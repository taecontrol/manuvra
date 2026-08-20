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
    daemon: Option<Child>,
}

impl Harness {
    fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        let temporary = root.path().join("tmp");
        let config = root.path().join("config");
        let mut harness = Self {
            _root: root,
            temporary,
            config,
            daemon: None,
        };
        harness.start_daemon();
        harness
    }

    fn start_daemon(&mut self) {
        let child = Command::new(DAEMON)
            .env("MANUVRA_TMPDIR", &self.temporary)
            .env("MANUVRA_CONFIG_HOME", &self.config)
            .env("MANUVRA_TEST_FAKE_ADAPTER", "1")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
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
}

impl Drop for Harness {
    fn drop(&mut self) {
        if let Some(mut daemon) = self.daemon.take() {
            let _ = daemon.kill();
            let _ = daemon.wait();
        }
    }
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
    assert_eq!(targets["targets"].as_array().unwrap().len(), 2);
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
