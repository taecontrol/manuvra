mod chrome;
mod installation;
mod maintenance;

pub use chrome::chrome_launch;
pub use installation::{Installation, InstallationError};
pub use maintenance::{legacy_config_root, migrate_legacy, purge_owned_roots};

use manuvra_protocol::{
    ControlAction, ControlRequest, ControlResponse, Invocation, ProtocolError, RpcRequest,
    RpcResponse, build_digest, read_frame, write_frame,
};
use serde_json::Value;
use std::fs::{self, File, OpenOptions};
use std::io;
use std::os::fd::AsRawFd;
use std::os::unix::fs::{FileTypeExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::os::unix::net::{Incoming, UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

pub const RESPONSE_WRITE_RESERVE: Duration = Duration::from_millis(25);

pub fn temporary_root() -> PathBuf {
    if cfg!(debug_assertions)
        && let Some(root) = std::env::var_os("MANUVRA_TMPDIR")
    {
        return PathBuf::from(root);
    }
    std::env::temp_dir()
}

pub fn config_root() -> PathBuf {
    if cfg!(debug_assertions)
        && let Some(root) = std::env::var_os("MANUVRA_CONFIG_HOME")
    {
        return PathBuf::from(root);
    }
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/nonexistent"))
        .join(".config/manuvra")
}

pub fn runtime_root() -> PathBuf {
    temporary_root().join("manuvra/runtime-v1")
}

pub fn socket_path() -> PathBuf {
    runtime_root().join("daemon.sock")
}

pub fn invoke_daemon(mut invocation: Invocation) -> Result<RpcResponse, ClientError> {
    let deadline = Instant::now() + Duration::from_millis(invocation.deadline_ms);
    ensure_compatible_daemon(deadline)?;
    let (mut stream, child) = connect_or_launch(deadline)?;
    let remaining = remaining_request_time(deadline)?;
    invocation.deadline_ms = runtime_deadline_ms(remaining)?;
    let response = exchange_invocation(&mut stream, invocation, remaining)?;
    drop(child);
    Ok(response)
}

fn runtime_deadline_ms(remaining: Duration) -> Result<u64, ClientError> {
    remaining
        .checked_sub(RESPONSE_WRITE_RESERVE)
        .filter(|budget| *budget >= Duration::from_millis(50))
        .ok_or(ClientError::Deadline)
        .map(|budget| budget.as_millis().min(u128::from(u64::MAX)) as u64)
}

fn exchange_invocation(
    stream: &mut UnixStream,
    invocation: Invocation,
    remaining: Duration,
) -> Result<RpcResponse, ClientError> {
    stream.set_read_timeout(Some(remaining))?;
    stream.set_write_timeout(Some(remaining))?;
    write_frame(stream, &RpcRequest::invocation(invocation))?;
    Ok(read_frame(stream)?)
}

pub fn daemon_status() -> Result<Value, ClientError> {
    match send_control(ControlAction::Status, Duration::from_secs(2)) {
        Ok(response) => control_value(response),
        Err(ClientError::Io(error))
            if matches!(
                error.kind(),
                io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused
            ) =>
        {
            Ok(serde_json::json!({"running": false, "build_id": null}))
        }
        Err(error) => Err(error),
    }
}

pub fn daemon_stop() -> Result<Value, ClientError> {
    match send_control(ControlAction::Stop, Duration::from_secs(5)) {
        Ok(response) => control_value(response),
        Err(ClientError::Io(error))
            if matches!(
                error.kind(),
                io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused
            ) =>
        {
            Ok(serde_json::json!({"running": false, "stopped": true, "already_stopped": true}))
        }
        Err(error) => Err(error),
    }
}

fn control_value(response: ControlResponse) -> Result<Value, ClientError> {
    if response.ok {
        Ok(response.daemon)
    } else {
        Err(ClientError::Control(
            Box::new(response.error.ok_or_else(|| {
                ClientError::Launch("control failure omitted its typed error".to_owned())
            })?),
            response.daemon,
        ))
    }
}

fn ensure_compatible_daemon(deadline: Instant) -> Result<(), ClientError> {
    let Some(status) = compatible_daemon_status(deadline)? else {
        return Ok(());
    };
    if status.daemon["build_id"].as_str() == Some(&build_digest()) {
        return Ok(());
    }
    stop_incompatible_daemon(deadline)
}

fn compatible_daemon_status(deadline: Instant) -> Result<Option<ControlResponse>, ClientError> {
    match send_control(
        ControlAction::Status,
        deadline.saturating_duration_since(Instant::now()),
    ) {
        Ok(status) => Ok(Some(status)),
        Err(error) if daemon_absent(&error) => Ok(None),
        Err(error) => Err(error),
    }
}

fn stop_incompatible_daemon(deadline: Instant) -> Result<(), ClientError> {
    stop_running_daemon(deadline)?;
    wait_for_socket_removal(deadline)
}

fn stop_running_daemon(deadline: Instant) -> Result<(), ClientError> {
    let stopped = send_control(
        ControlAction::Stop,
        deadline.saturating_duration_since(Instant::now()),
    )?;
    control_value(stopped).map(|_| ())
}

fn daemon_absent(error: &ClientError) -> bool {
    matches!(
        error,
        ClientError::Io(error)
            if matches!(
                error.kind(),
                io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused
            )
    )
}

fn wait_for_socket_removal(deadline: Instant) -> Result<(), ClientError> {
    wait_for_path_removal(&socket_path(), deadline)
}

fn wait_for_path_removal(path: &Path, deadline: Instant) -> Result<(), ClientError> {
    while Instant::now() < deadline {
        if !path.exists() {
            return Ok(());
        }
        thread::sleep(Duration::from_millis(5));
    }
    Err(ClientError::Deadline)
}

fn send_control(action: ControlAction, timeout: Duration) -> Result<ControlResponse, ClientError> {
    send_control_at(&socket_path(), action, timeout)
}

fn send_control_at(
    path: &Path,
    action: ControlAction,
    timeout: Duration,
) -> Result<ControlResponse, ClientError> {
    let mut stream = connect_control_stream(path, timeout)?;
    let request = ControlRequest::new(control_request_id(), action);
    write_frame(&mut stream, &request)?;
    let response: ControlResponse = read_frame(&mut stream)?;
    validate_control_response(&request, response)
}

fn connect_control_stream(path: &Path, timeout: Duration) -> Result<UnixStream, ClientError> {
    let stream = UnixStream::connect(path)?;
    let timeout = timeout.max(Duration::from_millis(50));
    stream.set_read_timeout(Some(timeout))?;
    stream.set_write_timeout(Some(timeout))?;
    Ok(stream)
}

fn validate_control_response(
    request: &ControlRequest,
    response: ControlResponse,
) -> Result<ControlResponse, ClientError> {
    if response.control_protocol != manuvra_protocol::CONTROL_PROTOCOL
        || response.request_id != request.request_id
    {
        return Err(ClientError::Launch(
            "invalid daemon control response envelope".to_owned(),
        ));
    }
    Ok(response)
}

fn control_request_id() -> String {
    format!("control-{}-{}", std::process::id(), monotonic_nonce())
}

fn monotonic_nonce() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos()
}

fn remaining_request_time(deadline: Instant) -> Result<Duration, ClientError> {
    let remaining = deadline.saturating_duration_since(Instant::now());
    if remaining < Duration::from_millis(50) + RESPONSE_WRITE_RESERVE {
        return Err(ClientError::Deadline);
    }
    Ok(remaining)
}

fn connect_or_launch(deadline: Instant) -> Result<(UnixStream, Option<Child>), ClientError> {
    match UnixStream::connect(socket_path()) {
        Ok(stream) => Ok((stream, None)),
        Err(error) => launch_after_connect_error(error, deadline),
    }
}

fn launch_after_connect_error(
    error: io::Error,
    deadline: Instant,
) -> Result<(UnixStream, Option<Child>), ClientError> {
    if connect_error_means_absent(&error) {
        launch_and_connect(deadline)
    } else {
        Err(ClientError::Io(error))
    }
}

fn connect_error_means_absent(error: &io::Error) -> bool {
    matches!(
        error.kind(),
        io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused
    )
}

fn launch_and_connect(deadline: Instant) -> Result<(UnixStream, Option<Child>), ClientError> {
    let child = launch_daemon()?;
    let stream = connect_until(deadline)?;
    Ok((stream, Some(child)))
}

fn connect_until(deadline: Instant) -> Result<UnixStream, ClientError> {
    connect_until_path(&socket_path(), deadline)
}

fn connect_until_path(path: &Path, deadline: Instant) -> Result<UnixStream, ClientError> {
    let mut last = None;
    while Instant::now() < deadline {
        match UnixStream::connect(path) {
            Ok(stream) => return Ok(stream),
            Err(error) => last = Some(error),
        }
        thread::sleep(Duration::from_millis(5));
    }
    Err(ClientError::Io(last.unwrap_or_else(|| {
        io::Error::new(io::ErrorKind::TimedOut, "daemon launch timed out")
    })))
}

fn launch_daemon() -> Result<Child, ClientError> {
    deny_disabled_autostart()?;
    spawn_installed_daemon(&Installation::current()?)
}

fn deny_disabled_autostart() -> Result<(), ClientError> {
    if std::env::var_os("MANUVRA_NO_AUTOSTART").is_some() {
        Err(ClientError::Launch(
            "daemon autostart is disabled".to_owned(),
        ))
    } else {
        Ok(())
    }
}

fn spawn_installed_daemon(installation: &Installation) -> Result<Child, ClientError> {
    daemon_launch_command(installation)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(ClientError::Io)
}

fn daemon_launch_command(installation: &Installation) -> Command {
    #[cfg(target_os = "macos")]
    if let Some(bundle) = &installation.bundle {
        let mut command = Command::new("/usr/bin/open");
        command.args(["-g", "-n", "-a"]).arg(bundle);
        return command;
    }

    Command::new(&installation.daemon)
}

pub struct DaemonSocket {
    listener: UnixListener,
    _lock: File,
    socket_path: PathBuf,
    socket_device: u64,
    socket_inode: u64,
}

impl DaemonSocket {
    pub fn bind() -> Result<Self, io::Error> {
        Self::bind_at(runtime_root())
    }

    fn bind_at(root: PathBuf) -> Result<Self, io::Error> {
        ensure_runtime_root(&root)?;
        let lock = exclusive_daemon_lock(&root)?;
        let bound = bind_private_socket(&root)?;
        Ok(Self {
            listener: bound.listener,
            _lock: lock,
            socket_path: bound.socket_path,
            socket_device: bound.device,
            socket_inode: bound.inode,
        })
    }

    pub fn incoming(&self) -> Incoming<'_> {
        self.listener.incoming()
    }

    pub fn try_clone_listener(&self) -> io::Result<UnixListener> {
        self.listener.try_clone()
    }

    pub fn verify_peer(stream: &UnixStream) -> io::Result<()> {
        verify_peer(stream)
    }
}

