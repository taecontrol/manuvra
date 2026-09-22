#![cfg(target_os = "linux")]

use crate::process::{self, WatchdogBootstrap, now_unix_ms};
use crate::store::{self, RunControl};
use serde_json::{Value, json};
use std::fs;
use std::io::Write;
use std::os::fd::{FromRawFd, RawFd};
use std::os::unix::fs::FileTypeExt;
use std::os::unix::process::CommandExt;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

const SHUTDOWN_GRACE: Duration = Duration::from_secs(10);

pub fn main() -> Result<(), String> {
    let (bootstrap, mut host_bytes, run_lock) = read_inherited_bootstrap()?;
    run_watchdog(&bootstrap, &mut host_bytes, run_lock)
}

fn read_inherited_bootstrap() -> Result<(WatchdogBootstrap, Vec<u8>, store::RunLock), String> {
    let input_fd = unsafe { libc::dup(libc::STDIN_FILENO) };
    if input_fd == -1 {
        return Err(format!(
            "cannot duplicate inherited bootstrap pipe: {}",
            std::io::Error::last_os_error()
        ));
    }
    let mut input = unsafe { fs::File::from_raw_fd(input_fd) };
    let watchdog_bytes = process::read_framed(&mut input)?;
    let bootstrap: WatchdogBootstrap = serde_json::from_slice(&watchdog_bytes)
        .map_err(|error| format!("invalid inherited watchdog bootstrap: {error}"))?;
    let host_bytes = process::read_framed(&mut input)?;
    drop(input);
    let run_lock = store::adopt_inherited_run_lock(
        &bootstrap.state_root,
        &bootstrap.intent.run_id,
        inherited_run_lock_fd()?,
    )?;
    Ok((bootstrap, host_bytes, run_lock))
}

fn run_watchdog(
    bootstrap: &WatchdogBootstrap,
    host_bytes: &mut [u8],
    run_lock: store::RunLock,
) -> Result<(), String> {
    let (read_fd, write_fd) = liveness_pipe()?;
    let mut child = spawn_host(read_fd, &run_lock)?;
    drop(run_lock);
    let host = send_host_bootstrap(&mut child, host_bytes);
    host_bytes.fill(0);
    let outcome = host.and_then(|host| supervise(&mut child, &host, bootstrap));
    close_fd(write_fd);
    outcome
}

fn inherited_run_lock_fd() -> Result<RawFd, String> {
    std::env::var("MANUVRA_RUN_LOCK_FD")
        .map_err(|_| "watchdog inherited no run lock descriptor".to_owned())?
        .parse()
        .map_err(|_| "watchdog inherited an invalid run lock descriptor".to_owned())
}

