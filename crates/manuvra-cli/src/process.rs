use crate::store::{self, ProcessIdentity, RequestIntent};
use manuvra_contract::Job;
use serde::{Deserialize, Serialize};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
#[cfg(target_os = "linux")]
use std::process::{Command, Stdio};
use std::time::{SystemTime, UNIX_EPOCH};

#[path = "process/linux.rs"]
mod platform;

pub use platform::signal_process_group;
pub use platform::{child_exited_without_reaping, process_identity, process_is_same};

pub const IPC_VERSION: u16 = 1;

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HostBootstrap {
    pub job: Job,
    pub intent: RequestIntent,
    pub lookup_request_id: String,
    pub state_root: PathBuf,
    pub browser: Option<PathBuf>,
    pub headless: bool,
    pub provider_key: Option<String>,
    pub runtime_dir: PathBuf,
    pub started_unix_ms: u64,
    pub lifetime_deadline_unix_ms: u64,
    pub pause_timeout_ms: u64,
    pub watchdog: Option<ProcessIdentity>,
    pub liveness_fd: Option<i32>,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WatchdogBootstrap {
    pub intent: RequestIntent,
    pub state_root: PathBuf,
    pub runtime_dir: PathBuf,
    pub started_unix_ms: u64,
    pub lifetime_deadline_unix_ms: u64,
    pub initial_result: serde_json::Value,
    pub watchdog: Option<ProcessIdentity>,
}

struct SensitiveBytes(Vec<u8>);

impl Drop for SensitiveBytes {
    fn drop(&mut self) {
        self.0.fill(0);
    }
}

pub fn now_unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}

pub fn runtime_run_dir(run_id: &str) -> Result<PathBuf, String> {
    let root = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .ok_or_else(|| "XDG_RUNTIME_DIR is required for background runs".to_owned())?;
    let manuvra = root.join("manuvra");
    store::create_private_dir(&manuvra)?;
    let root = manuvra.join("runs");
    store::create_private_dir(&root)?;
    let directory = root.join(run_id);
    store::create_private_dir(&directory)?;
    Ok(directory)
}

#[cfg(target_os = "linux")]
pub fn spawn_watchdog(
    mut host: HostBootstrap,
    mut watchdog: WatchdogBootstrap,
    run_lock: &store::RunLock,
) -> Result<ProcessIdentity, String> {
    use std::os::unix::process::CommandExt;

    let executable = std::env::current_exe().map_err(|error| error.to_string())?;
    let mut command = Command::new(executable);
    command
        .arg("__watchdog")
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .env_remove("TYPESAFE_API_KEY")
        .env_remove("MANUVRA_TEST_CALLER_BOOTSTRAP_FAULT");
    let run_lock_fd = run_lock.inherited_fd()?;
    command.env("MANUVRA_RUN_LOCK_FD", run_lock_fd.to_string());
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let spawned = command.spawn();
    let restored = run_lock.restore_cloexec();
    let mut child = spawned.map_err(|error| format!("cannot start watchdog: {error}"))?;
    restored?;
    let identity = process_identity(child.id())?;
    host.watchdog = Some(identity.clone());
    watchdog.watchdog = Some(identity.clone());
    send_watchdog_bootstrap(&mut child, &watchdog, &host)?;
    Ok(identity)
}

#[cfg(target_os = "linux")]
fn send_watchdog_bootstrap(
    child: &mut std::process::Child,
    watchdog: &WatchdogBootstrap,
    host: &HostBootstrap,
) -> Result<(), String> {
    let watchdog_bytes = serde_json::to_vec(watchdog).map_err(|error| error.to_string())?;
    let host_bytes = SensitiveBytes(serde_json::to_vec(host).map_err(|error| error.to_string())?);
    let mut stdin = child
        .stdin
        .take()
        .ok_or_else(|| "watchdog bootstrap pipe was unavailable".to_owned())?;
    write_framed(&mut stdin, &watchdog_bytes)
        .and_then(|()| write_framed(&mut stdin, &host_bytes.0))
        .map_err(|error| format!("cannot write watchdog bootstrap: {error}"))
}

#[cfg(not(target_os = "linux"))]
pub fn spawn_watchdog(
    _host: HostBootstrap,
    _watchdog: WatchdogBootstrap,
) -> Result<ProcessIdentity, String> {
    Err("unsupported_platform".into())
}