impl Drop for DaemonSocket {
    fn drop(&mut self) {
        if socket_identity_matches(&self.socket_path, self.socket_device, self.socket_inode) {
            let _ = fs::remove_file(&self.socket_path);
        }
    }
}

fn exclusive_daemon_lock(root: &Path) -> io::Result<File> {
    let lock = open_daemon_lock(&root.join("daemon.lock"))?;
    acquire_lock(&lock)?;
    Ok(lock)
}

struct BoundDaemonSocket {
    listener: UnixListener,
    socket_path: PathBuf,
    device: u64,
    inode: u64,
}

fn bind_private_socket(root: &Path) -> io::Result<BoundDaemonSocket> {
    let socket_path = root.join("daemon.sock");
    remove_stale_socket(&socket_path)?;
    let listener = UnixListener::bind(&socket_path)?;
    fs::set_permissions(&socket_path, fs::Permissions::from_mode(0o600))?;
    let metadata = fs::symlink_metadata(&socket_path)?;
    validate_socket_metadata(&metadata)?;
    Ok(BoundDaemonSocket {
        listener,
        socket_path,
        device: metadata.dev(),
        inode: metadata.ino(),
    })
}

fn ensure_runtime_root(root: &Path) -> io::Result<()> {
    if root.exists() {
        validate_existing_runtime_root(root)?;
    } else {
        fs::create_dir_all(root)?;
    }
    fs::set_permissions(root, fs::Permissions::from_mode(0o700))?;
    Ok(())
}

