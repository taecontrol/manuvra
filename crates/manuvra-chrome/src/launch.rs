use crate::endpoint::Endpoint;
use serde_json::{Value, json};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

pub const GOOGLE_CHROME_MACOS: &str =
    "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome";

const PROBE_TIMEOUT: Duration = Duration::from_millis(200);
const POLL_INTERVAL: Duration = Duration::from_millis(50);

#[derive(Debug, Clone)]
pub struct LaunchRequest {
    pub endpoint: Endpoint,
    pub profile: PathBuf,
    pub binary: PathBuf,
    pub timeout: Duration,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LaunchError {
    Unavailable(String),
    EndpointBusy(String),
    TimedOut(String),
    InvalidEndpoint(String),
}

impl LaunchError {
    pub fn catalog_code(&self) -> &'static str {
        match self {
            Self::Unavailable(_) => "chrome_unavailable",
            Self::EndpointBusy(_) => "chrome_endpoint_busy",
            Self::TimedOut(_) => "timed_out",
            Self::InvalidEndpoint(_) => "invalid_request",
        }
    }

    pub fn message(&self) -> &str {
        match self {
            Self::Unavailable(message)
            | Self::EndpointBusy(message)
            | Self::TimedOut(message)
            | Self::InvalidEndpoint(message) => message,
        }
    }
}

pub fn launch_dedicated_chrome(request: LaunchRequest) -> Result<Value, LaunchError> {
    let deadline = Instant::now() + request.timeout;
    match probe(&request.endpoint, remaining(deadline)?) {
        Probe::Chrome { pages } if pages >= 1 => Ok(launch_result("reused", &request)),
        Probe::Chrome { .. } => wait_for_page(&request, deadline, "reused"),
        Probe::Occupied(message) => Err(LaunchError::EndpointBusy(message)),
        Probe::Refused => spawn_and_wait(&request, deadline),
    }
}

fn spawn_and_wait(request: &LaunchRequest, deadline: Instant) -> Result<Value, LaunchError> {
    if !request.binary.is_file() {
        return Err(LaunchError::Unavailable(format!(
            "Google Chrome was not found at {}",
            request.binary.display()
        )));
    }
    ensure_private_directory(&request.profile)?;
    spawn_detached_chrome(request)?;
    wait_for_page(request, deadline, "launched")
}

fn ensure_private_directory(path: &Path) -> Result<(), LaunchError> {
    fs::create_dir_all(path).map_err(|error| {
        LaunchError::Unavailable(format!(
            "dedicated Chrome profile could not be created: {error}"
        ))
    })?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700)).map_err(|error| {
            LaunchError::Unavailable(format!(
                "dedicated Chrome profile permissions could not be set: {error}"
            ))
        })?;
    }
    Ok(())
}

fn spawn_detached_chrome(request: &LaunchRequest) -> Result<(), LaunchError> {
    let mut command = chrome_command(&request.binary, &request.endpoint, &request.profile);
    // Detach into a new process group so the CLI exiting cannot deliver SIGHUP.
    // Never kill or wait for this child; daily Chrome is also never quit.
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    command
        .spawn()
        .map_err(|error| {
            LaunchError::Unavailable(format!(
                "Google Chrome could not be spawned at {}: {error}",
                request.binary.display()
            ))
        })
        .map(std::mem::forget)
}

fn chrome_command(binary: &Path, endpoint: &Endpoint, profile: &Path) -> Command {
    let mut command = Command::new(binary);
    command
        .arg(format!("--remote-debugging-address={}", endpoint.ip()))
        .arg(format!("--remote-debugging-port={}", endpoint.port()))
        .arg(format!("--user-data-dir={}", profile.display()))
        .arg("--no-first-run")
        .arg("--no-default-browser-check")
        .arg("about:blank")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    command
}

