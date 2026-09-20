use crate::Invocation;
#[cfg(target_os = "linux")]
use crate::process::IPC_VERSION;
#[cfg(target_os = "linux")]
use crate::store::{self, RunControl};
#[cfg(target_os = "linux")]
use crate::{
    EXIT_INTERNAL, internal_error, result_exit_code, validate_request_id, validate_run_id,
};
#[cfg(target_os = "linux")]
use manuvra_contract::SchemaVersion;
#[cfg(target_os = "linux")]
use serde_json::{Value, json};
#[cfg(target_os = "linux")]
use std::io::Read;
#[cfg(target_os = "linux")]
use std::path::Path;
#[cfg(target_os = "linux")]
use std::time::{Duration, Instant};

pub fn status(run_id: Option<&str>, request_id: Option<&str>, wait_ms: Option<u64>) -> Invocation {
    try_status(run_id, request_id, wait_ms).unwrap_or_else(|error| error)
}

fn try_status(
    run_id: Option<&str>,
    request_id: Option<&str>,
    wait_ms: Option<u64>,
) -> Result<Invocation, Invocation> {
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (run_id, request_id, wait_ms);
        Err(Invocation::error(
            "unsupported_platform",
            "background runs are supported only on Linux",
            3,
        ))
    }
    #[cfg(target_os = "linux")]
    {
        try_status_linux(run_id, request_id, wait_ms)
    }
}

#[cfg(target_os = "linux")]
fn try_status_linux(
    run_id: Option<&str>,
    request_id: Option<&str>,
    wait_ms: Option<u64>,
) -> Result<Invocation, Invocation> {
    validate_status_selectors(run_id, request_id)?;
    let root = store::state_root().map_err(internal_error)?;
    let run_id = resolve_run_id(&root, run_id, request_id).map_err(internal_error)?;
    validate_run_id(&run_id).map_err(internal_error)?;
    let mut control = wait_for_control(&root, &run_id, wait_ms)?;
    refresh_status_from_host(&mut control);
    Ok(run_invocation(control))
}