fn spawn_host(read_fd: RawFd, run_lock: &store::RunLock) -> Result<Child, String> {
    let executable = std::env::current_exe().map_err(|error| error.to_string())?;
    let mut command = Command::new(executable);
    command
        .arg("__host")
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .env("MANUVRA_LIVENESS_FD", read_fd.to_string())
        .env_remove("TYPESAFE_API_KEY");
    let run_lock_fd = run_lock.inherited_fd()?;
    command.env("MANUVRA_RUN_LOCK_FD", run_lock_fd.to_string());
    unsafe {
        command.pre_exec(move || {
            if libc::setpgid(0, 0) == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let spawned = command.spawn();
    let restored = run_lock.restore_cloexec();
    let child = spawned.map_err(|error| format!("cannot start run host: {error}"))?;
    restored?;
    close_fd(read_fd);
    Ok(child)
}

fn send_host_bootstrap(
    child: &mut Child,
    host_bytes: &[u8],
) -> Result<store::ProcessIdentity, String> {
    let host = process::process_identity(child.id())?;
    child
        .stdin
        .take()
        .ok_or_else(|| "host bootstrap pipe was unavailable".to_owned())?
        .write_all(host_bytes)
        .map_err(|error| format!("cannot write host bootstrap: {error}"))?;
    Ok(host)
}

fn supervise(
    child: &mut Child,
    host: &store::ProcessIdentity,
    bootstrap: &WatchdogBootstrap,
) -> Result<(), String> {
    loop {
        if child_exited_without_reaping(child)? {
            let outcome = reconcile_dead_host(bootstrap, "host_lost", None, Some(host));
            reap_exited(child)?;
            return outcome;
        }
        let control = store::read_run_control(&bootstrap.state_root, &bootstrap.intent.run_id)?;
        if control
            .as_ref()
            .is_some_and(|run| run.result.get("terminal").and_then(Value::as_bool) == Some(true))
        {
            return finish_terminal_host(child, host, bootstrap);
        }
        let now = now_unix_ms();
        let deadline = control.as_ref().and_then(expired_deadline).or_else(|| {
            (now >= bootstrap.lifetime_deadline_unix_ms)
                .then_some(("lifetime_elapsed", bootstrap.lifetime_deadline_unix_ms))
        });
        if let Some((reason, _)) = deadline {
            return expire_hung_host(child, host, bootstrap, reason);
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn expired_deadline(control: &RunControl) -> Option<(&'static str, u64)> {
    let now = now_unix_ms();
    if now >= control.lifetime_deadline_unix_ms {
        return Some(("lifetime_elapsed", control.lifetime_deadline_unix_ms));
    }
    control
        .pause_deadline_unix_ms
        .filter(|deadline| now >= *deadline)
        .map(|deadline| ("resume_deadline_elapsed", deadline))
}

fn expire_hung_host(
    child: &mut Child,
    host: &store::ProcessIdentity,
    bootstrap: &WatchdogBootstrap,
    reason: &'static str,
) -> Result<(), String> {
    if let Some(control) = store::read_run_control(&bootstrap.state_root, &bootstrap.intent.run_id)?
    {
        let _ = crate::client::request_host_deadline(&control);
    }
    terminate_owned_group(child, host, shutdown_grace())?;
    reconcile_dead_host(bootstrap, reason, Some("expired"), Some(host))
}

fn shutdown_grace() -> Duration {
    #[cfg(debug_assertions)]
    if let Some(milliseconds) = std::env::var("MANUVRA_TEST_SHUTDOWN_GRACE_MS")
        .ok()
        .and_then(|value| value.parse().ok())
    {
        return Duration::from_millis(milliseconds);
    }
    SHUTDOWN_GRACE
}

fn finish_terminal_host(
    child: &mut Child,
    host: &store::ProcessIdentity,
    bootstrap: &WatchdogBootstrap,
) -> Result<(), String> {
    terminate_owned_group(child, host, shutdown_grace())?;
    remove_dead_socket(&bootstrap.runtime_dir.join("control.sock"))
}

fn terminate_owned_group(
    child: &mut Child,
    host: &store::ProcessIdentity,
    grace: Duration,
) -> Result<(), String> {
    request_graceful_group_exit(child, host, grace)?;
    kill_group_and_reap(child, host)
}

fn request_graceful_group_exit(
    child: &mut Child,
    host: &store::ProcessIdentity,
    grace: Duration,
) -> Result<(), String> {
    if wait_for_exit(child, grace)? {
        return Ok(());
    }
    let _ = process::signal_process_group(host, libc::SIGTERM)?;
    let _ = wait_for_exit(child, Duration::from_secs(2))?;
    Ok(())
}

fn kill_group_and_reap(child: &mut Child, host: &store::ProcessIdentity) -> Result<(), String> {
    let _ = process::signal_process_group(host, libc::SIGKILL)?;
    if !host_exited_after_kill(child)? {
        return Err("run host did not exit after the bounded shutdown sequence".into());
    }
    reap_exited(child)
}

fn host_exited_after_kill(child: &mut Child) -> Result<bool, String> {
    Ok(child_exited_without_reaping(child)? || wait_for_exit(child, Duration::from_secs(2))?)
}

fn wait_for_exit(child: &mut Child, duration: Duration) -> Result<bool, String> {
    let deadline = Instant::now() + duration;
    while Instant::now() < deadline {
        if child_exited_without_reaping(child)? {
            return Ok(true);
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    Ok(false)
}

fn reap_exited(child: &mut Child) -> Result<(), String> {
    child
        .try_wait()
        .map_err(|error| format!("cannot reap run host: {error}"))?
        .is_some()
        .then_some(())
        .ok_or_else(|| "run host was not ready for nonblocking reap".into())
}

fn reconcile_dead_host(
    bootstrap: &WatchdogBootstrap,
    reason: &'static str,
    state: Option<&'static str>,
    host: Option<&store::ProcessIdentity>,
) -> Result<(), String> {
    let socket = bootstrap.runtime_dir.join("control.sock");
    let _publication_lock = wait_for_publication_lock(bootstrap)?;
    if let Some(host) = host {
        let _ = process::signal_process_group(host, libc::SIGKILL)?;
    }
    remove_dead_socket(&socket)?;
    let mut control = store::read_run_control(&bootstrap.state_root, &bootstrap.intent.run_id)?
        .unwrap_or_else(|| lost_control(bootstrap));
    if control.result.get("terminal").and_then(Value::as_bool) == Some(true) {
        return Ok(());
    }
    control.sequence = control.sequence.saturating_add(1);
    control.pause_deadline_unix_ms = None;
    let cleanup = json!({
        "browser":"closed_by_watchdog",
        "profile":"removal_unconfirmed",
        "application_state":"caller_owned"
    });
    control.result["state"] = json!(state.unwrap_or("blocked"));
    control.result["terminal"] = json!(true);
    control.result["reason"] = crash_reason(bootstrap, reason);
    control.result["cleanup"] = cleanup.clone();
    control.result["evidence"]["complete"] = json!(true);
    if manuvra_flow::evidence::replace_result_cleanup_from_checkpoint(
        &bootstrap.intent.evidence_root,
        &bootstrap.intent.run_id,
        &cleanup,
        &control.result,
    )
    .is_err()
    {
        control.result["evidence"]["complete"] = json!(false);
    }
    store::write_run_control(&bootstrap.state_root, &control)
}

fn wait_for_publication_lock(bootstrap: &WatchdogBootstrap) -> Result<store::RunLock, String> {
    let deadline = Instant::now() + Duration::from_secs(1);
    loop {
        if let Some(lock) =
            store::try_lock_existing_run(&bootstrap.state_root, &bootstrap.intent.run_id)?
        {
            return Ok(lock);
        }
        if Instant::now() >= deadline {
            return Err("host death was not confirmed by the exclusive run lock".into());
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn child_exited_without_reaping(child: &Child) -> Result<bool, String> {
    process::child_exited_without_reaping(child.id())
}

fn lost_control(bootstrap: &WatchdogBootstrap) -> RunControl {
    RunControl {
        schema_version: manuvra_contract::SchemaVersion,
        ipc_version: process::IPC_VERSION,
        sequence: 0,
        run_id: bootstrap.intent.run_id.clone(),
        request_id: bootstrap.intent.public_request_id.clone(),
        job_digest: bootstrap.intent.job_digest.clone(),
        evidence_root: bootstrap.intent.evidence_root.clone(),
        started_unix_ms: bootstrap.started_unix_ms,
        lifetime_deadline_unix_ms: bootstrap.lifetime_deadline_unix_ms,
        pause_deadline_unix_ms: None,
        host: None,
        watchdog: bootstrap.watchdog.clone(),
        socket: bootstrap.runtime_dir.join("control.sock"),
        result: bootstrap.initial_result.clone(),
    }
}

fn crash_reason(bootstrap: &WatchdogBootstrap, code: &str) -> Value {
    let prepared =
        store::unresolved_action(&bootstrap.intent.evidence_root, &bootstrap.intent.run_id)
            .unwrap_or(true);
    json!({"code":code,"current_action":if prepared { "uncertain" } else { "none" }})
}

fn remove_dead_socket(path: &std::path::Path) -> Result<(), String> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_socket() => remove_socket(path),
        Ok(_) => Err("dead host socket path is not an owned socket".into()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.to_string()),
    }
}

fn remove_socket(path: &std::path::Path) -> Result<(), String> {
    fs::remove_file(path).map_err(|error| error.to_string())
}

fn liveness_pipe() -> Result<(RawFd, RawFd), String> {
    let mut descriptors = [-1_i32; 2];
    if unsafe { libc::pipe(descriptors.as_mut_ptr()) } == -1 {
        return Err(std::io::Error::last_os_error().to_string());
    }
    set_cloexec(descriptors[1])?;
    Ok((descriptors[0], descriptors[1]))
}

fn set_cloexec(fd: RawFd) -> Result<(), String> {
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
    if flags == -1 || unsafe { libc::fcntl(fd, libc::F_SETFD, flags | libc::FD_CLOEXEC) } == -1 {
        return Err(std::io::Error::last_os_error().to_string());
    }
    Ok(())
}

fn close_fd(fd: RawFd) {
    unsafe {
        libc::close(fd);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn deadline_classification_prefers_absolute_lifetime() {
        let now = now_unix_ms();
        let control = RunControl {
            schema_version: manuvra_contract::SchemaVersion,
            ipc_version: process::IPC_VERSION,
            sequence: 1,
            run_id: "r".into(),
            request_id: "q".into(),
            job_digest: "d".into(),
            evidence_root: "/tmp/e".into(),
            started_unix_ms: now.saturating_sub(2),
            lifetime_deadline_unix_ms: now.saturating_sub(1),
            pause_deadline_unix_ms: Some(now.saturating_sub(1)),
            host: None,
            watchdog: None,
            socket: "/tmp/s".into(),
            result: json!({}),
        };
        assert_eq!(expired_deadline(&control).unwrap().0, "lifetime_elapsed");
    }

    #[test]
    fn bounded_child_wait_distinguishes_exit_from_hang() {
        let mut exited = Command::new("sh").arg("-c").arg("exit 0").spawn().unwrap();
        assert!(wait_for_exit(&mut exited, Duration::from_secs(1)).unwrap());

        let mut sleeping = Command::new("sh").arg("-c").arg("sleep 1").spawn().unwrap();
        assert!(!wait_for_exit(&mut sleeping, Duration::from_millis(10)).unwrap());
        sleeping.kill().unwrap();
        sleeping.wait().unwrap();
    }

    fn bootstrap(temporary: &TempDir) -> WatchdogBootstrap {
        let state_root = temporary.path().join("state/manuvra");
        let runtime_dir = temporary.path().join("runtime/runs/r_crash");
        store::create_private_dir(&state_root).unwrap();
        store::create_private_dir(&runtime_dir).unwrap();
        WatchdogBootstrap {
            intent: store::RequestIntent {
                schema_version: manuvra_contract::SchemaVersion,
                public_request_id: "request".into(),
                run_id: "r_crash".into(),
                job_digest: "digest".into(),
                evidence_root: temporary.path().join("evidence"),
            },
            state_root,
            runtime_dir,
            started_unix_ms: now_unix_ms(),
            lifetime_deadline_unix_ms: now_unix_ms() + 10_000,
            initial_result: json!({
                "schema_version":1,"request_id":"request","run_id":"r_crash",
                "state":"running","terminal":false,"reason":null,
                "verdict":{"overall":"unresolved","steps":[{"id":"s","result":"unresolved"}],"expectations":[],"caller_assisted":false},
                "evidence":{"manifest":temporary.path().join("evidence/r_crash/manifest.json"),"complete":false},
                "escalation":null,
                "cleanup":{"browser":"not_started","profile":"not_created","application_state":"caller_owned"}
            }),
            watchdog: None,
        }
    }

    fn write_running_control(bootstrap: &WatchdogBootstrap) {
        store::write_run_control(
            &bootstrap.state_root,
            &RunControl {
                schema_version: manuvra_contract::SchemaVersion,
                ipc_version: process::IPC_VERSION,
                sequence: 1,
                run_id: bootstrap.intent.run_id.clone(),
                request_id: bootstrap.intent.public_request_id.clone(),
                job_digest: bootstrap.intent.job_digest.clone(),
                evidence_root: bootstrap.intent.evidence_root.clone(),
                started_unix_ms: bootstrap.started_unix_ms,
                lifetime_deadline_unix_ms: bootstrap.lifetime_deadline_unix_ms,
                pause_deadline_unix_ms: None,
                host: None,
                watchdog: None,
                socket: bootstrap.runtime_dir.join("control.sock"),
                result: json!({
                    "state":"running","terminal":false,
                    "reason":null,"evidence":{"complete":false},
                    "cleanup":{"browser":"alive"}
                }),
            },
        )
        .unwrap();
    }

    #[test]
    fn crash_publication_requires_death_lock_and_classifies_prepared_action() {
        let temporary = TempDir::new().unwrap();
        let bootstrap = bootstrap(&temporary);
        write_running_control(&bootstrap);
        let lock = store::lock_run(&bootstrap.state_root, &bootstrap.intent.run_id).unwrap();
        assert!(reconcile_dead_host(&bootstrap, "host_lost", None, None).is_err());
        drop(lock);

        reconcile_dead_host(&bootstrap, "host_lost", None, None).unwrap();
        let before = store::read_run_control(&bootstrap.state_root, &bootstrap.intent.run_id)
            .unwrap()
            .unwrap();
        assert_eq!(before.result["reason"]["current_action"], "none");
        assert_eq!(before.result["state"], "blocked");
        assert_eq!(before.result["evidence"]["complete"], false);

        write_running_control(&bootstrap);
        store::create_private_dir(&bootstrap.intent.evidence_root).unwrap();
        store::atomic_write_private(
            &bootstrap
                .intent
                .evidence_root
                .join(".r_crash.action-journal.jsonl"),
            b"{\"event\":\"action_prepared\"}\n{\"event\":\"action_fact\",\"fact\":{}}\n",
        )
        .unwrap();
        reconcile_dead_host(&bootstrap, "host_lost", None, None).unwrap();
        let completed_prior =
            store::read_run_control(&bootstrap.state_root, &bootstrap.intent.run_id)
                .unwrap()
                .unwrap();
        assert_eq!(completed_prior.result["reason"]["current_action"], "none");

        write_running_control(&bootstrap);
        let journal = bootstrap
            .intent
            .evidence_root
            .join(".r_crash.action-journal.jsonl");
        store::atomic_write_private(
            &journal,
            b"{\"event\":\"action_prepared\"}\n{\"event\":\"action_fact\",\"fact\":{}}\n{\"event\":\"action_prepared\"}\n",
        )
        .unwrap();
        reconcile_dead_host(&bootstrap, "host_lost", None, None).unwrap();
        let after = store::read_run_control(&bootstrap.state_root, &bootstrap.intent.run_id)
            .unwrap()
            .unwrap();
        assert_eq!(after.result["reason"]["current_action"], "uncertain");
    }
}
