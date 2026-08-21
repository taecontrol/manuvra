#![cfg(target_os = "macos")]

use manuvra_chrome::{ChromeAdapter, Endpoint};
use manuvra_protocol::Invocation;
use manuvra_runtime::{InteractionModule, InvocationReply, Runtime, RuntimeConfig, TargetAdapter};
use serde_json::{Value, json};
use std::fs;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

const CHROME: &str = "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome";
const FIXTURE: &str = r#"<!doctype html>
<html lang="en"><meta charset="utf-8"><title>CP-06 Chrome Fixture</title>
<style>body{font:20px system-ui;margin:32px;min-height:1800px}button,input{font:inherit;padding:10px;margin:8px}#status{padding:20px;border:3px solid #274c77}</style>
<h1>CP-06 Chrome Fixture</h1>
<button id="save" aria-label="Save changes">Save changes</button>
<input id="email" aria-label="Email" value="old@example.test">
<button aria-label="Duplicate">One</button><button aria-label="Duplicate">Two</button>
<button aria-label="Duplicate">Three</button><button aria-label="Duplicate">Four</button>
<button aria-label="Duplicate">Five</button><button aria-label="Duplicate">Six</button>
<div id="status">Ready</div><div style="margin-top:1200px" id="bottom">Bottom</div>
<iframe src="/frame" title="Fixture frame"></iframe>
<script>
const status=document.querySelector('#status');
document.querySelector('#save').addEventListener('click',()=>{
  console.log('cp06-save-clicked'); status.textContent='Saving 1';
  fetch('/api').then(r=>r.text()).then(()=>status.textContent='Saved 2');
  setTimeout(()=>status.textContent='Saved — settled',70);
});
document.querySelector('#email').addEventListener('keydown',event=>{
  if(event.key==='Enter'){status.textContent='Enter — '+event.target.value;console.info('cp06-enter');}
});
</script></html>"#;

const NEXT: &str = r#"<!doctype html><html lang="en"><meta charset="utf-8"><title>CP-06 Next</title><h1>Navigation complete</h1></html>"#;
const FRAME: &str = r#"<!doctype html><html lang="en"><meta charset="utf-8"><title>Frame</title><button aria-label="Frame target">Frame target</button></html>"#;
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

struct FixtureServer {
    address: std::net::SocketAddr,
    stop: Arc<AtomicBool>,
    worker: Option<thread::JoinHandle<()>>,
}

