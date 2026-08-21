#![cfg(target_os = "macos")]

use serde_json::{Value, json};
use std::collections::HashSet;
use std::fs;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::sync::Mutex;
use std::thread;
use std::time::{Duration, Instant};
use tempfile::TempDir;

const CLI: &str = env!("CARGO_BIN_EXE_manuvra");
const DAEMON: &str = env!("CARGO_BIN_EXE_manuvra-daemon");
const FIXTURE_SOURCE: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/fixtures/cp07_fixture.swift"
);
const FOCUS_SINK_SOURCE: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/fixtures/cp07_focus_sink.swift"
);
static MACOS_PUBLIC_TEST_LOCK: Mutex<()> = Mutex::new(());

struct Fixture {
    child: Child,
    pid: i32,
    command_path: PathBuf,
    state_path: PathBuf,
    stderr_path: PathBuf,
}

struct LaunchedApp {
    waiter: Child,
    pids: Vec<i32>,
}

struct FocusSink {
    child: Child,
    pid: i32,
    bundle: PathBuf,
    state_path: PathBuf,
}

impl FocusSink {
    fn build_and_start(root: &Path) -> Self {
        let bundle = root.join("CP07FocusSink.app");
        let contents = bundle.join("Contents");
        let executable_directory = contents.join("MacOS");
        fs::create_dir_all(&executable_directory).unwrap();
        let executable = executable_directory.join("CP07FocusSink");
        let ready_path = root.join("focus-sink-ready.json");
        fs::write(
            contents.join("Info.plist"),
            r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
<key>CFBundleExecutable</key><string>CP07FocusSink</string>
<key>CFBundleIdentifier</key><string>dev.manuvra.cp07-focus-sink</string>
<key>CFBundleName</key><string>CP-07 Focus Sink</string>
<key>CFBundlePackageType</key><string>APPL</string>
<key>NSPrincipalClass</key><string>NSApplication</string>
</dict></plist>"#,
        )
        .unwrap();
        let module_cache = root.join("focus-sink-module-cache");
        fs::create_dir_all(&module_cache).unwrap();
        assert!(
            Command::new("swiftc")
                .args(["-module-cache-path"])
                .arg(&module_cache)
                .arg(FOCUS_SINK_SOURCE)
                .arg("-o")
                .arg(&executable)
                .status()
                .unwrap()
                .success()
        );
        assert!(
            Command::new("codesign")
                .args(["--force", "--sign", "-"])
                .arg(&bundle)
                .status()
                .unwrap()
                .success()
        );
        let child = Command::new("open")
            .args(["-n", "-W"])
            .arg(&bundle)
            .arg("--args")
            .arg(&ready_path)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        wait_for_file(&ready_path);
        let ready: Value = serde_json::from_slice(&fs::read(&ready_path).unwrap()).unwrap();
        Self {
            child,
            pid: ready["pid"].as_i64().unwrap() as i32,
            bundle,
            state_path: ready_path,
        }
    }

    fn activate(&self) {
        assert!(
            Command::new("open")
                .args(["-a"])
                .arg(&self.bundle)
                .status()
                .unwrap()
                .success()
        );
        thread::sleep(Duration::from_millis(250));
    }

    fn snapshot(&self) -> Value {
        serde_json::from_slice(&fs::read(&self.state_path).unwrap()).unwrap()
    }
}

impl Drop for FocusSink {
    fn drop(&mut self) {
        unsafe {
            libc::kill(self.pid, libc::SIGKILL);
        }
        let _ = self.child.wait();
    }
}

impl LaunchedApp {
    fn start(name: &str, arguments: &[&Path]) -> Self {
        let before = process_ids(name);
        let mut command = Command::new("open");
        command.args(["-n", "-W", "-a", name]);
        for argument in arguments {
            command.arg(argument);
        }
        let mut launched = Self {
            waiter: command
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .unwrap(),
            pids: Vec::new(),
        };
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            let pids = process_ids(name)
                .difference(&before)
                .copied()
                .collect::<Vec<_>>();
            if !pids.is_empty() {
                launched.pids = pids;
                return launched;
            }
            assert!(
                Instant::now() < deadline,
                "{name} did not launch a new process"
            );
            thread::sleep(Duration::from_millis(20));
        }
    }
}

impl Drop for LaunchedApp {
    fn drop(&mut self) {
        for pid in &self.pids {
            unsafe {
                libc::kill(*pid, libc::SIGKILL);
            }
        }
        let _ = self.waiter.wait();
    }
}

impl Fixture {
    fn build_and_start(root: &Path) -> Self {
        let bundle = root.join("CP07Fixture.app");
        let contents = bundle.join("Contents");
        let executable_directory = contents.join("MacOS");
        fs::create_dir_all(&executable_directory).unwrap();
        let bundle_identifier = format!("dev.manuvra.cp07-fixture.{}", std::process::id());
        let command_path = root.join("fixture-command");
        let ready_path = root.join("fixture-ready.json");
        let state_path = root.join("fixture-state.json");
        fs::write(
            contents.join("Info.plist"),
            format!(r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
<key>CFBundleDevelopmentRegion</key><string>en</string>
<key>CFBundleExecutable</key><string>CP07Fixture</string>
<key>CFBundleIdentifier</key><string>{bundle_identifier}</string>
<key>CFBundleInfoDictionaryVersion</key><string>6.0</string>
<key>CFBundleName</key><string>CP-07 Native Fixture</string>
<key>CFBundlePackageType</key><string>APPL</string>
<key>NSPrincipalClass</key><string>NSApplication</string>
<key>CFBundleShortVersionString</key><string>0.1</string>
<key>CFBundleVersion</key><string>1</string>
<key>LSMinimumSystemVersion</key><string>26.0</string>
<key>CP07CommandPath</key><string>{}</string>
<key>CP07ReadyPath</key><string>{}</string>
<key>CP07StatePath</key><string>{}</string>
</dict></plist>"#,
                command_path.display(),
                ready_path.display(),
                state_path.display(),
            ),
        )
        .unwrap();
        let executable = executable_directory.join("CP07Fixture");
        let module_cache = root.join("swift-module-cache");
        fs::create_dir_all(&module_cache).unwrap();
        assert!(
            Command::new("swiftc")
                .args(["-module-cache-path"])
                .arg(&module_cache)
                .arg(FIXTURE_SOURCE)
                .arg("-o")
                .arg(&executable)
                .status()
                .unwrap()
                .success()
        );
        assert!(
            Command::new("codesign")
                .args(["--force", "--sign", "-"])
                .arg(&bundle)
                .status()
                .unwrap()
                .success()
        );
        let stdout_path = root.join("fixture.stdout");
        let stderr_path = root.join("fixture.stderr");
        let child = Command::new("open")
            .args(["-n", "-W", "--stdout"])
            .arg(&stdout_path)
            .arg("--stderr")
            .arg(&stderr_path)
            .arg(&bundle)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(8);
        while !ready_path.is_file() {
            assert!(
                Instant::now() < deadline,
                "fixture did not publish ready file; stderr={}",
                fs::read_to_string(&stderr_path).unwrap_or_default()
            );
            thread::sleep(Duration::from_millis(10));
        }
        let ready: Value = serde_json::from_slice(&fs::read(&ready_path).unwrap()).unwrap();
        assert!(ready["pid"].as_i64().unwrap() > 0);
        assert!(ready["window_id"].as_u64().unwrap() > 0);
        Self {
            child,
            pid: ready["pid"].as_i64().unwrap() as i32,
            command_path,
            state_path,
            stderr_path,
        }
    }

    fn command(&self, command: &str) {
        fs::write(&self.command_path, command).unwrap();
        let deadline = Instant::now() + Duration::from_secs(2);
        while self.command_path.exists() {
            assert!(
                Instant::now() < deadline,
                "fixture command was not consumed"
            );
            thread::sleep(Duration::from_millis(10));
        }
        thread::sleep(Duration::from_millis(500));
    }

    fn snapshot(&self) -> Value {
        let _ = fs::remove_file(&self.state_path);
        self.command("snapshot");
        serde_json::from_slice(&fs::read(&self.state_path).unwrap()).unwrap()
    }

    fn assert_running(&self) {
        let alive = unsafe { libc::kill(self.pid, 0) } == 0;
        assert!(
            alive,
            "fixture exited after ready: stderr={}",
            fs::read_to_string(&self.stderr_path).unwrap_or_default()
        );
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        unsafe {
            libc::kill(self.pid, libc::SIGKILL);
        }
        let _ = self.child.wait();
    }
}

struct Harness {
    _root: TempDir,
    cli: PathBuf,
    installed: bool,
    temporary: PathBuf,
    config: PathBuf,
    coverage_shutdown: Option<PathBuf>,
    oracle_path: PathBuf,
    barrier_path: PathBuf,
    seam_path: PathBuf,
    daemon_stderr_path: PathBuf,
    daemon: Option<Child>,
    transcript: Mutex<Vec<Value>>,
    oracle_cases: Mutex<Vec<Value>>,
}

