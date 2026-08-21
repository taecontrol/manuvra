use manuvra_chrome::ChromeAdapter;
use manuvra_cli::{
    DaemonSocket, Installation, RESPONSE_WRITE_RESERVE, config_root, socket_path, temporary_root,
};
#[cfg(target_os = "macos")]
use manuvra_macos::MacosAdapter;
use manuvra_protocol::{
    CONTROL_PROTOCOL, ControlAction, ControlRequest, ControlResponse, RpcRequest, RpcResponse,
    operational_error, read_frame, write_frame,
};
#[cfg(debug_assertions)]
use manuvra_runtime::fake::FakeAdapter;
#[cfg(debug_assertions)]
use manuvra_runtime::fake_diagnostics::ConfiguredDiagnostics;
use manuvra_runtime::{InteractionModule, Runtime, RuntimeConfig, TargetAdapter};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use std::{io, os::unix::net::UnixListener};

#[tokio::main(flavor = "multi_thread")]
async fn main() {
    if let Err(error) = serve().await {
        eprintln!("manuvra-daemon: {error}");
        std::process::exit(70);
    }
}

async fn serve() -> Result<(), String> {
    match Server::new() {
        Ok(server) => server.run().await.map_err(|error| error.to_string()),
        Err(error) => Err(error.to_string()),
    }
}

struct Server {
    _socket: DaemonSocket,
    listener: tokio::net::UnixListener,
    runtime: Arc<Runtime>,
    lifecycle: Arc<DaemonLifecycle>,
    installation: Arc<Installation>,
}

impl Server {
    fn new() -> io::Result<Self> {
        let installation = Installation::current().map_err(io::Error::other)?;
        // Singleton authority must be held before Runtime construction performs verified orphan
        // cleanup. A losing concurrent daemon must never inspect or remove the live daemon's
        // session directories.
        let socket = DaemonSocket::bind()?;
        let runtime = runtime(&installation)?;
        Self::with_bound_socket(socket, runtime, installation)
    }

    fn with_bound_socket(
        socket: DaemonSocket,
        runtime: Arc<Runtime>,
        installation: Installation,
    ) -> io::Result<Self> {
        tokio_listener(&socket).map(|listener| Self {
            _socket: socket,
            listener,
            runtime,
            lifecycle: Arc::new(DaemonLifecycle::new()),
            installation: Arc::new(installation),
        })
    }

    async fn run(self) -> io::Result<()> {
        loop {
            if self.lifecycle.should_exit(&self.runtime) {
                return Ok(());
            }
            match next_connection(&self.listener).await? {
                None => continue,
                Some(_) if test_shutdown_requested() => return Ok(()),
                Some(stream) => self.serve_connection(stream)?,
            }
        }
    }

    fn serve_connection(&self, stream: tokio::net::UnixStream) -> io::Result<()> {
        spawn_connection(
            stream,
            self.runtime.clone(),
            self.lifecycle.clone(),
            self.installation.clone(),
        )
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Admission {
    Open,
    Draining,
}

struct LifecycleState {
    admission: Admission,
    in_flight: usize,
    shutdown: bool,
    last_activity: Instant,
}

struct DaemonLifecycle {
    state: Mutex<LifecycleState>,
}

impl DaemonLifecycle {
    fn new() -> Self {
        Self {
            state: Mutex::new(LifecycleState {
                admission: Admission::Open,
                in_flight: 0,
                shutdown: false,
                last_activity: Instant::now(),
            }),
        }
    }

    fn begin(self: &Arc<Self>, command: &str) -> Result<InvocationGuard, &'static str> {
        let mut state = self.state.lock().expect("daemon lifecycle");
        if state.admission == Admission::Draining && !cleanup_command(command) {
            return Err("daemon_draining");
        }
        state.in_flight += 1;
        state.last_activity = Instant::now();
        Ok(InvocationGuard {
            lifecycle: self.clone(),
        })
    }

    fn drain(&self) {
        let mut state = self.state.lock().expect("daemon lifecycle");
        state.admission = Admission::Draining;
        state.last_activity = Instant::now();
    }

    fn stop(&self, runtime: &Runtime) -> Result<(), &'static str> {
        self.drain();
        let mut state = self.state.lock().expect("daemon lifecycle");
        if state.in_flight != 0 || runtime.has_active_sessions() {
            return Err("daemon_busy");
        }
        state.shutdown = true;
        Ok(())
    }