impl FixtureServer {
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
                        serve_request(&mut stream);
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

impl Drop for FixtureServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

fn serve_request(stream: &mut std::net::TcpStream) {
    let mut request = [0_u8; 2048];
    let count = stream.read(&mut request).unwrap_or(0);
    if count == 0 {
        return;
    }
    let line = std::str::from_utf8(&request[..count])
        .ok()
        .and_then(|text| text.lines().next())
        .unwrap_or_default();
    let path = request_path(line.split_whitespace().nth(1).unwrap_or("/"));
    let path = path.split('?').next().unwrap_or(path);
    let (content_type, body) = match path {
        "/next" => ("text/html; charset=utf-8", NEXT.as_bytes()),
        "/frame" => ("text/html; charset=utf-8", FRAME.as_bytes()),
        "/busy" => ("text/html; charset=utf-8", BUSY.as_bytes()),
        "/regions" => ("text/html; charset=utf-8", REGIONS.as_bytes()),
        "/api" | "/poll" => ("text/plain; charset=utf-8", b"ok" as &[u8]),
        _ => ("text/html; charset=utf-8", FIXTURE.as_bytes()),
    };
    let _ = write!(
        stream,
        "HTTP/1.1 200 OK\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    let _ = stream.write_all(body);
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

struct ChromeProcess {
    child: Child,
    profile: PathBuf,
}

impl ChromeProcess {
    fn start(url: &str, root: &Path) -> (Self, u16) {
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
                "--remote-debugging-port=0",
                "--window-size=900,700",
                &format!("--user-data-dir={}", profile.display()),
                url,
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let port_file = profile.join("DevToolsActivePort");
        let deadline = Instant::now() + Duration::from_secs(8);
        let port = loop {
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
        };
        (Self { child, profile }, port)
    }
}

impl Drop for ChromeProcess {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = fs::remove_dir_all(&self.profile);
    }
}

struct Harness {
    _root: tempfile::TempDir,
    runtime: Runtime,
    target_id: String,
}

impl Harness {
    fn start(endpoint: Endpoint) -> Self {
        let root = tempfile::tempdir().unwrap();
        let adapter: Arc<dyn TargetAdapter> = Arc::new(ChromeAdapter::new(vec![endpoint]));
        let target_id = wait_for_target(&adapter);
        let runtime = Runtime::new(
            RuntimeConfig {
                temporary_root: root.path().join("tmp"),
                config_root: root.path().join("config"),
            },
            vec![adapter],
        )
        .unwrap();
        Self {
            _root: root,
            runtime,
            target_id,
        }
    }

    fn invoke(&self, id: &str, command: &str, input: Value, timeout_ms: u64) -> Value {
        let reply = self.reply(id, command, input, timeout_ms);
        assert_eq!(reply.exit_code, 0, "{id}/{command}: {}", reply.value);
        reply.value
    }

    fn reply(&self, id: &str, command: &str, input: Value, timeout_ms: u64) -> InvocationReply {
        self.runtime
            .invoke(Invocation::new(command, input, id.to_owned(), timeout_ms))
    }
}

fn wait_for_target(adapter: &Arc<dyn TargetAdapter>) -> String {
    let deadline = Instant::now() + Duration::from_secs(8);
    loop {
        if let Some(target) = adapter.targets().into_iter().next() {
            return target.target_id;
        }
        assert!(
            Instant::now() < deadline,
            "Chrome page target was not discovered"
        );
        thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn real_chrome_background_vertical_slice() {
    if std::env::var_os("MANUVRA_RUN_CHROME_TESTS").is_none() {
        return;
    }
    let fixture = FixtureServer::start();
    let process_root = tempfile::tempdir().unwrap();
    let (_chrome, port) = ChromeProcess::start(&fixture.url("/"), process_root.path());
    let harness = Harness::start(Endpoint::parse(&format!("127.0.0.1:{port}")).unwrap());
    let opened = harness.invoke(
        "open",
        "session.open",
        json!({"target_id": harness.target_id, "role": "actor", "mode": "background"}),
        5_000,
    );
    let session = opened["session_id"].as_str().unwrap();

    let screenshot = harness.invoke(
        "screenshot",
        "observe.screenshot",
        json!({"session_id": session}),
        5_000,
    );
    assert!(screenshot["width"].as_u64().unwrap() > 100);

    let query = harness.invoke(
        "query",
        "observe.query",
        json!({"session_id": session, "semantic": {"kind": "semantic", "role": "button", "name": "Save changes"}}),
        5_000,
    );
    assert_eq!(query["matches"].as_array().unwrap().len(), 1);

    let frame_query = harness.invoke(
        "frame-query",
        "observe.query",
        json!({"session_id": session, "semantic": {"kind": "semantic", "role": "button", "name": "Frame target"}}),
        5_000,
    );
    assert_eq!(frame_query["matches"].as_array().unwrap().len(), 1);
    let identified = harness.invoke(
        "identifier-query",
        "observe.query",
        json!({"session_id": session, "semantic": {"kind": "semantic", "identifier": "status", "text": "Ready"}}),
        5_000,
    );
    assert_eq!(identified["matches"].as_array().unwrap().len(), 1);
    let overflow = harness.invoke(
        "overflow-query",
        "observe.query",
        json!({"session_id": session, "semantic": {"kind": "semantic", "role": "button", "name": "Duplicate"}, "limit": 1}),
        5_000,
    );
    assert_eq!(overflow["matches"].as_array().unwrap().len(), 1);
    assert!(Path::new(overflow["overflow_path"].as_str().unwrap()).is_file());
    for (id, name, code) in [
        ("missing", "Not present", "element_not_found"),
        ("ambiguous", "Duplicate", "ambiguous_target"),
    ] {
        let reply = harness.reply(
            id,
            "action.click",
            json!({"session_id": session, "locator": {"kind": "semantic", "role": "button", "name": name}}),
            5_000,
        );
        assert_eq!(reply.value["error"]["code"], code);
        assert_eq!(reply.value["delivery"], "not_dispatched");
    }
    let save_query = harness.invoke(
        "save-ref-query",
        "observe.query",
        json!({"session_id": session, "semantic": {"kind": "semantic", "role": "button", "name": "Save changes"}}),
        5_000,
    );
    let save_ref = save_query["matches"][0]["ref"].as_str().unwrap();

    for (id, command, input) in [
        (
            "click",
            "action.click",
            json!({"session_id": session, "locator": {"kind": "ref", "ref": save_ref}}),
        ),
        (
            "type",
            "action.type",
            json!({"session_id": session, "locator": {"kind": "semantic", "role": "textbox", "name": "Email"}, "text": "agent@example.test", "replace": true}),
        ),
        (
            "press",
            "action.press",
            json!({"session_id": session, "locator": {"kind": "semantic", "role": "textbox", "name": "Email"}, "key": "Enter"}),
        ),
        (
            "scroll",
            "action.scroll",
            json!({"session_id": session, "delta_x": 0, "delta_y": 500}),
        ),
    ] {
        let result = harness.invoke(id, command, input, 5_000);
        assert_eq!(result["outcome"], "observed", "{command}: {result}");
        assert!(Path::new(result["observation"]["screenshot_path"].as_str().unwrap()).is_file());
    }
    let stale = harness.reply(
        "stale-ref",
        "action.click",
        json!({"session_id": session, "locator": {"kind": "ref", "ref": save_ref}}),
        5_000,
    );
    assert_eq!(stale.value["error"]["code"], "element_stale");

    let point_frame = harness.invoke(
        "point-frame",
        "observe.screenshot",
        json!({"session_id": session}),
        5_000,
    );
    let outside_point = harness.reply(
        "outside-point",
        "action.click",
        json!({"session_id": session, "locator": {"kind": "point", "x": 5000, "y": 5000, "frame_token": point_frame["frame_token"]}}),
        5_000,
    );
    assert_eq!(outside_point.value["error"]["code"], "element_not_found");
    assert_eq!(outside_point.value["delivery"], "not_dispatched");
    let point = harness.invoke(
        "point-click",
        "action.click",
        json!({"session_id": session, "locator": {"kind": "point", "x": 1, "y": 1, "frame_token": point_frame["frame_token"]}}),
        5_000,
    );
    assert_eq!(point["outcome"], "observed");
    let stale_point = harness.reply(
        "stale-point",
        "action.click",
        json!({"session_id": session, "locator": {"kind": "point", "x": 1, "y": 1, "frame_token": point_frame["frame_token"]}}),
        5_000,
    );
    assert_eq!(stale_point.value["error"]["code"], "frame_stale");

    let raw_query = harness.invoke(
        "raw-query",
        "raw.cdp",
        json!({"session_id": session, "intent": "query", "method": "Runtime.evaluate", "params": {"expression": "document.querySelector('#email').value", "returnByValue": true}}),
        5_000,
    );
    assert!(Path::new(raw_query["response_path"].as_str().unwrap()).is_file());
    let raw_value: Value =
        serde_json::from_slice(&fs::read(raw_query["response_path"].as_str().unwrap()).unwrap())
            .unwrap();
    assert_eq!(
        raw_value
            .pointer("/result/result/value")
            .and_then(Value::as_str),
        Some("agent@example.test")
    );
    let scroll_query = harness.invoke(
        "scroll-query",
        "raw.cdp",
        json!({"session_id": session, "intent": "query", "method": "Runtime.evaluate", "params": {"expression": "window.scrollY", "returnByValue": true}}),
        5_000,
    );
    let scroll_value: Value =
        serde_json::from_slice(&fs::read(scroll_query["response_path"].as_str().unwrap()).unwrap())
            .unwrap();
    assert!(
        scroll_value
            .pointer("/result/result/value")
            .and_then(Value::as_f64)
            .unwrap()
            > 0.0
    );
    let raw_error = harness.invoke(
        "raw-error",
        "raw.cdp",
        json!({"session_id": session, "intent": "query", "method": "ComputerUse.noSuchMethod", "params": {}}),
        5_000,
    );
    assert_eq!(raw_error["delivery"], "backend_rejected");
    let raw_error_value: Value =
        serde_json::from_slice(&fs::read(raw_error["response_path"].as_str().unwrap()).unwrap())
            .unwrap();
    assert!(raw_error_value.get("error").is_some());
    let raw_action_error = harness.reply(
        "raw-action-error",
        "raw.cdp",
        json!({"session_id": session, "intent": "action", "method": "ComputerUse.noSuchAction", "params": {}}),
        5_000,
    );
    assert_eq!(
        raw_action_error.value["error"]["code"],
        "raw_protocol_error"
    );
    let raw_action_manifest =
        PathBuf::from(raw_action_error.value["manifest_path"].as_str().unwrap());
    let raw_action_manifest: Value =
        serde_json::from_slice(&fs::read(raw_action_manifest).unwrap()).unwrap();
    let response_path = raw_action_manifest["artifacts"]
        .as_array()
        .unwrap()
        .iter()
        .find(|entry| entry["request_id"] == "raw-action-error")
        .unwrap()["absolute_path"]
        .as_str()
        .unwrap();
    let raw_action_error_value: Value =
        serde_json::from_slice(&fs::read(response_path).unwrap()).unwrap();
    assert!(raw_action_error_value.get("error").is_some());

    let raw_action = harness.invoke(
        "raw-action",
        "raw.cdp",
        json!({"session_id": session, "intent": "action", "method": "Runtime.evaluate", "params": {"expression": "document.querySelector('#status').textContent='Raw — settled'"}}),
        5_000,
    );
    assert_eq!(raw_action["outcome"], "observed");

    let tree = harness.invoke(
        "tree",
        "observe.tree",
        json!({"session_id": session}),
        10_000,
    );
    assert!(tree["node_count"].as_u64().unwrap() > 1);
    for kind in ["logs", "events", "diagnostics", "timings"] {
        let evidence = harness.invoke(
            &format!("evidence-{kind}"),
            "observe.evidence",
            json!({"session_id": session, "kind": kind}),
            5_000,
        );
        let path = Path::new(evidence["path"].as_str().unwrap());
        assert!(path.is_file());
        let contents = fs::read_to_string(path).unwrap();
        if kind == "logs" {
            assert!(contents.contains("cp06-save-clicked"));
        }
        if kind == "events" {
            assert!(contents.contains("Network.requestWillBeSent"));
            assert!(contents.contains("\"action_sequence\":1"));
        }
    }

    let navigate = harness.invoke(
        "navigate",
        "action.navigate",
        json!({"session_id": session, "url": fixture.url("/next")}),
        10_000,
    );
    assert_eq!(navigate["outcome"], "observed");
    let navigated_tree = harness.invoke(
        "navigated-tree",
        "observe.tree",
        json!({"session_id": session}),
        5_000,
    );
    assert!(navigated_tree["node_count"].as_u64().unwrap() > 1);
    let navigated_tree_contents =
        fs::read_to_string(navigated_tree["tree_path"].as_str().unwrap()).unwrap();
    assert!(navigated_tree_contents.contains("Navigation complete"));

    let session_dir = PathBuf::from(navigate["observation"]["screenshot_path"].as_str().unwrap())
        .parent()
        .unwrap()
        .to_path_buf();
    harness.invoke(
        "close",
        "session.close",
        json!({"session_id": session}),
        5_000,
    );
    assert!(!session_dir.exists());
}

#[test]
fn real_chrome_warm_latency_budget() {
    if std::env::var_os("MANUVRA_RUN_CHROME_BENCH").is_none() {
        return;
    }
    let fixture = FixtureServer::start();
    let process_root = tempfile::tempdir().unwrap();
    let (_chrome, port) = ChromeProcess::start(&fixture.url("/"), process_root.path());
    let harness = Harness::start(Endpoint::parse(&format!("127.0.0.1:{port}")).unwrap());
    let opened = harness.invoke(
        "bench-open",
        "session.open",
        json!({"target_id": harness.target_id, "role": "actor", "mode": "background"}),
        5_000,
    );
    let session = opened["session_id"].as_str().unwrap();
    let mut raw_total = Vec::new();
    let mut dispatch = Vec::new();
    let mut capture = Vec::new();
    let mut action_total = Vec::new();
    for index in 0..50 {
        let raw = harness.invoke(
            &format!("bench-raw-{index}"),
            "raw.cdp",
            json!({"session_id": session, "intent": "query", "method": "Runtime.evaluate", "params": {"expression": "40+2", "returnByValue": true}}),
            5_000,
        );
        raw_total.push(raw["timing_ms"]["total"].as_u64().unwrap());
        let action = harness.invoke(
            &format!("bench-action-{index}"),
            "raw.cdp",
            json!({"session_id": session, "intent": "action", "method": "Runtime.evaluate", "params": {"expression": format!("document.querySelector('#status').textContent='Bench {index}'")}}),
            5_000,
        );
        dispatch.push(action["timing_ms"]["dispatch"].as_u64().unwrap());
        capture.push(action["timing_ms"]["capture"].as_u64().unwrap());
        action_total.push(action["timing_ms"]["total"].as_u64().unwrap());
    }
    let report = json!({
        "schema": "manuvra/cp-06-chrome-benchmark@1",
        "samples": 50,
        "raw_query_total_ms": {"p95": percentile(&raw_total, 95), "values": raw_total},
        "action_dispatch_ms": {"p95": percentile(&dispatch, 95), "values": dispatch},
        "capture_ms": {"p95": percentile(&capture, 95), "values": capture},
        "action_total_ms": {"p95": percentile(&action_total, 95), "values": action_total},
    });
    assert!(
        report["raw_query_total_ms"]["p95"].as_u64().unwrap() <= 150,
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
    if let Some(path) = std::env::var_os("MANUVRA_CP06_BENCH_PATH") {
        let path = PathBuf::from(path);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, serde_json::to_vec_pretty(&report).unwrap()).unwrap();
    }
    harness.invoke(
        "bench-close",
        "session.close",
        json!({"session_id": session}),
        5_000,
    );
}

#[test]
fn navigate_settles_on_document_ready_while_network_continues() {
    if std::env::var_os("MANUVRA_RUN_CHROME_TESTS").is_none() {
        return;
    }
    let fixture = FixtureServer::start();
    let process_root = tempfile::tempdir().unwrap();
    let (_chrome, port) = ChromeProcess::start(&fixture.url("/"), process_root.path());
    let harness = Harness::start(Endpoint::parse(&format!("127.0.0.1:{port}")).unwrap());
    let opened = harness.invoke(
        "open",
        "session.open",
        json!({"target_id": harness.target_id, "role": "actor", "mode": "background"}),
        5_000,
    );
    let session = opened["session_id"].as_str().unwrap();
    let started = Instant::now();
    let navigate = harness.invoke(
        "busy-navigate",
        "action.navigate",
        json!({"session_id": session, "url": fixture.url("/busy")}),
        3_000,
    );
    assert_eq!(navigate["outcome"], "observed", "{navigate}");
    assert!(
        started.elapsed() < Duration::from_secs(3),
        "document-ready settle exceeded a modest deadline: {:?}",
        started.elapsed()
    );
    let heading = harness.invoke(
        "busy-heading",
        "observe.query",
        json!({"session_id": session, "semantic": {"kind": "semantic", "role": "heading", "name": "Busy document"}}),
        5_000,
    );
    assert_eq!(heading["matches"].as_array().unwrap().len(), 1);
    harness.invoke(
        "busy-close",
        "session.close",
        json!({"session_id": session}),
        5_000,
    );
}

#[test]
fn exact_ancestor_scope_disambiguates_and_query_exposes_description() {
    if std::env::var_os("MANUVRA_RUN_CHROME_TESTS").is_none() {
        return;
    }
    let fixture = FixtureServer::start();
    let process_root = tempfile::tempdir().unwrap();
    let (_chrome, port) = ChromeProcess::start(&fixture.url("/regions"), process_root.path());
    let harness = Harness::start(Endpoint::parse(&format!("127.0.0.1:{port}")).unwrap());
    let opened = harness.invoke(
        "open",
        "session.open",
        json!({"target_id": harness.target_id, "role": "actor", "mode": "background"}),
        5_000,
    );
    let session = opened["session_id"].as_str().unwrap();
    let query = harness.invoke(
        "checkout-query",
        "observe.query",
        json!({"session_id": session, "semantic": {"kind": "semantic", "role": "button", "name": "Checkout"}}),
        5_000,
    );
    assert_eq!(query["matches"].as_array().unwrap().len(), 2);
    assert!(
        query["matches"]
            .as_array()
            .unwrap()
            .iter()
            .any(|entry| entry["description"] == "Primary region checkout")
    );
    let unconstrained = harness.reply(
        "unconstrained-click",
        "action.click",
        json!({"session_id": session, "locator": {"kind": "semantic", "role": "button", "name": "Checkout"}}),
        5_000,
    );
    assert_eq!(unconstrained.value["error"]["code"], "ambiguous_target");
    assert_eq!(unconstrained.value["delivery"], "not_dispatched");
    let scoped = harness.invoke(
        "scoped-click",
        "action.click",
        json!({"session_id": session, "locator": {
            "kind": "semantic",
            "role": "button",
            "name": "Checkout",
            "within_role": "region",
            "within_name": "Primary"
        }}),
        5_000,
    );
    assert_eq!(scoped["outcome"], "observed", "{scoped}");
    harness.invoke(
        "regions-close",
        "session.close",
        json!({"session_id": session}),
        5_000,
    );
}

#[test]
fn click_follows_an_optional_following_document() {
    if std::env::var_os("MANUVRA_RUN_CHROME_TESTS").is_none() {
        return;
    }
    let fixture = FixtureServer::start();
    let process_root = tempfile::tempdir().unwrap();
    let (_chrome, port) = ChromeProcess::start(&fixture.url("/regions"), process_root.path());
    let harness = Harness::start(Endpoint::parse(&format!("127.0.0.1:{port}")).unwrap());
    let opened = harness.invoke(
        "open",
        "session.open",
        json!({"target_id": harness.target_id, "role": "actor", "mode": "background"}),
        5_000,
    );
    let session = opened["session_id"].as_str().unwrap();
    let toggle = harness.invoke(
        "toggle-click",
        "action.click",
        json!({"session_id": session, "locator": {"kind": "semantic", "role": "button", "name": "Toggle"}}),
        5_000,
    );
    assert_eq!(toggle["outcome"], "observed", "{toggle}");
    assert!(toggle.get("page_url").is_none());
    let status = harness.invoke(
        "toggle-status",
        "observe.query",
        json!({"session_id": session, "semantic": {"kind": "semantic", "identifier": "status", "text": "On"}}),
        5_000,
    );
    assert_eq!(status["matches"].as_array().unwrap().len(), 1);
    let navigated = harness.invoke(
        "continue-click",
        "action.click",
        json!({"session_id": session, "locator": {"kind": "semantic", "role": "link", "name": "Continue"}}),
        8_000,
    );
    assert_eq!(navigated["outcome"], "observed", "{navigated}");
    assert!(navigated.get("page_url").is_none());
    let heading = harness.invoke(
        "destination-heading",
        "observe.query",
        json!({"session_id": session, "semantic": {"kind": "semantic", "role": "heading", "name": "Navigation complete"}}),
        5_000,
    );
    assert_eq!(heading["matches"].as_array().unwrap().len(), 1);
    harness.invoke(
        "follow-close",
        "session.close",
        json!({"session_id": session}),
        5_000,
    );
}

fn percentile(values: &[u64], percent: usize) -> u64 {
    let mut sorted = values.to_vec();
    sorted.sort_unstable();
    let rank = (sorted.len() * percent).div_ceil(100).saturating_sub(1);
    sorted[rank]
}
