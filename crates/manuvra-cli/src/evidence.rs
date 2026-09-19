use std::collections::{BTreeMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

use manuvra_contract::{
    Artifact, Cleanup, EvidenceRef, ExpectationVerdict, Job, Manifest, Reason, RunResult, RunState,
    SchemaVersion, StepVerdict, Verdict, VerdictResult,
};
use rand::{Rng, RngCore, distr::Alphanumeric};
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};

use crate::store;

pub enum BlockedStop {
    MissingValue { value_name: String, step_id: String },
    Unsupported { feature: &'static str },
}

impl BlockedStop {
    pub fn missing_value(value_name: String, step_id: String) -> Self {
        Self::MissingValue {
            value_name,
            step_id,
        }
    }

    pub fn unsupported(feature: &'static str) -> Self {
        Self::Unsupported { feature }
    }

    fn into_reason(self, redactor: &Redactor) -> Reason {
        match self {
            Self::MissingValue {
                value_name,
                step_id,
            } => Reason {
                code: "missing_value".into(),
                details: BTreeMap::from([
                    ("step_id".into(), json!(redactor.redact_text(&step_id))),
                    (
                        "value_name".into(),
                        json!(redactor.redact_text(&value_name)),
                    ),
                ]),
            },
            Self::Unsupported { feature } => Reason {
                code: "unsupported_in_this_build".into(),
                details: BTreeMap::from([("feature".into(), json!(feature))]),
            },
        }
    }
}

pub struct Published {
    pub result: Value,
}

#[derive(Clone)]
pub struct Redactor {
    replacements: Vec<(String, String)>,
}

impl Redactor {
    pub fn for_job(job: &Job) -> Result<Self, String> {
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
            .flat_map(|(_, value)| value_renderings(value))
            .filter(|rendering| !rendering.is_empty())
            .filter(|rendering| seen.insert((*rendering).to_owned()))
            .map(str::to_owned)
            .collect();
        let used_characters = job_characters(job);
        let mut placeholders =
            private_use_characters().filter(|candidate| !used_characters.contains(candidate));
        let mut replacements = Vec::new();
        for (index, rendering) in renderings.iter().enumerate() {
            let boundary = placeholders.next().ok_or_else(|| {
                "job has too many distinct classified value renderings".to_owned()
            })?;
            let readable = format!("{boundary}<masked:{}>{boundary}", index + 1);
            let placeholder = if renderings
                .iter()
                .any(|sensitive| readable.contains(sensitive.as_str()))
            {
                boundary.to_string()
            } else {
                readable
            };
            replacements.push((rendering.clone(), placeholder));
        }
        replacements.sort_by(|left, right| {
            right
                .0
                .len()
                .cmp(&left.0.len())
                .then_with(|| left.0.cmp(&right.0))
        });
        Ok(Self { replacements })
    }

    pub fn redact_text(&self, text: &str) -> String {
        let mut output = String::with_capacity(text.len());
        let mut offset = 0;
        while offset < text.len() {
            let remaining = &text[offset..];
            if let Some((sensitive, placeholder)) = self
                .replacements
                .iter()
                .find(|(sensitive, _)| remaining.starts_with(sensitive))
            {
                output.push_str(placeholder);
                offset += sensitive.len();
            } else {
                let character = remaining
                    .chars()
                    .next()
                    .expect("offset is before the end of the string");
                output.push(character);
                offset += character.len_utf8();
            }
        }
        output
    }

