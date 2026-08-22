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
const NEW_BLANK_PAGE_PATH: &str = "/json/new?about%3Ablank";

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
        empty @ Probe::Chrome { .. } => wait_for_page(&request, deadline, "reused", empty),
        Probe::Occupied(message) => Err(LaunchError::EndpointBusy(message)),
        Probe::Refused => spawn_and_wait(&request, deadline),
        transient @ Probe::Transient => wait_for_page(&request, deadline, "reused", transient),
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
    wait_for_page(request, deadline, "launched", Probe::Refused)
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
    mut current: Probe,
) -> Result<Value, LaunchError> {
    let mut creation_attempted = false;
    loop {
        if let Some(result) =
            settle_current_probe(request, deadline, state, current, &mut creation_attempted)?
        {
            return Ok(result);
        }
        current = probe(&request.endpoint, probe_timeout(deadline)?);
    }
}

fn settle_current_probe(
    request: &LaunchRequest,
    deadline: Instant,
    state: &str,
    current: Probe,
    creation_attempted: &mut bool,
) -> Result<Option<Value>, LaunchError> {
    match current {
        Probe::Chrome { pages } if pages >= 1 => Ok(Some(launch_result(state, request))),
        Probe::Occupied(message) => Err(LaunchError::EndpointBusy(message)),
        Probe::Chrome { .. } if !*creation_attempted => {
            // Mark before dispatch because every response failure is ambiguous:
            // Chrome may have created the page. Never replay this mutation.
            *creation_attempted = true;
            let _ = request
                .endpoint
                .put_json(NEW_BLANK_PAGE_PATH, probe_timeout(deadline)?);
            Ok(None)
        }
        Probe::Chrome { .. } | Probe::Refused | Probe::Transient => {
            thread::sleep(POLL_INTERVAL.min(remaining(deadline)?));
            Ok(None)
        }
    }
}

fn probe_timeout(deadline: Instant) -> Result<Duration, LaunchError> {
    Ok(remaining(deadline)?.min(PROBE_TIMEOUT))
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
    Transient,
    Chrome { pages: usize },
    Occupied(String),
}