    fn should_exit(&self, runtime: &Runtime) -> bool {
        let state = self.state.lock().expect("daemon lifecycle");
        state.shutdown
            || state.in_flight == 0
                && !runtime.has_active_sessions()
                && state.last_activity.elapsed() >= idle_timeout()
    }

    fn snapshot(&self, runtime: &Runtime, installation: &Installation) -> serde_json::Value {
        let state = self.state.lock().expect("daemon lifecycle");
        let runtime = runtime.lifecycle_snapshot();
        serde_json::json!({
            "running": true,
            "pid": std::process::id(),
            "build_id": manuvra_protocol::build_digest(),
            "registry_version": manuvra_protocol::REGISTRY_VERSION,
            "resource_manifest_sha256": manuvra_protocol::sha256_hex(manuvra_protocol::RELEASE_MANIFEST_JSON.as_bytes()),
            "canonical_bundle": installation.bundle,
            "canonical_daemon": installation.daemon,
            "socket": socket_path(),
            "socket_owner_uid": unsafe { libc::geteuid() },
            "admission": if state.admission == Admission::Open { "open" } else { "draining" },
            "in_flight": state.in_flight,
            "active_sessions": runtime["active_sessions"],
            "pending_requests": runtime["pending_requests"],
        })
    }
}

struct InvocationGuard {
    lifecycle: Arc<DaemonLifecycle>,
}

impl Drop for InvocationGuard {
    fn drop(&mut self) {
        let mut state = self.lifecycle.state.lock().expect("daemon lifecycle");
        state.in_flight = state.in_flight.saturating_sub(1);
        state.last_activity = Instant::now();
    }
}

fn cleanup_command(command: &str) -> bool {
    matches!(
        command,
        "session.close" | "request.cancel" | "artifact.export" | "system.doctor"
    )
}

fn idle_timeout() -> Duration {
    if cfg!(debug_assertions)
        && let Some(value) = std::env::var_os("MANUVRA_TEST_IDLE_MS")
        && let Ok(milliseconds) = value.to_string_lossy().parse::<u64>()
    {
        return Duration::from_millis(milliseconds.max(10));
    }
    Duration::from_secs(5 * 60)
}

fn test_shutdown_requested() -> bool {
    cfg!(debug_assertions)
        && std::env::var_os("MANUVRA_TEST_SHUTDOWN_FILE")
            .is_some_and(|path| Path::new(&path).is_file())
}

fn tokio_listener(socket: &DaemonSocket) -> io::Result<tokio::net::UnixListener> {
    socket
        .try_clone_listener()
        .and_then(configure_nonblocking)
        .and_then(tokio::net::UnixListener::from_std)
}

fn configure_nonblocking(listener: UnixListener) -> io::Result<UnixListener> {
    listener.set_nonblocking(true).map(|()| listener)
}

async fn next_connection(
    listener: &tokio::net::UnixListener,
) -> io::Result<Option<tokio::net::UnixStream>> {
    match tokio::time::timeout(Duration::from_millis(100), listener.accept()).await {
        Ok(accepted) => Ok(Some(accepted?.0)),
        Err(_) => Ok(None),
    }
}

fn spawn_connection(
    stream: tokio::net::UnixStream,
    runtime: Arc<Runtime>,
    lifecycle: Arc<DaemonLifecycle>,
    installation: Arc<Installation>,
) -> io::Result<()> {
    let stream = stream.into_std()?;
    stream.set_nonblocking(false)?;
    tokio::task::spawn_blocking(move || handle(stream, runtime, lifecycle, installation));
    Ok(())
}

fn runtime(installation: &Installation) -> io::Result<Arc<Runtime>> {
    #[cfg(debug_assertions)]
    if std::env::var_os("MANUVRA_TEST_FAKE_ADAPTER").as_deref() == Some(std::ffi::OsStr::new("1")) {
        return debug_fake_runtime(installation);
    }
    let adapters = production_adapters()?;
    Runtime::new(
        RuntimeConfig {
            temporary_root: temporary_root(),
            config_root: config_root(),
        },
        adapters,
    )
    .map(|runtime| Arc::new(runtime.with_setup_installation(installation.identity())))
    .map_err(io::Error::other)
}

