mod client;
#[cfg(any(target_os = "linux", target_os = "macos"))]
mod control_socket;
mod evidence;
#[cfg(any(target_os = "linux", target_os = "macos"))]
mod host;
#[cfg(any(target_os = "linux", target_os = "macos"))]
mod process;
mod recovery;
#[cfg(any(target_os = "linux", target_os = "macos"))]
mod runtime;
#[cfg(any(target_os = "linux", target_os = "macos"))]
mod socket_auth;
mod store;
#[cfg(any(target_os = "linux", target_os = "macos"))]
mod watchdog;

use recovery::{result_exit_code, validate_published_evidence};
use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};

use clap::{ArgGroup, Parser, Subcommand, ValueEnum};
use manuvra_contract::{Job, SchemaKind, SchemaVersion, schema};
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
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        "__host" => Some(host::main().map_or(EXIT_INTERNAL, |()| EXIT_PASSED)),
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        "__watchdog" => Some(watchdog::main().map_or(EXIT_INTERNAL, |()| EXIT_PASSED)),
        #[cfg(not(any(target_os = "linux", target_os = "macos")))]
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
    let runtime_root = admitted_runtime_root()?;
    if evidence_root.to_str().is_none() {
        return Err(Invocation::error(
            "invalid_input",
            "evidence path must be valid UTF-8",
            EXIT_INVALID,
        ));
    }
    let request = RunRequest {
        request_id,
        job_path,
        evidence_root,
        runtime_root: runtime_root.as_deref(),
        browser,
        headless,
    };
    match prepare_run(&request)? {
        PreparedRun::Existing(invocation) => Ok(invocation),
        PreparedRun::New(prepared) => publish_new_run(prepared, wait_ms),
    }
}

struct RunRequest<'a> {
    request_id: &'a str,
    job_path: &'a Path,
    evidence_root: &'a Path,
    runtime_root: Option<&'a Path>,
    browser: Option<&'a Path>,
    headless: bool,
}

