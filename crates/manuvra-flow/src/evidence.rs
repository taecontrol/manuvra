use manuvra_contract::{Artifact, Job, Manifest, SchemaVersion};
use rand::RngCore;
use serde::Serialize;
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};
use std::collections::HashSet;
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
        "document_id",
        "route",
        "title",
        "dialogs",
        "focused",
        "visible_text",
        "covered_text",
        "dialog_texts",
        "elements",
        "index",
        "node_id",
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
    if final_dir.exists() {
        return Err("evidence directory already exists".into());
    }
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
    publication.commit(root, redactor)?;
    Ok(result)
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
    (!metadata.file_type().is_symlink() && metadata.is_dir())
        .then_some(path)
        .ok_or_else(|| "evidence run path is not a regular directory".into())
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
        self.write_tail(bundle.trace, &bundle.cleanup, &bundle.result)
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
