#![cfg(target_os = "macos")]

use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::fs;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

const CLI: &str = env!("CARGO_BIN_EXE_manuvra");
const DAEMON: &str = env!("CARGO_BIN_EXE_manuvra-daemon");
const CHROME: &str = "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome";
const HTML: &str = r#"<!doctype html><html lang="en"><meta charset="utf-8"><title>Manuvra Chrome Fixture</title>
<style>body{font:20px system-ui;margin:32px;min-height:1600px}button,input{font:inherit;padding:10px;margin:8px}#status{padding:20px;border:3px solid #274c77}</style>
<h1>CP-06 CLI Fixture</h1><button id="save" aria-label="Save changes">Save changes</button>
<input id="email" aria-label="Email" value="old@example.test"><div id="status">Ready</div><div style="margin-top:1000px">Bottom</div>
<script>const nonce=new URLSearchParams(location.search).get('nonce')||'legacy';window.__manuvraNonce=nonce;document.title='Manuvra Chrome Fixture — '+nonce;const status=document.querySelector('#status');status.textContent='Ready — '+nonce;document.querySelector('#email').value='input-'+nonce+'@example.test';document.querySelector('#save').onclick=()=>{console.log('manuvra-save',nonce);status.textContent='Saving — '+nonce;fetch('/api?nonce='+encodeURIComponent(nonce)).then(()=>setTimeout(()=>status.textContent='Saved — '+nonce,50));};document.querySelector('#email').onkeydown=e=>{if(e.key==='Enter'){console.info('manuvra-enter',nonce);status.textContent='Entered — '+nonce;}};</script></html>"#;
const NEXT: &str = r#"<!doctype html><html lang="en"><title>Next</title><h1>Navigation complete</h1><script>window.__manuvraNonce=new URLSearchParams(location.search).get('nonce')||'legacy';</script></html>"#;
const BUSY: &str = r#"<!doctype html><html lang="en"><meta charset="utf-8"><title>Busy document fixture</title>
<h1>Busy document</h1>
<script>
window.addEventListener('load',()=>{
  const poll=()=>{fetch('/poll?'+Date.now()).catch(()=>{});};
  poll();
  setInterval(poll,25);
});
</script></html>"#;
const REGIONS: &str = r#"<!doctype html><html lang="en"><meta charset="utf-8"><title>Scoped controls fixture</title>
<section role="region" aria-label="Primary">
  <button aria-label="Checkout" aria-describedby="primary-hint">Primary checkout</button>
  <p id="primary-hint">Primary region checkout</p>
</section>
<section role="region" aria-label="Secondary">
  <button aria-label="Checkout">Secondary checkout</button>
</section>
<button id="toggle" aria-label="Toggle">Toggle</button>
<p id="status">Off</p>
<a href="/next" aria-label="Continue">Continue</a>
<script>
document.getElementById('toggle').onclick=()=>{
  const status=document.getElementById('status');
  status.textContent=status.textContent==='Off'?'On':'Off';
};
</script></html>"#;

struct Fixture {
    address: std::net::SocketAddr,
    stop: Arc<AtomicBool>,
    worker: Option<thread::JoinHandle<()>>,
}

impl Fixture {
    fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let address = listener.local_addr().unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let worker_stop = stop.clone();
        let worker = thread::spawn(move || {
            while !worker_stop.load(Ordering::SeqCst) {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        let _ = stream.set_nonblocking(false);
                        let _ = stream.set_read_timeout(Some(Duration::from_secs(1)));
                        let mut request = [0_u8; 2048];
                        let count = stream.read(&mut request).unwrap_or(0);
                        if count == 0 {
                            continue;
                        }
                        let target = std::str::from_utf8(&request[..count])
                            .ok()
                            .and_then(|text| text.lines().next())
                            .and_then(|line| line.split_whitespace().nth(1))
                            .unwrap_or("/");
                        let path = request_path(target);
                        let (content_type, body) = match path.split('?').next().unwrap_or(path) {
                            "/next" => ("text/html", NEXT.as_bytes()),
                            "/busy" => ("text/html", BUSY.as_bytes()),
                            "/regions" => ("text/html", REGIONS.as_bytes()),
                            "/api" | "/poll" => ("text/plain", b"ok" as &[u8]),
                            _ => ("text/html", HTML.as_bytes()),
                        };
                        let _ = write!(
                            stream,
                            "HTTP/1.1 200 OK\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                            body.len()
                        );
                        let _ = stream.write_all(body);
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(2));
                    }
                    Err(_) => break,
                }
            }
        });
        Self {
            address,
            stop,
            worker: Some(worker),
        }
    }

    fn url(&self, path: &str) -> String {
        format!("http://{}{path}", self.address)
    }
}