pub fn write_framed(mut writer: impl Write, bytes: &[u8]) -> Result<(), std::io::Error> {
    let length: u64 = bytes
        .len()
        .try_into()
        .map_err(|_| std::io::Error::other("bootstrap frame is too large"))?;
    writer.write_all(&length.to_le_bytes())?;
    writer.write_all(bytes)
}

pub fn read_framed(mut reader: impl Read) -> Result<Vec<u8>, String> {
    let mut encoded = [0_u8; 8];
    reader
        .read_exact(&mut encoded)
        .map_err(|error| format!("invalid inherited bootstrap frame: {error}"))?;
    let length: usize = u64::from_le_bytes(encoded)
        .try_into()
        .map_err(|_| "inherited bootstrap frame is too large".to_owned())?;
    if length > 32 * 1024 * 1024 {
        return Err("inherited bootstrap frame exceeds the safety bound".into());
    }
    let mut bytes = vec![0; length];
    reader
        .read_exact(&mut bytes)
        .map_err(|error| format!("invalid inherited bootstrap frame: {error}"))?;
    Ok(bytes)
}

pub fn read_bootstrap<T: for<'de> Deserialize<'de>>() -> Result<T, String> {
    serde_json::from_reader(std::io::stdin().lock())
        .map_err(|error| format!("invalid inherited bootstrap: {error}"))
}

pub fn ensure_socket_parent(path: &Path) -> Result<(), String> {
    let parent = path
        .parent()
        .ok_or_else(|| "socket path has no parent".to_owned())?;
    store::create_private_dir(parent)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::process::Stdio;
    use std::time::{Duration, Instant};
    use tempfile::TempDir;

    #[test]
    #[cfg(target_os = "linux")]
    fn process_identity_rejects_pid_reuse_shape() {
        use std::os::unix::process::CommandExt;

        let mut command = Command::new("sh");
        command
            .args(["-c", "while :; do sleep 1; done"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        unsafe {
            command.pre_exec(|| {
                if libc::setpgid(0, 0) == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let mut child = command.spawn().unwrap();
        let current = process_identity(child.id()).unwrap();
        assert!(process_is_same(&current));
        let reused = ProcessIdentity {
            start_marker: current.start_marker.saturating_add(1),
            ..current.clone()
        };
        assert!(!process_is_same(&reused));
        assert!(!signal_process_group(&reused, libc::SIGTERM).unwrap());
        std::thread::sleep(Duration::from_millis(50));
        assert!(child.try_wait().unwrap().is_none());
        assert!(signal_process_group(&current, libc::SIGKILL).unwrap());
        child.wait().unwrap();
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn owned_process_group_signal_cleans_resistant_descendant_after_leader_is_reaped() {
        use std::os::unix::process::CommandExt;

        let temporary = TempDir::new().unwrap();
        let child_pid_path = temporary.path().join("child.pid");
        let script = format!(
            "sh -c 'trap \"\" TERM HUP; while :; do sleep 1; done' & child=$!; printf '%s' \"$child\" > '{}'; exit 0",
            child_pid_path.display()
        );
        let mut command = Command::new("sh");
        command
            .args(["-c", &script])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        unsafe {
            command.pre_exec(|| {
                if libc::setpgid(0, 0) == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let mut child = command.spawn().unwrap();
        let identity = process_identity(child.id()).unwrap();
        let deadline = Instant::now() + Duration::from_secs(1);
        while !child_pid_path.exists() {
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(5));
        }
        let descendant: u32 = fs::read_to_string(&child_pid_path)
            .unwrap()
            .parse()
            .unwrap();
        child.wait().unwrap();
        assert!(!process_is_same(&identity));
        assert_eq!(unsafe { libc::kill(descendant as i32, libc::SIGTERM) }, 0);
        std::thread::sleep(Duration::from_millis(20));
        assert_eq!(unsafe { libc::kill(descendant as i32, 0) }, 0);
        assert!(signal_process_group(&identity, libc::SIGKILL).unwrap());
        let deadline = Instant::now() + Duration::from_secs(1);
        while unsafe { libc::kill(descendant as i32, 0) } == 0 {
            assert!(
                Instant::now() < deadline,
                "descendant survived group cleanup"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
    }
}