fn probe(endpoint: &Endpoint, timeout: Duration) -> Probe {
    match endpoint.get_json_for_probe("/json/list", timeout) {
        Ok(value) => match discoverable_page_count(&value) {
            Some(pages) => Probe::Chrome { pages },
            None => Probe::Occupied(
                "listener answered but Chrome /json/list was not an array".to_owned(),
            ),
        },
        Err(error) if error.is_connection_refused() => Probe::Refused,
        Err(error) if error.is_transient() => Probe::Transient,
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
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    struct ListServer {
        address: std::net::SocketAddr,
        stop: Arc<AtomicBool>,
        worker: Option<thread::JoinHandle<()>>,
    }

    impl ListServer {
        fn start(body: Vec<u8>, status: &'static str) -> Self {
            Self::bind(TcpListener::bind("127.0.0.1:0").unwrap(), body, status)
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

    #[derive(Clone, Copy)]
    enum RecoveryMode {
        ExistingPage,
        CreateSucceeds,
        CreateResponseIsAmbiguous,
        TransientListReadAfterCreate,
        PartialListBodyAfterCreate,
        MalformedAfterCreate,
    }

    struct RecoveryServer {
        address: std::net::SocketAddr,
        create_requests: Arc<AtomicUsize>,
        list_requests: Arc<AtomicUsize>,
        stop: Arc<AtomicBool>,
        worker: Option<thread::JoinHandle<()>>,
    }

    impl RecoveryServer {
        fn start(mode: RecoveryMode) -> Self {
            Self::bind(TcpListener::bind("127.0.0.1:0").unwrap(), mode)
        }

        fn start_at(address: std::net::SocketAddr, mode: RecoveryMode) -> Self {
            Self::bind(TcpListener::bind(address).unwrap(), mode)
        }

        fn bind(listener: TcpListener, mode: RecoveryMode) -> Self {
            listener.set_nonblocking(true).unwrap();
            let address = listener.local_addr().unwrap();
            let create_requests = Arc::new(AtomicUsize::new(0));
            let worker_create_requests = create_requests.clone();
            let list_requests = Arc::new(AtomicUsize::new(0));
            let worker_list_requests = list_requests.clone();
            let stop = Arc::new(AtomicBool::new(false));
            let worker_stop = stop.clone();
            let created = Arc::new(AtomicBool::new(false));
            let worker_created = created.clone();
            let worker = thread::spawn(move || {
                while !worker_stop.load(Ordering::SeqCst) {
                    match listener.accept() {
                        Ok((mut stream, _)) => {
                            let _ = stream.set_nonblocking(false);
                            let mut request = [0_u8; 1024];
                            let count = stream.read(&mut request).unwrap_or_default();
                            let request = String::from_utf8_lossy(&request[..count]);
                            let request_line = request.lines().next().unwrap_or_default();
                            if request_line == "PUT /json/new?about%3Ablank HTTP/1.1" {
                                worker_create_requests.fetch_add(1, Ordering::SeqCst);
                                worker_created.store(true, Ordering::SeqCst);
                                if matches!(mode, RecoveryMode::CreateResponseIsAmbiguous) {
                                    continue;
                                }
                                write_json_response(&mut stream, &page_body());
                            } else if request_line == "GET /json/list HTTP/1.1" {
                                let list_request =
                                    worker_list_requests.fetch_add(1, Ordering::SeqCst) + 1;
                                if hold_first_list_after_create(
                                    mode,
                                    worker_created.load(Ordering::SeqCst),
                                    list_request,
                                    &mut stream,
                                ) {
                                    continue;
                                }
                                write_json_response(&mut stream, &list_body(mode, &worker_created));
                            } else {
                                let _ = write!(
                                    stream,
                                    "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                                );
                            }
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
                create_requests,
                list_requests,
                stop,
                worker: Some(worker),
            }
        }

        fn create_request_count(&self) -> usize {
            self.create_requests.load(Ordering::SeqCst)
        }

        fn list_request_count(&self) -> usize {
            self.list_requests.load(Ordering::SeqCst)
        }
    }

    impl Drop for RecoveryServer {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::SeqCst);
            if let Some(worker) = self.worker.take() {
                let _ = worker.join();
            }
        }
    }

    fn page_body() -> Vec<u8> {
        serde_json::to_vec(&json!([{
            "id": "page-1",
            "type": "page",
            "title": "Fixture",
            "webSocketDebuggerUrl": "ws://127.0.0.1:0/devtools/page/abc"
        }]))
        .unwrap()
    }

    fn write_json_response(stream: &mut impl Write, body: &[u8]) {
        let _ = write!(
            stream,
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        );
        let _ = stream.write_all(body);
    }

    fn hold_first_list_after_create(
        mode: RecoveryMode,
        created: bool,
        list_request: usize,
        stream: &mut impl Write,
    ) -> bool {
        if !created || list_request != 2 {
            return false;
        }
        match mode {
            RecoveryMode::TransientListReadAfterCreate => {
                thread::sleep(PROBE_TIMEOUT.saturating_mul(2));
                true
            }
            RecoveryMode::PartialListBodyAfterCreate => {
                write_held_partial_json(stream, &page_body());
                true
            }
            _ => false,
        }
    }

    fn write_held_partial_json(stream: &mut impl Write, body: &[u8]) {
        let prefix = &body[..body.len() / 2];
        let _ = write!(
            stream,
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        );
        let _ = stream.write_all(prefix);
        let _ = stream.flush();
        thread::sleep(PROBE_TIMEOUT.saturating_mul(2));
    }

    fn list_body(mode: RecoveryMode, created: &AtomicBool) -> Vec<u8> {
        match mode {
            RecoveryMode::ExistingPage => page_body(),
            RecoveryMode::MalformedAfterCreate if created.load(Ordering::SeqCst) => {
                b"not-chrome".to_vec()
            }
            _ if created.load(Ordering::SeqCst) => page_body(),
            _ => b"[]".to_vec(),
        }
    }

    fn request_for(endpoint: Endpoint, root: &Path) -> LaunchRequest {
        LaunchRequest {
            endpoint,
            profile: root.join("chrome-dedicated"),
            binary: root.join("missing-chrome"),
            timeout: Duration::from_secs(2),
        }
    }

    #[test]
    fn launch_reuses_an_answering_chrome_list_without_spawning() {
        let server = RecoveryServer::start(RecoveryMode::ExistingPage);
        let endpoint = Endpoint::parse(&server.address.to_string()).unwrap();
        let temp = tempfile::tempdir().unwrap();
        let result = launch_dedicated_chrome(request_for(endpoint.clone(), temp.path())).unwrap();
        assert_eq!(
            result,
            json!({
                "state": "reused",
                "endpoint": endpoint.label(),
                "profile": temp.path().join("chrome-dedicated"),
            })
        );
        assert!(!temp.path().join("missing-chrome").exists());
        assert_eq!(server.create_request_count(), 0);
        assert_eq!(server.list_request_count(), 1);
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
    fn empty_chrome_list_creates_one_blank_page_and_reuses_endpoint() {
        let server = RecoveryServer::start(RecoveryMode::CreateSucceeds);
        let endpoint = Endpoint::parse(&server.address.to_string()).unwrap();
        let temp = tempfile::tempdir().unwrap();
        let result = launch_dedicated_chrome(request_for(endpoint, temp.path())).unwrap();
        assert_eq!(result["state"], "reused");
        assert_eq!(server.create_request_count(), 1);
        assert_eq!(server.list_request_count(), 2);
        assert!(!temp.path().join("chrome-dedicated").exists());
        assert!(!temp.path().join("missing-chrome").exists());
    }

    #[test]
    fn ambiguous_create_response_is_not_retried() {
        let server = RecoveryServer::start(RecoveryMode::CreateResponseIsAmbiguous);
        let endpoint = Endpoint::parse(&server.address.to_string()).unwrap();
        let temp = tempfile::tempdir().unwrap();
        let result = launch_dedicated_chrome(request_for(endpoint, temp.path())).unwrap();
        assert_eq!(result["state"], "reused");
        assert_eq!(server.create_request_count(), 1);
        assert_eq!(server.list_request_count(), 2);
    }

    #[test]
    fn transient_list_read_after_creation_retries_discovery_without_recreating() {
        let server = RecoveryServer::start(RecoveryMode::TransientListReadAfterCreate);
        let endpoint = Endpoint::parse(&server.address.to_string()).unwrap();
        let temp = tempfile::tempdir().unwrap();
        let result = launch_dedicated_chrome(request_for(endpoint, temp.path())).unwrap();
        assert_eq!(result["state"], "reused");
        assert_eq!(server.create_request_count(), 1);
        assert!(server.list_request_count() >= 3);
    }

    #[test]
    fn partial_list_body_timeout_after_creation_retries_discovery_without_recreating() {
        let server = RecoveryServer::start(RecoveryMode::PartialListBodyAfterCreate);
        let endpoint = Endpoint::parse(&server.address.to_string()).unwrap();
        let temp = tempfile::tempdir().unwrap();
        let result = launch_dedicated_chrome(request_for(endpoint, temp.path())).unwrap();
        assert_eq!(result["state"], "reused");
        assert_eq!(server.create_request_count(), 1);
        assert!(server.list_request_count() >= 3);
    }

    #[test]
    fn malformed_endpoint_after_creation_fails_promptly_without_retry() {
        let server = RecoveryServer::start(RecoveryMode::MalformedAfterCreate);
        let endpoint = Endpoint::parse(&server.address.to_string()).unwrap();
        let temp = tempfile::tempdir().unwrap();
        let error = launch_dedicated_chrome(request_for(endpoint, temp.path())).unwrap_err();
        assert_eq!(error.catalog_code(), "chrome_endpoint_busy");
        assert_eq!(server.create_request_count(), 1);
        assert_eq!(server.list_request_count(), 2);
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
    fn launch_recovers_an_empty_endpoint_after_spawning_and_returns_launched() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        drop(listener);
        let endpoint = Endpoint::parse(&address.to_string()).unwrap();
        let server = Arc::new(std::sync::Mutex::new(None));
        let delayed = server.clone();
        thread::spawn(move || {
            thread::sleep(Duration::from_millis(80));
            *delayed.lock().expect("delayed list server") = Some(RecoveryServer::start_at(
                address,
                RecoveryMode::CreateSucceeds,
            ));
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
        assert_eq!(
            result,
            json!({
                "state": "launched",
                "endpoint": endpoint.label(),
                "profile": profile,
            })
        );
        let server = server.lock().expect("recovery server");
        let server = server.as_ref().expect("recovery server started");
        assert_eq!(server.create_request_count(), 1);
        assert_eq!(server.list_request_count(), 2);
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
