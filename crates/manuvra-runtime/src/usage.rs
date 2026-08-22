use crate::util::{atomic_write, ensure_private_directory, opaque_id, unix_mode};
use manuvra_protocol::sha256_hex;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::fs;
use std::io;
use std::io::Write;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
#[cfg(test)]
use std::sync::atomic::{AtomicBool, Ordering};
use thiserror::Error;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct UsageConfig {
    schema_version: u64,
    raw_usage_enabled: bool,
}

impl Default for UsageConfig {
    fn default() -> Self {
        Self {
            schema_version: 1,
            raw_usage_enabled: false,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct UsageAggregate {
    pub schema_version: u64,
    pub counters: Vec<UsageCounter>,
}

impl Default for UsageAggregate {
    fn default() -> Self {
        Self {
            schema_version: 1,
            counters: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct UsageCounter {
    pub backend: String,
    pub operation: String,
    pub intent: Option<String>,
    pub outcome: String,
    pub count: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct UsageActionResult {
    pub action: String,
    pub enabled: bool,
    pub usage_path: String,
    pub export_path: Option<String>,
    pub warning: Option<String>,
}

#[derive(Debug, Clone)]
pub struct UsageKey<'a> {
    pub backend: &'a str,
    pub operation: &'a str,
    pub intent: Option<&'a str>,
    pub outcome: &'a str,
}

#[derive(Debug)]
pub struct UsageStore {
    root: PathBuf,
    lock: Mutex<()>,
    #[cfg(test)]
    fail_next_write: AtomicBool,
}

impl UsageStore {
    pub fn new(root: PathBuf) -> Result<Self, UsageError> {
        ensure_private_directory(&root)?;
        Ok(Self {
            root,
            lock: Mutex::new(()),
            #[cfg(test)]
            fail_next_write: AtomicBool::new(false),
        })
    }

    pub fn manage(
        &self,
        action: &str,
        destination: Option<&Path>,
    ) -> Result<UsageActionResult, UsageError> {
        let _guard = self
            .lock
            .lock()
            .map_err(|_| UsageError::Unavailable("usage lock poisoned".to_owned()))?;
        self.apply_usage_action(action, destination)
    }

    fn apply_usage_action(
        &self,
        action: &str,
        destination: Option<&Path>,
    ) -> Result<UsageActionResult, UsageError> {
        match action {
            "enable" => self.set_enabled(true, action),
            "disable" => self.set_enabled(false, action),
            "show" => self.show(),
            "reset" => self.reset(),
            "export" => self.export_action(destination),
            _ => Err(UsageError::Invalid(format!(
                "unknown usage action {action}"
            ))),
        }
    }

    fn export_action(&self, destination: Option<&Path>) -> Result<UsageActionResult, UsageError> {
        self.export(
            destination
                .ok_or_else(|| UsageError::Invalid("export destination is required".to_owned()))?,
        )
    }

    pub fn record(&self, key: UsageKey<'_>) -> Result<bool, UsageError> {
        let _guard = self
            .lock
            .lock()
            .map_err(|_| UsageError::Unavailable("usage lock poisoned".to_owned()))?;
        let config = self.load_config()?;
        if !config.raw_usage_enabled {
            return Ok(false);
        }
        validate_key(&key)?;
        let mut usage = self.load_usage(true)?;
        increment_counter(&mut usage, &key)?;
        self.write_usage(&usage)?;
        Ok(true)
    }

    pub fn usage_path(&self) -> PathBuf {
        self.root.join("usage.json")
    }

    fn set_enabled(&self, enabled: bool, action: &str) -> Result<UsageActionResult, UsageError> {
        let config = UsageConfig {
            schema_version: 1,
            raw_usage_enabled: enabled,
        };
        atomic_write(&self.config_path(), &serde_json::to_vec(&config)?)?;
        Ok(self.result(action, enabled, None, None))
    }

    fn show(&self) -> Result<UsageActionResult, UsageError> {
        let config = self.load_config()?;
        let usage = self.load_usage(true)?;
        if !self.usage_path().exists() {
            self.write_usage(&usage)?;
        }
        Ok(self.result("show", config.raw_usage_enabled, None, None))
    }

    fn reset(&self) -> Result<UsageActionResult, UsageError> {
        let config = self.load_config()?;
        self.write_usage(&UsageAggregate::default())?;
        Ok(self.result("reset", config.raw_usage_enabled, None, None))
    }

    fn export(&self, destination: &Path) -> Result<UsageActionResult, UsageError> {
        if !destination.is_absolute() {
            return Err(UsageError::Invalid(
                "usage export destination must be absolute".to_owned(),
            ));
        }
        let config = self.load_config()?;
        copy_exact_without_overwrite(destination, &self.export_bytes()?)?;
        Ok(self.result("export", config.raw_usage_enabled, Some(destination), None))
    }

    fn export_bytes(&self) -> Result<Vec<u8>, UsageError> {
        if self.usage_path().exists() {
            Ok(fs::read(self.usage_path())?)
        } else {
            Ok(serde_json::to_vec(&UsageAggregate::default())?)
        }
    }

    fn result(
        &self,
        action: &str,
        enabled: bool,
        export_path: Option<&Path>,
        warning: Option<String>,
    ) -> UsageActionResult {
        UsageActionResult {
            action: action.to_owned(),
            enabled,
            usage_path: absolute_display(&self.usage_path()),
            export_path: export_path.map(absolute_display),
            warning,
        }
    }

    fn config_path(&self) -> PathBuf {
        self.root.join("config.json")
    }

    fn load_config(&self) -> Result<UsageConfig, UsageError> {
        let path = self.config_path();
        if !path.exists() {
            return Ok(UsageConfig::default());
        }
        verify_state_file(&path)?;
        let value: Value = serde_json::from_slice(&fs::read(&path)?)
            .map_err(|error| UsageError::Corrupt(error.to_string()))?;
        let version = value.get("schema_version").and_then(Value::as_u64);
        if version != Some(1) {
            return Err(UsageError::Unsupported(version));
        }
        serde_json::from_value(value).map_err(|error| UsageError::Corrupt(error.to_string()))
    }

    fn load_usage(&self, migrate: bool) -> Result<UsageAggregate, UsageError> {
        let path = self.usage_path();
        if !path.exists() {
            return Ok(UsageAggregate::default());
        }
        verify_state_file(&path)?;
        let bytes = fs::read(&path)?;
        let value: Value = serde_json::from_slice(&bytes)
            .map_err(|error| UsageError::Corrupt(error.to_string()))?;
        match value.get("schema_version").and_then(Value::as_u64) {
            Some(1) => parse_usage(value),
            Some(0) if migrate => self.migrate_v0(value),
            version => Err(UsageError::Unsupported(version)),
        }
    }

    fn migrate_v0(&self, mut value: Value) -> Result<UsageAggregate, UsageError> {
        value["schema_version"] = Value::from(1_u64);
        let usage = parse_usage(value)?;
        self.write_usage(&usage)?;
        Ok(usage)
    }

    fn write_usage(&self, usage: &UsageAggregate) -> Result<(), UsageError> {
        validate_usage(usage)?;
        #[cfg(test)]
        if self.fail_next_write.swap(false, Ordering::SeqCst) {
            return Err(UsageError::Io(io::Error::other(
                "injected usage commit failure",
            )));
        }
        atomic_write(&self.usage_path(), &serde_json::to_vec(usage)?)?;
        Ok(())
    }
}

fn parse_usage(value: Value) -> Result<UsageAggregate, UsageError> {
    let usage: UsageAggregate =
        serde_json::from_value(value).map_err(|error| UsageError::Corrupt(error.to_string()))?;
    validate_usage(&usage)?;
    Ok(usage)
}

fn validate_usage(usage: &UsageAggregate) -> Result<(), UsageError> {
    if usage.schema_version != 1 {
        return Err(UsageError::Unsupported(Some(usage.schema_version)));
    }
    for counter in &usage.counters {
        let key = UsageKey {
            backend: &counter.backend,
            operation: &counter.operation,
            intent: counter.intent.as_deref(),
            outcome: &counter.outcome,
        };
        validate_key(&key)?;
        if counter.count == 0 {
            return Err(UsageError::Corrupt("counter must be positive".to_owned()));
        }
    }
    Ok(())
}

fn validate_key(key: &UsageKey<'_>) -> Result<(), UsageError> {
    if !matches!(key.backend, "cdp" | "ax") {
        return Err(UsageError::Corrupt("invalid backend".to_owned()));
    }
    if !valid_operation(key.operation) {
        return Err(UsageError::Corrupt("invalid operation".to_owned()));
    }
    if !matches!(key.intent, None | Some("query") | Some("action")) {
        return Err(UsageError::Corrupt("invalid intent".to_owned()));
    }
    if !matches!(key.outcome, "completed" | "not_performed" | "uncertain") {
        return Err(UsageError::Corrupt("invalid outcome".to_owned()));
    }
    Ok(())
}

fn valid_operation(operation: &str) -> bool {
    let mut chars = operation.chars();
    let first = chars.next();
    operation.len() <= 128
        && first.is_some_and(|character| character.is_ascii_alphabetic())
        && chars.all(|character| character.is_ascii_alphanumeric() || "_.:-".contains(character))
}

fn increment_counter(usage: &mut UsageAggregate, key: &UsageKey<'_>) -> Result<(), UsageError> {
    if let Some(counter) = usage
        .counters
        .iter_mut()
        .find(|counter| counter_matches(counter, key))
    {
        counter.count = counter.count.checked_add(1).ok_or(UsageError::Overflow)?;
        return Ok(());
    }
    usage.counters.push(UsageCounter {
        backend: key.backend.to_owned(),
        operation: key.operation.to_owned(),
        intent: key.intent.map(str::to_owned),
        outcome: key.outcome.to_owned(),
        count: 1,
    });
    usage.counters.sort_by(|left, right| {
        (&left.backend, &left.operation, &left.intent, &left.outcome).cmp(&(
            &right.backend,
            &right.operation,
            &right.intent,
            &right.outcome,
        ))
    });
    Ok(())
}

fn counter_matches(counter: &UsageCounter, key: &UsageKey<'_>) -> bool {
    counter.backend == key.backend
        && counter.operation == key.operation
        && counter.intent.as_deref() == key.intent
        && counter.outcome == key.outcome
}

fn verify_state_file(path: &Path) -> Result<(), UsageError> {
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.file_type().is_file() || metadata.file_type().is_symlink() {
        return Err(UsageError::Corrupt(
            "state path is not a regular file".to_owned(),
        ));
    }
    if unix_mode(path)? != 0o600 {
        return Err(UsageError::Corrupt(
            "state file mode is not 0600".to_owned(),
        ));
    }
    if metadata.uid() != unsafe { libc::geteuid() } {
        return Err(UsageError::Corrupt(
            "state file belongs to a different user".to_owned(),
        ));
    }
    Ok(())
}

fn copy_exact_without_overwrite(destination: &Path, bytes: &[u8]) -> Result<(), UsageError> {
    if let Some(parent) = destination.parent() {
        fs::create_dir_all(parent)?;
    }
    write_or_reuse_export(destination, bytes)
}

fn write_or_reuse_export(destination: &Path, bytes: &[u8]) -> Result<(), UsageError> {
    if destination.exists() {
        return reuse_usage_export(destination, bytes);
    }
    match atomic_write_export(destination, bytes) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
            reuse_usage_export(destination, bytes)
        }
        Err(error) => Err(error.into()),
    }
}

fn reuse_usage_export(destination: &Path, bytes: &[u8]) -> Result<(), UsageError> {
    let metadata = fs::symlink_metadata(destination)?;
    if !metadata.file_type().is_file() || metadata.file_type().is_symlink() {
        return Err(UsageError::Unavailable(
            "refusing to reuse a non-regular export path".to_owned(),
        ));
    }
    if sha256_hex(&fs::read(destination)?) == sha256_hex(bytes) {
        Ok(())
    } else {
        Err(UsageError::Unavailable(
            "refusing to overwrite a different export".to_owned(),
        ))
    }
}

fn atomic_write_export(destination: &Path, bytes: &[u8]) -> io::Result<()> {
    let parent = destination
        .parent()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "destination has no parent"))?;
    if !parent.exists() {
        fs::create_dir_all(parent)?;
    }
    let temporary = destination.with_file_name(format!(
        ".{}.{}.export-partial",
        destination
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("usage"),
        opaque_id("")
    ));
    let mut file = fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .mode(0o600)
        .open(&temporary)?;
    let result = write_export_and_link(&mut file, bytes, &temporary, destination, parent);
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

fn write_export_and_link(
    file: &mut fs::File,
    bytes: &[u8],
    temporary: &Path,
    destination: &Path,
    parent: &Path,
) -> io::Result<()> {
    file.write_all(bytes)?;
    file.sync_all()?;
    fs::hard_link(temporary, destination)?;
    fs::remove_file(temporary)?;
    fs::File::open(parent)?.sync_all()?;
    Ok(())
}

fn absolute_display(path: &Path) -> String {
    if path.is_absolute() {
        path.display().to_string()
    } else {
        fs::canonicalize(path)
            .unwrap_or_else(|_| path.to_path_buf())
            .display()
            .to_string()
    }
}

#[derive(Debug, Error)]
pub enum UsageError {
    #[error("usage store corrupt: {0}")]
    Corrupt(String),
    #[error("usage schema unsupported: {0:?}")]
    Unsupported(Option<u64>),
    #[error("usage count overflow")]
    Overflow,
    #[error("usage input invalid: {0}")]
    Invalid(String),
    #[error("usage store unavailable: {0}")]
    Unavailable(String),
    #[error("usage I/O: {0}")]
    Io(#[from] io::Error),
    #[error("usage JSON: {0}")]
    Json(#[from] serde_json::Error),
}

impl UsageError {
    pub fn code(&self) -> &'static str {
        match self {
            Self::Corrupt(_) => "usage_store_corrupt",
            Self::Unsupported(_) => "usage_schema_unsupported",
            Self::Invalid(_) => "invalid_request",
            Self::Overflow | Self::Unavailable(_) | Self::Io(_) | Self::Json(_) => {
                "usage_store_corrupt"
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::{PermissionsExt, symlink};
    use tempfile::tempdir;

    #[test]
    fn disabled_by_default_then_records_only_allowed_key() {
        let temporary = tempdir().unwrap();
        let store = UsageStore::new(temporary.path().join("config")).unwrap();
        let key = UsageKey {
            backend: "cdp",
            operation: "Runtime.evaluate",
            intent: Some("query"),
            outcome: "completed",
        };
        assert!(!store.record(key.clone()).unwrap());
        store.manage("enable", None).unwrap();
        assert!(store.record(key.clone()).unwrap());
        assert!(store.record(key).unwrap());
        let contents = fs::read_to_string(store.usage_path()).unwrap();
        assert!(!contents.contains("document.title"));
        let aggregate = serde_json::from_str::<UsageAggregate>(&contents).unwrap();
        assert_eq!(aggregate.counters.len(), 1);
        assert_eq!(aggregate.counters[0].count, 2);
    }

    #[test]
    fn corrupt_store_is_preserved_and_reset_recovers() {
        let temporary = tempdir().unwrap();
        let store = UsageStore::new(temporary.path().join("config")).unwrap();
        atomic_write(&store.usage_path(), b"not-json").unwrap();
        let original = fs::read(store.usage_path()).unwrap();
        assert!(matches!(
            store.manage("show", None),
            Err(UsageError::Corrupt(_))
        ));
        assert_eq!(fs::read(store.usage_path()).unwrap(), original);
        store.manage("reset", None).unwrap();
        assert_eq!(store.manage("show", None).unwrap().action, "show");
    }

    #[test]
    fn registered_v0_migrates_without_changing_counters() {
        let temporary = tempdir().unwrap();
        let store = UsageStore::new(temporary.path().join("config")).unwrap();
        let v0 = serde_json::json!({
            "schema_version": 0,
            "counters": [{
                "backend": "ax", "operation": "AXPress", "intent": null,
                "outcome": "completed", "count": 2
            }]
        });
        atomic_write(&store.usage_path(), &serde_json::to_vec(&v0).unwrap()).unwrap();
        store.manage("show", None).unwrap();
        let migrated: UsageAggregate =
            serde_json::from_slice(&fs::read(store.usage_path()).unwrap()).unwrap();
        assert_eq!(migrated.schema_version, 1);
        assert_eq!(migrated.counters[0].count, 2);
    }

    #[test]
    fn failed_registered_migration_preserves_original_v0_bytes() {
        let temporary = tempdir().unwrap();
        let store = UsageStore::new(temporary.path().join("config")).unwrap();
        let original = br#"{"schema_version":0,"counters":[{"backend":"ax","operation":"AXPress","intent":null,"outcome":"completed","count":2}]}"#;
        atomic_write(&store.usage_path(), original).unwrap();
        store.fail_next_write.store(true, Ordering::SeqCst);

        assert!(matches!(store.manage("show", None), Err(UsageError::Io(_))));
        assert_eq!(fs::read(store.usage_path()).unwrap(), original);
    }

    #[test]
    fn future_schema_is_preserved() {
        let temporary = tempdir().unwrap();
        let store = UsageStore::new(temporary.path().join("config")).unwrap();
        let bytes = br#"{"schema_version":99,"counters":[]}"#;
        atomic_write(&store.usage_path(), bytes).unwrap();
        assert!(matches!(
            store.manage("show", None),
            Err(UsageError::Unsupported(Some(99)))
        ));
        assert_eq!(fs::read(store.usage_path()).unwrap(), bytes);
    }

    #[test]
    fn corrupt_config_blocks_management_until_explicit_enable_rewrites_it() {
        let temporary = tempdir().unwrap();
        let store = UsageStore::new(temporary.path().join("config")).unwrap();
        atomic_write(&store.config_path(), b"not-json").unwrap();
        assert!(matches!(
            store.manage("show", None),
            Err(UsageError::Corrupt(_))
        ));
        assert!(matches!(
            store.manage("reset", None),
            Err(UsageError::Corrupt(_))
        ));
        assert!(matches!(
            store.manage("export", Some(&temporary.path().join("export.json"))),
            Err(UsageError::Corrupt(_))
        ));
        store.manage("enable", None).unwrap();
        assert!(store.manage("show", None).unwrap().enabled);
    }

    #[test]
    fn every_usage_error_has_a_stable_public_code() {
        let json_error = serde_json::from_str::<Value>("bad").unwrap_err();
        let cases = [
            UsageError::Corrupt("x".to_owned()),
            UsageError::Unsupported(Some(2)),
            UsageError::Overflow,
            UsageError::Invalid("x".to_owned()),
            UsageError::Unavailable("x".to_owned()),
            UsageError::Io(io::Error::other("x")),
            UsageError::Json(json_error),
        ];
        let codes = cases.iter().map(UsageError::code).collect::<Vec<_>>();
        assert_eq!(
            codes,
            [
                "usage_store_corrupt",
                "usage_schema_unsupported",
                "usage_store_corrupt",
                "invalid_request",
                "usage_store_corrupt",
                "usage_store_corrupt",
                "usage_store_corrupt",
            ]
        );
    }

    #[test]
    fn overflow_and_unsafe_state_files_preserve_existing_bytes() {
        let key = UsageKey {
            backend: "cdp",
            operation: "Runtime.evaluate",
            intent: Some("query"),
            outcome: "completed",
        };
        let mut aggregate = UsageAggregate {
            schema_version: 1,
            counters: vec![UsageCounter {
                backend: key.backend.to_owned(),
                operation: key.operation.to_owned(),
                intent: key.intent.map(str::to_owned),
                outcome: key.outcome.to_owned(),
                count: u64::MAX,
            }],
        };
        assert!(matches!(
            increment_counter(&mut aggregate, &key),
            Err(UsageError::Overflow)
        ));
        assert_eq!(aggregate.counters[0].count, u64::MAX);

        let temporary = tempdir().unwrap();
        let store = UsageStore::new(temporary.path().join("config")).unwrap();
        atomic_write(
            &store.usage_path(),
            br#"{"schema_version":1,"counters":[]}"#,
        )
        .unwrap();
        fs::set_permissions(store.usage_path(), fs::Permissions::from_mode(0o644)).unwrap();
        let original = fs::read(store.usage_path()).unwrap();
        assert!(matches!(
            store.manage("show", None),
            Err(UsageError::Corrupt(_))
        ));
        assert_eq!(fs::read(store.usage_path()).unwrap(), original);

        fs::remove_file(store.usage_path()).unwrap();
        let outside = temporary.path().join("outside.json");
        fs::write(&outside, b"outside").unwrap();
        symlink(&outside, store.usage_path()).unwrap();
        assert!(matches!(
            store.manage("show", None),
            Err(UsageError::Corrupt(_))
        ));
        assert_eq!(fs::read(outside).unwrap(), b"outside");
    }

    #[test]
    fn export_is_idempotent_and_never_overwrites_a_different_or_symlink_file() {
        let temporary = tempdir().unwrap();
        let store = UsageStore::new(temporary.path().join("config")).unwrap();
        store.manage("enable", None).unwrap();
        store
            .record(UsageKey {
                backend: "cdp",
                operation: "Runtime.evaluate",
                intent: Some("query"),
                outcome: "completed",
            })
            .unwrap();

        let destination = temporary.path().join("usage-export.json");
        store.manage("export", Some(&destination)).unwrap();
        let exported = fs::read(&destination).unwrap();
        store.manage("export", Some(&destination)).unwrap();
        assert_eq!(fs::read(&destination).unwrap(), exported);

        fs::write(&destination, b"different").unwrap();
        assert!(matches!(
            store.manage("export", Some(&destination)),
            Err(UsageError::Unavailable(_))
        ));
        assert_eq!(fs::read(&destination).unwrap(), b"different");

        let outside = temporary.path().join("outside.json");
        fs::write(&outside, b"outside").unwrap();
        let linked_destination = temporary.path().join("linked-export.json");
        symlink(&outside, &linked_destination).unwrap();
        assert!(matches!(
            store.manage("export", Some(&linked_destination)),
            Err(UsageError::Unavailable(_))
        ));
        assert_eq!(fs::read(outside).unwrap(), b"outside");
    }
}