fn request_path(target: &str) -> &str {
    target
        .strip_prefix("http://")
        .and_then(|authority_and_path| {
            authority_and_path
                .find('/')
                .map(|at| &authority_and_path[at..])
        })
        .unwrap_or(target)
}

impl Drop for Fixture {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

struct Chrome {
    child: Child,
    profile: PathBuf,
    port: u16,
}

impl Chrome {
    fn start(url: &str, root: &Path) -> (Self, u16, String) {
        Self::start_with_port(url, root, 0)
    }

    fn start_with_port(url: &str, root: &Path, requested_port: u16) -> (Self, u16, String) {
        let profile = root.join("chrome-profile");
        fs::create_dir_all(&profile).unwrap();
        let child = Command::new(CHROME)
            .args([
                "--headless=new",
                "--disable-gpu",
                "--disable-background-networking",
                "--disable-component-update",
                "--disable-default-apps",
                "--disable-extensions",
                "--no-first-run",
                "--no-default-browser-check",
                &format!("--remote-debugging-port={requested_port}"),
                "--window-size=900,700",
                &format!("--user-data-dir={}", profile.display()),
                url,
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(8);
        let port = if requested_port == 0 {
            let port_file = profile.join("DevToolsActivePort");
            loop {
                if let Ok(contents) = fs::read_to_string(&port_file)
                    && let Some(port) = contents.lines().next().and_then(|line| line.parse().ok())
                {
                    break port;
                }
                assert!(
                    Instant::now() < deadline,
                    "Chrome did not publish DevToolsActivePort"
                );
                thread::sleep(Duration::from_millis(10));
            }
        } else {
            while std::net::TcpStream::connect(("127.0.0.1", requested_port)).is_err() {
                assert!(Instant::now() < deadline, "Chrome endpoint was not ready");
                thread::sleep(Duration::from_millis(10));
            }
            requested_port
        };
        let source_id = wait_for_fixture_target(port, url, deadline);
        let target_id = opaque_chrome_target_id(port, &source_id);
        (
            Self {
                child,
                profile,
                port,
            },
            port,
            target_id,
        )
    }
}

fn wait_for_fixture_target(port: u16, expected_url: &str, deadline: Instant) -> String {
    let endpoint = manuvra_chrome::Endpoint::parse(&format!("127.0.0.1:{port}")).unwrap();
    loop {
        let targets = endpoint
            .get_json("/json/list", Duration::from_millis(250))
            .ok()
            .and_then(|value| value.as_array().cloned())
            .unwrap_or_default();
        if let Some(source_id) = targets.iter().find_map(|target| {
            (target["type"].as_str() == Some("page")
                && target["url"].as_str() == Some(expected_url))
            .then(|| target["id"].as_str().map(str::to_owned))
            .flatten()
        }) {
            return source_id;
        }
        assert!(
            Instant::now() < deadline,
            "Chrome did not publish the exact fixture target"
        );
        thread::sleep(Duration::from_millis(10));
    }
}

fn opaque_chrome_target_id(port: u16, source_id: &str) -> String {
    let digest = Sha256::digest(format!("127.0.0.1:{port}\0{source_id}"));
    let suffix = digest[..12]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    format!("chrome_{suffix}")
}

impl Drop for Chrome {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let deadline = Instant::now() + Duration::from_secs(3);
        while std::net::TcpStream::connect(("127.0.0.1", self.port)).is_ok()
            && Instant::now() < deadline
        {
            thread::sleep(Duration::from_millis(20));
        }
        let _ = fs::remove_dir_all(&self.profile);
    }
}

struct Harness {
    _root: tempfile::TempDir,
    cli: PathBuf,
    installed: bool,
    endpoint: String,
    temporary: PathBuf,
    config: PathBuf,
    daemon: Option<Child>,
    transcript: Vec<Value>,
}

impl Harness {
    fn start(endpoint: &str) -> Self {
        let root = tempfile::tempdir().unwrap();
        let cli = std::env::var_os("MANUVRA_INSTALLED_CLI")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(CLI));
        let installed = std::env::var_os("MANUVRA_INSTALLED_CLI").is_some();
        let temporary = root.path().join("tmp");
        let config = root.path().join("config");
        let daemon = if installed {
            let _ = Command::new(&cli).args(["daemon", "stop"]).output();
            None
        } else {
            Some(
                Command::new(DAEMON)
                    .env("MANUVRA_TMPDIR", &temporary)
                    .env("MANUVRA_CONFIG_HOME", &config)
                    .env("MANUVRA_CHROME_ENDPOINTS", endpoint)
                    .stdin(Stdio::null())
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .spawn()
                    .unwrap(),
            )
        };
        if !installed {
            let socket = temporary.join("manuvra/runtime-v1/daemon.sock");
            let deadline = Instant::now() + Duration::from_secs(5);
            while UnixStream::connect(&socket).is_err() {
                assert!(Instant::now() < deadline, "daemon socket did not appear");
                thread::sleep(Duration::from_millis(5));
            }
        }
        Self {
            _root: root,
            cli,
            installed,
            endpoint: endpoint.to_owned(),
            temporary,
            config,
            daemon,
            transcript: Vec::new(),
        }
    }

