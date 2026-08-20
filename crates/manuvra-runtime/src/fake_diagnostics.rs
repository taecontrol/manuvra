use crate::fake::FakeAdapter;
use crate::{
    AdapterContext, AdapterError, AdapterOperation, AdapterReply, TargetAdapter, TargetDescriptor,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::fs;
use std::io::Write;
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};
use std::time::Instant;

const MAX_CONFIG_BYTES: u64 = 64 * 1024;
pub struct ConfiguredDiagnostics {
    pub adapter: Arc<dyn TargetAdapter>,
    pub installation: Value,
    pub doctor_warnings: Vec<String>,
}

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct DiagnosticsConfig {
    permissions: PermissionSet,
    installation: InstallationConfig,
    #[serde(default)]
    doctor_warnings: Vec<String>,
    evidence_path: PathBuf,
}

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct PermissionSet {
    accessibility: PermissionFact,
    screen_recording: PermissionFact,
    post_event: PermissionFact,
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct PermissionFact {
    before_granted: bool,
    prompt_requested: bool,
    settings_opened: bool,
    granted: bool,
    freshly_granted: bool,
    residual: bool,
}

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct InstallationConfig {
    installed: bool,
    bundle: Option<PathBuf>,
}

#[derive(Clone, Default, Serialize)]
struct PermissionCounts {
    accessibility: u64,
    screen_recording: u64,
    post_event: u64,
}

#[derive(Clone, Default, Serialize)]
struct PaneCounts {
    accessibility: u64,
    screen_recording: u64,
}

#[derive(Clone, Default, Serialize)]
struct FakeEvidence {
    setup_invocations: u64,
    request_attempts: PermissionCounts,
    rechecks: PermissionCounts,
    pane_opens: PaneCounts,
    external_permission_api_calls: u64,
    external_open_process_calls: u64,
}

struct ConfiguredFakeAdapter {
    permissions: PermissionSet,
    evidence_path: PathBuf,
    evidence: Mutex<FakeEvidence>,
}

impl ConfiguredDiagnostics {
    pub fn load(config_path: &Path, evidence_root: &Path) -> Result<Self, String> {
        let canonical_root = canonical_temporary_root(evidence_root)?;
        let config_path = validate_config_path(config_path, &canonical_root)?;
        let bytes = fs::read(config_path).map_err(|error| error.to_string())?;
        let config: DiagnosticsConfig = serde_json::from_slice(&bytes)
            .map_err(|error| format!("invalid fake diagnostics config: {error}"))?;
        let evidence_path = config.validate(&canonical_root)?;
        let installation = config.installation.value();
        let doctor_warnings = config.doctor_warnings.clone();
        let adapter = Arc::new(ConfiguredFakeAdapter::new(
            config.permissions,
            evidence_path,
        )?);
        Ok(Self {
            adapter,
            installation,
            doctor_warnings,
        })
    }
}

impl DiagnosticsConfig {
    fn validate(&self, canonical_root: &Path) -> Result<PathBuf, String> {
        self.permissions.validate()?;
        self.installation.validate()?;
        validate_warnings(&self.doctor_warnings)?;
        validate_evidence_path(&self.evidence_path, canonical_root)
    }
}

impl PermissionSet {
    fn validate(&self) -> Result<(), String> {
        self.accessibility.validate("accessibility")?;
        self.screen_recording.validate("screen_recording")?;
        self.post_event.validate("post_event")
    }

    fn value(&self) -> Value {
        json!({
            "accessibility": self.accessibility,
            "screen_recording": self.screen_recording,
            "post_event": self.post_event,
        })
    }
}

impl PermissionFact {
    fn validate(&self, name: &str) -> Result<(), String> {
        if self.prompt_requested != !self.before_granted {
            return Err(format!("{name} request fact contradicts its preflight"));
        }
        if self.freshly_granted != (!self.before_granted && self.granted) {
            return Err(format!("{name} freshly-granted fact is inconsistent"));
        }
        if self.residual == self.granted {
            return Err(format!("{name} residual fact is inconsistent"));
        }
        if self.settings_opened && !self.residual {
            return Err(format!(
                "{name} cannot open settings after a successful recheck"
            ));
        }
        Ok(())
    }
}

impl InstallationConfig {
    fn validate(&self) -> Result<(), String> {
        if self.installed != self.bundle.is_some() {
            return Err("fake installation and bundle presence disagree".to_owned());
        }
        if self
            .bundle
            .as_ref()
            .is_some_and(|bundle| !bundle.is_absolute())
        {
            return Err("fake installation bundle must be absolute".to_owned());
        }
        Ok(())
    }

    fn value(&self) -> Value {
        json!({"installed": self.installed, "bundle": self.bundle})
    }
}

impl ConfiguredFakeAdapter {
    fn new(permissions: PermissionSet, evidence_path: PathBuf) -> Result<Self, String> {
        let adapter = Self {
            permissions,
            evidence_path,
            evidence: Mutex::new(FakeEvidence::default()),
        };
        adapter.write_current_evidence()?;
        Ok(adapter)
    }

    fn setup_result(&self) -> Value {
        json!({"permissions": self.permissions.value()})
    }

    fn record_setup(&self) -> Result<(), String> {
        let mut evidence = self.evidence.lock().expect("fake diagnostic evidence");
        evidence.setup_invocations += 1;
        record_permission_events(&mut evidence, &self.permissions);
        write_evidence(&self.evidence_path, &evidence)
    }

    fn write_current_evidence(&self) -> Result<(), String> {
        let evidence = self.evidence.lock().expect("fake diagnostic evidence");
        write_evidence(&self.evidence_path, &evidence)
    }

    fn diagnostics_value(&self) -> Value {
        let completed = self
            .evidence
            .lock()
            .expect("fake diagnostic evidence")
            .setup_invocations
            > 0;
        json!({
            "kind": "macos",
            "permissions": {
                "accessibility": permission_snapshot(&self.permissions.accessibility, completed),
                "screen_recording": permission_snapshot(&self.permissions.screen_recording, completed),
                "post_event": permission_snapshot(&self.permissions.post_event, completed),
                "responsible_identity": "manuvra-daemon (configured debug fake)",
                "recovery": {
                    "accessibility": "System Settings > Privacy & Security > Accessibility",
                    "screen_recording": "System Settings > Privacy & Security > Screen & System Audio Recording",
                    "post_event": "System Settings > Privacy & Security > Accessibility"
                },
                "prompts_triggered": false
            },
            "targets": 2,
            "discovery_error": null
        })
    }
}

impl TargetAdapter for ConfiguredFakeAdapter {
    fn targets(&self) -> Vec<TargetDescriptor> {
        FakeAdapter.targets()
    }

    fn diagnostics(&self) -> Value {
        self.diagnostics_value()
    }

    fn setup_permissions(&self, deadline: Instant) -> Option<Result<Value, AdapterError>> {
        if Instant::now() >= deadline {
            return Some(Err(fake_adapter_error(
                "timed_out",
                "fake setup deadline expired",
            )));
        }
        if let Err(message) = self.record_setup() {
            return Some(Err(fake_adapter_error("internal_error", &message)));
        }
        Some(Ok(self.setup_result()))
    }

    fn invoke(
        &self,
        context: &AdapterContext,
        operation: &AdapterOperation,
        cancellation: Arc<AtomicBool>,
    ) -> AdapterReply {
        FakeAdapter.invoke(context, operation, cancellation)
    }
}

fn permission_snapshot(permission: &PermissionFact, completed: bool) -> bool {
    if completed {
        permission.granted
    } else {
        permission.before_granted
    }
}

fn record_permission_events(evidence: &mut FakeEvidence, permissions: &PermissionSet) {
    evidence.request_attempts.accessibility +=
        u64::from(permissions.accessibility.prompt_requested);
    evidence.request_attempts.screen_recording +=
        u64::from(permissions.screen_recording.prompt_requested);
    evidence.request_attempts.post_event += u64::from(permissions.post_event.prompt_requested);
    evidence.rechecks.accessibility += 1;
    evidence.rechecks.screen_recording += 1;
    evidence.rechecks.post_event += 1;
    evidence.pane_opens.accessibility += u64::from(
        permissions.accessibility.settings_opened || permissions.post_event.settings_opened,
    );
    evidence.pane_opens.screen_recording += u64::from(permissions.screen_recording.settings_opened);
}

fn canonical_temporary_root(root: &Path) -> Result<PathBuf, String> {
    validate_normal_absolute(root, "temporary root")?;
    let canonical = fs::canonicalize(root).map_err(|error| error.to_string())?;
    if !canonical.is_dir() {
        return Err("fake diagnostics temporary root must be a directory".to_owned());
    }
    Ok(canonical)
}

fn validate_config_path(path: &Path, canonical_root: &Path) -> Result<PathBuf, String> {
    validate_normal_absolute(path, "config")?;
    let metadata = fs::symlink_metadata(path).map_err(|error| error.to_string())?;
    if !metadata.file_type().is_file() || metadata.file_type().is_symlink() {
        return Err("fake diagnostics config must be a regular file".to_owned());
    }
    if metadata.len() > MAX_CONFIG_BYTES {
        return Err("fake diagnostics config exceeds 65536 bytes".to_owned());
    }
    let canonical = fs::canonicalize(path).map_err(|error| error.to_string())?;
    require_canonical_containment(&canonical, canonical_root, "config")?;
    Ok(canonical)
}

fn validate_evidence_path(path: &Path, canonical_root: &Path) -> Result<PathBuf, String> {
    validate_normal_absolute(path, "evidence")?;
    let parent = path
        .parent()
        .ok_or_else(|| "fake evidence path has no parent".to_owned())?;
    let canonical_parent = fs::canonicalize(parent).map_err(|error| error.to_string())?;
    require_canonical_containment(&canonical_parent, canonical_root, "evidence parent")?;
    if !canonical_parent.is_dir() {
        return Err("fake evidence parent must already exist".to_owned());
    }
    let file_name = path
        .file_name()
        .ok_or_else(|| "fake evidence path has no file name".to_owned())?;
    let anchored = canonical_parent.join(file_name);
    validate_evidence_target(&anchored)?;
    Ok(anchored)
}

fn validate_normal_absolute(path: &Path, label: &str) -> Result<(), String> {
    if !path.is_absolute() || !path.components().all(normal_component) {
        return Err(format!(
            "fake diagnostics {label} path must be a normal absolute path"
        ));
    }
    Ok(())
}

fn normal_component(component: Component<'_>) -> bool {
    matches!(component, Component::RootDir | Component::Normal(_))
}

fn require_canonical_containment(
    path: &Path,
    canonical_root: &Path,
    label: &str,
) -> Result<(), String> {
    if !path.starts_with(canonical_root) {
        return Err(format!(
            "fake diagnostics {label} escapes the canonical temporary root"
        ));
    }
    Ok(())
}

fn validate_evidence_target(path: &Path) -> Result<(), String> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_file() && !metadata.file_type().is_symlink() => {
            Ok(())
        }
        Ok(_) => Err("fake evidence target must be a regular file when it exists".to_owned()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.to_string()),
    }
}