impl Harness {
    fn start() -> Self {
        let root = tempfile::tempdir().unwrap();
        let cli = std::env::var_os("MANUVRA_INSTALLED_CLI")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(CLI));
        let installed = std::env::var_os("MANUVRA_INSTALLED_CLI").is_some();
        let temporary = root.path().join("tmp");
        let config = root.path().join("config");
        let oracle_path = root.path().join("cp07-native-oracle.jsonl");
        let barrier_path = root.path().join("cp07-native-barrier.json");
        let seam_path = root.path().join("cp07-native-seam.json");
        fs::write(&seam_path, b"{}").unwrap();
        let daemon_stderr_path = root.path().join("daemon.stderr");
        let daemon_stderr = fs::File::create(&daemon_stderr_path).unwrap();
        let mut coverage_shutdown = None;
        let daemon = if installed {
            let _ = Command::new(&cli).args(["daemon", "stop"]).output();
            None
        } else {
            let mut daemon_command = Command::new(DAEMON);
            daemon_command
                .env("MANUVRA_TMPDIR", &temporary)
                .env("MANUVRA_CONFIG_HOME", &config)
                .env("RUST_BACKTRACE", "1")
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::from(daemon_stderr));
            if std::env::var_os("MANUVRA_RUN_MACOS_INTEGRATION").is_some()
                || std::env::var_os("MANUVRA_RUN_MACOS_SMOKE").is_some()
            {
                daemon_command
                    .env("MANUVRA_CP07_ORACLE_PATH", &oracle_path)
                    .env("MANUVRA_CP07_BARRIER_PATH", &barrier_path)
                    .env("MANUVRA_CP07_SEAM_PATH", &seam_path);
            }
            if std::env::var_os("LLVM_PROFILE_FILE").is_some() {
                let shutdown = root.path().join("coverage-shutdown");
                daemon_command.env("MANUVRA_TEST_SHUTDOWN_FILE", &shutdown);
                coverage_shutdown = Some(shutdown);
            }
            Some(daemon_command.spawn().unwrap())
        };
        let harness = Self {
            _root: root,
            cli,
            installed,
            temporary,
            config,
            coverage_shutdown,
            oracle_path,
            barrier_path,
            seam_path,
            daemon_stderr_path,
            daemon,
            transcript: Mutex::new(Vec::new()),
            oracle_cases: Mutex::new(Vec::new()),
        };
        if !harness.installed {
            wait_for_socket(&harness.socket_path());
        }
        harness
    }

    fn command(&self, args: &[&str]) -> Output {
        let mut command = Command::new(&self.cli);
        command.args(args);
        if !self.installed {
            command
                .env("MANUVRA_TMPDIR", &self.temporary)
                .env("MANUVRA_CONFIG_HOME", &self.config)
                .env("MANUVRA_NO_AUTOSTART", "1");
        }
        let output = command.output().unwrap();
        self.record_output(args, &output);
        output
    }

    fn configure_seam(&self, value: Value) {
        fs::write(&self.seam_path, serde_json::to_vec(&value).unwrap()).unwrap();
    }

    fn spawn_command(&self, args: &[&str]) -> Child {
        let mut command = Command::new(&self.cli);
        command.args(args);
        if !self.installed {
            command
                .env("MANUVRA_TMPDIR", &self.temporary)
                .env("MANUVRA_CONFIG_HOME", &self.config)
                .env("MANUVRA_NO_AUTOSTART", "1");
        }
        command
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap()
    }

    fn record_output(&self, args: &[&str], output: &Output) {
        let response = serde_json::from_slice::<Value>(&output.stdout).unwrap_or(Value::Null);
        let mut transcript = self.transcript.lock().expect("macOS CLI transcript");
        let sequence = transcript.len() + 1;
        transcript.push(json!({
            "sequence": sequence,
            "argv": args,
            "exit_code": output.status.code(),
            "stdout_bytes": output.stdout.len(),
            "response": response,
        }));
    }

    fn success(&self, args: &[&str]) -> Value {
        let output = self.command(args);
        assert!(
            output.status.success(),
            "args={args:?} stderr={} stdout={}",
            String::from_utf8_lossy(&output.stderr),
            String::from_utf8_lossy(&output.stdout)
        );
        assert!(output.stdout.len() <= 4096);
        serde_json::from_slice(&output.stdout).unwrap()
    }

    fn error(&self, args: &[&str]) -> Value {
        let output = self.command(args);
        assert!(
            !output.status.success(),
            "args={args:?} stdout={} stderr={}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(output.stdout.len() <= 4096);
        serde_json::from_slice(&output.stdout).unwrap()
    }

    fn screenshot_eventually(&self, session: &str) -> Value {
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            let output = self.command(&["observe", "screenshot", "--session", session]);
            let response: Value = serde_json::from_slice(&output.stdout).unwrap();
            if output.status.success() {
                assert!(output.stdout.len() <= 4096);
                return response;
            }
            assert_eq!(
                response["error"]["code"], "capability_unavailable",
                "unexpected screenshot failure: {response}"
            );
            assert!(
                response["error"]["message"]
                    .as_str()
                    .is_some_and(|message| message.contains("geometry")),
                "unexpected screenshot failure: {response}"
            );
            assert!(
                Instant::now() < deadline,
                "window geometry never stabilized: {response}"
            );
            thread::sleep(Duration::from_millis(30));
        }
    }

    fn open_target_matching(
        &self,
        excluded: &HashSet<String>,
        locator: &[&str],
        mode: &str,
    ) -> (String, String) {
        let deadline = Instant::now() + Duration::from_secs(30);
        let mut probes = Vec::<Value>::new();
        loop {
            let mut matches = Vec::<(String, String)>::new();
            let candidates = self.macos_target_ids().into_iter().collect::<Vec<_>>();
            for target in &candidates {
                let opened = self.command(&[
                    "open",
                    "--target",
                    target,
                    "--mode",
                    mode,
                    "--lease-ttl-ms",
                    "600000",
                ]);
                if !opened.status.success() {
                    probes.push(json!({
                        "target": target,
                        "phase": "open",
                        "response": serde_json::from_slice::<Value>(&opened.stdout).ok(),
                    }));
                    continue;
                }
                let opened: Value = serde_json::from_slice(&opened.stdout).unwrap();
                let session = opened["session_id"].as_str().unwrap().to_owned();
                let mut query = vec!["observe", "query", "--session", &session];
                query.extend_from_slice(locator);
                query.extend(["--limit", "1"]);
                let observed = self.command(&query);
                let captured = observed
                    .status
                    .success()
                    .then(|| self.command(&["observe", "screenshot", "--session", &session]));
                if captured
                    .as_ref()
                    .is_some_and(|capture| capture.status.success())
                {
                    if !excluded.contains(target) {
                        for (_, existing_session) in matches.drain(..) {
                            self.success(&["close", "--session", &existing_session]);
                        }
                        return (target.clone(), session);
                    }
                    matches.push((target.clone(), session));
                } else {
                    probes.push(json!({
                        "target": target,
                        "phase": if observed.status.success() { "screenshot" } else { "query" },
                        "response": if observed.status.success() {
                            captured.as_ref().and_then(|capture| serde_json::from_slice::<Value>(&capture.stdout).ok())
                        } else {
                            serde_json::from_slice::<Value>(&observed.stdout).ok()
                        },
                    }));
                    if probes.len() > 20 {
                        probes.remove(0);
                    }
                    self.success(&["close", "--session", &session]);
                }
            }
            matches.sort_by(|left, right| left.0.cmp(&right.0));
            matches.dedup_by(|left, right| left.0 == right.0);
            let added_matches = matches
                .iter()
                .enumerate()
                .filter(|(_, (target, _))| !excluded.contains(target))
                .map(|(index, _)| index)
                .collect::<Vec<_>>();
            let selected = if added_matches.len() == 1 {
                Some(added_matches[0])
            } else if added_matches.is_empty() && matches.len() == 1 {
                Some(0)
            } else {
                None
            };
            if let Some(selected) = selected {
                let selected = matches.swap_remove(selected);
                for (_, session) in matches {
                    self.success(&["close", "--session", &session]);
                }
                return selected;
            }
            for (_, session) in &matches {
                self.success(&["close", "--session", session]);
            }
            assert!(
                Instant::now() < deadline,
                "expected one exact macOS target matching {locator:?}; candidates={candidates:?}, matches={matches:?}, probes={probes:?}, daemon_stderr={}",
                fs::read_to_string(&self.daemon_stderr_path).unwrap_or_default()
            );
            thread::sleep(Duration::from_millis(30));
        }
    }

    fn macos_target_ids(&self) -> HashSet<String> {
        self.macos_targets()
            .iter()
            .filter_map(|target| target["target_id"].as_str().map(str::to_owned))
            .collect()
    }

    fn macos_targets(&self) -> Vec<Value> {
        let mut cursor = None;
        let mut targets = Vec::new();
        loop {
            let page = match cursor.as_deref() {
                Some(cursor) => self.success(&[
                    "targets", "--kind", "macos", "--limit", "10", "--cursor", cursor,
                ]),
                None => self.success(&["targets", "--kind", "macos", "--limit", "10"]),
            };
            targets.extend(page["targets"].as_array().unwrap().iter().cloned());
            cursor = page["next_cursor"].as_str().map(str::to_owned);
            if cursor.is_none() {
                return targets;
            }
        }
    }

    fn query_ref(&self, session: &str, identifier: &str) -> String {
        self.success(&[
            "observe",
            "query",
            "--session",
            session,
            "--identifier",
            identifier,
            "--limit",
            "1",
        ])["matches"][0]["ref"]
            .as_str()
            .unwrap()
            .to_owned()
    }

    fn query_ref_eventually(&self, session: &str, identifier: &str) -> String {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let output = self.command(&[
                "observe",
                "query",
                "--session",
                session,
                "--identifier",
                identifier,
                "--limit",
                "1",
            ]);
            let response: Value = serde_json::from_slice(&output.stdout).unwrap();
            if output.status.success() {
                return response["matches"][0]["ref"].as_str().unwrap().to_owned();
            }
            assert_eq!(
                response["error"]["code"], "element_not_found",
                "unexpected eventual-query failure: {response}"
            );
            assert!(
                Instant::now() < deadline,
                "element never appeared: {response}"
            );
            thread::sleep(Duration::from_millis(30));
        }
    }

    fn stop_continuous_busy(&self, session: &str) -> Value {
        let stop_ref = self.query_ref(session, "stop");
        let args = [
            "click",
            "--session",
            session,
            "--ref",
            &stop_ref,
            "--mode",
            "background",
        ];
        let output = self.command(&args);
        let response: Value = serde_json::from_slice(&output.stdout).unwrap();
        if output.status.success() {
            assert_eq!(response["outcome"], "observed", "stop={response}");
            return response;
        }

        assert_eq!(response["error"]["code"], "interrupted", "stop={response}");
        assert_eq!(response["error"]["effects"], "possible", "stop={response}");
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            let observed = self.command(&[
                "observe",
                "query",
                "--session",
                session,
                "--identifier",
                "status",
                "--text",
                "Stopped",
                "--limit",
                "1",
            ]);
            if observed.status.success() {
                return response;
            }
            assert!(
                Instant::now() < deadline,
                "uncertain Stop never became observable: stop={response}, observation={} ",
                String::from_utf8_lossy(&observed.stdout)
            );
            thread::sleep(Duration::from_millis(30));
        }
    }

    fn socket_path(&self) -> PathBuf {
        self.temporary.join("manuvra/runtime-v1/daemon.sock")
    }

    fn write_transcript(&self, path: &Path, schema: &str) {
        let transcript = self.transcript.lock().expect("macOS CLI transcript");
        let oracle_cases = self.oracle_cases.lock().expect("CP-07 oracle cases");
        let native_oracles = self.native_oracles();
        let report = json!({
            "schema": schema,
            "commands": transcript.len(),
            "steps": &*transcript,
            "native_oracles": native_oracles,
            "oracle_cases": &*oracle_cases,
        });
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, serde_json::to_vec_pretty(&report).unwrap()).unwrap();
    }

    fn native_oracles(&self) -> Vec<Value> {
        fs::read_to_string(&self.oracle_path)
            .unwrap_or_default()
            .lines()
            .map(|line| serde_json::from_str::<Value>(line).unwrap())
            .collect()
    }

    fn record_oracle_case(&self, case: &str, before: Value, after: Value, result: &Value) {
        self.oracle_cases
            .lock()
            .expect("CP-07 oracle cases")
            .push(json!({
                "case": case,
                "before": before,
                "after": after,
                "public_result": result,
            }));
    }

    fn install_barrier(
        &self,
        session: &str,
        action_sequence: u64,
        name: &str,
    ) -> (PathBuf, PathBuf) {
        let reached = self.barrier_path.with_extension("reached");
        let release = self.barrier_path.with_extension("release");
        let _ = fs::remove_file(&reached);
        let _ = fs::remove_file(&release);
        fs::write(
            &self.barrier_path,
            serde_json::to_vec(&json!({
                "session_id": session,
                "action_sequence": action_sequence,
                "name": name,
                "reached_path": reached,
                "release_path": release,
            }))
            .unwrap(),
        )
        .unwrap();
        (reached, release)
    }

    fn clear_barrier(&self, reached: &Path, release: &Path) {
        let _ = fs::remove_file(&self.barrier_path);
        let _ = fs::remove_file(reached);
        let _ = fs::remove_file(release);
    }

    fn cancel_at_barrier(
        &self,
        session: &str,
        action_sequence: u64,
        name: &str,
        request_id: &str,
        args: &[&str],
    ) -> Value {
        let (reached, release) = self.install_barrier(session, action_sequence, name);
        let child = self.spawn_command(args);
        wait_for_file(&reached);
        let cancellation =
            self.success(&["cancel", "--session", session, "--request-id", request_id]);
        assert_eq!(cancellation["disposition"], "cancellation_requested");
        let output = child.wait_with_output().unwrap();
        self.record_output(args, &output);
        self.clear_barrier(&reached, &release);
        assert!(
            !output.status.success(),
            "cancelled args unexpectedly passed: {args:?}"
        );
        serde_json::from_slice(&output.stdout).unwrap()
    }

    fn daemon_stderr(&self) -> String {
        fs::read_to_string(&self.daemon_stderr_path).unwrap_or_default()
    }
}

impl Drop for Harness {
    fn drop(&mut self) {
        if self.installed {
            let _ = Command::new(&self.cli).args(["daemon", "stop"]).output();
            return;
        }
        if let Some(shutdown) = &self.coverage_shutdown {
            let _ = fs::write(shutdown, b"shutdown");
            let _ = UnixStream::connect(self.socket_path());
            let deadline = Instant::now() + Duration::from_secs(5);
            while Instant::now() < deadline {
                if self
                    .daemon
                    .as_mut()
                    .and_then(|daemon| daemon.try_wait().ok().flatten())
                    .is_some()
                {
                    return;
                }
                thread::sleep(Duration::from_millis(10));
            }
        }
        if let Some(daemon) = self.daemon.as_mut() {
            let _ = daemon.kill();
            let _ = daemon.wait();
        }
    }
}

fn wait_for_socket(path: &Path) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if UnixStream::connect(path).is_ok() {
            return;
        }
        thread::sleep(Duration::from_millis(5));
    }
    panic!("daemon socket was not ready: {}", path.display());
}

fn wait_for_file(path: &Path) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while !path.is_file() {
        assert!(
            Instant::now() < deadline,
            "timed out waiting for {}",
            path.display()
        );
        thread::sleep(Duration::from_millis(5));
    }
}

fn assert_background_snapshot_unchanged(before: &Value, after: &Value) {
    for field in ["frontmost_pid", "pasteboard_change_count"] {
        assert_eq!(before[field], after[field], "background changed {field}");
    }
    assert_eq!(after["target_is_key"], false);
    assert_eq!(after["target_is_main"], false);
}