fn wait_for_page(
    request: &LaunchRequest,
    deadline: Instant,
    state: &str,
) -> Result<Value, LaunchError> {
    loop {
        let timeout = remaining(deadline)?;
        match probe(&request.endpoint, timeout.min(PROBE_TIMEOUT)) {
            Probe::Chrome { pages } if pages >= 1 => return Ok(launch_result(state, request)),
            Probe::Chrome { .. } | Probe::Refused | Probe::Occupied(_) => {
                thread::sleep(POLL_INTERVAL.min(remaining(deadline)?));
            }
        }
    }
}

fn launch_result(state: &str, request: &LaunchRequest) -> Value {
    json!({
        "state": state,
        "endpoint": request.endpoint.label(),
        "profile": request.profile,
    })
}

enum Probe {
    Refused,
    Chrome { pages: usize },
    Occupied(String),
}

fn probe(endpoint: &Endpoint, timeout: Duration) -> Probe {
    match endpoint.get_json("/json/list", timeout) {
        Ok(value) => match discoverable_page_count(&value) {
            Some(pages) => Probe::Chrome { pages },
            None => Probe::Occupied(
                "listener answered but Chrome /json/list was not an array".to_owned(),
            ),
        },
        Err(error) if error.is_connection_refused() => Probe::Refused,
        Err(error) => Probe::Occupied(error.to_string()),
    }
}

fn discoverable_page_count(value: &Value) -> Option<usize> {
    let items = value.as_array()?;
    Some(
        items
            .iter()
            .filter(|item| {
                item.get("type").and_then(Value::as_str) == Some("page")
                    && item
                        .get("webSocketDebuggerUrl")
                        .and_then(Value::as_str)
                        .is_some()
            })
            .count(),
    )
}