    pub fn contains_sensitive(&self, text: &str) -> bool {
        self.replacements
            .iter()
            .any(|(sensitive, _)| text.contains(sensitive))
    }
}

fn job_characters(job: &Job) -> HashSet<char> {
    let value = serde_json::to_value(job).expect("a validated job serializes");
    let mut characters = HashSet::new();
    collect_characters(&value, &mut characters);
    characters
}

fn collect_characters(value: &Value, characters: &mut HashSet<char>) {
    match value {
        Value::String(text) => characters.extend(text.chars()),
        Value::Array(items) => {
            for item in items {
                collect_characters(item, characters);
            }
        }
        Value::Object(fields) => {
            for (key, value) in fields {
                characters.extend(key.chars());
                collect_characters(value, characters);
            }
        }
        _ => {}
    }
}

fn private_use_characters() -> impl Iterator<Item = char> {
    (0xE000..=0xF8FF)
        .chain(0xF0000..=0xFFFFD)
        .chain(0x100000..=0x10FFFD)
        .filter_map(char::from_u32)
}

fn value_renderings(value: &manuvra_contract::JobValue) -> impl Iterator<Item = &str> {
    std::iter::once(value.value.as_str()).chain(value.formats.iter().flat_map(|formats| {
        [formats.iso.as_deref(), formats.display.as_deref()]
            .into_iter()
            .flatten()
    }))
}

pub fn prepare_root(root: &Path, redactor: &Redactor) -> Result<PathBuf, String> {
    if root.to_str().is_none() {
        return Err("evidence path must be valid UTF-8".into());
    }
    store::create_private_dir(root)?;
    let absolute = canonicalize_root(root)?;
    let text = path_text(&absolute)?;
    if redactor.contains_sensitive(&text) {
        return Err("evidence path contains a classified value rendering".into());
    }
    Ok(absolute)
}

pub fn publish(
    evidence_root: &Path,
    request_id: &str,
    run_id: &str,
    job: &Job,
    stop: BlockedStop,
    redactor: &Redactor,
) -> Result<Published, String> {
    store::create_private_dir(evidence_root)?;
    let final_paths = EvidencePaths::at(evidence_root.join(run_id));
    let expected = expected_evidence(&final_paths, request_id, run_id, job, stop, redactor)?;
    if final_paths.directory.exists() {
        return recover_published(&final_paths, expected);
    }
    publish_staged(evidence_root, run_id, &final_paths, &expected)?;
    Ok(Published {
        result: expected.result,
    })
}

fn recover_published(
    paths: &EvidencePaths,
    expected: ExpectedEvidence,
) -> Result<Published, String> {
    validate_existing(paths, &expected)?;
    Ok(Published {
        result: expected.result,
    })
}

fn publish_staged(
    evidence_root: &Path,
    run_id: &str,
    final_paths: &EvidencePaths,
    expected: &ExpectedEvidence,
) -> Result<(), String> {
    let stage = StageDirectory::create(evidence_root, run_id)?;
    let stage_paths = EvidencePaths::at(stage.path.clone());
    store::atomic_write_private(&stage_paths.job, &expected.job_bytes)?;
    store::atomic_write_private(&stage_paths.manifest, &expected.manifest_bytes)?;
    store::atomic_write_private(&stage_paths.result, &expected.result_bytes)?;
    stage.commit(&final_paths.directory)
}

struct EvidencePaths {
    directory: PathBuf,
    job: PathBuf,
    result: PathBuf,
    manifest: PathBuf,
}

impl EvidencePaths {
    fn at(directory: PathBuf) -> Self {
        Self {
            job: directory.join("job.json"),
            result: directory.join("result.json"),
            manifest: directory.join("manifest.json"),
            directory,
        }
    }
}

struct ExpectedEvidence {
    job_bytes: Vec<u8>,
    result: Value,
    result_bytes: Vec<u8>,
    manifest_bytes: Vec<u8>,
}

fn expected_evidence(
    paths: &EvidencePaths,
    request_id: &str,
    run_id: &str,
    job: &Job,
    stop: BlockedStop,
    redactor: &Redactor,
) -> Result<ExpectedEvidence, String> {
    let job_bytes = redacted_job_bytes(job, redactor)?;
    let result = blocked_result(request_id, run_id, job, stop, &paths.manifest, redactor)?;
    let result_bytes = pretty_json(&result)?;
    let manifest = Manifest {
        schema_version: SchemaVersion,
        run_id: run_id.to_owned(),
        complete: true,
        artifacts: vec![
            artifact("normalized_job", &paths.job, &job_bytes)?,
            artifact("result", &paths.result, &result_bytes)?,
        ],
    };
    let manifest_bytes = pretty_json(&manifest)?;
    Ok(ExpectedEvidence {
        job_bytes,
        result,
        result_bytes,
        manifest_bytes,
    })
}

fn validate_existing(paths: &EvidencePaths, expected: &ExpectedEvidence) -> Result<(), String> {
    store::create_private_dir(&paths.directory)?;
    for (path, wanted) in [
        (&paths.job, &expected.job_bytes),
        (&paths.result, &expected.result_bytes),
        (&paths.manifest, &expected.manifest_bytes),
    ] {
        let actual = store::read_private(path, "evidence artifact")?;
        if actual != *wanted {
            return Err(format!(
                "existing evidence artifact {} does not match its request intent",
                path.display()
            ));
        }
    }
    Ok(())
}

struct StageDirectory {
    path: PathBuf,
    committed: bool,
}

impl StageDirectory {
    fn create(root: &Path, run_id: &str) -> Result<Self, String> {
        let mut random = [0_u8; 8];
        rand::rng().fill_bytes(&mut random);
        let path = root.join(format!(".{run_id}.{}.staging", hex::encode(random)));
        fs::create_dir(&path)
            .map_err(|error| format!("cannot create evidence staging directory: {error}"))?;
        store::create_private_dir(&path)?;
        Ok(Self {
            path,
            committed: false,
        })
    }

