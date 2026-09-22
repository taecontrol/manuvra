mod client;
#[cfg(any(target_os = "linux", target_os = "macos"))]
#[allow(dead_code)]
mod control_socket;
mod evidence;
#[cfg(target_os = "linux")]
mod host;
#[cfg(any(target_os = "linux", target_os = "macos"))]
#[cfg_attr(target_os = "macos", allow(dead_code))]
mod process;
#[cfg(any(target_os = "linux", target_os = "macos"))]
#[allow(dead_code)]
mod runtime;
#[cfg(any(target_os = "linux", target_os = "macos"))]
#[allow(dead_code)]
mod socket_auth;
#[cfg_attr(target_os = "macos", allow(dead_code))]
mod store;
#[cfg(any(target_os = "linux", target_os = "macos"))]
mod watchdog;

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};

use clap::{ArgGroup, Parser, Subcommand, ValueEnum};
use manuvra_contract::{Job, Manifest, SchemaKind, SchemaVersion, schema};
use serde_json::{Value, json};

const EXIT_PASSED: u8 = 0;
const EXIT_BLOCKED: u8 = 3;
const EXIT_INVALID: u8 = 64;
pub(crate) const EXIT_INTERNAL: u8 = 70;

#[derive(Debug, Parser)]
#[command(name = "manuvra", disable_version_flag = true)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    Run {
        #[arg(long)]
        request_id: String,
        #[arg(long)]
        job: PathBuf,
        #[arg(long)]
        evidence: PathBuf,
        #[arg(long)]
        browser: Option<PathBuf>,
        #[arg(long)]
        headless: bool,
        #[arg(long)]
        wait_ms: Option<u64>,
    },
    Resume {
        run_id: String,
        #[arg(long)]
        request_id: String,
        #[arg(long)]
        input: PathBuf,
    },
    #[command(group(
        ArgGroup::new("run_selector")
            .required(true)
            .multiple(false)
            .args(["run_id", "request_id"])
    ))]
    Status {
        run_id: Option<String>,
        #[arg(long)]
        request_id: Option<String>,
        #[arg(long)]
        wait_ms: Option<u64>,
    },
    Abort {
        run_id: String,
        #[arg(long)]
        request_id: String,
    },
    Schema {
        kind: SchemaArg,
    },
    Version,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum SchemaArg {
    Job,
    Result,
    Disposition,
    Manifest,
}

#[derive(Debug)]
pub struct Invocation {
    pub output: Value,
    pub exit_code: u8,
}

impl Invocation {
    fn success(output: Value) -> Self {
        Self {
            output,
            exit_code: EXIT_PASSED,
        }
    }

    fn error(code: &str, message: impl Into<String>, exit_code: u8) -> Self {
        let message = scrub_provider_key(&message.into());
        Self {
            output: json!({
                "schema_version": 1,
                "error": {"code": code, "message": message}
            }),
            exit_code,
        }
    }
}

fn scrub_provider_key(message: &str) -> String {
    std::env::var("TYPESAFE_API_KEY")
        .ok()
        .filter(|key| !key.is_empty())
        .map_or_else(
            || message.to_owned(),
            |key| message.replace(&key, "<masked-key>"),
        )
}

pub fn invoke(args: impl IntoIterator<Item = OsString>) -> Invocation {
    let cli = match Cli::try_parse_from(args) {
        Ok(cli) => cli,
        Err(error) => {
            return Invocation::error("invalid_arguments", error.to_string(), EXIT_INVALID);
        }
    };
    cli.command.execute()
}

pub fn internal_main(command: &str) -> Option<u8> {
    match command {
        #[cfg(target_os = "linux")]
        "__host" => Some(host::main().map_or(EXIT_INTERNAL, |()| EXIT_PASSED)),
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        "__watchdog" => Some(watchdog::main().map_or(EXIT_INTERNAL, |()| EXIT_PASSED)),
        #[cfg(not(target_os = "linux"))]
        "__host" => Some(EXIT_BLOCKED),
        _ => None,
    }
}

impl Command {
    fn execute(self) -> Invocation {
        match self {
            Self::Run {
                request_id,
                job,
                evidence,
                browser,
                headless,
                wait_ms,
            } => run(
                &request_id,
                &job,
                &evidence,
                browser.as_deref(),
                headless,
                wait_ms,
            ),
            Self::Schema { kind } => Invocation::success(schema(kind.into())),
            Self::Version => Invocation::success(json!({
                "schema_version": 1,
                "version": env!("CARGO_PKG_VERSION")
            })),
            Self::Resume {
                run_id,
                request_id,
                input,
            } => client::resume(&run_id, &request_id, &input),
            Self::Status {
                run_id,
                request_id,
                wait_ms,
            } => client::status(run_id.as_deref(), request_id.as_deref(), wait_ms),
            Self::Abort { run_id, request_id } => client::abort(&run_id, &request_id),
        }
    }
}

impl From<SchemaArg> for SchemaKind {
    fn from(value: SchemaArg) -> Self {
        match value {
            SchemaArg::Job => Self::Job,
            SchemaArg::Result => Self::Result,
            SchemaArg::Disposition => Self::Disposition,
            SchemaArg::Manifest => Self::Manifest,
        }
    }
}

fn run(
    request_id: &str,
    job_path: &Path,
    evidence_root: &Path,
    browser: Option<&Path>,
    headless: bool,
    wait_ms: Option<u64>,
) -> Invocation {
    try_run(
        request_id,
        job_path,
        evidence_root,
        browser,
        headless,
        wait_ms,
    )
    .unwrap_or_else(|error| error)
}

fn try_run(
    request_id: &str,
    job_path: &Path,
    evidence_root: &Path,
    browser: Option<&Path>,
    headless: bool,
    wait_ms: Option<u64>,
) -> Result<Invocation, Invocation> {
    #[cfg(target_os = "macos")]
    if runtime::runtime_root().is_err() {
        return Err(Invocation::error(
            "runtime_directory_unavailable",
            "neither XDG_RUNTIME_DIR nor TMPDIR is set",
            EXIT_BLOCKED,
        ));
    }
    if evidence_root.to_str().is_none() {
        return Err(Invocation::error(
            "invalid_input",
            "evidence path must be valid UTF-8",
            EXIT_INVALID,
        ));
    }
    match prepare_run(request_id, job_path, evidence_root, browser, headless)? {
        PreparedRun::Existing(invocation) => Ok(invocation),
        PreparedRun::New(prepared) => publish_new_run(prepared, wait_ms),
    }
}

enum PreparedRun {
    Existing(Invocation),
    New(Box<Prepared>),
}

struct Prepared {
    job: Job,
    intent: store::RequestIntent,
    lookup_request_id: String,
    redactor: evidence::Redactor,
    state_root: PathBuf,
    _request_lock: store::RequestLock,
    browser: Option<PathBuf>,
    headless: bool,
    #[cfg(target_os = "linux")]
    existing_intent: bool,
}

