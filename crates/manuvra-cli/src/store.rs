use std::env;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
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

pub fn keyed_digest(root: &Path, input: &[u8]) -> Result<String, String> {
    create_private_dir(root)?;
    let _lock = lock_file(&root.join(".digest-key.lock"), "digest key")?;
    let key_path = root.join("digest.key");
    let key = load_or_create_digest_key(root, &key_path)?;
    let mut digest = <Hmac<Sha256> as Mac>::new_from_slice(&key)
        .map_err(|error| format!("cannot initialize request digest: {error}"))?;
    digest.update(input);
    Ok(hex::encode(digest.finalize().into_bytes()))
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
    options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
}

#[cfg(not(unix))]
fn set_file_creation_options(_options: &mut OpenOptions) {}

#[cfg(unix)]
fn set_no_follow(options: &mut OpenOptions) {
    use std::os::unix::fs::OpenOptionsExt;
    options.custom_flags(libc::O_NOFOLLOW);
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