#[cfg(debug_assertions)]
fn debug_fake_runtime(installation: &Installation) -> io::Result<Arc<Runtime>> {
    let configured = match std::env::var_os("MANUVRA_TEST_DIAGNOSTICS_CONFIG") {
        Some(path) => Some(
            ConfiguredDiagnostics::load(Path::new(&path), &temporary_root())
                .map_err(io::Error::other)?,
        ),
        None => None,
    };
    let adapters: Vec<Arc<dyn TargetAdapter>> = configured
        .as_ref()
        .map(|scenario| vec![scenario.adapter.clone()])
        .unwrap_or_else(|| vec![Arc::new(FakeAdapter)]);
    let setup_installation = configured
        .as_ref()
        .map(|scenario| scenario.installation.clone())
        .unwrap_or_else(|| installation.identity());
    let doctor_warnings = configured
        .map(|scenario| scenario.doctor_warnings)
        .unwrap_or_default();
    Runtime::new(
        RuntimeConfig {
            temporary_root: temporary_root(),
            config_root: config_root(),
        },
        adapters,
    )
    .map(|runtime| {
        Arc::new(
            runtime
                .with_setup_installation(setup_installation)
                .with_doctor_warnings(doctor_warnings),
        )
    })
    .map_err(io::Error::other)
}

fn production_adapters() -> io::Result<Vec<Arc<dyn TargetAdapter>>> {
    let mut adapters: Vec<Arc<dyn TargetAdapter>> = vec![Arc::new(
        ChromeAdapter::from_env().map_err(|error| io::Error::other(error.to_string()))?,
    )];
    #[cfg(target_os = "macos")]
    adapters.push(Arc::new(MacosAdapter::new().map_err(io::Error::other)?));
    Ok(adapters)
}

fn handle(
    mut stream: UnixStream,
    runtime: Arc<Runtime>,
    lifecycle: Arc<DaemonLifecycle>,
    installation: Arc<Installation>,
) {
    if DaemonSocket::verify_peer(&stream).is_err() {
        return;
    }
    let Some(envelope) = read_or_reject_frame(&mut stream) else {
        return;
    };
    if envelope.get("control_protocol").is_some() {
        handle_control(&mut stream, envelope, &runtime, &lifecycle, &installation);
        return;
    }
    handle_invocation(stream, envelope, runtime, lifecycle);
}

fn read_or_reject_frame(stream: &mut UnixStream) -> Option<serde_json::Value> {
    match read_frame(stream) {
        Ok(request) => Some(request),
        Err(error) => {
            let response = RpcResponse::transport_error(String::new(), -32700, error.to_string());
            let _ = write_frame(stream, &response);
            None
        }
    }
}

fn handle_invocation(
    mut stream: UnixStream,
    envelope: serde_json::Value,
    runtime: Arc<Runtime>,
    lifecycle: Arc<DaemonLifecycle>,
) {
    let request = match parse_rpc_request(envelope) {
        Ok(request) => request,
        Err(response) => {
            let _ = write_frame(&mut stream, &response);
            return;
        }
    };
    if let Err(response) = validate_rpc_envelope(&request) {
        let _ = write_frame(&mut stream, &response);
        return;
    }
    match lifecycle.begin(&request.params.command) {
        Ok(guard) => write_invocation_reply(&mut stream, request, &runtime, guard),
        Err(code) => write_admission_error(&mut stream, request.id, code),
    }
}

fn parse_rpc_request(envelope: serde_json::Value) -> Result<RpcRequest, RpcResponse> {
    serde_json::from_value(envelope)
        .map_err(|error| RpcResponse::transport_error(String::new(), -32600, error.to_string()))
}

fn validate_rpc_envelope(request: &RpcRequest) -> Result<(), RpcResponse> {
    if request.jsonrpc == "2.0"
        && request.method == "manuvra.invoke"
        && !request.id.is_empty()
        && request.id == request.params.request_id
    {
        Ok(())
    } else {
        Err(RpcResponse::transport_error(
            request.id.clone(),
            -32600,
            "invalid JSON-RPC invocation envelope",
        ))
    }
}

fn write_admission_error(stream: &mut UnixStream, id: String, code: &'static str) {
    let (error, exit_code) = operational_error(code, None);
    let response = RpcResponse::result(id, serde_json::json!({"error": error}), exit_code);
    let _ = write_frame(stream, &response);
}

fn write_invocation_reply(
    stream: &mut UnixStream,
    request: RpcRequest,
    runtime: &Runtime,
    guard: InvocationGuard,
) {
    let id = request.id;
    let deadline =
        Instant::now() + Duration::from_millis(request.params.deadline_ms) + RESPONSE_WRITE_RESERVE;
    let reply = runtime.invoke(request.params);
    let response = RpcResponse::result(id, reply.value, reply.exit_code);
    let _ = stream.set_write_timeout(Some(
        deadline
            .saturating_duration_since(Instant::now())
            .max(Duration::from_nanos(1)),
    ));
    let _ = write_frame(stream, &response);
    drop(guard);
}

