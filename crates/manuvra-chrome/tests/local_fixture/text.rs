use super::*;
use serde_json::{Value, json};

const TEXT_FIXTURE: &str = include_str!("../../../../tests/browser/text.html");

fn snapshot(browser: &OwnedBrowser) -> Value {
    let observation = browser.observe().unwrap();
    let mut wire = serde_json::to_value(&observation).unwrap();
    wire["colors_complete"] = json!(observation.colors_complete);
    wire
}

#[test]
#[ignore = "requires the local Chromium executable"]
fn painted_text_inventory_follows_composed_ancestors_and_excludes_unpainted_content() {
    let _serial = REAL_BROWSER.lock().unwrap();
    let server = FixtureServer::with_body(TEXT_FIXTURE);
    let mut browser = launch_headless();
    browser.navigate(&server.url()).unwrap();
    let snapshot = snapshot(&browser);
    let inventory = &snapshot["painted_text"]["viewport"];
    assert_eq!(inventory["complete"], true, "{snapshot}");
    let hidden = inventory["painted_aria_hidden"]
        .as_str()
        .expect("painted aria-hidden inventory");
    for amount in [
        "$0.00", "$9.00", "$10.00", "$11.00", "$12.00", "$13.00", "$20.00", "$23.00", "$24.00",
    ] {
        assert!(
            hidden.lines().any(|line| line == amount),
            "missing {amount}: {snapshot}"
        );
    }
    for amount in [
        "$3.00", "$4.00", "$5.00", "$6.00", "$7.00", "$8.00", "$14.00", "$15.00", "$21.00",
        "$30.00",
    ] {
        assert!(
            !hidden.lines().any(|line| line == amount),
            "ineligible {amount}: {snapshot}"
        );
    }
    assert!(
        inventory["accessible"]
            .as_str()
            .unwrap()
            .contains("Accessible $1.00")
    );
    assert_eq!(
        snapshot["painted_text"]["dialogs"]["Month details"]["painted_aria_hidden"],
        "$20.00\n$24.00"
    );
    assert!(
        !snapshot["painted_text"]["dialogs"]["Month details"]["accessible"]
            .as_str()
            .unwrap()
            .contains("$21.00")
    );
    assert!(
        snapshot["dialog_texts"]["Month details"]
            .as_str()
            .unwrap()
            .contains("$21.00"),
        "legacy innerText retained"
    );
    for name in ["Hidden control", "Shadow hidden control"] {
        assert!(
            !snapshot["elements"]
                .as_array()
                .unwrap()
                .iter()
                .any(|e| e["name"] == name)
        );
    }
    assert!(hidden.contains("Amount spaced"));
    browser.close().unwrap();
}

#[test]
#[ignore = "requires the local Chromium executable"]
fn unsupported_painted_geometry_is_incomplete_without_changing_legacy_coverage() {
    let _serial = REAL_BROWSER.lock().unwrap();
    let server = FixtureServer::with_body(
        "<!doctype html><p>Ready</p><p aria-hidden='true' style='clip-path:circle(10px)'>Unverifiable amount</p>",
    );
    let mut browser = launch_headless();
    browser.navigate(&server.url()).unwrap();
    let snapshot = snapshot(&browser);
    assert_eq!(snapshot["painted_text"]["viewport"]["complete"], false);
    assert!(
        !snapshot["painted_text"]["viewport"]["painted_aria_hidden"]
            .as_str()
            .unwrap()
            .contains("Unverifiable amount")
    );
    assert_eq!(snapshot["coverage"]["viewport_complete"], true);
    assert_eq!(snapshot["coverage"]["gaps"], json!([]));
    browser.close().unwrap();
}

#[test]
#[ignore = "requires the local Chromium executable"]
fn painted_text_is_bounded_independently_of_the_color_owner_limit() {
    let _serial = REAL_BROWSER.lock().unwrap();
    let cells = (0..520)
        .map(|i| format!("<span aria-hidden='true'>a{i}</span>"))
        .collect::<String>();
    let server = FixtureServer::with_body(&format!(
        "<!doctype html><main style='font:1px system-ui;display:grid;grid-template-columns:repeat(30,15px)'>{cells}</main>"
    ));
    let mut browser = launch_headless();
    browser.navigate(&server.url()).unwrap();
    let many = snapshot(&browser);
    assert_eq!(many["painted_text"]["viewport"]["complete"], true);
    assert!(
        many["painted_text"]["viewport"]["painted_aria_hidden"]
            .as_str()
            .unwrap()
            .contains("a519")
    );
    assert_eq!(many["colors_complete"], false);
    browser.close().unwrap();
    let large = "x".repeat(8001);
    let server = FixtureServer::with_body(&format!(
        "<!doctype html><p>Ready</p><dialog open aria-label='Details' style='position:static'><span aria-hidden='true' style='font:1px system-ui'>{large}</span></dialog>"
    ));
    let mut browser = launch_headless();
    browser.navigate(&server.url()).unwrap();
    let limited = snapshot(&browser);
    for inventory in [
        &limited["painted_text"]["viewport"],
        &limited["painted_text"]["dialogs"]["Details"],
    ] {
        assert_eq!(inventory["complete"], false);
        assert_eq!(
            inventory["painted_aria_hidden"].as_str().unwrap().len(),
            8000
        );
    }
    browser.close().unwrap();
}

#[test]
#[ignore = "requires the local Chromium executable"]
fn painted_text_truncation_keeps_unicode_valid_without_breaking_default_observation() {
    let _serial = REAL_BROWSER.lock().unwrap();
    let prefix = "x".repeat(7999);
    let server = FixtureServer::with_body(&format!(
        "<!doctype html><meta charset='utf-8'><p>Ready</p><span inert>I</span><span aria-hidden='true' style='font:1px system-ui'>{prefix}🔥</span>"
    ));
    let mut browser = launch_headless();
    browser.navigate(&server.url()).unwrap();
    let observed = snapshot(&browser);
    assert_eq!(observed["visible_text"], "Ready");
    assert_eq!(observed["painted_text"]["viewport"]["complete"], false);
    assert_eq!(
        observed["painted_text"]["viewport"]["painted_aria_hidden"],
        prefix
    );
    browser.close().unwrap();
}

#[test]
#[ignore = "requires the local Chromium executable"]
fn bare_shadow_and_assigned_text_follow_their_painted_parent() {
    let _serial = REAL_BROWSER.lock().unwrap();
    let server = FixtureServer::with_body(
        "<!doctype html><p>Ready</p><div id='bare' aria-hidden='true'></div><div id='assigned'>Assigned amount</div><div id='inert'>Inert assigned amount</div><script>
        bare.attachShadow({mode:'open'}).textContent='Bare shadow amount';
        assigned.attachShadow({mode:'open'}).innerHTML='<div aria-hidden=true><slot></slot></div>';
        inert.attachShadow({mode:'open'}).innerHTML='<div inert><slot></slot></div>';
        </script>",
    );
    let mut browser = launch_headless();
    browser.navigate(&server.url()).unwrap();
    let observed = snapshot(&browser);
    let viewport = &observed["painted_text"]["viewport"];
    assert_eq!(viewport["complete"], true);
    let accessible = viewport["accessible"].as_str().unwrap();
    let hidden = viewport["painted_aria_hidden"].as_str().unwrap();
    assert!(hidden.contains("Bare shadow amount"));
    assert!(hidden.contains("Assigned amount"));
    assert!(!accessible.contains("Assigned amount"));
    assert!(!accessible.contains("Inert assigned amount"));
    assert!(!hidden.contains("Inert assigned amount"));
    browser.close().unwrap();
}
