use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use manuvra_contract::RunResult;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tempfile::TempDir;

fn binary() -> &'static str {
    env!("CARGO_BIN_EXE_manuvra")
}

fn valid_job() -> Value {
    json!({
        "schema_version": 1,
        "target": {"kind": "browser", "url": "http://127.0.0.1:4351/"},
        "context": {
            "journey": "Create account", "revision": "abc", "environment": "fixture",
            "actor": "synthetic owner", "authority": "fixture only"
        },
        "values": {
            "private_name": {"value": "secret-marker", "description": "Account name", "secret": true},
            "redacted_name": {"value": "redact-marker", "description": "Other name"}
        },
        "steps": [{
            "id": "name", "goal": "Fill missing name", "requires_values": ["missing_name"],
            "done_when": [{"field": "Account name", "equals_value": "missing_name"}]
        }],
        "expectations": [],
        "options": {
            "allowed_origins": ["http://127.0.0.1:4351"],
            "redact_values": ["redacted_name"]
        }
    })
}

fn fixture(temp: &TempDir, job: &Value) -> PathBuf {
    let path = temp.path().join("job.json");
    fs::write(&path, serde_json::to_vec(job).unwrap()).unwrap();
    path
}

fn invoke(temp: &TempDir, args: &[&str]) -> Output {
    Command::new(binary())
        .args(args)
        .env("XDG_STATE_HOME", temp.path().join("state"))
        .output()
        .unwrap()
}

fn request_index(state_root: &Path, request_id: &str) -> PathBuf {
    state_root.join("requests").join(format!(
        "{}.json",
        hex::encode(Sha256::digest(request_id.as_bytes()))
    ))
}

fn write_private_json(path: &Path, value: &Value) {
    fs::write(path, serde_json::to_vec_pretty(value).unwrap()).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600)).unwrap();
    }
}

fn intent_from_result(result: &Value, job_digest: &str) -> Value {
    let manifest = PathBuf::from(result["evidence"]["manifest"].as_str().unwrap());
    json!({
        "phase": "intent",
        "schema_version": 1,
        "request_id": result["request_id"],
        "run_id": result["run_id"],
        "job_digest": job_digest,
        "evidence_root": manifest.parent().unwrap().parent().unwrap()
    })
}

fn all_file_bytes(root: &Path) -> Vec<u8> {
    let mut output = Vec::new();
    let mut pending = vec![root.to_owned()];
    while let Some(path) = pending.pop() {
        for entry in fs::read_dir(path).unwrap() {
            let entry = entry.unwrap();
            if entry.file_type().unwrap().is_dir() {
                pending.push(entry.path());
            } else {
                output.extend(fs::read(entry.path()).unwrap());
            }
        }
    }
    output
}

