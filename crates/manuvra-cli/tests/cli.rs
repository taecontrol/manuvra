use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use manuvra_contract::RunResult;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tempfile::TempDir;

#[cfg(target_os = "macos")]
struct DarwinHttpFixture {
    address: std::net::SocketAddr,
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
    worker: Option<std::thread::JoinHandle<()>>,
}

#[cfg(target_os = "macos")]
impl DarwinHttpFixture {
    fn start() -> Self {
        use std::io::{Read, Write};
        use std::sync::atomic::Ordering;

        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let address = listener.local_addr().unwrap();
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let worker_stop = stop.clone();
        let worker = std::thread::spawn(move || {
            while !worker_stop.load(Ordering::SeqCst) {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        let _ = stream.set_read_timeout(Some(std::time::Duration::from_secs(1)));
                        let mut request = [0_u8; 4096];
                        let _ = stream.read(&mut request);
                        let body = b"<!doctype html><title>Darwin hosted fixture</title><main>Ready <button id=activate onclick=\"document.querySelector('output').textContent='Activated'\">Activate</button> <output>Idle</output></main>";
                        let response = format!(
                            "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                            body.len()
                        );
                        let _ = stream.write_all(response.as_bytes());
                        let _ = stream.write_all(body);
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(std::time::Duration::from_millis(10));
                    }
                    Err(_) => break,
                }
            }
        });
        Self {
            address,
            stop,
            worker: Some(worker),
        }
    }

    fn url(&self) -> String {
        format!("http://{}/ready", self.address)
    }
}

#[cfg(target_os = "macos")]
impl Drop for DarwinHttpFixture {
    fn drop(&mut self) {
        use std::sync::atomic::Ordering;

        self.stop.store(true, Ordering::SeqCst);
        let _ = std::net::TcpStream::connect(self.address);
        if let Some(worker) = self.worker.take() {
            worker.join().unwrap();
        }
    }
}

fn binary() -> &'static str {
    env!("CARGO_BIN_EXE_manuvra")
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
const EXECUTION_STOP_REASON: &str = "browser_unavailable";
#[cfg(not(any(target_os = "linux", target_os = "macos")))]
const EXECUTION_STOP_REASON: &str = "unsupported_platform";

