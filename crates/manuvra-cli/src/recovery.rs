//! Validation of published run evidence before a request is answered from it. A recovered
//! result is trusted only when its manifest, every listed artifact, and the result's identity
//! match exactly what the evidence writer publishes.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

use manuvra_contract::Manifest;
use serde_json::Value;

/// Returns the terminal result and exit code published for `run_id` under `evidence_root`, or
/// `None` when that run published no evidence.
pub(crate) fn recover_published_run(
    evidence_root: &Path,
    run_id: &str,
    request_id: &str,
) -> Result<Option<(Value, u8)>, String> {
    let Some(run_dir) = existing_run_directory(evidence_root.join(run_id))? else {
        return Ok(None);
    };
    let result =
        validate_published_evidence(&run_dir.join("manifest.json"), run_id, request_id, None)?;
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

pub(crate) fn validate_published_evidence(
    manifest_path: &Path,
    run_id: &str,
    request_id: &str,
    recorded_result: Option<&Value>,
) -> Result<Value, String> {
    validate_published_result(manifest_path, run_id, request_id, recorded_result, true)
}

pub(crate) fn validate_published_result(
    manifest_path: &Path,
    run_id: &str,
    request_id: &str,
    recorded_result: Option<&Value>,
    require_terminal: bool,
) -> Result<Value, String> {
    let manifest_path = canonical_regular_file(manifest_path)?;
    let run_dir = manifest_path
        .parent()
        .ok_or_else(|| "prior manifest has no evidence directory".to_owned())?;
    let manifest = read_prior_manifest(&manifest_path, run_id)?;
    let artifacts = validate_manifest_artifacts(run_dir, &manifest)?;
    let result = read_published_result(&artifacts)?;
    validate_evidence_shape(&manifest, &result, require_terminal)?;
    validate_result_identity(&result, run_id, request_id, &manifest_path)?;
    recorded_result
        .is_none_or(|recorded| recorded == &result)
        .then_some(result)
        .ok_or_else(|| "completed request record does not match published result".into())
}

fn validate_evidence_shape(
    manifest: &Manifest,
    result: &Value,
    require_terminal: bool,
) -> Result<(), String> {
    validate_terminal_shape(result, require_terminal)?;
    validate_flow_artifact_shape(manifest, result)?;
    validate_verification_artifact_shape(manifest, result)?;
    validate_passed_verdict_shape(result)
}

fn validate_terminal_shape(result: &Value, require_terminal: bool) -> Result<(), String> {
    if require_terminal && result.get("terminal").and_then(Value::as_bool) != Some(true) {
        return Err("complete evidence does not contain a terminal result".into());
    }
    Ok(())
}

fn validate_flow_artifact_shape(manifest: &Manifest, result: &Value) -> Result<(), String> {
    let short = manifest.artifacts.len() == 2;
    let admission_reason = result
        .pointer("/reason/code")
        .and_then(Value::as_str)
        .is_some_and(|code| code == "missing_value");
    let admission_cleanup = result.pointer("/cleanup/browser").and_then(Value::as_str)
        == Some("not_started")
        && result.pointer("/cleanup/profile").and_then(Value::as_str) == Some("not_created");
    if short && !(admission_reason && admission_cleanup) {
        return Err("prior result is missing required flow artifacts".into());
    }
    Ok(())
}

fn validate_verification_artifact_shape(manifest: &Manifest, result: &Value) -> Result<(), String> {
    let verification_count = manifest
        .artifacts
        .iter()
        .filter(|artifact| artifact.role == "verification")
        .count();
    if verification_count > 1 {
        return Err("prior manifest repeats singleton role verification".into());
    }
    let expectations_evaluated = result
        .pointer("/verdict/expectations")
        .and_then(Value::as_array)
        .is_some_and(|expectations| {
            expectations.iter().any(|expectation| {
                expectation.get("result").and_then(Value::as_str) != Some("not_run")
            })
        });
    let verification_phase =
        result.pointer("/escalation/phase").and_then(Value::as_str) == Some("verification");
    let passed = result.get("state").and_then(Value::as_str) == Some("passed");
    if (passed || expectations_evaluated || verification_phase) && verification_count != 1 {
        return Err("prior result is missing its final verification artifact".into());
    }
    Ok(())
}

fn validate_passed_verdict_shape(result: &Value) -> Result<(), String> {
    if result.get("state").and_then(Value::as_str) != Some("passed") {
        return Ok(());
    }
    let overall_satisfied =
        result.pointer("/verdict/overall").and_then(Value::as_str) == Some("satisfied");
    let all_satisfied = ["/verdict/steps", "/verdict/expectations"]
        .into_iter()
        .all(|pointer| verdict_array_is_satisfied(result, pointer));
    if !overall_satisfied || !all_satisfied {
        return Err("passed result contains an incomplete verdict".into());
    }
    Ok(())
}

fn verdict_array_is_satisfied(result: &Value, pointer: &str) -> bool {
    result
        .pointer(pointer)
        .and_then(Value::as_array)
        .is_some_and(|verdicts| {
            verdicts
                .iter()
                .all(|verdict| verdict.get("result").and_then(Value::as_str) == Some("satisfied"))
        })
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
    const SINGLETONS: &[&str] = &[
        "normalized_job",
        "result",
        "provenance",
        "trace",
        "cleanup",
        "verification",
    ];
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
        ("verification", "verification/final.json"),
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
        ("disposition", "dispositions/", ".json"),
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

pub(crate) fn result_exit_code(result: &Value) -> Result<u8, String> {
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

#[cfg(test)]
mod tests {
    use super::{role_matches_path, validate_evidence_shape};
    use manuvra_contract::{Artifact, Manifest, SchemaVersion};
    use serde_json::json;

    #[test]
    fn completed_resume_checkpoint_may_be_nonterminal_but_product_may_not() {
        let artifact = |role: &str| Artifact {
            role: role.into(),
            path: format!("/e/{role}.json"),
            digest: "0".repeat(64),
            complete: true,
        };
        let manifest = Manifest {
            schema_version: SchemaVersion,
            run_id: "r_checkpoint".into(),
            complete: true,
            artifacts: vec![
                artifact("normalized_job"),
                artifact("result"),
                artifact("trace"),
            ],
        };
        let checkpoint = json!({"terminal":false,"state":"uncertain"});
        assert!(validate_evidence_shape(&manifest, &checkpoint, false).is_ok());
        assert!(validate_evidence_shape(&manifest, &checkpoint, true).is_err());
    }

    #[test]
    fn final_verification_role_has_one_exact_safe_path() {
        assert!(role_matches_path(
            "verification",
            std::path::Path::new("verification/final.json")
        ));
        assert!(!role_matches_path(
            "verification",
            std::path::Path::new("verification/other.json")
        ));
        assert!(!role_matches_path(
            "verification",
            std::path::Path::new("../verification/final.json")
        ));
    }

    #[test]
    fn passed_recovery_requires_one_verification_artifact_and_satisfied_verdicts() {
        let artifact = |role: &str| Artifact {
            role: role.into(),
            path: format!("/e/{role}.json"),
            digest: "0".repeat(64),
            complete: true,
        };
        let mut manifest = Manifest {
            schema_version: SchemaVersion,
            run_id: "r_passed".into(),
            complete: true,
            artifacts: vec![
                artifact("normalized_job"),
                artifact("result"),
                artifact("provenance"),
                artifact("trace"),
                artifact("cleanup"),
            ],
        };
        let passed = json!({
            "terminal":true,
            "state":"passed",
            "verdict":{
                "overall":"satisfied",
                "steps":[{"result":"satisfied"}],
                "expectations":[{"result":"satisfied"}]
            }
        });
        assert!(validate_evidence_shape(&manifest, &passed, true).is_err());
        manifest.artifacts.push(artifact("verification"));
        assert!(validate_evidence_shape(&manifest, &passed, true).is_ok());
        manifest.artifacts.push(artifact("verification"));
        assert!(validate_evidence_shape(&manifest, &passed, true).is_err());

        manifest.artifacts.pop();
        let mut unresolved = passed.clone();
        unresolved["verdict"]["expectations"][0]["result"] = json!("unresolved");
        assert!(validate_evidence_shape(&manifest, &unresolved, true).is_err());

        let mut incomplete_step = passed.clone();
        incomplete_step["verdict"]["steps"][0]["result"] = json!("not_satisfied");
        assert!(validate_evidence_shape(&manifest, &incomplete_step, true).is_err());

        let mut incomplete_overall = passed;
        incomplete_overall["verdict"]["overall"] = json!("unresolved");
        assert!(validate_evidence_shape(&manifest, &incomplete_overall, true).is_err());
    }
}