fn one_object(output: &Output) -> Value {
    assert!(
        output.stderr.is_empty(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        output.stdout.iter().filter(|byte| **byte == b'\n').count(),
        1
    );
    serde_json::from_slice(&output.stdout).unwrap()
}

#[test]
fn missing_value_blocks_without_browser_and_publishes_private_complete_evidence() {
    let temp = TempDir::new().unwrap();
    let job: Value = serde_json::from_slice(include_bytes!("fixtures/missing-value.json")).unwrap();
    let job_path = fixture(&temp, &job);
    let evidence = temp.path().join("evidence");
    let output = invoke(
        &temp,
        &[
            "run",
            "--request-id",
            "missing-1",
            "--job",
            job_path.to_str().unwrap(),
            "--evidence",
            evidence.to_str().unwrap(),
        ],
    );
    assert_eq!(output.status.code(), Some(3));
    let result = one_object(&output);
    assert_eq!(result["state"], "blocked");
    assert_eq!(result["reason"]["code"], "missing_value");
    assert_eq!(result["reason"]["value_name"], "account_name");
    assert_eq!(result["reason"]["step_id"], "name");
    assert_eq!(result["cleanup"]["browser"], "not_started");
    assert!(result["evidence"]["complete"].as_bool().unwrap());

    let manifest_path = PathBuf::from(result["evidence"]["manifest"].as_str().unwrap());
    assert!(manifest_path.is_absolute());
    let run_dir = manifest_path.parent().unwrap();
    let job_copy = fs::read_to_string(run_dir.join("job.json")).unwrap();
    assert!(!job_copy.contains("slice-one-secret"));
    assert!(!job_copy.contains("slice-one-redacted"));
    assert!(!job_copy.contains("secret-iso-form"));
    assert!(!job_copy.contains("secret-display-form"));
    assert!(!job_copy.contains("redacted-iso-form"));
    assert!(!job_copy.contains("redacted-display-form"));
    let exported_job: Value = serde_json::from_str(&job_copy).unwrap();
    assert_ne!(
        exported_job["values"]["private_note"]["value"],
        "slice-one-secret"
    );
    assert_ne!(
        exported_job["values"]["redacted_note"]["value"],
        "slice-one-redacted"
    );

    let manifest: Value = serde_json::from_slice(&fs::read(&manifest_path).unwrap()).unwrap();
    assert_eq!(manifest["complete"], true);
    for artifact in manifest["artifacts"].as_array().unwrap() {
        let path = Path::new(artifact["path"].as_str().unwrap());
        let digest = hex::encode(Sha256::digest(fs::read(path).unwrap()));
        assert_eq!(artifact["digest"], digest);
        assert_eq!(artifact["complete"], true);
    }

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            fs::metadata(run_dir).unwrap().permissions().mode() & 0o777,
            0o700
        );
        for name in ["job.json", "result.json", "manifest.json"] {
            assert_eq!(
                fs::metadata(run_dir.join(name))
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
        }
    }
    let state_run = temp
        .path()
        .join("state/manuvra/runs")
        .join(result["run_id"].as_str().unwrap());
    assert!(state_run.join("request.json").is_file());
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            fs::metadata(temp.path().join("state/manuvra/digest.key"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
    }
}

#[test]
fn natural_language_done_conditions_reach_browser_execution() {
    let temp = TempDir::new().unwrap();
    let mut job = valid_job();
    job["values"]["missing_name"] = json!({"value": "Wallet", "description": "Missing name"});
    job["steps"][0]["done_when"] = json!("The page is ready");
    let job_path = fixture(&temp, &job);
    let evidence = temp.path().join("evidence");
    let output = invoke(
        &temp,
        &[
            "run",
            "--request-id",
            "natural-1",
            "--job",
            job_path.to_str().unwrap(),
            "--evidence",
            evidence.to_str().unwrap(),
            "--browser",
            "/definitely/not/a/browser",
        ],
    );
    assert_eq!(output.status.code(), Some(3));
    let result = one_object(&output);
    assert_eq!(result["reason"]["code"], "browser_unavailable");
}

#[test]
fn malformed_input_and_unimplemented_commands_return_one_error_object() {
    let temp = TempDir::new().unwrap();
    let mut job = valid_job();
    job["unknown"] = json!(true);
    let job_path = fixture(&temp, &job);
    let evidence = temp.path().join("evidence");
    let invalid = invoke(
        &temp,
        &[
            "run",
            "--request-id",
            "invalid-1",
            "--job",
            job_path.to_str().unwrap(),
            "--evidence",
            evidence.to_str().unwrap(),
        ],
    );
    assert_eq!(invalid.status.code(), Some(64));
    assert_eq!(one_object(&invalid)["error"]["code"], "invalid_job");
    assert!(!evidence.exists());

    for args in [
        vec![
            "resume",
            "r_1",
            "--request-id",
            "resume-1",
            "--input",
            job_path.to_str().unwrap(),
        ],
        vec!["status", "r_1"],
        vec!["abort", "r_1", "--request-id", "abort-1"],
    ] {
        let output = invoke(&temp, &args);
        assert_eq!(output.status.code(), Some(64));
        assert_eq!(one_object(&output)["error"]["code"], "not_implemented");
    }

    for args in [
        vec!["status"],
        vec!["status", "r_1", "--request-id", "status-1"],
    ] {
        let output = invoke(&temp, &args);
        assert_eq!(output.status.code(), Some(64));
        assert_eq!(one_object(&output)["error"]["code"], "invalid_arguments");
    }
}

#[test]
fn invalid_job_does_not_expose_a_declared_secret_from_validation() {
    let temp = TempDir::new().unwrap();
    let mut job = valid_job();
    let secret = job["values"]["private_name"]["value"]
        .as_str()
        .unwrap()
        .to_owned();
    job["steps"][0]["id"] = json!(secret);
    let duplicate = job["steps"][0].clone();
    job["steps"].as_array_mut().unwrap().push(duplicate);
    let job_path = fixture(&temp, &job);
    let evidence = temp.path().join("evidence");

    let output = invoke(
        &temp,
        &[
            "run",
            "--request-id",
            "invalid-secret",
            "--job",
            job_path.to_str().unwrap(),
            "--evidence",
            evidence.to_str().unwrap(),
        ],
    );

    assert_eq!(output.status.code(), Some(64));
    let error = one_object(&output);
    assert_eq!(error["error"]["code"], "invalid_job");
    assert_eq!(error["error"]["message"], "job is invalid");
    for stream in [&output.stdout, &output.stderr] {
        assert!(
            !stream
                .windows(secret.len())
                .any(|bytes| bytes == secret.as_bytes())
        );
    }
    assert!(!evidence.exists());
}

#[test]
fn provider_key_is_scrubbed_from_errors_before_job_admission() {
    let temp = TempDir::new().unwrap();
    let key = "provider-key-must-not-reach-stdout";
    let missing_job = temp.path().join(key);
    let output = Command::new(binary())
        .args([
            "run",
            "--request-id",
            "key-scrub",
            "--job",
            missing_job.to_str().unwrap(),
            "--evidence",
            temp.path().join("evidence").to_str().unwrap(),
        ])
        .env("XDG_STATE_HOME", temp.path().join("state"))
        .env("TYPESAFE_API_KEY", key)
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(64));
    assert!(
        !output
            .stdout
            .windows(key.len())
            .any(|part| part == key.as_bytes())
    );
    assert_eq!(one_object(&output)["error"]["code"], "invalid_input");
}

#[test]
fn provider_key_collision_preserves_protocol_and_persisted_stdout() {
    let temp = TempDir::new().unwrap();
    let job_path = fixture(&temp, &valid_job());
    let evidence = temp.path().join("evidence");
    let args = [
        "run",
        "--request-id",
        "key-collision",
        "--job",
        job_path.to_str().unwrap(),
        "--evidence",
        evidence.to_str().unwrap(),
    ];
    let first = Command::new(binary())
        .args(args)
        .env("XDG_STATE_HOME", temp.path().join("state"))
        .env("TYPESAFE_API_KEY", "blocked")
        .output()
        .unwrap();
    assert_eq!(first.status.code(), Some(3));
    let result = one_object(&first);
    assert_eq!(result["state"], "blocked");
    assert_eq!(result["reason"]["code"], "missing_value");
    serde_json::from_value::<RunResult>(result.clone()).unwrap();
    let manifest = PathBuf::from(result["evidence"]["manifest"].as_str().unwrap());
    let persisted: Value =
        serde_json::from_slice(&fs::read(manifest.parent().unwrap().join("result.json")).unwrap())
            .unwrap();
    assert_eq!(result, persisted);

    let retry = Command::new(binary())
        .args(args)
        .env("XDG_STATE_HOME", temp.path().join("state"))
        .env("TYPESAFE_API_KEY", "blocked")
        .output()
        .unwrap();
    assert_eq!(retry.stdout, first.stdout);
}