fn prepare_run(
    request_id: &str,
    job_path: &Path,
    evidence_root: &Path,
    browser: Option<&Path>,
    headless: bool,
) -> Result<PreparedRun, Invocation> {
    validate_request_id(request_id)
        .map_err(|message| Invocation::error("invalid_request_id", message, EXIT_INVALID))?;
    let job = load_job(job_path)?;
    let (admission, existing, browser) = admit_run(request_id, browser, headless, job)?;
    finish_preparation(
        request_id,
        evidence_root,
        admission,
        existing,
        browser.as_deref(),
        headless,
    )
}

fn admit_run(
    request_id: &str,
    browser: Option<&Path>,
    headless: bool,
    job: Job,
) -> Result<(Admission, RequestLookup, Option<PathBuf>), Invocation> {
    let redactor = evidence::Redactor::for_job(&job).map_err(internal_error)?;
    let state_root =
        store::state_root().map_err(|error| internal_error(redactor.redact_text(&error)))?;
    reject_sensitive_internal_path(&state_root, &redactor, "state")?;
    let request_lock = store::lock_request(&state_root, request_id)
        .map_err(|error| internal_error(redactor.redact_text(&error)))?;
    let browser = effective_browser_selection(browser);
    let digest = canonical_digest(&state_root, &job, browser.as_deref(), headless, &redactor)?;
    let existing = lookup_request(&state_root, request_id, &digest, &redactor)?;
    Ok((
        Admission {
            job,
            redactor,
            state_root,
            request_lock,
            digest,
        },
        existing,
        browser,
    ))
}

fn reject_sensitive_internal_path(
    path: &Path,
    redactor: &evidence::Redactor,
    purpose: &str,
) -> Result<(), Invocation> {
    (!redactor.contains_sensitive(&path.to_string_lossy()))
        .then_some(())
        .ok_or_else(|| {
            Invocation::error(
                "invalid_input",
                format!("{purpose} path contains a classified value rendering"),
                EXIT_INVALID,
            )
        })
}

fn effective_browser_selection(explicit: Option<&Path>) -> Option<PathBuf> {
    explicit
        .map(Path::to_path_buf)
        .or_else(|| std::env::var_os("MANUVRA_BROWSER").map(PathBuf::from))
}

struct Admission {
    job: Job,
    redactor: evidence::Redactor,
    state_root: PathBuf,
    request_lock: store::RequestLock,
    digest: String,
}

fn finish_preparation(
    request_id: &str,
    evidence_root: &Path,
    admission: Admission,
    existing: RequestLookup,
    browser: Option<&Path>,
    headless: bool,
) -> Result<PreparedRun, Invocation> {
    let (intent, existing_intent) = match existing {
        RequestLookup::Complete(invocation) => return Ok(PreparedRun::Existing(invocation)),
        RequestLookup::Intent(intent) => (intent, true),
        RequestLookup::Missing => (
            create_intent(
                request_id,
                evidence_root,
                &admission.redactor,
                &admission.state_root,
                admission.digest,
            )?,
            false,
        ),
    };
    #[cfg(not(target_os = "linux"))]
    let _ = existing_intent;
    Ok(PreparedRun::New(Box::new(Prepared {
        job: admission.job,
        intent,
        lookup_request_id: request_id.to_owned(),
        redactor: admission.redactor,
        state_root: admission.state_root,
        _request_lock: admission.request_lock,
        browser: browser.map(Path::to_path_buf),
        headless,
        #[cfg(target_os = "linux")]
        existing_intent,
    })))
}

fn create_intent(
    request_id: &str,
    evidence_root: &Path,
    redactor: &evidence::Redactor,
    state_root: &Path,
    digest: String,
) -> Result<store::RequestIntent, Invocation> {
    reject_sensitive_evidence_path(evidence_root, redactor)?;
    let absolute_evidence = evidence::prepare_root(evidence_root, redactor)
        .map_err(|error| internal_error(redactor.redact_text(&error)))?;
    let intent = store::RequestIntent {
        schema_version: SchemaVersion,
        public_request_id: redactor.redact_export_text(request_id),
        run_id: evidence::new_run_id(),
        job_digest: digest,
        evidence_root: absolute_evidence,
    };
    store::record_intent(state_root, request_id, &intent)
        .map_err(|error| internal_error(redactor.redact_text(&error)))?;
    Ok(intent)
}

fn reject_sensitive_evidence_path(
    evidence_root: &Path,
    redactor: &evidence::Redactor,
) -> Result<(), Invocation> {
    if redactor.contains_sensitive(&evidence_root.to_string_lossy()) {
        Err(Invocation::error(
            "invalid_input",
            "evidence path contains a classified value rendering",
            EXIT_INVALID,
        ))
    } else {
        Ok(())
    }
}

fn publish_new_run(
    prepared: Box<Prepared>,
    wait_ms: Option<u64>,
) -> Result<Invocation, Invocation> {
    if let Some(stop) = admission_stop(&prepared) {
        return publish_admission_stop(prepared, stop);
    }
    publish_executable_run(prepared, wait_ms)
}

fn admission_stop(prepared: &Prepared) -> Option<evidence::BlockedStop> {
    prepared
        .job
        .first_missing_value()
        .map(|missing| evidence::BlockedStop::missing_value(missing.value_name, missing.step_id))
        .or_else(|| {
            prepared
                .job
                .first_unsupported_feature()
                .map(evidence::BlockedStop::unsupported)
        })
}

fn publish_admission_stop(
    prepared: Box<Prepared>,
    stop: evidence::BlockedStop,
) -> Result<Invocation, Invocation> {
    if let Some((result, exit_code)) = recover_flow_result(&prepared)
        .map_err(|error| internal_error(prepared.redactor.redact_text(&error)))?
    {
        return finalize(prepared, result, exit_code);
    }
    publish_blocked(prepared, stop)
}

#[cfg(target_os = "linux")]
fn publish_executable_run(
    prepared: Box<Prepared>,
    wait_ms: Option<u64>,
) -> Result<Invocation, Invocation> {
    if prepared.existing_intent {
        return attach_or_recover_background_run(prepared, wait_ms);
    }
    start_background_run(prepared, wait_ms)
}

#[cfg(not(target_os = "linux"))]
fn publish_executable_run(
    prepared: Box<Prepared>,
    wait_ms: Option<u64>,
) -> Result<Invocation, Invocation> {
    let _ = wait_ms;
    if let Some((result, exit_code)) = recover_flow_result(&prepared)
        .map_err(|error| internal_error(prepared.redactor.redact_text(&error)))?
    {
        return finalize(prepared, result, exit_code);
    }
    let outcome = manuvra_flow::run(
        &prepared.job,
        manuvra_flow::FlowConfig {
            request_id: prepared.intent.public_request_id.clone(),
            run_id: prepared.intent.run_id.clone(),
            evidence_root: prepared.intent.evidence_root.clone(),
            browser: prepared.browser.clone(),
            headless: prepared.headless,
        },
        &prepared.redactor,
    )
    .map_err(|error| internal_error(prepared.redactor.redact_text(&error)))?;
    finalize(prepared, outcome.result, outcome.exit_code)
}

