use manuvra_contract::{Artifact, Job, Manifest, SchemaVersion};
use rand::RngCore;
use serde::Serialize;
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

#[derive(Clone)]
pub struct Redactor {
    replacements: Vec<(String, String)>,
    rendered_sensitive_values: Vec<String>,
    provider_key: Option<String>,
    provider_replacement: Option<String>,
}

impl Redactor {
    pub fn for_job(job: &Job) -> Result<Self, String> {
        let provider_key = std::env::var("TYPESAFE_API_KEY")
            .ok()
            .filter(|key| !key.is_empty());
        Self::for_job_with_provider_key(job, provider_key.as_deref())
    }

    pub fn for_job_with_provider_key(
        job: &Job,
        provider_key: Option<&str>,
    ) -> Result<Self, String> {
        let explicit = job.options.redact_values.as_deref().unwrap_or_default();
        let classified: Vec<_> = job
            .values
            .iter()
            .filter(|(name, value)| {
                value.secret || explicit.iter().any(|redacted| redacted == *name)
            })
            .collect();
        let mut seen = HashSet::new();
        let renderings: Vec<_> = classified
            .into_iter()
            .flat_map(|(_, value)| {
                std::iter::once(value.value.as_str()).chain(value.formats.iter().flat_map(
                    |formats| {
                        [formats.iso.as_deref(), formats.display.as_deref()]
                            .into_iter()
                            .flatten()
                    },
                ))
            })
            .filter(|value| !value.is_empty() && seen.insert((*value).to_owned()))
            .map(str::to_owned)
            .collect();
        let rendered_sensitive_values = renderings.clone();
        let provider_key = provider_key
            .filter(|key| !key.is_empty())
            .map(str::to_owned);
        let used = job_characters(job);
        let mut markers = (0xE000..=0xF8FF).filter_map(char::from_u32).filter(|c| {
            !used.contains(c) && !provider_key.as_ref().is_some_and(|key| key.contains(*c))
        });
        let mut replacements = Vec::new();
        for (index, rendering) in renderings.iter().enumerate() {
            let marker = markers
                .next()
                .ok_or_else(|| "too many classified renderings".to_owned())?;
            let readable = format!("{marker}<masked:{}>{marker}", index + 1);
            let replacement = if renderings
                .iter()
                .any(|sensitive| readable.contains(sensitive))
            {
                marker.to_string()
            } else {
                readable
            };
            replacements.push((rendering.clone(), replacement));
        }
        let provider_replacement = if let Some(key) = &provider_key {
            let marker = markers
                .next()
                .ok_or_else(|| "too many classified renderings".to_owned())?;
            let readable = format!("{marker}<masked-provider>{marker}");
            Some(if readable.contains(key) {
                marker.to_string()
            } else {
                readable
            })
        } else {
            None
        };
        replacements.sort_by(|a, b| b.0.len().cmp(&a.0.len()).then_with(|| a.0.cmp(&b.0)));
        Ok(Self {
            replacements,
            rendered_sensitive_values,
            provider_key,
            provider_replacement,
        })
    }

    pub fn redact_text(&self, text: &str) -> String {
        let mut output = String::with_capacity(text.len());
        let mut offset = 0;
        while offset < text.len() {
            let remaining = &text[offset..];
            if let Some((sensitive, replacement)) = self
                .replacements
                .iter()
                .find(|(sensitive, _)| remaining.starts_with(sensitive))
            {
                output.push_str(replacement);
                offset += sensitive.len();
            } else {
                let character = remaining.chars().next().expect("offset is in bounds");
                output.push(character);
                offset += character.len_utf8();
            }
        }
        output
    }

    pub fn redact_export_text(&self, text: &str) -> String {
        let redacted = self.redact_text(text);
        match (&self.provider_key, &self.provider_replacement) {
            (Some(key), Some(replacement)) => redacted.replace(key, replacement),
            _ => redacted,
        }
    }

    pub fn redact_external_text(&self, text: &str) -> String {
        self.redact_export_text(text)
    }

    pub fn contains_sensitive(&self, text: &str) -> bool {
        self.replacements
            .iter()
            .any(|(sensitive, _)| text.contains(sensitive))
            || self
                .provider_key
                .as_ref()
                .is_some_and(|key| text.contains(key))
    }

    pub fn sensitive_values(&self) -> Vec<String> {
        let mut values = self.rendered_sensitive_values.clone();
        if let Some(key) = &self.provider_key
            && !values.contains(key)
        {
            values.push(key.clone());
        }
        values
    }

    fn leak_scan_values(&self) -> impl Iterator<Item = &str> {
        self.replacements
            .iter()
            .map(|(value, _)| value.as_str())
            .chain(self.provider_key.iter().map(String::as_str))
            .filter(|value| !is_protocol_collision(value))
    }

    pub fn contains_export_leak(&self, bytes: &[u8]) -> bool {
        self.leak_scan_values().any(|secret| {
            !secret.is_empty()
                && bytes
                    .windows(secret.len())
                    .any(|window| window == secret.as_bytes())
        })
    }
}

