use std::env;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
#[cfg(target_os = "linux")]
use std::os::fd::{AsRawFd, FromRawFd, RawFd};
use std::path::{Path, PathBuf};

use hmac::{Hmac, Mac};
use manuvra_contract::SchemaVersion;
use rand::RngCore;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RequestIntent {
    pub schema_version: SchemaVersion,
    #[serde(rename = "request_id")]
    pub public_request_id: String,
    pub run_id: String,
    pub job_digest: String,
    pub evidence_root: PathBuf,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RequestRecord {
    pub schema_version: SchemaVersion,
    #[serde(rename = "request_id")]
    pub public_request_id: String,
    pub run_id: String,
    pub job_digest: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result_digest: Option<String>,
    pub exit_code: u8,
    pub result: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "phase", rename_all = "snake_case")]
pub enum RequestEntry {
    Intent(RequestIntent),
    Complete(RequestRecord),
}

pub struct RequestLock {
    _file: File,
}

#[cfg(target_os = "linux")]
pub struct RunLock {
    _file: File,
}

#[cfg(target_os = "linux")]
impl RunLock {
    pub fn inherited_fd(&self) -> Result<RawFd, String> {
        let fd = self._file.as_raw_fd();
        set_fd_cloexec(fd, false)?;
        Ok(fd)
    }

