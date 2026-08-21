use serde_json::Value;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::PathBuf;
use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::Duration;

const CLI: &str = env!("CARGO_BIN_EXE_manuvra");

struct ListServer {
    address: std::net::SocketAddr,
    stop: Arc<AtomicBool>,
    worker: Option<thread::JoinHandle<()>>,
}

impl ListServer {
    fn start(body: Vec<u8>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let address = listener.local_addr().unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let worker_stop = stop.clone();
        let body = Arc::new(body);
        let worker = thread::spawn(move || {
            while !worker_stop.load(Ordering::SeqCst) {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        let _ = stream.set_nonblocking(false);
                        let mut request = [0_u8; 1024];
                        let _ = stream.read(&mut request);
                        let _ = write!(
                            stream,
                            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                            body.len()
                        );
                        let _ = stream.write_all(&body);
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
}

impl Drop for ListServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

fn cli(config: &PathBuf, endpoint: &str, args: &[&str]) -> (Value, i32) {
    let output = Command::new(CLI)
        .args(args)
        .env("MANUVRA_CONFIG_HOME", config)
        .env("MANUVRA_CHROME_ENDPOINTS", endpoint)
        .env("MANUVRA_NO_AUTOSTART", "1")
        .output()
        .unwrap();
    let value: Value = serde_json::from_slice(&output.stdout).unwrap_or_else(|_| {
        panic!(
            "stdout={} stderr={}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        )
    });
    (value, output.status.code().unwrap_or(1))
}

#[test]
fn commands_get_and_help_expose_chrome_launch() {
    let root = tempfile::tempdir().unwrap();
    let (help, status) = cli(
        &root.path().join("config"),
        "127.0.0.1:1",
        &["commands", "get", "system.chrome.launch"],
    );
    assert_eq!(status, 0, "{help}");
    assert_eq!(help["command"], "system.chrome.launch");
    assert!(
        help["examples"]
            .as_array()
            .unwrap()
            .iter()
            .any(|example| example == "manuvra chrome launch")
    );
    assert!(
        help["errors"]
            .as_array()
            .unwrap()
            .iter()
            .any(|error| error == "chrome_unavailable")
    );

    let output = Command::new(CLI)
        .args(["chrome", "launch", "--help"])
        .output()
        .unwrap();
    let text = String::from_utf8_lossy(&output.stdout);
    assert!(output.status.success(), "{text}");
    assert!(text.contains("chrome"));
    assert!(text.contains("launch"));
}

#[test]
fn chrome_launch_reuses_an_answering_cdp_endpoint() {
    let body = serde_json::to_vec(&serde_json::json!([{
        "id": "page-1",
        "type": "page",
        "webSocketDebuggerUrl": "ws://127.0.0.1:0/devtools/page/abc"
    }]))
    .unwrap();
    let server = ListServer::start(body);
    let root = tempfile::tempdir().unwrap();
    let config = root.path().join("config");
    let (value, status) = cli(&config, &server.address.to_string(), &["chrome", "launch"]);
    assert_eq!(status, 0, "{value}");
    assert_eq!(value["state"], "reused");
    assert_eq!(value["endpoint"], server.address.to_string());
    assert_eq!(
        value["profile"].as_str().unwrap(),
        config.join("chrome-dedicated").to_str().unwrap()
    );
    manuvra_protocol::validate_command_result("system.chrome.launch", &value).unwrap();
}

#[test]
fn chrome_launch_fails_honestly_when_a_non_cdp_listener_owns_the_port() {
    let server = ListServer::start(b"not-chrome".to_vec());
    let root = tempfile::tempdir().unwrap();
    let (value, status) = cli(
        &root.path().join("config"),
        &server.address.to_string(),
        &["chrome", "launch"],
    );
    assert_eq!(status, 4, "{value}");
    assert_eq!(value["error"]["code"], "chrome_endpoint_busy");
    assert_eq!(
        value["error"]["help_command"],
        "manuvra commands errors chrome_endpoint_busy"
    );
    assert!(
        value["error"]["recovery_command"]
            .as_str()
            .unwrap()
            .contains("manuvra chrome launch")
    );
}