fn is_protocol_collision(value: &str) -> bool {
    const OWNED_VOCABULARY: &[&str] = &[
        "schema_version",
        "target",
        "kind",
        "browser",
        "url",
        "context",
        "journey",
        "revision",
        "environment",
        "actor",
        "authority",
        "values",
        "description",
        "formats",
        "iso",
        "display",
        "secret",
        "steps",
        "id",
        "goal",
        "done_when",
        "requires_values",
        "mutation_limit",
        "text_visible",
        "text_absent",
        "scope",
        "viewport",
        "dialog",
        "field",
        "nonempty",
        "equals_value",
        "role",
        "dialog_open",
        "dialog_closed",
        "url_contains",
        "expectations",
        "claim",
        "exact_literals",
        "literal",
        "within_text",
        "options",
        "allowed_origins",
        "active_timeout_ms",
        "pause_timeout_ms",
        "lifetime_ms",
        "max_actions",
        "max_model_calls",
        "width",
        "height",
        "redact_values",
        "debug",
        "force_stop_at_step",
        "request_id",
        "run_id",
        "state",
        "terminal",
        "reason",
        "code",
        "verdict",
        "overall",
        "result",
        "basis",
        "noul",
        "numeric_checks",
        "present",
        "caller_assisted",
        "evidence",
        "manifest",
        "complete",
        "escalation",
        "cleanup",
        "profile",
        "application_state",
        "artifacts",
        "path",
        "digest",
        "normalized_job",
        "provenance",
        "observation",
        "screenshot",
        "trace",
        "event",
        "done",
        "mutation_limit_consumed",
        "redaction_verified",
        "browser_path",
        "browser_version",
        "display_mode",
        "route",
        "title",
        "dialogs",
        "focused",
        "visible_text",
        "covered_text",
        "dialog_texts",
        "elements",
        "index",
        "input_type",
        "value",
        "checked",
        "selected",
        "expanded",
        "disabled",
        "in_dialog",
        "operations",
        "rect",
        "scroll_x",
        "scroll_y",
        "document_height",
        "coverage",
        "viewport_complete",
        "open_shadow_roots",
        "slots",
        "same_origin_frames",
        "gaps",
        "running",
        "uncertain",
        "passed",
        "failed",
        "blocked",
        "aborted",
        "expired",
        "satisfied",
        "not_satisfied",
        "unresolved",
        "not_run",
        "structured",
        "headed",
        "headless",
        "closed",
        "closure_unconfirmed",
        "removed",
        "removal_unconfirmed",
        "not_started",
        "not_created",
        "caller_owned",
        "CLICK",
        "TYPE_TEXT",
        "SELECT",
        "missing_value",
        "unsupported_in_this_build",
        "unsupported_platform",
        "browser_unavailable",
        "browser_launch_failed",
        "browser_control_failed",
        "cleanup_failed",
        "observation_unknown",
        "redaction_unverifiable",
        "done_condition_not_met",
        "visible_text_truncated",
        "covered_text_truncated",
        "dialog_text_truncated",
        "closed_shadow_root",
        "cross_origin_frame",
        "canvas",
        "generated_content",
    ];
    OWNED_VOCABULARY.iter().any(|owned| owned.contains(value))
}

fn job_characters(job: &Job) -> HashSet<char> {
    let mut value = serde_json::to_value(job).expect("job serializes");
    let mut set = HashSet::new();
    collect_characters(&mut value, &mut set);
    set
}

fn collect_characters(value: &mut Value, set: &mut HashSet<char>) {
    match value {
        Value::String(text) => set.extend(text.chars()),
        Value::Array(items) => items
            .iter_mut()
            .for_each(|item| collect_characters(item, set)),
        Value::Object(fields) => fields.iter_mut().for_each(|(key, item)| {
            set.extend(key.chars());
            collect_characters(item, set);
        }),
        _ => {}
    }
}

pub struct EvidenceBundle {
    pub complete: bool,
    pub job: Value,
    pub provenance: Value,
    pub observations: Vec<(String, Value, Option<Vec<u8>>)>,
    pub decisions: Vec<(String, Value)>,
    pub steps: Vec<(String, Value)>,
    pub escalations: Vec<(String, Value)>,
    pub dispositions: Vec<(String, Value)>,
    pub verification: Option<Value>,
    pub trace: Vec<Value>,
    pub cleanup: Value,
    pub result: Value,
}

pub fn publish(
    root: &Path,
    run_id: &str,
    bundle: EvidenceBundle,
    redactor: &Redactor,
) -> Result<Value, String> {
    create_private_dir(root)?;
    let final_dir = root.join(run_id);
    let replacing = validate_replacement(&final_dir, run_id, redactor)?;
    let stage = staged_directory(root, run_id)?;
    let result = bundle.result.clone();
    let mut publication = Publication {
        stage,
        final_dir,
        artifacts: Vec::new(),
    };
    let complete = bundle.complete;
    publication.write_bundle(bundle)?;
    publication.write_manifest(run_id, complete)?;
    commit_publication(publication, root, redactor, replacing)?;
    Ok(result)
}

fn commit_publication(
    publication: Publication,
    root: &Path,
    redactor: &Redactor,
    replacing: bool,
) -> Result<(), String> {
    if replacing {
        publication.commit_replacing(root, redactor)
    } else {
        publication.commit(root, redactor)
    }
}

fn validate_replacement(
    final_dir: &Path,
    run_id: &str,
    redactor: &Redactor,
) -> Result<bool, String> {
    let Some(run_dir) = existing_replacement_directory(final_dir)? else {
        return Ok(false);
    };
    let manifest = read_complete_manifest(&final_dir.join("manifest.json"), run_id)?;
    validate_replacement_artifacts(&run_dir, &manifest)?;
    reject_leaks(&run_dir, redactor)?;
    Ok(true)
}

fn existing_replacement_directory(path: &Path) -> Result<Option<PathBuf>, String> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.to_string()),
    };
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err("evidence run path is not a regular directory".into());
    }
    fs::canonicalize(path)
        .map(Some)
        .map_err(|error| error.to_string())
}

fn validate_replacement_artifacts(run_dir: &Path, manifest: &Manifest) -> Result<(), String> {
    let mut listed = BTreeSet::new();
    let mut roles = BTreeMap::new();
    for artifact in &manifest.artifacts {
        validate_replacement_artifact(run_dir, artifact, &mut listed)?;
        *roles.entry(artifact.role.as_str()).or_insert(0_usize) += 1;
    }
    validate_replacement_roles(&roles)?;
    let mut actual = BTreeSet::new();
    collect_evidence_files(run_dir, &mut actual)?;
    actual.remove(&run_dir.join("manifest.json"));
    (actual == listed)
        .then_some(())
        .ok_or_else(|| "existing evidence contains unmanifested or missing artifacts".into())
}

fn validate_replacement_roles(roles: &BTreeMap<&str, usize>) -> Result<(), String> {
    const SINGLETONS: &[&str] = &["normalized_job", "provenance", "trace", "cleanup", "result"];
    for role in SINGLETONS {
        if roles.get(role) != Some(&1) {
            return Err(format!(
                "existing evidence must contain exactly one {role} artifact"
            ));
        }
    }
    if roles.get("verification").copied().unwrap_or_default() > 1 {
        return Err("existing evidence must contain at most one verification artifact".into());
    }
    let allowed = [
        "normalized_job",
        "provenance",
        "trace",
        "cleanup",
        "result",
        "observation",
        "screenshot",
        "decision",
        "step",
        "escalation",
        "disposition",
        "verification",
    ];
    roles
        .keys()
        .find(|role| !allowed.contains(role))
        .map_or(Ok(()), |role| {
            Err(format!("existing evidence contains unexpected role {role}"))
        })
}