fn validate_existing_runtime_root(root: &Path) -> io::Result<()> {
    let metadata = fs::symlink_metadata(root)?;
    if !is_real_directory(&metadata) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "runtime root is unsafe",
        ));
    }
    reject_foreign_uid(metadata.uid(), "runtime root belongs to a different user")
}

fn is_real_directory(metadata: &fs::Metadata) -> bool {
    metadata.file_type().is_dir() && !metadata.file_type().is_symlink()
}

fn reject_foreign_uid(uid: u32, message: &str) -> io::Result<()> {
    if uid == unsafe { libc::geteuid() } {
        Ok(())
    } else {
        Err(io::Error::new(io::ErrorKind::PermissionDenied, message))
    }
}

fn open_daemon_lock(path: &Path) -> io::Result<File> {
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)?;
    let metadata = file.metadata()?;
    if !metadata.file_type().is_file() || metadata.uid() != unsafe { libc::geteuid() } {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "daemon lock is not a same-user regular file",
        ));
    }
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    Ok(file)
}

fn acquire_lock(file: &File) -> io::Result<()> {
    let result = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if result == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

fn remove_stale_socket(path: &Path) -> io::Result<()> {
    if !path.exists() {
        return Ok(());
    }
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.file_type().is_socket() || metadata.file_type().is_symlink() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "refusing to replace non-socket daemon path",
        ));
    }
    fs::remove_file(path)
}