#[test]
fn request_identity_includes_browser_execution_flags() {
    let temp = TempDir::new().unwrap();
    let job_path = fixture(&temp, &valid_job());
    let evidence = temp.path().join("evidence");
    let base = [
        "run",
        "--request-id",
        "execution-flags",
        "--job",
        job_path.to_str().unwrap(),
        "--evidence",
        evidence.to_str().unwrap(),
    ];
    assert_eq!(invoke(&temp, &base).status.code(), Some(3));
    let mut changed = base.to_vec();
    changed.push("--headless");
    let output = invoke(&temp, &changed);
    assert_eq!(output.status.code(), Some(64));
    assert_eq!(one_object(&output)["error"]["code"], "request_conflict");
}

#[test]
fn request_identity_includes_environment_browser_without_launching_it() {
    let temp = TempDir::new().unwrap();
    let job_path = fixture(&temp, &valid_job());
    let evidence = temp.path().join("evidence");
    let args = [
        "run",
        "--request-id",
        "environment-browser",
        "--job",
        job_path.to_str().unwrap(),
        "--evidence",
        evidence.to_str().unwrap(),
    ];
    let first = Command::new(binary())
        .args(args)
        .env("XDG_STATE_HOME", temp.path().join("state"))
        .env("MANUVRA_BROWSER", "/missing/browser-one")
        .output()
        .unwrap();
    assert_eq!(first.status.code(), Some(3));
    assert_eq!(one_object(&first)["reason"]["code"], "missing_value");

    let changed = Command::new(binary())
        .args(args)
        .env("XDG_STATE_HOME", temp.path().join("state"))
        .env("MANUVRA_BROWSER", "/missing/browser-two")
        .output()
        .unwrap();
    assert_eq!(changed.status.code(), Some(64));
    assert_eq!(one_object(&changed)["error"]["code"], "request_conflict");
    assert_eq!(fs::read_dir(evidence).unwrap().count(), 1);
}

#[test]
fn explicit_browser_identity_overrides_environment_selection() {
    let temp = TempDir::new().unwrap();
    let job_path = fixture(&temp, &valid_job());
    let evidence = temp.path().join("evidence");
    let args = [
        "run",
        "--request-id",
        "explicit-browser",
        "--job",
        job_path.to_str().unwrap(),
        "--evidence",
        evidence.to_str().unwrap(),
        "--browser",
        "/missing/explicit-browser",
    ];
    let first = Command::new(binary())
        .args(args)
        .env("XDG_STATE_HOME", temp.path().join("state"))
        .env("MANUVRA_BROWSER", "/missing/environment-one")
        .output()
        .unwrap();
    assert_eq!(first.status.code(), Some(3));

    let deduplicated = Command::new(binary())
        .args(args)
        .env("XDG_STATE_HOME", temp.path().join("state"))
        .env("MANUVRA_BROWSER", "/missing/environment-two")
        .output()
        .unwrap();
    assert_eq!(deduplicated.stdout, first.stdout);
}

#[test]
fn schemas_and_version_are_single_json_objects() {
    let temp = TempDir::new().unwrap();
    for kind in ["job", "result", "disposition", "manifest"] {
        let output = invoke(&temp, &["schema", kind]);
        assert_eq!(output.status.code(), Some(0));
        let schema = one_object(&output);
        assert!(schema.get("$schema").is_some());
        assert_eq!(schema["$defs"]["SchemaVersion"]["const"], 1);
    }
    let output = invoke(&temp, &["version"]);
    assert_eq!(output.status.code(), Some(0));
    let version = one_object(&output);
    assert_eq!(version["schema_version"], 1);
    assert_eq!(version["version"], env!("CARGO_PKG_VERSION"));
}

#[cfg(unix)]
#[test]
fn non_utf8_evidence_path_is_rejected_without_a_false_reference() {
    use std::ffi::OsString;
    use std::os::unix::ffi::OsStringExt;

    let temp = TempDir::new().unwrap();
    let job_path = fixture(&temp, &valid_job());
    let invalid_component = OsString::from_vec(vec![b'e', b'v', b'-', 0xff]);
    let evidence = temp.path().join(invalid_component);
    let output = Command::new(binary())
        .args(["run", "--request-id", "non-utf8", "--job"])
        .arg(job_path)
        .arg("--evidence")
        .arg(&evidence)
        .env("XDG_STATE_HOME", temp.path().join("state"))
        .output()
        .unwrap();

    assert_eq!(output.status.code(), Some(64));
    assert_eq!(one_object(&output)["error"]["code"], "invalid_input");
    assert!(!evidence.exists());
}

#[test]
fn request_id_deduplicates_same_job_and_rejects_conflict() {
    let temp = TempDir::new().unwrap();
    let job_path = fixture(&temp, &valid_job());
    let evidence = temp.path().join("evidence");
    let args = [
        "run",
        "--request-id",
        "dedup-1",
        "--job",
        job_path.to_str().unwrap(),
        "--evidence",
        evidence.to_str().unwrap(),
    ];
    let first = one_object(&invoke(&temp, &args));
    let second_output = invoke(&temp, &args);
    assert_eq!(second_output.status.code(), Some(3));
    let second = one_object(&second_output);
    assert_eq!(first["run_id"], second["run_id"]);

    let mut changed = valid_job();
    changed["context"]["revision"] = json!("different");
    let changed_path = temp.path().join("changed.json");
    fs::write(&changed_path, serde_json::to_vec(&changed).unwrap()).unwrap();
    let conflict = invoke(
        &temp,
        &[
            "run",
            "--request-id",
            "dedup-1",
            "--job",
            changed_path.to_str().unwrap(),
            "--evidence",
            evidence.to_str().unwrap(),
        ],
    );
    assert_eq!(conflict.status.code(), Some(64));
    assert_eq!(one_object(&conflict)["error"]["code"], "request_conflict");
}