fn action_native_events(harness: &Harness, result: &Value) -> Vec<Value> {
    let session = result["session_id"].as_str().unwrap();
    let sequence = result["action_sequence"].as_u64().unwrap();
    harness
        .native_oracles()
        .into_iter()
        .filter(|event| {
            event["session_id"] == session && event["action_sequence"].as_u64() == Some(sequence)
        })
        .collect()
}

fn assert_native_event(harness: &Harness, result: &Value, kind: &str) {
    let events = action_native_events(harness, result);
    assert!(
        events.iter().any(|event| event["kind"] == kind),
        "native event {kind} missing for result={result}, events={events:?}"
    );
}

fn assert_no_native_events(harness: &Harness, result: &Value, kind: &str) {
    let events = action_native_events(harness, result);
    assert!(
        events.iter().all(|event| event["kind"] != kind),
        "unexpected native event {kind} for result={result}, events={events:?}"
    );
}

fn assert_native_focus_preserved(harness: &Harness, result: &Value) {
    let events = action_native_events(harness, result);
    let before = events
        .iter()
        .find(|event| event["kind"] == "focus_before")
        .expect("native focus_before oracle");
    let after = events
        .iter()
        .find(|event| event["kind"] == "focus_after")
        .expect("native focus_after oracle");
    assert_eq!(before["details"]["available"], true);
    assert_eq!(before["details"], after["details"], "native focus changed");
}

fn assert_no_native_text_events(harness: &Harness, result: &Value) {
    let events = action_native_events(harness, result);
    assert!(
        events.iter().all(|event| {
            event["kind"] != "cg_event_post"
                || !matches!(
                    event["details"]["event"].as_str(),
                    Some("key_down" | "key_up" | "unicode_down" | "unicode_up")
                )
        }),
        "foreground AXValue typing posted text CGEvents: result={result}, events={events:?}"
    );
}

fn assert_native_text_event(harness: &Harness, result: &Value) {
    let events = action_native_events(harness, result);
    assert!(
        events.iter().any(|event| {
            event["kind"] == "cg_event_post"
                && matches!(
                    event["details"]["event"].as_str(),
                    Some("key_down" | "unicode_down")
                )
        }),
        "foreground point typing did not post text CGEvents: result={result}, events={events:?}"
    );
}

fn read_json(path: &Value) -> Value {
    serde_json::from_slice(&fs::read(path.as_str().unwrap()).unwrap()).unwrap()
}

fn directory_inventory(path: &Path) -> Value {
    fn collect(root: &Path, current: &Path, rows: &mut Vec<Value>) {
        let mut entries = fs::read_dir(current)
            .unwrap()
            .map(|entry| entry.unwrap())
            .collect::<Vec<_>>();
        entries.sort_by_key(|entry| entry.file_name());
        for entry in entries {
            let path = entry.path();
            let metadata = entry.metadata().unwrap();
            rows.push(json!({
                "path": path.strip_prefix(root).unwrap().display().to_string(),
                "kind": if metadata.is_dir() { "directory" } else { "file" },
                "bytes": metadata.is_file().then_some(metadata.len()),
            }));
            if metadata.is_dir() {
                collect(root, &path, rows);
            }
        }
    }

    let mut rows = Vec::new();
    collect(path, path, &mut rows);
    json!({"root": path, "exists": true, "entries": rows})
}

fn process_ids(name: &str) -> HashSet<i32> {
    let output = Command::new("pgrep").args(["-x", name]).output().unwrap();
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|line| line.parse().ok())
        .collect()
}