#[cfg(target_os = "linux")]
fn start_background_run(
    prepared: Box<Prepared>,
    wait_ms: Option<u64>,
) -> Result<Invocation, Invocation> {
    start_new_background_run(prepared, wait_ms)
}

#[cfg(target_os = "linux")]
fn attach_or_recover_background_run(
    prepared: Box<Prepared>,
    wait_ms: Option<u64>,
) -> Result<Invocation, Invocation> {
    match existing_control_after_bootstrap(&prepared)? {
        Some(control) => attach_to_existing_control(prepared, control, wait_ms),
        None => recover_existing_without_control(prepared),
    }
}

#[cfg(target_os = "linux")]
fn existing_control_after_bootstrap(
    prepared: &Prepared,
) -> Result<Option<store::RunControl>, Invocation> {
    let mut control = store::read_run_control(&prepared.state_root, &prepared.intent.run_id)
        .map_err(|error| internal_error(prepared.redactor.redact_text(&error)))?;
    if control.as_ref().is_some_and(control_is_bootstrapping) {
        control = reconcile_abandoned_bootstrap(prepared)?;
        if control.as_ref().is_some_and(control_is_bootstrapping) {
            await_host_readiness(prepared)?;
            control = store::read_run_control(&prepared.state_root, &prepared.intent.run_id)
                .map_err(|error| internal_error(prepared.redactor.redact_text(&error)))?;
        }
    }
    Ok(control)
}

#[cfg(target_os = "linux")]
fn reconcile_abandoned_bootstrap(
    prepared: &Prepared,
) -> Result<Option<store::RunControl>, Invocation> {
    let Some(_publication_lock) =
        store::try_lock_existing_run(&prepared.state_root, &prepared.intent.run_id)
            .map_err(|error| internal_error(prepared.redactor.redact_text(&error)))?
    else {
        return store::read_run_control(&prepared.state_root, &prepared.intent.run_id)
            .map_err(|error| internal_error(prepared.redactor.redact_text(&error)));
    };
    reconcile_abandoned_bootstrap_locked(prepared)
}

#[cfg(target_os = "linux")]
fn reconcile_abandoned_bootstrap_locked(
    prepared: &Prepared,
) -> Result<Option<store::RunControl>, Invocation> {
    store::read_run_control(&prepared.state_root, &prepared.intent.run_id)
        .map_err(|error| internal_error(prepared.redactor.redact_text(&error)))?
        .map(|control| reconcile_abandoned_control(prepared, control))
        .transpose()
}

#[cfg(target_os = "linux")]
fn reconcile_abandoned_control(
    prepared: &Prepared,
    mut control: store::RunControl,
) -> Result<store::RunControl, Invocation> {
    validate_existing_control(prepared, &control)?;
    if !control_is_bootstrapping(&control) {
        return Ok(control);
    }
    if !bootstrap_has_no_owner(&control) || !bootstrap_socket_is_absent(prepared, &control.socket)?
    {
        return Ok(control);
    }
    publish_abandoned_bootstrap(prepared, &mut control)?;
    Ok(control)
}

#[cfg(target_os = "linux")]
fn bootstrap_has_no_owner(control: &store::RunControl) -> bool {
    control.host.is_none() && control.watchdog.is_none()
}

#[cfg(target_os = "linux")]
fn bootstrap_socket_is_absent(prepared: &Prepared, socket: &Path) -> Result<bool, Invocation> {
    match fs::symlink_metadata(socket) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(true),
        Ok(_) => Ok(false),
        Err(error) => Err(internal_error(prepared.redactor.redact_text(&format!(
            "cannot inspect bootstrapping run socket: {error}"
        )))),
    }
}

#[cfg(target_os = "linux")]
fn publish_abandoned_bootstrap(
    prepared: &Prepared,
    control: &mut store::RunControl,
) -> Result<(), Invocation> {
    control.sequence = control.sequence.saturating_add(1);
    control.pause_deadline_unix_ms = None;
    control.result["state"] = json!("blocked");
    control.result["terminal"] = json!(true);
    control.result["reason"] = json!({"code":"host_lost","current_action":"none"});
    control.result["evidence"]["complete"] = json!(false);
    control.result["cleanup"] = json!({
        "browser":"not_started",
        "profile":"not_created",
        "application_state":"caller_owned"
    });
    store::write_run_control(&prepared.state_root, control)
        .map_err(|error| internal_error(prepared.redactor.redact_text(&error)))
}

#[cfg(target_os = "linux")]
fn control_is_bootstrapping(control: &store::RunControl) -> bool {
    control.result.get("terminal").and_then(Value::as_bool) != Some(true) && control.host.is_none()
}

#[cfg(target_os = "linux")]
fn attach_to_existing_control(
    prepared: Box<Prepared>,
    control: store::RunControl,
    wait_ms: Option<u64>,
) -> Result<Invocation, Invocation> {
    validate_existing_control(&prepared, &control)?;
    if control.result.get("terminal").and_then(Value::as_bool) == Some(true) {
        return recover_or_return_terminal_control(prepared, control, wait_ms);
    }
    if client::request_host_ready(
        &control.socket,
        &prepared.intent.run_id,
        &prepared.intent.job_digest,
    )
    .is_err()
    {
        confirm_dead_or_reject_unreachable_host(&prepared, &control)?;
    }
    Ok(wait_existing_run(prepared, wait_ms))
}

#[cfg(target_os = "linux")]
fn recover_or_return_terminal_control(
    prepared: Box<Prepared>,
    control: store::RunControl,
    wait_ms: Option<u64>,
) -> Result<Invocation, Invocation> {
    if control
        .result
        .pointer("/evidence/complete")
        .and_then(Value::as_bool)
        == Some(true)
    {
        let (result, exit_code) = recover_flow_result(&prepared)
            .map_err(|error| internal_error(prepared.redactor.redact_text(&error)))?
            .ok_or_else(|| internal_error("terminal run has no published evidence".into()))?;
        return finalize(prepared, result, exit_code);
    }
    Ok(wait_existing_run(prepared, wait_ms))
}

#[cfg(target_os = "linux")]
fn recover_existing_without_control(prepared: Box<Prepared>) -> Result<Invocation, Invocation> {
    let (result, exit_code) = required_recovered_result(&prepared)?;
    finalize(prepared, result, exit_code)
}

#[cfg(target_os = "linux")]
fn required_recovered_result(prepared: &Prepared) -> Result<(Value, u8), Invocation> {
    recover_flow_result(prepared)
        .map_err(|error| internal_error(prepared.redactor.redact_text(&error)))
        .and_then(|recovered| {
            recovered.ok_or_else(|| {
                internal_error(
                    "existing browser run has no recoverable host control; it will not be restarted"
                        .into(),
                )
            })
        })
}

#[cfg(target_os = "linux")]
fn wait_existing_run(prepared: Box<Prepared>, wait_ms: Option<u64>) -> Invocation {
    let run_id = prepared.intent.run_id.clone();
    drop(prepared);
    client::wait_for_run(&run_id, wait_ms.or(Some(15_000)))
}

