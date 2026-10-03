use super::*;

const COLOR_FIXTURE: &str = include_str!("../../../../tests/browser/color.html");

fn invoke(temp: &TempDir, args: &[&str]) -> (i32, Value) {
    let output = hosted_command()
        .args(args)
        .env("XDG_STATE_HOME", temp.path().join("state"))
        .env("XDG_RUNTIME_DIR", temp.path().join("runtime"))
        .env_remove("TYPESAFE_API_KEY")
        .output()
        .unwrap();
    (output.status.code().unwrap(), one_object(&output))
}

fn color_job(url: &str, assertions: Value) -> Value {
    json!({
        "schema_version":1,"target":{"kind":"browser","url":url},
        "context":{"journey":"Verify color","revision":"fixture","environment":"synthetic HTTP","actor":"synthetic owner","authority":"read only"},
        "steps":[{"id":"ready","goal":"Observe the loaded target","done_when":[{"url_contains":"/ready"}]}],
        "expectations":[{"id":"color","assertions":assertions}],
        "options":{"allowed_origins":[url.split('/').take(3).collect::<Vec<_>>().join("/")],"active_timeout_ms":30000,"pause_timeout_ms":30000,"lifetime_ms":60000}
    })
}

fn run(temp: &TempDir, job: &Value) -> (i32, Value) {
    let job_path = fixture(temp, job);
    let evidence = temp.path().join("evidence");
    let mut result = invoke(
        temp,
        &[
            "run",
            "--request-id",
            "color-observation",
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

fn artifact(result: &Value, role: &str) -> Value {
    let path = PathBuf::from(result["evidence"]["manifest"].as_str().unwrap());
    let manifest: Value = serde_json::from_slice(&fs::read(path).unwrap()).unwrap();
    let artifact = manifest["artifacts"]
        .as_array()
        .unwrap()
        .iter()
        .find(|a| a["role"] == role)
        .unwrap();
    serde_json::from_slice(&fs::read(artifact["path"].as_str().unwrap()).unwrap()).unwrap()
}

fn assert_read_only(result: &Value) {
    viewport::assert_no_step_input(result);
    assert_eq!(result["verdict"]["caller_assisted"], false);
    assert_eq!(artifact(result, "verification")["provider"], Value::Null);
    assert_complete_artifacts(result);
}

#[test]
#[ignore = "requires the local Chromium executable"]
fn color_cli_final_checks_pass_and_fail_without_provider_or_mutations() {
    let http = HttpFixture::with_body(COLOR_FIXTURE);
    for (text, exit, state, verdict) in [
        ("-$12.34", 0, "passed", "satisfied"),
        ("$12.34", 4, "failed", "not_satisfied"),
    ] {
        let temp = hosted_temp();
        let job = color_job(
            &http.url(),
            json!([{"color":{"target":{"text":text},"equals":"#b91c1c"}}]),
        );
        let (code, result) = run(&temp, &job);
        assert_eq!(code, exit, "{result}");
        assert_eq!(result["state"], state, "{result}");
        assert_eq!(result["verdict"]["expectations"][0]["result"], verdict);
        assert_eq!(result["cleanup"]["browser"], "closed");
        assert_eq!(result["cleanup"]["profile"], "removed");
        assert_read_only(&result);
        let check =
            artifact(&result, "verification")["expectations"][0]["assertion_checks"][0]["color"]
                .clone();
        assert_eq!(
            check["target"]["raw"],
            if text.starts_with('-') {
                "rgb(185, 28, 28)"
            } else {
                "rgb(31, 41, 55)"
            }
        );
        assert!(check["target"]["rgba"].is_array());
        assert!(!check.to_string().contains("node_id"));
    }
}

#[test]
#[ignore = "requires the local Chromium executable"]
fn color_cli_done_and_relative_final_assertions_share_authoritative_values() {
    let http = HttpFixture::with_body(COLOR_FIXTURE);
    let temp = hosted_temp();
    let same =
        json!({"color":{"target":{"text":"-$12.34"},"same_as":{"text":"Destructive reference"}}});
    let different =
        json!({"color":{"target":{"text":"-$12.34"},"different_from":{"text":"$12.34"}}});
    let mut job = color_job(&http.url(), json!([same.clone(), different]));
    job["steps"][0]["done_when"] = json!([same]);
    let (exit, result) = run(&temp, &job);
    assert_eq!(exit, 0, "{result}");
    assert_eq!(result["state"], "passed");
    assert_read_only(&result);
    let report = artifact(&result, "verification");
    assert_eq!(
        report["expectations"][0]["assertion_checks"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
    let check = &report["expectations"][0]["assertion_checks"][0]["color"];
    assert_eq!(check["target"]["rgba"], json!([185, 28, 28, 255]));
    assert_eq!(check["reference"]["rgba"], json!([185, 28, 28, 255]));
}

#[test]
#[ignore = "requires the local Chromium executable"]
fn color_cli_rejects_ambiguous_scopes_and_records_painted_aria_hidden_evidence() {
    let http = HttpFixture::with_body(COLOR_FIXTURE);
    for (target, expected) in [
        (json!({"text":"$0.00"}), "uncertain"),
        (json!({"text":"$0.00","container":"Checking"}), "passed"),
        (json!({"text":"$0.00","dialog":"Amount details"}), "passed"),
        (json!({"text":"-$9.00"}), "passed"),
        (json!({"name":"Current month","role":"button"}), "failed"),
        (json!({"text":"-$42.00"}), "failed"),
        (json!({"text":"BeforeMiddleAfter"}), "failed"),
        (json!({"text":"Transparent amount"}), "failed"),
    ] {
        let temp = hosted_temp();
        let job = color_job(
            &http.url(),
            json!([{"color":{"target":target,"equals":"#b91c1c"}}]),
        );
        let (_, result) = run(&temp, &job);
        assert_eq!(result["state"], expected, "{job}: {result}");
        assert_read_only(&result);
        if target["text"] == "-$9.00" {
            assert_eq!(
                artifact(&result, "verification")["expectations"][0]["assertion_checks"][0]["color"]
                    ["target"]["channel"],
                "painted_aria_hidden"
            );
            let snapshot = artifact(&result, "observation");
            assert!(
                !snapshot["visible_text"]
                    .as_str()
                    .unwrap()
                    .contains("-$9.00")
            );
        }
        if expected == "uncertain" {
            assert_eq!(
                result["escalation"]["dispositions"],
                json!(["retry_observation", "abort"])
            );
            let (_, ended) = invoke(
                &temp,
                &[
                    "abort",
                    result["run_id"].as_str().unwrap(),
                    "--request-id",
                    "color-abort",
                ],
            );
            assert_eq!(ended["cleanup"]["browser"], "closed");
        }
    }
}

#[test]
#[ignore = "requires the local Chromium executable"]
fn color_cli_classified_text_is_redacted_in_every_persisted_artifact() {
    let http = HttpFixture::with_body(COLOR_FIXTURE);
    let temp = hosted_temp();
    let mut job = color_job(
        &http.url(),
        json!([{"color":{"target":{"text":"-$9.00"},"equals":"#b91c1c"}}]),
    );
    job["values"] =
        json!({"amount":{"value":"-$9.00","description":"classified amount","secret":true}});
    let (exit, result) = run(&temp, &job);
    assert_eq!(exit, 0, "{result}");
    assert_read_only(&result);
    let manifest: Value = serde_json::from_slice(
        &fs::read(result["evidence"]["manifest"].as_str().unwrap()).unwrap(),
    )
    .unwrap();
    for artifact in manifest["artifacts"].as_array().unwrap() {
        let bytes = fs::read(artifact["path"].as_str().unwrap()).unwrap();
        assert!(
            !String::from_utf8_lossy(&bytes).contains("-$9.00"),
            "classified text in {}",
            artifact["role"]
        );
    }
    assert!(!result.to_string().contains("-$9.00"));
    assert_eq!(
        artifact(&result, "verification")["expectations"][0]["assertion_checks"][0]["color"]["target"]
            ["rgba"],
        json!([185, 28, 28, 255])
    );
}

#[test]
#[ignore = "requires the local Chromium executable"]
fn color_cli_scoped_shadow_owners_share_their_main_document_scopes() {
    let http = HttpFixture::with_body(include_str!(
        "../../../../tests/browser/color-eligibility.html"
    ));
    for target in [
        json!({"text":"Scoped shadow amount","dialog":"Shadow details","container":"Shadow checking"}),
        json!({"name":"Scoped shadow choice","role":"button","dialog":"Shadow details","container":"Shadow checking"}),
        json!({"text":"$0.01","container":"Direct checking"}),
        json!({"text":"Scoped visible amount","container":"Filtered checking"}),
    ] {
        let temp = hosted_temp();
        let job = color_job(
            &http.url(),
            json!([{"color":{"target":target,"equals":"#b91c1c"}}]),
        );
        let (exit, result) = run(&temp, &job);
        assert_eq!(exit, 0, "{result}");
        assert_read_only(&result);
    }
}

#[test]
#[ignore = "requires the local Chromium executable"]
fn colorless_capture_and_stable_color_targets_allow_unrelated_color_animation() {
    let http = HttpFixture::with_body(
        r#"<!doctype html><style>@keyframes pulse{from{color:black}to{color:white}}#pulse{animation:pulse 1s linear infinite alternate}</style><p id=pulse>Animated</p><p>Ready</p><p style='color:#b91c1c'>Amount</p>"#,
    );
    for assertions in [
        json!([{"text_visible":"Ready"}]),
        json!([{"color":{"target":{"text":"Amount"},"equals":"#b91c1c"}}]),
    ] {
        let temp = hosted_temp();
        let mut job = color_job(&http.url(), assertions);
        job["steps"][0]["done_when"] = json!([{"text_visible":"Ready"}]);
        let (exit, result) = run(&temp, &job);
        assert_eq!(exit, 0, "{result}");
        assert_read_only(&result);
    }
}

#[test]
#[ignore = "requires the local Chromium executable"]
fn color_cli_exact_text_case_whitespace_named_roles_and_literal_alpha() {
    let http = HttpFixture::with_body(COLOR_FIXTURE);
    for (target, equals, state) in [
        (json!({"text":"inherited amount"}), "#b91c1c", "failed"),
        (json!({"text":"  Inherited  amount  "}), "#b91c1c", "passed"),
        (
            json!({"name":"CURRENT MONTH","role":"button"}),
            "#1f2937",
            "passed",
        ),
        (
            json!({"name":"Current month","role":"link"}),
            "#1f2937",
            "failed",
        ),
        (json!({"text":"Low alpha amount"}), "#0102031a", "passed"),
    ] {
        let temp = hosted_temp();
        let job = color_job(
            &http.url(),
            json!([{"color":{"target":target,"equals":equals}}]),
        );
        let (_, result) = run(&temp, &job);
        assert_eq!(result["state"], state, "{job}: {result}");
        assert_read_only(&result);
    }
}

#[test]
#[ignore = "requires the local Chromium executable"]
fn color_cli_unicode_text_targets_match_painted_segments_without_a_provider() {
    for text in ["Amount\u{85}line", "Amount\u{feff}line"] {
        let body = format!(
            "<!doctype html><meta charset='utf-8'><p>Ready</p><p style='color:#b91c1c'>{text}</p>"
        );
        let http = HttpFixture::with_body(&body);
        let temp = hosted_temp();
        let job = color_job(
            &http.url(),
            json!([{"color":{"target":{"text":text},"equals":"#b91c1c"}}]),
        );
        let (exit, result) = run(&temp, &job);
        assert_eq!(exit, 0, "{text:?}: {result}");
        assert_eq!(result["state"], "passed");
        assert_read_only(&result);
    }
}

#[test]
#[ignore = "requires the local Chromium executable"]
fn color_cli_duplicate_owners_in_shadow_and_frame_contexts_remain_unresolved() {
    let http = HttpFixture::with_body(&COLOR_FIXTURE.replace("Frame amount", "Shadow amount"));
    let temp = hosted_temp();
    let job = color_job(
        &http.url(),
        json!([{"color":{"target":{"text":"Shadow amount"},"equals":"#b91c1c"}}]),
    );
    let (_, result) = run(&temp, &job);
    assert_eq!(result["state"], "uncertain", "{result}");
    assert_eq!(
        artifact(&result, "verification")["expectations"][0]["assertion_checks"][0]["color"]["reason"],
        "ambiguous_owner"
    );
    let (_, ended) = invoke(
        &temp,
        &[
            "abort",
            result["run_id"].as_str().unwrap(),
            "--request-id",
            "duplicate-context-abort",
        ],
    );
    assert_eq!(ended["cleanup"]["browser"], "closed");
}

#[test]
#[ignore = "requires the local Chromium executable"]
fn color_cli_classified_scopes_and_canonical_channel_numerals_publish_complete_evidence() {
    for (body, target, secret, tolerance) in [
        (
            "<!doctype html><p>Ready</p><p style='color:#b91c1c'>185</p>".to_owned(),
            json!({"text":"185"}),
            "185",
            0,
        ),
        (
            "<!doctype html><p>Ready</p><p style='color:#b91c1c'>185</p>".to_owned(),
            json!({"text":"185"}),
            "185",
            185,
        ),
        (
            COLOR_FIXTURE.to_owned(),
            json!({"text":"$0.00","container":"Checking"}),
            "Checking",
            0,
        ),
    ] {
        let http = HttpFixture::with_body(&body);
        let temp = hosted_temp();
        let mut job = color_job(
            &http.url(),
            json!([{"color":{"target":target,"equals":"#b91c1c","tolerance":tolerance}}]),
        );
        job["values"] =
            json!({"marker":{"value":secret,"description":"classified value","secret":true}});
        let (exit, result) = run(&temp, &job);
        assert_eq!(exit, 0, "{result}");
        assert_read_only(&result);
        let target = artifact(&result,"verification")["expectations"][0]["assertion_checks"][0]["color"]["target"].clone();
        assert_eq!(target["rgba"], json!([185, 28, 28, 255]));
        assert!(!target["selector"].to_string().contains(secret));
        if secret == "185" {
            assert!(!target["raw"].as_str().unwrap().contains(secret));
        }
    }
}

#[test]
#[ignore = "requires the local Chromium executable"]
fn color_cli_classified_relative_reference_retains_both_numeric_values() {
    let http = HttpFixture::with_body(COLOR_FIXTURE);
    let temp = hosted_temp();
    let mut job = color_job(
        &http.url(),
        json!([{"color":{"target":{"text":"-$12.34"},"same_as":{"text":"Destructive reference"}}}]),
    );
    job["values"] = json!({"marker":{"value":"Destructive reference","description":"classified reference","secret":true}});
    let (exit, result) = run(&temp, &job);
    assert_eq!(exit, 0, "{result}");
    assert_read_only(&result);
    let check =
        artifact(&result, "verification")["expectations"][0]["assertion_checks"][0]["color"]
            .clone();
    assert_eq!(check["target"]["rgba"], json!([185, 28, 28, 255]));
    assert_eq!(check["reference"]["rgba"], json!([185, 28, 28, 255]));
    assert!(!check.to_string().contains("Destructive reference"));
}

#[test]
fn color_cli_invalid_job_stops_before_creating_evidence() {
    let temp = hosted_temp();
    let job = color_job(
        "http://127.0.0.1:4351/ready",
        json!([{"color":{"target":{"text":"Amount"},"equals":"#invalid"}}]),
    );
    let path = fixture(&temp, &job);
    let evidence = temp.path().join("evidence");
    let (exit, result) = invoke(
        &temp,
        &[
            "run",
            "--request-id",
            "invalid-color",
            "--job",
            path.to_str().unwrap(),
            "--evidence",
            evidence.to_str().unwrap(),
            "--headless",
        ],
    );
    assert_eq!(exit, 64);
    assert_eq!(result["error"]["code"], "invalid_job");
    assert!(!evidence.exists());
}