fn handle_control(
    stream: &mut UnixStream,
    envelope: serde_json::Value,
    runtime: &Runtime,
    lifecycle: &DaemonLifecycle,
    installation: &Installation,
) {
    let Some(request) = parse_control_request(envelope) else {
        return;
    };
    let result = apply_control_action(request.action, runtime, lifecycle);
    let daemon = control_daemon_snapshot(request.action, &result, runtime, lifecycle, installation);
    let response = control_reply(request, result, daemon);
    let _ = write_frame(stream, &response);
}

fn parse_control_request(envelope: serde_json::Value) -> Option<ControlRequest> {
    match serde_json::from_value::<ControlRequest>(envelope) {
        Ok(request) if request.control_protocol == CONTROL_PROTOCOL => Some(request),
        _ => None,
    }
}

fn apply_control_action(
    action: ControlAction,
    runtime: &Runtime,
    lifecycle: &DaemonLifecycle,
) -> Result<(), &'static str> {
    match action {
        ControlAction::Status => Ok(()),
        ControlAction::Drain => {
            lifecycle.drain();
            Ok(())
        }
        ControlAction::Stop => lifecycle.stop(runtime),
    }
}

fn control_daemon_snapshot(
    action: ControlAction,
    result: &Result<(), &'static str>,
    runtime: &Runtime,
    lifecycle: &DaemonLifecycle,
    installation: &Installation,
) -> serde_json::Value {
    let mut daemon = lifecycle.snapshot(runtime, installation);
    if action == ControlAction::Stop && result.is_ok() {
        daemon["stopped"] = serde_json::Value::Bool(true);
    }
    daemon
}

fn control_reply(
    request: ControlRequest,
    result: Result<(), &'static str>,
    daemon: serde_json::Value,
) -> ControlResponse {
    let (ok, error) = match result {
        Ok(()) => (true, None),
        Err(code) => (false, Some(operational_error(code, None).0)),
    };
    ControlResponse {
        control_protocol: CONTROL_PROTOCOL,
        request_id: request.request_id,
        ok,
        daemon,
        error,
    }
}

#[cfg(all(test, target_os = "macos"))]
mod tests {
    use super::*;
    use manuvra_protocol::Invocation;
    use serde_json::json;
    use std::io::{Read, Write};
    use std::thread;

    fn test_runtime() -> Arc<Runtime> {
        let root = tempfile::tempdir().unwrap().keep();
        Arc::new(
            Runtime::new(
                RuntimeConfig {
                    temporary_root: root.join("tmp"),
                    config_root: root.join("config"),
                },
                vec![Arc::new(FakeAdapter)],
            )
            .unwrap(),
        )
    }

    fn test_installation() -> Arc<Installation> {
        Arc::new(Installation::current().unwrap())
    }

    fn exchange(request: RpcRequest) -> RpcResponse {
        let (mut client, server) = UnixStream::pair().unwrap();
        let worker = thread::spawn(move || {
            handle(
                server,
                test_runtime(),
                Arc::new(DaemonLifecycle::new()),
                test_installation(),
            )
        });
        write_frame(&mut client, &request).unwrap();
        let response = read_frame(&mut client).unwrap();
        worker.join().unwrap();
        response
    }

    #[test]
    fn handler_returns_runtime_results_and_rejects_bad_envelopes() {
        let invocation = Invocation::new("target.list", json!({}), "r_valid".to_owned(), 500);
        let valid = exchange(RpcRequest::invocation(invocation.clone()));
        assert!(valid.result.is_some());

        let mut invalid = RpcRequest::invocation(invocation);
        invalid.method = "wrong.method".to_owned();
        assert_eq!(exchange(invalid).error.unwrap().code, -32600);

        let invocation = Invocation::new("target.list", json!({}), "r_inner".to_owned(), 500);
        let mut mismatched = RpcRequest::invocation(invocation);
        mismatched.id = "r_outer".to_owned();
        assert_eq!(exchange(mismatched).error.unwrap().code, -32600);
    }

    #[test]
    fn handler_returns_parse_error_for_malformed_frame() {
        let (mut client, server) = UnixStream::pair().unwrap();
        let worker = thread::spawn(move || {
            handle(
                server,
                test_runtime(),
                Arc::new(DaemonLifecycle::new()),
                test_installation(),
            )
        });
        client.write_all(&3_u32.to_be_bytes()).unwrap();
        client.write_all(b"bad").unwrap();
        let response: RpcResponse = read_frame(&mut client).unwrap();
        assert_eq!(response.error.unwrap().code, -32700);
        worker.join().unwrap();
    }