#[cfg(target_os = "linux")]
fn confirm_dead_or_reject_unreachable_host(
    prepared: &Prepared,
    control: &store::RunControl,
) -> Result<(), Invocation> {
    let acquired = store::try_lock_existing_run(&prepared.state_root, &prepared.intent.run_id)
        .map_err(|error| internal_error(prepared.redactor.redact_text(&error)))?;
    if let Some(lock) = acquired {
        remove_confirmed_dead_socket(&control.socket)
            .map_err(|error| internal_error(prepared.redactor.redact_text(&error)))?;
        drop(lock);
        return Ok(());
    }
    let lock_present = store::run_lock_present(&prepared.state_root, &prepared.intent.run_id)
        .map_err(|error| internal_error(prepared.redactor.redact_text(&error)))?;
    Err(internal_error(if lock_present {
        "existing run host holds its lock but failed the live IPC identity check".into()
    } else {
        "existing browser run has no death-detection lock; it will not be restarted".into()
    }))
}

#[cfg(target_os = "linux")]
fn remove_confirmed_dead_socket(path: &Path) -> Result<(), String> {
    use std::os::unix::fs::FileTypeExt;

    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_socket() => fs::remove_file(path)
            .map_err(|error| format!("cannot remove confirmed dead host socket: {error}")),
        Ok(_) => Err("confirmed dead host socket path is not an owned socket".into()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(format!(
            "cannot inspect confirmed dead host socket: {error}"
        )),
    }
}

