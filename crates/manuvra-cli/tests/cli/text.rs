use super::*;

const TEXT_FIXTURE: &str = include_str!("../../../../tests/browser/text.html");

fn invoke(temp: &TempDir, args: &[&str]) -> (i32, Value) {
    let output = hosted_command()
        .args(args)
        .env("XDG_STATE_HOME", temp.path().join("state"))
        .env("XDG_RUNTIME_DIR", temp.path().join("runtime"))
        .env_remove("TYPESAFE_API_KEY")
        .output()
        .unwrap();
    assert!(output.stderr.is_empty(), "{output:?}");
    (output.status.code().unwrap(), one_object(&output))
}

fn job(url: &str, assertion: Value) -> Value {
    json!({
        "schema_version":1,"target":{"kind":"browser","url":url},
        "context":{"journey":"Verify painted text","revision":"fixture","environment":"synthetic HTTP","actor":"owner","authority":"read only"},
        "steps":[{"id":"ready","goal":"Observe target","done_when":[{"url_contains":"/ready"}]}],
        "expectations":[{"id":"text","assertions":[assertion]}],
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
            "text-observation",
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
    let manifest: Value = serde_json::from_slice(
        &fs::read(result["evidence"]["manifest"].as_str().unwrap()).unwrap(),
    )
    .unwrap();
    let artifact = manifest["artifacts"]
        .as_array()
        .unwrap()
        .iter()
        .find(|a| a["role"] == role)
        .unwrap();
    serde_json::from_slice(&fs::read(artifact["path"].as_str().unwrap()).unwrap()).unwrap()
}

fn terminal_read_only(result: &Value) {
    viewport::assert_no_step_input(result);
    assert_eq!(result["verdict"]["caller_assisted"], false);
    assert_eq!(artifact(result, "verification")["provider"], Value::Null);
    assert_eq!(result["cleanup"]["browser"], "closed");
    assert_eq!(result["cleanup"]["profile"], "removed");
    assert_complete_artifacts(result);
}

fn assert_no_classified_state(root: &Path, sensitive: &str) {
    for entry in fs::read_dir(root).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            assert_no_classified_state(&path, sensitive);
        } else if path.is_file() {
            let bytes = fs::read(&path).unwrap();
            assert!(
                !bytes
                    .windows(sensitive.len())
                    .any(|chunk| chunk == sensitive.as_bytes()),
                "classified text leaked in {}",
                path.display()
            );
        }
    }
}

#[test]
#[ignore = "requires the local Chromium executable"]
fn text_cli_defaults_opt_in_and_absence_publish_authoritative_channel_evidence() {
    let http = HttpFixture::with_body(TEXT_FIXTURE);
    for (assertion, state, channel) in [
        (json!({"text_visible":"$0.00"}), "failed", Value::Null),
        (
            json!({"text_visible":"$0.00","include_aria_hidden":true}),
            "passed",
            json!("painted_aria_hidden"),
        ),
        (json!({"text_absent":"$0.00"}), "passed", Value::Null),
        (
            json!({"text_absent":"$0.00","include_aria_hidden":true}),
            "failed",
            json!("painted_aria_hidden"),
        ),
        (
            json!({"text_visible":"$2.00","include_aria_hidden":true}),
            "passed",
            json!("accessible"),
        ),
        (
            json!({"text_absent":"$30.00","include_aria_hidden":true}),
            "passed",
            Value::Null,
        ),
    ] {
        let temp = hosted_temp();
        let wire = job(&http.url(), assertion.clone());
        let (_, result) = run(&temp, &wire);
        assert_eq!(result["state"], state, "{assertion}: {result}");
        terminal_read_only(&result);
        let report = artifact(&result, "verification");
        let checks = &report["expectations"][0]["assertion_checks"];
        assert_eq!(
            checks,
            &result["verdict"]["expectations"][0]["assertion_checks"]
        );
        assert_eq!(checks[0]["text"]["matched_channel"], channel);
        assert!(!checks.to_string().contains("node_id"));
    }
}

