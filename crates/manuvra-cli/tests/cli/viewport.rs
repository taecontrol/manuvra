use super::*;

const VIEWPORT_FIXTURE: &str = include_str!("../../../../tests/browser/viewport.html");

fn invoke(temp: &TempDir, args: &[&str]) -> (i32, Value) {
    let output = hosted_command()
        .args(args)
        .env("XDG_STATE_HOME", temp.path().join("state"))
        .env("XDG_RUNTIME_DIR", temp.path().join("runtime"))
        .env_remove("TYPESAFE_API_KEY")
        .output()
        .unwrap();
    assert!(
        output.status.code().is_some(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    (output.status.code().unwrap(), one_object(&output))
}

fn run(temp: &TempDir, url: &str, viewport: Option<(u16, u16)>) -> (i32, Value) {
    let origin = url.split('/').take(3).collect::<Vec<_>>().join("/");
    let mut job = json!({
        "schema_version":1,
        "target":{"kind":"browser","url":url},
        "context":{"journey":"Observe initial viewport","revision":"fixture","environment":"synthetic HTTP fixture","actor":"synthetic owner","authority":"read only"},
        "steps":[{"id":"ready","goal":"Observe the loaded target","done_when":[{"url_contains":"/ready"}]}],
        "options":{"allowed_origins":[origin],"active_timeout_ms":30000,"pause_timeout_ms":30000,"lifetime_ms":60000}
    });
    if let Some((width, height)) = viewport {
        job["options"]["viewport"] = json!({"width":width,"height":height});
    }
    manuvra_contract::Job::parse(&serde_json::to_vec(&job).unwrap()).unwrap();
    let job_path = fixture(temp, &job);
    let evidence = temp.path().join("evidence");
    let mut result = invoke(
        temp,
        &[
            "run",
            "--request-id",
            "viewport-observation",
            "--job",
            job_path.to_str().unwrap(),
            "--evidence",
            evidence.to_str().unwrap(),
            "--headless",
            "--wait-ms",
            "1000",
        ],
    );
    for _ in 0..4 {
        if result.1["state"] != "running" {
            break;
        }
        result = invoke(
            temp,
            &[
                "status",
                result.1["run_id"].as_str().unwrap(),
                "--wait-ms",
                "30000",
            ],
        );
    }
    assert_ne!(result.1["state"], "running", "{}", result.1);
    result
}

fn provenance(result: &Value) -> Value {
    let manifest = PathBuf::from(result["evidence"]["manifest"].as_str().unwrap());
    serde_json::from_slice(&fs::read(manifest.parent().unwrap().join("provenance.json")).unwrap())
        .unwrap()
}

pub(super) fn assert_no_step_input(result: &Value) {
    let manifest = PathBuf::from(result["evidence"]["manifest"].as_str().unwrap());
    let trace = fs::read_to_string(manifest.parent().unwrap().join("trace.jsonl")).unwrap();
    for line in trace.lines() {
        let event: Value = serde_json::from_str(line).unwrap();
        assert_ne!(event["event"], "action_prepared");
    }
}

fn retain_viewport_capture(result: &Value) {
    let Some(directory) = std::env::var_os("MANUVRA_FIXTURE_EVIDENCE") else {
        return;
    };
    let directory = PathBuf::from(directory);
    fs::create_dir_all(&directory).unwrap();
    let manifest = PathBuf::from(result["evidence"]["manifest"].as_str().unwrap());
    let manifest: Value = serde_json::from_slice(&fs::read(manifest).unwrap()).unwrap();
    for (role, name) in [
        ("screenshot", "cli-viewport-390x844.png"),
        ("observation", "cli-viewport-390x844-observation.json"),
    ] {
        let artifact = manifest["artifacts"]
            .as_array()
            .unwrap()
            .iter()
            .find(|artifact| artifact["role"] == role)
            .unwrap();
        fs::copy(artifact["path"].as_str().unwrap(), directory.join(name)).unwrap();
    }
    fs::write(
        directory.join("cli-viewport-provenance.json"),
        serde_json::to_vec_pretty(&provenance(result)).unwrap(),
    )
    .unwrap();
}

#[test]
#[ignore = "requires the local Chromium executable"]
fn viewport_cli_provenance_records_initial_target_width_and_verifiable_artifacts() {
    let http = HttpFixture::with_body(VIEWPORT_FIXTURE);
    for (requested, width, height) in [(Some((390, 844)), 390, 844), (None, 1120, 780)] {
        let temporary = hosted_temp();
        let (exit, result) = run(&temporary, &http.url(), requested);
        assert_eq!(exit, 0, "{result}");
        assert_eq!(result["state"], "passed");
        assert_eq!(
            provenance(&result)["viewport"],
            json!({"width":width,"height":height,"initial_client_width":width})
        );
        assert_eq!(result["cleanup"]["browser"], "closed");
        assert_eq!(result["cleanup"]["profile"], "removed");
        assert_complete_artifacts(&result);
        assert_no_step_input(&result);
        if requested == Some((390, 844)) {
            retain_viewport_capture(&result);
        }
        let recovered = invoke(
            &temporary,
            &["status", "--request-id", "viewport-observation"],
        );
        assert_eq!(recovered.0, 0);
        assert_eq!(provenance(&recovered.1), provenance(&result));
    }
}

#[test]
#[ignore = "requires the local Chromium executable"]
fn viewport_cli_provenance_records_measured_mismatch_without_normalizing_it() {
    let http = HttpFixture::with_body(
        "<!doctype html><title>Measured mismatch</title><script>Object.defineProperty(document.documentElement,'clientWidth',{get:()=>375})</script>",
    );
    let temporary = hosted_temp();
    let (exit, result) = run(&temporary, &http.url(), Some((390, 844)));
    assert_eq!(exit, 0, "{result}");
    assert_eq!(
        provenance(&result)["viewport"],
        json!({"width":390,"height":844,"initial_client_width":375})
    );
    assert_complete_artifacts(&result);
    assert_no_step_input(&result);
}

#[test]
#[ignore = "requires the local Chromium executable"]
fn viewport_cli_blocks_before_steps_when_initial_measurement_is_unavailable() {
    for getter in ["return 'invalid'", "throw new Error('Width unavailable')"] {
        let html = format!(
            "<!doctype html><script>Object.defineProperty(document.documentElement,'clientWidth',{{get(){{{getter}}}}})</script>"
        );
        let http = HttpFixture::with_body(&html);
        let temporary = hosted_temp();
        let (exit, result) = run(&temporary, &http.url(), Some((390, 844)));
        assert_eq!(exit, 3, "{getter}: {result}");
        assert_eq!(result["state"], "blocked");
        assert_eq!(result["reason"]["code"], "browser_control_failed");
        assert_eq!(result["verdict"]["steps"][0]["result"], "not_run");
        assert_eq!(
            provenance(&result)["viewport"],
            json!({"width":390,"height":844,"initial_client_width":null})
        );
        assert_eq!(result["cleanup"]["browser"], "closed");
        assert_eq!(result["cleanup"]["profile"], "removed");
        assert_complete_artifacts(&result);
        assert_no_step_input(&result);
    }
}

#[test]
#[ignore = "requires the local Chromium executable"]
fn viewport_cli_failed_navigation_keeps_initial_width_explicitly_unavailable() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}/ready", listener.local_addr().unwrap());
    drop(listener);
    let temporary = hosted_temp();
    let (exit, result) = run(&temporary, &url, Some((390, 844)));
    assert_eq!(exit, 3, "{result}");
    assert_eq!(result["reason"]["code"], "browser_control_failed");
    assert_eq!(
        provenance(&result)["viewport"],
        json!({"width":390,"height":844,"initial_client_width":null})
    );
    assert_eq!(result["verdict"]["steps"][0]["result"], "not_run");
    assert_eq!(result["cleanup"]["browser"], "closed");
    assert_eq!(result["cleanup"]["profile"], "removed");
    assert_complete_artifacts(&result);
    assert_no_step_input(&result);
}
