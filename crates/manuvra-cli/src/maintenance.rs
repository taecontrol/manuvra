use crate::{config_root, temporary_root};
use serde_json::{Value, json};
use std::fs;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};

pub fn legacy_config_root() -> PathBuf {
    if cfg!(debug_assertions)
        && let Some(root) = std::env::var_os("MANUVRA_LEGACY_CONFIG_HOME")
    {
        return PathBuf::from(root);
    }
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/nonexistent"))
        .join(".config/computer-use")
}

pub fn migrate_legacy() -> Result<Value, String> {
    let source = legacy_config_root();
    let destination = config_root();
    validate_migration_source(&source)?;
    prepare_migration_destination(&destination)?;
    copy_directory(&source, &destination)?;
    Ok(json!({
        "source": source,
        "destination": destination,
        "copied": true,
        "source_preserved": true,
    }))
}

fn validate_migration_source(source: &Path) -> Result<(), String> {
    source.exists().then_some(()).ok_or_else(|| {
        format!(
            "legacy configuration does not exist at {}",
            source.display()
        )
    })?;
    validate_owned_directory(source)
}

fn prepare_migration_destination(destination: &Path) -> Result<(), String> {
    if destination.exists() {
        return validate_empty_destination(destination);
    }
    fs::create_dir_all(destination).map_err(|error| error.to_string())?;
    fs::set_permissions(destination, fs::Permissions::from_mode(0o700))
        .map_err(|error| error.to_string())
}

fn validate_empty_destination(destination: &Path) -> Result<(), String> {
    validate_owned_directory(destination)?;
    let empty = fs::read_dir(destination)
        .map_err(|error| error.to_string())?
        .next()
        .is_none();
    empty.then_some(()).ok_or_else(|| {
        format!(
            "destination already contains data at {}",
            destination.display()
        )
    })
}

pub fn purge_owned_roots() -> Result<Value, String> {
    let roots = [config_root(), temporary_root().join("manuvra")];
    let mut removed = Vec::new();
    for root in roots {
        if !root.exists() {
            continue;
        }
        validate_owned_directory(&root)?;
        fs::remove_dir_all(&root).map_err(|error| error.to_string())?;
        removed.push(root);
    }
    Ok(json!({"removed": removed, "retained_exports": true}))
}

fn copy_directory(source: &Path, destination: &Path) -> Result<(), String> {
    let mut entries = fs::read_dir(source)
        .map_err(|error| error.to_string())?
        .map(|entry| entry.map_err(|error| error.to_string()))
        .collect::<Result<Vec<_>, _>>()?;
    entries.sort_by_key(|entry| entry.file_name());
    for entry in entries {
        copy_entry(&entry.path(), &destination.join(entry.file_name()))?;
    }
    Ok(())
}

fn copy_entry(source: &Path, destination: &Path) -> Result<(), String> {
    let metadata = fs::symlink_metadata(source).map_err(|error| error.to_string())?;
    validate_owner(&metadata)?;
    if metadata.is_dir() {
        return copy_subdirectory(source, destination);
    }
    if metadata.is_file() {
        return copy_file(source, destination);
    }
    Err(format!(
        "legacy configuration contains an unsupported entry at {}",
        source.display()
    ))
}

fn copy_subdirectory(source: &Path, destination: &Path) -> Result<(), String> {
    fs::create_dir(destination).map_err(|error| error.to_string())?;
    fs::set_permissions(destination, fs::Permissions::from_mode(0o700))
        .map_err(|error| error.to_string())?;
    copy_directory(source, destination)
}

fn copy_file(source: &Path, destination: &Path) -> Result<(), String> {
    fs::copy(source, destination).map_err(|error| error.to_string())?;
    fs::set_permissions(destination, fs::Permissions::from_mode(0o600))
        .map_err(|error| error.to_string())
}

fn validate_owned_directory(path: &Path) -> Result<(), String> {
    let metadata = fs::symlink_metadata(path).map_err(|error| error.to_string())?;
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        return Err(format!(
            "owned root is not a real directory: {}",
            path.display()
        ));
    }
    validate_owner(&metadata)
}

fn validate_owner(metadata: &fs::Metadata) -> Result<(), String> {
    if metadata.uid() == unsafe { libc::geteuid() } {
        Ok(())
    } else {
        Err("owned state belongs to a different user".to_owned())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_owned_destination_is_accepted_and_populated_destination_is_rejected() {
        let temporary = tempfile::tempdir().unwrap();
        let empty = temporary.path().join("empty");
        fs::create_dir(&empty).unwrap();
        validate_empty_destination(&empty).unwrap();

        fs::write(empty.join("keep.json"), b"{}").unwrap();
        let error = validate_empty_destination(&empty).unwrap_err();
        assert!(error.contains("destination already contains data"));
        assert!(empty.join("keep.json").is_file());

        let file = temporary.path().join("file");
        fs::write(&file, b"not a directory").unwrap();
        assert!(validate_empty_destination(&file).is_err());
    }

    #[test]
    fn copy_rejects_symlinks_and_preserves_source() {
        use std::os::unix::fs::symlink;
        let temporary = tempfile::tempdir().unwrap();
        let source = temporary.path().join("legacy");
        let destination = temporary.path().join("new");
        fs::create_dir(&source).unwrap();
        fs::create_dir(&destination).unwrap();
        fs::write(source.join("usage.json"), b"{}").unwrap();
        fs::create_dir(source.join("nested")).unwrap();
        fs::write(source.join("nested/state.json"), b"[]").unwrap();
        copy_directory(&source, &destination).unwrap();
        assert_eq!(fs::read(destination.join("usage.json")).unwrap(), b"{}");
        assert_eq!(
            fs::read(destination.join("nested/state.json")).unwrap(),
            b"[]"
        );
        assert!(source.join("usage.json").exists());

        let unsafe_source = temporary.path().join("unsafe");
        fs::create_dir(&unsafe_source).unwrap();
        symlink(source.join("usage.json"), unsafe_source.join("linked")).unwrap();
        assert!(copy_directory(&unsafe_source, &destination).is_err());
    }
}
