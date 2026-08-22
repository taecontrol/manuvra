use crate::util::{atomic_write, ensure_private_directory, opaque_id, rfc3339, unix_mode};
use manuvra_protocol::sha256_hex;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::SystemTime;
use thiserror::Error;

const OWNER_FILE: &str = ".manuvra-owner.json";
const MANIFEST_FILE: &str = "manifest.json";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct OwnershipRecord {
    schema: String,
    session_id: String,
    daemon_instance: String,
    canonical_directory: String,
    effective_uid: u32,
    target_id: String,
    target_generation: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ArtifactManifest {
    pub schema: String,
    pub session_id: String,
    pub target_id: String,
    pub generation: u64,
    pub lifetime: String,
    pub session_directory: String,
    pub artifacts: Vec<ArtifactEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ArtifactEntry {
    pub artifact_id: String,
    pub kind: String,
    pub absolute_path: String,
    pub media_type: String,
    pub bytes: u64,
    pub sha256: String,
    pub complete: bool,
    pub request_id: String,
    pub action_sequence: Option<u64>,
    pub created_at: String,
    pub lifetime: String,
}

#[derive(Debug, Clone)]
pub struct PublishedArtifact {
    pub artifact_id: String,
    pub kind: String,
    pub path: PathBuf,
    pub sha256: String,
    pub manifest_path: PathBuf,
}

pub struct ArtifactWrite<'a> {
    pub kind: &'a str,
    pub extension: &'a str,
    pub media_type: &'a str,
    pub bytes: &'a [u8],
    pub request_id: &'a str,
    pub action_sequence: u64,
}

#[derive(Debug)]
pub struct ArtifactStore {
    sessions_root: PathBuf,
    daemon_instance: String,
    effective_uid: u32,
    lock: Mutex<()>,
}

#[derive(Debug, Default)]
pub struct CleanupReport {
    pub removed: Vec<PathBuf>,
    pub preserved: Vec<PathBuf>,
}

enum OrphanAction {
    Preserve,
    Remove,
}

impl ArtifactStore {
    pub fn new(temporary_root: &Path, daemon_instance: String) -> Result<Self, ArtifactError> {
        let sessions_root = temporary_root.join("manuvra/sessions-v1");
        ensure_private_directory(&sessions_root)?;
        Ok(Self {
            sessions_root,
            daemon_instance,
            effective_uid: unsafe { libc::geteuid() },
            lock: Mutex::new(()),
        })
    }

    pub fn create_session(
        &self,
        session_id: &str,
        target_id: &str,
        target_generation: u64,
    ) -> Result<PathBuf, ArtifactError> {
        let _guard = self.lock()?;
        let directory = tempfile::Builder::new()
            .prefix("session-")
            .tempdir_in(&self.sessions_root)?
            .keep();
        fs::set_permissions(&directory, fs::Permissions::from_mode(0o700))?;
        let canonical = fs::canonicalize(&directory)?;
        let owner = OwnershipRecord {
            schema: "manuvra/session-owner@1".to_owned(),
            session_id: session_id.to_owned(),
            daemon_instance: self.daemon_instance.clone(),
            canonical_directory: canonical.display().to_string(),
            effective_uid: self.effective_uid,
            target_id: target_id.to_owned(),
            target_generation,
        };
        let manifest = ArtifactManifest {
            schema: "manuvra/artifact-manifest@1".to_owned(),
            session_id: session_id.to_owned(),
            target_id: target_id.to_owned(),
            generation: target_generation,
            lifetime: "until_session_close".to_owned(),
            session_directory: canonical.display().to_string(),
            artifacts: Vec::new(),
        };
        let result = self.commit_initial_metadata(&canonical, &owner, &manifest);
        if result.is_err() {
            let _ = fs::remove_dir_all(&canonical);
        }
        result.map(|_| canonical)
    }

    fn commit_initial_metadata(
        &self,
        directory: &Path,
        owner: &OwnershipRecord,
        manifest: &ArtifactManifest,
    ) -> Result<(), ArtifactError> {
        atomic_write(&directory.join(OWNER_FILE), &serde_json::to_vec(owner)?)?;
        atomic_write(
            &directory.join(MANIFEST_FILE),
            &serde_json::to_vec(manifest)?,
        )?;
        Ok(())
    }

    pub fn publish(
        &self,
        directory: &Path,
        write: ArtifactWrite<'_>,
    ) -> Result<PublishedArtifact, ArtifactError> {
        let _guard = self.lock()?;
        self.verify_owned(directory, true)?;
        let artifact_id = opaque_id("a_");
        let path = directory.join(format!("{artifact_id}.{}", write.extension));
        write_new_complete_file(&path, write.bytes)?;
        let digest = sha256_hex(write.bytes);
        let mut manifest = self.read_manifest(directory)?;
        manifest.artifacts.push(ArtifactEntry {
            artifact_id: artifact_id.clone(),
            kind: write.kind.to_owned(),
            absolute_path: path.display().to_string(),
            media_type: write.media_type.to_owned(),
            bytes: write.bytes.len() as u64,
            sha256: digest.clone(),
            complete: true,
            request_id: write.request_id.to_owned(),
            action_sequence: (write.action_sequence > 0).then_some(write.action_sequence),
            created_at: rfc3339(SystemTime::now()),
            lifetime: "until_session_close".to_owned(),
        });
        if let Err(error) = self.write_manifest(directory, &manifest) {
            let _ = fs::remove_file(&path);
            return Err(error);
        }
        Ok(PublishedArtifact {
            artifact_id,
            kind: write.kind.to_owned(),
            path,
            sha256: digest,
            manifest_path: directory.join(MANIFEST_FILE),
        })
    }

    pub fn manifest_path(&self, directory: &Path) -> PathBuf {
        directory.join(MANIFEST_FILE)
    }

    pub fn export(
        &self,
        directory: &Path,
        destination: &Path,
        selected: Option<&[String]>,
    ) -> Result<Vec<PublishedArtifact>, ArtifactError> {
        let _guard = self.lock()?;
        self.prepare_export(directory, destination)?;
        let manifest = self.read_manifest(directory)?;
        let exported = copy_selected_entries(&manifest, destination, selected)?;
        maybe_write_exported_manifest(&manifest, destination, &exported, selected)?;
        Ok(exported)
    }

    fn prepare_export(&self, directory: &Path, destination: &Path) -> Result<(), ArtifactError> {
        self.verify_owned(directory, true)?;
        reject_export_inside_session(directory, destination)?;
        ensure_export_directory(destination)
    }

    pub fn close_session(&self, directory: &Path) -> Result<(), ArtifactError> {
        let _guard = self.lock()?;
        self.verify_owned(directory, true)?;
        fs::remove_dir_all(directory)?;
        Ok(())
    }

    pub fn cleanup_orphans(&self) -> Result<CleanupReport, ArtifactError> {
        let _guard = self.lock()?;
        let mut report = CleanupReport::default();
        for entry in fs::read_dir(&self.sessions_root)? {
            self.classify_orphan(entry?, &mut report)?;
        }
        Ok(report)
    }

    fn classify_orphan(
        &self,
        entry: fs::DirEntry,
        report: &mut CleanupReport,
    ) -> Result<(), ArtifactError> {
        let path = entry.path();
        match self.orphan_disposition(&path)? {
            OrphanAction::Preserve => report.preserved.push(path),
            OrphanAction::Remove => {
                fs::remove_dir_all(&path)?;
                report.removed.push(path);
            }
        }
        Ok(())
    }

    fn orphan_disposition(&self, path: &Path) -> Result<OrphanAction, ArtifactError> {
        if !is_direct_real_child(&self.sessions_root, path)? {
            return Ok(OrphanAction::Preserve);
        }
        let owner = match self.read_owner(path) {
            Ok(owner) => owner,
            Err(_) => return Ok(OrphanAction::Preserve),
        };
        if owner.daemon_instance == self.daemon_instance || self.verify_owned(path, false).is_err()
        {
            Ok(OrphanAction::Preserve)
        } else {
            Ok(OrphanAction::Remove)
        }
    }

    fn verify_owned(&self, directory: &Path, require_current: bool) -> Result<(), ArtifactError> {
        let canonical = self.canonical_owned_directory(directory)?;
        let owner = self.read_owner(&canonical)?;
        let manifest = self.read_manifest(&canonical)?;
        if !self.ownership_records_agree(&canonical, &owner, &manifest, require_current) {
            return Err(ArtifactError::Ownership(
                "ownership records do not agree".to_owned(),
            ));
        }
        validate_manifest_entries(&canonical, &manifest, self.effective_uid)?;
        Ok(())
    }

    fn canonical_owned_directory(&self, directory: &Path) -> Result<PathBuf, ArtifactError> {
        let metadata = fs::symlink_metadata(directory)?;
        validate_session_metadata(directory, &metadata, self.effective_uid)?;
        let canonical = fs::canonicalize(directory)?;
        validate_direct_child(&self.sessions_root, &canonical)?;
        Ok(canonical)
    }

    fn ownership_records_agree(
        &self,
        canonical: &Path,
        owner: &OwnershipRecord,
        manifest: &ArtifactManifest,
        require_current: bool,
    ) -> bool {
        current_daemon_agrees(require_current, owner, &self.daemon_instance)
            && owner_record_is_well_formed(canonical, owner, self.effective_uid)
            && owner_matches_manifest(owner, manifest)
    }

    fn read_owner(&self, directory: &Path) -> Result<OwnershipRecord, ArtifactError> {
        let path = directory.join(OWNER_FILE);
        verify_private_regular_file(&path)?;
        Ok(serde_json::from_slice(&fs::read(path)?)?)
    }

    pub fn read_manifest(&self, directory: &Path) -> Result<ArtifactManifest, ArtifactError> {
        let path = directory.join(MANIFEST_FILE);
        verify_private_regular_file(&path)?;
        Ok(serde_json::from_slice(&fs::read(path)?)?)
    }

    fn write_manifest(
        &self,
        directory: &Path,
        manifest: &ArtifactManifest,
    ) -> Result<(), ArtifactError> {
        atomic_write(
            &directory.join(MANIFEST_FILE),
            &serde_json::to_vec(manifest)?,
        )?;
        Ok(())
    }

    fn lock(&self) -> Result<std::sync::MutexGuard<'_, ()>, ArtifactError> {
        self.lock
            .lock()
            .map_err(|_| ArtifactError::Ownership("artifact lock poisoned".to_owned()))
    }
}

