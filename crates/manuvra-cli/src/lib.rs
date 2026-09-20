mod evidence;
mod store;

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
        #[arg(long)]
        browser: Option<PathBuf>,
        #[arg(long)]
        headless: bool,
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

impl Command {
    fn execute(self) -> Invocation {
        match self {
            Self::Run {
                request_id,
                job,
                evidence,
                browser,
                headless,
            } => run(&request_id, &job, &evidence, browser.as_deref(), headless),
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

fn run(
    request_id: &str,
    job_path: &Path,
    evidence_root: &Path,
    browser: Option<&Path>,
    headless: bool,
) -> Invocation {
    try_run(request_id, job_path, evidence_root, browser, headless).unwrap_or_else(|error| error)
}

fn try_run(
    request_id: &str,
    job_path: &Path,
    evidence_root: &Path,
    browser: Option<&Path>,
    headless: bool,
) -> Result<Invocation, Invocation> {
    if evidence_root.to_str().is_none() {
        return Err(Invocation::error(
            "invalid_input",
            "evidence path must be valid UTF-8",
            EXIT_INVALID,
        ));
    }
    match prepare_run(request_id, job_path, evidence_root, browser, headless)? {
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
    lookup_request_id: String,
    redactor: evidence::Redactor,
    state_root: PathBuf,
    _request_lock: store::RequestLock,
    browser: Option<PathBuf>,
    headless: bool,
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
    let redactor = evidence::Redactor::for_job(&job).map_err(internal_error)?;
    let state_root =
        store::state_root().map_err(|error| internal_error(redactor.redact_text(&error)))?;
    let request_lock = store::lock_request(&state_root, request_id)
        .map_err(|error| internal_error(redactor.redact_text(&error)))?;
    let browser = effective_browser_selection(browser);
    let digest = canonical_digest(&state_root, &job, browser.as_deref(), headless, &redactor)?;
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
        browser.as_deref(),
        headless,
    )
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
        lookup_request_id: request_id.to_owned(),
        redactor: admission.redactor,
        state_root: admission.state_root,
        _request_lock: admission.request_lock,
        browser: browser.map(Path::to_path_buf),
        headless,
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

fn publish_new_run(prepared: Box<Prepared>) -> Result<Invocation, Invocation> {
    if let Some((result, exit_code)) = recover_flow_result(&prepared)
        .map_err(|error| internal_error(prepared.redactor.redact_text(&error)))?
    {
        return finalize(prepared, result, exit_code);
    }
    if let Some(missing) = prepared.job.first_missing_value() {
        return publish_blocked(
            prepared,
            evidence::BlockedStop::missing_value(missing.value_name, missing.step_id),
        );
    }
    if let Some(feature) = prepared.job.first_unsupported_feature() {
        return publish_blocked(prepared, evidence::BlockedStop::unsupported(feature));
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
    let manifest_path = canonical_regular_file(manifest_path)?;
    let run_dir = manifest_path
        .parent()
        .ok_or_else(|| "prior manifest has no evidence directory".to_owned())?;
    let manifest = read_prior_manifest(&manifest_path, run_id)?;
    let artifacts = validate_manifest_artifacts(run_dir, &manifest)?;
    let result = read_published_result(&artifacts)?;
    validate_evidence_shape(&manifest, &result)?;
    validate_result_identity(&result, run_id, request_id, &manifest_path)?;
    recorded_result
        .is_none_or(|recorded| recorded == &result)
        .then_some(result)
        .ok_or_else(|| "completed request record does not match published result".into())
}

fn validate_evidence_shape(manifest: &Manifest, result: &Value) -> Result<(), String> {
    let short = manifest.artifacts.len() == 2;
    let admission_reason = result
        .pointer("/reason/code")
        .and_then(Value::as_str)
        .is_some_and(|code| matches!(code, "missing_value" | "unsupported_in_this_build"));
    let admission_cleanup = result.pointer("/cleanup/browser").and_then(Value::as_str)
        == Some("not_started")
        && result.pointer("/cleanup/profile").and_then(Value::as_str) == Some("not_created");
    (!short || (admission_reason && admission_cleanup))
        .then_some(())
        .ok_or_else(|| "prior result is missing required flow artifacts".into())
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
    const SINGLETONS: &[&str] = &["normalized_job", "result", "provenance", "trace", "cleanup"];
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

fn result_exit_code(result: &Value) -> Result<u8, String> {
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
    store::keyed_digest(state_root, &bytes)
        .map_err(|error| internal_error(redactor.redact_text(&error)))
}

fn internal_error(message: String) -> Invocation {
    Invocation::error("internal", message, EXIT_INTERNAL)
}