    pub fn restore_cloexec(&self) -> Result<(), String> {
        set_fd_cloexec(self._file.as_raw_fd(), true)
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[derive(Debug, Clone, Serialize)]
pub struct ProcessIdentity {
    pub pid: u32,
    pub process_group: u32,
    #[serde(rename = "start_ticks")]
    pub start_marker: u64,
    pub session_id: u32,
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
impl<'de> Deserialize<'de> for ProcessIdentity {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct PersistedIdentity {
            pid: u32,
            #[serde(default)]
            process_group: Option<u32>,
            start_ticks: u64,
            session_id: u32,
        }

        let persisted = PersistedIdentity::deserialize(deserializer)?;
        Ok(Self {
            pid: persisted.pid,
            process_group: persisted.process_group.unwrap_or(persisted.pid),
            start_marker: persisted.start_ticks,
            session_id: persisted.session_id,
        })
    }
}

#[cfg(target_os = "linux")]
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RunControl {
    pub schema_version: SchemaVersion,
    pub ipc_version: u16,
    pub sequence: u64,
    pub run_id: String,
    pub request_id: String,
    pub job_digest: String,
    pub evidence_root: PathBuf,
    pub started_unix_ms: u64,
    pub lifetime_deadline_unix_ms: u64,
    pub pause_deadline_unix_ms: Option<u64>,
    pub host: Option<ProcessIdentity>,
    pub watchdog: Option<ProcessIdentity>,
    pub socket: PathBuf,
    pub result: Value,
}

pub fn state_root() -> Result<PathBuf, String> {
    if let Some(root) = env::var_os("XDG_STATE_HOME") {
        return Ok(PathBuf::from(root).join("manuvra"));
    }
    env::var_os("HOME")
        .map(PathBuf::from)
        .map(|root| root.join(".local/state/manuvra"))
        .ok_or_else(|| "neither XDG_STATE_HOME nor HOME is set".into())
}

pub fn lookup_request(root: &Path, request_id: &str) -> Result<Option<RequestEntry>, String> {
    let path = request_index_path(root, request_id);
    let bytes = match read_private_file(&path, "request record") {
        Ok(bytes) => bytes,
        Err(SecureReadError::NotFound) => return Ok(None),
        Err(SecureReadError::Invalid(message)) => return Err(message),
    };
    parse_request_entry(&path, &bytes).map(Some)
}

fn parse_request_entry(path: &Path, bytes: &[u8]) -> Result<RequestEntry, String> {
    match serde_json::from_slice(bytes) {
        Ok(entry) => Ok(entry),
        Err(tagged_error) => serde_json::from_slice(bytes)
            .map(RequestEntry::Complete)
            .map_err(|legacy_error| {
                format!(
                    "invalid request record {}: {tagged_error}; legacy form: {legacy_error}",
                    path.display()
                )
            }),
    }
}

pub fn lock_request(root: &Path, request_id: &str) -> Result<RequestLock, String> {
    create_private_dir(root)?;
    let requests_dir = root.join("requests");
    create_private_dir(&requests_dir)?;
    let path = requests_dir.join(format!("{}.lock", request_index_name(request_id)));
    lock_file(&path, "request")
}

#[cfg(target_os = "linux")]
pub fn lock_run(root: &Path, run_id: &str) -> Result<RunLock, String> {
    let directory = run_state_dir(root, run_id)?;
    let path = directory.join("run.lock");
    let mut options = OpenOptions::new();
    options.read(true).write(true).create(true);
    set_file_creation_options(&mut options);
    let file = options
        .open(&path)
        .map_err(|error| format!("cannot open run lock {}: {error}", path.display()))?;
    validate_private_file(&file, &path, "run lock")?;
    File::lock(&file).map_err(|error| format!("cannot lock run {}: {error}", path.display()))?;
    Ok(RunLock { _file: file })
}

#[cfg(target_os = "linux")]
pub fn adopt_inherited_run_lock(root: &Path, run_id: &str, fd: RawFd) -> Result<RunLock, String> {
    if fd < 0 {
        return Err("inherited run lock descriptor is invalid".into());
    }
    let file = unsafe { File::from_raw_fd(fd) };
    let path = root.join("runs").join(run_id).join("run.lock");
    validate_private_file(&file, &path, "inherited run lock")?;
    validate_inherited_lock_path(&file, &path)?;
    File::lock(&file).map_err(|error| {
        format!(
            "cannot confirm inherited run lock {}: {error}",
            path.display()
        )
    })?;
    set_fd_cloexec(file.as_raw_fd(), true)?;
    Ok(RunLock { _file: file })
}

#[cfg(target_os = "linux")]
fn validate_inherited_lock_path(file: &File, path: &Path) -> Result<(), String> {
    use std::os::unix::fs::MetadataExt;

    let descriptor = file
        .metadata()
        .map_err(|error| format!("cannot inspect inherited run lock: {error}"))?;
    let path_metadata = fs::symlink_metadata(path)
        .map_err(|error| format!("cannot inspect run lock {}: {error}", path.display()))?;
    (!path_metadata.file_type().is_symlink()
        && descriptor.dev() == path_metadata.dev()
        && descriptor.ino() == path_metadata.ino())
    .then_some(())
    .ok_or_else(|| "inherited run lock does not match durable run state".into())
}

#[cfg(target_os = "linux")]
fn set_fd_cloexec(fd: RawFd, enabled: bool) -> Result<(), String> {
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
    if flags == -1 {
        return Err(format!(
            "cannot inspect run lock descriptor: {}",
            std::io::Error::last_os_error()
        ));
    }
    let updated = if enabled {
        flags | libc::FD_CLOEXEC
    } else {
        flags & !libc::FD_CLOEXEC
    };
    if unsafe { libc::fcntl(fd, libc::F_SETFD, updated) } == -1 {
        return Err(format!(
            "cannot configure run lock inheritance: {}",
            std::io::Error::last_os_error()
        ));
    }
    Ok(())
}

#[cfg(target_os = "linux")]
pub fn try_lock_existing_run(root: &Path, run_id: &str) -> Result<Option<RunLock>, String> {
    let path = root.join("runs").join(run_id).join("run.lock");
    open_run_lock_for_inspection(&path)?
        .map(|file| try_lock_inspected_run(file, &path))
        .transpose()
        .map(Option::flatten)
}

#[cfg(target_os = "linux")]
pub fn run_lock_present(root: &Path, run_id: &str) -> Result<bool, String> {
    let path = root.join("runs").join(run_id).join("run.lock");
    open_run_lock_for_inspection(&path)?.map_or(Ok(false), |file| {
        validate_private_file(&file, &path, "run lock").map(|()| true)
    })
}

#[cfg(target_os = "linux")]
fn try_lock_inspected_run(file: File, path: &Path) -> Result<Option<RunLock>, String> {
    validate_private_file(&file, path, "run lock")?;
    match File::try_lock(&file) {
        Ok(()) => Ok(Some(RunLock { _file: file })),
        Err(std::fs::TryLockError::WouldBlock) => Ok(None),
        Err(error) => Err(format!(
            "cannot inspect run lock {}: {error}",
            path.display()
        )),
    }
}

#[cfg(target_os = "linux")]
fn open_run_lock_for_inspection(path: &Path) -> Result<Option<File>, String> {
    let mut options = OpenOptions::new();
    options.read(true).write(true);
    set_no_follow(&mut options);
    match options.open(path) {
        Ok(file) => Ok(Some(file)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(format!("cannot open run lock {}: {error}", path.display())),
    }
}

#[cfg(target_os = "linux")]
pub fn write_run_control(root: &Path, control: &RunControl) -> Result<(), String> {
    let directory = run_state_dir(root, &control.run_id)?;
    let bytes = serde_json::to_vec_pretty(control).map_err(|error| error.to_string())?;
    atomic_write_private(&directory.join("control.json"), &bytes)
}

#[cfg(target_os = "linux")]
pub fn read_run_control(root: &Path, run_id: &str) -> Result<Option<RunControl>, String> {
    let path = root.join("runs").join(run_id).join("control.json");
    match read_private_file(&path, "run control") {
        Ok(bytes) => serde_json::from_slice(&bytes)
            .map(Some)
            .map_err(|error| format!("invalid run control {}: {error}", path.display())),
        Err(SecureReadError::NotFound) => Ok(None),
        Err(SecureReadError::Invalid(message)) => Err(message),
    }
}

#[cfg(target_os = "linux")]
pub fn unresolved_action(evidence_root: &Path, run_id: &str) -> Result<bool, String> {
    let journal = evidence_root.join(format!(".{run_id}.action-journal.jsonl"));
    let bytes = match read_private_file(&journal, "action journal") {
        Ok(bytes) => bytes,
        Err(SecureReadError::NotFound) => return Ok(false),
        Err(SecureReadError::Invalid(message)) => return Err(message),
    };
    let text = String::from_utf8(bytes).map_err(|_| "action journal is not UTF-8".to_owned())?;
    text.lines().try_fold(false, |unresolved, line| {
        let value: Value = serde_json::from_str(line)
            .map_err(|error| format!("invalid action journal entry: {error}"))?;
        match value.get("event").and_then(Value::as_str) {
            Some("action_prepared") => Ok(true),
            Some("action_fact") if unresolved => Ok(false),
            Some("action_fact") => Err("action journal closes no prepared action".into()),
            _ => Ok(unresolved),
        }
    })
}

#[cfg(target_os = "linux")]
pub fn request_run_id(root: &Path, request_id: &str) -> Result<Option<String>, String> {
    Ok(lookup_request(root, request_id)?.map(|entry| match entry {
        RequestEntry::Intent(intent) => intent.run_id,
        RequestEntry::Complete(record) => record.run_id,
    }))
}

#[cfg(target_os = "linux")]
fn run_state_dir(root: &Path, run_id: &str) -> Result<PathBuf, String> {
    let runs = root.join("runs");
    create_private_dir(&runs)?;
    let directory = runs.join(run_id);
    create_private_dir(&directory)?;
    Ok(directory)
}

pub const DOMAIN_RUN_JOB: &[u8] = b"run-job";
#[cfg(target_os = "linux")]
pub const DOMAIN_RESUME_REQUEST: &[u8] = b"resume-request";
#[cfg(target_os = "linux")]
pub const DOMAIN_RESUME_RESULT: &[u8] = b"resume-result";
#[cfg(target_os = "linux")]
pub const DOMAIN_ABORT_REQUEST: &[u8] = b"abort-request";

pub fn keyed_digest(root: &Path, domain: &[u8], input: &[u8]) -> Result<String, String> {
    if domain.is_empty() || domain.contains(&0) {
        return Err("digest domain must be nonempty and contain no NUL bytes".into());
    }
    create_private_dir(root)?;
    let _lock = lock_file(&root.join(".digest-key.lock"), "digest key")?;
    let key_path = root.join("digest.key");
    let key = load_or_create_digest_key(root, &key_path)?;
    let mut digest = <Hmac<Sha256> as Mac>::new_from_slice(&key)
        .map_err(|error| format!("cannot initialize request digest: {error}"))?;
    digest.update(b"manuvra-hmac-v1\0");
    digest.update(domain);
    digest.update(&[0]);
    digest.update(input);
    Ok(hex::encode(digest.finalize().into_bytes()))
}

#[cfg(target_os = "linux")]
pub fn sealed_control_record(
    root: &Path,
    request_id: &str,
    run_id: &str,
    request_digest: &str,
    exit_code: u8,
    result: Value,
) -> Result<RequestRecord, String> {
    let result_digest =
        control_result_digest(root, request_id, run_id, request_digest, exit_code, &result)?;
    Ok(RequestRecord {
        schema_version: SchemaVersion,
        public_request_id: request_id.to_owned(),
        run_id: run_id.to_owned(),
        job_digest: request_digest.to_owned(),
        result_digest: Some(result_digest),
        exit_code,
        result,
    })
}

#[cfg(target_os = "linux")]
pub fn validate_control_record(root: &Path, record: &RequestRecord) -> Result<(), String> {
    let expected = control_result_digest(
        root,
        &record.public_request_id,
        &record.run_id,
        &record.job_digest,
        record.exit_code,
        &record.result,
    )?;
    (record.result_digest.as_deref() == Some(expected.as_str()))
        .then_some(())
        .ok_or_else(|| "completed control result digest does not match".into())
}

#[cfg(target_os = "linux")]
fn control_result_digest(
    root: &Path,
    request_id: &str,
    run_id: &str,
    request_digest: &str,
    exit_code: u8,
    result: &Value,
) -> Result<String, String> {
    let bytes = serde_json::to_vec(&serde_json::json!({
        "request_id":request_id,
        "run_id":run_id,
        "request_digest":request_digest,
        "exit_code":exit_code,
        "result":result,
    }))
    .map_err(|error| error.to_string())?;
    keyed_digest(root, DOMAIN_RESUME_RESULT, &bytes)
}

fn load_or_create_digest_key(root: &Path, key_path: &Path) -> Result<Vec<u8>, String> {
    let key = match read_private_file(key_path, "digest key") {
        Ok(key) => key,
        Err(SecureReadError::NotFound) => {
            if request_history_exists(root)? {
                return Err(format!(
                    "digest key {} is missing while request history exists; internal state is corrupt",
                    key_path.display()
                ));
            }
            let mut key = [0_u8; 32];
            rand::rng().fill_bytes(&mut key);
            atomic_write_private(key_path, &key)?;
            key.to_vec()
        }
        Err(SecureReadError::Invalid(message)) => return Err(message),
    };
    validate_digest_key(key_path, &key)?;
    Ok(key)
}

fn request_history_exists(root: &Path) -> Result<bool, String> {
    directory_has_record(&root.join("requests"), false).and_then(|requests| {
        if requests {
            Ok(true)
        } else {
            directory_has_record(&root.join("runs"), true)
        }
    })
}

fn directory_has_record(path: &Path, nested: bool) -> Result<bool, String> {
    let Some(metadata) = existing_metadata(path, "request history")? else {
        return Ok(false);
    };
    validate_owned_directory(path, &metadata)?;
    set_dir_mode(path)?;
    history_paths(path)?
        .into_iter()
        .try_fold(false, |found, entry| {
            Ok(found || is_request_record(&entry, nested))
        })
}

fn history_paths(path: &Path) -> Result<Vec<PathBuf>, String> {
    fs::read_dir(path)
        .map_err(|error| format!("cannot inspect request history {}: {error}", path.display()))?
        .map(|entry| {
            entry.map(|item| item.path()).map_err(|error| {
                format!("cannot inspect request history {}: {error}", path.display())
            })
        })
        .collect()
}

fn is_request_record(path: &Path, nested: bool) -> bool {
    if nested {
        path.join("request.json").exists()
    } else {
        path.extension()
            .is_some_and(|extension| extension == "json")
    }
}

fn validate_digest_key(key_path: &Path, key: &[u8]) -> Result<(), String> {
    (key.len() == 32).then_some(()).ok_or_else(|| {
        format!(
            "digest key {} must contain exactly 32 bytes",
            key_path.display()
        )
    })
}

fn lock_file(path: &Path, purpose: &str) -> Result<RequestLock, String> {
    let mut options = OpenOptions::new();
    options.read(true).write(true).create(true);
    set_file_creation_options(&mut options);
    let file = options
        .open(path)
        .map_err(|error| format!("cannot open {purpose} lock {}: {error}", path.display()))?;
    validate_private_file(&file, path, purpose)?;
    File::lock(&file)
        .map_err(|error| format!("cannot lock {purpose} {}: {error}", path.display()))?;
    Ok(RequestLock { _file: file })
}

pub fn record_intent(
    root: &Path,
    lookup_request_id: &str,
    intent: &RequestIntent,
) -> Result<(), String> {
    let bytes = tagged_bytes(&RequestEntry::Intent(intent.clone()))?;
    let requests_dir = root.join("requests");
    create_private_dir(&requests_dir)?;
    atomic_write_private(&request_index_path(root, lookup_request_id), &bytes)?;
    write_run_entry(root, &intent.run_id, &bytes)
}

#[cfg(target_os = "linux")]
pub fn record_control_intent(
    root: &Path,
    lookup_request_id: &str,
    intent: &RequestIntent,
) -> Result<(), String> {
    let bytes = tagged_bytes(&RequestEntry::Intent(intent.clone()))?;
    let requests_dir = root.join("requests");
    create_private_dir(&requests_dir)?;
    atomic_write_private(&request_index_path(root, lookup_request_id), &bytes)
}

pub fn finalize_request(
    root: &Path,
    lookup_request_id: &str,
    record: &RequestRecord,
) -> Result<(), String> {
    let bytes = tagged_bytes(&RequestEntry::Complete(record.clone()))?;
    write_run_entry(root, &record.run_id, &bytes)?;
    let requests_dir = root.join("requests");
    create_private_dir(&requests_dir)?;
    atomic_write_private(&request_index_path(root, lookup_request_id), &bytes)
}

#[cfg(target_os = "linux")]
pub fn finalize_control_request(
    root: &Path,
    lookup_request_id: &str,
    record: &RequestRecord,
) -> Result<(), String> {
    tagged_bytes(&RequestEntry::Complete(record.clone())).and_then(|bytes| {
        let requests_dir = root.join("requests");
        create_private_dir(&requests_dir).and_then(|()| {
            atomic_write_private(&request_index_path(root, lookup_request_id), &bytes)
        })
    })
}

fn tagged_bytes(entry: &RequestEntry) -> Result<Vec<u8>, String> {
    serde_json::to_vec_pretty(entry).map_err(|error| error.to_string())
}

fn write_run_entry(root: &Path, run_id: &str, bytes: &[u8]) -> Result<(), String> {
    let runs_dir = root.join("runs");
    create_private_dir(&runs_dir)?;
    let run_dir = runs_dir.join(run_id);
    create_private_dir(&run_dir)?;
    atomic_write_private(&run_dir.join("request.json"), bytes)
}

fn request_index_path(root: &Path, request_id: &str) -> PathBuf {
    root.join("requests")
        .join(format!("{}.json", request_index_name(request_id)))
}

fn request_index_name(request_id: &str) -> String {
    hex::encode(Sha256::digest(request_id.as_bytes()))
}

pub fn create_private_dir(path: &Path) -> Result<(), String> {
    ensure_directory(path)?;
    let metadata = required_metadata(path, "private directory")?;
    validate_owned_directory(path, &metadata)?;
    set_dir_mode(path)
}

fn ensure_directory(path: &Path) -> Result<(), String> {
    if existing_metadata(path, "private directory")?.is_none() {
        fs::create_dir_all(path).map_err(|error| {
            format!(
                "cannot create private directory {}: {error}",
                path.display()
            )
        })?;
    }
    Ok(())
}

fn existing_metadata(path: &Path, purpose: &str) -> Result<Option<fs::Metadata>, String> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => Ok(Some(metadata)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(format!(
            "cannot inspect {purpose} {}: {error}",
            path.display()
        )),
    }
}

fn required_metadata(path: &Path, purpose: &str) -> Result<fs::Metadata, String> {
    existing_metadata(path, purpose)?
        .ok_or_else(|| format!("{purpose} {} does not exist", path.display()))
}

pub fn atomic_write_private(path: &Path, contents: &[u8]) -> Result<(), String> {
    let parent = path
        .parent()
        .ok_or_else(|| format!("path has no parent: {}", path.display()))?;
    create_private_dir(parent)?;
    validate_existing_destination(path)?;
    let temporary = private_temporary_path(parent, path.file_name().unwrap_or_default());
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    set_file_creation_options(&mut options);
    let file = options
        .open(&temporary)
        .map_err(|error| format!("cannot create {}: {error}", temporary.display()))?;
    validate_private_file(&file, &temporary, "temporary state file")?;
    write_temporary(file, &temporary, contents)?;
    publish_temporary(&temporary, path)?;
    sync_directory(parent)
}

fn write_temporary(mut file: File, temporary: &Path, contents: &[u8]) -> Result<(), String> {
    file.write_all(contents)
        .and_then(|()| file.sync_all())
        .map_err(|error| {
            let _ = fs::remove_file(temporary);
            format!("cannot write {}: {error}", temporary.display())
        })
}

fn publish_temporary(temporary: &Path, path: &Path) -> Result<(), String> {
    fs::rename(temporary, path).map_err(|error| {
        let _ = fs::remove_file(temporary);
        format!(
            "cannot publish {} as {}: {error}",
            temporary.display(),
            path.display()
        )
    })
}

fn private_temporary_path(parent: &Path, file_name: &std::ffi::OsStr) -> PathBuf {
    let mut suffix = [0_u8; 8];
    rand::rng().fill_bytes(&mut suffix);
    parent.join(format!(
        ".{}.{}.tmp",
        file_name.to_string_lossy(),
        hex::encode(suffix)
    ))
}

fn validate_existing_destination(path: &Path) -> Result<(), String> {
    match open_existing_private(path, "state file") {
        Ok(_) | Err(SecureReadError::NotFound) => Ok(()),
        Err(SecureReadError::Invalid(message)) => Err(message),
    }
}

enum SecureReadError {
    NotFound,
    Invalid(String),
}

impl SecureReadError {
    fn message(self, path: &Path, purpose: &str) -> String {
        if let Self::Invalid(message) = self {
            return message;
        }
        format!("{purpose} {} does not exist", path.display())
    }
}

fn read_private_file(path: &Path, purpose: &str) -> Result<Vec<u8>, SecureReadError> {
    let mut file = open_existing_private(path, purpose)?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes).map_err(|error| {
        SecureReadError::Invalid(format!("cannot read {purpose} {}: {error}", path.display()))
    })?;
    Ok(bytes)
}

pub fn read_private(path: &Path, purpose: &str) -> Result<Vec<u8>, String> {
    read_private_file(path, purpose).map_err(|error| error.message(path, purpose))
}

fn open_existing_private(path: &Path, purpose: &str) -> Result<File, SecureReadError> {
    let mut options = OpenOptions::new();
    options.read(true);
    set_no_follow(&mut options);
    let file = options.open(path).map_err(|error| {
        if error.kind() == std::io::ErrorKind::NotFound {
            SecureReadError::NotFound
        } else {
            SecureReadError::Invalid(format!("cannot open {purpose} {}: {error}", path.display()))
        }
    })?;
    validate_private_file(&file, path, purpose).map_err(SecureReadError::Invalid)?;
    Ok(file)
}

#[cfg(unix)]
fn set_file_creation_options(options: &mut OpenOptions) {
    use std::os::unix::fs::OpenOptionsExt;
    options
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC);
}