fn validate_replacement_artifact(
    run_dir: &Path,
    artifact: &Artifact,
    listed: &mut BTreeSet<PathBuf>,
) -> Result<(), String> {
    validate_replacement_header(artifact)?;
    let canonical = canonical_replacement_artifact(run_dir, artifact)?;
    if !listed.insert(canonical.clone()) {
        return Err("existing evidence manifest repeats an artifact path".into());
    }
    replacement_digest_matches(&canonical, &artifact.digest)
}

fn validate_replacement_header(artifact: &Artifact) -> Result<(), String> {
    (artifact.complete && valid_artifact_digest(&artifact.digest))
        .then_some(())
        .ok_or_else(|| "existing evidence manifest contains an incomplete artifact".into())
}

fn canonical_replacement_artifact(run_dir: &Path, artifact: &Artifact) -> Result<PathBuf, String> {
    let recorded = Path::new(&artifact.path);
    let metadata = fs::symlink_metadata(recorded).map_err(|error| error.to_string())?;
    regular_artifact(&metadata)?;
    let canonical = fs::canonicalize(recorded).map_err(|error| error.to_string())?;
    exact_in_run_path(run_dir, recorded, &canonical)?;
    let relative = canonical
        .strip_prefix(run_dir)
        .map_err(|_| "existing evidence artifact escapes its run directory")?;
    role_matches_relative_path(&artifact.role, relative)
        .then_some(canonical)
        .ok_or_else(|| "existing evidence artifact role has an unexpected path".into())
}

fn regular_artifact(metadata: &fs::Metadata) -> Result<(), String> {
    (!metadata.file_type().is_symlink() && metadata.is_file())
        .then_some(())
        .ok_or_else(|| "existing evidence artifact is not a regular file".into())
}

fn exact_in_run_path(run_dir: &Path, recorded: &Path, canonical: &Path) -> Result<(), String> {
    (recorded == canonical && canonical.starts_with(run_dir))
        .then_some(())
        .ok_or_else(|| "existing evidence artifact escapes its run directory".into())
}

fn replacement_digest_matches(path: &Path, digest: &str) -> Result<(), String> {
    let bytes = fs::read(path).map_err(|error| error.to_string())?;
    (hex::encode(Sha256::digest(bytes)) == digest)
        .then_some(())
        .ok_or_else(|| "existing evidence artifact digest does not match its manifest".into())
}

fn collect_evidence_files(directory: &Path, files: &mut BTreeSet<PathBuf>) -> Result<(), String> {
    fs::read_dir(directory)
        .map_err(|error| error.to_string())?
        .try_for_each(|entry| {
            let path = entry.map_err(|error| error.to_string())?.path();
            collect_evidence_path(&path, files)
        })
}

fn collect_evidence_path(path: &Path, files: &mut BTreeSet<PathBuf>) -> Result<(), String> {
    let metadata = fs::symlink_metadata(path).map_err(|error| error.to_string())?;
    if metadata.file_type().is_symlink() {
        return Err("existing evidence contains a symlink".into());
    }
    if metadata.is_dir() {
        return collect_evidence_files(path, files);
    }
    let canonical = metadata
        .is_file()
        .then(|| fs::canonicalize(path).map_err(|error| error.to_string()))
        .ok_or_else(|| "existing evidence contains an unsupported file type".to_owned())??;
    files.insert(canonical);
    Ok(())
}