    fn commit(mut self, final_path: &Path) -> Result<(), String> {
        ensure_publish_target_absent(final_path)?;
        rename_evidence(&self.path, final_path)?;
        self.committed = true;
        Ok(())
    }
}

fn ensure_publish_target_absent(final_path: &Path) -> Result<(), String> {
    match fs::symlink_metadata(final_path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Ok(_) => Err(format!(
            "evidence directory {} already exists",
            final_path.display()
        )),
        Err(error) => Err(format!(
            "cannot inspect evidence directory {}: {error}",
            final_path.display()
        )),
    }
}

fn rename_evidence(stage: &Path, final_path: &Path) -> Result<(), String> {
    fs::rename(stage, final_path).map_err(|error| {
        format!(
            "cannot publish evidence directory {}: {error}",
            final_path.display()
        )
    })?;
    sync_directory(final_path.parent().unwrap_or(Path::new(".")))
}

impl Drop for StageDirectory {
    fn drop(&mut self) {
        if !self.committed {
            let _ = fs::remove_dir_all(&self.path);
        }
    }
}

fn canonicalize_root(root: &Path) -> Result<PathBuf, String> {
    fs::canonicalize(root)
        .map_err(|error| format!("cannot resolve evidence root {}: {error}", root.display()))
}

fn redacted_job_bytes(job: &Job, redactor: &Redactor) -> Result<Vec<u8>, String> {
    let mut output = serde_json::to_value(job).map_err(|error| error.to_string())?;
    redact_job(&mut output, redactor)?;
    pretty_json(&output)
}

fn redact_job(output: &mut Value, redactor: &Redactor) -> Result<(), String> {
    let root = output
        .as_object_mut()
        .ok_or_else(|| "serialized job must be an object".to_owned())?;
    if let Some(Value::Object(values)) = root.get_mut("values") {
        redact_caller_keys(values, redactor)?;
    }
    redact_job_strings(output, redactor);
    Ok(())
}

fn redact_caller_keys(fields: &mut Map<String, Value>, redactor: &Redactor) -> Result<(), String> {
    let original = std::mem::take(fields);
    for (key, value) in original {
        let redacted_key = redactor.redact_text(&key);
        if fields.insert(redacted_key.clone(), value).is_some() {
            return Err(format!(
                "classified value replacement produced duplicate value name {redacted_key:?}"
            ));
        }
    }
    Ok(())
}

fn redact_job_strings(value: &mut Value, redactor: &Redactor) {
    match value {
        Value::String(text) => *text = redactor.redact_text(text),
        Value::Array(items) => redact_job_array(items, redactor),
        Value::Object(fields) => redact_job_object(fields, redactor),
        _ => {}
    }
}

fn redact_job_array(items: &mut [Value], redactor: &Redactor) {
    for item in items {
        redact_job_strings(item, redactor);
    }
}

fn redact_job_object(fields: &mut Map<String, Value>, redactor: &Redactor) {
    for (key, value) in fields {
        if !is_protocol_literal(key, value) {
            redact_job_strings(value, redactor);
        }
    }
}

fn is_protocol_literal(key: &str, value: &Value) -> bool {
    matches!(key, "kind" | "scope") && matches!(value, Value::String(_))
}

fn blocked_result(
    request_id: &str,
    run_id: &str,
    job: &Job,
    stop: BlockedStop,
    manifest_path: &Path,
    redactor: &Redactor,
) -> Result<Value, String> {
    let result = RunResult {
        schema_version: SchemaVersion,
        request_id: redactor.redact_text(request_id),
        run_id: run_id.to_owned(),
        state: RunState::Blocked,
        terminal: true,
        reason: Some(stop.into_reason(redactor)),
        verdict: Verdict {
            overall: VerdictResult::Unresolved,
            steps: job
                .steps
                .iter()
                .map(|step| StepVerdict {
                    id: redactor.redact_text(&step.id),
                    result: VerdictResult::NotRun,
                    basis: None,
                })
                .collect(),
            expectations: job
                .expectations
                .iter()
                .map(|expectation| ExpectationVerdict {
                    id: redactor.redact_text(&expectation.id),
                    result: VerdictResult::NotRun,
                    noul: None,
                    numeric_checks: Vec::new(),
                })
                .collect(),
            caller_assisted: false,
        },
        evidence: EvidenceRef {
            manifest: path_text(manifest_path)?,
            complete: true,
        },
        escalation: None,
        cleanup: Cleanup {
            browser: "not_started".into(),
            profile: "not_created".into(),
            application_state: "caller_owned".into(),
        },
    };
    serde_json::to_value(result).map_err(|error| error.to_string())
}

fn artifact(role: &str, path: &Path, contents: &[u8]) -> Result<Artifact, String> {
    Ok(Artifact {
        role: role.into(),
        path: path_text(path)?,
        digest: sha256(contents),
        complete: true,
    })
}

fn path_text(path: &Path) -> Result<String, String> {
    path.to_str()
        .map(str::to_owned)
        .ok_or_else(|| format!("path is not valid UTF-8: {}", path.display()))
}

fn pretty_json(value: &impl serde::Serialize) -> Result<Vec<u8>, String> {
    let mut bytes = serde_json::to_vec_pretty(value).map_err(|error| error.to_string())?;
    bytes.push(b'\n');
    Ok(bytes)
}

pub fn new_run_id() -> String {
    let suffix: String = rand::rng()
        .sample_iter(&Alphanumeric)
        .take(16)
        .map(char::from)
        .collect();
    format!("r_{suffix}")
}

pub fn sha256(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

fn sync_directory(path: &Path) -> Result<(), String> {
    fs::File::open(path)
        .and_then(|directory| directory.sync_all())
        .map_err(|error| format!("cannot sync evidence root {}: {error}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn uncommitted_staged_result_is_never_a_published_run() {
        let temporary = TempDir::new().unwrap();
        let root = temporary.path().join("evidence");
        store::create_private_dir(&root).unwrap();
        let final_path = root.join("r_interrupted");
        {
            let stage = StageDirectory::create(&root, "r_interrupted").unwrap();
            store::atomic_write_private(
                &stage.path.join("result.json"),
                br#"{"evidence":{"complete":true}}"#,
            )
            .unwrap();
            assert!(!final_path.exists());
        }
        assert!(!final_path.exists());
        assert_eq!(fs::read_dir(root).unwrap().count(), 0);
    }

    #[test]
    fn staged_commit_refuses_an_existing_run_directory() {
        let temporary = TempDir::new().unwrap();
        let final_path = temporary.path().join("r_existing");
        fs::create_dir(&final_path).unwrap();
        let error = ensure_publish_target_absent(&final_path).unwrap_err();
        assert!(error.contains("already exists"));
    }
}