#[test]
fn request_digest_is_keyed_by_the_private_state_root() {
    let first = TempDir::new().unwrap();
    let second = TempDir::new().unwrap();
    let mut digests = Vec::new();

    for temp in [&first, &second] {
        let job_path = fixture(temp, &valid_job());
        let evidence = temp.path().join("evidence");
        let result = one_object(&invoke(
            temp,
            &[
                "run",
                "--request-id",
                "keyed-1",
                "--job",
                job_path.to_str().unwrap(),
                "--evidence",
                evidence.to_str().unwrap(),
            ],
        ));
        let request_path = temp
            .path()
            .join("state/manuvra/runs")
            .join(result["run_id"].as_str().unwrap())
            .join("request.json");
        let request: Value = serde_json::from_slice(&fs::read(request_path).unwrap()).unwrap();
        digests.push(request["job_digest"].as_str().unwrap().to_owned());
    }

    assert_ne!(digests[0], digests[1]);
}

#[test]
fn corrupt_digest_key_fails_without_publishing_evidence() {
    let temp = TempDir::new().unwrap();
    let job_path = fixture(&temp, &valid_job());
    let state_root = temp.path().join("state/manuvra");
    fs::create_dir_all(&state_root).unwrap();
    fs::write(state_root.join("digest.key"), b"too short").unwrap();
    let evidence = temp.path().join("evidence");

    let output = invoke(
        &temp,
        &[
            "run",
            "--request-id",
            "bad-key-1",
            "--job",
            job_path.to_str().unwrap(),
            "--evidence",
            evidence.to_str().unwrap(),
        ],
    );

    assert_eq!(output.status.code(), Some(70));
    assert_eq!(one_object(&output)["error"]["code"], "internal");
    assert!(!evidence.exists());
}

#[test]
fn overlapping_request_ids_publish_one_run() {
    let temp = TempDir::new().unwrap();
    let job_path = fixture(&temp, &valid_job());
    let evidence = temp.path().join("evidence");
    let state = temp.path().join("state");
    let mut children = Vec::new();
    for _ in 0..12 {
        children.push(
            Command::new(binary())
                .args([
                    "run",
                    "--request-id",
                    "overlap-1",
                    "--job",
                    job_path.to_str().unwrap(),
                    "--evidence",
                    evidence.to_str().unwrap(),
                ])
                .env("XDG_STATE_HOME", &state)
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .unwrap(),
        );
    }

    let outputs: Vec<_> = children
        .into_iter()
        .map(|child| child.wait_with_output().unwrap())
        .collect();
    let results: Vec<_> = outputs.iter().map(one_object).collect();
    assert!(outputs.iter().all(|output| output.status.code() == Some(3)));
    assert!(
        results
            .iter()
            .all(|result| result["run_id"] == results[0]["run_id"])
    );
    assert_eq!(fs::read_dir(&evidence).unwrap().count(), 1);

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        for path in [state.join("manuvra"), state.join("manuvra/runs")] {
            assert_eq!(
                fs::metadata(path).unwrap().permissions().mode() & 0o777,
                0o700
            );
        }
    }
}

#[test]
fn retry_from_request_intent_before_evidence_reuses_the_run() {
    let temp = TempDir::new().unwrap();
    let job_path = fixture(&temp, &valid_job());
    let evidence = temp.path().join("evidence");
    let args = [
        "run",
        "--request-id",
        "intent-before-evidence",
        "--job",
        job_path.to_str().unwrap(),
        "--evidence",
        evidence.to_str().unwrap(),
    ];
    let first = one_object(&invoke(&temp, &args));
    let state_root = temp.path().join("state/manuvra");
    let index = request_index(&state_root, "intent-before-evidence");
    let complete: Value = serde_json::from_slice(&fs::read(&index).unwrap()).unwrap();
    let intent = intent_from_result(&first, complete["job_digest"].as_str().unwrap());
    write_private_json(&index, &intent);
    let state_run_record = state_root
        .join("runs")
        .join(first["run_id"].as_str().unwrap())
        .join("request.json");
    write_private_json(&state_run_record, &intent);
    fs::remove_dir_all(
        PathBuf::from(first["evidence"]["manifest"].as_str().unwrap())
            .parent()
            .unwrap(),
    )
    .unwrap();

    let retried_output = invoke(&temp, &args);
    assert_eq!(retried_output.status.code(), Some(3));
    let retried = one_object(&retried_output);
    assert_eq!(retried["run_id"], first["run_id"]);
    assert!(
        Path::new(retried["evidence"]["manifest"].as_str().unwrap()).is_file(),
        "the same run should be completed from its durable intent"
    );
    assert_eq!(fs::read_dir(&evidence).unwrap().count(), 1);
}

#[test]
fn retry_from_request_intent_after_evidence_finalizes_the_same_run() {
    let temp = TempDir::new().unwrap();
    let job_path = fixture(&temp, &valid_job());
    let evidence = temp.path().join("evidence");
    let args = [
        "run",
        "--request-id",
        "intent-after-evidence",
        "--job",
        job_path.to_str().unwrap(),
        "--evidence",
        evidence.to_str().unwrap(),
    ];
    let first = one_object(&invoke(&temp, &args));
    let state_root = temp.path().join("state/manuvra");
    let index = request_index(&state_root, "intent-after-evidence");
    let complete: Value = serde_json::from_slice(&fs::read(&index).unwrap()).unwrap();
    let intent = intent_from_result(&first, complete["job_digest"].as_str().unwrap());
    write_private_json(&index, &intent);

    let retried_output = invoke(&temp, &args);
    assert_eq!(retried_output.status.code(), Some(3));
    let retried = one_object(&retried_output);
    assert_eq!(retried["run_id"], first["run_id"]);
    assert_eq!(retried, first);
    assert_eq!(fs::read_dir(&evidence).unwrap().count(), 1);
    let finalized: Value = serde_json::from_slice(&fs::read(index).unwrap()).unwrap();
    assert_eq!(finalized["phase"], "complete");
}