fn valid_artifact_digest(digest: &str) -> bool {
    digest.len() == 64
        && digest
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn role_matches_relative_path(role: &str, relative: &Path) -> bool {
    let text = relative.to_string_lossy();
    const EXACT: &[(&str, &str)] = &[
        ("normalized_job", "job.json"),
        ("provenance", "provenance.json"),
        ("trace", "trace.jsonl"),
        ("cleanup", "cleanup.json"),
        ("result", "result.json"),
        ("verification", "verification/final.json"),
    ];
    const NESTED: &[(&str, &str, &str)] = &[
        ("observation", "observations/", ".json"),
        ("screenshot", "observations/", ".png"),
        ("decision", "decisions/", ".json"),
        ("step", "steps/", ".json"),
        ("escalation", "escalations/", ".json"),
        ("disposition", "dispositions/", ".json"),
    ];
    EXACT
        .iter()
        .any(|(expected_role, path)| role == *expected_role && text == *path)
        || NESTED.iter().any(|(expected_role, prefix, suffix)| {
            role == *expected_role && safe_artifact_leaf(&text, prefix, suffix)
        })
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

pub fn replace_result_cleanup(
    root: &Path,
    run_id: &str,
    cleanup: &impl Serialize,
    result: &Value,
    redactor: &Redactor,
) -> Result<(), String> {
    checked_run_directory(root, run_id).and_then(|run_dir| {
        replace_tail_in_directory(run_dir, run_id, cleanup, result, Some(redactor))
    })
}

/// Replaces a hosted checkpoint after the secret-owning host is confirmed dead.
///
/// The caller must derive `cleanup` and `result` exclusively from the already exported,
/// leak-scanned checkpoint. This boundary deliberately does not require the watchdog to retain
/// the job values or provider key merely to publish a terminal timeout/crash fact.
pub fn replace_result_cleanup_from_checkpoint(
    root: &Path,
    run_id: &str,
    cleanup: &impl Serialize,
    result: &Value,
) -> Result<(), String> {
    checked_run_directory(root, run_id)
        .and_then(|run_dir| replace_tail_in_directory(run_dir, run_id, cleanup, result, None))
}

fn replace_tail_in_directory(
    run_dir: PathBuf,
    run_id: &str,
    cleanup: &impl Serialize,
    result: &Value,
    redactor: Option<&Redactor>,
) -> Result<(), String> {
    let manifest_path = run_dir.join("manifest.json");
    read_complete_manifest(&manifest_path, run_id).and_then(|manifest| {
        serialized_tail(cleanup, result).and_then(|(cleanup_bytes, result_bytes)| {
            replace_tail_artifacts(
                &run_dir,
                &manifest_path,
                manifest,
                cleanup_bytes,
                result_bytes,
                redactor,
            )
        })
    })
}

fn serialized_tail(cleanup: &impl Serialize, result: &Value) -> Result<(Vec<u8>, Vec<u8>), String> {
    pretty(cleanup)
        .and_then(|cleanup_bytes| pretty(result).map(|result_bytes| (cleanup_bytes, result_bytes)))
}

fn replace_tail_artifacts(
    run_dir: &Path,
    manifest_path: &Path,
    mut manifest: Manifest,
    cleanup_bytes: Vec<u8>,
    result_bytes: Vec<u8>,
    redactor: Option<&Redactor>,
) -> Result<(), String> {
    if let Some(redactor) = redactor {
        reject_tail_leak(redactor, &cleanup_bytes, &result_bytes)?;
    }
    mark_tail_replacement_incomplete(run_dir, manifest_path, &mut manifest)?;
    replace_artifact(
        run_dir,
        &mut manifest,
        "cleanup",
        "cleanup.json",
        &cleanup_bytes,
    )?;
    replace_artifact(
        run_dir,
        &mut manifest,
        "result",
        "result.json",
        &result_bytes,
    )?;
    manifest.complete = true;
    validate_and_write_tail(
        run_dir,
        manifest_path,
        &manifest,
        &cleanup_bytes,
        &result_bytes,
        redactor,
    )
}

fn validate_and_write_tail(
    run_dir: &Path,
    manifest_path: &Path,
    manifest: &Manifest,
    cleanup_bytes: &[u8],
    result_bytes: &[u8],
    _redactor: Option<&Redactor>,
) -> Result<(), String> {
    write_tail_files(run_dir, cleanup_bytes, result_bytes)?;
    write_manifest_and_sync(run_dir, manifest_path, manifest)
}

fn mark_tail_replacement_incomplete(
    run_dir: &Path,
    manifest_path: &Path,
    manifest: &mut Manifest,
) -> Result<(), String> {
    for (role, relative) in [("cleanup", "cleanup.json"), ("result", "result.json")] {
        let expected = run_dir.join(relative);
        let artifact = manifest
            .artifacts
            .iter_mut()
            .find(|artifact| artifact.role == role)
            .ok_or_else(|| format!("evidence manifest has no {role} artifact"))?;
        if Path::new(&artifact.path) != expected {
            return Err(format!("evidence {role} artifact has an unexpected path"));
        }
        artifact.complete = false;
    }
    manifest.complete = false;
    write_manifest_and_sync(run_dir, manifest_path, manifest)
}

fn write_tail_files(run_dir: &Path, cleanup: &[u8], result: &[u8]) -> Result<(), String> {
    write_atomic(&run_dir.join("cleanup.json"), cleanup)?;
    write_atomic(&run_dir.join("result.json"), result)
}

fn write_manifest_and_sync(
    run_dir: &Path,
    manifest_path: &Path,
    manifest: &Manifest,
) -> Result<(), String> {
    pretty(manifest)
        .and_then(|bytes| write_atomic(manifest_path, &bytes))
        .and_then(|()| sync_dir(run_dir))
}

fn checked_run_directory(root: &Path, run_id: &str) -> Result<PathBuf, String> {
    let path = root.join(run_id);
    let metadata = fs::symlink_metadata(&path).map_err(|error| error.to_string())?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err("evidence run path is not a regular directory".into());
    }
    fs::canonicalize(path).map_err(|error| error.to_string())
}

fn read_complete_manifest(path: &Path, run_id: &str) -> Result<Manifest, String> {
    let metadata = fs::symlink_metadata(path).map_err(|error| error.to_string())?;
    let regular = !metadata.file_type().is_symlink() && metadata.is_file();
    let bytes = regular
        .then(|| fs::read(path).map_err(|error| error.to_string()))
        .ok_or_else(|| "evidence manifest is not a regular file".to_owned())??;
    let manifest: Manifest = serde_json::from_slice(&bytes).map_err(|error| error.to_string())?;
    (manifest.run_id == run_id && manifest.complete)
        .then_some(manifest)
        .ok_or_else(|| "cannot replace an incomplete or mismatched evidence bundle".into())
}

fn reject_tail_leak(redactor: &Redactor, cleanup: &[u8], result: &[u8]) -> Result<(), String> {
    (!redactor.contains_export_leak(cleanup) && !redactor.contains_export_leak(result))
        .then_some(())
        .ok_or_else(|| "evidence leak scan rejected hosted terminal result".into())
}

fn replace_artifact(
    run_dir: &Path,
    manifest: &mut Manifest,
    role: &str,
    relative: &str,
    bytes: &[u8],
) -> Result<(), String> {
    let expected = run_dir.join(relative);
    let artifact = manifest
        .artifacts
        .iter_mut()
        .find(|artifact| artifact.role == role)
        .ok_or_else(|| format!("evidence manifest has no {role} artifact"))?;
    (Path::new(&artifact.path) == expected)
        .then_some(())
        .ok_or_else(|| format!("evidence {role} artifact has an unexpected path"))?;
    artifact.digest = hex::encode(Sha256::digest(bytes));
    artifact.complete = true;
    Ok(())
}

struct Publication {
    stage: PathBuf,
    final_dir: PathBuf,
    artifacts: Vec<Artifact>,
}

impl Drop for Publication {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.stage);
    }
}

impl Publication {
    fn write_bundle(&mut self, bundle: EvidenceBundle) -> Result<(), String> {
        self.write_headers(&bundle)?;
        self.write_observations(bundle.observations)?;
        self.write_named("decisions", "decision", bundle.decisions)?;
        self.write_steps(bundle.steps)?;
        self.write_named("escalations", "escalation", bundle.escalations)?;
        self.write_named("dispositions", "disposition", bundle.dispositions)?;
        self.write_verification(bundle.verification)?;
        self.write_tail(bundle.trace, &bundle.cleanup, &bundle.result)
    }

    fn write_verification(&mut self, verification: Option<Value>) -> Result<(), String> {
        if let Some(verification) = verification {
            write_json(
                &self.stage,
                &self.final_dir,
                "verification/final.json",
                "verification",
                &verification,
                &mut self.artifacts,
            )?;
        }
        Ok(())
    }

