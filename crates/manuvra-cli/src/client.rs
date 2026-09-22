use crate::Invocation;
#[cfg(target_os = "linux")]
use crate::process::{IPC_VERSION, now_unix_ms};
#[cfg(target_os = "linux")]
use crate::store::{self, RunControl};
#[cfg(target_os = "linux")]
use crate::{EXIT_INTERNAL, internal_error, result_exit_code};
use crate::{validate_request_id, validate_run_id};
use manuvra_contract::DispositionRequest;
#[cfg(target_os = "linux")]
use manuvra_contract::SchemaVersion;
#[cfg(target_os = "linux")]
use serde_json::{Value, json};
use std::fs;
#[cfg(target_os = "linux")]
use std::io::Read;
use std::path::Path;
#[cfg(target_os = "linux")]
use std::path::PathBuf;
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
    validate_status_selectors(run_id, request_id)?;
    #[cfg(not(target_os = "linux"))]
    {
        let _ = wait_ms;
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
    let root = store::state_root().map_err(internal_error)?;
    let run_id = resolve_run_id(&root, run_id, request_id).map_err(internal_error)?;
    validate_run_id(&run_id).map_err(internal_error)?;
    let mut control = wait_for_control(&root, &run_id, wait_ms)?;
    refresh_status_from_host(&mut control);
    Ok(run_invocation(control))
}

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
    if let Err(error) = validate_control_ids(run_id, request_id) {
        return error;
    }
    #[cfg(not(target_os = "linux"))]
    {
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

pub fn resume(run_id: &str, request_id: &str, input: &Path) -> Invocation {
    let disposition = match load_disposition(run_id, request_id, input) {
        Ok(disposition) => disposition,
        Err(error) => return error,
    };
    #[cfg(not(target_os = "linux"))]
    {
        let _ = disposition;
        Invocation::error(
            "unsupported_platform",
            "background runs are supported only on Linux",
            3,
        )
    }
    #[cfg(target_os = "linux")]
    {
        resume_linux(run_id, request_id, disposition).unwrap_or_else(|error| error)
    }
}

#[cfg(target_os = "linux")]
fn resume_linux(
    run_id: &str,
    request_id: &str,
    disposition: DispositionRequest,
) -> Result<Invocation, Invocation> {
    resume_at_root(
        store::state_root().map_err(internal_error),
        run_id,
        request_id,
        disposition,
    )
}

#[cfg(target_os = "linux")]
fn resume_at_root(
    root: Result<PathBuf, Invocation>,
    run_id: &str,
    request_id: &str,
    disposition: DispositionRequest,
) -> Result<Invocation, Invocation> {
    let root = root?;
    let _lock = store::lock_request(&root, request_id).map_err(internal_error)?;
    let digest = resume_digest(&root, run_id, &disposition)?;
    prepare_resume(&root, run_id, request_id, &digest)?.finish(
        &root,
        run_id,
        request_id,
        disposition,
        digest,
    )
}

fn load_disposition(
    run_id: &str,
    request_id: &str,
    input: &Path,
) -> Result<DispositionRequest, Invocation> {
    validate_control_ids(run_id, request_id)?;
    let bytes = fs::read(input).map_err(|error| {
        Invocation::error(
            "invalid_input",
            format!("cannot read disposition {}: {error}", input.display()),
            64,
        )
    })?;
    serde_json::from_slice(&bytes)
        .map_err(|_| Invocation::error("invalid_disposition", "disposition input is invalid", 64))
}

fn validate_control_ids(run_id: &str, request_id: &str) -> Result<(), Invocation> {
    validate_run_id(run_id).map_err(|message| Invocation::error("invalid_run_id", message, 64))?;
    validate_request_id(request_id)
        .map_err(|message| Invocation::error("invalid_request_id", message, 64))
}

#[cfg(target_os = "linux")]
fn resume_digest(
    root: &Path,
    run_id: &str,
    disposition: &DispositionRequest,
) -> Result<String, Invocation> {
    let digest_bytes = serde_json::to_vec(&json!({
        "command":"resume",
        "run_id":run_id,
        "disposition":disposition,
    }))
    .map_err(|error| internal_error(error.to_string()))?;
    store::keyed_digest(root, store::DOMAIN_RESUME_REQUEST, &digest_bytes).map_err(internal_error)
}

#[cfg(target_os = "linux")]
enum ResumeStart {
    Prior(Invocation),
    Send {
        control: Box<RunControl>,
        record_intent: bool,
    },
}

#[cfg(target_os = "linux")]
impl ResumeStart {
    fn finish(
        self,
        root: &Path,
        run_id: &str,
        request_id: &str,
        disposition: DispositionRequest,
        digest: String,
    ) -> Result<Invocation, Invocation> {
        match self {
            Self::Prior(invocation) => Ok(invocation),
            Self::Send {
                control,
                record_intent,
            } => submit_resume(
                root,
                run_id,
                request_id,
                disposition,
                digest,
                *control,
                record_intent,
            ),
        }
    }
}

#[cfg(target_os = "linux")]
fn prepare_resume(
    root: &Path,
    run_id: &str,
    request_id: &str,
    digest: &str,
) -> Result<ResumeStart, Invocation> {
    if let Some(entry) = store::lookup_request(root, request_id).map_err(internal_error)? {
        return prepare_prior_resume(root, run_id, digest, entry);
    }
    prepare_new_resume(root, run_id, request_id, digest)
}

#[cfg(target_os = "linux")]
fn prepare_prior_resume(
    root: &Path,
    run_id: &str,
    digest: &str,
    entry: store::RequestEntry,
) -> Result<ResumeStart, Invocation> {
    match entry {
        store::RequestEntry::Complete(record) => {
            classify_prior_resume(root, store::RequestEntry::Complete(record), digest)
                .map(ResumeStart::Prior)
        }
        store::RequestEntry::Intent(intent)
            if intent.job_digest == digest && intent.run_id == run_id =>
        {
            load_resume_control(root, run_id).map(|control| ResumeStart::Send {
                control: Box::new(control),
                record_intent: false,
            })
        }
        store::RequestEntry::Intent(_) => Err(request_conflict()),
    }
}

#[cfg(target_os = "linux")]
fn prepare_new_resume(
    root: &Path,
    run_id: &str,
    request_id: &str,
    digest: &str,
) -> Result<ResumeStart, Invocation> {
    let control = load_resume_control(root, run_id)?;
    if let Err(stale) = reject_terminal_resume(&control) {
        persist_resume_error(root, run_id, request_id, digest, &stale)?;
        return Ok(ResumeStart::Prior(stale));
    }
    Ok(ResumeStart::Send {
        control: Box::new(control),
        record_intent: true,
    })
}

#[cfg(target_os = "linux")]
fn load_resume_control(root: &Path, run_id: &str) -> Result<RunControl, Invocation> {
    let control = store::read_run_control(root, run_id)
        .map_err(internal_error)?
        .ok_or_else(|| Invocation::error("run_not_found", "run was not found", 64))?;
    Ok(control)
}

#[cfg(target_os = "linux")]
fn reject_terminal_resume(control: &RunControl) -> Result<(), Invocation> {
    if is_terminal(control) {
        return Err(Invocation::error(
            "stale_escalation",
            "the run no longer accepts this escalation",
            64,
        ));
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn submit_resume(
    root: &Path,
    run_id: &str,
    request_id: &str,
    disposition: DispositionRequest,
    digest: String,
    control: RunControl,
    record_intent: bool,
) -> Result<Invocation, Invocation> {
    if record_intent {
        record_resume_intent(root, run_id, request_id, &digest, &control)?;
    }
    let payload = json!({
        "kind":"resume",
        "ipc_version":IPC_VERSION,
        "run_id":control.run_id,
        "job_digest":control.job_digest,
        "request_id":request_id,
        "request_digest":digest,
        "request":disposition,
    });
    send_or_recover_resume(root, run_id, request_id, &digest, &control, &payload)?;
    wait_for_resume_completion(root, request_id, &digest, control.lifetime_deadline_unix_ms)
}

#[cfg(target_os = "linux")]
fn send_or_recover_resume(
    root: &Path,
    run_id: &str,
    request_id: &str,
    digest: &str,
    control: &RunControl,
    payload: &Value,
) -> Result<(), Invocation> {
    let Err(error) = send_resume(control, payload) else {
        return Ok(());
    };
    if completed_resume(root, request_id, digest)?.is_some() {
        return Ok(());
    }
    if error.exit_code == 64 {
        persist_resume_error(root, run_id, request_id, digest, &error)?;
    }
    Err(error)
}

#[cfg(target_os = "linux")]
fn completed_resume(
    root: &Path,
    request_id: &str,
    digest: &str,
) -> Result<Option<Invocation>, Invocation> {
    match store::lookup_request(root, request_id).map_err(internal_error)? {
        Some(entry @ store::RequestEntry::Complete(_)) => {
            classify_prior_resume(root, entry, digest).map(Some)
        }
        Some(store::RequestEntry::Intent(intent)) if intent.job_digest == digest => Ok(None),
        Some(_) => Err(request_conflict()),
        None => Err(internal_error(
            "resume request intent disappeared while awaiting its result".into(),
        )),
    }
}

#[cfg(target_os = "linux")]
fn record_resume_intent(
    root: &Path,
    run_id: &str,
    request_id: &str,
    digest: &str,
    control: &RunControl,
) -> Result<(), Invocation> {
    let intent = store::RequestIntent {
        schema_version: SchemaVersion,
        public_request_id: request_id.to_owned(),
        run_id: run_id.to_owned(),
        job_digest: digest.to_owned(),
        evidence_root: control.evidence_root.clone(),
    };
    store::record_control_intent(root, request_id, &intent).map_err(internal_error)
}

#[cfg(target_os = "linux")]
fn wait_for_resume_completion(
    root: &Path,
    request_id: &str,
    digest: &str,
    lifetime_deadline_unix_ms: u64,
) -> Result<Invocation, Invocation> {
    let remaining_ms = lifetime_deadline_unix_ms
        .saturating_sub(now_unix_ms())
        .min(3_600_000);
    let deadline = Instant::now() + Duration::from_millis(remaining_ms.saturating_add(16_000));
    loop {
        if let Some(result) = completed_resume(root, request_id, digest)? {
            return Ok(result);
        }
        if Instant::now() >= deadline {
            return Err(internal_error(
                "resume was accepted but no immutable response was published".into(),
            ));
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[cfg(target_os = "linux")]
fn send_resume(control: &RunControl, payload: &Value) -> Result<(), Invocation> {
    let response = request_value(&control.socket, payload).map_err(map_resume_send_error)?;
    response_identity_matches(&response, &control.run_id, &control.job_digest)
        .then_some(())
        .ok_or_else(|| internal_error("run host response does not match durable control".into()))
}

#[cfg(target_os = "linux")]
fn map_resume_send_error(error: String) -> Invocation {
    match error.as_str() {
        "stale_escalation" => Invocation::error(
            "stale_escalation",
            "the escalation was stale or already consumed",
            64,
        ),
        "request_conflict" => request_conflict(),
        _ => internal_error(error),
    }
}

#[cfg(target_os = "linux")]
fn request_conflict() -> Invocation {
    Invocation::error(
        "request_conflict",
        "request_id was already used for a different request",
        64,
    )
}

#[cfg(target_os = "linux")]
fn classify_prior_resume(
    root: &Path,
    entry: store::RequestEntry,
    digest: &str,
) -> Result<Invocation, Invocation> {
    match entry {
        store::RequestEntry::Complete(record) if record.job_digest == digest => {
            validate_prior_resume_record(root, record)
        }
        _ => Err(request_conflict()),
    }
}

#[cfg(target_os = "linux")]
fn validate_prior_resume_record(
    root: &Path,
    record: store::RequestRecord,
) -> Result<Invocation, Invocation> {
    store::validate_control_record(root, &record).map_err(internal_error)?;
    if record.result.get("error").is_some() {
        return validate_prior_resume_error(record);
    }
    validate_prior_resume_result(record)
}

#[cfg(target_os = "linux")]
fn validate_prior_resume_error(record: store::RequestRecord) -> Result<Invocation, Invocation> {
    let code = record
        .result
        .pointer("/error/code")
        .and_then(Value::as_str)
        .ok_or_else(|| internal_error("completed resume error has no code".into()))?;
    let exit_code = resume_error_exit_code(code)
        .ok_or_else(|| internal_error("completed resume error has an invalid code".into()))?;
    let version_valid = record.result.get("schema_version").and_then(Value::as_u64) == Some(1);
    (version_valid && exit_code == record.exit_code)
        .then_some(Invocation {
            output: record.result,
            exit_code,
        })
        .ok_or_else(|| {
            internal_error("completed resume error exit code does not match its result".into())
        })
}

#[cfg(target_os = "linux")]
fn resume_error_exit_code(code: &str) -> Option<u8> {
    match code {
        "internal" => Some(70),
        "request_conflict" | "stale_escalation" => Some(64),
        _ => None,
    }
}

#[cfg(target_os = "linux")]
fn validate_prior_resume_result(record: store::RequestRecord) -> Result<Invocation, Invocation> {
    let (manifest, exit_code) = validate_resume_result_shape(&record)?;
    validate_terminal_resume_evidence(&record, manifest)?;
    Ok(Invocation {
        output: record.result,
        exit_code,
    })
}

#[cfg(target_os = "linux")]
fn validate_resume_result_shape(record: &store::RequestRecord) -> Result<(&str, u8), Invocation> {
    let manifest = record
        .result
        .pointer("/evidence/manifest")
        .and_then(Value::as_str)
        .ok_or_else(|| internal_error("completed resume has no evidence manifest".into()))?;
    let exit_code = result_exit_code(&record.result).map_err(internal_error)?;
    if !resume_result_identity_matches(record) || exit_code != record.exit_code {
        return Err(internal_error(
            "completed resume exit code does not match its result".into(),
        ));
    }
    Ok((manifest, exit_code))
}

#[cfg(target_os = "linux")]
fn resume_result_identity_matches(record: &store::RequestRecord) -> bool {
    record.result.get("schema_version").and_then(Value::as_u64) == Some(1)
        && record.result.get("run_id").and_then(Value::as_str) == Some(record.run_id.as_str())
        && record
            .result
            .get("request_id")
            .and_then(Value::as_str)
            .is_some()
        && record
            .result
            .pointer("/evidence/complete")
            .and_then(Value::as_bool)
            .is_some()
}

#[cfg(target_os = "linux")]
fn validate_terminal_resume_evidence(
    record: &store::RequestRecord,
    manifest: &str,
) -> Result<(), Invocation> {
    if record.result.get("terminal").and_then(Value::as_bool) != Some(true)
        || record
            .result
            .pointer("/evidence/complete")
            .and_then(Value::as_bool)
            != Some(true)
    {
        return Ok(());
    }
    let result_request_id = record
        .result
        .get("request_id")
        .and_then(Value::as_str)
        .ok_or_else(|| internal_error("completed resume result has no request id".into()))?;
    crate::validate_published_result(
        Path::new(manifest),
        &record.run_id,
        result_request_id,
        Some(&record.result),
        true,
    )
    .map(|_| ())
    .map_err(internal_error)
}

#[cfg(target_os = "linux")]
fn persist_resume_error(
    root: &Path,
    run_id: &str,
    request_id: &str,
    digest: &str,
    error: &Invocation,
) -> Result<(), Invocation> {
    let record = resume_record(
        root,
        request_id,
        run_id,
        digest,
        error.exit_code,
        error.output.clone(),
    )?;
    store::finalize_control_request(root, request_id, &record).map_err(internal_error)
}

#[cfg(target_os = "linux")]
fn resume_record(
    root: &Path,
    request_id: &str,
    run_id: &str,
    request_digest: &str,
    exit_code: u8,
    result: Value,
) -> Result<store::RequestRecord, Invocation> {
    store::sealed_control_record(root, request_id, run_id, request_digest, exit_code, result)
        .map_err(internal_error)
}

#[cfg(target_os = "linux")]
fn abort_linux(run_id: &str, request_id: &str) -> Invocation {
    abort_in_state(run_id, request_id).unwrap_or_else(|invocation| invocation)
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
        .and_then(|bytes| store::keyed_digest(root, store::DOMAIN_ABORT_REQUEST, &bytes))
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
        _ => Err(request_conflict()),
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
    request_effect_for_control(&control, "abort").map_err(internal_error)?;
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
        result_digest: None,
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
    request_value(socket, &json!({"kind":kind,"ipc_version":IPC_VERSION}))
}

#[cfg(target_os = "linux")]
fn request_value(socket: &Path, payload: &Value) -> Result<Value, String> {
    use std::net::Shutdown;
    use std::os::unix::net::UnixStream;

    validate_socket(socket)?;
    let mut stream = UnixStream::connect(socket).map_err(|error| error.to_string())?;
    stream
        .set_read_timeout(Some(Duration::from_millis(250)))
        .and_then(|()| stream.set_write_timeout(Some(Duration::from_millis(250))))
        .map_err(|error| error.to_string())?;
    serde_json::to_writer(&mut stream, payload).map_err(|error| error.to_string())?;
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
    request_effect_for_control(control, "deadline").map(|_| ())
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
fn request_effect_for_control(control: &RunControl, kind: &str) -> Result<Value, String> {
    let response = request_value(
        &control.socket,
        &json!({
            "kind":kind,
            "ipc_version":IPC_VERSION,
            "run_id":control.run_id,
            "job_digest":control.job_digest,
        }),
    )?;
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
fn validate_response(response: Value) -> Result<Value, String> {
    let version_ok =
        response.get("ipc_version").and_then(Value::as_u64) == Some(u64::from(IPC_VERSION));
    let accepted = response.get("accepted").and_then(Value::as_bool) == Some(true);
    if version_ok && accepted {
        Ok(response)
    } else {
        Err(response
            .get("error_code")
            .and_then(Value::as_str)
            .unwrap_or("run host rejected the IPC request")
            .to_owned())
    }
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;
    use manuvra_contract::{
        Disposition, Manifest, RetryObservationDisposition, RetryObservationKind,
    };
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

    fn retry_disposition() -> DispositionRequest {
        DispositionRequest {
            schema_version: SchemaVersion,
            escalation_id: "e_1".into(),
            disposition: Disposition::RetryObservation(RetryObservationDisposition {
                kind: RetryObservationKind::RetryObservation,
            }),
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
            result_digest: None,
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
    fn resume_dedup_preserves_the_sealed_processed_checkpoint() {
        let temporary = TempDir::new().unwrap();
        let root = temporary.path().join("state");
        let record = resume_record(
            &root,
            "resume",
            "r",
            "same",
            64,
            json!({"schema_version":1,"error":{"code":"stale_escalation","message":"stale"}}),
        )
        .unwrap();
        let prior = match classify_prior_resume(
            &root,
            store::RequestEntry::Complete(record.clone()),
            "same",
        ) {
            Ok(prior) => prior,
            Err(_) => panic!("matching completed resume must deduplicate"),
        };
        assert_eq!(prior.output["error"]["code"], "stale_escalation");
        let mut tampered = record;
        tampered.result["error"]["message"] = json!("changed");
        let corruption =
            classify_prior_resume(&root, store::RequestEntry::Complete(tampered), "same")
                .unwrap_err();
        assert_eq!(corruption.output["error"]["code"], "internal");
        assert!(
            classify_prior_resume(
                &root,
                store::RequestEntry::Intent(store::RequestIntent {
                    schema_version: SchemaVersion,
                    public_request_id: "resume".into(),
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
    fn completed_resume_validates_its_sealed_product_record() {
        let temporary = TempDir::new().unwrap();
        let job = manuvra_contract::Job::parse(
            serde_json::to_vec(&json!({
                "schema_version":1,
                "target":{"kind":"browser","url":"http://example.test"},
                "context":{"journey":"x","revision":"x","environment":"x","actor":"x","authority":"x"},
                "steps":[{"id":"s","goal":"g","requires_values":["missing"],"done_when":[{"url_contains":"/"}]}]
            }))
            .unwrap()
            .as_slice(),
        )
        .unwrap();
        let redactor = crate::evidence::Redactor::for_job_with_provider_key(&job, None).unwrap();
        let evidence_root = temporary.path().join("evidence");
        let published = crate::evidence::publish(
            &evidence_root,
            "original-request",
            "r_valid_resume",
            &job,
            crate::evidence::BlockedStop::missing_value("missing".into(), "s".into()),
            &redactor,
        )
        .unwrap();
        let state_root = temporary.path().join("state");
        let record = resume_record(
            &state_root,
            "resume-request",
            "r_valid_resume",
            "digest",
            3,
            published.result,
        )
        .unwrap();
        let invocation = match validate_prior_resume_record(&state_root, record) {
            Ok(invocation) => invocation,
            Err(_) => panic!("valid published resume evidence should be accepted"),
        };
        assert_eq!(invocation.output["reason"]["code"], "missing_value");
    }

    #[test]
    fn terminal_resume_replay_rejects_every_corrupt_published_bundle_shape() {
        for corruption in [
            "missing",
            "manifest_incomplete",
            "artifact_incomplete",
            "extra",
        ] {
            let temporary = TempDir::new().unwrap();
            let job = manuvra_contract::Job::parse(
                serde_json::to_vec(&json!({
                    "schema_version":1,
                    "target":{"kind":"browser","url":"http://example.test"},
                    "context":{"journey":"x","revision":"x","environment":"x","actor":"x","authority":"x"},
                    "steps":[{"id":"s","goal":"g","requires_values":["missing"],"done_when":[{"url_contains":"/"}]}]
                }))
                .unwrap()
                .as_slice(),
            )
            .unwrap();
            let redactor =
                crate::evidence::Redactor::for_job_with_provider_key(&job, None).unwrap();
            let evidence_root = temporary.path().join("evidence");
            let run_id = format!("r_corrupt_{corruption}");
            let published = crate::evidence::publish(
                &evidence_root,
                "original-request",
                &run_id,
                &job,
                crate::evidence::BlockedStop::missing_value("missing".into(), "s".into()),
                &redactor,
            )
            .unwrap();
            let state_root = temporary.path().join("state");
            let record = resume_record(
                &state_root,
                "resume-request",
                &run_id,
                "digest",
                3,
                published.result,
            )
            .unwrap();
            let run_dir = evidence_root.join(&run_id);
            let manifest_path = run_dir.join("manifest.json");
            match corruption {
                "missing" => std::fs::remove_file(run_dir.join("result.json")).unwrap(),
                "extra" => std::fs::write(run_dir.join("unmanifested.json"), b"{}\n").unwrap(),
                kind => {
                    let mut manifest: Manifest =
                        serde_json::from_slice(&std::fs::read(&manifest_path).unwrap()).unwrap();
                    if kind == "manifest_incomplete" {
                        manifest.complete = false;
                    } else {
                        manifest
                            .artifacts
                            .iter_mut()
                            .find(|artifact| artifact.role == "result")
                            .unwrap()
                            .complete = false;
                    }
                    std::fs::write(
                        &manifest_path,
                        serde_json::to_vec_pretty(&manifest).unwrap(),
                    )
                    .unwrap();
                }
            }
            let error = validate_prior_resume_record(&state_root, record).unwrap_err();
            assert_eq!(error.output["error"]["code"], "internal", "{corruption}");
        }
    }

    #[test]
    fn resume_send_errors_preserve_stale_conflict_and_internal_meaning() {
        assert_eq!(
            map_resume_send_error("stale_escalation".into()).output["error"]["code"],
            "stale_escalation"
        );
        assert_eq!(
            map_resume_send_error("request_conflict".into()).output["error"]["code"],
            "request_conflict"
        );
        assert_eq!(
            map_resume_send_error("transport lost".into()).output["error"]["code"],
            "internal"
        );
    }

    #[test]
    fn resume_preparation_distinguishes_new_prior_terminal_and_missing_runs() {
        let temporary = TempDir::new().unwrap();
        let root = temporary.path().join("state");
        store::keyed_digest(&root, b"test-bootstrap", b"bootstrap").unwrap();
        let running = test_control(&root, false);
        store::write_run_control(&root, &running).unwrap();
        assert!(matches!(
            prepare_resume(&root, "r_abort", "new", "digest"),
            Ok(ResumeStart::Send {
                record_intent: true,
                ..
            })
        ));
        store::record_control_intent(
            &root,
            "recover",
            &store::RequestIntent {
                schema_version: SchemaVersion,
                public_request_id: "recover".into(),
                run_id: "r_abort".into(),
                job_digest: "digest".into(),
                evidence_root: root.join("evidence"),
            },
        )
        .unwrap();
        assert!(matches!(
            prepare_resume(&root, "r_abort", "recover", "digest"),
            Ok(ResumeStart::Send {
                record_intent: false,
                ..
            })
        ));

        let record = resume_record(
            &root,
            "prior",
            "r_abort",
            "digest",
            64,
            json!({"schema_version":1,"error":{"code":"stale_escalation","message":"stale"}}),
        )
        .unwrap();
        store::finalize_control_request(&root, "prior", &record).unwrap();
        assert!(matches!(
            prepare_resume(&root, "r_abort", "prior", "digest"),
            Ok(ResumeStart::Prior(_))
        ));
        let prior = ResumeStart::Prior(Invocation {
            output: json!({"state":"passed"}),
            exit_code: 0,
        });
        let prior = match prior.finish(
            &root,
            "r_abort",
            "unused",
            retry_disposition(),
            "digest".into(),
        ) {
            Ok(invocation) => invocation,
            Err(error) => panic!("prior resume failed: {}", error.output),
        };
        assert_eq!(prior.output["state"], "passed");
        assert!(prepare_resume(&root, "r_abort", "prior", "other").is_err());

        store::write_run_control(&root, &test_control(&root, true)).unwrap();
        let stale = match prepare_resume(&root, "r_abort", "terminal", "digest") {
            Ok(ResumeStart::Prior(stale)) => stale,
            _ => panic!("terminal run must finalize a stale resume"),
        };
        assert_eq!(stale.output["error"]["code"], "stale_escalation");
        assert!(matches!(
            prepare_resume(&root, "r_abort", "terminal", "digest"),
            Ok(ResumeStart::Prior(_))
        ));
        let conflict = match prepare_resume(&root, "r_abort", "terminal", "different") {
            Err(error) => error,
            Ok(_) => panic!("a stale request id must still reject a different digest"),
        };
        assert_eq!(conflict.output["error"]["code"], "request_conflict");
        let missing = match prepare_resume(&root, "missing", "missing", "digest") {
            Err(error) => error,
            Ok(_) => panic!("missing run must reject a resume"),
        };
        assert_eq!(missing.output["error"]["code"], "run_not_found");

        let disposition = retry_disposition();
        let resume_root = temporary.path().join("resume-state");
        let valid_run = "r_1234567890abcdef";
        let digest = match resume_digest(&resume_root, valid_run, &disposition) {
            Ok(digest) => digest,
            Err(error) => panic!("resume digest failed: {}", error.output),
        };
        let prior_record = resume_record(
            &resume_root,
            "prior-root",
            valid_run,
            &digest,
            64,
            json!({"schema_version":1,"error":{"code":"request_conflict","message":"conflict"}}),
        )
        .unwrap();
        store::finalize_control_request(&resume_root, "prior-root", &prior_record).unwrap();
        let resumed = match resume_at_root(
            Ok(resume_root),
            valid_run,
            "prior-root",
            disposition.clone(),
        ) {
            Ok(invocation) => invocation,
            Err(error) => panic!("resume from injected root failed: {}", error.output),
        };
        assert_eq!(resumed.output["error"]["code"], "request_conflict");
        let root_error = match resume_at_root(
            Err(Invocation::error("internal", "state unavailable", 3)),
            valid_run,
            "new-root",
            disposition,
        ) {
            Err(error) => error,
            Ok(_) => panic!("missing state root must fail"),
        };
        assert_eq!(root_error.output["error"]["code"], "internal");
    }

    #[test]
    fn lost_resume_response_returns_the_exact_historical_checkpoint_after_later_progress() {
        let temporary = TempDir::new().unwrap();
        let root = temporary.path().join("state");
        store::keyed_digest(&root, b"test-bootstrap", b"bootstrap").unwrap();
        let historical = json!({
            "schema_version":1,
            "request_id":"original-request",
            "run_id":"r_abort",
            "state":"uncertain",
            "terminal":false,
            "evidence":{"complete":true,"manifest":"/tmp/checkpoint/manifest.json"},
            "escalation":{"id":"e_2"}
        });
        let record = resume_record(
            &root,
            "resume-request",
            "r_abort",
            "resume-digest",
            2,
            historical.clone(),
        )
        .unwrap();
        store::finalize_control_request(&root, "resume-request", &record).unwrap();
        let mut later = test_control(&root, true);
        later.sequence = 9;
        later.result = json!({"state":"passed","terminal":true,"escalation":null});
        store::write_run_control(&root, &later).unwrap();

        let recovered = completed_resume(&root, "resume-request", "resume-digest")
            .unwrap()
            .expect("sealed historical checkpoint must be recoverable");
        assert_eq!(recovered.output["escalation"]["id"], "e_2");
        assert_eq!(recovered.output, historical);
    }

    #[test]
    fn submit_resume_sends_disposition_and_finalizes_the_advanced_checkpoint() {
        use std::io::Write;
        use std::os::unix::net::UnixListener;

        let temporary = TempDir::new().unwrap();
        let root = temporary.path().join("state");
        store::keyed_digest(&root, b"test-bootstrap", b"bootstrap").unwrap();
        let mut initial = test_control(&root, false);
        initial.result = json!({"state":"uncertain","terminal":false,"escalation":{"id":"e_1"}});
        initial.socket = root.join("resume.sock");
        store::write_run_control(&root, &initial).unwrap();
        std::fs::create_dir_all(&root).unwrap();
        let listener = UnixListener::bind(&initial.socket).unwrap();
        let server_root = root.clone();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut bytes = Vec::new();
            stream.read_to_end(&mut bytes).unwrap();
            let request: Value = serde_json::from_slice(&bytes).unwrap();
            assert_eq!(request["kind"], "resume");
            assert_eq!(request["request"]["escalation_id"], "e_1");
            assert_eq!(request["request_id"], "resume-request");
            assert_eq!(request["request_digest"], "resume-digest");
            assert_eq!(request["run_id"], "r_abort");
            assert_eq!(request["job_digest"], "job");
            serde_json::to_writer(
                &mut stream,
                &json!({"ipc_version":IPC_VERSION,"run_id":"r_abort","job_digest":"job","accepted":true}),
            )
            .unwrap();
            stream.flush().unwrap();
            let result = json!({
                "schema_version":1,
                "request_id":"original",
                "run_id":"r_abort",
                "state":"running",
                "terminal":false,
                "evidence":{"complete":false,"manifest":"/tmp/checkpoint/manifest.json"}
            });
            let record = store::sealed_control_record(
                &server_root,
                "resume-request",
                "r_abort",
                "resume-digest",
                6,
                result,
            )
            .unwrap();
            store::finalize_control_request(&server_root, "resume-request", &record).unwrap();
        });

        let invocation = match (ResumeStart::Send {
            control: Box::new(initial),
            record_intent: true,
        })
        .finish(
            &root,
            "r_abort",
            "resume-request",
            retry_disposition(),
            "resume-digest".into(),
        ) {
            Ok(invocation) => invocation,
            Err(error) => panic!("submit failed: {}", error.output),
        };
        server.join().unwrap();
        assert_eq!(invocation.output["state"], "running");
        assert!(matches!(
            store::lookup_request(&root, "resume-request").unwrap(),
            Some(store::RequestEntry::Complete(_))
        ));
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
            process_group: u32::MAX,
            start_marker: 1,
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