fn copy_selected_entries(
    manifest: &ArtifactManifest,
    destination: &Path,
    selected: Option<&[String]>,
) -> Result<Vec<PublishedArtifact>, ArtifactError> {
    let entries = selected_entries(manifest, selected)?;
    let mut exported = Vec::with_capacity(entries.len());
    for entry in entries {
        exported.push(copy_verified(entry, destination)?);
    }
    Ok(exported)
}

fn maybe_write_exported_manifest(
    source: &ArtifactManifest,
    destination: &Path,
    exported: &[PublishedArtifact],
    selected: Option<&[String]>,
) -> Result<(), ArtifactError> {
    if selected.is_none() {
        write_exported_manifest(source, destination, exported)?;
    }
    Ok(())
}

fn write_exported_manifest(
    source: &ArtifactManifest,
    destination: &Path,
    exported: &[PublishedArtifact],
) -> Result<(), ArtifactError> {
    let exported_manifest = exported_manifest(source, destination, exported)?;
    let manifest_bytes = serde_json::to_vec_pretty(&exported_manifest)?;
    copy_without_overwrite(&destination.join(MANIFEST_FILE), &manifest_bytes)
}

fn exported_manifest(
    source: &ArtifactManifest,
    destination: &Path,
    exported: &[PublishedArtifact],
) -> Result<ArtifactManifest, ArtifactError> {
    let destination = fs::canonicalize(destination)?;
    let mut artifacts = Vec::with_capacity(source.artifacts.len());
    for entry in &source.artifacts {
        artifacts.push(rewrite_exported_entry(entry, exported)?);
    }
    let manifest = ArtifactManifest {
        schema: "manuvra/exported-artifact-manifest@1".to_owned(),
        session_id: source.session_id.clone(),
        target_id: source.target_id.clone(),
        generation: source.generation,
        lifetime: "caller_owned".to_owned(),
        session_directory: destination.display().to_string(),
        artifacts,
    };
    let value = serde_json::to_value(&manifest)?;
    manuvra_protocol::validate_exported_artifact_manifest(&value).map_err(ArtifactError::Export)?;
    Ok(manifest)
}