#[test]
#[ignore = "requires the local Chromium executable"]
fn text_cli_step_and_final_checks_share_channels_and_classified_text_is_redacted() {
    let http = HttpFixture::with_body(TEXT_FIXTURE);
    let temp = hosted_temp();
    let assertion = json!({"text_visible":"$0.00","include_aria_hidden":true});
    let mut wire = job(&http.url(), assertion.clone());
    wire["steps"][0]["done_when"] = json!([assertion]);
    wire["expectations"][0]["assertions"]
        .as_array_mut()
        .unwrap()
        .push(json!({"color":{"target":{"text":"$0.00"},"equals":"#b91c1c"}}));
    wire["values"] =
        json!({"marker":{"value":"$0.00","description":"classified amount","secret":true}});
    let (code, result) = run(&temp, &wire);
    assert_eq!(code, 0, "{result}");
    terminal_read_only(&result);
    let verification = artifact(&result, "verification");
    let checks = &verification["expectations"][0]["assertion_checks"];
    assert_eq!(checks[0]["text"]["matched_channel"], "painted_aria_hidden");
    assert_eq!(
        checks[1]["color"]["target"]["channel"],
        "painted_aria_hidden"
    );
    let manifest: Value = serde_json::from_slice(
        &fs::read(result["evidence"]["manifest"].as_str().unwrap()).unwrap(),
    )
    .unwrap();
    let mut text_step = false;
    for artifact in manifest["artifacts"].as_array().unwrap() {
        let bytes = fs::read(artifact["path"].as_str().unwrap()).unwrap();
        assert!(
            !bytes.windows(5).any(|chunk| chunk == b"$0.00"),
            "classified marker leaked in {}",
            artifact["role"]
        );
        if artifact["role"] == "trace" {
            text_step = String::from_utf8(bytes)
                .unwrap()
                .lines()
                .map(|line| serde_json::from_str::<Value>(line).unwrap())
                .any(|entry| {
                    entry["assertion_checks"][0]["text"]["matched_channel"] == "painted_aria_hidden"
                });
        }
    }
    assert!(text_step, "step evidence must name its text channel");
    let observation = artifact(&result, "observation");
    assert!(observation.get("painted_text").is_some());
    assert!(!result.to_string().contains("$0.00"));
}

#[test]
#[ignore = "requires the local Chromium executable"]
fn text_cli_classified_accessible_and_dialog_sources_are_redacted() {
    let http = HttpFixture::with_body(TEXT_FIXTURE);
    for (kind, text, dialog, state, channel) in [
        ("text_visible", "$1.00", None, "passed", "accessible"),
        (
            "text_visible",
            "$20.00",
            Some("Month details"),
            "passed",
            "painted_aria_hidden",
        ),
        (
            "text_absent",
            "$20.00",
            Some("Month details"),
            "failed",
            "painted_aria_hidden",
        ),
    ] {
        let temp = hosted_temp();
        let mut assertion = json!({kind:text,"include_aria_hidden":true});
        let mut done = json!({"text_visible":text,"include_aria_hidden":true});
        if let Some(dialog) = dialog {
            assertion["scope"] = json!({"dialog":dialog});
            done["scope"] = json!({"dialog":dialog});
        }
        let mut wire = job(&http.url(), assertion);
        wire["steps"][0]["done_when"] = json!([done]);
        wire["values"] =
            json!({"marker":{"value":text,"description":"classified amount","secret":true}});
        if let Some(dialog) = dialog {
            wire["values"]["dialog"] =
                json!({"value":dialog,"description":"classified scope","secret":true});
        }
        let (_, result) = run(&temp, &wire);
        assert_eq!(result["state"], state, "{kind}: {result}");
        terminal_read_only(&result);
        let verification = artifact(&result, "verification");
        assert_eq!(
            verification["expectations"][0]["assertion_checks"][0]["text"]["matched_channel"],
            channel
        );
        let manifest: Value = serde_json::from_slice(
            &fs::read(result["evidence"]["manifest"].as_str().unwrap()).unwrap(),
        )
        .unwrap();
        for sensitive in std::iter::once(text).chain(dialog) {
            assert!(!result.to_string().contains(sensitive));
            assert_no_classified_state(&temp.path().join("state"), sensitive);
            for artifact in manifest["artifacts"].as_array().unwrap() {
                let bytes = fs::read(artifact["path"].as_str().unwrap()).unwrap();
                assert!(
                    !bytes
                        .windows(sensitive.len())
                        .any(|chunk| chunk == sensitive.as_bytes()),
                    "classified text leaked in {}",
                    artifact["role"]
                );
            }
        }
    }
}

#[test]
#[ignore = "requires the local Chromium executable"]
fn text_cli_dialog_default_retains_inert_text_but_opt_in_uses_strict_paint() {
    let http = HttpFixture::with_body(TEXT_FIXTURE);
    for (text, include, state, channel) in [
        ("$21.00", false, "passed", json!("dialog_text")),
        ("$21.00", true, "failed", Value::Null),
        ("$20.00", true, "passed", json!("painted_aria_hidden")),
        ("$23.00", true, "failed", Value::Null),
    ] {
        let temp = hosted_temp();
        let assertion = json!({"text_visible":text,"scope":{"dialog":"month DETAILS"},"include_aria_hidden":include});
        let (_, result) = run(&temp, &job(&http.url(), assertion));
        assert_eq!(result["state"], state, "{result}");
        terminal_read_only(&result);
        assert_eq!(
            artifact(&result, "verification")["expectations"][0]["assertion_checks"][0]["text"]["matched_channel"],
            channel
        );
    }
}