#[cfg(not(unix))]
fn set_file_creation_options(_options: &mut OpenOptions) {}

#[cfg(unix)]
fn set_no_follow(options: &mut OpenOptions) {
    use std::os::unix::fs::OpenOptionsExt;
    options.custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC);
}

#[cfg(not(unix))]
fn set_no_follow(_options: &mut OpenOptions) {}

#[cfg(unix)]
fn validate_private_file(file: &File, path: &Path, purpose: &str) -> Result<(), String> {
    use std::os::unix::fs::MetadataExt;
    let metadata = file
        .metadata()
        .map_err(|error| format!("cannot inspect {purpose} {}: {error}", path.display()))?;
    if !metadata.file_type().is_file() {
        return Err(format!(
            "{purpose} {} is not a regular file",
            path.display()
        ));
    }
    if metadata.uid() != unsafe { libc::geteuid() } {
        return Err(format!(
            "{purpose} {} is not owned by this user",
            path.display()
        ));
    }
    if metadata.mode() & 0o7777 != 0o600 {
        return Err(format!("{purpose} {} must have mode 0600", path.display()));
    }
    Ok(())
}

#[cfg(not(unix))]
fn validate_private_file(file: &File, path: &Path, purpose: &str) -> Result<(), String> {
    let metadata = file
        .metadata()
        .map_err(|error| format!("cannot inspect {purpose} {}: {error}", path.display()))?;
    if metadata.is_file() {
        Ok(())
    } else {
        Err(format!(
            "{purpose} {} is not a regular file",
            path.display()
        ))
    }
}