fn rewrite_exported_entry(
    entry: &ArtifactEntry,
    exported: &[PublishedArtifact],
) -> Result<ArtifactEntry, ArtifactError> {
    let published = exported
        .iter()
        .find(|candidate| candidate.artifact_id == entry.artifact_id)
        .ok_or_else(|| {
            ArtifactError::Export(format!(
                "exported artifact {} was absent from the completed copy set",
                entry.artifact_id
            ))
        })?;
    let bytes = fs::read(&published.path)?;
    if sha256_hex(&bytes) != entry.sha256 {
        return Err(ArtifactError::Export(format!(
            "destination hash differs for {}",
            entry.artifact_id
        )));
    }
    let mut entry = entry.clone();
    entry.absolute_path = fs::canonicalize(&published.path)?.display().to_string();
    entry.lifetime = "caller_owned".to_owned();
    Ok(entry)
}

fn current_daemon_agrees(
    require_current: bool,
    owner: &OwnershipRecord,
    daemon_instance: &str,
) -> bool {
    !require_current || owner.daemon_instance == daemon_instance
}

fn owner_record_is_well_formed(
    canonical: &Path,
    owner: &OwnershipRecord,
    effective_uid: u32,
) -> bool {
    owner.schema == "manuvra/session-owner@1"
        && owner.effective_uid == effective_uid
        && owner.canonical_directory == canonical.display().to_string()
}

