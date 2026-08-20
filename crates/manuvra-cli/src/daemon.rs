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
use manuvra_runtime::fake::FakeAdapter;
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
        let runtime = runtime()?;
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
            let accepted =
                tokio::time::timeout(Duration::from_millis(100), self.listener.accept()).await;
            let (stream, _) = match accepted {
                Ok(accepted) => accepted?,
                Err(_) => continue,
            };
            if test_shutdown_requested() {
                return Ok(());
            }
            spawn_connection(
                stream,
                self.runtime.clone(),
                self.lifecycle.clone(),
                self.installation.clone(),
            )?;
        }
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

fn runtime() -> io::Result<Arc<Runtime>> {
    let adapters: Vec<Arc<dyn TargetAdapter>> =
        if cfg!(debug_assertions) && std::env::var_os("MANUVRA_TEST_FAKE_ADAPTER").is_some() {
            vec![Arc::new(FakeAdapter)]
        } else {
            production_adapters()?
        };
    Runtime::new(
        RuntimeConfig {
            temporary_root: temporary_root(),
            config_root: config_root(),
        },
        adapters,
    )
    .map(Arc::new)
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
    let envelope = match read_frame::<serde_json::Value>(&mut stream) {
        Ok(request) => request,
        Err(error) => {
            let response = RpcResponse::transport_error(String::new(), -32700, error.to_string());
            let _ = write_frame(&mut stream, &response);
            return;
        }
    };
    if envelope.get("control_protocol").is_some() {
        handle_control(&mut stream, envelope, &runtime, &lifecycle, &installation);
        return;
    }
    let request = match serde_json::from_value::<RpcRequest>(envelope) {
        Ok(request) => request,
        Err(error) => {
            let response = RpcResponse::transport_error(String::new(), -32600, error.to_string());
            let _ = write_frame(&mut stream, &response);
            return;
        }
    };
    if request.jsonrpc != "2.0"
        || request.method != "manuvra.invoke"
        || request.id.is_empty()
        || request.id != request.params.request_id
    {
        let response = RpcResponse::transport_error(
            request.id,
            -32600,
            "invalid JSON-RPC invocation envelope",
        );
        let _ = write_frame(&mut stream, &response);
        return;
    }
    let guard = match lifecycle.begin(&request.params.command) {
        Ok(guard) => guard,
        Err(code) => {
            let id = request.id;
            let (error, exit_code) = operational_error(code, None);
            let response = RpcResponse::result(id, serde_json::json!({"error": error}), exit_code);
            let _ = write_frame(&mut stream, &response);
            return;
        }
    };
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
    let _ = write_frame(&mut stream, &response);
    drop(guard);
}

fn handle_control(
    stream: &mut UnixStream,
    envelope: serde_json::Value,
    runtime: &Runtime,
    lifecycle: &DaemonLifecycle,
    installation: &Installation,
) {
    let request = match serde_json::from_value::<ControlRequest>(envelope) {
        Ok(request) if request.control_protocol == CONTROL_PROTOCOL => request,
        _ => return,
    };
    let result = match request.action {
        ControlAction::Status => Ok(()),
        ControlAction::Drain => {
            lifecycle.drain();
            Ok(())
        }
        ControlAction::Stop => lifecycle.stop(runtime),
    };
    let mut daemon = lifecycle.snapshot(runtime, installation);
    if request.action == ControlAction::Stop && result.is_ok() {
        daemon["stopped"] = serde_json::Value::Bool(true);
    }
    let (ok, error) = match result {
        Ok(()) => (true, None),
        Err(code) => (false, Some(operational_error(code, None).0)),
    };
    let response = ControlResponse {
        control_protocol: CONTROL_PROTOCOL,
        request_id: request.request_id,
        ok,
        daemon,
        error,
    };
    let _ = write_frame(stream, &response);
}

#[cfg(all(test, target_os = "macos"))]
mod tests {
    use super::*;
    use manuvra_protocol::Invocation;
    use serde_json::json;
    use std::io::Write;
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
}