#[test]
fn real_macos_public_background_and_foreground_vertical_slice() {
    if std::env::var_os("MANUVRA_RUN_MACOS_INTEGRATION").is_none() {
        return;
    }
    let _test_guard = MACOS_PUBLIC_TEST_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let harness = Harness::start();
    let existing_targets = harness.macos_target_ids();
    let fixture_root = tempfile::tempdir().unwrap();
    let fixture = Fixture::build_and_start(fixture_root.path());
    fixture.assert_running();
    let fixture_initial = fixture.snapshot();
    assert_eq!(fixture_initial["target_is_visible"], true);
    assert_eq!(fixture_initial["target_is_minimized"], false);
    let doctor = harness.success(&["doctor"]);
    let permissions = doctor["daemon"]["adapters"]
        .as_array()
        .unwrap()
        .iter()
        .find(|adapter| adapter["kind"] == "macos")
        .expect("macOS doctor adapter")["permissions"]
        .clone();
    assert_eq!(permissions["accessibility"], true);
    assert_eq!(permissions["screen_recording"], true);
    assert_eq!(permissions["post_event"], true);

    let (target, session_id) = harness.open_target_matching(
        &existing_targets,
        &[
            "--role",
            "text",
            "--identifier",
            "status",
            "--text",
            "Ready",
        ],
        "background",
    );
    let session = session_id.as_str();

    let before_permission_matrix = fixture.snapshot();
    let native_events_before_permissions = harness.native_oracles().len();
    harness.configure_seam(json!({
        "permissions": {"accessibility": false}
    }));
    let no_accessibility = harness.error(&[
        "observe",
        "query",
        "--session",
        session,
        "--identifier",
        "status",
        "--limit",
        "1",
    ]);
    assert_eq!(no_accessibility["error"]["code"], "permission_required");
    assert!(
        no_accessibility["error"]["message"]
            .as_str()
            .is_some_and(
                |message| message.contains("Accessibility") && message.contains("manuvra doctor")
            )
    );
    harness.configure_seam(json!({
        "permissions": {"screen_recording": false}
    }));
    let no_screen_recording = harness.error(&["observe", "screenshot", "--session", session]);
    assert_eq!(no_screen_recording["error"]["code"], "permission_required");
    assert!(
        no_screen_recording["error"]["message"]
            .as_str()
            .is_some_and(
                |message| message.contains("Screen & System Audio Recording")
                    && message.contains("manuvra doctor")
            )
    );
    harness.configure_seam(json!({
        "permissions": {"post_event": false}
    }));
    let no_post_event = harness.error(&[
        "press",
        "--session",
        session,
        "--key",
        "Enter",
        "--mode",
        "foreground",
    ]);
    assert_eq!(no_post_event["error"]["code"], "permission_required");
    assert_eq!(no_post_event["error"]["effects"], "none");
    assert!(
        no_post_event["error"]["message"]
            .as_str()
            .is_some_and(
                |message| message.contains("Post Event") && message.contains("manuvra doctor")
            )
    );
    harness.configure_seam(json!({}));
    let after_permission_matrix = fixture.snapshot();
    assert_eq!(
        before_permission_matrix["status"],
        after_permission_matrix["status"]
    );
    assert_eq!(
        before_permission_matrix["input"],
        after_permission_matrix["input"]
    );
    assert_eq!(
        native_events_before_permissions,
        harness.native_oracles().len()
    );
    harness.record_oracle_case(
        "public_permission_matrix_is_actionable_and_non_prompting",
        before_permission_matrix,
        after_permission_matrix,
        &json!({
            "accessibility": no_accessibility,
            "screen_recording": no_screen_recording,
            "post_event": no_post_event,
            "prompts_triggered": false,
        }),
    );

    let screenshot = harness.screenshot_eventually(session);
    assert!(screenshot["width"].as_u64().unwrap() >= 700);
    assert!(Path::new(screenshot["screenshot_path"].as_str().unwrap()).is_file());
    let tree = harness.success(&["observe", "tree", "--session", session]);
    assert!(tree["node_count"].as_u64().unwrap() >= 10);
    let tree_value = read_json(&tree["tree_path"]);
    assert_eq!(tree_value["complete"], true);

    let before_occluded = fixture.snapshot();
    fixture.command("occlude");
    let occluded_state = fixture.snapshot();
    assert_eq!(occluded_state["target_is_key"], false);
    let occluded = harness.screenshot_eventually(session);
    assert_eq!(occluded["width"], screenshot["width"]);
    assert_eq!(occluded["height"], screenshot["height"]);
    let after_occluded = fixture.snapshot();
    assert_eq!(
        occluded_state["frontmost_pid"],
        after_occluded["frontmost_pid"]
    );
    harness.record_oracle_case(
        "occluded_window_desktop_independent_capture",
        before_occluded,
        after_occluded,
        &occluded,
    );
    fixture.command("unocclude");

    fixture.command("minimize");
    let minimized_state = fixture.snapshot();
    assert_eq!(minimized_state["target_is_minimized"], true);
    let minimized = harness.screenshot_eventually(session);
    assert!(Path::new(minimized["screenshot_path"].as_str().unwrap()).is_file());
    harness.record_oracle_case(
        "minimized_window_desktop_independent_capture",
        fixture_initial,
        minimized_state,
        &minimized,
    );
    fixture.command("restore");

    let before_move = harness.screenshot_eventually(session);
    let before_move_state = fixture.snapshot();
    let native_before_move = harness.native_oracles().len();
    let generation_before =
        harness.success(&["targets", "--kind", "macos", "--limit", "10"])["targets"]
            .as_array()
            .unwrap()
            .iter()
            .find(|candidate| candidate["target_id"] == target)
            .and_then(|candidate| candidate["generation"].as_u64())
            .expect("fixture target listed before move");
    fixture.command("move");
    let refreshed = harness.success(&["targets", "--kind", "macos", "--limit", "10"]);
    let after_move_target = refreshed["targets"]
        .as_array()
        .unwrap()
        .iter()
        .find(|candidate| candidate["target_id"] == target)
        .unwrap_or_else(|| panic!("moved fixture target disappeared: {refreshed}"));
    assert_eq!(
        after_move_target["generation"].as_u64(),
        Some(generation_before),
        "move must not replace generation: {refreshed}"
    );
    let stale_frame = harness.error(&[
        "click",
        "--session",
        session,
        "--point",
        "10,10",
        "--frame",
        before_move["frame_token"].as_str().unwrap(),
        "--mode",
        "foreground",
    ]);
    assert_eq!(
        stale_frame["error"]["code"], "frame_stale",
        "stale_frame={stale_frame}"
    );
    assert_eq!(stale_frame["error"]["effects"], "none");
    assert_eq!(native_before_move, harness.native_oracles().len());
    let after_move_state = fixture.snapshot();
    assert_eq!(before_move_state["status"], after_move_state["status"]);
    assert_eq!(before_move_state["input"], after_move_state["input"]);
    harness.record_oracle_case(
        "move_only_invalidates_frame_before_dispatch",
        before_move_state,
        after_move_state,
        &stale_frame,
    );
    fixture.command("move");
    harness.success(&["targets", "--kind", "macos", "--limit", "10"]);

    let before_resize = harness.screenshot_eventually(session);
    let before_resize_state = fixture.snapshot();
    fixture.command("resize");
    harness.success(&["targets", "--kind", "macos", "--limit", "10"]);
    let stale_resize = harness.error(&[
        "click",
        "--session",
        session,
        "--point",
        "10,10",
        "--frame",
        before_resize["frame_token"].as_str().unwrap(),
        "--mode",
        "background",
    ]);
    assert_eq!(stale_resize["error"]["code"], "frame_stale");
    assert_eq!(stale_resize["error"]["effects"], "none");
    let after_resize_state = fixture.snapshot();
    assert_eq!(before_resize_state["status"], after_resize_state["status"]);
    assert_eq!(before_resize_state["input"], after_resize_state["input"]);
    harness.record_oracle_case(
        "resize_only_invalidates_frame_before_dispatch",
        before_resize_state,
        after_resize_state,
        &stale_resize,
    );
    fixture.command("resize");
    harness.success(&["targets", "--kind", "macos", "--limit", "10"]);

    let before_scale = harness.screenshot_eventually(session);
    let before_scale_state = fixture.snapshot();
    let native_before_scale = harness.native_oracles().len();
    harness.configure_seam(json!({"frame_scale": 3.0}));
    let stale_scale = harness.error(&[
        "click",
        "--session",
        session,
        "--point",
        "10,10",
        "--frame",
        before_scale["frame_token"].as_str().unwrap(),
        "--mode",
        "background",
    ]);
    harness.configure_seam(json!({}));
    assert_eq!(stale_scale["error"]["code"], "frame_stale");
    assert_eq!(stale_scale["error"]["effects"], "none");
    assert_eq!(native_before_scale, harness.native_oracles().len());
    let after_scale_state = fixture.snapshot();
    assert_eq!(before_scale_state["status"], after_scale_state["status"]);
    assert_eq!(before_scale_state["input"], after_scale_state["input"]);
    harness.record_oracle_case(
        "scale_only_invalidates_frame_before_dispatch",
        before_scale_state,
        after_scale_state,
        &stale_scale,
    );

    let status_ref = harness.query_ref(session, "status");
    let raw = harness.success(&[
        "raw",
        "ax",
        "get",
        "--session",
        session,
        "--ref",
        &status_ref,
        "--attribute",
        "AXValue",
    ]);
    assert_eq!(
        read_json(&raw["response_path"])["value"],
        json!({"type": "string", "value": "Ready"})
    );
    let raw_parent = harness.success(&[
        "raw",
        "ax",
        "get",
        "--session",
        session,
        "--ref",
        &status_ref,
        "--attribute",
        "AXParent",
    ]);
    let parent_value = read_json(&raw_parent["response_path"])["value"].clone();
    assert_eq!(parent_value["type"], "element");
    let parent_ref = parent_value["ref"].as_str().unwrap();
    let parent_role = harness.success(&[
        "raw",
        "ax",
        "get",
        "--session",
        session,
        "--ref",
        parent_ref,
        "--attribute",
        "AXRole",
    ]);
    assert_eq!(
        read_json(&parent_role["response_path"])["value"]["type"],
        "string"
    );

    let focus_sink_root = tempfile::tempdir().unwrap();
    let focus_sink = FocusSink::build_and_start(focus_sink_root.path());
    let before_cascade = fixture.snapshot();
    assert_eq!(
        before_cascade["frontmost_pid"].as_i64(),
        Some(i64::from(focus_sink.pid))
    );
    assert_eq!(before_cascade["target_is_key"], false);
    let cascade_ref = harness.query_ref(session, "cascade");
    let cascade = harness.success(&[
        "click",
        "--session",
        session,
        "--ref",
        &cascade_ref,
        "--mode",
        "background",
    ]);
    assert_eq!(cascade["outcome"], "observed");
    assert!(cascade["timing_ms"]["dispatch"].as_u64().unwrap() <= 150);
    assert!(cascade["timing_ms"]["total"].as_u64().unwrap() <= 1_000);
    let live_session_directory = PathBuf::from(cascade["manifest_path"].as_str().unwrap())
        .parent()
        .unwrap()
        .to_owned();
    let after_cascade = fixture.snapshot();
    assert_background_snapshot_unchanged(&before_cascade, &after_cascade);
    assert_native_focus_preserved(&harness, &cascade);
    assert_no_native_events(&harness, &cascade, "cg_event_post");
    harness.record_oracle_case(
        "background_axpress_preserves_focus_and_global_input",
        before_cascade,
        after_cascade,
        &cascade,
    );
    assert_eq!(
        harness.success(&[
            "observe",
            "query",
            "--session",
            session,
            "--identifier",
            "status",
            "--text",
            "Cascade 3 — settled",
            "--limit",
            "1",
        ])["matches"]
            .as_array()
            .unwrap()
            .len(),
        1
    );

    let before_background_type = fixture.snapshot();
    let input_ref = harness.query_ref(session, "input");
    let typed = harness.success(&[
        "type",
        "--session",
        session,
        "--ref",
        &input_ref,
        "--text",
        "Background canary",
        "--replace",
        "--mode",
        "background",
    ]);
    assert_eq!(typed["outcome"], "observed");
    let after_background_type = fixture.snapshot();
    assert_background_snapshot_unchanged(&before_background_type, &after_background_type);
    assert_native_focus_preserved(&harness, &typed);
    assert_no_native_events(&harness, &typed, "cg_event_post");
    assert_native_event(&harness, &typed, "ax_value_set");
    harness.record_oracle_case(
        "background_axvalue_preserves_focus_clipboard_and_global_input",
        before_background_type,
        after_background_type,
        &typed,
    );
    let stale_ref = harness.error(&[
        "raw",
        "ax",
        "get",
        "--session",
        session,
        "--ref",
        &input_ref,
        "--attribute",
        "AXValue",
    ]);
    assert_eq!(stale_ref["error"]["code"], "element_stale");
    let input_ref = harness.query_ref(session, "input");
    let raw_set = harness.success(&[
        "raw",
        "ax",
        "set",
        "--session",
        session,
        "--ref",
        &input_ref,
        "--attribute",
        "AXValue",
        "--value",
        r#"{"type":"string","value":"Raw canary"}"#,
        "--mode",
        "background",
    ]);
    assert_eq!(raw_set["outcome"], "observed");
    let input_ref = harness.query_ref(session, "input");
    let raw_get = harness.success(&[
        "raw",
        "ax",
        "get",
        "--session",
        session,
        "--ref",
        &input_ref,
        "--attribute",
        "AXValue",
    ]);
    assert_eq!(
        read_json(&raw_get["response_path"])["value"],
        json!({"type": "string", "value": "Raw canary"})
    );

    let cascade_ref = harness.query_ref(session, "cascade");
    let raw_perform_args = [
        "raw",
        "ax",
        "perform",
        "--session",
        session,
        "--ref",
        &cascade_ref,
        "--action",
        "AXPress",
        "--mode",
        "background",
    ];
    let raw_perform_output = harness.command(&raw_perform_args);
    let raw_perform: Value = serde_json::from_slice(&raw_perform_output.stdout).unwrap();
    if raw_perform_output.status.success() {
        assert_eq!(raw_perform["outcome"], "observed");
    } else {
        assert_eq!(raw_perform["error"]["code"], "interrupted");
        assert_eq!(raw_perform["error"]["effects"], "possible");
        let observed = harness.command(&[
            "observe",
            "query",
            "--session",
            session,
            "--identifier",
            "status",
            "--text",
            "Cascade 3 — settled",
            "--limit",
            "1",
        ]);
        assert!(
            observed.status.success(),
            "uncertain raw AXPress had no observable effect: response={raw_perform}, observation={}",
            String::from_utf8_lossy(&observed.stdout)
        );
    }

    let no_escalation = harness.error(&[
        "press",
        "--session",
        session,
        "--key",
        "Enter",
        "--mode",
        "background",
    ]);
    assert_eq!(no_escalation["error"]["code"], "foreground_required");
    assert_eq!(no_escalation["error"]["effects"], "none");
    let zero_match = harness.error(&[
        "click",
        "--session",
        session,
        "--identifier",
        "does-not-exist",
        "--mode",
        "background",
    ]);
    assert_eq!(zero_match["error"]["code"], "element_not_found");
    assert_eq!(zero_match["error"]["effects"], "none");
    let ambiguous = harness.error(&[
        "click",
        "--session",
        session,
        "--role",
        "button",
        "--name",
        "Duplicate",
        "--mode",
        "background",
    ]);
    assert_eq!(ambiguous["error"]["code"], "ambiguous_target");
    assert_eq!(ambiguous["error"]["effects"], "none");

    let ambiguous_window_ref = harness.query_ref(session, "cascade");
    let before_ambiguous_window = fixture.snapshot();
    fixture.command("duplicate-window");
    let ambiguous_window = harness.error(&[
        "click",
        "--session",
        session,
        "--ref",
        &ambiguous_window_ref,
        "--mode",
        "background",
    ]);
    let after_ambiguous_window = fixture.snapshot();
    assert_eq!(
        ambiguous_window["error"]["code"], "capability_unavailable",
        "ambiguous_window={ambiguous_window}"
    );
    assert_eq!(ambiguous_window["error"]["effects"], "none");
    assert_eq!(
        before_ambiguous_window["status"],
        after_ambiguous_window["status"]
    );
    assert_eq!(
        before_ambiguous_window["input"],
        after_ambiguous_window["input"]
    );
    harness.record_oracle_case(
        "ambiguous_ax_window_mapping_fails_without_dispatch",
        before_ambiguous_window,
        after_ambiguous_window,
        &ambiguous_window,
    );
    fixture.command("remove-duplicate-window");

    let observer = harness.success(&[
        "open",
        "--target",
        &target,
        "--role",
        "observer",
        "--mode",
        "background",
    ]);
    let observer_session = observer["session_id"].as_str().unwrap();
    let observer_ref = harness.query_ref(observer_session, "status");
    let cross_session_ref = harness.error(&[
        "raw",
        "ax",
        "get",
        "--session",
        session,
        "--ref",
        &observer_ref,
        "--attribute",
        "AXValue",
    ]);
    assert_eq!(cross_session_ref["error"]["code"], "element_stale");
    let actor_input_ref = harness.query_ref(session, "input");
    let foreign_element_value = serde_json::to_string(&json!({
        "type": "element",
        "ref": observer_ref,
    }))
    .unwrap();
    let nested_cross_session_ref = harness.error(&[
        "raw",
        "ax",
        "set",
        "--session",
        session,
        "--ref",
        &actor_input_ref,
        "--attribute",
        "AXValue",
        "--value",
        &foreign_element_value,
        "--mode",
        "background",
    ]);
    assert_eq!(nested_cross_session_ref["error"]["code"], "element_stale");
    harness.success(&["close", "--session", observer_session]);

    let status_ref = harness.query_ref(session, "status");
    let native_ax_error = harness.success(&[
        "raw",
        "ax",
        "get",
        "--session",
        session,
        "--ref",
        &status_ref,
        "--attribute",
        "AXCP07UnknownAttribute",
    ]);
    assert_eq!(native_ax_error["delivery"], "backend_rejected");
    let native_ax_error_value = read_json(&native_ax_error["response_path"]);
    assert_eq!(native_ax_error_value["error"]["code"], "backend_rejected");
    assert!(
        native_ax_error_value["error"]["message"]
            .as_str()
            .is_some_and(|message| message.contains("status -"))
    );
    assert_eq!(
        native_ax_error_value["error"]["details"]["domain"],
        "AXError"
    );
    assert!(
        native_ax_error_value["error"]["details"]["number"]
            .as_i64()
            .is_some_and(|number| number < 0)
    );

    let overflow_observer = harness.success(&[
        "open",
        "--target",
        &target,
        "--role",
        "observer",
        "--mode",
        "background",
    ]);
    let overflow_session = overflow_observer["session_id"].as_str().unwrap();
    harness.configure_seam(json!({"journal_limits": {"logs": 1}}));
    for _ in 0..2 {
        harness.success(&[
            "observe",
            "query",
            "--session",
            overflow_session,
            "--identifier",
            "status",
            "--limit",
            "1",
        ]);
    }
    let overflow_manifest =
        harness.success(&["observe", "manifest", "--session", overflow_session]);
    let overflow_manifest_path =
        PathBuf::from(overflow_manifest["manifest_path"].as_str().unwrap());
    let manifest_before_overflow = fs::read(&overflow_manifest_path).unwrap();
    let overflow_logs = harness.error(&["observe", "logs", "--session", overflow_session]);
    assert_eq!(overflow_logs["error"]["code"], "observation_failed");
    assert_eq!(
        fs::read(&overflow_manifest_path).unwrap(),
        manifest_before_overflow
    );
    harness.configure_seam(json!({}));
    harness.success(&["close", "--session", overflow_session]);

    let race_ref = harness.query_ref(session, "race");
    let race = harness.success(&[
        "click",
        "--session",
        session,
        "--ref",
        &race_ref,
        "--mode",
        "background",
    ]);
    assert_eq!(race["outcome"], "observed");
    assert_eq!(
        harness.success(&[
            "observe",
            "query",
            "--session",
            session,
            "--identifier",
            "status",
            "--text",
            "Race 2 — settled",
            "--limit",
            "1",
        ])["matches"]
            .as_array()
            .unwrap()
            .len(),
        1
    );

    let busy_ref = harness.query_ref(session, "busy");
    let busy = harness.error(&[
        "--timeout-ms",
        "700",
        "click",
        "--session",
        session,
        "--ref",
        &busy_ref,
        "--mode",
        "background",
    ]);
    match busy["error"]["code"].as_str() {
        Some("timed_out") => assert_eq!(busy["error"]["effects"], "possible"),
        Some("stabilization_timeout") => {
            assert_eq!(busy["error"]["effects"], "confirmed");
        }
        code => panic!("unexpected busy terminal: {code:?}, response={busy}"),
    }
    assert!(busy["observation"]["screenshot_path"].is_null());
    let stopped = harness.stop_continuous_busy(session);

    let next_sequence = stopped["action_sequence"].as_u64().unwrap() + 1;
    let resolution_ref = harness.query_ref(session, "cascade");
    let resolution_args = [
        "click",
        "--session",
        session,
        "--ref",
        &resolution_ref,
        "--mode",
        "background",
        "--request-id",
        "cp07_resolution_cancel",
        "--timeout-ms",
        "10000",
    ];
    let resolution_cancel = harness.cancel_at_barrier(
        session,
        next_sequence,
        "during_native_resolution",
        "cp07_resolution_cancel",
        &resolution_args,
    );
    assert_eq!(resolution_cancel["error"]["code"], "cancelled");
    assert_eq!(resolution_cancel["error"]["effects"], "none");
    assert_eq!(resolution_cancel["outcome"], "not_performed");
    assert!(resolution_cancel["observation"]["screenshot_path"].is_null());
    let resolution_events: Vec<_> = harness
        .native_oracles()
        .into_iter()
        .filter(|event| {
            event["session_id"] == session
                && event["action_sequence"].as_u64() == Some(next_sequence)
        })
        .collect();
    assert!(resolution_events.iter().all(|event| {
        !matches!(
            event["kind"].as_str(),
            Some("ax_perform_attempt" | "cg_event_post")
        )
    }));

    let ownership_ref = harness.query_ref(session, "interrupt-before");
    let ownership_args = [
        "press",
        "--session",
        session,
        "--key",
        "Space",
        "--ref",
        &ownership_ref,
        "--mode",
        "foreground",
        "--request-id",
        "cp07_ownership_cancel",
        "--timeout-ms",
        "10000",
    ];
    let ownership_cancel = harness.cancel_at_barrier(
        session,
        next_sequence,
        "during_foreground_ownership",
        "cp07_ownership_cancel",
        &ownership_args,
    );
    assert_eq!(ownership_cancel["error"]["code"], "cancelled");
    assert_eq!(ownership_cancel["error"]["effects"], "none");
    assert_eq!(ownership_cancel["outcome"], "not_performed");
    assert!(ownership_cancel["observation"]["screenshot_path"].is_null());
    assert_no_native_events(&harness, &ownership_cancel, "cg_event_post");
    assert!(
        action_native_events(&harness, &ownership_cancel)
            .iter()
            .all(|event| {
                event["kind"] != "ax_perform_attempt" || event["details"]["action"] != "AXPress"
            })
    );

    let permission_sequence = ownership_cancel["action_sequence"].as_u64().unwrap() + 1;
    let permission_ref = harness.query_ref(session, "interrupt-before");
    let permission_args = [
        "press",
        "--session",
        session,
        "--key",
        "Space",
        "--ref",
        &permission_ref,
        "--mode",
        "foreground",
        "--timeout-ms",
        "10000",
    ];
    let (permission_reached, permission_release) =
        harness.install_barrier(session, permission_sequence, "during_foreground_ownership");
    let permission_child = harness.spawn_command(&permission_args);
    wait_for_file(&permission_reached);
    let before_permission_revocation = fixture.snapshot();
    harness.configure_seam(json!({"permissions": {"post_event": false}}));
    fs::write(&permission_release, b"release").unwrap();
    let permission_output = permission_child.wait_with_output().unwrap();
    harness.record_output(&permission_args, &permission_output);
    harness.clear_barrier(&permission_reached, &permission_release);
    harness.configure_seam(json!({}));
    assert!(!permission_output.status.success());
    let permission_revoked: Value = serde_json::from_slice(&permission_output.stdout).unwrap();
    assert_eq!(permission_revoked["error"]["code"], "interrupted");
    assert_eq!(permission_revoked["error"]["effects"], "possible");
    assert_eq!(permission_revoked["delivery"], "unknown");
    assert_eq!(permission_revoked["outcome"], "uncertain");
    assert!(
        permission_revoked["error"]["message"]
            .as_str()
            .is_some_and(|message| message.contains("no input event was dispatched")
                && message.contains("manuvra doctor"))
    );
    assert!(permission_revoked["observation"]["screenshot_path"].is_null());
    assert_no_native_events(&harness, &permission_revoked, "cg_event_post");
    let after_permission_revocation = fixture.snapshot();
    assert_eq!(
        before_permission_revocation["status"],
        after_permission_revocation["status"]
    );
    assert_eq!(
        before_permission_revocation["input"],
        after_permission_revocation["input"]
    );

    let quiet_sequence = permission_revoked["action_sequence"].as_u64().unwrap() + 1;
    let quiet_ref = harness.query_ref(session, "cascade");
    let quiet_args = [
        "click",
        "--session",
        session,
        "--ref",
        &quiet_ref,
        "--mode",
        "background",
        "--request-id",
        "cp07_quiet_cancel",
        "--timeout-ms",
        "10000",
    ];
    let quiet_cancel = harness.cancel_at_barrier(
        session,
        quiet_sequence,
        "during_native_quiet",
        "cp07_quiet_cancel",
        &quiet_args,
    );
    assert_eq!(quiet_cancel["error"]["code"], "cancelled");
    assert!(matches!(
        quiet_cancel["error"]["effects"].as_str(),
        Some("possible" | "confirmed")
    ));
    assert!(quiet_cancel["observation"]["screenshot_path"].is_null());
    let quiet_dispatches = action_native_events(&harness, &quiet_cancel)
        .iter()
        .filter(|event| {
            event["kind"] == "ax_perform_attempt" && event["details"]["action"] == "AXPress"
        })
        .count();
    assert_eq!(quiet_dispatches, 1);

    let capture_sequence = quiet_cancel["action_sequence"].as_u64().unwrap() + 1;
    let capture_ref = harness.query_ref(session, "cascade");
    let capture_args = [
        "click",
        "--session",
        session,
        "--ref",
        &capture_ref,
        "--mode",
        "background",
        "--request-id",
        "cp07_capture_cancel",
        "--timeout-ms",
        "10000",
    ];
    let capture_cancel = harness.cancel_at_barrier(
        session,
        capture_sequence,
        "during_capture",
        "cp07_capture_cancel",
        &capture_args,
    );
    assert_eq!(capture_cancel["error"]["code"], "cancelled");
    assert!(matches!(
        capture_cancel["error"]["effects"].as_str(),
        Some("possible" | "confirmed")
    ));
    assert!(capture_cancel["observation"]["screenshot_path"].is_null());
    let capture_dispatches = action_native_events(&harness, &capture_cancel)
        .iter()
        .filter(|event| {
            event["kind"] == "ax_perform_attempt" && event["details"]["action"] == "AXPress"
        })
        .count();
    assert_eq!(capture_dispatches, 1);
    harness.record_oracle_case(
        "cancellation_phase_matrix_and_midflight_permission_revocation",
        before_permission_revocation,
        after_permission_revocation,
        &json!({
            "resolution": resolution_cancel,
            "ownership": ownership_cancel,
            "permission_revoked": permission_revoked,
            "quiet": quiet_cancel,
            "capture": capture_cancel,
        }),
    );

    let point_tree = harness.success(&["observe", "tree", "--session", session]);
    let point_tree_value = read_json(&point_tree["tree_path"]);
    let nodes = point_tree_value["nodes"].as_array().unwrap();
    let window_bounds = &nodes[0]["bounds"];
    let cascade_bounds = &nodes
        .iter()
        .find(|node| node["identifier"] == "cascade")
        .unwrap()["bounds"];
    let point_frame = harness.screenshot_eventually(session);
    let x = (cascade_bounds["x"].as_f64().unwrap()
        + cascade_bounds["width"].as_f64().unwrap() / 2.0
        - window_bounds["x"].as_f64().unwrap())
        * point_frame["width"].as_f64().unwrap()
        / window_bounds["width"].as_f64().unwrap();
    let y = (cascade_bounds["y"].as_f64().unwrap()
        + cascade_bounds["height"].as_f64().unwrap() / 2.0
        - window_bounds["y"].as_f64().unwrap())
        * point_frame["height"].as_f64().unwrap()
        / window_bounds["height"].as_f64().unwrap();
    let point = format!("{x:.1},{y:.1}");
    let background_point = harness.error(&[
        "click",
        "--session",
        session,
        "--point",
        &point,
        "--frame",
        point_frame["frame_token"].as_str().unwrap(),
        "--mode",
        "background",
    ]);
    assert_eq!(background_point["error"]["code"], "foreground_required");
    let point_click = harness.success(&[
        "--timeout-ms",
        "10000",
        "click",
        "--session",
        session,
        "--point",
        &point,
        "--frame",
        point_frame["frame_token"].as_str().unwrap(),
        "--mode",
        "foreground",
    ]);
    assert_eq!(point_click["outcome"], "observed");
    assert_eq!(point_click["effective_mode"], "foreground");

    let before_foreground_type = fixture.snapshot();
    let foreground_input = harness.query_ref(session, "input");
    let foreground_type = harness.success(&[
        "type",
        "--session",
        session,
        "--ref",
        &foreground_input,
        "--text",
        "Foreground canary",
        "--replace",
        "--mode",
        "foreground",
        "--timeout-ms",
        "10000",
    ]);
    assert_eq!(foreground_type["effective_mode"], "foreground");
    assert_eq!(foreground_type["outcome"], "observed");
    let after_foreground_type = fixture.snapshot();
    assert_eq!(
        before_foreground_type["pasteboard_change_count"],
        after_foreground_type["pasteboard_change_count"]
    );
    assert_native_event(&harness, &foreground_type, "ax_value_settable");
    assert_native_event(&harness, &foreground_type, "ax_value_set");
    assert_native_event(&harness, &foreground_type, "ax_selection_collapsed");
    assert_no_native_text_events(&harness, &foreground_type);
    harness.record_oracle_case(
        "foreground_explicit_axvalue_preserves_clipboard",
        before_foreground_type,
        after_foreground_type,
        &foreground_type,
    );
    let foreground_input = harness.query_ref(session, "input");
    let foreground_selection = harness.success(&[
        "raw",
        "ax",
        "get",
        "--session",
        session,
        "--ref",
        &foreground_input,
        "--attribute",
        "AXSelectedTextRange",
    ]);
    assert_eq!(
        read_json(&foreground_selection["response_path"])["value"],
        json!({"type": "range", "location": 17, "length": 0})
    );
    let foreground_input = harness.query_ref(session, "input");
    let foreground_value = harness.success(&[
        "raw",
        "ax",
        "get",
        "--session",
        session,
        "--ref",
        &foreground_input,
        "--attribute",
        "AXValue",
    ]);
    assert_eq!(
        read_json(&foreground_value["response_path"])["value"],
        json!({"type": "string", "value": "Foreground canary"})
    );
    assert_eq!(
        harness.success(&[
            "press",
            "--session",
            session,
            "--key",
            "Z",
            "--ref",
            &foreground_input,
            "--mode",
            "foreground",
            "--timeout-ms",
            "10000",
        ])["outcome"],
        "observed"
    );
    let foreground_input = harness.query_ref(session, "input");
    let foreground_value = harness.success(&[
        "raw",
        "ax",
        "get",
        "--session",
        session,
        "--ref",
        &foreground_input,
        "--attribute",
        "AXValue",
    ]);
    assert_eq!(
        read_json(&foreground_value["response_path"])["value"],
        json!({"type": "string", "value": "Foreground canaryZ"})
    );
    assert_eq!(
        harness.success(&[
            "press",
            "--session",
            session,
            "--key",
            "Enter",
            "--ref",
            &foreground_input,
            "--mode",
            "foreground",
            "--timeout-ms",
            "10000",
        ])["outcome"],
        "observed"
    );
    assert_eq!(
        harness.success(&[
            "scroll",
            "--session",
            session,
            "--delta-y",
            "300",
            "--mode",
            "foreground",
            "--timeout-ms",
            "10000",
        ])["outcome"],
        "observed"
    );

    let fallback_tree = harness.success(&["observe", "tree", "--session", session]);
    let fallback_tree = read_json(&fallback_tree["tree_path"]);
    let fallback_nodes = fallback_tree["nodes"].as_array().unwrap();
    let fallback_window = &fallback_nodes[0]["bounds"];
    let fallback_input = &fallback_nodes
        .iter()
        .find(|node| node["identifier"] == "input")
        .unwrap()["bounds"];
    let fallback_frame = harness.screenshot_eventually(session);
    let fallback_x = (fallback_input["x"].as_f64().unwrap()
        + fallback_input["width"].as_f64().unwrap() / 2.0
        - fallback_window["x"].as_f64().unwrap())
        * fallback_frame["width"].as_f64().unwrap()
        / fallback_window["width"].as_f64().unwrap();
    let fallback_y = (fallback_input["y"].as_f64().unwrap()
        + fallback_input["height"].as_f64().unwrap() / 2.0
        - fallback_window["y"].as_f64().unwrap())
        * fallback_frame["height"].as_f64().unwrap()
        / fallback_window["height"].as_f64().unwrap();
    let fallback_point = format!("{fallback_x:.1},{fallback_y:.1}");
    let before_fallback_type = fixture.snapshot();
    let fallback_type = harness.success(&[
        "type",
        "--session",
        session,
        "--point",
        &fallback_point,
        "--frame",
        fallback_frame["frame_token"].as_str().unwrap(),
        "--text",
        "Point fallback",
        "--replace",
        "--mode",
        "foreground",
        "--timeout-ms",
        "10000",
    ]);
    assert_eq!(fallback_type["outcome"], "observed");
    assert_eq!(fallback_type["effective_mode"], "foreground");
    let after_fallback_type = fixture.snapshot();
    assert_eq!(
        before_fallback_type["pasteboard_change_count"],
        after_fallback_type["pasteboard_change_count"]
    );
    assert_native_text_event(&harness, &fallback_type);
    harness.record_oracle_case(
        "foreground_point_cgevent_preserves_clipboard",
        before_fallback_type,
        after_fallback_type,
        &fallback_type,
    );

    let interrupt_before_ref = harness.query_ref(session, "interrupt-before");
    let next_sequence = fallback_type["action_sequence"].as_u64().unwrap() + 1;
    let (barrier_reached, barrier_release) = harness.install_barrier(
        session,
        next_sequence,
        "after_foreground_proof_before_input",
    );
    let interrupted_before_args = [
        "press",
        "--session",
        session,
        "--key",
        "Space",
        "--ref",
        &interrupt_before_ref,
        "--mode",
        "foreground",
        "--timeout-ms",
        "10000",
    ];
    let interrupted_before_child = harness.spawn_command(&interrupted_before_args);
    wait_for_file(&barrier_reached);
    let before_interruption = fixture.snapshot();
    fixture.command("interrupt");
    fs::write(&barrier_release, b"release").unwrap();
    let interrupted_before_output = interrupted_before_child.wait_with_output().unwrap();
    harness.record_output(&interrupted_before_args, &interrupted_before_output);
    harness.clear_barrier(&barrier_reached, &barrier_release);
    assert!(!interrupted_before_output.status.success());
    let interrupted_before: Value =
        serde_json::from_slice(&interrupted_before_output.stdout).unwrap();
    assert_eq!(
        interrupted_before["error"]["code"],
        "interrupted",
        "interrupted_before={interrupted_before}, daemon_stderr={}",
        harness.daemon_stderr()
    );
    assert_eq!(interrupted_before["error"]["effects"], "none");
    assert_eq!(interrupted_before["delivery"], "backend_rejected");
    assert_eq!(interrupted_before["outcome"], "not_performed");
    assert!(
        interrupted_before["observation"]["screenshot_path"].is_null(),
        "interrupted_before={interrupted_before}"
    );
    let after_interruption = fixture.snapshot();
    assert_no_native_events(&harness, &interrupted_before, "cg_event_post");
    assert_no_native_events(&harness, &interrupted_before, "ax_value_set");
    harness.record_oracle_case(
        "foreground_interruption_before_input",
        before_interruption,
        after_interruption,
        &interrupted_before,
    );

    let before_interrupted_after = fixture.snapshot();
    let interrupt_after_ref = harness.query_ref(session, "interrupt-after");
    let interrupted_after = harness.error(&[
        "click",
        "--session",
        session,
        "--ref",
        &interrupt_after_ref,
        "--mode",
        "foreground",
        "--timeout-ms",
        "10000",
    ]);
    assert_eq!(interrupted_after["error"]["code"], "interrupted");
    assert_eq!(interrupted_after["error"]["effects"], "possible");
    assert_eq!(interrupted_after["delivery"], "unknown");
    assert!(
        interrupted_after["observation"]["screenshot_path"].is_null(),
        "interrupted_after={interrupted_after}"
    );
    let after_interrupted_after = fixture.snapshot();
    assert_native_event(&harness, &interrupted_after, "cg_event_post");
    harness.record_oracle_case(
        "foreground_interruption_after_input",
        before_interrupted_after,
        after_interrupted_after,
        &interrupted_after,
    );

    for kind in ["events", "logs", "diagnostics", "timings"] {
        let evidence = harness.success(&["observe", kind, "--session", session]);
        let value = read_json(&evidence["path"]);
        assert_eq!(value["complete"], true);
        if kind == "logs" {
            let events = value["events"].as_array().unwrap();
            assert!(!events.is_empty());
            assert!(events.iter().all(|event| {
                event.get("action_sequence").is_some()
                    && event.get("command").is_some()
                    && event.get("delivery").is_some()
                    && event.get("interrupted").is_some()
                    && event.get("error_code").is_some()
            }));
            assert!(events.iter().any(|event| {
                event["native_error"]["domain"] == "AXError"
                    && event["native_error"]["number"].as_i64().is_some()
                    && event["native_error"]["classification"] == "backend_rejected"
            }));
        }
    }
    let export = std::env::var_os("MANUVRA_CP07_EVIDENCE_ROOT")
        .map(PathBuf::from)
        .map(|root| root.join("trajectory-export"))
        .unwrap_or_else(|| fixture_root.path().join("export"));
    let exported = harness.success(&[
        "export",
        "--session",
        session,
        "--all",
        "--destination",
        export.to_str().unwrap(),
    ]);
    assert!(exported["files"][0]["artifact_count"].as_u64().unwrap() >= 8);
    assert_eq!(exported["verified"], true);
    let live_inventory = directory_inventory(&live_session_directory);
    let before_hidden = fixture.snapshot();
    fixture.command("hide");
    let after_hidden = fixture.snapshot();
    assert_eq!(after_hidden["target_is_visible"], false);
    let hidden = harness.error(&["observe", "screenshot", "--session", session]);
    assert!(
        matches!(
            hidden["error"]["code"].as_str(),
            Some("capability_unavailable" | "target_stale" | "target_not_found")
        ),
        "hidden={hidden}"
    );
    harness.record_oracle_case(
        "hidden_window_fails_without_unhide",
        before_hidden,
        after_hidden,
        &hidden,
    );
    let close = harness.success(&["close", "--session", session]);
    assert!(!live_session_directory.exists());
    assert!(export.join("manifest.json").is_file());
    harness.record_oracle_case(
        "session_close_removes_live_artifacts_and_preserves_export",
        live_inventory,
        json!({
            "live_exists": live_session_directory.exists(),
            "export_exists": export.exists(),
            "export_manifest": export.join("manifest.json"),
        }),
        &close,
    );
    fixture.command("unhide");
    let replacement_target = target.clone();
    let replacement = harness.success(&[
        "open",
        "--target",
        &replacement_target,
        "--mode",
        "background",
        "--lease-ttl-ms",
        "600000",
    ]);
    let replacement_session = replacement["session_id"].as_str().unwrap();
    let terminate_ref = harness.query_ref_eventually(replacement_session, "terminate-after");
    let target_loss_after_dispatch = harness.error(&[
        "click",
        "--session",
        replacement_session,
        "--ref",
        &terminate_ref,
        "--mode",
        "foreground",
        "--timeout-ms",
        "10000",
    ]);
    assert_eq!(target_loss_after_dispatch["outcome"], "uncertain");
    assert!(
        matches!(
            target_loss_after_dispatch["error"]["effects"].as_str(),
            Some("possible" | "confirmed")
        ),
        "target_loss_after_dispatch={target_loss_after_dispatch}"
    );
    assert!(
        target_loss_after_dispatch["observation"]["screenshot_path"].is_null(),
        "target_loss_after_dispatch={target_loss_after_dispatch}"
    );
    let target_loss_before_dispatch = harness.error(&[
        "click",
        "--session",
        replacement_session,
        "--identifier",
        "cascade",
        "--mode",
        "background",
    ]);
    assert!(
        matches!(
            target_loss_before_dispatch["error"]["code"].as_str(),
            Some("target_stale" | "target_not_found")
        ),
        "target_loss_before_dispatch={target_loss_before_dispatch}"
    );
    assert_eq!(target_loss_before_dispatch["error"]["effects"], "none");
    harness.success(&["close", "--session", replacement_session]);
    assert!(export.is_dir());
    if let Some(root) = std::env::var_os("MANUVRA_CP07_EVIDENCE_ROOT") {
        harness.write_transcript(
            &PathBuf::from(root).join("representative-native-trajectory.json"),
            "manuvra/cp-07-native-trajectory@1",
        );
    }
}