    fn exchange_control(
        action: ControlAction,
        runtime: Arc<Runtime>,
        lifecycle: Arc<DaemonLifecycle>,
    ) -> ControlResponse {
        let (mut client, server) = UnixStream::pair().unwrap();
        let installation = test_installation();
        let worker = thread::spawn(move || handle(server, runtime, lifecycle, installation));
        write_frame(
            &mut client,
            &ControlRequest::new("c_control".to_owned(), action),
        )
        .unwrap();
        let response = read_frame(&mut client).unwrap();
        worker.join().unwrap();
        response
    }

    #[test]
    fn control_status_drain_and_stop_keep_their_verbs() {
        let runtime = test_runtime();
        let lifecycle = Arc::new(DaemonLifecycle::new());
        let status = exchange_control(ControlAction::Status, runtime.clone(), lifecycle.clone());
        assert!(status.ok);
        assert_eq!(status.daemon["admission"], "open");
        assert!(status.daemon.get("stopped").is_none());

        let drained = exchange_control(ControlAction::Drain, runtime.clone(), lifecycle.clone());
        assert!(drained.ok);
        assert_eq!(drained.daemon["admission"], "draining");

        let stopped = exchange_control(ControlAction::Stop, runtime, lifecycle);
        assert!(stopped.ok);
        assert_eq!(stopped.daemon["stopped"], true);
    }

    #[test]
    fn control_stop_stays_busy_while_a_session_is_open() {
        let runtime = test_runtime();
        let opened = runtime.invoke(Invocation::new(
            "session.open",
            json!({
                "target_id": "chrome_fake_1",
                "role": "actor",
                "mode": "background",
                "lease_ttl_ms": 120_000
            }),
            "r_open".to_owned(),
            500,
        ));
        assert_eq!(opened.exit_code, 0);
        let busy = exchange_control(
            ControlAction::Stop,
            runtime,
            Arc::new(DaemonLifecycle::new()),
        );
        assert!(!busy.ok);
        assert_eq!(busy.error.as_ref().unwrap().code, "daemon_busy");
        assert!(busy.daemon.get("stopped").is_none());
    }

    #[test]
    fn control_rejects_the_wrong_protocol_without_a_reply() {
        let (mut client, server) = UnixStream::pair().unwrap();
        let worker = thread::spawn(move || {
            handle(
                server,
                test_runtime(),
                Arc::new(DaemonLifecycle::new()),
                test_installation(),
            )
        });
        write_frame(
            &mut client,
            &json!({
                "control_protocol": 99,
                "request_id": "c_bad",
                "action": "status"
            }),
        )
        .unwrap();
        client.shutdown(std::net::Shutdown::Write).unwrap();
        let mut buf = [0_u8; 1];
        assert_eq!(client.read(&mut buf).unwrap(), 0);
        worker.join().unwrap();
    }

    #[test]
    fn draining_daemon_rejects_new_work_and_still_allows_cleanup() {
        let lifecycle = Arc::new(DaemonLifecycle::new());
        lifecycle.drain();
        let (mut client, server) = UnixStream::pair().unwrap();
        let runtime = test_runtime();
        let installation = test_installation();
        let lifecycle_for_handle = lifecycle.clone();
        let worker =
            thread::spawn(move || handle(server, runtime, lifecycle_for_handle, installation));
        write_frame(
            &mut client,
            &RpcRequest::invocation(Invocation::new(
                "target.list",
                json!({}),
                "r_drain".to_owned(),
                500,
            )),
        )
        .unwrap();
        let rejected: RpcResponse = read_frame(&mut client).unwrap();
        worker.join().unwrap();
        let error = rejected.result.unwrap();
        assert_eq!(error["error"]["code"], "daemon_draining");

        let (mut client, server) = UnixStream::pair().unwrap();
        let worker =
            thread::spawn(move || handle(server, test_runtime(), lifecycle, test_installation()));
        write_frame(
            &mut client,
            &RpcRequest::invocation(Invocation::new(
                "system.doctor",
                json!({}),
                "r_doctor".to_owned(),
                500,
            )),
        )
        .unwrap();
        let cleanup: RpcResponse = read_frame(&mut client).unwrap();
        worker.join().unwrap();
        assert!(cleanup.result.is_some());
        assert!(cleanup.error.is_none());
    }
}