fn validate_warnings(warnings: &[String]) -> Result<(), String> {
    if warnings.len() > 16 {
        return Err("fake diagnostics warnings exceed 16 entries".to_owned());
    }
    if warnings
        .iter()
        .any(|warning| warning.is_empty() || warning.len() > 256)
    {
        return Err("fake diagnostics warnings must contain 1 through 256 bytes".to_owned());
    }
    Ok(())
}

fn write_evidence(path: &Path, evidence: &FakeEvidence) -> Result<(), String> {
    let parent = path
        .parent()
        .ok_or_else(|| "fake evidence path has no parent".to_owned())?;
    let mut temporary =
        tempfile::NamedTempFile::new_in(parent).map_err(|error| error.to_string())?;
    serde_json::to_writer(&mut temporary, evidence).map_err(|error| error.to_string())?;
    temporary
        .write_all(b"\n")
        .map_err(|error| error.to_string())?;
    temporary.flush().map_err(|error| error.to_string())?;
    temporary.persist(path).map_err(|error| error.to_string())?;
    Ok(())
}

fn fake_adapter_error(code: &str, message: &str) -> AdapterError {
    AdapterError {
        code: code.to_owned(),
        message: Some(message.to_owned()),
        details: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::os::unix::fs::symlink;

    fn permission(before: bool, granted: bool, settings_opened: bool) -> Value {
        json!({
            "before_granted": before,
            "prompt_requested": !before,
            "settings_opened": settings_opened,
            "granted": granted,
            "freshly_granted": !before && granted,
            "residual": !granted
        })
    }

    fn write_config(root: &Path, value: &Value) -> PathBuf {
        let path = root.join("scenario.json");
        fs::write(&path, serde_json::to_vec(value).unwrap()).unwrap();
        path
    }

    fn valid_config(evidence: &Path) -> Value {
        json!({
            "permissions": {
                "accessibility": permission(true, true, false),
                "screen_recording": permission(true, true, false),
                "post_event": permission(true, true, false)
            },
            "installation": {"installed": false, "bundle": null},
            "evidence_path": evidence
        })
    }

    #[test]
    fn configured_setup_records_truth_transition_and_shared_pane_once() {
        let root = tempfile::tempdir().unwrap();
        let evidence = root.path().join("evidence.json");
        let config = write_config(
            root.path(),
            &json!({
                "permissions": {
                    "accessibility": permission(false, false, true),
                    "screen_recording": permission(true, true, false),
                    "post_event": permission(false, false, true)
                },
                "installation": {"installed": true, "bundle": "/opt/manuvra/Manuvra.app"},
                "doctor_warnings": ["future_warning"],
                "evidence_path": evidence
            }),
        );
        let scenario = ConfiguredDiagnostics::load(&config, root.path()).unwrap();
        assert_eq!(
            scenario.adapter.diagnostics()["permissions"]["accessibility"],
            false
        );

        let setup = scenario
            .adapter
            .setup_permissions(Instant::now() + std::time::Duration::from_secs(1))
            .unwrap()
            .unwrap();

        assert_eq!(
            setup["permissions"]["accessibility"]["settings_opened"],
            true
        );
        assert_eq!(
            scenario.adapter.diagnostics()["permissions"]["accessibility"],
            false
        );
        let recorded: Value = serde_json::from_slice(&fs::read(evidence).unwrap()).unwrap();
        assert_eq!(recorded["setup_invocations"], 1);
        assert_eq!(recorded["request_attempts"]["accessibility"], 1);
        assert_eq!(recorded["rechecks"]["post_event"], 1);
        assert_eq!(recorded["pane_opens"]["accessibility"], 1);
        assert_eq!(recorded["external_permission_api_calls"], 0);
        assert_eq!(recorded["external_open_process_calls"], 0);
    }

    #[test]
    fn malformed_or_contradictory_config_fails_closed_without_evidence() {
        let root = tempfile::tempdir().unwrap();
        let evidence = root.path().join("evidence.json");
        let config = write_config(
            root.path(),
            &json!({
                "permissions": {
                    "accessibility": {
                        "before_granted": true,
                        "prompt_requested": true,
                        "settings_opened": false,
                        "granted": false,
                        "freshly_granted": false,
                        "residual": true
                    },
                    "screen_recording": permission(true, true, false),
                    "post_event": permission(true, true, false)
                },
                "installation": {"installed": false, "bundle": null},
                "evidence_path": evidence
            }),
        );

        assert!(ConfiguredDiagnostics::load(&config, root.path()).is_err());
        assert!(!evidence.exists());
    }

    #[test]
    fn lexical_parent_traversal_never_reads_or_writes_outside_root() {
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir_in(root.path().parent().unwrap()).unwrap();
        let outside_config = outside.path().join("scenario.json");
        let outside_evidence = outside.path().join("evidence.json");
        fs::write(&outside_config, b"not valid json").unwrap();
        let traversal = root
            .path()
            .join("..")
            .join(outside.path().file_name().unwrap())
            .join("scenario.json");

        let error = ConfiguredDiagnostics::load(&traversal, root.path())
            .err()
            .expect("traversal must be rejected");

        assert!(error.contains("normal absolute path"));
        assert_eq!(fs::read(outside_config).unwrap(), b"not valid json");
        assert!(!outside_evidence.exists());
    }

    #[test]
    fn symlinked_parents_never_read_or_write_outside_root() {
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let linked = root.path().join("linked");
        symlink(outside.path(), &linked).unwrap();
        let outside_config = outside.path().join("scenario.json");
        fs::write(&outside_config, b"not valid json").unwrap();

        let config_error = ConfiguredDiagnostics::load(&linked.join("scenario.json"), root.path())
            .err()
            .expect("symlinked config parent must be rejected");

        assert!(config_error.contains("escapes the canonical temporary root"));
        assert_eq!(fs::read(&outside_config).unwrap(), b"not valid json");
        let outside_evidence = outside.path().join("evidence.json");
        let inside_config = write_config(root.path(), &valid_config(&linked.join("evidence.json")));

        let evidence_error = ConfiguredDiagnostics::load(&inside_config, root.path())
            .err()
            .expect("symlinked evidence parent must be rejected");

        assert!(evidence_error.contains("escapes the canonical temporary root"));
        assert!(!outside_evidence.exists());
    }
}