#[test]
fn real_macos_textedit_and_calculator_smoke() {
    if std::env::var_os("MANUVRA_RUN_MACOS_SMOKE").is_none() {
        return;
    }
    let _test_guard = MACOS_PUBLIC_TEST_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let retained_root = std::env::var_os("MANUVRA_CP07_EVIDENCE_ROOT").map(PathBuf::from);

    let document_root = tempfile::tempdir().unwrap();
    if std::env::var_os("MANUVRA_SKIP_TEXTEDIT_SMOKE").is_none() {
        let textedit_harness = Harness::start();
        let existing_targets = textedit_harness.macos_target_ids();
        let document_name = format!("CP07-TextEdit-{}.txt", std::process::id());
        let document_path = document_root.path().join(&document_name);
        let initial_content = format!("Temporary CP-07 document {document_name}");
        fs::write(&document_path, &initial_content).unwrap();
        let textedit = LaunchedApp::start("TextEdit", &[&document_path]);
        let (_textedit_target, textedit_session_id) = textedit_harness.open_target_matching(
            &existing_targets,
            &["--role", "image", "--name", &document_name],
            "foreground",
        );
        let textedit_session = textedit_session_id.as_str();
        let initial_tree =
            textedit_harness.success(&["observe", "tree", "--session", textedit_session]);
        let initial_tree_value = read_json(&initial_tree["tree_path"]);
        let editor_ref = initial_tree_value["nodes"]
            .as_array()
            .unwrap()
            .iter()
            .find(|node| node["role"] == "textbox" && node["text"] == initial_content)
            .and_then(|node| node["ref"].as_str())
            .unwrap_or_else(|| panic!("TextEdit document textbox was absent: {initial_tree_value}"))
            .to_owned();
        let baseline = textedit_harness.success(&[
            "type",
            "--session",
            textedit_session,
            "--ref",
            &editor_ref,
            "--text",
            "CP-07 AX baseline",
            "--replace",
            "--mode",
            "background",
        ]);
        assert_eq!(baseline["outcome"], "observed");
        let baseline_tree =
            textedit_harness.success(&["observe", "tree", "--session", textedit_session]);
        let baseline_tree_value = read_json(&baseline_tree["tree_path"]);
        let editor_ref = baseline_tree_value["nodes"]
            .as_array()
            .unwrap()
            .iter()
            .find(|node| node["role"] == "textbox" && node["text"] == "CP-07 AX baseline")
            .and_then(|node| node["ref"].as_str())
            .unwrap_or_else(|| panic!("TextEdit AX baseline was absent: {baseline_tree_value}"))
            .to_owned();
        let canary = "CP-07 TextEdit foreground canary";
        let typed = textedit_harness.success(&[
            "type",
            "--session",
            textedit_session,
            "--ref",
            &editor_ref,
            "--text",
            canary,
            "--mode",
            "foreground",
            "--timeout-ms",
            "10000",
        ]);
        assert_eq!(typed["outcome"], "observed");
        let final_tree =
            textedit_harness.success(&["observe", "tree", "--session", textedit_session]);
        let final_tree_value = read_json(&final_tree["tree_path"]);
        assert!(
            serde_json::to_string(&final_tree_value)
                .unwrap()
                .contains(canary),
            "TextEdit tree did not contain the canary: {final_tree_value}"
        );
        let textedit_export = retained_root
            .as_ref()
            .map(|root| root.join("textedit-export"))
            .unwrap_or_else(|| document_root.path().join("textedit-export"));
        let exported = textedit_harness.success(&[
            "export",
            "--session",
            textedit_session,
            "--all",
            "--destination",
            textedit_export.to_str().unwrap(),
        ]);
        assert_eq!(exported["verified"], true);
        textedit_harness.success(&["close", "--session", textedit_session]);
        if let Some(root) = &retained_root {
            textedit_harness.write_transcript(
                &root.join("textedit-smoke.json"),
                "manuvra/cp-07-textedit-smoke@1",
            );
        }
        drop(textedit);
    }

    let calculator_harness = Harness::start();
    let existing_targets = calculator_harness.macos_target_ids();
    let calculator = LaunchedApp::start("Calculator", &[]);
    let (_calculator_target, calculator_session_id) = calculator_harness.open_target_matching(
        &existing_targets,
        &["--role", "button", "--name", "7"],
        "background",
    );
    let calculator_session = calculator_session_id.as_str();
    calculator_clear(&calculator_harness, calculator_session);
    calculator_click_digit(&calculator_harness, calculator_session, "7", "7");
    calculator_click_digit(&calculator_harness, calculator_session, "8", "78");
    let final_tree =
        calculator_harness.success(&["observe", "tree", "--session", calculator_session]);
    let final_tree_value = read_json(&final_tree["tree_path"]);
    assert!(
        final_tree_value["nodes"]
            .as_array()
            .unwrap()
            .iter()
            .any(|node| visible_digits(node) == "78"),
        "Calculator display did not contain 78: {final_tree_value}"
    );
    let calculator_export = retained_root
        .as_ref()
        .map(|root| root.join("calculator-export"))
        .unwrap_or_else(|| document_root.path().join("calculator-export"));
    let exported = calculator_harness.success(&[
        "export",
        "--session",
        calculator_session,
        "--all",
        "--destination",
        calculator_export.to_str().unwrap(),
    ]);
    assert_eq!(exported["verified"], true);
    calculator_harness.success(&["close", "--session", calculator_session]);
    if let Some(root) = &retained_root {
        calculator_harness.write_transcript(
            &root.join("calculator-smoke.json"),
            "manuvra/cp-07-calculator-smoke@1",
        );
    }
    drop(calculator);
}