#[test]
#[ignore = "requires the local Chromium executable"]
fn text_cli_unverifiable_masks_block_with_redacted_observations_and_no_png() {
    for (body, secret) in [
        (
            "<dialog id='d' aria-label='Details'><div id='bare' aria-hidden='true'></div></dialog><script>bare.attachShadow({mode:'open'}).textContent='bare-shadow-secret';d.showModal()</script>",
            "bare-shadow-secret",
        ),
        (
            r#"<iframe aria-hidden='true' style='width:300px;height:100px;border:0;transform:scale(2);transform-origin:0 0' srcdoc="<div id='bare' aria-hidden='true'></div><script>bare.attachShadow({mode:'open'}).textContent='bare-shadow-secret'</script>"></iframe>"#,
            "bare-shadow-secret",
        ),
        (
            "<style>html{zoom:2}</style><div id='bare' aria-hidden='true'></div><script>bare.attachShadow({mode:'open'}).textContent='bare-shadow-secret'</script>",
            "bare-shadow-secret",
        ),
        (
            "<div id='bare' aria-hidden='true'></div><script>bare.attachShadow({mode:'open'}).innerHTML='First<span>Second</span>'</script>",
            "First\nSecond",
        ),
        (
            "<style>@keyframes slide{from{transform:translateX(0)}to{transform:translateX(500px)}}#bare{animation:slide .1s linear infinite alternate}</style><div id='bare' aria-hidden='true'></div><script>bare.attachShadow({mode:'open'}).textContent='bare-shadow-secret'</script>",
            "bare-shadow-secret",
        ),
        (
            "<div id='bare' aria-hidden='true' style='text-shadow:0 40px black'></div><script>bare.attachShadow({mode:'open'}).textContent='bare-shadow-secret'</script>",
            "bare-shadow-secret",
        ),
        (
            "<div id='bare' aria-hidden='true' style='filter:drop-shadow(0 40px 0 black)'></div><script>bare.attachShadow({mode:'open'}).textContent='bare-shadow-secret'</script>",
            "bare-shadow-secret",
        ),
        (
            "<style>@keyframes grow{from{width:0}to{width:500px}}#row{display:flex}#sibling{flex:none;animation:grow .1s linear infinite alternate}</style><div id='row'><div id='sibling'>Spacer</div><div id='bare' aria-hidden='true'></div></div><script>bare.attachShadow({mode:'open'}).textContent='bare-shadow-secret'</script>",
            "bare-shadow-secret",
        ),
    ] {
        let http = HttpFixture::with_body(&format!("<!doctype html><p>Ready</p>{body}"));
        let temp = hosted_temp();
        let assertion = json!({"text_visible":secret,"include_aria_hidden":true});
        let mut wire = job(&http.url(), assertion.clone());
        wire["steps"][0]["done_when"] = json!([assertion]);
        wire["values"] = json!({"marker":{"value":secret,"description":"classified painted text","secret":true}});
        let (_, result) = run(&temp, &wire);
        assert_eq!(result["state"], "blocked", "{body}: {result}");
        assert_eq!(result["reason"]["code"], "redaction_unverifiable");
        assert_eq!(result["verdict"]["overall"], "unresolved");
        assert_eq!(result["verdict"]["steps"][0]["result"], "not_run");
        viewport::assert_no_step_input(&result);
        assert_eq!(result["verdict"]["caller_assisted"], false);
        assert_eq!(result["cleanup"]["browser"], "closed");
        assert_eq!(result["cleanup"]["profile"], "removed");
        assert_complete_artifacts(&result);
        assert_no_classified_state(&temp.path().join("state"), secret);
        let manifest: Value = serde_json::from_slice(
            &fs::read(result["evidence"]["manifest"].as_str().unwrap()).unwrap(),
        )
        .unwrap();
        let mut observations = 0;
        let mut retained_text_check = false;
        for artifact in manifest["artifacts"].as_array().unwrap() {
            let path = Path::new(artifact["path"].as_str().unwrap());
            assert_ne!(
                path.extension().and_then(|extension| extension.to_str()),
                Some("png")
            );
            let bytes = fs::read(path).unwrap();
            if artifact["role"] == "trace" {
                retained_text_check = String::from_utf8(bytes.clone())
                    .unwrap()
                    .lines()
                    .map(|line| serde_json::from_str::<Value>(line).unwrap())
                    .any(|entry| {
                        entry["assertion_checks"][0]["text"]["result"] == "satisfied"
                            && entry["assertion_checks"][0]["text"]["matched_channel"]
                                == "painted_aria_hidden"
                    });
            }
            if path.extension().and_then(|extension| extension.to_str()) == Some("json") {
                let exported: Value = serde_json::from_slice(&bytes).unwrap();
                assert!(!exported.to_string().contains(secret));
                if artifact["role"] == "observation" {
                    observations += 1;
                    assert_eq!(exported["screenshot"]["withheld"], "redaction_unverifiable");
                    assert!(
                        exported["painted_text"]["viewport"]["painted_aria_hidden"]
                            .as_str()
                            .unwrap()
                            .contains("<masked:")
                    );
                }
            }
        }
        assert!(observations >= 1);
        assert!(retained_text_check);
    }
}