fn owner_matches_manifest(owner: &OwnershipRecord, manifest: &ArtifactManifest) -> bool {
    manifest.schema == "manuvra/artifact-manifest@1"
        && manifest.lifetime == "until_session_close"
        && owner.session_id == manifest.session_id
        && owner.target_id == manifest.target_id
        && owner.target_generation == manifest.generation
        && manifest.session_directory == owner.canonical_directory
}

fn validate_session_metadata(
    directory: &Path,
    metadata: &fs::Metadata,
    effective_uid: u32,
) -> Result<(), ArtifactError> {
    if !metadata.file_type().is_dir() || metadata.file_type().is_symlink() {
        return Err(ArtifactError::Ownership(
            "session root is not a real directory".to_owned(),
        ));
    }
    if unix_mode(directory)? != 0o700 || metadata.uid() != effective_uid {
        return Err(ArtifactError::Ownership(
            "session root mode is not 0700".to_owned(),
        ));
    }
    Ok(())
}

fn validate_direct_child(root: &Path, canonical: &Path) -> Result<(), ArtifactError> {
    let root = fs::canonicalize(root)?;
    if canonical.parent() != Some(root.as_path()) {
        return Err(ArtifactError::Ownership(
            "session is not a direct child".to_owned(),
        ));
    }
    Ok(())
}

fn write_new_complete_file(path: &Path, bytes: &[u8]) -> Result<(), ArtifactError> {
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("artifact");
    let partial = path.with_file_name(format!(".{file_name}.{}.partial", opaque_id("")));
    let mut file = OpenOptions::new()
        .create_new(true)
        .write(true)
        .mode(0o600)
        .open(&partial)?;
    let result = write_and_link(&mut file, bytes, &partial, path);
    if result.is_err() {
        let _ = fs::remove_file(&partial);
    }
    result
}

fn write_and_link(
    file: &mut fs::File,
    bytes: &[u8],
    partial: &Path,
    destination: &Path,
) -> Result<(), ArtifactError> {
    persist_partial_file(file, bytes)?;
    fs::hard_link(partial, destination)?;
    fs::remove_file(partial)?;
    sync_parent_directory(destination)
}

fn persist_partial_file(file: &mut fs::File, bytes: &[u8]) -> Result<(), ArtifactError> {
    file.write_all(bytes)?;
    file.sync_all()?;
    Ok(())
}

fn sync_parent_directory(destination: &Path) -> Result<(), ArtifactError> {
    if let Some(parent) = destination.parent() {
        fs::File::open(parent)?.sync_all()?;
    }
    Ok(())
}

fn selected_entries<'a>(
    manifest: &'a ArtifactManifest,
    selected: Option<&[String]>,
) -> Result<Vec<&'a ArtifactEntry>, ArtifactError> {
    match selected {
        None => Ok(manifest.artifacts.iter().collect()),
        Some(ids) => ids
            .iter()
            .map(|id| {
                manifest
                    .artifacts
                    .iter()
                    .find(|entry| &entry.artifact_id == id)
                    .ok_or_else(|| ArtifactError::Export(format!("unknown artifact {id}")))
            })
            .collect(),
    }
}