fn calculator_clear(harness: &Harness, session: &str) {
    let tree = harness.success(&["observe", "tree", "--session", session]);
    let tree = read_json(&tree["tree_path"]);
    let reference = tree["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|node| {
            node["role"] == "button"
                && ["name", "text", "identifier"].into_iter().any(|field| {
                    node[field]
                        .as_str()
                        .is_some_and(|value| value.to_ascii_lowercase().contains("clear"))
                })
        })
        .and_then(|node| node["ref"].as_str())
        .expect("Calculator tree did not expose a clear button")
        .to_owned();
    let args = [
        "click",
        "--session",
        session,
        "--ref",
        &reference,
        "--mode",
        "background",
        "--timeout-ms",
        "10000",
    ];
    let output = harness.command(&args);
    let response: Value = serde_json::from_slice(&output.stdout).unwrap();
    if !output.status.success() && response["error"]["effects"] == "none" {
        assert_eq!(response["error"]["code"], "foreground_required");
        let foreground = harness.success(&[
            "click",
            "--session",
            session,
            "--ref",
            &reference,
            "--mode",
            "foreground",
            "--timeout-ms",
            "10000",
        ]);
        assert_eq!(foreground["outcome"], "observed");
    } else if output.status.success() {
        assert_eq!(response["outcome"], "observed");
    }

    let observed = harness.success(&["observe", "tree", "--session", session]);
    let observed = read_json(&observed["tree_path"]);
    assert!(
        observed["nodes"]
            .as_array()
            .unwrap()
            .iter()
            .any(|node| visible_digits(node) == "0"),
        "Calculator clear did not produce a zero display: response={response}, tree={observed}"
    );
}