#[test]
fn incomplete_existing_evidence_is_not_reported_complete_or_replaced_by_a_new_run() {
    let temp = TempDir::new().unwrap();
    let job_path = fixture(&temp, &valid_job());
    let evidence = temp.path().join("evidence");
    let args = [
        "run",
        "--request-id",
        "incomplete-evidence",
        "--job",
        job_path.to_str().unwrap(),
        "--evidence",
        evidence.to_str().unwrap(),
    ];
    let first = one_object(&invoke(&temp, &args));
    let state_root = temp.path().join("state/manuvra");
    let index = request_index(&state_root, "incomplete-evidence");
    let complete: Value = serde_json::from_slice(&fs::read(&index).unwrap()).unwrap();
    write_private_json(
        &index,
        &intent_from_result(&first, complete["job_digest"].as_str().unwrap()),
    );
    fs::remove_file(first["evidence"]["manifest"].as_str().unwrap()).unwrap();

    let output = invoke(&temp, &args);
    assert_eq!(output.status.code(), Some(70));
    let error = one_object(&output);
    assert_eq!(error["error"]["code"], "internal");
    assert!(error.get("evidence").is_none());
    assert_eq!(fs::read_dir(&evidence).unwrap().count(), 1);
    assert!(evidence.join(first["run_id"].as_str().unwrap()).is_dir());
}

#[test]
fn completed_requests_reject_every_evidence_corruption_without_relaunching() {
    for corruption in [
        "manifest_incomplete",
        "artifact_incomplete",
        "artifact_digest",
        "artifact_role_path",
        "missing_required_artifact",
        "unmanifested_file",
        "record_result_mismatch",
    ] {
        let temp = TempDir::new().unwrap();
        let job_path = fixture(&temp, &valid_job());
        let evidence = temp.path().join("evidence");
        let request_id = format!("corrupt-{corruption}");
        let args = [
            "run",
            "--request-id",
            request_id.as_str(),
            "--job",
            job_path.to_str().unwrap(),
            "--evidence",
            evidence.to_str().unwrap(),
        ];
        let first = one_object(&invoke(&temp, &args));
        let manifest_path = PathBuf::from(first["evidence"]["manifest"].as_str().unwrap());
        let run_dir = manifest_path.parent().unwrap();
        let index = request_index(&temp.path().join("state/manuvra"), &request_id);
        if corruption == "unmanifested_file" {
            fs::write(run_dir.join("unexpected.json"), b"{}\n").unwrap();
        } else if corruption == "record_result_mismatch" {
            let mut record: Value = serde_json::from_slice(&fs::read(&index).unwrap()).unwrap();
            record["result"]["state"] = json!("passed");
            write_private_json(&index, &record);
        } else {
            let mut manifest: Value =
                serde_json::from_slice(&fs::read(&manifest_path).unwrap()).unwrap();
            match corruption {
                "manifest_incomplete" => manifest["complete"] = json!(false),
                "artifact_incomplete" => manifest["artifacts"][0]["complete"] = json!(false),
                "artifact_digest" => manifest["artifacts"][0]["digest"] = json!("0".repeat(64)),
                "artifact_role_path" => manifest["artifacts"][0]["role"] = json!("result"),
                "missing_required_artifact" => {
                    manifest["artifacts"].as_array_mut().unwrap().remove(0);
                }
                _ => unreachable!(),
            }
            write_private_json(&manifest_path, &manifest);
        }

        let output = invoke(&temp, &args);
        assert_eq!(output.status.code(), Some(70), "{corruption}");
        assert_eq!(one_object(&output)["error"]["code"], "internal");
        assert_eq!(fs::read_dir(&evidence).unwrap().count(), 1);
        assert!(run_dir.is_dir(), "corrupt evidence must be preserved");
    }
}

#[test]
fn missing_digest_key_with_request_history_reports_corruption() {
    let temp = TempDir::new().unwrap();
    let job_path = fixture(&temp, &valid_job());
    let evidence = temp.path().join("evidence");
    let args = [
        "run",
        "--request-id",
        "lost-key",
        "--job",
        job_path.to_str().unwrap(),
        "--evidence",
        evidence.to_str().unwrap(),
    ];
    let first = one_object(&invoke(&temp, &args));
    let key = temp.path().join("state/manuvra/digest.key");
    fs::remove_file(&key).unwrap();

    let output = invoke(&temp, &args);
    assert_eq!(output.status.code(), Some(70));
    let error = one_object(&output);
    assert_eq!(error["error"]["code"], "internal");
    assert!(
        error["error"]["message"]
            .as_str()
            .unwrap()
            .contains("request history exists")
    );
    assert!(!key.exists(), "history must never be re-keyed implicitly");
    assert_eq!(fs::read_dir(&evidence).unwrap().count(), 1);
    assert!(Path::new(first["evidence"]["manifest"].as_str().unwrap()).is_file());
}

#[cfg(unix)]
#[test]
fn digest_key_rejects_exposed_mode_and_symlink_substitution() {
    use std::os::unix::fs::{PermissionsExt, symlink};

    for substitution in ["mode", "symlink"] {
        let temp = TempDir::new().unwrap();
        let state_root = temp.path().join("state/manuvra");
        fs::create_dir_all(&state_root).unwrap();
        fs::set_permissions(&state_root, fs::Permissions::from_mode(0o700)).unwrap();
        let key = state_root.join("digest.key");
        if substitution == "mode" {
            fs::write(&key, [7_u8; 32]).unwrap();
            fs::set_permissions(&key, fs::Permissions::from_mode(0o644)).unwrap();
        } else {
            let target = temp.path().join("outside-key");
            fs::write(&target, [7_u8; 32]).unwrap();
            fs::set_permissions(&target, fs::Permissions::from_mode(0o600)).unwrap();
            symlink(target, &key).unwrap();
        }
        let job_path = fixture(&temp, &valid_job());
        let evidence = temp.path().join("evidence");
        let output = invoke(
            &temp,
            &[
                "run",
                "--request-id",
                substitution,
                "--job",
                job_path.to_str().unwrap(),
                "--evidence",
                evidence.to_str().unwrap(),
            ],
        );
        assert_eq!(output.status.code(), Some(70));
        assert_eq!(one_object(&output)["error"]["code"], "internal");
        assert!(!evidence.exists());
    }
}