#[cfg(target_os = "linux")]
fn validate_status_selectors(
    run_id: Option<&str>,
    request_id: Option<&str>,
) -> Result<(), Invocation> {
    if let Some(run_id) = run_id {
        validate_run_id(run_id)
            .map_err(|message| Invocation::error("invalid_run_id", message, 64))?;
    }
    if let Some(request_id) = request_id {
        validate_request_id(request_id)
            .map_err(|message| Invocation::error("invalid_request_id", message, 64))?;
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn refresh_status_from_host(control: &mut RunControl) {
    if is_terminal(control) {
        return;
    }
    let Some(result) = request_for_control(control, "status")
        .ok()
        .and_then(|response| response.get("result").cloned())
    else {
        return;
    };
    control.result = result;
}

#[cfg(target_os = "linux")]
pub fn wait_for_run(run_id: &str, wait_ms: Option<u64>) -> Invocation {
    let root = match store::state_root() {
        Ok(root) => root,
        Err(error) => return internal_error(error),
    };
    match wait_for_control(&root, run_id, wait_ms) {
        Ok(control) => run_invocation(control),
        Err(error) => error,
    }
}

pub fn abort(run_id: &str, request_id: &str) -> Invocation {
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (run_id, request_id);
        Invocation::error(
            "unsupported_platform",
            "background runs are supported only on Linux",
            3,
        )
    }
    #[cfg(target_os = "linux")]
    {
        abort_linux(run_id, request_id)
    }
}

#[cfg(target_os = "linux")]
fn abort_linux(run_id: &str, request_id: &str) -> Invocation {
    validate_run_id(run_id)
        .map_err(|message| Invocation::error("invalid_run_id", message, 64))
        .and_then(|()| {
            validate_request_id(request_id)
                .map_err(|message| Invocation::error("invalid_request_id", message, 64))
        })
        .and_then(|()| abort_in_state(run_id, request_id))
        .unwrap_or_else(|invocation| invocation)
}

#[cfg(target_os = "linux")]
fn abort_in_state(run_id: &str, request_id: &str) -> Result<Invocation, Invocation> {
    let root = store::state_root().map_err(internal_error)?;
    let _request_lock = store::lock_request(&root, request_id).map_err(internal_error)?;
    let digest = abort_digest(&root, run_id).map_err(internal_error)?;
    match prior_abort(&root, request_id, &digest)? {
        Some(invocation) => Ok(invocation),
        None => abort_new(&root, run_id, request_id, digest),
    }
}

#[cfg(target_os = "linux")]
fn abort_digest(root: &Path, run_id: &str) -> Result<String, String> {
    serde_json::to_vec(&json!({"command":"abort","run_id":run_id}))
        .map_err(|error| error.to_string())
        .and_then(|bytes| store::keyed_digest(root, &bytes))
}

#[cfg(target_os = "linux")]
fn prior_abort(
    root: &Path,
    request_id: &str,
    digest: &str,
) -> Result<Option<Invocation>, Invocation> {
    store::lookup_request(root, request_id)
        .map_err(internal_error)?
        .map_or(Ok(None), |entry| classify_prior_abort(entry, digest))
}

#[cfg(target_os = "linux")]
fn classify_prior_abort(
    entry: store::RequestEntry,
    digest: &str,
) -> Result<Option<Invocation>, Invocation> {
    match entry {
        store::RequestEntry::Complete(record) if record.job_digest == digest => {
            Ok(Some(Invocation {
                output: record.result,
                exit_code: record.exit_code,
            }))
        }
        _ => Err(Invocation::error(
            "request_conflict",
            "request_id was already used for a different request",
            64,
        )),
    }
}

#[cfg(target_os = "linux")]
fn abort_new(
    root: &Path,
    run_id: &str,
    request_id: &str,
    digest: String,
) -> Result<Invocation, Invocation> {
    let control = store::read_run_control(root, run_id)
        .map_err(internal_error)?
        .ok_or_else(|| Invocation::error("run_not_found", "run was not found", 64))?;
    if is_terminal(&control) {
        return Ok(finalize_abort(root, request_id, digest, control));
    }
    request_for_control(&control, "abort").map_err(internal_error)?;
    Ok(await_abort(root, run_id, request_id, digest))
}

#[cfg(target_os = "linux")]
fn await_abort(root: &Path, run_id: &str, request_id: &str, digest: String) -> Invocation {
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        if let Some(invocation) = poll_abort(root, run_id, request_id, &digest, deadline) {
            return invocation;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[cfg(target_os = "linux")]
fn poll_abort(
    root: &Path,
    run_id: &str,
    request_id: &str,
    digest: &str,
    deadline: Instant,
) -> Option<Invocation> {
    match store::read_run_control(root, run_id) {
        Ok(Some(next)) if is_terminal(&next) => {
            Some(finalize_abort(root, request_id, digest.to_owned(), next))
        }
        Ok(Some(next)) if Instant::now() >= deadline => Some(run_invocation(next)),
        Ok(Some(_)) => None,
        Ok(None) => Some(Invocation::error("run_not_found", "run was not found", 64)),
        Err(error) => Some(internal_error(error)),
    }
}

#[cfg(target_os = "linux")]
fn is_terminal(control: &RunControl) -> bool {
    control.result.get("terminal").and_then(Value::as_bool) == Some(true)
}

#[cfg(target_os = "linux")]
fn finalize_abort(
    root: &Path,
    request_id: &str,
    digest: String,
    control: RunControl,
) -> Invocation {
    let invocation = run_invocation(control.clone());
    let record = store::RequestRecord {
        schema_version: SchemaVersion,
        public_request_id: control.request_id,
        run_id: control.run_id,
        job_digest: digest,
        exit_code: invocation.exit_code,
        result: invocation.output.clone(),
    };
    store::finalize_control_request(root, request_id, &record)
        .map(|()| invocation)
        .unwrap_or_else(internal_error)
}

#[cfg(target_os = "linux")]
fn resolve_run_id(
    root: &Path,
    run_id: Option<&str>,
    request_id: Option<&str>,
) -> Result<String, String> {
    match (run_id, request_id) {
        (Some(run_id), None) => Ok(run_id.to_owned()),
        (None, Some(request_id)) => store::request_run_id(root, request_id)?
            .ok_or_else(|| "request was not found".to_owned()),
        _ => Err("exactly one run selector is required".into()),
    }
}

#[cfg(target_os = "linux")]
fn wait_for_control(
    root: &Path,
    run_id: &str,
    wait_ms: Option<u64>,
) -> Result<RunControl, Invocation> {
    let deadline = wait_ms.map(|wait| Instant::now() + Duration::from_millis(wait));
    let initial = read_control(root, run_id)?;
    if initial.is_none() && is_immediate(wait_ms) {
        return Err(Invocation::error("run_not_found", "run was not found", 64));
    }
    let initial_sequence = initial.as_ref().map(|control| control.sequence);
    loop {
        if let Some(outcome) = poll_control(root, run_id, initial_sequence, wait_ms, deadline)? {
            return outcome;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[cfg(target_os = "linux")]
fn poll_control(
    root: &Path,
    run_id: &str,
    initial_sequence: Option<u64>,
    wait_ms: Option<u64>,
    deadline: Option<Instant>,
) -> Result<Option<Result<RunControl, Invocation>>, Invocation> {
    match read_control(root, run_id)? {
        Some(control) if control_ready(&control, initial_sequence, wait_ms, deadline) => {
            Ok(Some(validate_control(control)))
        }
        None if deadline_elapsed(deadline) => Ok(Some(Err(Invocation::error(
            "run_not_found",
            "run was not found",
            64,
        )))),
        _ => Ok(None),
    }
}

#[cfg(target_os = "linux")]
fn read_control(root: &Path, run_id: &str) -> Result<Option<RunControl>, Invocation> {
    let control = store::read_run_control(root, run_id).map_err(internal_error)?;
    control
        .map(|control| reconcile_confirmed_host_loss(root, control).map(Some))
        .unwrap_or(Ok(None))
}

#[cfg(target_os = "linux")]
fn reconcile_confirmed_host_loss(
    root: &Path,
    mut control: RunControl,
) -> Result<RunControl, Invocation> {
    if is_terminal(&control)
        || control.host.is_none()
        || !socket_is_absent(&control.socket).map_err(internal_error)?
    {
        return Ok(control);
    }
    let Some(_publication_lock) =
        store::try_lock_existing_run(root, &control.run_id).map_err(internal_error)?
    else {
        return Ok(control);
    };
    let current_action = current_action(root, &control);
    control.sequence = control.sequence.saturating_add(1);
    control.pause_deadline_unix_ms = None;
    control.result["state"] = json!("blocked");
    control.result["terminal"] = json!(true);
    control.result["reason"] = json!({"code":"host_lost","current_action":current_action});
    control.result["evidence"]["complete"] = json!(false);
    control.result["cleanup"] = json!({
        "browser":"closure_unconfirmed",
        "profile":"removal_unconfirmed",
        "application_state":"caller_owned"
    });
    store::write_run_control(root, &control).map_err(internal_error)?;
    Ok(control)
}

#[cfg(target_os = "linux")]
fn socket_is_absent(path: &Path) -> Result<bool, String> {
    match std::fs::symlink_metadata(path) {
        Ok(_) => Ok(false),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(true),
        Err(error) => Err(format!("cannot inspect run host socket: {error}")),
    }
}

#[cfg(target_os = "linux")]
fn current_action(_root: &Path, control: &RunControl) -> &'static str {
    if store::unresolved_action(&control.evidence_root, &control.run_id).unwrap_or(true) {
        "uncertain"
    } else {
        "none"
    }
}

#[cfg(target_os = "linux")]
fn is_immediate(wait_ms: Option<u64>) -> bool {
    wait_ms.is_none_or(|wait| wait == 0)
}

#[cfg(target_os = "linux")]
fn deadline_elapsed(deadline: Option<Instant>) -> bool {
    deadline.is_some_and(|value| Instant::now() >= value)
}

#[cfg(target_os = "linux")]
fn control_ready(
    control: &RunControl,
    initial_sequence: Option<u64>,
    wait_ms: Option<u64>,
    deadline: Option<Instant>,
) -> bool {
    is_terminal(control)
        || control.result.get("state").and_then(Value::as_str) != Some("running")
        || initial_sequence.is_some_and(|sequence| control.sequence > sequence)
        || is_immediate(wait_ms)
        || deadline_elapsed(deadline)
}

#[cfg(target_os = "linux")]
fn validate_control(control: RunControl) -> Result<RunControl, Invocation> {
    (control.ipc_version == IPC_VERSION)
        .then_some(control)
        .ok_or_else(|| internal_error("run host IPC version is incompatible".into()))
}

#[cfg(target_os = "linux")]
fn run_invocation(control: RunControl) -> Invocation {
    let exit_code = result_exit_code(&control.result).unwrap_or(EXIT_INTERNAL);
    Invocation {
        output: control.result,
        exit_code,
    }
}

#[cfg(target_os = "linux")]
fn request(socket: &Path, kind: &str) -> Result<Value, String> {
    use std::net::Shutdown;
    use std::os::unix::net::UnixStream;

    validate_socket(socket)?;
    let mut stream = UnixStream::connect(socket).map_err(|error| error.to_string())?;
    stream
        .set_read_timeout(Some(Duration::from_millis(250)))
        .and_then(|()| stream.set_write_timeout(Some(Duration::from_millis(250))))
        .map_err(|error| error.to_string())?;
    write_request(&mut stream, kind)?;
    stream
        .shutdown(Shutdown::Write)
        .map_err(|error| error.to_string())?;
    let mut bytes = Vec::new();
    stream
        .read_to_end(&mut bytes)
        .map_err(|error| error.to_string())?;
    let response: Value = serde_json::from_slice(&bytes).map_err(|error| error.to_string())?;
    validate_response(response)
}

#[cfg(target_os = "linux")]
pub(crate) fn request_host_deadline(control: &RunControl) -> Result<(), String> {
    request_for_control(control, "deadline").map(|_| ())
}

#[cfg(target_os = "linux")]
pub(crate) fn request_host_ready(
    socket: &Path,
    run_id: &str,
    job_digest: &str,
) -> Result<(), String> {
    let response = request(socket, "ready")?;
    response_identity_matches(&response, run_id, job_digest)
        .then_some(())
        .ok_or_else(|| "run host readiness response does not match the admitted run".into())
}

#[cfg(target_os = "linux")]
fn request_for_control(control: &RunControl, kind: &str) -> Result<Value, String> {
    let response = request(&control.socket, kind)?;
    response_identity_matches(&response, &control.run_id, &control.job_digest)
        .then_some(response)
        .ok_or_else(|| "run host response does not match durable control".into())
}

#[cfg(target_os = "linux")]
fn response_identity_matches(response: &Value, run_id: &str, job_digest: &str) -> bool {
    response.get("run_id").and_then(Value::as_str) == Some(run_id)
        && response.get("job_digest").and_then(Value::as_str) == Some(job_digest)
}

#[cfg(target_os = "linux")]
fn validate_socket(socket: &Path) -> Result<(), String> {
    use std::os::unix::fs::FileTypeExt;
    let metadata = std::fs::symlink_metadata(socket)
        .map_err(|error| format!("run host socket is unavailable: {error}"))?;
    (!metadata.file_type().is_symlink() && metadata.file_type().is_socket())
        .then_some(())
        .ok_or_else(|| "run host socket is not a private Unix socket".into())
}

#[cfg(target_os = "linux")]
fn write_request(stream: &mut std::os::unix::net::UnixStream, kind: &str) -> Result<(), String> {
    serde_json::to_writer(stream, &json!({"kind":kind,"ipc_version":IPC_VERSION}))
        .map_err(|error| error.to_string())
}

#[cfg(target_os = "linux")]
fn validate_response(response: Value) -> Result<Value, String> {
    let version_ok =
        response.get("ipc_version").and_then(Value::as_u64) == Some(u64::from(IPC_VERSION));
    let accepted = response.get("accepted").and_then(Value::as_bool) == Some(true);
    (version_ok && accepted)
        .then_some(response)
        .ok_or_else(|| "run host rejected the IPC request".into())
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn test_control(root: &Path, terminal: bool) -> RunControl {
        RunControl {
            schema_version: SchemaVersion,
            ipc_version: IPC_VERSION,
            sequence: 1,
            run_id: "r_abort".into(),
            request_id: "original".into(),
            job_digest: "job".into(),
            evidence_root: root.join("evidence"),
            started_unix_ms: 0,
            lifetime_deadline_unix_ms: u64::MAX,
            pause_deadline_unix_ms: None,
            host: None,
            watchdog: None,
            socket: root.join("socket"),
            result: json!({"state":if terminal {"aborted"} else {"running"},"terminal":terminal}),
        }
    }

    #[test]
    fn running_checkpoint_uses_exit_six() {
        let control = RunControl {
            schema_version: manuvra_contract::SchemaVersion,
            ipc_version: IPC_VERSION,
            sequence: 1,
            run_id: "r".into(),
            request_id: "q".into(),
            job_digest: "d".into(),
            evidence_root: "/tmp/e".into(),
            started_unix_ms: 0,
            lifetime_deadline_unix_ms: 1,
            pause_deadline_unix_ms: None,
            host: None,
            watchdog: None,
            socket: "/tmp/s".into(),
            result: json!({"state":"running"}),
        };
        assert_eq!(run_invocation(control).exit_code, 6);
    }

    #[test]
    fn abort_dedup_accepts_only_the_same_completed_digest() {
        let record = store::RequestRecord {
            schema_version: SchemaVersion,
            public_request_id: "original".into(),
            run_id: "r".into(),
            job_digest: "same".into(),
            exit_code: 5,
            result: json!({"state":"aborted","terminal":true}),
        };
        let same = match classify_prior_abort(store::RequestEntry::Complete(record.clone()), "same")
        {
            Ok(Some(invocation)) => invocation,
            _ => panic!("same abort request should deduplicate"),
        };
        assert_eq!(same.exit_code, 5);
        assert_eq!(same.output["state"], "aborted");
        assert!(classify_prior_abort(store::RequestEntry::Complete(record), "different").is_err());
        assert!(
            classify_prior_abort(
                store::RequestEntry::Intent(store::RequestIntent {
                    schema_version: SchemaVersion,
                    public_request_id: "q".into(),
                    run_id: "r".into(),
                    job_digest: "same".into(),
                    evidence_root: "/tmp/e".into(),
                }),
                "same"
            )
            .is_err()
        );
    }

    #[test]
    fn abort_poll_covers_terminal_timeout_pending_missing_and_corrupt_control() {
        let temporary = TempDir::new().unwrap();
        let root = temporary.path().join("state");
        let terminal = test_control(&root, true);
        store::write_run_control(&root, &terminal).unwrap();
        let completed = await_abort(&root, "r_abort", "abort-request", "abort-digest".into());
        assert_eq!(completed.output["state"], "aborted");

        let running = test_control(&root, false);
        store::write_run_control(&root, &running).unwrap();
        let expired =
            poll_abort(&root, "r_abort", "another-abort", "digest", Instant::now()).unwrap();
        assert_eq!(expired.output["state"], "running");
        assert!(
            poll_abort(
                &root,
                "r_abort",
                "another-abort",
                "digest",
                Instant::now() + Duration::from_secs(1)
            )
            .is_none()
        );
        let missing =
            poll_abort(&root, "missing", "another-abort", "digest", Instant::now()).unwrap();
        assert_eq!(missing.output["error"]["code"], "run_not_found");

        let control_path = root.join("runs/r_abort/control.json");
        std::fs::write(control_path, b"not-json").unwrap();
        let corrupt =
            poll_abort(&root, "r_abort", "another-abort", "digest", Instant::now()).unwrap();
        assert_eq!(corrupt.output["error"]["code"], "internal");
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn absent_socket_plus_obtainable_lock_publishes_host_loss_once() {
        let temporary = TempDir::new().unwrap();
        let root = temporary.path().join("state");
        let mut control = test_control(&root, false);
        control.host = Some(store::ProcessIdentity {
            pid: u32::MAX,
            start_ticks: 1,
            session_id: 1,
        });
        let lock = store::lock_run(&root, &control.run_id).unwrap();
        store::write_run_control(&root, &control).unwrap();
        drop(lock);
        assert!(!control.socket.exists());
        let deadline = Instant::now() + Duration::from_secs(1);
        let recovered = loop {
            match read_control(&root, &control.run_id) {
                Ok(Some(control)) if control.result["state"] == "blocked" => break control,
                Ok(Some(_)) if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(5));
                }
                _ => panic!("host loss should become recoverable"),
            }
        };
        assert_eq!(recovered.result["state"], "blocked");
        assert_eq!(recovered.result["reason"]["code"], "host_lost");
        assert_eq!(recovered.result["reason"]["current_action"], "none");
        assert_eq!(recovered.result["evidence"]["complete"], false);

        let repeated = match read_control(&root, &control.run_id) {
            Ok(Some(control)) => control,
            _ => panic!("terminal host loss should remain readable"),
        };
        assert_eq!(repeated.sequence, recovered.sequence);
    }

    #[test]
    fn readiness_rejects_stale_and_wrong_identity_sockets() {
        use std::os::unix::net::UnixListener;

        for (version, run_id, digest) in [
            (IPC_VERSION + 1, "expected", "expected-digest"),
            (IPC_VERSION, "stale-run", "expected-digest"),
            (IPC_VERSION, "expected", "stale-digest"),
        ] {
            let temporary = TempDir::new().unwrap();
            let socket = temporary.path().join("control.sock");
            let listener = UnixListener::bind(&socket).unwrap();
            let server = std::thread::spawn(move || {
                let (mut stream, _) = listener.accept().unwrap();
                let mut request = Vec::new();
                stream.read_to_end(&mut request).unwrap();
                let request: Value = serde_json::from_slice(&request).unwrap();
                assert_eq!(request["kind"], "ready");
                serde_json::to_writer(
                    &mut stream,
                    &json!({
                        "ipc_version":version,
                        "run_id":run_id,
                        "job_digest":digest,
                        "accepted":true
                    }),
                )
                .unwrap();
            });
            assert!(request_host_ready(&socket, "expected", "expected-digest").is_err());
            server.join().unwrap();
        }

        let temporary = TempDir::new().unwrap();
        let socket = temporary.path().join("stale.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        drop(listener);
        assert!(request_host_ready(&socket, "expected", "expected-digest").is_err());
    }
}