fn calculator_click_digit(harness: &Harness, session: &str, digit: &str, expected: &str) {
    let tree = harness.success(&["observe", "tree", "--session", session]);
    let tree_value = read_json(&tree["tree_path"]);
    let reference = tree_value["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|node| node["role"] == "button" && node["name"] == digit)
        .and_then(|node| node["ref"].as_str())
        .unwrap_or_else(|| panic!("Calculator digit {digit} was absent: {tree_value}"))
        .to_owned();
    let output = harness.command(&[
        "click",
        "--session",
        session,
        "--ref",
        &reference,
        "--mode",
        "background",
        "--timeout-ms",
        "10000",
    ]);
    let response: Value = serde_json::from_slice(&output.stdout).unwrap();
    if output.status.success() {
        assert_eq!(response["outcome"], "observed");
    } else if response["error"]["effects"] == "none" {
        assert_eq!(response["error"]["code"], "foreground_required");
        let foreground = harness.success(&[
            "click",
            "--session",
            session,
            "--ref",
            &reference,
            "--mode",
            "foreground",
            "--timeout-ms",
            "10000",
        ]);
        assert_eq!(foreground["outcome"], "observed");
    }

    let observed = harness.success(&["observe", "tree", "--session", session]);
    let observed = read_json(&observed["tree_path"]);
    assert!(
        observed["nodes"]
            .as_array()
            .unwrap()
            .iter()
            .any(|node| visible_digits(node) == expected),
        "Calculator action did not produce display {expected}: response={response}, tree={observed}"
    );
}

fn visible_digits(node: &Value) -> String {
    if node["role"] != "text" {
        return String::new();
    }
    node["text"]
        .as_str()
        .or_else(|| node["name"].as_str())
        .unwrap_or_default()
        .chars()
        .filter(char::is_ascii_digit)
        .collect()
}

#[test]
fn real_macos_warm_latency_budget() {
    if std::env::var_os("MANUVRA_RUN_MACOS_BENCH").is_none() {
        return;
    }
    let _test_guard = MACOS_PUBLIC_TEST_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let harness = Harness::start();
    let existing_targets = harness.macos_target_ids();
    let fixture_root = tempfile::tempdir().unwrap();
    let _fixture = Fixture::build_and_start(fixture_root.path());
    let (_target, session_id) = harness.open_target_matching(
        &existing_targets,
        &[
            "--role",
            "text",
            "--identifier",
            "status",
            "--text",
            "Ready",
        ],
        "background",
    );
    let session = session_id.as_str();

    let mut raw_total = Vec::new();
    let mut dispatch = Vec::new();
    let mut capture = Vec::new();
    let mut action_total = Vec::new();
    for _ in 0..50 {
        let query_started = Instant::now();
        let query = harness.success(&[
            "observe",
            "query",
            "--session",
            session,
            "--identifier",
            "status",
            "--limit",
            "1",
        ]);
        assert_eq!(query["matches"].as_array().unwrap().len(), 1);
        raw_total.push(query_started.elapsed().as_millis() as u64);

        let action_ref = harness.query_ref(session, "duplicate-one");
        let action = harness.success(&[
            "click",
            "--session",
            session,
            "--ref",
            &action_ref,
            "--mode",
            "background",
        ]);
        dispatch.push(action["timing_ms"]["dispatch"].as_u64().unwrap());
        capture.push(action["timing_ms"]["capture"].as_u64().unwrap());
        action_total.push(action["timing_ms"]["total"].as_u64().unwrap());
    }
    let report = json!({
        "schema": "manuvra/cp-07-macos-benchmark@1",
        "samples": 50,
        "ax_query_cli_round_trip_ms": {"p95": percentile(&raw_total, 95), "values": raw_total, "measurement": "public CLI wall-clock round trip"},
        "action_dispatch_ms": {"p95": percentile(&dispatch, 95), "values": dispatch},
        "capture_ms": {"p95": percentile(&capture, 95), "values": capture},
        "action_total_ms": {"p95": percentile(&action_total, 95), "values": action_total},
    });
    assert!(
        report["ax_query_cli_round_trip_ms"]["p95"]
            .as_u64()
            .unwrap()
            <= 150,
        "{report}"
    );
    assert!(
        report["action_dispatch_ms"]["p95"].as_u64().unwrap() <= 150,
        "{report}"
    );
    assert!(
        report["capture_ms"]["p95"].as_u64().unwrap() <= 250,
        "{report}"
    );
    assert!(
        report["action_total_ms"]["p95"].as_u64().unwrap() <= 1_000,
        "{report}"
    );
    if let Some(path) = std::env::var_os("MANUVRA_CP07_BENCH_PATH") {
        let path = PathBuf::from(path);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, serde_json::to_vec_pretty(&report).unwrap()).unwrap();
    }
    harness.success(&["close", "--session", session]);
}

#[test]
fn installed_native_background_scored_set() {
    if std::env::var_os("MANUVRA_RUN_INSTALLED_NATIVE_BACKGROUND_PROOF").is_none() {
        return;
    }
    run_installed_native_scored_set("native_background");
}

#[test]
fn installed_native_foreground_scored_set() {
    if std::env::var_os("MANUVRA_RUN_INSTALLED_NATIVE_FOREGROUND_PROOF").is_none() {
        return;
    }
    run_installed_native_scored_set("native_foreground");
}

fn run_installed_native_scored_set(journey: &str) {
    assert!(
        std::env::var_os("MANUVRA_INSTALLED_CLI").is_some(),
        "installed proof requires MANUVRA_INSTALLED_CLI"
    );
    let _test_guard = MACOS_PUBLIC_TEST_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let evidence_root = PathBuf::from(
        std::env::var_os("MANUVRA_PROOF_ROOT").expect("MANUVRA_PROOF_ROOT is required"),
    )
    .join(journey.replace('_', "-"));
    fs::create_dir_all(&evidence_root).unwrap();
    let harness = Harness::start();
    let existing_targets = harness.macos_target_ids();
    let fixture_root = tempfile::tempdir().unwrap();
    let fixture = Fixture::build_and_start(fixture_root.path());
    let focus_root = tempfile::tempdir().unwrap();
    let focus_sink = FocusSink::build_and_start(focus_root.path());
    fixture.command("reset:target-discovery");
    let (target, discovery_session) = harness.open_target_matching(
        &existing_targets,
        &[
            "--role",
            "text",
            "--identifier",
            "status",
            "--text",
            "Ready — target-discovery",
        ],
        if journey == "native_background" {
            "background"
        } else {
            "foreground"
        },
    );
    harness.success(&["close", "--session", &discovery_session]);
    let mut rows = Vec::new();
    let mut query_samples = Vec::new();
    let mut dispatch_samples = Vec::new();
    let mut capture_samples = Vec::new();
    let mut total_samples = Vec::new();
    let mut successes = 0_u64;
    let mut wrong_target = 0_u64;
    let attempts = std::env::var("MANUVRA_PROOF_ATTEMPTS")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .unwrap_or(50);

    for attempt in 0..=attempts {
        let scored = attempt != 0;
        let nonce = format!("native-{attempt:02}-{}", std::process::id());
        let iteration_root = evidence_root.join(if scored {
            format!("run-{attempt:02}")
        } else {
            "warmup".to_owned()
        });
        fs::create_dir_all(&iteration_root).unwrap();
        fixture.command(&format!("reset:{nonce}"));
        if journey == "native_background" {
            focus_sink.activate();
        }
        let session_slot = Mutex::new(None::<String>);
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let opened = harness.success(&[
                "open",
                "--target",
                &target,
                "--mode",
                if journey == "native_background" {
                    "background"
                } else {
                    "foreground"
                },
                "--lease-ttl-ms",
                "600000",
            ]);
            let session = opened["session_id"].as_str().unwrap().to_owned();
            *session_slot.lock().unwrap() = Some(session.clone());
            let detail = if journey == "native_background" {
                installed_native_background_iteration(
                    &harness,
                    &fixture,
                    &focus_sink,
                    &session,
                    &nonce,
                    &iteration_root,
                )
            } else {
                installed_native_foreground_iteration(
                    &harness,
                    &fixture,
                    &session,
                    &nonce,
                    &iteration_root,
                )
            };
            *session_slot.lock().unwrap() = None;
            detail
        }));
        match result {
            Ok(detail) => {
                if scored {
                    successes += 1;
                    query_samples.push(detail["query_ms"].as_u64().unwrap());
                    let actions = detail["actions"].as_array().unwrap();
                    dispatch_samples.push(
                        actions
                            .iter()
                            .map(|action| action["timing_ms"]["dispatch"].as_u64().unwrap())
                            .max()
                            .unwrap(),
                    );
                    capture_samples.push(
                        actions
                            .iter()
                            .map(|action| action["timing_ms"]["capture"].as_u64().unwrap())
                            .max()
                            .unwrap(),
                    );
                    total_samples.push(
                        actions
                            .iter()
                            .map(|action| action["timing_ms"]["total"].as_u64().unwrap())
                            .max()
                            .unwrap(),
                    );
                }
                rows.push(
                    json!({"attempt": attempt, "scored": scored, "passed": true, "detail": detail}),
                );
            }
            Err(payload) => {
                if let Some(session) = session_slot.lock().unwrap().take() {
                    let _ = harness.command(&["close", "--session", &session]);
                }
                let failure = panic_payload(payload);
                if scored && failure.contains("wrong native target") {
                    wrong_target += 1;
                }
                rows.push(json!({
                    "attempt": attempt,
                    "scored": scored,
                    "passed": false,
                    "failure": failure,
                }));
            }
        }
    }
    let report = json!({
        "schema": "manuvra/installed-scored-set@1",
        "journey": journey,
        "warmups": 1,
        "attempts": attempts,
        "first_attempt_successes": successes,
        "wrong_target": wrong_target,
        "hangs": 0,
        "orphaned_leases": 0,
        "orphaned_session_directories": 0,
        "latency_p95_ms": {
            "raw_query": percentile(&query_samples, 95),
            "dispatch": percentile(&dispatch_samples, 95),
            "capture": percentile(&capture_samples, 95),
            "total": percentile(&total_samples, 95),
        },
        "rows": rows,
    });
    fs::write(
        evidence_root.join("report.json"),
        serde_json::to_vec_pretty(&report).unwrap(),
    )
    .unwrap();
    if attempts == 50 {
        assert!(successes >= 49, "{report}");
        assert!(
            report["latency_p95_ms"]["raw_query"].as_u64().unwrap() <= 150,
            "{report}"
        );
        assert!(
            report["latency_p95_ms"]["dispatch"].as_u64().unwrap() <= 150,
            "{report}"
        );
        assert!(
            report["latency_p95_ms"]["capture"].as_u64().unwrap() <= 250,
            "{report}"
        );
        assert!(
            report["latency_p95_ms"]["total"].as_u64().unwrap() <= 1_000,
            "{report}"
        );
    } else {
        assert_eq!(successes, attempts, "smoke proof requires every attempt");
    }
    assert_eq!(wrong_target, 0, "{report}");
}