#[cfg(unix)]
#[test]
fn state_lock_and_request_index_reject_symlink_substitution() {
    use std::os::unix::fs::{PermissionsExt, symlink};

    let lock_temp = TempDir::new().unwrap();
    let state_root = lock_temp.path().join("state/manuvra");
    let requests = state_root.join("requests");
    fs::create_dir_all(&requests).unwrap();
    fs::set_permissions(&state_root, fs::Permissions::from_mode(0o700)).unwrap();
    fs::set_permissions(&requests, fs::Permissions::from_mode(0o700)).unwrap();
    let outside = lock_temp.path().join("outside-lock");
    fs::write(&outside, []).unwrap();
    fs::set_permissions(&outside, fs::Permissions::from_mode(0o600)).unwrap();
    let lock_name = request_index(&state_root, "linked-lock")
        .file_stem()
        .unwrap()
        .to_owned();
    symlink(outside, requests.join(lock_name).with_extension("lock")).unwrap();
    let job_path = fixture(&lock_temp, &valid_job());
    let evidence = lock_temp.path().join("evidence");
    let output = invoke(
        &lock_temp,
        &[
            "run",
            "--request-id",
            "linked-lock",
            "--job",
            job_path.to_str().unwrap(),
            "--evidence",
            evidence.to_str().unwrap(),
        ],
    );
    assert_eq!(output.status.code(), Some(70));
    assert!(!evidence.exists());

    let index_temp = TempDir::new().unwrap();
    let job_path = fixture(&index_temp, &valid_job());
    let evidence = index_temp.path().join("evidence");
    let args = [
        "run",
        "--request-id",
        "linked-index",
        "--job",
        job_path.to_str().unwrap(),
        "--evidence",
        evidence.to_str().unwrap(),
    ];
    one_object(&invoke(&index_temp, &args));
    let state_root = index_temp.path().join("state/manuvra");
    let index = request_index(&state_root, "linked-index");
    let outside = index_temp.path().join("outside-index");
    fs::rename(&index, &outside).unwrap();
    symlink(&outside, &index).unwrap();
    let output = invoke(&index_temp, &args);
    assert_eq!(output.status.code(), Some(70));
    assert_eq!(one_object(&output)["error"]["code"], "internal");
    assert_eq!(fs::read_dir(&evidence).unwrap().count(), 1);
}

#[test]
fn classified_renderings_are_absent_from_stdout_and_all_evidence_fields() {
    let temp = TempDir::new().unwrap();
    let mut job = valid_job();
    job["context"]["journey"] = json!("redact-marker appears in ordinary text");
    job["values"]["secret-marker"] =
        json!({"value": "public-value", "description": "key is classified elsewhere"});
    job["steps"][0]["id"] = json!("secret-marker");
    job["steps"][0]["goal"] = json!("use redact-marker without exporting it");
    let job_path = fixture(&temp, &job);
    let evidence = temp.path().join("evidence");
    let args = [
        "run",
        "--request-id",
        "secret-marker",
        "--job",
        job_path.to_str().unwrap(),
        "--evidence",
        evidence.to_str().unwrap(),
    ];
    let first_output = invoke(&temp, &args);
    assert_eq!(first_output.status.code(), Some(3));
    let first = one_object(&first_output);
    assert_ne!(first["request_id"], "secret-marker");
    for marker in [b"secret-marker".as_slice(), b"redact-marker".as_slice()] {
        assert!(
            !first_output
                .stdout
                .windows(marker.len())
                .any(|part| part == marker)
        );
    }
    for marker in ["secret-marker", "redact-marker"] {
        assert_no_marker(&evidence, marker);
        assert_no_marker(&temp.path().join("state/manuvra"), marker);
    }

    let second_output = invoke(&temp, &args);
    assert_eq!(second_output.status.code(), Some(3));
    assert_eq!(one_object(&second_output)["run_id"], first["run_id"]);
    assert_eq!(fs::read_dir(&evidence).unwrap().count(), 1);
}