#[cfg(any(target_os = "linux", target_os = "macos"))]
const VALID_BACKGROUND_COMMAND_RESULT: (i32, &str) = (64, "run_not_found");
#[cfg(not(any(target_os = "linux", target_os = "macos")))]
const VALID_BACKGROUND_COMMAND_RESULT: (i32, &str) = (3, "unsupported_platform");

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
        .env("XDG_RUNTIME_DIR", temp.path().join("runtime"))
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
            } else if entry.file_type().unwrap().is_file() {
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

#[cfg(target_os = "macos")]
#[test]
fn missing_runtime_variables_refuse_before_state_evidence_or_children() {
    let temp = TempDir::new().unwrap();
    let job_path = fixture(&temp, &valid_job());
    let state = temp.path().join("state");
    let evidence = temp.path().join("evidence");
    let output = Command::new(binary())
        .args([
            "run",
            "--request-id",
            "no-runtime",
            "--job",
            job_path.to_str().unwrap(),
            "--evidence",
            evidence.to_str().unwrap(),
        ])
        .env("XDG_STATE_HOME", &state)
        .env_remove("XDG_RUNTIME_DIR")
        .env_remove("TMPDIR")
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(3));
    let result = one_object(&output);
    assert_eq!(result["error"]["code"], "runtime_directory_unavailable");
    assert!(!state.exists());
    assert!(!evidence.exists());
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
    assert!(!job_copy.contains("classified-fixture-secret"));
    assert!(!job_copy.contains("classified-fixture-redacted"));
    assert!(!job_copy.contains("secret-iso-form"));
    assert!(!job_copy.contains("secret-display-form"));
    assert!(!job_copy.contains("redacted-iso-form"));
    assert!(!job_copy.contains("redacted-display-form"));
    let exported_job: Value = serde_json::from_str(&job_copy).unwrap();
    assert_ne!(
        exported_job["values"]["private_note"]["value"],
        "classified-fixture-secret"
    );
    assert_ne!(
        exported_job["values"]["redacted_note"]["value"],
        "classified-fixture-redacted"
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
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    let temp = hosted_temp();
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
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
    assert_eq!(result["reason"]["code"], EXECUTION_STOP_REASON);
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

    let resume = invoke(
        &temp,
        &[
            "resume",
            "r_0000000000000001",
            "--request-id",
            "resume-1",
            "--input",
            job_path.to_str().unwrap(),
        ],
    );
    assert_eq!(resume.status.code(), Some(64));
    assert_eq!(one_object(&resume)["error"]["code"], "invalid_disposition");

    for args in [
        vec!["status", "r_0000000000000001"],
        vec!["abort", "r_0000000000000001", "--request-id", "abort-1"],
    ] {
        let output = invoke(&temp, &args);
        assert_eq!(
            output.status.code(),
            Some(VALID_BACKGROUND_COMMAND_RESULT.0)
        );
        assert_eq!(
            one_object(&output)["error"]["code"],
            VALID_BACKGROUND_COMMAND_RESULT.1
        );
    }

    for args in [
        vec!["status"],
        vec!["status", "r_0000000000000001", "--request-id", "status-1"],
    ] {
        let output = invoke(&temp, &args);
        assert_eq!(output.status.code(), Some(64));
        assert_eq!(one_object(&output)["error"]["code"], "invalid_arguments");
    }
}

#[test]
fn caller_run_ids_reject_traversal_and_aliases_before_state_access() {
    let temp = TempDir::new().unwrap();
    let input = temp.path().join("disposition.json");
    fs::write(&input, b"{}").unwrap();
    for run_id in [
        "../outside",
        "/tmp/absolute",
        ".",
        "r_0000000000000001/../other",
        "r_short",
        "r_000000000000000_",
    ] {
        for args in [
            vec!["status", run_id],
            vec!["abort", run_id, "--request-id", "abort-invalid-run"],
            vec![
                "resume",
                run_id,
                "--request-id",
                "resume-invalid-run",
                "--input",
                input.to_str().unwrap(),
            ],
        ] {
            let output = invoke(&temp, &args);
            assert_eq!(output.status.code(), Some(64));
            assert_eq!(one_object(&output)["error"]["code"], "invalid_run_id");
        }
    }
    assert!(!temp.path().join("state").exists());
    assert!(!temp.path().join("outside").exists());
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
        "complete_nonterminal_result",
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
        } else if corruption == "complete_nonterminal_result" {
            let result_path = run_dir.join("result.json");
            let mut result: Value =
                serde_json::from_slice(&fs::read(&result_path).unwrap()).unwrap();
            result["state"] = json!("uncertain");
            result["terminal"] = json!(false);
            write_private_json(&result_path, &result);
            let mut manifest: Value =
                serde_json::from_slice(&fs::read(&manifest_path).unwrap()).unwrap();
            let result_artifact = manifest["artifacts"]
                .as_array_mut()
                .unwrap()
                .iter_mut()
                .find(|artifact| artifact["role"] == "result")
                .unwrap();
            result_artifact["digest"] =
                json!(hex::encode(Sha256::digest(fs::read(&result_path).unwrap())));
            write_private_json(&manifest_path, &manifest);
            let mut record: Value = serde_json::from_slice(&fs::read(&index).unwrap()).unwrap();
            record["result"] = result;
            record["exit_code"] = json!(2);
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

#[cfg(target_os = "linux")]
#[test]
fn caller_loss_does_not_kill_host_and_active_request_ids_attach_without_restart() {
    use std::os::unix::fs::PermissionsExt;
    use std::time::{Duration, Instant};

    let temp = TempDir::new().unwrap();
    let runtime = temp.path().join("runtime");
    fs::create_dir(&runtime).unwrap();
    fs::set_permissions(&runtime, fs::Permissions::from_mode(0o700)).unwrap();
    let mut job = valid_job();
    job["values"]["missing_name"] =
        json!({"value":"Wallet","description":"Previously missing name"});
    job["steps"][0]["done_when"] = json!([{"url_contains":"/never"}]);
    let job_path = fixture(&temp, &job);
    let changed_path = temp.path().join("changed.json");
    let mut changed = job.clone();
    changed["context"]["revision"] = json!("other");
    fs::write(&changed_path, serde_json::to_vec(&changed).unwrap()).unwrap();
    let fake_browser = temp.path().join("fake-browser");
    fs::write(&fake_browser, b"#!/bin/sh\nsleep 60\n").unwrap();
    fs::set_permissions(&fake_browser, fs::Permissions::from_mode(0o700)).unwrap();
    let evidence = temp.path().join("evidence");
    let state = temp.path().join("state");
    let args = [
        "run",
        "--request-id",
        "detached-request",
        "--job",
        job_path.to_str().unwrap(),
        "--evidence",
        evidence.to_str().unwrap(),
        "--browser",
        fake_browser.to_str().unwrap(),
    ];
    let mut caller = Command::new(binary())
        .args(args)
        .env("XDG_STATE_HOME", &state)
        .env("XDG_RUNTIME_DIR", &runtime)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();

    let ready_deadline = Instant::now() + Duration::from_secs(10);
    let admitted = loop {
        let output = Command::new(binary())
            .args([
                "status",
                "--request-id",
                "detached-request",
                "--wait-ms",
                "0",
            ])
            .env("XDG_STATE_HOME", &state)
            .env("XDG_RUNTIME_DIR", &runtime)
            .output()
            .unwrap();
        if output.status.code() == Some(6) {
            break one_object(&output);
        }
        assert!(Instant::now() < ready_deadline, "host never became ready");
        std::thread::sleep(Duration::from_millis(20));
    };
    let control_path = state
        .join("manuvra/runs")
        .join(admitted["run_id"].as_str().unwrap())
        .join("control.json");
    let running = wait_for_ready_host_control(&control_path);
    caller.kill().unwrap();
    caller.wait().unwrap();

    let attached = Command::new(binary())
        .args(args.into_iter().chain(["--wait-ms", "0"]))
        .env("XDG_STATE_HOME", &state)
        .env("XDG_RUNTIME_DIR", &runtime)
        .output()
        .unwrap();
    assert_eq!(attached.status.code(), Some(6));
    assert_eq!(one_object(&attached)["run_id"], running["run_id"]);

    let conflict = Command::new(binary())
        .args([
            "run",
            "--request-id",
            "detached-request",
            "--job",
            changed_path.to_str().unwrap(),
            "--evidence",
            evidence.to_str().unwrap(),
            "--browser",
            fake_browser.to_str().unwrap(),
            "--wait-ms",
            "0",
        ])
        .env("XDG_STATE_HOME", &state)
        .env("XDG_RUNTIME_DIR", &runtime)
        .output()
        .unwrap();
    assert_eq!(conflict.status.code(), Some(64));
    assert_eq!(one_object(&conflict)["error"]["code"], "request_conflict");

    let recovered = Command::new(binary())
        .args([
            "status",
            "--request-id",
            "detached-request",
            "--wait-ms",
            "45000",
        ])
        .env("XDG_STATE_HOME", &state)
        .env("XDG_RUNTIME_DIR", &runtime)
        .output()
        .unwrap();
    assert_eq!(recovered.status.code(), Some(3));
    assert_eq!(
        one_object(&recovered)["reason"]["code"],
        "browser_launch_failed"
    );
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[test]
fn identical_active_paused_request_attaches_before_terminal_evidence_recovery() {
    let temp = hosted_temp();
    let mut job = fault_window_job();
    job["options"]["pause_timeout_ms"] = json!(5_000);
    let job_path = fixture(&temp, &job);
    let first = Command::new(binary())
        .args([
            "run",
            "--request-id",
            "active-paused",
            "--job",
            job_path.to_str().unwrap(),
            "--evidence",
            temp.path().join("evidence").to_str().unwrap(),
            "--browser",
            "/missing/fault-browser",
            "--wait-ms",
            "1000",
        ])
        .env("XDG_STATE_HOME", temp.path().join("state"))
        .env("XDG_RUNTIME_DIR", temp.path().join("runtime"))
        .env("MANUVRA_TEST_HOST_FAULT", "pause_abort")
        .output()
        .unwrap();
    assert_eq!(first.status.code(), Some(2));
    let initial = one_object(&first);
    let control_path = temp
        .path()
        .join("state/manuvra/runs")
        .join(initial["run_id"].as_str().unwrap())
        .join("control.json");
    let paused = wait_for_control_value(&control_path, |value| {
        value["result"]["state"] == "uncertain"
            && value["result"]["terminal"] == false
            && value["result"]["evidence"]["complete"] == true
    });
    let run_id = initial["run_id"].as_str().unwrap();
    assert_eq!(paused["run_id"], run_id);

    let attached = invoke(
        &temp,
        &[
            "run",
            "--request-id",
            "active-paused",
            "--job",
            temp.path().join("job.json").to_str().unwrap(),
            "--evidence",
            temp.path().join("evidence").to_str().unwrap(),
            "--browser",
            "/missing/fault-browser",
            "--wait-ms",
            "0",
        ],
    );
    assert_eq!(attached.status.code(), Some(2));
    let attached = one_object(&attached);
    assert_eq!(attached["run_id"], run_id);
    assert_eq!(attached["state"], "uncertain");

    let original_control: Value =
        serde_json::from_slice(&fs::read(&control_path).unwrap()).unwrap();
    let stale_socket = temp.path().join("runtime/stale-control.sock");
    let listener = std::os::unix::net::UnixListener::bind(&stale_socket).unwrap();
    let expected_run_id = run_id.to_owned();
    let stale_peer = std::thread::spawn(move || {
        use std::io::Read;

        let (mut stream, _) = listener.accept().unwrap();
        let mut request = Vec::new();
        stream.read_to_end(&mut request).unwrap();
        let _ = serde_json::to_writer(
            &mut stream,
            &json!({
                "ipc_version":1,
                "run_id":expected_run_id,
                "job_digest":"stale-digest",
                "accepted":true,
                "result":{"state":"uncertain","terminal":false}
            }),
        );
    });
    let mut stale_control = original_control.clone();
    stale_control["socket"] = json!(stale_socket);
    write_private_json(&control_path, &stale_control);
    let rejected = invoke(
        &temp,
        &[
            "run",
            "--request-id",
            "active-paused",
            "--job",
            temp.path().join("job.json").to_str().unwrap(),
            "--evidence",
            temp.path().join("evidence").to_str().unwrap(),
            "--browser",
            "/missing/fault-browser",
            "--wait-ms",
            "0",
        ],
    );
    stale_peer.join().unwrap();
    assert_eq!(rejected.status.code(), Some(70));
    assert_eq!(one_object(&rejected)["error"]["code"], "internal");
    assert_eq!(
        fs::read_dir(temp.path().join("evidence")).unwrap().count(),
        1
    );
    write_private_json(&control_path, &original_control);

    let aborted = invoke(
        &temp,
        &["abort", run_id, "--request-id", "paused-attach-abort"],
    );
    assert_eq!(aborted.status.code(), Some(5));
    assert_eq!(one_object(&aborted)["state"], "aborted");
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn fault_window_job() -> Value {
    json!({
        "schema_version": 1,
        "target": {"kind":"browser","url":"http://127.0.0.1:4351/"},
        "context": {"journey":"host fault fixture","revision":"run-lifecycle","environment":"fake","actor":"synthetic owner","authority":"fixture only"},
        "values": {},
        "steps": [{"id":"submit","goal":"submit","done_when":[{"url_contains":"/done"}]}],
        "expectations": [],
        "options": {"allowed_origins":["http://127.0.0.1:4351"],"active_timeout_ms":1000,"pause_timeout_ms":100,"lifetime_ms":5000}
    })
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn start_fault_window(temp: &TempDir, scenario: &str) -> (Value, PathBuf, PathBuf) {
    let mut job = fault_window_job();
    if scenario == "pause_abort" {
        job["options"]["pause_timeout_ms"] = json!(30_000);
        job["options"]["lifetime_ms"] = json!(30_000);
    }
    let job_path = fixture(temp, &job);
    let evidence = temp.path().join("evidence");
    let state = temp.path().join("state");
    let runtime = temp.path().join("runtime");
    let mut command = hosted_command();
    isolate_abrupt_exit_coverage(&mut command, temp);
    let output = command
        .args([
            "run",
            "--request-id",
            scenario,
            "--job",
            job_path.to_str().unwrap(),
            "--evidence",
            evidence.to_str().unwrap(),
            "--browser",
            "/missing/fault-browser",
            "--wait-ms",
            "0",
        ])
        .env("XDG_STATE_HOME", &state)
        .env("XDG_RUNTIME_DIR", &runtime)
        .env("MANUVRA_TEST_HOST_FAULT", scenario)
        .env("MANUVRA_TEST_SHUTDOWN_GRACE_MS", "20")
        .output()
        .unwrap();
    assert!(
        matches!(output.status.code(), Some(2 | 3 | 5 | 6)),
        "unexpected status {:?}: stdout={} stderr={}",
        output.status.code(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let initial = one_object(&output);
    let run_id = initial["run_id"].as_str().unwrap().to_owned();
    let control = state
        .join("manuvra/runs")
        .join(&run_id)
        .join("control.json");
    let runtime_run = runtime.join("manuvra/runs").join(run_id);
    (initial, control, runtime_run)
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn hosted_command() -> Command {
    Command::new(binary())
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn isolate_abrupt_exit_coverage(command: &mut Command, temp: &TempDir) {
    if std::env::var_os("LLVM_PROFILE_FILE").is_some() {
        // These fixtures intentionally SIGKILL descendants. Their production boundaries have
        // direct coverage; keep necessarily truncated profiles out of the strict merge set.
        command.env(
            "LLVM_PROFILE_FILE",
            temp.path().join("abrupt-exit-%p.profraw"),
        );
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn hosted_invoke(temp: &TempDir, args: &[&str]) -> Output {
    hosted_command()
        .args(args)
        .env("XDG_STATE_HOME", temp.path().join("state"))
        .env("XDG_RUNTIME_DIR", temp.path().join("runtime"))
        .output()
        .unwrap()
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn hosted_temp() -> TempDir {
    tempfile::Builder::new()
        .prefix("m4")
        .tempdir_in("/tmp")
        .unwrap()
}

#[cfg(target_os = "macos")]
fn public_darwin_forced_job(http: &DarwinHttpFixture, pause_timeout_ms: u64) -> Value {
    json!({
        "schema_version":1,
        "target":{"kind":"browser","url":http.url()},
        "context":{
            "journey":"Exercise a public Darwin disposition",
            "revision":"slice-5",
            "environment":"disposable HTTP fixture",
            "actor":"synthetic owner",
            "authority":"click Activate once"
        },
        "values":{
            "classified_marker":{
                "value":"public-darwin-redaction-marker",
                "description":"Evidence redaction probe",
                "secret":true
            }
        },
        "steps":[{
            "id":"activate",
            "goal":"Click Activate.",
            "done_when":[{"text_visible":"Activated"}]
        }],
        "expectations":[],
        "options":{
            "allowed_origins":[format!("http://{}", http.address)],
            "active_timeout_ms":30_000,
            "pause_timeout_ms":pause_timeout_ms,
            "lifetime_ms":60_000,
            "debug":{"force_stop_at_step":"activate"}
        }
    })
}

#[cfg(target_os = "macos")]
fn start_public_darwin_forced_run(
    temporary: &TempDir,
    http: &DarwinHttpFixture,
    request_id: &str,
    pause_timeout_ms: u64,
) -> Value {
    let job = public_darwin_forced_job(http, pause_timeout_ms);
    let job_path = fixture(temporary, &job);
    let chrome = "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome";
    assert!(Path::new(chrome).is_file(), "Google Chrome is unavailable");
    let output = Command::new(binary())
        .args([
            "run",
            "--request-id",
            request_id,
            "--job",
            job_path.to_str().unwrap(),
            "--evidence",
            temporary.path().join("evidence").to_str().unwrap(),
            "--browser",
            chrome,
            "--headless",
            "--wait-ms",
            "45000",
        ])
        .env("XDG_STATE_HOME", temporary.path().join("state"))
        .env("XDG_RUNTIME_DIR", temporary.path().join("runtime"))
        .output()
        .unwrap();
    assert_eq!(
        output.status.code(),
        Some(2),
        "stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let result = one_object(&output);
    assert_eq!(result["state"], "uncertain");
    assert_ne!(result["reason"]["code"], "unsupported_platform");
    assert!(
        result["escalation"]["dispositions"]
            .as_array()
            .unwrap()
            .contains(&json!("execute"))
    );
    result
}

#[cfg(target_os = "macos")]
fn assert_complete_artifacts(result: &Value) {
    assert_eq!(result["evidence"]["complete"], true);
    let manifest_path = PathBuf::from(result["evidence"]["manifest"].as_str().unwrap());
    let manifest: Value = serde_json::from_slice(&fs::read(manifest_path).unwrap()).unwrap();
    assert_eq!(manifest["complete"], true);
    for artifact in manifest["artifacts"].as_array().unwrap() {
        assert_eq!(artifact["complete"], true);
        let bytes = fs::read(artifact["path"].as_str().unwrap()).unwrap();
        assert_eq!(artifact["digest"], hex::encode(Sha256::digest(bytes)));
    }
}

#[cfg(target_os = "macos")]
#[test]
fn public_darwin_run_recovers_by_run_and_request_id_with_owned_chrome() {
    let temporary = hosted_temp();
    let http = DarwinHttpFixture::start();
    let url = http.url();
    let job = json!({
        "schema_version":1,
        "target":{"kind":"browser","url":url},
        "context":{
            "journey":"Darwin hosted runtime fixture",
            "revision":"slice-5",
            "environment":"disposable HTTP fixture",
            "actor":"synthetic owner",
            "authority":"fixture only"
        },
        "values":{},
        "steps":[{"id":"ready","goal":"Observe the fixture","done_when":[{"url_contains":"/ready"}]}],
        "expectations":[],
        "options":{
            "allowed_origins":[format!("http://{}", http.address)],
            "active_timeout_ms":10_000,
            "pause_timeout_ms":5_000,
            "lifetime_ms":30_000
        }
    });
    let job_path = fixture(&temporary, &job);
    let chrome = "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome";
    assert!(
        Path::new(chrome).is_file(),
        "Google Chrome fixture is unavailable"
    );
    let output = hosted_command()
        .args([
            "run",
            "--request-id",
            "darwin-hosted-http",
            "--job",
            job_path.to_str().unwrap(),
            "--evidence",
            temporary.path().join("evidence").to_str().unwrap(),
            "--browser",
            chrome,
            "--headless",
            "--wait-ms",
            "45000",
        ])
        .env("XDG_STATE_HOME", temporary.path().join("state"))
        .env("XDG_RUNTIME_DIR", temporary.path().join("runtime"))
        .output()
        .unwrap();
    assert_eq!(
        output.status.code(),
        Some(6),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let initial = one_object(&output);
    let run_id = initial["run_id"].as_str().unwrap();
    let control_path = temporary
        .path()
        .join("state/manuvra/runs")
        .join(run_id)
        .join("control.json");
    let running = wait_for_control_value(&control_path, |control| {
        control["host"]["pid"].is_u64() && control["watchdog"]["pid"].is_u64()
    });
    let host_pid = running["host"]["pid"].as_u64().unwrap() as u32;
    let watchdog_pid = running["watchdog"]["pid"].as_u64().unwrap() as u32;
    let status_by_run = hosted_command()
        .args(["status", run_id, "--wait-ms", "45000"])
        .env("XDG_STATE_HOME", temporary.path().join("state"))
        .env("XDG_RUNTIME_DIR", temporary.path().join("runtime"))
        .output()
        .unwrap();
    assert_eq!(
        status_by_run.status.code(),
        Some(0),
        "stdout={} stderr={}",
        String::from_utf8_lossy(&status_by_run.stdout),
        String::from_utf8_lossy(&status_by_run.stderr)
    );
    let result = one_object(&status_by_run);
    assert_eq!(result["run_id"], initial["run_id"]);
    assert_eq!(result["state"], "passed");
    assert_eq!(result["terminal"], true);
    assert_eq!(result["verdict"]["steps"][0]["result"], "satisfied");
    assert_eq!(result["cleanup"]["browser"], "closed");
    assert_eq!(result["cleanup"]["profile"], "removed");
    assert_eq!(result["evidence"]["complete"], true);
    let manifest = PathBuf::from(result["evidence"]["manifest"].as_str().unwrap());
    assert!(manifest.is_file());
    let status_by_request = hosted_command()
        .args([
            "status",
            "--request-id",
            "darwin-hosted-http",
            "--wait-ms",
            "0",
        ])
        .env("XDG_STATE_HOME", temporary.path().join("state"))
        .env("XDG_RUNTIME_DIR", temporary.path().join("runtime"))
        .output()
        .unwrap();
    assert_eq!(status_by_request.status.code(), Some(0));
    assert_eq!(one_object(&status_by_request), result);
    let repeated = hosted_command()
        .args([
            "run",
            "--request-id",
            "darwin-hosted-http",
            "--job",
            job_path.to_str().unwrap(),
            "--evidence",
            temporary.path().join("evidence").to_str().unwrap(),
            "--browser",
            chrome,
            "--headless",
            "--wait-ms",
            "0",
        ])
        .env("XDG_STATE_HOME", temporary.path().join("state"))
        .env("XDG_RUNTIME_DIR", temporary.path().join("runtime"))
        .output()
        .unwrap();
    assert_eq!(repeated.status.code(), Some(0));
    assert_eq!(repeated.stdout, status_by_run.stdout);
    assert_process_gone(host_pid);
    assert_process_gone(watchdog_pid);
}

#[cfg(target_os = "macos")]
#[test]
#[ignore = "requires Google Chrome and TYPESAFE_API_KEY"]
fn public_darwin_forced_disposition_revalidates_and_finishes_same_run() {
    let provider_key = std::env::var("TYPESAFE_API_KEY")
        .ok()
        .filter(|key| !key.is_empty())
        .expect("TYPESAFE_API_KEY is required");
    let temporary = hosted_temp();
    let http = DarwinHttpFixture::start();
    let state = temporary.path().join("state");
    let runtime = temporary.path().join("runtime");
    let evidence = temporary.path().join("evidence");
    let initial = start_public_darwin_forced_run(&temporary, &http, "darwin-public-resume", 30_000);
    let run_id = initial["run_id"].as_str().unwrap();
    let control_path = state.join("manuvra/runs").join(run_id).join("control.json");
    let paused = wait_for_control_value(&control_path, |control| {
        control["result"]["state"] == "uncertain" && control["host"]["pid"].is_u64()
    });
    let host_pid = paused["host"]["pid"].as_u64().unwrap();

    let by_run = Command::new(binary())
        .args(["status", run_id, "--wait-ms", "0"])
        .env("XDG_STATE_HOME", &state)
        .env("XDG_RUNTIME_DIR", &runtime)
        .output()
        .unwrap();
    let by_request = Command::new(binary())
        .args([
            "status",
            "--request-id",
            "darwin-public-resume",
            "--wait-ms",
            "0",
        ])
        .env("XDG_STATE_HOME", &state)
        .env("XDG_RUNTIME_DIR", &runtime)
        .output()
        .unwrap();
    assert_eq!(by_run.status.code(), Some(2));
    assert_eq!(by_request.status.code(), Some(2));
    assert_eq!(one_object(&by_run)["run_id"], run_id);
    assert_eq!(one_object(&by_request)["run_id"], run_id);
    let attached_control: Value =
        serde_json::from_slice(&fs::read(&control_path).unwrap()).unwrap();
    assert_eq!(attached_control["host"]["pid"], host_pid);

    let payload_path = PathBuf::from(initial["escalation"]["payload"].as_str().unwrap());
    let payload: Value = serde_json::from_slice(&fs::read(payload_path).unwrap()).unwrap();
    let disposition_path = temporary.path().join("execute.json");
    fs::write(
        &disposition_path,
        serde_json::to_vec(&json!({
            "schema_version":1,
            "escalation_id":initial["escalation"]["id"],
            "disposition":{
                "kind":"execute",
                "candidate_id":payload["offered_candidate"]["id"]
            }
        }))
        .unwrap(),
    )
    .unwrap();
    let resume_args = [
        "resume",
        run_id,
        "--request-id",
        "darwin-public-resume-execute",
        "--input",
        disposition_path.to_str().unwrap(),
    ];
    let resumed = Command::new(binary())
        .args(resume_args)
        .env("XDG_STATE_HOME", &state)
        .env("XDG_RUNTIME_DIR", &runtime)
        .output()
        .unwrap();
    assert_eq!(resumed.status.code(), Some(6));
    let resumed_checkpoint = one_object(&resumed);
    assert_eq!(resumed_checkpoint["run_id"], run_id);
    assert_eq!(resumed_checkpoint["state"], "running");
    assert_eq!(resumed_checkpoint["terminal"], false);
    assert_eq!(resumed_checkpoint["verdict"]["caller_assisted"], true);

    let repeated_resume = Command::new(binary())
        .args(resume_args)
        .env("XDG_STATE_HOME", &state)
        .env("XDG_RUNTIME_DIR", &runtime)
        .output()
        .unwrap();
    assert_eq!(repeated_resume.status.code(), Some(6));
    assert_eq!(repeated_resume.stdout, resumed.stdout);
    let terminal_output = Command::new(binary())
        .args(["status", run_id, "--wait-ms", "45000"])
        .env("XDG_STATE_HOME", &state)
        .env("XDG_RUNTIME_DIR", &runtime)
        .output()
        .unwrap();
    assert_eq!(terminal_output.status.code(), Some(0));
    let terminal = one_object(&terminal_output);
    assert_eq!(terminal["run_id"], run_id);
    assert_eq!(terminal["state"], "passed");
    assert_eq!(terminal["terminal"], true);
    assert_eq!(terminal["verdict"]["caller_assisted"], true);
    assert_eq!(terminal["cleanup"]["browser"], "closed");
    assert_eq!(terminal["cleanup"]["profile"], "removed");
    assert_complete_artifacts(&terminal);
    let manifest_path = PathBuf::from(terminal["evidence"]["manifest"].as_str().unwrap());
    let trace = fs::read_to_string(manifest_path.parent().unwrap().join("trace.jsonl")).unwrap();
    assert!(trace.lines().any(|line| {
        serde_json::from_str::<Value>(line).is_ok_and(|entry| {
            entry["event"] == "action_prepared" && entry["basis"] == "caller_authority"
        })
    }));
    for marker in [provider_key.as_bytes(), b"public-darwin-redaction-marker"] {
        let evidence_bytes = all_file_bytes(&evidence);
        assert!(
            !evidence_bytes
                .windows(marker.len())
                .any(|part| part == marker)
        );
    }
    assert_process_gone(host_pid as u32);
    assert_path_gone(
        &runtime
            .join("manuvra/runs")
            .join(run_id)
            .join("control.sock"),
    );
}

#[cfg(target_os = "macos")]
#[test]
#[ignore = "requires Google Chrome and TYPESAFE_API_KEY"]
fn public_darwin_abort_and_expiry_close_owned_chrome() {
    let provider_key = std::env::var("TYPESAFE_API_KEY")
        .ok()
        .filter(|key| !key.is_empty())
        .expect("TYPESAFE_API_KEY is required");
    let abort_temp = hosted_temp();
    let abort_http = DarwinHttpFixture::start();
    let abort_state = abort_temp.path().join("state");
    let abort_runtime = abort_temp.path().join("runtime");
    let abort_initial =
        start_public_darwin_forced_run(&abort_temp, &abort_http, "darwin-public-abort", 30_000);
    let abort_run_id = abort_initial["run_id"].as_str().unwrap();
    let abort_control_path = abort_state
        .join("manuvra/runs")
        .join(abort_run_id)
        .join("control.json");
    let abort_control = wait_for_control_value(&abort_control_path, |control| {
        control["result"]["state"] == "uncertain" && control["host"]["pid"].is_u64()
    });
    let abort_host_pid = abort_control["host"]["pid"].as_u64().unwrap() as u32;
    let aborted = Command::new(binary())
        .args([
            "abort",
            abort_run_id,
            "--request-id",
            "darwin-public-abort-control",
        ])
        .env("XDG_STATE_HOME", &abort_state)
        .env("XDG_RUNTIME_DIR", &abort_runtime)
        .output()
        .unwrap();
    assert_eq!(aborted.status.code(), Some(5));
    let aborted_result = one_object(&aborted);
    assert_eq!(aborted_result["run_id"], abort_run_id);
    assert_eq!(aborted_result["state"], "aborted");
    assert_eq!(aborted_result["reason"]["code"], "caller_aborted");
    assert_eq!(aborted_result["cleanup"]["browser"], "closed");
    assert_eq!(aborted_result["cleanup"]["profile"], "removed");
    assert_complete_artifacts(&aborted_result);
    for marker in [provider_key.as_bytes(), b"public-darwin-redaction-marker"] {
        let evidence_bytes = all_file_bytes(&abort_temp.path().join("evidence"));
        assert!(
            !evidence_bytes
                .windows(marker.len())
                .any(|part| part == marker)
        );
    }
    let repeated_abort = Command::new(binary())
        .args([
            "abort",
            abort_run_id,
            "--request-id",
            "darwin-public-abort-control",
        ])
        .env("XDG_STATE_HOME", &abort_state)
        .env("XDG_RUNTIME_DIR", &abort_runtime)
        .output()
        .unwrap();
    assert_eq!(repeated_abort.status.code(), Some(5));
    assert_eq!(repeated_abort.stdout, aborted.stdout);
    assert_process_gone(abort_host_pid);
    assert_path_gone(
        &abort_runtime
            .join("manuvra/runs")
            .join(abort_run_id)
            .join("control.sock"),
    );

    let expiry_temp = hosted_temp();
    let expiry_http = DarwinHttpFixture::start();
    let expiry_state = expiry_temp.path().join("state");
    let expiry_runtime = expiry_temp.path().join("runtime");
    let expiry_initial =
        start_public_darwin_forced_run(&expiry_temp, &expiry_http, "darwin-public-expiry", 500);
    let expiry_run_id = expiry_initial["run_id"].as_str().unwrap();
    let expiry_control_path = expiry_state
        .join("manuvra/runs")
        .join(expiry_run_id)
        .join("control.json");
    let expiry_control = wait_for_control_value(&expiry_control_path, |control| {
        control["result"]["state"] == "uncertain" && control["host"]["pid"].is_u64()
    });
    let expiry_host_pid = expiry_control["host"]["pid"].as_u64().unwrap() as u32;
    let _ = wait_for_control_value(&expiry_control_path, |control| {
        control["result"]["state"] == "expired" && control["result"]["terminal"] == true
    });
    let expired = Command::new(binary())
        .args(["status", expiry_run_id, "--wait-ms", "0"])
        .env("XDG_STATE_HOME", &expiry_state)
        .env("XDG_RUNTIME_DIR", &expiry_runtime)
        .output()
        .unwrap();
    assert_eq!(expired.status.code(), Some(5));
    let expired_result = one_object(&expired);
    assert_eq!(expired_result["run_id"], expiry_run_id);
    assert_eq!(expired_result["state"], "expired");
    assert_eq!(expired_result["reason"]["code"], "resume_deadline_elapsed");
    assert_eq!(expired_result["cleanup"]["browser"], "closed");
    assert_eq!(expired_result["cleanup"]["profile"], "removed");
    assert_complete_artifacts(&expired_result);
    for marker in [provider_key.as_bytes(), b"public-darwin-redaction-marker"] {
        let evidence_bytes = all_file_bytes(&expiry_temp.path().join("evidence"));
        assert!(
            !evidence_bytes
                .windows(marker.len())
                .any(|part| part == marker)
        );
    }
    assert_process_gone(expiry_host_pid);
    assert_path_gone(
        &expiry_runtime
            .join("manuvra/runs")
            .join(expiry_run_id)
            .join("control.sock"),
    );
}

#[cfg(target_os = "macos")]
#[test]
fn public_darwin_commands_use_hosted_errors_instead_of_unsupported_platform() {
    let host = Command::new(binary())
        .arg("__host")
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert_eq!(host.status.code(), Some(70));

    let temporary = hosted_temp();
    let job_path = fixture(&temporary, &fault_window_job());
    let public = Command::new(binary())
        .args([
            "run",
            "--request-id",
            "public-darwin-missing-browser",
            "--job",
            job_path.to_str().unwrap(),
            "--evidence",
            temporary.path().join("public-evidence").to_str().unwrap(),
            "--browser",
            "/missing/public-darwin-browser",
        ])
        .env("XDG_STATE_HOME", temporary.path().join("public-state"))
        .env("XDG_RUNTIME_DIR", temporary.path().join("public-runtime"))
        .output()
        .unwrap();
    assert_eq!(public.status.code(), Some(3));
    assert_eq!(one_object(&public)["reason"]["code"], "browser_unavailable");

    for args in [
        vec!["status", "r_1234567890abcdef"],
        vec![
            "abort",
            "r_1234567890abcdef",
            "--request-id",
            "closed-abort",
        ],
    ] {
        let output = Command::new(binary()).args(args).output().unwrap();
        assert_eq!(output.status.code(), Some(64));
        assert_eq!(one_object(&output)["error"]["code"], "run_not_found");
    }

    let disposition = temporary.path().join("disposition.json");
    fs::write(
        &disposition,
        serde_json::to_vec(&json!({
            "schema_version":1,
            "escalation_id":"e_closed",
            "disposition":{"kind":"abort"}
        }))
        .unwrap(),
    )
    .unwrap();
    let resume = Command::new(binary())
        .args([
            "resume",
            "r_1234567890abcdef",
            "--request-id",
            "closed-resume",
            "--input",
            disposition.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert_eq!(resume.status.code(), Some(64));
    assert_eq!(one_object(&resume)["error"]["code"], "run_not_found");
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn wait_for_control_value(control: &Path, predicate: impl Fn(&Value) -> bool) -> Value {
    use std::time::{Duration, Instant};
    let deadline = Instant::now() + Duration::from_secs(15);
    let mut last = None;
    loop {
        if let Ok(bytes) = fs::read(control)
            && let Ok(value) = serde_json::from_slice::<Value>(&bytes)
        {
            if predicate(&value) {
                return value;
            }
            last = Some(value);
        }
        assert!(
            Instant::now() < deadline,
            "control condition was not published at {}: {last:?}",
            control.display()
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[cfg(target_os = "linux")]
fn wait_for_ready_host_control(control_path: &Path) -> Value {
    use std::io::{Read, Write};
    use std::net::Shutdown;
    use std::os::unix::net::UnixStream;
    use std::time::{Duration, Instant};

    let deadline = Instant::now() + Duration::from_secs(15);
    let mut last = None;
    loop {
        if let Some(control) = published_host_control(control_path) {
            if ready_handshake_matches_control(&control, &mut |socket| {
                let mut stream = UnixStream::connect(socket)?;
                stream.set_read_timeout(Some(Duration::from_millis(250)))?;
                stream.set_write_timeout(Some(Duration::from_millis(250)))?;
                serde_json::to_writer(&mut stream, &json!({"kind":"ready","ipc_version":1}))?;
                stream.write_all(b"\n")?;
                stream.shutdown(Shutdown::Write)?;
                let mut response = Vec::new();
                stream.read_to_end(&mut response)?;
                serde_json::from_slice(&response).map_err(std::io::Error::other)
            }) {
                return control;
            }
            last = Some(control);
        }
        assert!(
            Instant::now() < deadline,
            "host did not complete a validated readiness handshake at {}: {last:?}",
            control_path.display()
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[cfg(target_os = "linux")]
fn published_host_control(control_path: &Path) -> Option<Value> {
    let bytes = fs::read(control_path).ok()?;
    let control: Value = serde_json::from_slice(&bytes).ok()?;
    let sequence = control["sequence"].as_u64()?;
    (sequence > 0 && control["host"]["pid"].is_u64() && control["watchdog"]["pid"].is_u64())
        .then_some(control)
}

#[cfg(target_os = "linux")]
fn ready_handshake_matches_control(
    control: &Value,
    request: &mut impl FnMut(&Path) -> std::io::Result<Value>,
) -> bool {
    let Some(socket) = control["socket"].as_str() else {
        return false;
    };
    let Ok(response) = request(Path::new(socket)) else {
        return false;
    };
    response["ipc_version"] == 1
        && response["accepted"] == true
        && response["run_id"] == control["run_id"]
        && response["job_digest"] == control["job_digest"]
        && response["result"]["state"] == "running"
        && response["result"]["terminal"] == false
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn wait_for_pid_file(path: &Path) -> u32 {
    use std::time::{Duration, Instant};
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        if let Ok(value) = fs::read_to_string(path)
            && let Ok(pid) = value.parse()
        {
            return pid;
        }
        assert!(
            Instant::now() < deadline,
            "fixture PID was not published at {}",
            path.display()
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn wait_for_file(path: &Path) -> Vec<u8> {
    use std::time::{Duration, Instant};
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        if let Ok(value) = fs::read(path) {
            return value;
        }
        assert!(
            Instant::now() < deadline,
            "fixture file was not published: {}",
            path.display()
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn assert_process_gone(pid: u32) {
    use std::time::{Duration, Instant};
    let deadline = Instant::now() + Duration::from_secs(15);
    while unsafe { libc::kill(pid as i32, 0) } == 0 {
        assert!(Instant::now() < deadline, "process {pid} remained alive");
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[cfg(target_os = "macos")]
fn assert_path_gone(path: &Path) {
    use std::time::{Duration, Instant};

    let deadline = Instant::now() + Duration::from_secs(15);
    while path.exists() {
        assert!(
            Instant::now() < deadline,
            "owned runtime path remained: {}",
            path.display()
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[test]
fn host_crash_windows_hang_and_watchdog_loss_publish_without_another_cli_call() {
    for (scenario, prepared, dispatched) in [
        ("before_action_prepared", false, false),
        ("after_action_prepared", true, false),
        ("after_dispatch_before_receipt", true, true),
    ] {
        let temp = hosted_temp();
        let (initial, control_path, runtime_run) = start_fault_window(&temp, scenario);
        let browser_pid = wait_for_pid_file(&runtime_run.join("fake-browser.pid"));
        let browser_child_pid = wait_for_pid_file(&runtime_run.join("fake-browser-child.pid"));
        let terminal =
            wait_for_control_value(&control_path, |value| value["result"]["terminal"] == true);
        assert_eq!(terminal["result"]["state"], "blocked");
        assert_eq!(terminal["result"]["reason"]["code"], "host_lost");
        assert_eq!(terminal["result"]["verdict"]["overall"], "unresolved");
        assert_eq!(
            terminal["result"]["verdict"]["steps"][0]["result"],
            "unresolved"
        );
        assert_eq!(
            terminal["result"]["reason"]["current_action"],
            if prepared { "uncertain" } else { "none" }
        );
        let run_id = initial["run_id"].as_str().unwrap();
        let journal = temp
            .path()
            .join("evidence")
            .join(format!(".{run_id}.action-journal.jsonl"));
        let journal_text = fs::read_to_string(&journal).unwrap_or_default();
        assert_eq!(journal_text.contains("action_prepared"), prepared);
        let dispatch_log = runtime_run.join("fake-browser.dispatch.log");
        assert_eq!(dispatch_log.is_file(), dispatched);
        if dispatched {
            assert!(
                String::from_utf8(wait_for_file(&dispatch_log))
                    .unwrap()
                    .contains("dispatch click submit")
            );
        }
        assert_process_gone(browser_pid);
        assert_process_gone(browser_child_pid);
    }

    let temp = hosted_temp();
    let (_, control_path, runtime_run) = start_fault_window(&temp, "pause_hang");
    let browser_pid = wait_for_pid_file(&runtime_run.join("fake-browser.pid"));
    let browser_child_pid = wait_for_pid_file(&runtime_run.join("fake-browser-child.pid"));
    let terminal =
        wait_for_control_value(&control_path, |value| value["result"]["terminal"] == true);
    assert_eq!(terminal["result"]["state"], "expired");
    assert_eq!(
        terminal["result"]["reason"]["code"],
        "resume_deadline_elapsed"
    );
    assert_eq!(terminal["result"]["evidence"]["complete"], true);
    let manifest_path = PathBuf::from(terminal["result"]["evidence"]["manifest"].as_str().unwrap());
    let manifest: Value = serde_json::from_slice(&fs::read(&manifest_path).unwrap()).unwrap();
    assert_eq!(manifest["complete"], true);
    let persisted: Value = serde_json::from_slice(
        &fs::read(manifest_path.parent().unwrap().join("result.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(persisted["terminal"], true);
    assert_eq!(persisted["state"], "expired");
    let retry = hosted_command()
        .args([
            "run",
            "--request-id",
            "pause_hang",
            "--job",
            temp.path().join("job.json").to_str().unwrap(),
            "--evidence",
            temp.path().join("evidence").to_str().unwrap(),
            "--browser",
            "/missing/fault-browser",
            "--wait-ms",
            "0",
        ])
        .env("XDG_STATE_HOME", temp.path().join("state"))
        .env("XDG_RUNTIME_DIR", temp.path().join("runtime"))
        .output()
        .unwrap();
    assert_eq!(retry.status.code(), Some(5));
    let retried = one_object(&retry);
    assert_eq!(retried["run_id"], terminal["result"]["run_id"]);
    assert_eq!(retried["state"], "expired");
    assert_eq!(
        fs::read_dir(temp.path().join("evidence")).unwrap().count(),
        1
    );
    assert_process_gone(browser_pid);
    assert_process_gone(browser_child_pid);

    let temp = hosted_temp();
    let (_, control_path, runtime_run) = start_fault_window(&temp, "watchdog_lost");
    let browser_pid = wait_for_pid_file(&runtime_run.join("fake-browser.pid"));
    let browser_child_pid = wait_for_pid_file(&runtime_run.join("fake-browser-child.pid"));
    let running = wait_for_control_value(&control_path, |value| value["watchdog"]["pid"].is_u64());
    let watchdog_pid = running["watchdog"]["pid"].as_u64().unwrap() as i32;
    assert_eq!(unsafe { libc::kill(watchdog_pid, libc::SIGKILL) }, 0);
    let terminal =
        wait_for_control_value(&control_path, |value| value["result"]["terminal"] == true);
    assert_eq!(terminal["result"]["state"], "blocked");
    assert_eq!(terminal["result"]["reason"]["code"], "watchdog_lost");
    assert_eq!(terminal["result"]["cleanup"]["browser"], "closed");
    assert_process_gone(browser_pid);
    assert_process_gone(browser_child_pid);
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[test]
fn watchdog_reconciles_host_loss_at_every_bootstrap_checkpoint() {
    use std::time::{Duration, Instant};

    for point in ["before_lock", "after_lock", "after_socket", "after_control"] {
        let temp = hosted_temp();
        let job_path = fixture(&temp, &fault_window_job());
        let started = Instant::now();
        let mut command = hosted_command();
        isolate_abrupt_exit_coverage(&mut command, &temp);
        let output = command
            .args([
                "run",
                "--request-id",
                point,
                "--job",
                job_path.to_str().unwrap(),
                "--evidence",
                temp.path().join("evidence").to_str().unwrap(),
                "--browser",
                "/missing/fault-browser",
                "--wait-ms",
                "15000",
            ])
            .env("XDG_STATE_HOME", temp.path().join("state"))
            .env("XDG_RUNTIME_DIR", temp.path().join("runtime"))
            .env("MANUVRA_TEST_HOST_BOOTSTRAP_FAULT", point)
            .env("MANUVRA_TEST_SHUTDOWN_GRACE_MS", "20")
            .output()
            .unwrap();
        assert!(started.elapsed() < Duration::from_secs(15));
        assert_eq!(output.status.code(), Some(3));
        let result = one_object(&output);
        assert_eq!(result["state"], "blocked");
        assert_eq!(result["reason"]["code"], "host_lost");
        assert_eq!(result["reason"]["current_action"], "none");
        let run_id = result["run_id"].as_str().unwrap();
        let state_run = temp.path().join("state/manuvra/runs").join(run_id);
        assert!(state_run.join("run.lock").is_file());
        assert!(state_run.join("control.json").is_file());

        let retry = hosted_invoke(
            &temp,
            &[
                "run",
                "--request-id",
                point,
                "--job",
                job_path.to_str().unwrap(),
                "--evidence",
                temp.path().join("evidence").to_str().unwrap(),
                "--browser",
                "/missing/fault-browser",
                "--wait-ms",
                "0",
            ],
        );
        assert_eq!(retry.status.code(), Some(3));
        assert_eq!(one_object(&retry)["run_id"], run_id);
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[test]
fn retry_reconciles_caller_death_before_watchdog_without_relaunch() {
    use std::os::unix::process::ExitStatusExt;
    use std::time::{Duration, Instant};

    let temp = hosted_temp();
    let job_path = fixture(&temp, &fault_window_job());
    let evidence = temp.path().join("evidence");
    let state = temp.path().join("state");
    let runtime = temp.path().join("runtime");
    let args = [
        "run",
        "--request-id",
        "caller-bootstrap-death",
        "--job",
        job_path.to_str().unwrap(),
        "--evidence",
        evidence.to_str().unwrap(),
        "--browser",
        "/missing/fault-browser",
        "--wait-ms",
        "0",
    ];
    let mut caller_command = hosted_command();
    isolate_abrupt_exit_coverage(&mut caller_command, &temp);
    let mut caller = caller_command
        .args(args)
        .env("XDG_STATE_HOME", &state)
        .env("XDG_RUNTIME_DIR", &runtime)
        .env("MANUVRA_TEST_CALLER_BOOTSTRAP_FAULT", "after_run_basis")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();

    let runs_root = state.join("manuvra/runs");
    let deadline = Instant::now() + Duration::from_secs(15);
    let (run_id, state_run, runtime_run) = loop {
        if let Ok(entries) = fs::read_dir(&runs_root) {
            let runs = entries
                .filter_map(Result::ok)
                .filter(|entry| entry.path().join("control.json").is_file())
                .collect::<Vec<_>>();
            if let [entry] = runs.as_slice() {
                let run_id = entry.file_name().to_string_lossy().into_owned();
                let runtime_run = runtime.join("manuvra/runs").join(&run_id);
                if runtime_run.join("caller-bootstrap-fault.ready").is_file() {
                    break (run_id, entry.path(), runtime_run);
                }
            }
        }
        assert!(
            Instant::now() < deadline,
            "caller did not reach the pre-watchdog fault point"
        );
        std::thread::sleep(Duration::from_millis(10));
    };

    let initial: Value =
        serde_json::from_slice(&fs::read(state_run.join("control.json")).unwrap()).unwrap();
    assert_eq!(initial["sequence"], 0);
    assert_eq!(initial["host"], Value::Null);
    assert_eq!(initial["watchdog"], Value::Null);
    assert!(!runtime_run.join("control.sock").exists());
    assert!(!runtime_run.join("fake-browser.pid").exists());

    assert_eq!(unsafe { libc::kill(caller.id() as i32, libc::SIGKILL) }, 0);
    assert_eq!(caller.wait().unwrap().signal(), Some(libc::SIGKILL));

    let retry = |state: &Path, runtime: &Path| {
        hosted_command()
            .args(args)
            .env("XDG_STATE_HOME", state)
            .env("XDG_RUNTIME_DIR", runtime)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap()
    };
    let first = retry(&state, &runtime);
    let second = retry(&state, &runtime);
    let first = first.wait_with_output().unwrap();
    let second = second.wait_with_output().unwrap();
    for output in [&first, &second] {
        assert_eq!(output.status.code(), Some(3));
        let result = one_object(output);
        assert_eq!(result["run_id"], run_id);
        assert_eq!(result["state"], "blocked");
        assert_eq!(result["terminal"], true);
        assert_eq!(result["reason"]["code"], "host_lost");
        assert_eq!(result["reason"]["current_action"], "none");
        assert_eq!(result["cleanup"]["browser"], "not_started");
        assert_eq!(result["evidence"]["complete"], false);
    }

    let recovered: Value =
        serde_json::from_slice(&fs::read(state_run.join("control.json")).unwrap()).unwrap();
    assert_eq!(recovered["sequence"], 1);
    assert_eq!(recovered["host"], Value::Null);
    assert_eq!(recovered["watchdog"], Value::Null);
    assert!(!runtime_run.join("control.sock").exists());
    assert!(!runtime_run.join("fake-browser.pid").exists());
    assert_eq!(fs::read_dir(&evidence).unwrap().count(), 0);
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[test]
fn watchdog_bounds_terminal_checkpoint_hang_and_sweeps_the_process_group() {
    let temp = hosted_temp();
    let (_, control_path, runtime_run) = start_fault_window(&temp, "terminal_hang");
    let browser_pid = wait_for_pid_file(&runtime_run.join("fake-browser.pid"));
    let browser_child_pid = wait_for_pid_file(&runtime_run.join("fake-browser-child.pid"));
    let terminal = wait_for_control_value(&control_path, |value| {
        value["result"]["reason"]["code"] == "terminal_checkpoint_fixture"
            && value["watchdog"]["pid"].is_u64()
    });
    let host_pid = terminal["host"]["pid"].as_u64().unwrap() as u32;
    let watchdog_pid = terminal["watchdog"]["pid"].as_u64().unwrap() as u32;
    assert_process_gone(host_pid);
    assert_process_gone(browser_pid);
    assert_process_gone(browser_child_pid);
    assert_process_gone(watchdog_pid);
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[test]
fn paused_abort_is_deduplicated_and_conflicting_control_request_is_rejected() {
    let temp = hosted_temp();
    let (initial, _, runtime_run) = start_fault_window(&temp, "pause_abort");
    let run_id = initial["run_id"].as_str().unwrap();
    let browser_pid = wait_for_pid_file(&runtime_run.join("fake-browser.pid"));
    let control_path = temp
        .path()
        .join("state/manuvra/runs")
        .join(run_id)
        .join("control.json");
    let control = wait_for_control_value(&control_path, |value| value["watchdog"]["pid"].is_u64());
    let watchdog_pid = control["watchdog"]["pid"].as_u64().unwrap() as u32;
    let process_group = control["host"]["process_group"].as_u64().unwrap() as i32;
    assert_eq!(unsafe { libc::getpgid(browser_pid as i32) }, process_group);
    let args = ["abort", run_id, "--request-id", "abort-once"];
    let first = hosted_invoke(&temp, &args);
    assert_eq!(first.status.code(), Some(5));
    let first_value = one_object(&first);
    assert_eq!(first_value["state"], "aborted");
    assert_eq!(first_value["reason"]["code"], "caller_aborted");
    assert_process_gone(browser_pid);
    assert_process_gone(watchdog_pid);

    let repeated = hosted_invoke(&temp, &args);
    assert_eq!(repeated.status.code(), Some(5));
    assert_eq!(repeated.stdout, first.stdout);

    let conflict = hosted_invoke(
        &temp,
        &["abort", "r_0000000000000002", "--request-id", "abort-once"],
    );
    assert_eq!(conflict.status.code(), Some(64));
    assert_eq!(one_object(&conflict)["error"]["code"], "request_conflict");
}

#[cfg(target_os = "linux")]
#[test]
fn watchdog_lifetime_state_excludes_provider_key_embedded_in_caller_text() {
    let temp = TempDir::new().unwrap();
    let key = "provider-key-inside-hostile-caller-text";
    let mut job = fault_window_job();
    job["context"]["journey"] = json!(format!("journey repeats {key}"));
    job["context"]["actor"] = json!(key);
    job["steps"][0]["goal"] = json!(format!("never export {key}"));
    job["options"]["pause_timeout_ms"] = json!(3_000);
    let job_path = fixture(&temp, &job);
    let output = Command::new(binary())
        .args([
            "run",
            "--request-id",
            key,
            "--job",
            job_path.to_str().unwrap(),
            "--evidence",
            temp.path().join("evidence").to_str().unwrap(),
            "--browser",
            "/missing/fault-browser",
            "--wait-ms",
            "1000",
        ])
        .env("XDG_STATE_HOME", temp.path().join("state"))
        .env("XDG_RUNTIME_DIR", temp.path().join("runtime"))
        .env("TYPESAFE_API_KEY", key)
        .env("MANUVRA_TEST_HOST_FAULT", "pause_abort")
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(2));
    let paused = one_object(&output);
    assert_eq!(paused["state"], "uncertain");
    assert!(
        !output
            .stdout
            .windows(key.len())
            .any(|part| part == key.as_bytes())
    );
    let run_id = paused["run_id"].as_str().unwrap();
    let control_path = temp
        .path()
        .join("state/manuvra/runs")
        .join(run_id)
        .join("control.json");
    let control = wait_for_control_value(&control_path, |value| {
        value["result"]["state"] == "uncertain" && value["watchdog"]["pid"].is_u64()
    });
    let watchdog_pid = control["watchdog"]["pid"].as_u64().unwrap();
    for field in ["cmdline", "environ"] {
        let bytes = fs::read(format!("/proc/{watchdog_pid}/{field}")).unwrap();
        assert!(!bytes.windows(key.len()).any(|part| part == key.as_bytes()));
    }
    for root in [
        temp.path().join("state"),
        temp.path().join("runtime"),
        temp.path().join("evidence"),
    ] {
        assert_no_marker(&root, key);
    }

    let abort = invoke(
        &temp,
        &["abort", run_id, "--request-id", "secret-proof-abort"],
    );
    assert_eq!(abort.status.code(), Some(5));
    assert_eq!(one_object(&abort)["state"], "aborted");
    assert_no_marker(&temp.path().join("state"), key);
    assert_no_marker(&temp.path().join("evidence"), key);
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
    assert_export_boundary(&output, &evidence, provider, EXECUTION_STOP_REASON);
}