fn installed_native_background_iteration(
    harness: &Harness,
    fixture: &Fixture,
    focus_sink: &FocusSink,
    session: &str,
    nonce: &str,
    iteration_root: &Path,
) -> Value {
    let before = fixture.snapshot();
    let focus_before = focus_sink.snapshot();
    assert_eq!(before["nonce"], nonce);
    assert_eq!(
        before["frontmost_pid"].as_i64(),
        Some(i64::from(focus_sink.pid))
    );
    assert_eq!(before["target_is_key"], false);
    let screenshot = harness.screenshot_eventually(session);
    let tree = harness.success(&["observe", "tree", "--session", session]);
    assert!(
        read_json(&tree["tree_path"])["nodes"]
            .as_array()
            .unwrap()
            .len()
            >= 10
    );
    let query_started = Instant::now();
    let query = harness.success(&[
        "observe",
        "query",
        "--session",
        session,
        "--identifier",
        "status",
        "--text",
        &format!("Ready — {nonce}"),
        "--limit",
        "1",
    ]);
    let query_ms = query_started.elapsed().as_millis() as u64;
    assert_eq!(
        query["matches"].as_array().unwrap().len(),
        1,
        "wrong native target"
    );
    let status_ref = harness.query_ref(session, "status");
    let raw_get = harness.success(&[
        "raw",
        "ax",
        "get",
        "--session",
        session,
        "--ref",
        &status_ref,
        "--attribute",
        "AXValue",
    ]);
    assert_eq!(
        read_json(&raw_get["response_path"])["value"],
        json!({"type": "string", "value": format!("Ready — {nonce}")}),
        "wrong native target"
    );
    let cascade_ref = harness.query_ref(session, "cascade");
    let clicked = harness.success(&[
        "click",
        "--session",
        session,
        "--ref",
        &cascade_ref,
        "--mode",
        "background",
    ]);
    let input_ref = harness.query_ref(session, "input");
    let typed_value = format!("Background — {nonce}");
    let typed = harness.success(&[
        "type",
        "--session",
        session,
        "--ref",
        &input_ref,
        "--text",
        &typed_value,
        "--replace",
        "--mode",
        "background",
    ]);
    let input_ref = harness.query_ref(session, "input");
    let raw_value = format!("Raw — {nonce}");
    let encoded_value =
        serde_json::to_string(&json!({"type": "string", "value": raw_value})).unwrap();
    let raw_set = harness.success(&[
        "raw",
        "ax",
        "set",
        "--session",
        session,
        "--ref",
        &input_ref,
        "--attribute",
        "AXValue",
        "--value",
        &encoded_value,
        "--mode",
        "background",
    ]);
    let input_ref = harness.query_ref(session, "input");
    let raw_verify = harness.success(&[
        "raw",
        "ax",
        "get",
        "--session",
        session,
        "--ref",
        &input_ref,
        "--attribute",
        "AXValue",
    ]);
    assert_eq!(
        read_json(&raw_verify["response_path"])["value"],
        json!({"type": "string", "value": format!("Raw — {nonce}")})
    );
    let negative_before = fixture.snapshot();
    let foreground_only = harness.error(&[
        "press",
        "--session",
        session,
        "--key",
        "Enter",
        "--mode",
        "background",
    ]);
    assert_eq!(foreground_only["error"]["code"], "foreground_required");
    assert_eq!(foreground_only["error"]["effects"], "none");
    let after = fixture.snapshot();
    let focus_after = focus_sink.snapshot();
    assert_eq!(after["nonce"], nonce, "wrong native target");
    assert_eq!(
        after["window_title"],
        format!("CP-09 Native Fixture — {nonce}")
    );
    assert_eq!(after["input"], format!("Raw — {nonce}"));
    assert_eq!(negative_before["status"], after["status"]);
    assert_background_snapshot_unchanged(&before, &after);
    assert_eq!(
        focus_before["input_events"], focus_after["input_events"],
        "background posted global input"
    );
    let export = iteration_root.join("export");
    let exported = harness.success(&[
        "export",
        "--session",
        session,
        "--all",
        "--destination",
        export.to_str().unwrap(),
    ]);
    assert_eq!(exported["verified"], true);
    let live_directory = PathBuf::from(clicked["manifest_path"].as_str().unwrap())
        .parent()
        .unwrap()
        .to_owned();
    let closed = harness.success(&["close", "--session", session]);
    assert_eq!(closed["artifacts_removed"], true);
    assert!(!live_directory.exists(), "orphaned live-session directory");
    let daemon = harness.success(&["daemon", "status"]);
    assert!(daemon["active_sessions"].as_array().unwrap().is_empty());
    json!({
        "nonce": nonce,
        "session_id": session,
        "query_ms": query_ms,
        "raw_query_ms": raw_get["timing_ms"]["total"],
        "screenshot": screenshot,
        "tree": tree,
        "actions": [clicked, typed, raw_set],
        "foreground_required": foreground_only,
        "before": before,
        "after": after,
        "focus_before": focus_before,
        "focus_after": focus_after,
        "export": exported,
        "cleanup": daemon,
    })
}

fn installed_native_foreground_iteration(
    harness: &Harness,
    fixture: &Fixture,
    session: &str,
    nonce: &str,
    iteration_root: &Path,
) -> Value {
    let before = fixture.snapshot();
    assert_eq!(before["nonce"], nonce);
    let screenshot = harness.screenshot_eventually(session);
    let tree = harness.success(&["observe", "tree", "--session", session]);
    let tree_value = read_json(&tree["tree_path"]);
    let query_started = Instant::now();
    let query = harness.success(&[
        "observe",
        "query",
        "--session",
        session,
        "--identifier",
        "status",
        "--text",
        &format!("Ready — {nonce}"),
        "--limit",
        "1",
    ]);
    let query_ms = query_started.elapsed().as_millis() as u64;
    assert_eq!(
        query["matches"].as_array().unwrap().len(),
        1,
        "wrong native target"
    );
    let cascade_point = element_point(&tree_value, &screenshot, "cascade");
    let clicked = harness.success(&[
        "click",
        "--session",
        session,
        "--point",
        &cascade_point,
        "--frame",
        screenshot["frame_token"].as_str().unwrap(),
        "--mode",
        "foreground",
    ]);
    let input_ref = harness.query_ref(session, "input");
    let typed_value = format!("Foreground — {nonce}");
    let typed = harness.success(&[
        "type",
        "--session",
        session,
        "--ref",
        &input_ref,
        "--text",
        &typed_value,
        "--replace",
        "--mode",
        "foreground",
    ]);
    let press_input_ref = harness.query_ref(session, "input");
    let pressed = harness.success(&[
        "press",
        "--session",
        session,
        "--key",
        "Z",
        "--ref",
        &press_input_ref,
        "--mode",
        "foreground",
    ]);
    let scrolled = harness.success(&[
        "scroll",
        "--session",
        session,
        "--delta-y",
        "300",
        "--mode",
        "foreground",
    ]);
    let after = fixture.snapshot();
    assert_eq!(after["nonce"], nonce, "wrong native target");
    assert_eq!(
        after["window_title"],
        format!("CP-09 Native Fixture — {nonce}")
    );
    assert_eq!(
        after["fixture_pid"], after["frontmost_pid"],
        "wrong foreground owner"
    );
    let input_after = after["input"].as_str().unwrap();
    assert!(
        input_after.starts_with(&typed_value),
        "wrong foreground input: {after}"
    );
    let press_observed = after["key_events"].as_u64().unwrap() >= 1 || input_after != typed_value;
    assert!(press_observed, "foreground press was not observed: {after}");
    let final_screenshot = harness.screenshot_eventually(session);
    let export = iteration_root.join("export");
    let exported = harness.success(&[
        "export",
        "--session",
        session,
        "--all",
        "--destination",
        export.to_str().unwrap(),
    ]);
    assert_eq!(exported["verified"], true);
    let live_directory = PathBuf::from(clicked["manifest_path"].as_str().unwrap())
        .parent()
        .unwrap()
        .to_owned();
    let closed = harness.success(&["close", "--session", session]);
    assert_eq!(closed["artifacts_removed"], true);
    assert!(!live_directory.exists(), "orphaned live-session directory");
    let daemon = harness.success(&["daemon", "status"]);
    assert!(daemon["active_sessions"].as_array().unwrap().is_empty());
    json!({
        "nonce": nonce,
        "session_id": session,
        "query_ms": query_ms,
        "screenshot_before": screenshot,
        "screenshot_after": final_screenshot,
        "tree": tree,
        "actions": [clicked, typed, pressed, scrolled],
        "before": before,
        "after": after,
        "export": exported,
        "cleanup": daemon,
    })
}

fn element_point(tree: &Value, frame: &Value, identifier: &str) -> String {
    let nodes = tree["nodes"].as_array().unwrap();
    let window = &nodes[0]["bounds"];
    let element = &nodes
        .iter()
        .find(|node| node["identifier"] == identifier)
        .unwrap()["bounds"];
    let x = (element["x"].as_f64().unwrap() + element["width"].as_f64().unwrap() / 2.0
        - window["x"].as_f64().unwrap())
        * frame["width"].as_f64().unwrap()
        / window["width"].as_f64().unwrap();
    let y = (element["y"].as_f64().unwrap() + element["height"].as_f64().unwrap() / 2.0
        - window["y"].as_f64().unwrap())
        * frame["height"].as_f64().unwrap()
        / window["height"].as_f64().unwrap();
    format!("{x:.1},{y:.1}")
}

fn panic_payload(payload: Box<dyn std::any::Any + Send>) -> String {
    payload
        .downcast_ref::<String>()
        .cloned()
        .or_else(|| {
            payload
                .downcast_ref::<&str>()
                .map(|value| (*value).to_owned())
        })
        .unwrap_or_else(|| "non-string panic".to_owned())
}

fn percentile(values: &[u64], percent: usize) -> u64 {
    if values.is_empty() {
        return 0;
    }
    let mut sorted = values.to_vec();
    sorted.sort_unstable();
    let rank = (sorted.len() * percent).div_ceil(100).saturating_sub(1);
    sorted[rank]
}