fn copy_verified(
    entry: &ArtifactEntry,
    destination: &Path,
) -> Result<PublishedArtifact, ArtifactError> {
    let source = Path::new(&entry.absolute_path);
    verify_private_regular_file(source)?;
    let bytes = fs::read(source)?;
    if sha256_hex(&bytes) != entry.sha256 {
        return Err(ArtifactError::Export(
            "source hash differs from manifest".to_owned(),
        ));
    }
    let file_name = source
        .file_name()
        .ok_or_else(|| ArtifactError::Export("artifact has no file name".to_owned()))?;
    let target = destination.join(file_name);
    copy_without_overwrite(&target, &bytes)?;
    Ok(PublishedArtifact {
        artifact_id: entry.artifact_id.clone(),
        kind: entry.kind.clone(),
        path: target,
        sha256: entry.sha256.clone(),
        manifest_path: destination.join(MANIFEST_FILE),
    })
}

fn copy_without_overwrite(path: &Path, bytes: &[u8]) -> Result<(), ArtifactError> {
    if path.exists() {
        return reuse_artifact_export(path, bytes);
    }
    match write_new_complete_file(path, bytes) {
        Ok(()) => Ok(()),
        Err(ArtifactError::Io(error)) if error.kind() == io::ErrorKind::AlreadyExists => {
            reuse_artifact_export(path, bytes)
        }
        Err(error) => Err(error),
    }
}

fn reuse_artifact_export(path: &Path, bytes: &[u8]) -> Result<(), ArtifactError> {
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.file_type().is_file() || metadata.file_type().is_symlink() {
        return Err(ArtifactError::Export(format!(
            "refusing to reuse non-regular {}",
            path.display()
        )));
    }
    if sha256_hex(&fs::read(path)?) == sha256_hex(bytes) {
        Ok(())
    } else {
        Err(ArtifactError::Export(format!(
            "refusing to overwrite {}",
            path.display()
        )))
    }
}

fn validate_manifest_entries(
    directory: &Path,
    manifest: &ArtifactManifest,
    effective_uid: u32,
) -> Result<(), ArtifactError> {
    let mut expected = HashSet::from([directory.join(OWNER_FILE), directory.join(MANIFEST_FILE)]);
    let mut ids = HashSet::new();
    for entry in &manifest.artifacts {
        validate_manifest_entry(directory, entry, effective_uid, &mut ids)?;
        expected.insert(PathBuf::from(&entry.absolute_path));
    }
    validate_directory_entries(directory, &expected)
}

fn validate_manifest_entry(
    directory: &Path,
    entry: &ArtifactEntry,
    effective_uid: u32,
    ids: &mut HashSet<String>,
) -> Result<(), ArtifactError> {
    validate_manifest_metadata(entry, ids)?;
    let path = Path::new(&entry.absolute_path);
    let metadata = fs::symlink_metadata(path)?;
    validate_artifact_file(directory, path, &metadata, effective_uid)?;
    validate_manifest_file_facts(entry, path, &metadata)?;
    validate_artifact_file_name(entry, path)
}

fn validate_manifest_metadata(
    entry: &ArtifactEntry,
    ids: &mut HashSet<String>,
) -> Result<(), ArtifactError> {
    if !entry.complete
        || entry.lifetime != "until_session_close"
        || !entry.artifact_id.starts_with("a_")
        || !ids.insert(entry.artifact_id.clone())
    {
        return Err(ArtifactError::Ownership(
            "manifest artifact metadata is invalid".to_owned(),
        ));
    }
    Ok(())
}

fn validate_manifest_file_facts(
    entry: &ArtifactEntry,
    path: &Path,
    metadata: &fs::Metadata,
) -> Result<(), ArtifactError> {
    if metadata.len() != entry.bytes || sha256_hex(&fs::read(path)?) != entry.sha256 {
        return Err(ArtifactError::Ownership(
            "manifest artifact facts do not match the file".to_owned(),
        ));
    }
    Ok(())
}

fn validate_artifact_file_name(entry: &ArtifactEntry, path: &Path) -> Result<(), ArtifactError> {
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or_default();
    if name.starts_with(&format!("{}.", entry.artifact_id)) {
        Ok(())
    } else {
        Err(ArtifactError::Ownership(
            "manifest artifact name does not match its ID".to_owned(),
        ))
    }
}

fn validate_artifact_file(
    directory: &Path,
    path: &Path,
    metadata: &fs::Metadata,
    effective_uid: u32,
) -> Result<(), ArtifactError> {
    require_absolute_regular_file(path, metadata)?;
    require_private_uid_mode(path, metadata, effective_uid)?;
    require_direct_session_child(directory, path)
}