/// Resolves the private runtime directory before any state, evidence, or child exists, so a
/// caller can fix its environment and retry the same request id.
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn admitted_runtime_root() -> Result<Option<PathBuf>, Invocation> {
    runtime::runtime_root().map(Some).map_err(|message| {
        Invocation::error("runtime_directory_unavailable", message, EXIT_BLOCKED)
    })
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn admitted_runtime_root() -> Result<Option<PathBuf>, Invocation> {
    Ok(None)
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
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    existing_intent: bool,
}

fn prepare_run(request: &RunRequest<'_>) -> Result<PreparedRun, Invocation> {
    validate_request_id(request.request_id)
        .map_err(|message| Invocation::error("invalid_request_id", message, EXIT_INVALID))?;
    let job = load_job(request.job_path)?;
    let (admission, existing, browser) = admit_run(request, job)?;
    finish_preparation(
        request.request_id,
        request.evidence_root,
        admission,
        existing,
        browser.as_deref(),
        request.headless,
    )
}

fn admit_run(
    request: &RunRequest<'_>,
    job: Job,
) -> Result<(Admission, RequestLookup, Option<PathBuf>), Invocation> {
    let redactor = evidence::Redactor::for_job(&job).map_err(internal_error)?;
    let state_root =
        store::state_root().map_err(|error| internal_error(redactor.redact_text(&error)))?;
    reject_sensitive_internal_path(&state_root, &redactor, "state")?;
    request.runtime_root.map_or(Ok(()), |root| {
        reject_sensitive_internal_path(root, &redactor, "runtime")
    })?;
    let request_lock = store::lock_request(&state_root, request.request_id)
        .map_err(|error| internal_error(redactor.redact_text(&error)))?;
    let browser = effective_browser_selection(request.browser);
    let digest = canonical_digest(
        &state_root,
        &job,
        browser.as_deref(),
        request.headless,
        &redactor,
    )?;
    let existing = lookup_request(&state_root, request.request_id, &digest, &redactor)?;
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
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
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
        #[cfg(any(target_os = "linux", target_os = "macos"))]
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

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn publish_executable_run(
    prepared: Box<Prepared>,
    wait_ms: Option<u64>,
) -> Result<Invocation, Invocation> {
    publish_hosted_run(prepared, wait_ms)
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn publish_hosted_run(
    prepared: Box<Prepared>,
    wait_ms: Option<u64>,
) -> Result<Invocation, Invocation> {
    if prepared.existing_intent {
        return attach_or_recover_background_run(prepared, wait_ms);
    }
    start_background_run(prepared, wait_ms)
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
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

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn start_background_run(
    prepared: Box<Prepared>,
    wait_ms: Option<u64>,
) -> Result<Invocation, Invocation> {
    start_new_background_run(prepared, wait_ms)
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn attach_or_recover_background_run(
    prepared: Box<Prepared>,
    wait_ms: Option<u64>,
) -> Result<Invocation, Invocation> {
    match existing_control_after_bootstrap(&prepared)? {
        Some(control) => attach_to_existing_control(prepared, control, wait_ms),
        None => recover_existing_without_control(prepared),
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
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

#[cfg(any(target_os = "linux", target_os = "macos"))]
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

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn reconcile_abandoned_bootstrap_locked(
    prepared: &Prepared,
) -> Result<Option<store::RunControl>, Invocation> {
    store::read_run_control(&prepared.state_root, &prepared.intent.run_id)
        .map_err(|error| internal_error(prepared.redactor.redact_text(&error)))?
        .map(|control| reconcile_abandoned_control(prepared, control))
        .transpose()
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
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

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn bootstrap_has_no_owner(control: &store::RunControl) -> bool {
    control.host.is_none() && control.watchdog.is_none()
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn bootstrap_socket_is_absent(prepared: &Prepared, socket: &Path) -> Result<bool, Invocation> {
    match fs::symlink_metadata(socket) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(true),
        Ok(_) => Ok(false),
        Err(error) => Err(internal_error(prepared.redactor.redact_text(&format!(
            "cannot inspect bootstrapping run socket: {error}"
        )))),
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
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

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn control_is_bootstrapping(control: &store::RunControl) -> bool {
    control.result.get("terminal").and_then(Value::as_bool) != Some(true) && control.host.is_none()
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn attach_to_existing_control(
    prepared: Box<Prepared>,
    control: store::RunControl,
    wait_ms: Option<u64>,
) -> Result<Invocation, Invocation> {
    validate_existing_control(&prepared, &control)?;
    if control.result.get("terminal").and_then(Value::as_bool) == Some(true) {
        return recover_or_return_terminal_control(prepared, control, wait_ms);
    }
    confirm_existing_host_reachable(&prepared, &control)?;
    Ok(wait_existing_run(prepared, wait_ms))
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn confirm_existing_host_reachable(
    prepared: &Prepared,
    control: &store::RunControl,
) -> Result<(), Invocation> {
    if client::request_host_ready(
        &control.socket,
        &prepared.intent.run_id,
        &prepared.intent.job_digest,
    )
    .is_ok()
    {
        Ok(())
    } else {
        confirm_dead_or_reject_unreachable_host(prepared, control)
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
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

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn recover_existing_without_control(prepared: Box<Prepared>) -> Result<Invocation, Invocation> {
    let (result, exit_code) = required_recovered_result(&prepared)?;
    finalize(prepared, result, exit_code)
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
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

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn wait_existing_run(prepared: Box<Prepared>, wait_ms: Option<u64>) -> Invocation {
    let run_id = prepared.intent.run_id.clone();
    drop(prepared);
    client::wait_for_run(&run_id, wait_ms.or(Some(15_000)))
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn confirm_dead_or_reject_unreachable_host(
    prepared: &Prepared,
    control: &store::RunControl,
) -> Result<(), Invocation> {
    store::try_lock_existing_run(&prepared.state_root, &prepared.intent.run_id)
        .map_err(|error| internal_error(prepared.redactor.redact_text(&error)))
        .and_then(|acquired| finish_dead_host_confirmation(prepared, control, acquired))
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn finish_dead_host_confirmation(
    prepared: &Prepared,
    control: &store::RunControl,
    acquired: Option<store::RunLock>,
) -> Result<(), Invocation> {
    if let Some(lock) = acquired {
        return remove_dead_host_socket(prepared, control, lock);
    }
    reject_unreachable_host(prepared)
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn remove_dead_host_socket(
    prepared: &Prepared,
    control: &store::RunControl,
    lock: store::RunLock,
) -> Result<(), Invocation> {
    runtime::sweep_recorded_run(&control.socket, &control.run_id)
        .map_err(|error| internal_error(prepared.redactor.redact_text(&error)))?;
    drop(lock);
    Ok(())
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn reject_unreachable_host(prepared: &Prepared) -> Result<(), Invocation> {
    store::run_lock_present(&prepared.state_root, &prepared.intent.run_id)
        .map_err(|error| internal_error(prepared.redactor.redact_text(&error)))
        .and_then(|lock_present| Err(internal_error(unreachable_host_message(lock_present))))
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn unreachable_host_message(lock_present: bool) -> String {
    if lock_present {
        "existing run host holds its lock but failed the live IPC identity check".into()
    } else {
        "existing browser run has no death-detection lock; it will not be restarted".into()
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
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

#[cfg(any(target_os = "linux", target_os = "macos"))]
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

#[cfg(any(target_os = "linux", target_os = "macos"))]
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

#[cfg(all(any(target_os = "linux", target_os = "macos"), debug_assertions))]
fn caller_bootstrap_fault(prepared: &Prepared, runtime_dir: &Path) -> Result<(), Invocation> {
    match std::env::var("MANUVRA_TEST_CALLER_BOOTSTRAP_FAULT").as_deref() {
        Ok("after_run_basis") => publish_caller_fault_marker(prepared, runtime_dir),
        _ => Ok(()),
    }
}

#[cfg(all(any(target_os = "linux", target_os = "macos"), debug_assertions))]
fn publish_caller_fault_marker(prepared: &Prepared, runtime_dir: &Path) -> Result<(), Invocation> {
    store::atomic_write_private(
        &runtime_dir.join("caller-bootstrap-fault.ready"),
        b"ready\n",
    )
    .map_err(|error| internal_error(prepared.redactor.redact_text(&error)))?;
    park_caller_forever()
}

#[cfg(all(any(target_os = "linux", target_os = "macos"), debug_assertions))]
fn park_caller_forever() -> ! {
    loop {
        std::thread::park();
    }
}

#[cfg(all(any(target_os = "linux", target_os = "macos"), not(debug_assertions)))]
fn caller_bootstrap_fault(_: &Prepared, _: &Path) -> Result<(), Invocation> {
    Ok(())
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn validated_runtime_run_dir(prepared: &Prepared) -> Result<PathBuf, Invocation> {
    process::runtime_run_dir(&prepared.intent.run_id)
        .map_err(|error| internal_error(prepared.redactor.redact_text(&error)))
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
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

#[cfg(any(target_os = "linux", target_os = "macos"))]
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

#[cfg(any(target_os = "linux", target_os = "macos"))]
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

#[cfg(any(target_os = "linux", target_os = "macos"))]
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

#[cfg(any(target_os = "linux", target_os = "macos"))]
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
    recovery::recover_published_run(
        &prepared.intent.evidence_root,
        &prepared.intent.run_id,
        &prepared.intent.public_request_id,
    )
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