    fn write_headers(&mut self, bundle: &EvidenceBundle) -> Result<(), String> {
        write_json(
            &self.stage,
            &self.final_dir,
            "job.json",
            "normalized_job",
            &bundle.job,
            &mut self.artifacts,
        )?;
        write_json(
            &self.stage,
            &self.final_dir,
            "provenance.json",
            "provenance",
            &bundle.provenance,
            &mut self.artifacts,
        )
    }

    fn write_observations(
        &mut self,
        observations: Vec<(String, Value, Option<Vec<u8>>)>,
    ) -> Result<(), String> {
        for (name, observation, screenshot) in observations {
            write_json(
                &self.stage,
                &self.final_dir,
                &format!("observations/{name}.json"),
                "observation",
                &observation,
                &mut self.artifacts,
            )?;
            if let Some(png) = screenshot {
                write_bytes(
                    &self.stage,
                    &self.final_dir,
                    &format!("observations/{name}.png"),
                    "screenshot",
                    &png,
                    &mut self.artifacts,
                )?;
            }
        }
        Ok(())
    }

    fn write_steps(&mut self, steps: Vec<(String, Value)>) -> Result<(), String> {
        for (name, step) in steps {
            write_json(
                &self.stage,
                &self.final_dir,
                &format!("steps/{name}.json"),
                "step",
                &step,
                &mut self.artifacts,
            )?;
        }
        Ok(())
    }

    fn write_named(
        &mut self,
        directory: &str,
        role: &str,
        values: Vec<(String, Value)>,
    ) -> Result<(), String> {
        for (name, value) in values {
            write_json(
                &self.stage,
                &self.final_dir,
                &format!("{directory}/{name}.json"),
                role,
                &value,
                &mut self.artifacts,
            )?;
        }
        Ok(())
    }

    fn write_tail(
        &mut self,
        trace: Vec<Value>,
        cleanup: &Value,
        result: &Value,
    ) -> Result<(), String> {
        let trace = trace_lines(trace)?;
        write_bytes(
            &self.stage,
            &self.final_dir,
            "trace.jsonl",
            "trace",
            trace.as_bytes(),
            &mut self.artifacts,
        )?;
        write_json(
            &self.stage,
            &self.final_dir,
            "cleanup.json",
            "cleanup",
            cleanup,
            &mut self.artifacts,
        )?;
        write_json(
            &self.stage,
            &self.final_dir,
            "result.json",
            "result",
            result,
            &mut self.artifacts,
        )
    }

    fn write_manifest(&self, run_id: &str, complete: bool) -> Result<(), String> {
        let manifest = Manifest {
            schema_version: SchemaVersion,
            run_id: run_id.to_owned(),
            complete,
            artifacts: self.artifacts.clone(),
        };
        write_atomic(&self.stage.join("manifest.json"), &pretty(&manifest)?)
    }

    fn commit(self, root: &Path, redactor: &Redactor) -> Result<(), String> {
        if let Err(error) = reject_leaks(&self.stage, redactor) {
            let _ = fs::remove_dir_all(&self.stage);
            return Err(error);
        }
        fs::rename(&self.stage, &self.final_dir)
            .map_err(|e| format!("cannot publish evidence: {e}"))?;
        sync_dir(root)
    }

    fn commit_replacing(self, root: &Path, redactor: &Redactor) -> Result<(), String> {
        if let Err(error) = reject_leaks(&self.stage, redactor) {
            let _ = fs::remove_dir_all(&self.stage);
            return Err(error);
        }
        reject_leaks(&self.final_dir, redactor)?;
        #[cfg(target_os = "linux")]
        {
            exchange_directories(&self.stage, &self.final_dir)?;
            sync_dir(root)?;
            let _ = fs::remove_dir_all(&self.stage);
            sync_dir(root)
        }
        #[cfg(not(target_os = "linux"))]
        self.commit_replacing_portably(root)
    }

    #[cfg(not(target_os = "linux"))]
    fn commit_replacing_portably(self, root: &Path) -> Result<(), String> {
        let mut random = [0u8; 8];
        rand::rng().fill_bytes(&mut random);
        let backup = root.join(format!(".evidence-{}.previous", hex::encode(random)));
        fs::rename(&self.final_dir, &backup)
            .map_err(|error| format!("cannot stage prior evidence: {error}"))?;
        if let Err(error) = fs::rename(&self.stage, &self.final_dir) {
            let _ = fs::rename(&backup, &self.final_dir);
            return Err(format!("cannot replace evidence: {error}"));
        }
        sync_dir(root)?;
        fs::remove_dir_all(backup).map_err(|error| error.to_string())?;
        sync_dir(root)
    }
}

#[cfg(target_os = "linux")]
fn exchange_directories(left: &Path, right: &Path) -> Result<(), String> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;

    let left = CString::new(left.as_os_str().as_bytes())
        .map_err(|_| "evidence staging path contains NUL".to_owned())?;
    let right = CString::new(right.as_os_str().as_bytes())
        .map_err(|_| "evidence destination path contains NUL".to_owned())?;
    let result = unsafe {
        libc::renameat2(
            libc::AT_FDCWD,
            left.as_ptr(),
            libc::AT_FDCWD,
            right.as_ptr(),
            libc::RENAME_EXCHANGE,
        )
    };
    (result == 0).then_some(()).ok_or_else(|| {
        format!(
            "cannot atomically replace evidence: {}",
            std::io::Error::last_os_error()
        )
    })
}

fn trace_lines(trace: Vec<Value>) -> Result<String, String> {
    trace
        .into_iter()
        .map(|entry| {
            serde_json::to_string(&entry).map(|mut line| {
                line.push('\n');
                line
            })
        })
        .collect::<Result<String, _>>()
        .map_err(|e| e.to_string())
}

fn reject_leaks(directory: &Path, redactor: &Redactor) -> Result<(), String> {
    let mut pending = vec![directory.to_path_buf()];
    while let Some(path) = pending.pop() {
        for entry in fs::read_dir(&path).map_err(|e| e.to_string())? {
            let path = entry.map_err(|e| e.to_string())?.path();
            if path.is_dir() {
                pending.push(path);
                continue;
            }
            let bytes = fs::read(&path).map_err(|e| e.to_string())?;
            if redactor.contains_export_leak(&bytes) {
                return Err(format!("evidence leak scan rejected {}", path.display()));
            }
        }
    }
    Ok(())
}