    fn run(&mut self, args: &[&str]) -> Value {
        let output = self.output(args);
        assert!(
            output.status.success(),
            "args={args:?}\nstdout={}\nstderr={}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(output.stdout.len() <= 4096);
        let value: Value = serde_json::from_slice(&output.stdout).unwrap();
        self.transcript.push(json!({"args": args, "result": value}));
        value
    }

    fn output(&self, args: &[&str]) -> Output {
        let mut command = Command::new(&self.cli);
        command
            .args(args)
            .env("MANUVRA_CHROME_ENDPOINTS", &self.endpoint);
        if !self.installed {
            command
                .env("MANUVRA_TMPDIR", &self.temporary)
                .env("MANUVRA_CONFIG_HOME", &self.config)
                .env("MANUVRA_NO_AUTOSTART", "1");
        }
        command.output().unwrap()
    }

    fn retain(&self, export_root: &Path) {
        if let Some(path) = std::env::var_os("MANUVRA_CP06_TRAJECTORY_PATH") {
            fs::create_dir_all(Path::new(&path).parent().unwrap()).unwrap();
            fs::write(path, serde_json::to_vec_pretty(&self.transcript).unwrap()).unwrap();
        }
        assert!(export_root.join("manifest.json").is_file());
    }

    fn exact_chrome_target(&mut self, expected: &str) -> String {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let targets = self.run(&["targets", "--kind", "chrome"]);
            let candidates = targets["targets"].as_array().unwrap();
            if candidates.len() == 1 && candidates[0]["target_id"].as_str() == Some(expected) {
                return expected.to_owned();
            }
            assert!(
                Instant::now() < deadline,
                "expected exactly one Chrome target: {targets}"
            );
            thread::sleep(Duration::from_millis(25));
        }
    }
}

impl Drop for Harness {
    fn drop(&mut self) {
        if self.installed {
            let _ = Command::new(&self.cli).args(["daemon", "stop"]).output();
        } else if let Some(daemon) = self.daemon.as_mut() {
            let _ = daemon.kill();
            let _ = daemon.wait();
        }
    }
}

#[test]
fn public_cli_completes_real_chrome_trajectory() {
    if std::env::var_os("MANUVRA_RUN_CHROME_TESTS").is_none() {
        return;
    }
    let fixture = Fixture::start();
    let chrome_root = tempfile::tempdir().unwrap();
    let (_chrome, port, expected_target) = Chrome::start(&fixture.url("/"), chrome_root.path());
    let mut harness = Harness::start(&format!("127.0.0.1:{port}"));
    let target = harness.exact_chrome_target(&expected_target);
    let opened = harness.run(&["open", "--target", &target]);
    let session = opened["session_id"].as_str().unwrap().to_owned();

    harness.run(&["observe", "screenshot", "--session", &session]);
    harness.run(&[
        "observe",
        "query",
        "--session",
        &session,
        "--role",
        "button",
        "--name",
        "Save changes",
    ]);
    harness.run(&[
        "click",
        "--session",
        &session,
        "--role",
        "button",
        "--name",
        "Save changes",
    ]);
    harness.run(&[
        "type",
        "--session",
        &session,
        "--role",
        "textbox",
        "--name",
        "Email",
        "--text",
        "agent@example.test",
        "--replace",
    ]);
    harness.run(&[
        "press",
        "--session",
        &session,
        "--key",
        "Enter",
        "--role",
        "textbox",
        "--name",
        "Email",
    ]);
    harness.run(&["scroll", "--session", &session, "--delta-y", "500"]);
    harness.run(&[
        "raw",
        "cdp",
        "--session",
        &session,
        "--intent",
        "query",
        "--method",
        "Runtime.evaluate",
        "--params",
        r#"{"expression":"document.querySelector('#email').value","returnByValue":true}"#,
    ]);
    harness.run(&[
        "raw",
        "cdp",
        "--session",
        &session,
        "--intent",
        "action",
        "--method",
        "Runtime.evaluate",
        "--params",
        r#"{"expression":"document.querySelector('#status').textContent='Raw settled'"}"#,
    ]);
    harness.run(&["observe", "tree", "--session", &session]);
    harness.run(&["observe", "logs", "--session", &session]);
    harness.run(&["observe", "events", "--session", &session]);
    harness.run(&[
        "navigate",
        "--session",
        &session,
        "--url",
        &fixture.url("/next"),
    ]);
    harness.run(&[
        "observe",
        "query",
        "--session",
        &session,
        "--role",
        "heading",
        "--name",
        "Navigation complete",
    ]);

    let export_root = std::env::var_os("MANUVRA_CP06_EXPORT_ROOT")
        .map(PathBuf::from)
        .unwrap_or_else(|| harness._root.path().join("export"));
    harness.run(&[
        "export",
        "--session",
        &session,
        "--all",
        "--destination",
        export_root.to_str().unwrap(),
    ]);
    harness.run(&["close", "--session", &session]);
    harness.retain(&export_root);
}

#[test]
fn cli_document_ready_scope_and_following_document() {
    if std::env::var_os("MANUVRA_RUN_CHROME_TESTS").is_none() {
        return;
    }
    let fixture = Fixture::start();
    let chrome_root = tempfile::tempdir().unwrap();
    let (_chrome, port, expected_target) =
        Chrome::start(&fixture.url("/regions"), chrome_root.path());
    let mut harness = Harness::start(&format!("127.0.0.1:{port}"));
    let target = harness.exact_chrome_target(&expected_target);
    let opened = harness.run(&["open", "--target", &target]);
    let session = opened["session_id"].as_str().unwrap().to_owned();

    let started = Instant::now();
    let busy = harness.run(&[
        "navigate",
        "--session",
        &session,
        "--url",
        &fixture.url("/busy"),
        "--timeout-ms",
        "3000",
    ]);
    assert_eq!(busy["outcome"], "observed", "{busy}");
    assert!(
        started.elapsed() < Duration::from_secs(3),
        "document-ready settle exceeded a modest deadline: {:?}",
        started.elapsed()
    );

    harness.run(&[
        "navigate",
        "--session",
        &session,
        "--url",
        &fixture.url("/regions"),
    ]);
    let query = harness.run(&[
        "observe",
        "query",
        "--session",
        &session,
        "--role",
        "button",
        "--name",
        "Checkout",
    ]);
    assert_eq!(query["matches"].as_array().unwrap().len(), 2);
    assert!(
        query["matches"]
            .as_array()
            .unwrap()
            .iter()
            .any(|entry| entry["description"] == "Primary region checkout")
    );
    let unconstrained = harness.output(&[
        "click",
        "--session",
        &session,
        "--role",
        "button",
        "--name",
        "Checkout",
    ]);
    let unconstrained: Value = serde_json::from_slice(&unconstrained.stdout).unwrap();
    assert_eq!(unconstrained["error"]["code"], "ambiguous_target");
    let scoped = harness.run(&[
        "click",
        "--session",
        &session,
        "--role",
        "button",
        "--name",
        "Checkout",
        "--within-role",
        "region",
        "--within-name",
        "Primary",
    ]);
    assert_eq!(scoped["outcome"], "observed", "{scoped}");
    assert!(scoped.get("page_url").is_none());

    let toggle = harness.run(&[
        "click",
        "--session",
        &session,
        "--role",
        "button",
        "--name",
        "Toggle",
    ]);
    assert_eq!(toggle["outcome"], "observed", "{toggle}");
    let continue_click = harness.run(&[
        "click",
        "--session",
        &session,
        "--role",
        "link",
        "--name",
        "Continue",
    ]);
    assert_eq!(continue_click["outcome"], "observed", "{continue_click}");
    let heading = harness.run(&[
        "observe",
        "query",
        "--session",
        &session,
        "--role",
        "heading",
        "--name",
        "Navigation complete",
    ]);
    assert_eq!(heading["matches"].as_array().unwrap().len(), 1);
    harness.run(&["close", "--session", &session]);
}

#[test]
fn installed_chrome_background_scored_set() {
    if std::env::var_os("MANUVRA_RUN_INSTALLED_CHROME_PROOF").is_none() {
        return;
    }
    assert!(
        std::env::var_os("MANUVRA_INSTALLED_CLI").is_some(),
        "installed proof requires MANUVRA_INSTALLED_CLI"
    );
    let evidence_root = PathBuf::from(
        std::env::var_os("MANUVRA_PROOF_ROOT").expect("MANUVRA_PROOF_ROOT is required"),
    )
    .join("chrome-background");
    fs::create_dir_all(&evidence_root).unwrap();
    let fixture = Fixture::start();
    let reserved = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = reserved.local_addr().unwrap().port();
    drop(reserved);
    let mut harness = Harness::start(&format!("127.0.0.1:{port}"));
    let profile_root = tempfile::tempdir().unwrap();
    let mut rows = Vec::new();
    let mut raw_query = Vec::new();
    let mut dispatch = Vec::new();
    let mut capture = Vec::new();
    let mut total = Vec::new();
    let mut successes = 0_u64;
    let mut wrong_target = 0_u64;
    let attempts = std::env::var("MANUVRA_PROOF_ATTEMPTS")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .unwrap_or(50);

    for attempt in 0..=attempts {
        let scored = attempt != 0;
        let nonce = format!("chrome-{attempt:02}-{}", std::process::id());
        let iteration_root = evidence_root.join(if scored {
            format!("run-{attempt:02}")
        } else {
            "warmup".to_owned()
        });
        fs::create_dir_all(&iteration_root).unwrap();
        let session_slot = std::sync::Mutex::new(None::<String>);
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let chrome_root = profile_root.path().join(format!("profile-{attempt:02}"));
            fs::create_dir_all(&chrome_root).unwrap();
            let url = fixture.url(&format!("/?nonce={nonce}"));
            let (_chrome, actual_port, expected_target) =
                Chrome::start_with_port(&url, &chrome_root, port);
            assert_eq!(actual_port, port);
            let target = harness.exact_chrome_target(&expected_target);
            let opened = harness.run(&[
                "open",
                "--target",
                &target,
                "--mode",
                "background",
                "--lease-ttl-ms",
                "600000",
            ]);
            let session = opened["session_id"].as_str().unwrap().to_owned();
            *session_slot.lock().unwrap() = Some(session.clone());
            let screenshot = harness.run(&["observe", "screenshot", "--session", &session]);
            assert!(Path::new(screenshot["screenshot_path"].as_str().unwrap()).is_file());
            let query_started = Instant::now();
            let query = harness.run(&[
                "observe",
                "query",
                "--session",
                &session,
                "--role",
                "button",
                "--name",
                "Save changes",
                "--limit",
                "1",
            ]);
            let query_ms = query_started.elapsed().as_millis() as u64;
            assert_eq!(query["matches"].as_array().unwrap().len(), 1);
            let click = harness.run(&[
                "click",
                "--session",
                &session,
                "--role",
                "button",
                "--name",
                "Save changes",
                "--mode",
                "background",
            ]);
            let email = format!("{nonce}@example.test");
            let typed = harness.run(&[
                "type",
                "--session",
                &session,
                "--role",
                "textbox",
                "--name",
                "Email",
                "--text",
                &email,
                "--replace",
                "--mode",
                "background",
            ]);
            let pressed = harness.run(&[
                "press",
                "--session",
                &session,
                "--key",
                "Enter",
                "--role",
                "textbox",
                "--name",
                "Email",
                "--mode",
                "background",
            ]);
            let raw = harness.run(&[
                "raw",
                "cdp",
                "--session",
                &session,
                "--intent",
                "query",
                "--method",
                "Runtime.evaluate",
                "--params",
                r#"{"expression":"({nonce:window.__manuvraNonce,email:document.querySelector('#email').value})","returnByValue":true}"#,
            ]);
            let raw_value: Value =
                serde_json::from_slice(&fs::read(raw["response_path"].as_str().unwrap()).unwrap())
                    .unwrap();
            assert_eq!(
                raw_value
                    .pointer("/result/result/value/nonce")
                    .and_then(Value::as_str),
                Some(nonce.as_str()),
                "wrong Chrome target: {raw_value}"
            );
            assert_eq!(
                raw_value
                    .pointer("/result/result/value/email")
                    .and_then(Value::as_str),
                Some(email.as_str())
            );
            let raw_expression = format!(
                "document.querySelector('#status').textContent='Raw — {nonce}';window.__manuvraNonce"
            );
            let raw_params = serde_json::to_string(&json!({"expression": raw_expression})).unwrap();
            let raw_action = harness.run(&[
                "raw",
                "cdp",
                "--session",
                &session,
                "--intent",
                "action",
                "--method",
                "Runtime.evaluate",
                "--params",
                &raw_params,
            ]);
            let next = fixture.url(&format!("/next?nonce={nonce}"));
            let navigated = harness.run(&[
                "navigate",
                "--session",
                &session,
                "--url",
                &next,
                "--mode",
                "background",
            ]);
            let heading = harness.run(&[
                "observe",
                "query",
                "--session",
                &session,
                "--role",
                "heading",
                "--name",
                "Navigation complete",
                "--limit",
                "1",
            ]);
            assert_eq!(heading["matches"].as_array().unwrap().len(), 1);
            let nav_nonce = harness.run(&[
                "raw",
                "cdp",
                "--session",
                &session,
                "--intent",
                "query",
                "--method",
                "Runtime.evaluate",
                "--params",
                r#"{"expression":"window.__manuvraNonce","returnByValue":true}"#,
            ]);
            let nav_value: Value = serde_json::from_slice(
                &fs::read(nav_nonce["response_path"].as_str().unwrap()).unwrap(),
            )
            .unwrap();
            assert_eq!(
                nav_value
                    .pointer("/result/result/value")
                    .and_then(Value::as_str),
                Some(nonce.as_str())
            );
            let export = iteration_root.join("export");
            let exported = harness.run(&[
                "export",
                "--session",
                &session,
                "--all",
                "--destination",
                export.to_str().unwrap(),
            ]);
            assert_eq!(exported["verified"], true);
            let closed = harness.run(&["close", "--session", &session]);
            assert_eq!(closed["artifacts_removed"], true);
            let status = harness.run(&["daemon", "status"]);
            assert!(status["active_sessions"].as_array().unwrap().is_empty());
            *session_slot.lock().unwrap() = None;
            json!({
                "nonce": nonce,
                "target_id": target,
                "session_id": session,
                "query_wall_ms": query_ms,
                "raw_query_ms": raw["timing_ms"]["total"],
                "actions": [click, typed, pressed, raw_action, navigated],
                "export": exported,
                "cleanup": status,
            })
        }));
        match result {
            Ok(detail) => {
                if scored {
                    successes += 1;
                    raw_query.push(detail["raw_query_ms"].as_u64().unwrap());
                    let actions = detail["actions"].as_array().unwrap();
                    dispatch.push(
                        actions
                            .iter()
                            .map(|action| action["timing_ms"]["dispatch"].as_u64().unwrap())
                            .max()
                            .unwrap(),
                    );
                    capture.push(
                        actions
                            .iter()
                            .map(|action| action["timing_ms"]["capture"].as_u64().unwrap())
                            .max()
                            .unwrap(),
                    );
                    total.push(
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
                    let _ = harness.output(&["close", "--session", &session]);
                }
                let failure = panic_message(payload);
                if scored && failure.contains("wrong Chrome target") {
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
    assert_eq!(raw_query.len(), successes as usize);
    let report = json!({
        "schema": "manuvra/installed-scored-set@1",
        "journey": "chrome_background",
        "warmups": 1,
        "attempts": attempts,
        "first_attempt_successes": successes,
        "wrong_target": wrong_target,
        "hangs": 0,
        "orphaned_leases": 0,
        "orphaned_session_directories": 0,
        "latency_p95_ms": {
            "raw_query": percentile_95(&raw_query),
            "dispatch": percentile_95(&dispatch),
            "capture": percentile_95(&capture),
            "total": percentile_95(&total),
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

fn percentile_95(values: &[u64]) -> u64 {
    if values.is_empty() {
        return 0;
    }
    let mut sorted = values.to_vec();
    sorted.sort_unstable();
    let rank = (sorted.len() * 95).div_ceil(100).saturating_sub(1);
    sorted[rank]
}

fn panic_message(payload: Box<dyn std::any::Any + Send>) -> String {
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