fn require_absolute_regular_file(
    path: &Path,
    metadata: &fs::Metadata,
) -> Result<(), ArtifactError> {
    if path.is_absolute() && metadata.file_type().is_file() && !metadata.file_type().is_symlink() {
        Ok(())
    } else {
        Err(ArtifactError::Ownership(
            "manifest artifact is not a private direct child".to_owned(),
        ))
    }
}

fn require_private_uid_mode(
    path: &Path,
    metadata: &fs::Metadata,
    effective_uid: u32,
) -> Result<(), ArtifactError> {
    if metadata.uid() == effective_uid && unix_mode(path)? == 0o600 {
        Ok(())
    } else {
        Err(ArtifactError::Ownership(
            "manifest artifact is not a private direct child".to_owned(),
        ))
    }
}

fn require_direct_session_child(directory: &Path, path: &Path) -> Result<(), ArtifactError> {
    if fs::canonicalize(path)?.parent() == Some(directory) {
        Ok(())
    } else {
        Err(ArtifactError::Ownership(
            "manifest artifact is not a private direct child".to_owned(),
        ))
    }
}

fn validate_directory_entries(
    directory: &Path,
    expected: &HashSet<PathBuf>,
) -> Result<(), ArtifactError> {
    for entry in fs::read_dir(directory)? {
        let path = entry?.path();
        if !expected.contains(&path) {
            return Err(ArtifactError::Ownership(format!(
                "unexpected session entry {}",
                path.display()
            )));
        }
    }
    Ok(())
}

fn reject_export_inside_session(session: &Path, destination: &Path) -> Result<(), ArtifactError> {
    let candidate = if destination.exists() {
        fs::canonicalize(destination)?
    } else {
        let parent = destination
            .parent()
            .ok_or_else(|| ArtifactError::Export("destination has no parent".to_owned()))?;
        fs::canonicalize(parent)?.join(destination.file_name().unwrap_or_default())
    };
    if candidate.starts_with(session) {
        return Err(ArtifactError::Export(
            "destination is inside the session tree".to_owned(),
        ));
    }
    Ok(())
}

fn verify_private_regular_file(path: &Path) -> Result<(), ArtifactError> {
    let metadata = fs::symlink_metadata(path)?;
    require_regular_file(path, &metadata)?;
    require_owner_mode_600(path, &metadata)
}

fn require_regular_file(path: &Path, metadata: &fs::Metadata) -> Result<(), ArtifactError> {
    if metadata.file_type().is_file() && !metadata.file_type().is_symlink() {
        Ok(())
    } else {
        Err(ArtifactError::Ownership(format!(
            "{} is not a regular file",
            path.display()
        )))
    }
}

fn require_owner_mode_600(path: &Path, metadata: &fs::Metadata) -> Result<(), ArtifactError> {
    if unix_mode(path)? == 0o600 && metadata.uid() == unsafe { libc::geteuid() } {
        Ok(())
    } else {
        Err(ArtifactError::Ownership(format!(
            "{} mode is not 0600",
            path.display()
        )))
    }
}

fn is_direct_real_child(root: &Path, path: &Path) -> Result<bool, ArtifactError> {
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.file_type().is_dir() || metadata.file_type().is_symlink() {
        return Ok(false);
    }
    Ok(fs::canonicalize(path)?.parent() == Some(fs::canonicalize(root)?.as_path()))
}

fn ensure_export_directory(path: &Path) -> Result<(), ArtifactError> {
    if path.exists() {
        let metadata = fs::symlink_metadata(path)?;
        if !metadata.file_type().is_dir() || metadata.file_type().is_symlink() {
            return Err(ArtifactError::Export(
                "destination is not a real directory".to_owned(),
            ));
        }
        return Ok(());
    }
    fs::create_dir_all(path)?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    Ok(())
}

#[derive(Debug, Error)]
pub enum ArtifactError {
    #[error("artifact I/O: {0}")]
    Io(#[from] io::Error),
    #[error("artifact JSON: {0}")]
    Json(#[from] serde_json::Error),
    #[error("artifact ownership: {0}")]
    Ownership(String),
    #[error("artifact export: {0}")]
    Export(String),
}

#[cfg(test)]
mod tests {
    use super::sync_parent_directory;
    use std::path::Path;

    #[test]
    fn sync_parent_directory_skips_paths_without_a_parent() {
        assert!(sync_parent_directory(Path::new("/")).is_ok());
    }
}