#[cfg(unix)]
fn validate_owned_directory(path: &Path, metadata: &fs::Metadata) -> Result<(), String> {
    use std::os::unix::fs::MetadataExt;
    if !metadata.file_type().is_dir() {
        return Err(format!(
            "private path {} is not a directory",
            path.display()
        ));
    }
    if metadata.uid() != unsafe { libc::geteuid() } {
        return Err(format!(
            "private directory {} is not owned by this user",
            path.display()
        ));
    }
    Ok(())
}

#[cfg(not(unix))]
fn validate_owned_directory(path: &Path, metadata: &fs::Metadata) -> Result<(), String> {
    if metadata.is_dir() {
        Ok(())
    } else {
        Err(format!(
            "private path {} is not a directory",
            path.display()
        ))
    }
}

#[cfg(unix)]
fn set_dir_mode(path: &Path) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))
        .map_err(|error| format!("cannot protect directory {}: {error}", path.display()))
}

#[cfg(not(unix))]
fn set_dir_mode(_path: &Path) -> Result<(), String> {
    Ok(())
}

fn sync_directory(path: &Path) -> Result<(), String> {
    File::open(path)
        .and_then(|directory| directory.sync_all())
        .map_err(|error| format!("cannot sync directory {}: {error}", path.display()))
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn keyed_digests_are_explicitly_domain_separated() {
        let temporary = TempDir::new().unwrap();
        let payload = b"same canonical bytes";
        let run = keyed_digest(temporary.path(), DOMAIN_RUN_JOB, payload).unwrap();
        let resume = keyed_digest(temporary.path(), DOMAIN_RESUME_REQUEST, payload).unwrap();
        let result = keyed_digest(temporary.path(), DOMAIN_RESUME_RESULT, payload).unwrap();
        let abort = keyed_digest(temporary.path(), DOMAIN_ABORT_REQUEST, payload).unwrap();
        let unique = std::collections::BTreeSet::from([run, resume, result, abort]);
        assert_eq!(unique.len(), 4);
        assert!(keyed_digest(temporary.path(), b"", payload).is_err());
        assert!(keyed_digest(temporary.path(), b"bad\0domain", payload).is_err());
    }

    #[test]
    fn control_intent_indexes_request_without_replacing_run_owner_record() {
        let temporary = TempDir::new().unwrap();
        let root = temporary.path();
        let intent = RequestIntent {
            schema_version: SchemaVersion,
            public_request_id: "resume-request".into(),
            run_id: "r_resume".into(),
            job_digest: "digest".into(),
            evidence_root: root.join("evidence"),
        };
        record_control_intent(root, "resume-request", &intent).unwrap();
        assert!(matches!(
            lookup_request(root, "resume-request").unwrap(),
            Some(RequestEntry::Intent(found)) if found.run_id == "r_resume"
        ));
        assert!(!root.join("runs/r_resume/request.json").exists());
    }
}

#[cfg(all(test, any(target_os = "linux", target_os = "macos")))]
mod process_identity_tests {
    use super::ProcessIdentity;

    #[test]
    fn legacy_identity_derives_the_owned_group_from_its_leader() {
        let identity: ProcessIdentity =
            serde_json::from_str(r#"{"pid":42,"start_ticks":700,"session_id":9}"#).unwrap();
        assert_eq!(identity.process_group, 42);
        assert_eq!(serde_json::to_value(identity).unwrap()["process_group"], 42);
    }
}