#[test]
fn classified_request_id_never_enters_intent_or_completed_state() {
    let temp = TempDir::new().unwrap();
    let provider = "provider-request-state-marker";
    let job_path = fixture(&temp, &valid_job());
    let evidence = temp.path().join("evidence");
    let state_root = temp.path().join("state/manuvra");
    let args = [
        "run",
        "--request-id",
        provider,
        "--job",
        job_path.to_str().unwrap(),
        "--evidence",
        evidence.to_str().unwrap(),
    ];
    let run = || {
        Command::new(binary())
            .args(args)
            .env("XDG_STATE_HOME", temp.path().join("state"))
            .env("TYPESAFE_API_KEY", provider)
            .output()
            .unwrap()
    };

    let first_output = run();
    assert_eq!(first_output.status.code(), Some(3));
    let first = one_object(&first_output);
    assert_ne!(first["request_id"], provider);
    assert_no_marker(&evidence, provider);
    assert_no_marker(&state_root, provider);

    let index = request_index(&state_root, provider);
    let complete: Value = serde_json::from_slice(&fs::read(&index).unwrap()).unwrap();
    assert_eq!(complete["phase"], "complete");
    assert_eq!(complete["request_id"], first["request_id"]);
    let intent = intent_from_result(&first, complete["job_digest"].as_str().unwrap());
    write_private_json(&index, &intent);
    let state_run_record = state_root
        .join("runs")
        .join(first["run_id"].as_str().unwrap())
        .join("request.json");
    write_private_json(&state_run_record, &intent);
    fs::remove_dir_all(
        PathBuf::from(first["evidence"]["manifest"].as_str().unwrap())
            .parent()
            .unwrap(),
    )
    .unwrap();
    assert_no_marker(&state_root, provider);

    let retry_output = run();
    assert_eq!(retry_output.status.code(), Some(3));
    assert_eq!(retry_output.stdout, first_output.stdout);
    let retry = one_object(&retry_output);
    assert_eq!(retry, first);
    let result: Value = serde_json::from_slice(
        &fs::read(
            PathBuf::from(retry["evidence"]["manifest"].as_str().unwrap())
                .parent()
                .unwrap()
                .join("result.json"),
        )
        .unwrap(),
    )
    .unwrap();
    assert_eq!(retry, result);
    assert_no_marker(&evidence, provider);
    assert_no_marker(&state_root, provider);

    let deduplicated_output = run();
    assert_eq!(deduplicated_output.status.code(), Some(3));
    assert_eq!(deduplicated_output.stdout, retry_output.stdout);
    assert_eq!(one_object(&deduplicated_output), result);

    let mut legacy_complete: Value = serde_json::from_slice(&fs::read(&index).unwrap()).unwrap();
    legacy_complete.as_object_mut().unwrap().remove("phase");
    write_private_json(&index, &legacy_complete);
    let legacy_retry = run();
    assert_eq!(legacy_retry.status.code(), Some(3));
    assert_eq!(legacy_retry.stdout, retry_output.stdout);
    assert_eq!(one_object(&legacy_retry), result);
    assert_no_marker(&state_root, provider);

    let mut changed = valid_job();
    changed["context"]["revision"] = json!("different");
    let changed_path = temp.path().join("changed.json");
    fs::write(&changed_path, serde_json::to_vec(&changed).unwrap()).unwrap();
    let conflict = Command::new(binary())
        .args([
            "run",
            "--request-id",
            provider,
            "--job",
            changed_path.to_str().unwrap(),
            "--evidence",
            evidence.to_str().unwrap(),
        ])
        .env("XDG_STATE_HOME", temp.path().join("state"))
        .env("TYPESAFE_API_KEY", provider)
        .output()
        .unwrap();
    assert_eq!(conflict.status.code(), Some(64));
    assert_eq!(one_object(&conflict)["error"]["code"], "request_conflict");
    assert_no_marker(&evidence, provider);
    assert_no_marker(&state_root, provider);
}

fn assert_no_marker(root: &Path, marker: &str) {
    let bytes = all_file_bytes(root);
    assert!(
        !bytes
            .windows(marker.len())
            .any(|part| part == marker.as_bytes()),
        "classified marker found under {}",
        root.display()
    );
}

#[test]
fn adversarial_redaction_preserves_protocol_and_dedup_output() {
    let temp = TempDir::new().unwrap();
    let mut job = valid_job();
    job["values"] = json!({
        "redacted-key": {
            "value": "redacted", "description": "first classified value", "secret": true,
            "formats": {"iso": "blocked", "display": "redactedblocked"}
        },
        "blocked-key": {
            "value": "blocked", "description": "colliding classified rendering", "secret": true
        },
        "redactedblocked-key": {
            "value": "redactedblocked", "description": "longest classified rendering", "secret": true
        },
        "empty-key": {
            "value": "", "description": "empty classified value", "secret": true
        }
    });
    job["context"]["journey"] = json!("redactedblocked then redacted then blocked");
    job["steps"] = json!([{
        "id": "step-blocked-redacted",
        "goal": "do not export redactedblocked",
        "requires_values": ["missing-redactedblocked"],
        "done_when": [{
            "field": "blocked redacted",
            "equals_value": "missing-redactedblocked"
        }]
    }]);
    job["options"] = json!({"allowed_origins": ["http://127.0.0.1:4351"]});
    let job_path = fixture(&temp, &job);
    let evidence = temp.path().join("evidence");
    let args = [
        "run",
        "--request-id",
        "request-redactedblocked",
        "--job",
        job_path.to_str().unwrap(),
        "--evidence",
        evidence.to_str().unwrap(),
    ];

    let first_output = invoke(&temp, &args);
    assert_eq!(first_output.status.code(), Some(3));
    let first = one_object(&first_output);
    assert_eq!(first["state"], "blocked");
    assert_eq!(first["reason"]["code"], "missing_value");
    assert_eq!(
        String::from_utf8_lossy(&first_output.stdout)
            .matches("blocked")
            .count(),
        1,
        "the only public occurrence is the protocol-owned run state"
    );
    let public_request_id = first["request_id"].as_str().unwrap();
    assert!(!public_request_id.contains("redacted"));
    assert!(!public_request_id.contains("blocked"));
    for field in ["step_id", "value_name"] {
        let value = first["reason"][field].as_str().unwrap();
        assert!(!value.contains("redacted"));
        assert!(!value.contains("blocked"));
    }
    serde_json::from_value::<RunResult>(first.clone()).unwrap();

    let manifest = PathBuf::from(first["evidence"]["manifest"].as_str().unwrap());
    let run_dir = manifest.parent().unwrap();
    let result_file: Value =
        serde_json::from_slice(&fs::read(run_dir.join("result.json")).unwrap()).unwrap();
    assert_eq!(first, result_file);
    let exported_job: Value =
        serde_json::from_slice(&fs::read(run_dir.join("job.json")).unwrap()).unwrap();
    assert_eq!(exported_job["target"]["kind"], "browser");
    assert_eq!(exported_job["values"].as_object().unwrap().len(), 4);
    for (key, value) in exported_job["values"].as_object().unwrap() {
        assert!(!key.contains("redacted"));
        assert!(!key.contains("blocked"));
        let rendering = value["value"].as_str().unwrap();
        assert!(!rendering.contains("redacted"));
        assert!(!rendering.contains("blocked"));
    }
    assert!(
        exported_job["values"]
            .as_object()
            .unwrap()
            .values()
            .any(|value| value["value"] == ""),
        "an empty classified value carries no bytes to leak and remains empty"
    );
    let job_bytes = fs::read(run_dir.join("job.json")).unwrap();
    assert!(!job_bytes.windows(8).any(|part| part == b"redacted"));
    assert!(!job_bytes.windows(7).any(|part| part == b"blocked"));

    let retry_output = invoke(&temp, &args);
    assert_eq!(retry_output.status.code(), Some(3));
    assert_eq!(retry_output.stdout, first_output.stdout);
    assert_eq!(one_object(&retry_output), result_file);
}