fn remaining(deadline: Instant) -> Result<Duration, LaunchError> {
    let remaining = deadline.saturating_duration_since(Instant::now());
    if remaining.is_zero() {
        return Err(LaunchError::TimedOut(
            "deadline elapsed before a Chrome target was discoverable".to_owned(),
        ));
    }
    Ok(remaining)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    struct ListServer {
        address: std::net::SocketAddr,
        stop: Arc<AtomicBool>,
        worker: Option<thread::JoinHandle<()>>,
    }

    impl ListServer {
        fn start(body: Vec<u8>, status: &'static str) -> Self {
            Self::bind(TcpListener::bind("127.0.0.1:0").unwrap(), body, status)
        }

        fn start_at(address: std::net::SocketAddr, body: Vec<u8>, status: &'static str) -> Self {
            Self::bind(TcpListener::bind(address).unwrap(), body, status)
        }

        fn bind(listener: TcpListener, body: Vec<u8>, status: &'static str) -> Self {
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
                                "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
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

    fn request_for(endpoint: Endpoint, root: &Path) -> LaunchRequest {
        LaunchRequest {
            endpoint,
            profile: root.join("chrome-dedicated"),
            binary: root.join("missing-chrome"),
            timeout: Duration::from_millis(400),
        }
    }

    #[test]
    fn launch_reuses_an_answering_chrome_list_without_spawning() {
        let body = serde_json::to_vec(&json!([{
            "id": "page-1",
            "type": "page",
            "title": "Fixture",
            "webSocketDebuggerUrl": "ws://127.0.0.1:0/devtools/page/abc"
        }]))
        .unwrap();
        let server = ListServer::start(body, "200 OK");
        let endpoint = Endpoint::parse(&server.address.to_string()).unwrap();
        let temp = tempfile::tempdir().unwrap();
        let result = launch_dedicated_chrome(request_for(endpoint.clone(), temp.path())).unwrap();
        assert_eq!(result["state"], "reused");
        assert_eq!(result["endpoint"], endpoint.label());
        assert_eq!(
            result["profile"].as_str().unwrap(),
            temp.path().join("chrome-dedicated").to_str().unwrap()
        );
        assert!(!temp.path().join("missing-chrome").exists());
    }

    #[test]
    fn missing_binary_fails_honestly_when_the_endpoint_is_refused() {
        let endpoint = Endpoint::parse("127.0.0.1:1").unwrap();
        let temp = tempfile::tempdir().unwrap();
        let error = launch_dedicated_chrome(request_for(endpoint, temp.path())).unwrap_err();
        assert_eq!(error.catalog_code(), "chrome_unavailable");
        assert!(error.message().contains("was not found"));
    }

    #[test]
    fn empty_chrome_list_times_out_without_spawning() {
        let server = ListServer::start(b"[]".to_vec(), "200 OK");
        let endpoint = Endpoint::parse(&server.address.to_string()).unwrap();
        let temp = tempfile::tempdir().unwrap();
        let error = launch_dedicated_chrome(request_for(endpoint, temp.path())).unwrap_err();
        assert_eq!(error.catalog_code(), "timed_out");
        assert!(!temp.path().join("chrome-dedicated").exists());
        assert!(!temp.path().join("missing-chrome").exists());
    }

    #[test]
    fn occupied_non_cdp_port_fails_without_spawning() {
        let server = ListServer::start(b"not-chrome".to_vec(), "200 OK");
        let endpoint = Endpoint::parse(&server.address.to_string()).unwrap();
        let temp = tempfile::tempdir().unwrap();
        let error = launch_dedicated_chrome(request_for(endpoint, temp.path())).unwrap_err();
        assert_eq!(error.catalog_code(), "chrome_endpoint_busy");
        assert!(!temp.path().join("chrome-dedicated").exists());
    }

    #[test]
    fn chrome_command_is_visible_detached_and_never_opens_amazon() {
        let endpoint = Endpoint::parse("127.0.0.1:9222").unwrap();
        let profile = PathBuf::from("/tmp/manuvra-chrome-dedicated");
        let command = chrome_command(Path::new(GOOGLE_CHROME_MACOS), &endpoint, &profile);
        let args: Vec<String> = command
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect();
        assert_eq!(command.get_program(), GOOGLE_CHROME_MACOS);
        assert!(args.contains(&"--remote-debugging-address=127.0.0.1".to_owned()));
        assert!(args.contains(&"--remote-debugging-port=9222".to_owned()));
        assert!(args.contains(&"--user-data-dir=/tmp/manuvra-chrome-dedicated".to_owned()));
        assert!(args.contains(&"--no-first-run".to_owned()));
        assert!(args.contains(&"--no-default-browser-check".to_owned()));
        assert!(args.contains(&"about:blank".to_owned()));
        assert!(!args.iter().any(|arg| arg.contains("headless")));
        assert!(!args.iter().any(|arg| arg.contains("amazon")));
    }

    #[test]
    fn non_loopback_endpoints_are_rejected_before_attach() {
        assert!(Endpoint::parse("192.0.2.1:9222").is_err());
        assert!(Endpoint::configured(Some("10.0.0.1:9222")).is_err());
    }

    #[test]
    fn launch_spawns_when_the_endpoint_is_refused_and_returns_launched() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        drop(listener);
        let endpoint = Endpoint::parse(&address.to_string()).unwrap();
        let body = serde_json::to_vec(&json!([{
            "id": "page-1",
            "type": "page",
            "webSocketDebuggerUrl": "ws://127.0.0.1/devtools/page/abc"
        }]))
        .unwrap();
        let server = Arc::new(std::sync::Mutex::new(None));
        let delayed = server.clone();
        thread::spawn(move || {
            thread::sleep(Duration::from_millis(80));
            *delayed.lock().expect("delayed list server") =
                Some(ListServer::start_at(address, body, "200 OK"));
        });
        let temp = tempfile::tempdir().unwrap();
        let profile = temp.path().join("chrome-dedicated");
        let result = launch_dedicated_chrome(LaunchRequest {
            endpoint: endpoint.clone(),
            profile: profile.clone(),
            binary: PathBuf::from("/usr/bin/true"),
            timeout: Duration::from_secs(2),
        })
        .unwrap();
        drop(server);
        assert_eq!(result["state"], "launched");
        assert_eq!(result["endpoint"], endpoint.label());
        assert_eq!(
            result["profile"].as_str().unwrap(),
            profile.to_str().unwrap()
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(&profile).unwrap().permissions().mode() & 0o777,
                0o700
            );
        }
    }
}