pub fn redacted_job(job: &Job, redactor: &Redactor) -> Result<Value, String> {
    let mut output = serde_json::to_value(job).map_err(|e| e.to_string())?;
    let root = output
        .as_object_mut()
        .ok_or_else(|| "serialized job must be an object".to_owned())?;
    if let Some(Value::Object(values)) = root.get_mut("values") {
        redact_caller_keys(values, redactor)?;
    }
    redact_job_strings(&mut output, redactor);
    Ok(output)
}

fn redact_caller_keys(fields: &mut Map<String, Value>, redactor: &Redactor) -> Result<(), String> {
    let original = std::mem::take(fields);
    for (key, value) in original {
        let redacted_key = redactor.redact_export_text(&key);
        if fields.insert(redacted_key.clone(), value).is_some() {
            return Err(format!(
                "sensitive replacement produced duplicate value name {redacted_key:?}"
            ));
        }
    }
    Ok(())
}

fn redact_job_strings(value: &mut Value, redactor: &Redactor) {
    match value {
        Value::String(text) => *text = redactor.redact_export_text(text),
        Value::Array(items) => items
            .iter_mut()
            .for_each(|item| redact_job_strings(item, redactor)),
        Value::Object(fields) => fields.iter_mut().for_each(|(key, item)| {
            if !is_job_protocol_literal(key, item) {
                redact_job_strings(item, redactor);
            }
        }),
        _ => {}
    }
}

fn is_job_protocol_literal(key: &str, value: &Value) -> bool {
    matches!(key, "kind" | "scope") && matches!(value, Value::String(_))
}

fn write_json(
    directory: &Path,
    final_dir: &Path,
    relative: &str,
    role: &str,
    value: &impl Serialize,
    artifacts: &mut Vec<Artifact>,
) -> Result<(), String> {
    write_bytes(
        directory,
        final_dir,
        relative,
        role,
        &pretty(value)?,
        artifacts,
    )
}

fn write_bytes(
    directory: &Path,
    final_dir: &Path,
    relative: &str,
    role: &str,
    bytes: &[u8],
    artifacts: &mut Vec<Artifact>,
) -> Result<(), String> {
    let path = directory.join(relative);
    if let Some(parent) = path.parent() {
        create_private_dir(parent)?;
    }
    write_atomic(&path, bytes)?;
    let root = fs::canonicalize(
        final_dir
            .parent()
            .ok_or_else(|| "invalid evidence directory".to_owned())?,
    )
    .map_err(|e| e.to_string())?;
    let absolute = root
        .join(
            final_dir
                .file_name()
                .ok_or_else(|| "invalid evidence directory".to_owned())?,
        )
        .join(relative);
    artifacts.push(Artifact {
        role: role.into(),
        path: absolute.to_string_lossy().into_owned(),
        digest: hex::encode(Sha256::digest(bytes)),
        complete: true,
    });
    Ok(())
}

fn pretty(value: &impl Serialize) -> Result<Vec<u8>, String> {
    let mut bytes = serde_json::to_vec_pretty(value).map_err(|e| e.to_string())?;
    bytes.push(b'\n');
    Ok(bytes)
}

fn staged_directory(root: &Path, run_id: &str) -> Result<PathBuf, String> {
    let mut random = [0u8; 8];
    rand::rng().fill_bytes(&mut random);
    let path = root.join(format!(".{run_id}.{}.staging", hex::encode(random)));
    create_private_dir(&path)?;
    Ok(path)
}

fn create_private_dir(path: &Path) -> Result<(), String> {
    fs::create_dir_all(path).map_err(|e| e.to_string())?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700)).map_err(|e| e.to_string())?;
    }
    Ok(())
}

fn write_atomic(path: &Path, bytes: &[u8]) -> Result<(), String> {
    let temporary = path.with_extension(format!("tmp-{}", std::process::id()));
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(&temporary).map_err(|e| e.to_string())?;
    file.write_all(bytes)
        .and_then(|()| file.sync_all())
        .map_err(|e| e.to_string())?;
    fs::rename(&temporary, path).map_err(|e| e.to_string())?;
    Ok(())
}