#[cfg(target_os = "linux")]
fn validate_existing_control(
    prepared: &Prepared,
    control: &store::RunControl,
) -> Result<(), Invocation> {
    if control.ipc_version != process::IPC_VERSION
        || control.run_id != prepared.intent.run_id
        || control.request_id != prepared.intent.public_request_id
        || control.job_digest != prepared.intent.job_digest
        || control.evidence_root != prepared.intent.evidence_root
    {
        return Err(internal_error(
            "existing run control does not match the admitted request".into(),
        ));
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn start_new_background_run(
    prepared: Box<Prepared>,
    wait_ms: Option<u64>,
) -> Result<Invocation, Invocation> {
    let runtime_dir = validated_runtime_run_dir(&prepared)?;
    let (host, watchdog, lifetime) = host_bootstrap(&prepared, runtime_dir);
    let run_lock = initialize_run_basis(&prepared, &watchdog)?;
    caller_bootstrap_fault(&prepared, &watchdog.runtime_dir)?;
    process::spawn_watchdog(host, watchdog, &run_lock)
        .map_err(|error| internal_error(prepared.redactor.redact_text(&error)))?;
    drop(run_lock);
    await_host_readiness(&prepared)?;
    let run_id = prepared.intent.run_id.clone();
    drop(prepared);
    let wait = wait_ms.unwrap_or(lifetime.saturating_add(15_000));
    Ok(client::wait_for_run(&run_id, Some(wait)))
}

#[cfg(target_os = "linux")]
fn initialize_run_basis(
    prepared: &Prepared,
    watchdog: &process::WatchdogBootstrap,
) -> Result<store::RunLock, Invocation> {
    let lock = store::lock_run(&prepared.state_root, &prepared.intent.run_id)
        .map_err(|error| internal_error(prepared.redactor.redact_text(&error)))?;
    let control = store::RunControl {
        schema_version: SchemaVersion,
        ipc_version: process::IPC_VERSION,
        sequence: 0,
        run_id: prepared.intent.run_id.clone(),
        request_id: prepared.intent.public_request_id.clone(),
        job_digest: prepared.intent.job_digest.clone(),
        evidence_root: prepared.intent.evidence_root.clone(),
        started_unix_ms: watchdog.started_unix_ms,
        lifetime_deadline_unix_ms: watchdog.lifetime_deadline_unix_ms,
        pause_deadline_unix_ms: None,
        host: None,
        watchdog: None,
        socket: watchdog.runtime_dir.join("control.sock"),
        result: watchdog.initial_result.clone(),
    };
    store::write_run_control(&prepared.state_root, &control)
        .map_err(|error| internal_error(prepared.redactor.redact_text(&error)))?;
    Ok(lock)
}

#[cfg(all(target_os = "linux", debug_assertions))]
fn caller_bootstrap_fault(prepared: &Prepared, runtime_dir: &Path) -> Result<(), Invocation> {
    match std::env::var("MANUVRA_TEST_CALLER_BOOTSTRAP_FAULT").as_deref() {
        Ok("after_run_basis") => publish_caller_fault_marker(prepared, runtime_dir),
        _ => Ok(()),
    }
}

#[cfg(all(target_os = "linux", debug_assertions))]
fn publish_caller_fault_marker(prepared: &Prepared, runtime_dir: &Path) -> Result<(), Invocation> {
    store::atomic_write_private(
        &runtime_dir.join("caller-bootstrap-fault.ready"),
        b"ready\n",
    )
    .map_err(|error| internal_error(prepared.redactor.redact_text(&error)))?;
    park_caller_forever()
}

#[cfg(all(target_os = "linux", debug_assertions))]
fn park_caller_forever() -> ! {
    loop {
        std::thread::park();
    }
}

#[cfg(all(target_os = "linux", not(debug_assertions)))]
fn caller_bootstrap_fault(_: &Prepared, _: &Path) -> Result<(), Invocation> {
    Ok(())
}

#[cfg(target_os = "linux")]
fn validated_runtime_run_dir(prepared: &Prepared) -> Result<PathBuf, Invocation> {
    let runtime_root = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .ok_or_else(|| internal_error("XDG_RUNTIME_DIR is required for background runs".into()))?;
    reject_sensitive_internal_path(&runtime_root, &prepared.redactor, "runtime")?;
    process::runtime_run_dir(&prepared.intent.run_id)
        .map_err(|error| internal_error(prepared.redactor.redact_text(&error)))
}

#[cfg(target_os = "linux")]
fn host_bootstrap(
    prepared: &Prepared,
    runtime_dir: PathBuf,
) -> (process::HostBootstrap, process::WatchdogBootstrap, u64) {
    let started = process::now_unix_ms();
    let lifetime = u64::from(prepared.job.options.lifetime_ms.unwrap_or(900_000));
    let pause = u64::from(prepared.job.options.pause_timeout_ms.unwrap_or(300_000));
    let host = process::HostBootstrap {
        job: prepared.job.clone(),
        intent: prepared.intent.clone(),
        lookup_request_id: prepared.lookup_request_id.clone(),
        state_root: prepared.state_root.clone(),
        browser: prepared.browser.clone(),
        headless: prepared.headless,
        provider_key: std::env::var("TYPESAFE_API_KEY")
            .ok()
            .filter(|value| !value.is_empty()),
        runtime_dir: runtime_dir.clone(),
        started_unix_ms: started,
        lifetime_deadline_unix_ms: started.saturating_add(lifetime),
        pause_timeout_ms: pause,
        watchdog: None,
        liveness_fd: None,
    };
    let watchdog = process::WatchdogBootstrap {
        intent: prepared.intent.clone(),
        state_root: prepared.state_root.clone(),
        runtime_dir: runtime_dir.clone(),
        started_unix_ms: started,
        lifetime_deadline_unix_ms: started.saturating_add(lifetime),
        initial_result: initial_watchdog_result(prepared),
        watchdog: None,
    };
    (host, watchdog, lifetime)
}

#[cfg(target_os = "linux")]
fn initial_watchdog_result(prepared: &Prepared) -> Value {
    let manifest = prepared
        .intent
        .evidence_root
        .join(&prepared.intent.run_id)
        .join("manifest.json");
    json!({
        "schema_version":1,
        "request_id":prepared.intent.public_request_id,
        "run_id":prepared.intent.run_id,
        "state":"running",
        "terminal":false,
        "reason":null,
        "verdict":{
            "overall":"unresolved",
            "steps":prepared.job.steps.iter().enumerate().map(|(index,step)|json!({
                "id":prepared.redactor.redact_export_text(&step.id),
                "result":if index == 0 {"unresolved"} else {"not_run"}
            })).collect::<Vec<_>>(),
            "expectations":prepared.job.expectations.iter().map(|expectation|json!({
                "id":prepared.redactor.redact_export_text(&expectation.id),
                "result":"not_run",
                "numeric_checks":[]
            })).collect::<Vec<_>>(),
            "caller_assisted":false
        },
        "evidence":{"manifest":manifest,"complete":false},
        "escalation":null,
        "cleanup":{"browser":"not_started","profile":"not_created","application_state":"caller_owned"}
    })
}

#[cfg(target_os = "linux")]
fn await_host_readiness(prepared: &Prepared) -> Result<(), Invocation> {
    let readiness_deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        if host_is_ready(prepared)? {
            return Ok(());
        }
        if std::time::Instant::now() >= readiness_deadline {
            return Err(internal_error(
                "run host did not complete the IPC readiness handshake".into(),
            ));
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
}

#[cfg(target_os = "linux")]
fn host_is_ready(prepared: &Prepared) -> Result<bool, Invocation> {
    let Some(control) = store::read_run_control(&prepared.state_root, &prepared.intent.run_id)
        .map_err(|error| internal_error(prepared.redactor.redact_text(&error)))?
    else {
        return Ok(false);
    };
    validate_readiness_control(prepared, &control)?;
    if control.result.get("terminal").and_then(Value::as_bool) == Some(true) {
        return Ok(true);
    }
    Ok(client::request_host_ready(
        &control.socket,
        &prepared.intent.run_id,
        &prepared.intent.job_digest,
    )
    .is_ok())
}

#[cfg(target_os = "linux")]
fn validate_readiness_control(
    prepared: &Prepared,
    control: &store::RunControl,
) -> Result<(), Invocation> {
    if control.ipc_version != process::IPC_VERSION {
        return Err(internal_error(
            "run host IPC version is incompatible".into(),
        ));
    }
    if control.run_id != prepared.intent.run_id || control.job_digest != prepared.intent.job_digest
    {
        return Err(internal_error(
            "run host readiness control does not match the admitted run".into(),
        ));
    }
    Ok(())
}

fn recover_flow_result(prepared: &Prepared) -> Result<Option<(Value, u8)>, String> {
    let run_dir = prepared.intent.evidence_root.join(&prepared.intent.run_id);
    let Some(run_dir) = existing_run_directory(run_dir)? else {
        return Ok(None);
    };
    let manifest_path = run_dir.join("manifest.json");
    let result = validate_published_evidence(
        &manifest_path,
        &prepared.intent.run_id,
        &prepared.intent.public_request_id,
        None,
    )?;
    let exit_code = result_exit_code(&result)?;
    Ok(Some((result, exit_code)))
}

fn existing_run_directory(path: PathBuf) -> Result<Option<PathBuf>, String> {
    let metadata = match fs::symlink_metadata(&path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(format!("cannot inspect prior evidence: {error}")),
    };
    (!metadata.file_type().is_symlink() && metadata.is_dir())
        .then_some(Some(path))
        .ok_or_else(|| "prior evidence path is not a regular directory".into())
}

fn read_prior_manifest(path: &Path, run_id: &str) -> Result<Manifest, String> {
    require_regular_file(path)?;
    let bytes = fs::read(path).map_err(|error| format!("cannot read prior manifest: {error}"))?;
    let manifest: Manifest = serde_json::from_slice(&bytes)
        .map_err(|error| format!("cannot parse prior manifest: {error}"))?;
    (manifest.run_id == run_id && manifest.complete)
        .then_some(manifest)
        .ok_or_else(|| "prior manifest is incomplete or has the wrong run id".into())
}

fn validate_published_evidence(
    manifest_path: &Path,
    run_id: &str,
    request_id: &str,
    recorded_result: Option<&Value>,
) -> Result<Value, String> {
    validate_published_result(manifest_path, run_id, request_id, recorded_result, true)
}

pub(crate) fn validate_published_result(
    manifest_path: &Path,
    run_id: &str,
    request_id: &str,
    recorded_result: Option<&Value>,
    require_terminal: bool,
) -> Result<Value, String> {
    let manifest_path = canonical_regular_file(manifest_path)?;
    let run_dir = manifest_path
        .parent()
        .ok_or_else(|| "prior manifest has no evidence directory".to_owned())?;
    let manifest = read_prior_manifest(&manifest_path, run_id)?;
    let artifacts = validate_manifest_artifacts(run_dir, &manifest)?;
    let result = read_published_result(&artifacts)?;
    validate_evidence_shape(&manifest, &result, require_terminal)?;
    validate_result_identity(&result, run_id, request_id, &manifest_path)?;
    recorded_result
        .is_none_or(|recorded| recorded == &result)
        .then_some(result)
        .ok_or_else(|| "completed request record does not match published result".into())
}

fn validate_evidence_shape(
    manifest: &Manifest,
    result: &Value,
    require_terminal: bool,
) -> Result<(), String> {
    validate_terminal_shape(result, require_terminal)?;
    validate_flow_artifact_shape(manifest, result)?;
    validate_verification_artifact_shape(manifest, result)?;
    validate_passed_verdict_shape(result)
}

fn validate_terminal_shape(result: &Value, require_terminal: bool) -> Result<(), String> {
    if require_terminal && result.get("terminal").and_then(Value::as_bool) != Some(true) {
        return Err("complete evidence does not contain a terminal result".into());
    }
    Ok(())
}

fn validate_flow_artifact_shape(manifest: &Manifest, result: &Value) -> Result<(), String> {
    let short = manifest.artifacts.len() == 2;
    let admission_reason = result
        .pointer("/reason/code")
        .and_then(Value::as_str)
        .is_some_and(|code| matches!(code, "missing_value" | "unsupported_in_this_build"));
    let admission_cleanup = result.pointer("/cleanup/browser").and_then(Value::as_str)
        == Some("not_started")
        && result.pointer("/cleanup/profile").and_then(Value::as_str) == Some("not_created");
    if short && !(admission_reason && admission_cleanup) {
        return Err("prior result is missing required flow artifacts".into());
    }
    Ok(())
}

fn validate_verification_artifact_shape(manifest: &Manifest, result: &Value) -> Result<(), String> {
    let verification_count = manifest
        .artifacts
        .iter()
        .filter(|artifact| artifact.role == "verification")
        .count();
    if verification_count > 1 {
        return Err("prior manifest repeats singleton role verification".into());
    }
    let expectations_evaluated = result
        .pointer("/verdict/expectations")
        .and_then(Value::as_array)
        .is_some_and(|expectations| {
            expectations.iter().any(|expectation| {
                expectation.get("result").and_then(Value::as_str) != Some("not_run")
            })
        });
    let verification_phase =
        result.pointer("/escalation/phase").and_then(Value::as_str) == Some("verification");
    let passed = result.get("state").and_then(Value::as_str) == Some("passed");
    if (passed || expectations_evaluated || verification_phase) && verification_count != 1 {
        return Err("prior result is missing its final verification artifact".into());
    }
    Ok(())
}

fn validate_passed_verdict_shape(result: &Value) -> Result<(), String> {
    if result.get("state").and_then(Value::as_str) != Some("passed") {
        return Ok(());
    }
    let overall_satisfied =
        result.pointer("/verdict/overall").and_then(Value::as_str) == Some("satisfied");
    let all_satisfied = ["/verdict/steps", "/verdict/expectations"]
        .into_iter()
        .all(|pointer| verdict_array_is_satisfied(result, pointer));
    if !overall_satisfied || !all_satisfied {
        return Err("passed result contains an incomplete verdict".into());
    }
    Ok(())
}

fn verdict_array_is_satisfied(result: &Value, pointer: &str) -> bool {
    result
        .pointer(pointer)
        .and_then(Value::as_array)
        .is_some_and(|verdicts| {
            verdicts
                .iter()
                .all(|verdict| verdict.get("result").and_then(Value::as_str) == Some("satisfied"))
        })
}

fn read_published_result(artifacts: &BTreeMap<String, PathBuf>) -> Result<Value, String> {
    let path = artifacts
        .get("result")
        .ok_or_else(|| "prior manifest has no result artifact".to_owned())?;
    let bytes = fs::read(path).map_err(|error| format!("cannot read prior result: {error}"))?;
    serde_json::from_slice(&bytes).map_err(|error| format!("cannot parse prior result: {error}"))
}

fn validate_manifest_artifacts(
    run_dir: &Path,
    manifest: &Manifest,
) -> Result<BTreeMap<String, PathBuf>, String> {
    let (by_role, listed_paths) = collect_manifest_artifacts(run_dir, manifest)?;
    require_roles(&by_role, &["normalized_job", "result"])?;
    require_full_run_roles(&by_role, manifest)?;
    let actual_paths = evidence_files(run_dir, &run_dir.join("manifest.json"))?;
    validate_evidence_file_set(&actual_paths, &listed_paths)?;
    Ok(by_role)
}

fn collect_manifest_artifacts(
    run_dir: &Path,
    manifest: &Manifest,
) -> Result<(BTreeMap<String, PathBuf>, BTreeSet<PathBuf>), String> {
    let mut by_role = BTreeMap::new();
    let mut listed_paths = BTreeSet::new();
    for artifact in &manifest.artifacts {
        let path = validate_manifest_artifact(run_dir, artifact)?;
        insert_manifest_path(&mut listed_paths, &path)?;
        record_singleton_role(&mut by_role, artifact, path)?;
    }
    Ok((by_role, listed_paths))
}

fn validate_evidence_file_set(
    actual: &BTreeSet<PathBuf>,
    listed: &BTreeSet<PathBuf>,
) -> Result<(), String> {
    (actual == listed)
        .then_some(())
        .ok_or_else(|| "prior evidence directory contains unmanifested or missing artifacts".into())
}

fn insert_manifest_path(paths: &mut BTreeSet<PathBuf>, path: &Path) -> Result<(), String> {
    paths
        .insert(path.to_path_buf())
        .then_some(())
        .ok_or_else(|| "prior manifest lists an artifact path more than once".into())
}

fn require_full_run_roles(
    by_role: &BTreeMap<String, PathBuf>,
    manifest: &Manifest,
) -> Result<(), String> {
    let full_run = manifest
        .artifacts
        .iter()
        .any(|artifact| !matches!(artifact.role.as_str(), "normalized_job" | "result"));
    full_run
        .then(|| require_roles(by_role, &["provenance", "trace", "cleanup"]))
        .transpose()
        .map(|_| ())
}

fn validate_manifest_artifact(
    run_dir: &Path,
    artifact: &manuvra_contract::Artifact,
) -> Result<PathBuf, String> {
    use sha2::{Digest, Sha256};

    validate_artifact_header(artifact)?;
    let recorded_path = Path::new(&artifact.path);
    let path = canonical_regular_file(recorded_path)?;
    validate_artifact_location(run_dir, recorded_path, &path)?;
    let relative = path
        .strip_prefix(run_dir)
        .map_err(|_| "prior manifest artifact escapes its run directory")?;
    role_matches_path(&artifact.role, relative)
        .then_some(())
        .ok_or_else(|| {
            format!(
                "prior manifest role {} has an unsafe or unexpected path",
                artifact.role
            )
        })?;
    let bytes =
        fs::read(&path).map_err(|error| format!("cannot read prior evidence artifact: {error}"))?;
    (hex::encode(Sha256::digest(&bytes)) == artifact.digest)
        .then_some(path)
        .ok_or_else(|| "prior evidence artifact digest does not match its manifest".into())
}

fn validate_artifact_header(artifact: &manuvra_contract::Artifact) -> Result<(), String> {
    (artifact.complete && valid_digest(&artifact.digest))
        .then_some(())
        .ok_or_else(|| "prior manifest contains an incomplete or invalid artifact".into())
}

fn validate_artifact_location(
    run_dir: &Path,
    recorded: &Path,
    canonical: &Path,
) -> Result<(), String> {
    (recorded == canonical && canonical.starts_with(run_dir))
        .then_some(())
        .ok_or_else(|| "prior manifest artifact path is not an exact in-run path".into())
}

fn record_singleton_role(
    by_role: &mut BTreeMap<String, PathBuf>,
    artifact: &manuvra_contract::Artifact,
    path: PathBuf,
) -> Result<(), String> {
    const SINGLETONS: &[&str] = &[
        "normalized_job",
        "result",
        "provenance",
        "trace",
        "cleanup",
        "verification",
    ];
    if !SINGLETONS.contains(&artifact.role.as_str()) {
        return Ok(());
    }
    by_role
        .insert(artifact.role.clone(), path)
        .is_none()
        .then_some(())
        .ok_or_else(|| format!("prior manifest repeats singleton role {}", artifact.role))
}

fn require_roles(by_role: &BTreeMap<String, PathBuf>, roles: &[&str]) -> Result<(), String> {
    roles
        .iter()
        .find(|role| !by_role.contains_key(**role))
        .map_or(Ok(()), |role| {
            Err(format!("prior manifest has no {role} artifact"))
        })
}

fn evidence_files(run_dir: &Path, manifest: &Path) -> Result<BTreeSet<PathBuf>, String> {
    let mut pending = vec![run_dir.to_path_buf()];
    let mut files = BTreeSet::new();
    while let Some(directory) = pending.pop() {
        for entry in fs::read_dir(&directory)
            .map_err(|error| format!("cannot inspect prior evidence directory: {error}"))?
        {
            let path = entry
                .map_err(|error| format!("cannot inspect prior evidence entry: {error}"))?
                .path();
            route_evidence_entry(evidence_entry(path)?, &mut pending, &mut files);
        }
    }
    files.remove(manifest);
    Ok(files)
}

fn route_evidence_entry(
    entry: EvidenceEntry,
    pending: &mut Vec<PathBuf>,
    files: &mut BTreeSet<PathBuf>,
) {
    match entry {
        EvidenceEntry::Directory(path) => pending.push(path),
        EvidenceEntry::File(path) => {
            files.insert(path);
        }
    }
}

enum EvidenceEntry {
    Directory(PathBuf),
    File(PathBuf),
}

fn evidence_entry(path: PathBuf) -> Result<EvidenceEntry, String> {
    let metadata = fs::symlink_metadata(&path)
        .map_err(|error| format!("cannot inspect prior evidence entry: {error}"))?;
    if metadata.file_type().is_symlink() {
        return Err("prior evidence contains a symlink".into());
    }
    if metadata.is_dir() {
        return Ok(EvidenceEntry::Directory(path));
    }
    if !metadata.is_file() {
        return Err("prior evidence contains a non-regular entry".into());
    }
    path.canonicalize()
        .map(EvidenceEntry::File)
        .map_err(|error| format!("cannot resolve prior evidence entry: {error}"))
}

fn role_matches_path(role: &str, path: &Path) -> bool {
    let text = path.to_string_lossy();
    const EXACT: &[(&str, &str)] = &[
        ("normalized_job", "job.json"),
        ("result", "result.json"),
        ("provenance", "provenance.json"),
        ("trace", "trace.jsonl"),
        ("cleanup", "cleanup.json"),
        ("verification", "verification/final.json"),
    ];
    if let Some((_, wanted)) = EXACT.iter().find(|(candidate, _)| candidate == &role) {
        return text == *wanted;
    }
    const NESTED: &[(&str, &str, &str)] = &[
        ("observation", "observations/", ".json"),
        ("screenshot", "observations/", ".png"),
        ("decision", "decisions/", ".json"),
        ("step", "steps/", ".json"),
        ("escalation", "escalations/", ".json"),
        ("disposition", "dispositions/", ".json"),
    ];
    NESTED
        .iter()
        .find(|(candidate, _, _)| candidate == &role)
        .is_some_and(|(_, prefix, suffix)| safe_artifact_leaf(&text, prefix, suffix))
}

fn safe_artifact_leaf(path: &str, prefix: &str, suffix: &str) -> bool {
    path.strip_prefix(prefix)
        .and_then(|leaf| leaf.strip_suffix(suffix))
        .is_some_and(|leaf| {
            !leaf.is_empty()
                && leaf
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
        })
}

fn valid_digest(digest: &str) -> bool {
    digest.len() == 64
        && digest
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn canonical_regular_file(path: &Path) -> Result<PathBuf, String> {
    require_regular_file(path)?;
    path.canonicalize()
        .map_err(|error| format!("cannot resolve prior evidence file: {error}"))
}

fn require_regular_file(path: &Path) -> Result<(), String> {
    let metadata = fs::symlink_metadata(path)
        .map_err(|error| format!("cannot inspect prior evidence file: {error}"))?;
    if metadata.is_file() && !metadata.file_type().is_symlink() {
        Ok(())
    } else {
        Err("prior evidence contains a non-regular file".into())
    }
}

fn validate_result_identity(
    result: &Value,
    run_id: &str,
    request_id: &str,
    manifest_path: &Path,
) -> Result<(), String> {
    let matches = result.get("schema_version").and_then(Value::as_u64) == Some(1)
        && result.get("run_id").and_then(Value::as_str) == Some(run_id)
        && result.get("request_id").and_then(Value::as_str) == Some(request_id)
        && result
            .pointer("/evidence/complete")
            .and_then(Value::as_bool)
            == Some(true)
        && result.pointer("/evidence/manifest").and_then(Value::as_str) == manifest_path.to_str();
    matches
        .then_some(())
        .ok_or_else(|| "prior result identity or evidence reference is inconsistent".into())
}

pub(crate) fn result_exit_code(result: &Value) -> Result<u8, String> {
    const STATES: &[(&str, u8)] = &[
        ("passed", 0),
        ("uncertain", 2),
        ("blocked", 3),
        ("failed", 4),
        ("aborted", 5),
        ("expired", 5),
        ("running", 6),
    ];
    let state = result.get("state").and_then(Value::as_str);
    STATES
        .iter()
        .find_map(|(name, code)| (Some(*name) == state).then_some(*code))
        .ok_or_else(|| "prior result has an invalid state".into())
}

fn publish_blocked(
    prepared: Box<Prepared>,
    stop: evidence::BlockedStop,
) -> Result<Invocation, Invocation> {
    let published = evidence::publish(
        &prepared.intent.evidence_root,
        &prepared.intent.public_request_id,
        &prepared.intent.run_id,
        &prepared.job,
        stop,
        &prepared.redactor,
    )
    .map_err(|error| internal_error(prepared.redactor.redact_text(&error)))?;
    finalize(prepared, published.result, EXIT_BLOCKED)
}

fn finalize(
    prepared: Box<Prepared>,
    result: Value,
    exit_code: u8,
) -> Result<Invocation, Invocation> {
    let record = store::RequestRecord {
        schema_version: SchemaVersion,
        public_request_id: prepared.intent.public_request_id.clone(),
        run_id: prepared.intent.run_id.clone(),
        job_digest: prepared.intent.job_digest.clone(),
        result_digest: None,
        exit_code,
        result: result.clone(),
    };
    store::finalize_request(&prepared.state_root, &prepared.lookup_request_id, &record)
        .map_err(|error| internal_error(prepared.redactor.redact_text(&error)))?;
    Ok(Invocation {
        output: result,
        exit_code,
    })
}

fn load_job(path: &Path) -> Result<Job, Invocation> {
    let bytes = fs::read(path).map_err(|error| {
        Invocation::error(
            "invalid_input",
            format!("cannot read job {}: {error}", path.display()),
            EXIT_INVALID,
        )
    })?;
    Job::parse(&bytes).map_err(|_| Invocation::error("invalid_job", "job is invalid", EXIT_INVALID))
}

enum RequestLookup {
    Missing,
    Intent(store::RequestIntent),
    Complete(Invocation),
}

fn lookup_request(
    state_root: &Path,
    request_id: &str,
    digest: &str,
    redactor: &evidence::Redactor,
) -> Result<RequestLookup, Invocation> {
    let record = store::lookup_request(state_root, request_id)
        .map_err(|error| internal_error(redactor.redact_text(&error)))?;
    match record {
        Some(store::RequestEntry::Complete(record)) if record.job_digest == digest => {
            validate_completed_request(record, redactor).map(RequestLookup::Complete)
        }
        Some(store::RequestEntry::Intent(mut intent)) if intent.job_digest == digest => {
            intent.public_request_id = redactor.redact_export_text(&intent.public_request_id);
            Ok(RequestLookup::Intent(intent))
        }
        Some(_) => Err(Invocation::error(
            "request_conflict",
            "request_id was already used for a different job",
            EXIT_INVALID,
        )),
        None => Ok(RequestLookup::Missing),
    }
}

fn validate_completed_request(
    record: store::RequestRecord,
    redactor: &evidence::Redactor,
) -> Result<Invocation, Invocation> {
    let manifest = record
        .result
        .pointer("/evidence/manifest")
        .and_then(Value::as_str)
        .ok_or_else(|| internal_error("completed request has no evidence manifest".into()))?;
    let result = validate_published_evidence(
        Path::new(manifest),
        &record.run_id,
        &record.public_request_id,
        Some(&record.result),
    )
    .map_err(|error| internal_error(redactor.redact_text(&error)))?;
    let exit_code =
        result_exit_code(&result).map_err(|error| internal_error(redactor.redact_text(&error)))?;
    (exit_code == record.exit_code)
        .then_some(Invocation {
            exit_code,
            output: result,
        })
        .ok_or_else(|| {
            internal_error("completed request exit code does not match its published result".into())
        })
}

fn validate_request_id(request_id: &str) -> Result<(), String> {
    if request_id.is_empty() || request_id.len() > 128 {
        return Err("request_id must contain 1 to 128 characters".into());
    }
    if request_id.chars().any(char::is_control) {
        return Err("request_id must not contain control characters".into());
    }
    Ok(())
}

#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub(crate) fn validate_run_id(run_id: &str) -> Result<(), String> {
    let suffix = run_id.strip_prefix("r_").ok_or_else(|| {
        "run_id must use the generated r_ followed by 16 ASCII alphanumeric characters format"
            .to_owned()
    })?;
    (suffix.len() == 16 && suffix.bytes().all(|byte| byte.is_ascii_alphanumeric()))
        .then_some(())
        .ok_or_else(|| {
            "run_id must use the generated r_ followed by 16 ASCII alphanumeric characters format"
                .to_owned()
        })
}

fn canonical_digest(
    state_root: &Path,
    job: &Job,
    browser: Option<&Path>,
    headless: bool,
    redactor: &evidence::Redactor,
) -> Result<String, Invocation> {
    let bytes = serde_json::to_vec(&json!({
        "job": job,
        "browser": browser,
        "headless": headless,
    }))
    .map_err(|error| internal_error(error.to_string()))?;
    store::keyed_digest(state_root, store::DOMAIN_RUN_JOB, &bytes)
        .map_err(|error| internal_error(redactor.redact_text(&error)))
}

pub(crate) fn internal_error(message: String) -> Invocation {
    Invocation::error("internal", message, EXIT_INTERNAL)
}

#[cfg(test)]
mod boundary_tests {
    #[cfg(target_os = "linux")]
    use super::remove_confirmed_dead_socket;
    use super::{role_matches_path, validate_evidence_shape};
    use manuvra_contract::{Artifact, Manifest, SchemaVersion};
    use serde_json::json;
    #[cfg(target_os = "linux")]
    use std::os::unix::net::UnixListener;
    #[cfg(target_os = "linux")]
    use tempfile::TempDir;

    #[cfg(target_os = "linux")]
    #[test]
    fn confirmed_dead_socket_removal_is_bounded_to_sockets() {
        let temporary = TempDir::new().unwrap();
        let absent = temporary.path().join("absent.sock");
        assert!(remove_confirmed_dead_socket(&absent).is_ok());

        let socket = temporary.path().join("owned.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        assert!(remove_confirmed_dead_socket(&socket).is_ok());
        assert!(!socket.exists());
        drop(listener);

        let ordinary = temporary.path().join("ordinary");
        std::fs::write(&ordinary, b"not a socket").unwrap();
        assert!(remove_confirmed_dead_socket(&ordinary).is_err());
        assert_eq!(std::fs::read(&ordinary).unwrap(), b"not a socket");
    }

    #[test]
    fn completed_resume_checkpoint_may_be_nonterminal_but_product_may_not() {
        let artifact = |role: &str| Artifact {
            role: role.into(),
            path: format!("/e/{role}.json"),
            digest: "0".repeat(64),
            complete: true,
        };
        let manifest = Manifest {
            schema_version: SchemaVersion,
            run_id: "r_checkpoint".into(),
            complete: true,
            artifacts: vec![
                artifact("normalized_job"),
                artifact("result"),
                artifact("trace"),
            ],
        };
        let checkpoint = json!({"terminal":false,"state":"uncertain"});
        assert!(validate_evidence_shape(&manifest, &checkpoint, false).is_ok());
        assert!(validate_evidence_shape(&manifest, &checkpoint, true).is_err());
    }

    #[test]
    fn final_verification_role_has_one_exact_safe_path() {
        assert!(role_matches_path(
            "verification",
            std::path::Path::new("verification/final.json")
        ));
        assert!(!role_matches_path(
            "verification",
            std::path::Path::new("verification/other.json")
        ));
        assert!(!role_matches_path(
            "verification",
            std::path::Path::new("../verification/final.json")
        ));
    }

    #[test]
    fn passed_recovery_requires_one_verification_artifact_and_satisfied_verdicts() {
        let artifact = |role: &str| Artifact {
            role: role.into(),
            path: format!("/e/{role}.json"),
            digest: "0".repeat(64),
            complete: true,
        };
        let mut manifest = Manifest {
            schema_version: SchemaVersion,
            run_id: "r_passed".into(),
            complete: true,
            artifacts: vec![
                artifact("normalized_job"),
                artifact("result"),
                artifact("provenance"),
                artifact("trace"),
                artifact("cleanup"),
            ],
        };
        let passed = json!({
            "terminal":true,
            "state":"passed",
            "verdict":{
                "overall":"satisfied",
                "steps":[{"result":"satisfied"}],
                "expectations":[{"result":"satisfied"}]
            }
        });
        assert!(validate_evidence_shape(&manifest, &passed, true).is_err());
        manifest.artifacts.push(artifact("verification"));
        assert!(validate_evidence_shape(&manifest, &passed, true).is_ok());
        manifest.artifacts.push(artifact("verification"));
        assert!(validate_evidence_shape(&manifest, &passed, true).is_err());

        manifest.artifacts.pop();
        let mut unresolved = passed.clone();
        unresolved["verdict"]["expectations"][0]["result"] = json!("unresolved");
        assert!(validate_evidence_shape(&manifest, &unresolved, true).is_err());

        let mut incomplete_step = passed.clone();
        incomplete_step["verdict"]["steps"][0]["result"] = json!("not_satisfied");
        assert!(validate_evidence_shape(&manifest, &incomplete_step, true).is_err());

        let mut incomplete_overall = passed;
        incomplete_overall["verdict"]["overall"] = json!("unresolved");
        assert!(validate_evidence_shape(&manifest, &incomplete_overall, true).is_err());
    }
}
