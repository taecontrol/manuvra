mod evidence;
mod store;

use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};

use clap::{ArgGroup, Parser, Subcommand, ValueEnum};
use manuvra_contract::{Job, SchemaKind, SchemaVersion, schema};
use serde_json::{Value, json};

const EXIT_PASSED: u8 = 0;
const EXIT_BLOCKED: u8 = 3;
const EXIT_INVALID: u8 = 64;
const EXIT_INTERNAL: u8 = 70;

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
        Self {
            output: json!({
                "schema_version": 1,
                "error": {"code": code, "message": message.into()}
            }),
            exit_code,
        }
    }
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

impl Command {
    fn execute(self) -> Invocation {
        match self {
            Self::Run {
                request_id,
                job,
                evidence,
            } => run(&request_id, &job, &evidence),
            Self::Schema { kind } => Invocation::success(schema(kind.into())),
            Self::Version => Invocation::success(json!({
                "schema_version": 1,
                "version": env!("CARGO_PKG_VERSION")
            })),
            Self::Resume { .. } => not_implemented("resume"),
            Self::Status { .. } => not_implemented("status"),
            Self::Abort { .. } => not_implemented("abort"),
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

fn not_implemented(command: &str) -> Invocation {
    Invocation::error(
        "not_implemented",
        format!("{command} is not implemented in this build"),
        EXIT_INVALID,
    )
}

fn run(request_id: &str, job_path: &Path, evidence_root: &Path) -> Invocation {
    try_run(request_id, job_path, evidence_root).unwrap_or_else(|error| error)
}

fn try_run(
    request_id: &str,
    job_path: &Path,
    evidence_root: &Path,
) -> Result<Invocation, Invocation> {
    if evidence_root.to_str().is_none() {
        return Err(Invocation::error(
            "invalid_input",
            "evidence path must be valid UTF-8",
            EXIT_INVALID,
        ));
    }
    match prepare_run(request_id, job_path, evidence_root)? {
        PreparedRun::Existing(invocation) => Ok(invocation),
        PreparedRun::New(prepared) => publish_new_run(prepared),
    }
}

enum PreparedRun {
    Existing(Invocation),
    New(Box<Prepared>),
}

struct Prepared {
    job: Job,
    intent: store::RequestIntent,
    redactor: evidence::Redactor,
    state_root: PathBuf,
    _request_lock: store::RequestLock,
}

fn prepare_run(
    request_id: &str,
    job_path: &Path,
    evidence_root: &Path,
) -> Result<PreparedRun, Invocation> {
    validate_request_id(request_id)
        .map_err(|message| Invocation::error("invalid_request_id", message, EXIT_INVALID))?;
    let job = load_job(job_path)?;
    let redactor = evidence::Redactor::for_job(&job).map_err(internal_error)?;
    let state_root =
        store::state_root().map_err(|error| internal_error(redactor.redact_text(&error)))?;
    let request_lock = store::lock_request(&state_root, request_id)
        .map_err(|error| internal_error(redactor.redact_text(&error)))?;
    let digest = canonical_digest(&state_root, &job, &redactor)?;
    let existing = lookup_request(&state_root, request_id, &digest, &redactor)?;
    finish_preparation(
        request_id,
        evidence_root,
        Admission {
            job,
            redactor,
            state_root,
            request_lock,
            digest,
        },
        existing,
    )
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
) -> Result<PreparedRun, Invocation> {
    let intent = match existing {
        RequestLookup::Complete(invocation) => return Ok(PreparedRun::Existing(invocation)),
        RequestLookup::Intent(intent) => intent,
        RequestLookup::Missing => create_intent(
            request_id,
            evidence_root,
            &admission.redactor,
            &admission.state_root,
            admission.digest,
        )?,
    };
    Ok(PreparedRun::New(Box::new(Prepared {
        job: admission.job,
        intent,
        redactor: admission.redactor,
        state_root: admission.state_root,
        _request_lock: admission.request_lock,
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
        request_id: request_id.to_owned(),
        run_id: evidence::new_run_id(),
        job_digest: digest,
        evidence_root: absolute_evidence,
    };
    store::record_intent(state_root, &intent)
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

fn publish_new_run(prepared: Box<Prepared>) -> Result<Invocation, Invocation> {
    let stop = if let Some(missing) = prepared.job.first_missing_value() {
        evidence::BlockedStop::missing_value(missing.value_name, missing.step_id)
    } else {
        evidence::BlockedStop::unsupported(prepared.job.first_unsupported_feature())
    };
    let published = evidence::publish(
        &prepared.intent.evidence_root,
        &prepared.intent.request_id,
        &prepared.intent.run_id,
        &prepared.job,
        stop,
        &prepared.redactor,
    )
    .map_err(|error| internal_error(prepared.redactor.redact_text(&error)))?;
    let record = store::RequestRecord {
        schema_version: SchemaVersion,
        request_id: prepared.intent.request_id.clone(),
        run_id: prepared.intent.run_id.clone(),
        job_digest: prepared.intent.job_digest.clone(),
        exit_code: EXIT_BLOCKED,
        result: published.result.clone(),
    };
    store::finalize_request(&prepared.state_root, &record)
        .map_err(|error| internal_error(prepared.redactor.redact_text(&error)))?;
    Ok(Invocation {
        output: published.result,
        exit_code: EXIT_BLOCKED,
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
            Ok(RequestLookup::Complete(Invocation {
                exit_code: record.exit_code,
                output: record.result,
            }))
        }
        Some(store::RequestEntry::Intent(intent)) if intent.job_digest == digest => {
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

fn validate_request_id(request_id: &str) -> Result<(), String> {
    if request_id.is_empty() || request_id.len() > 128 {
        return Err("request_id must contain 1 to 128 characters".into());
    }
    if request_id.chars().any(char::is_control) {
        return Err("request_id must not contain control characters".into());
    }
    Ok(())
}

fn canonical_digest(
    state_root: &Path,
    job: &Job,
    redactor: &evidence::Redactor,
) -> Result<String, Invocation> {
    let bytes = serde_json::to_vec(job).map_err(|error| internal_error(error.to_string()))?;
    store::keyed_digest(state_root, &bytes)
        .map_err(|error| internal_error(redactor.redact_text(&error)))
}

fn internal_error(message: String) -> Invocation {
    Invocation::error("internal", message, EXIT_INTERNAL)
}