fn validate_socket_metadata(metadata: &fs::Metadata) -> io::Result<()> {
    if !metadata.file_type().is_socket()
        || metadata.uid() != unsafe { libc::geteuid() }
        || metadata.permissions().mode() & 0o777 != 0o600
    {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "daemon socket ownership or mode is unsafe",
        ));
    }
    Ok(())
}

fn socket_identity_matches(path: &Path, device: u64, inode: u64) -> bool {
    fs::symlink_metadata(path).is_ok_and(|metadata| {
        metadata.file_type().is_socket() && metadata.dev() == device && metadata.ino() == inode
    })
}

#[cfg(target_os = "macos")]
fn verify_peer(stream: &UnixStream) -> io::Result<()> {
    let mut uid: libc::uid_t = 0;
    let mut gid: libc::gid_t = 0;
    let result = unsafe { libc::getpeereid(stream.as_raw_fd(), &mut uid, &mut gid) };
    if result != 0 {
        return Err(io::Error::last_os_error());
    }
    if uid != unsafe { libc::geteuid() } {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "daemon peer UID differs",
        ));
    }
    Ok(())
}

#[cfg(not(target_os = "macos"))]
fn verify_peer(_stream: &UnixStream) -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "peer credentials require macOS",
    ))
}

#[derive(Debug, thiserror::Error)]
pub enum ClientError {
    #[error("daemon I/O: {0}")]
    Io(#[from] io::Error),
    #[error("daemon protocol: {0}")]
    Protocol(#[from] ProtocolError),
    #[error("daemon launch: {0}")]
    Launch(String),
    #[error("daemon control rejected the request: {0:?}")]
    Control(Box<manuvra_protocol::OperationalError>, Value),
    #[error("installation: {0}")]
    Installation(#[from] InstallationError),
    #[error("request deadline expired before invocation could be sent")]
    Deadline,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;

    #[test]
    fn installed_daemon_launches_through_its_bundle_identity() {
        let installation = Installation {
            executable: PathBuf::from("/opt/homebrew/bin/manuvra"),
            daemon: PathBuf::from(
                "/opt/homebrew/Cellar/manuvra/9.8.7/libexec/Manuvra.app/Contents/MacOS/manuvra-daemon",
            ),
            bundle: Some(PathBuf::from(
                "/opt/homebrew/Cellar/manuvra/9.8.7/libexec/Manuvra.app",
            )),
            resources: PathBuf::from(
                "/opt/homebrew/Cellar/manuvra/9.8.7/libexec/Manuvra.app/Contents/Resources",
            ),
            installed: true,
        };
        let command = daemon_launch_command(&installation);

        #[cfg(target_os = "macos")]
        {
            assert_eq!(command.get_program(), "/usr/bin/open");
            assert_eq!(
                command.get_args().collect::<Vec<_>>(),
                [
                    "-g",
                    "-n",
                    "-a",
                    "/opt/homebrew/Cellar/manuvra/9.8.7/libexec/Manuvra.app"
                ]
            );
        }

        #[cfg(not(target_os = "macos"))]
        assert_eq!(command.get_program(), installation.daemon);
    }

    #[test]
    fn development_daemon_launches_directly() {
        let installation = Installation {
            executable: PathBuf::from("/work/target/debug/manuvra"),
            daemon: PathBuf::from("/work/target/debug/manuvra-daemon"),
            bundle: None,
            resources: PathBuf::from("/work/resources"),
            installed: false,
        };
        let command = daemon_launch_command(&installation);

        assert_eq!(command.get_program(), installation.daemon);
        assert_eq!(command.get_args().count(), 0);
    }

    #[test]
    fn daemon_socket_owns_private_paths_and_excludes_a_second_daemon() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("runtime");
        let socket = DaemonSocket::bind_at(root.clone()).unwrap();
        assert_eq!(
            fs::metadata(&root).unwrap().permissions().mode() & 0o777,
            0o700
        );
        assert_eq!(
            fs::metadata(root.join("daemon.sock"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        assert!(DaemonSocket::bind_at(root.clone()).is_err());
        drop(socket);
        assert!(!root.join("daemon.sock").exists());
    }

    #[test]
    fn runtime_root_and_stale_socket_checks_fail_closed() {
        let temp = tempfile::tempdir().unwrap();
        let unsafe_root = temp.path().join("file");
        fs::write(&unsafe_root, b"not a directory").unwrap();
        assert!(ensure_runtime_root(&unsafe_root).is_err());

        let stale = temp.path().join("not-a-socket");
        fs::write(&stale, b"keep me").unwrap();
        assert!(remove_stale_socket(&stale).is_err());
        assert_eq!(fs::read(&stale).unwrap(), b"keep me");

        let socket_path = temp.path().join("stale.sock");
        let listener = UnixListener::bind(&socket_path).unwrap();
        drop(listener);
        remove_stale_socket(&socket_path).unwrap();
        assert!(!socket_path.exists());
    }

    #[test]
    fn daemon_lock_rejects_symlinks_and_drop_preserves_replacements() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("runtime");
        ensure_runtime_root(&root).unwrap();
        let outside = temp.path().join("outside-lock");
        fs::write(&outside, b"do not touch").unwrap();
        symlink(&outside, root.join("daemon.lock")).unwrap();
        assert!(DaemonSocket::bind_at(root.clone()).is_err());
        assert_eq!(fs::read(&outside).unwrap(), b"do not touch");

        fs::remove_file(root.join("daemon.lock")).unwrap();
        let socket = DaemonSocket::bind_at(root.clone()).unwrap();
        fs::remove_file(root.join("daemon.sock")).unwrap();
        fs::write(root.join("daemon.sock"), b"replacement").unwrap();
        drop(socket);
        assert_eq!(fs::read(root.join("daemon.sock")).unwrap(), b"replacement");
    }

    #[test]
    fn connection_retry_supports_success_and_bounded_timeout() {
        let temp = tempfile::tempdir().unwrap();
        let socket_path = temp.path().join("eventual.sock");
        let listener = UnixListener::bind(&socket_path).unwrap();
        let stream = connect_until_path(&socket_path, Instant::now() + Duration::from_millis(50));
        assert!(stream.is_ok());
        drop(listener);

        let missing = temp.path().join("missing.sock");
        let error = connect_until_path(&missing, Instant::now() + Duration::from_millis(10));
        assert!(error.is_err());
    }

    #[test]
    fn connect_absence_and_socket_removal_wait_are_bounded() {
        assert!(connect_error_means_absent(&io::Error::from(
            io::ErrorKind::NotFound
        )));
        assert!(connect_error_means_absent(&io::Error::from(
            io::ErrorKind::ConnectionRefused
        )));
        assert!(!connect_error_means_absent(&io::Error::from(
            io::ErrorKind::PermissionDenied
        )));

        let denied = launch_after_connect_error(
            io::Error::from(io::ErrorKind::PermissionDenied),
            Instant::now() + Duration::from_millis(20),
        );
        assert!(matches!(denied, Err(ClientError::Io(_))));

        let temp = tempfile::tempdir().unwrap();
        let gone = temp.path().join("already-gone.sock");
        wait_for_path_removal(&gone, Instant::now() + Duration::from_millis(20)).unwrap();

        let lingering = temp.path().join("lingering.sock");
        fs::write(&lingering, b"keep").unwrap();
        let timeout = wait_for_path_removal(&lingering, Instant::now() + Duration::from_millis(15));
        assert!(matches!(timeout, Err(ClientError::Deadline)));
        assert!(lingering.exists());
    }

    #[test]
    fn control_exchange_accepts_matching_envelopes_and_rejects_mismatches() {
        let temp = tempfile::tempdir().unwrap();
        let socket_path = temp.path().join("control.sock");
        let listener = UnixListener::bind(&socket_path).unwrap();
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let request: ControlRequest = read_frame(&mut stream).unwrap();
            write_frame(
                &mut stream,
                &ControlResponse {
                    control_protocol: manuvra_protocol::CONTROL_PROTOCOL,
                    request_id: request.request_id,
                    ok: true,
                    daemon: serde_json::json!({"running": true}),
                    error: None,
                },
            )
            .unwrap();
        });
        let status =
            send_control_at(&socket_path, ControlAction::Status, Duration::from_secs(1)).unwrap();
        assert!(status.ok);
        assert_eq!(status.daemon["running"], true);
        server.join().unwrap();

        let mismatched = temp.path().join("mismatch.sock");
        let listener = UnixListener::bind(&mismatched).unwrap();
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let request: ControlRequest = read_frame(&mut stream).unwrap();
            write_frame(
                &mut stream,
                &ControlResponse {
                    control_protocol: 99,
                    request_id: request.request_id,
                    ok: true,
                    daemon: serde_json::json!({}),
                    error: None,
                },
            )
            .unwrap();
        });
        let error = send_control_at(&mismatched, ControlAction::Stop, Duration::from_secs(1));
        assert!(
            matches!(error, Err(ClientError::Launch(message)) if message.contains("control response envelope"))
        );
        server.join().unwrap();
    }

    #[test]
    fn runtime_root_rejects_symlinks_and_accepts_an_owned_directory() {
        let temp = tempfile::tempdir().unwrap();
        let created = temp.path().join("created");
        ensure_runtime_root(&created).unwrap();
        ensure_runtime_root(&created).unwrap();
        assert_eq!(
            fs::metadata(&created).unwrap().permissions().mode() & 0o777,
            0o700
        );

        let linked = temp.path().join("linked-root");
        symlink(&created, &linked).unwrap();
        assert!(ensure_runtime_root(&linked).is_err());
    }

    #[test]
    fn runtime_budget_rejects_too_small_remaining_time() {
        assert!(runtime_deadline_ms(RESPONSE_WRITE_RESERVE + Duration::from_millis(50)).is_ok());
        assert!(matches!(
            runtime_deadline_ms(Duration::from_millis(10)),
            Err(ClientError::Deadline)
        ));
    }
}