fn export_boundary_job(provider: &str, missing_value: bool) -> Value {
    let dynamic_name = format!("{provider}-dynamic-name");
    let step_id = format!("{provider}-step");
    let mut job = json!({
        "schema_version": 1,
        "target": {"kind": "browser", "url": format!("http://127.0.0.1:4351/{provider}")},
        "context": {
            "journey": format!("journey {provider}"),
            "revision": format!("revision {provider}"),
            "environment": format!("environment {provider}"),
            "actor": format!("actor {provider}"),
            "authority": format!("authority {provider}")
        },
        "values": {
            dynamic_name.clone(): {
                "value": provider,
                "description": format!("description {provider}")
            },
            "browser-secret": {"value": "browser", "description": "protocol collision", "secret": true},
            "viewport-secret": {"value": "viewport", "description": "protocol collision", "secret": true},
            "passed-secret": {"value": "passed", "description": "protocol collision", "secret": true},
            "failed-secret": {"value": "failed", "description": "protocol collision", "secret": true}
        },
        "steps": [{
            "id": step_id,
            "goal": format!("goal {provider}"),
            "done_when": [{"text_absent": format!("absent {provider}"), "scope": "viewport"}]
        }],
        "options": {"allowed_origins": ["http://127.0.0.1:4351"]}
    });
    if missing_value {
        job["steps"][0]["requires_values"] = json!([format!("missing-{provider}")]);
    }
    job
}

fn assert_export_boundary(
    output: &Output,
    evidence: &Path,
    provider: &str,
    expected_reason: &str,
) -> Value {
    let result = one_object(output);
    assert_eq!(result["state"], "blocked");
    assert_eq!(result["reason"]["code"], expected_reason);
    assert!(
        !output
            .stdout
            .windows(provider.len())
            .any(|part| part == provider.as_bytes())
    );
    let manifest = PathBuf::from(result["evidence"]["manifest"].as_str().unwrap());
    let run_dir = manifest.parent().unwrap();
    let result_file: Value =
        serde_json::from_slice(&fs::read(run_dir.join("result.json")).unwrap()).unwrap();
    assert_eq!(result, result_file);
    serde_json::from_value::<RunResult>(result.clone()).unwrap();

    let job_bytes = fs::read(run_dir.join("job.json")).unwrap();
    let exported: Value = serde_json::from_slice(&job_bytes).unwrap();
    manuvra_contract::Job::parse(&job_bytes).unwrap();
    assert_eq!(exported["target"]["kind"], "browser");
    assert_eq!(exported["steps"][0]["done_when"][0]["scope"], "viewport");
    for value in exported["values"].as_object().unwrap().values() {
        assert!(
            !["browser", "viewport", "passed", "failed"]
                .contains(&value["value"].as_str().unwrap())
        );
    }
    let evidence_bytes = all_file_bytes(evidence);
    assert!(
        !evidence_bytes
            .windows(provider.len())
            .any(|part| part == provider.as_bytes())
    );
    result
}

#[test]
fn blocked_export_redacts_provider_owned_caller_text_without_corrupting_protocol() {
    let temp = TempDir::new().unwrap();
    let provider = "provider-export-boundary-marker";
    let job_path = fixture(&temp, &export_boundary_job(provider, true));
    let evidence = temp.path().join("evidence");
    let args = [
        "run",
        "--request-id",
        &format!("request-{provider}"),
        "--job",
        job_path.to_str().unwrap(),
        "--evidence",
        evidence.to_str().unwrap(),
    ];
    let first = Command::new(binary())
        .args(args)
        .env("XDG_STATE_HOME", temp.path().join("state"))
        .env("TYPESAFE_API_KEY", provider)
        .output()
        .unwrap();
    assert_eq!(first.status.code(), Some(3));
    let result = assert_export_boundary(&first, &evidence, provider, "missing_value");

    let retry = Command::new(binary())
        .args(args)
        .env("XDG_STATE_HOME", temp.path().join("state"))
        .env("TYPESAFE_API_KEY", provider)
        .output()
        .unwrap();
    assert_eq!(retry.stdout, first.stdout);
    assert_eq!(one_object(&retry), result);
}

#[test]
fn browser_run_export_uses_the_same_caller_and_protocol_redaction_policy() {
    let temp = TempDir::new().unwrap();
    let provider = "provider-browser-export-marker";
    let job_path = fixture(&temp, &export_boundary_job(provider, false));
    let evidence = temp.path().join("evidence");
    let request_id = format!("request-{provider}");
    let output = Command::new(binary())
        .args([
            "run",
            "--request-id",
            &request_id,
            "--job",
            job_path.to_str().unwrap(),
            "--evidence",
            evidence.to_str().unwrap(),
            "--browser",
            "/missing/adversarial-browser",
        ])
        .env("XDG_STATE_HOME", temp.path().join("state"))
        .env("TYPESAFE_API_KEY", provider)
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(3));
    assert_export_boundary(&output, &evidence, provider, "browser_unavailable");
}