fn sync_dir(path: &Path) -> Result<(), String> {
    fs::File::open(path)
        .and_then(|f| f.sync_all())
        .map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use tempfile::TempDir;

    #[test]
    fn nested_and_overlapping_secrets_do_not_leak() {
        let job: Job=serde_json::from_value(json!({"schema_version":1,"target":{"kind":"browser","url":"http://127.0.0.1/"},"context":{"journey":"j","revision":"r","environment":"e","actor":"a","authority":"a"},"values":{"small":{"value":"token","description":"small","secret":true},"large":{"value":"token-long","description":"large","secret":true}},"steps":[{"id":"s","goal":"g","done_when":[{"url_contains":"/"}]}]})).unwrap();
        let redactor = Redactor::for_job(&job).unwrap();
        let text = redactor.redact_text("token-long token");
        assert!(!text.contains("token"));
    }

    #[test]
    fn inherited_provider_key_is_part_of_redaction_and_terminal_leak_scanning() {
        let job = test_job(false);
        let key = "provider-key-from-bootstrap";
        let redactor = Redactor::for_job_with_provider_key(&job, Some(key)).unwrap();
        assert!(!redactor.redact_export_text(key).contains(key));
        assert!(redactor.contains_sensitive(key));
        assert!(redactor.contains_export_leak(key.as_bytes()));
    }

    fn test_job(secret: bool) -> Job {
        serde_json::from_value(json!({"schema_version":1,"target":{"kind":"browser","url":"http://127.0.0.1/"},"context":{"journey":"j","revision":"r","environment":"e","actor":"a","authority":"a"},"values":{"marker":{"value":"classified-marker","description":"marker","secret":secret}},"steps":[{"id":"s","goal":"g","done_when":[{"url_contains":"/"}]}]})).unwrap()
    }

    fn bundle(text: &str, screenshot: Vec<u8>) -> EvidenceBundle {
        EvidenceBundle {
            complete: true,
            job: json!({"safe":text}),
            provenance: json!({"browser":text}),
            observations: vec![("o_1".into(), json!({"title":text}), Some(screenshot))],
            decisions: Vec::new(),
            steps: vec![("s".into(), json!({"done":"satisfied"}))],
            escalations: Vec::new(),
            dispositions: Vec::new(),
            verification: None,
            trace: vec![json!({"event":text})],
            cleanup: json!({"browser":"closed"}),
            result: json!({"state":"passed"}),
        }
    }

    #[test]
    fn publisher_writes_private_manifested_artifacts_with_final_paths() {
        let temporary = TempDir::new().unwrap();
        let job = test_job(false);
        let redactor = Redactor::for_job(&job).unwrap();
        publish(
            temporary.path(),
            "r_test",
            bundle("safe", b"png".to_vec()),
            &redactor,
        )
        .unwrap();
        let manifest: Manifest = serde_json::from_slice(
            &fs::read(temporary.path().join("r_test/manifest.json")).unwrap(),
        )
        .unwrap();
        assert!(manifest.complete);
        assert!(
            manifest
                .artifacts
                .iter()
                .all(|artifact| artifact.path.contains("/r_test/")
                    && !artifact.path.contains(".staging"))
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(temporary.path().join("r_test/result.json"))
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
        }
    }

    #[test]
    fn publisher_manifests_final_verification_at_the_contract_path() {
        let temporary = TempDir::new().unwrap();
        let job = test_job(false);
        let redactor = Redactor::for_job(&job).unwrap();
        let mut evidence = bundle("safe", b"png".to_vec());
        evidence.verification = Some(json!({"phase":"verification","expectations":[]}));
        publish(temporary.path(), "r_verify", evidence, &redactor).unwrap();
        let manifest: Manifest = serde_json::from_slice(
            &fs::read(temporary.path().join("r_verify/manifest.json")).unwrap(),
        )
        .unwrap();
        let artifact = manifest
            .artifacts
            .iter()
            .find(|artifact| artifact.role == "verification")
            .unwrap();
        assert_eq!(
            Path::new(&artifact.path),
            fs::canonicalize(temporary.path())
                .unwrap()
                .join("r_verify/verification/final.json")
        );
        assert!(artifact.complete);
    }

    #[cfg(unix)]
    #[test]
    fn hosted_tail_replacement_accepts_a_noncanonical_root_alias() {
        let temporary = TempDir::new().unwrap();
        let canonical_root = temporary.path().join("evidence");
        fs::create_dir(&canonical_root).unwrap();
        let root_alias = temporary.path().join("evidence-alias");
        std::os::unix::fs::symlink(&canonical_root, &root_alias).unwrap();
        let job = test_job(false);
        let redactor = Redactor::for_job(&job).unwrap();
        publish(
            &root_alias,
            "r_alias",
            bundle("safe", b"png".to_vec()),
            &redactor,
        )
        .unwrap();

        replace_result_cleanup(
            &root_alias,
            "r_alias",
            &json!({"browser":"closed"}),
            &json!({"state":"expired","evidence":{"complete":true}}),
            &redactor,
        )
        .unwrap();
    }

    #[test]
    fn hosted_republication_atomically_replaces_its_own_complete_bundle() {
        let temporary = TempDir::new().unwrap();
        let job = test_job(false);
        let redactor = Redactor::for_job(&job).unwrap();
        publish(
            temporary.path(),
            "r_replace",
            bundle("first", vec![1, 2, 3]),
            &redactor,
        )
        .unwrap();
        let second = publish(
            temporary.path(),
            "r_replace",
            bundle("second", vec![4, 5, 6]),
            &redactor,
        )
        .unwrap();
        assert_eq!(second["state"], "passed");
        let trace = fs::read_to_string(temporary.path().join("r_replace/trace.jsonl")).unwrap();
        assert!(trace.contains("second"));
        assert!(!trace.contains("first"));
        let manifest: Manifest = serde_json::from_slice(
            &fs::read(temporary.path().join("r_replace/manifest.json")).unwrap(),
        )
        .unwrap();
        assert!(manifest.complete);
        assert!(manifest.artifacts.iter().all(|artifact| artifact.complete));
    }

    #[test]
    fn hosted_republication_refuses_corrupt_or_unmanifested_prior_evidence() {
        for corruption in ["digest", "extra"] {
            let temporary = TempDir::new().unwrap();
            let job = test_job(false);
            let redactor = Redactor::for_job(&job).unwrap();
            publish(
                temporary.path(),
                "r_corrupt",
                bundle("first", vec![1, 2, 3]),
                &redactor,
            )
            .unwrap();
            let run_dir = temporary.path().join("r_corrupt");
            if corruption == "digest" {
                fs::write(run_dir.join("trace.jsonl"), b"tampered\n").unwrap();
            } else {
                fs::write(run_dir.join("extra.json"), b"{}\n").unwrap();
            }
            let error = publish(
                temporary.path(),
                "r_corrupt",
                bundle("second", vec![4, 5, 6]),
                &redactor,
            )
            .unwrap_err();
            assert!(
                error.contains("digest") || error.contains("unmanifested"),
                "unexpected validation error: {error}"
            );
        }
    }

    #[test]
    fn replacement_rejects_incomplete_roles_duplicates_unknown_roles_and_unsafe_leaves() {
        for corruption in ["missing", "duplicate", "unknown", "nested"] {
            let temporary = TempDir::new().unwrap();
            let job = test_job(false);
            let redactor = Redactor::for_job(&job).unwrap();
            publish(
                temporary.path(),
                "r_shape",
                bundle("first", vec![1, 2, 3]),
                &redactor,
            )
            .unwrap();
            let run_dir = temporary.path().join("r_shape");
            let manifest_path = run_dir.join("manifest.json");
            let mut manifest: Manifest =
                serde_json::from_slice(&fs::read(&manifest_path).unwrap()).unwrap();
            match corruption {
                "missing" => {
                    manifest
                        .artifacts
                        .retain(|artifact| artifact.role != "provenance");
                    fs::remove_file(run_dir.join("provenance.json")).unwrap();
                }
                "duplicate" => {
                    let cleanup = manifest
                        .artifacts
                        .iter()
                        .find(|artifact| artifact.role == "cleanup")
                        .unwrap()
                        .clone();
                    manifest.artifacts.push(cleanup);
                }
                "unknown" => {
                    manifest
                        .artifacts
                        .iter_mut()
                        .find(|artifact| artifact.role == "observation")
                        .unwrap()
                        .role = "unknown".into();
                }
                "nested" => {
                    let old = run_dir.join("observations/o_1.json");
                    let nested = run_dir.join("observations/nested/o_1.json");
                    fs::create_dir_all(nested.parent().unwrap()).unwrap();
                    fs::rename(&old, &nested).unwrap();
                    manifest
                        .artifacts
                        .iter_mut()
                        .find(|artifact| artifact.role == "observation")
                        .unwrap()
                        .path = fs::canonicalize(&nested)
                        .unwrap()
                        .to_string_lossy()
                        .into_owned();
                }
                _ => unreachable!(),
            }
            fs::write(
                &manifest_path,
                serde_json::to_vec_pretty(&manifest).unwrap(),
            )
            .unwrap();
            let error = publish(
                temporary.path(),
                "r_shape",
                bundle("second", vec![4, 5, 6]),
                &redactor,
            )
            .unwrap_err();
            assert!(
                error.contains("exactly one")
                    || error.contains("repeats")
                    || error.contains("unexpected"),
                "{corruption}: {error}"
            );
        }
    }

    #[test]
    fn replacement_leak_scans_the_old_bundle_before_atomic_exchange() {
        let temporary = TempDir::new().unwrap();
        let job = test_job(true);
        let redactor = Redactor::for_job(&job).unwrap();
        publish(
            temporary.path(),
            "r_old_leak",
            bundle("safe", vec![1, 2, 3]),
            &redactor,
        )
        .unwrap();
        let run_dir = temporary.path().join("r_old_leak");
        let trace_path = run_dir.join("trace.jsonl");
        fs::write(&trace_path, b"classified-marker\n").unwrap();
        let manifest_path = run_dir.join("manifest.json");
        let mut manifest: Manifest =
            serde_json::from_slice(&fs::read(&manifest_path).unwrap()).unwrap();
        manifest
            .artifacts
            .iter_mut()
            .find(|artifact| artifact.role == "trace")
            .unwrap()
            .digest = hex::encode(Sha256::digest(fs::read(&trace_path).unwrap()));
        fs::write(
            &manifest_path,
            serde_json::to_vec_pretty(&manifest).unwrap(),
        )
        .unwrap();

        let error = publish(
            temporary.path(),
            "r_old_leak",
            bundle("replacement", vec![4, 5, 6]),
            &redactor,
        )
        .unwrap_err();
        assert!(error.contains("leak scan"), "{error}");
        assert_eq!(fs::read(&trace_path).unwrap(), b"classified-marker\n");
    }

    #[test]
    fn publisher_preserves_an_incomplete_after_dispatch_marker() {
        let temporary = TempDir::new().unwrap();
        let job = test_job(false);
        let redactor = Redactor::for_job(&job).unwrap();
        let mut incomplete = bundle("safe", b"png".to_vec());
        incomplete.complete = false;
        incomplete.result["evidence"] = json!({"complete":false});
        let result = publish(temporary.path(), "r_incomplete", incomplete, &redactor).unwrap();
        let manifest: Manifest = serde_json::from_slice(
            &fs::read(temporary.path().join("r_incomplete/manifest.json")).unwrap(),
        )
        .unwrap();
        assert!(!manifest.complete);
        assert_eq!(result["evidence"]["complete"], false);
    }

    #[test]
    fn hosted_terminal_tail_replacement_updates_manifest_digests() {
        let temporary = TempDir::new().unwrap();
        let job = test_job(false);
        let redactor = Redactor::for_job(&job).unwrap();
        publish(
            temporary.path(),
            "r_hosted",
            bundle("safe", b"png".to_vec()),
            &redactor,
        )
        .unwrap();
        let cleanup = json!({"browser":"closed"});
        let result = json!({"state":"expired","evidence":{"complete":true}});
        replace_result_cleanup(temporary.path(), "r_hosted", &cleanup, &result, &redactor).unwrap();
        let manifest: Manifest = serde_json::from_slice(
            &fs::read(temporary.path().join("r_hosted/manifest.json")).unwrap(),
        )
        .unwrap();
        for artifact in manifest
            .artifacts
            .iter()
            .filter(|artifact| matches!(artifact.role.as_str(), "cleanup" | "result"))
        {
            assert_eq!(
                artifact.digest,
                hex::encode(Sha256::digest(fs::read(&artifact.path).unwrap()))
            );
        }
    }

    #[test]
    fn interrupted_hosted_tail_never_leaves_a_complete_stale_manifest() {
        let temporary = TempDir::new().unwrap();
        let job = test_job(false);
        let redactor = Redactor::for_job(&job).unwrap();
        publish(
            temporary.path(),
            "r_interrupted",
            bundle("safe", b"png".to_vec()),
            &redactor,
        )
        .unwrap();
        let run_dir = temporary.path().join("r_interrupted");
        fs::write(
            run_dir.join(format!("cleanup.tmp-{}", std::process::id())),
            b"occupied",
        )
        .unwrap();
        let error = replace_result_cleanup(
            temporary.path(),
            "r_interrupted",
            &json!({"browser":"closed"}),
            &json!({"state":"expired","evidence":{"complete":true}}),
            &redactor,
        )
        .unwrap_err();
        assert!(error.contains("exists"));
        let manifest: Manifest =
            serde_json::from_slice(&fs::read(run_dir.join("manifest.json")).unwrap()).unwrap();
        assert!(!manifest.complete);
        assert!(
            manifest
                .artifacts
                .iter()
                .filter(|artifact| matches!(artifact.role.as_str(), "cleanup" | "result"))
                .all(|artifact| !artifact.complete)
        );
    }

    #[test]
    fn final_leak_scan_rejects_json_and_binary_markers_before_publish() {
        let temporary = TempDir::new().unwrap();
        let job = test_job(true);
        let redactor = Redactor::for_job(&job).unwrap();
        let error = publish(
            temporary.path(),
            "r_leak",
            bundle("safe", b"classified-marker".to_vec()),
            &redactor,
        )
        .unwrap_err();
        assert!(error.contains("leak scan"));
        assert!(!temporary.path().join("r_leak").exists());
        assert_eq!(fs::read_dir(temporary.path()).unwrap().count(), 0);
    }

    #[test]
    fn redacted_job_masks_classified_values_everywhere() {
        let job = test_job(true);
        let redactor = Redactor::for_job(&job).unwrap();
        let output = redacted_job(&job, &redactor).unwrap();
        assert!(
            !serde_json::to_string(&output)
                .unwrap()
                .contains("classified-marker")
        );
    }
}
