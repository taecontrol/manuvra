use super::*;
use serde_json::{Value, json};

const COLOR_FIXTURE: &str = include_str!("../../../../tests/browser/color.html");

fn observation(browser: &OwnedBrowser) -> Value {
    serde_json::to_value(browser.observe().unwrap()).unwrap()
}

fn text<'a>(snapshot: &'a Value, wanted: &str) -> Vec<&'a Value> {
    snapshot["colors"]
        .as_array()
        .expect("snapshot must expose color owners")
        .iter()
        .filter(|entry| entry["text"] == wanted)
        .collect()
}

#[test]
#[ignore = "requires the local Chromium executable"]
fn color_snapshot_preserves_straight_channels_and_exact_text_owners() {
    let _serial = REAL_BROWSER.lock().unwrap();
    let server = FixtureServer::with_body(COLOR_FIXTURE);
    let mut browser = launch_headless();
    browser.navigate(&server.url()).unwrap();
    let snapshot = observation(&browser);
    assert_eq!(snapshot["colors_complete"], true);
    for (label, expected) in [
        ("-$12.34", json!([185, 28, 28, 255])),
        ("$12.34", json!([31, 41, 55, 255])),
        ("Inherited amount", json!([185, 28, 28, 255])),
        ("HSL amount", json!([185, 28, 28, 255])),
        ("Modern amount", json!([179, 36, 27, 255])),
        ("Low alpha amount", json!([1, 2, 3, 26])),
        ("Clamped amount", json!([0, 200, 0, 26])),
        ("Shadow amount", json!([185, 28, 28, 255])),
        ("Slotted amount", json!([185, 28, 28, 255])),
        ("Frame amount", json!([185, 28, 28, 255])),
    ] {
        let matches = text(&snapshot, label);
        assert_eq!(matches.len(), 1, "{label}: {snapshot}");
        assert_eq!(matches[0]["color"]["rgba"], expected, "{label}");
        assert!(
            matches[0]["color"]["raw"]
                .as_str()
                .is_some_and(|raw| !raw.is_empty())
        );
    }
    assert_eq!(text(&snapshot, "$0.00").len(), 3);
    for label in [
        "Hidden amount",
        "Opacity zero amount",
        "Inert amount",
        "Offscreen amount",
        "Clipped amount",
        "-$42.00",
        "BeforeMiddleAfter",
    ] {
        assert!(
            text(&snapshot, label).is_empty(),
            "ineligible/composite text {label}"
        );
    }
    let decorated = text(&snapshot, "-$9.00");
    assert_eq!(decorated.len(), 1);
    assert_eq!(decorated[0]["channel"], "painted_aria_hidden");
    assert_eq!(decorated[0]["color"]["rgba"], json!([185, 28, 28, 255]));
    let named = snapshot["colors"]
        .as_array()
        .unwrap()
        .iter()
        .find(|entry| entry["name"] == "Current month")
        .unwrap();
    assert_eq!(named["color"]["rgba"], json!([31, 41, 55, 255]));
    assert_eq!(named["role"], "button");
    browser.close().unwrap();
}

#[test]
#[ignore = "requires the local Chromium executable"]
fn color_normalization_is_read_only_during_fenced_capture() {
    let _serial = REAL_BROWSER.lock().unwrap();
    let server = FixtureServer::with_body(COLOR_FIXTURE);
    let mut browser = launch_headless();
    browser.navigate(&server.url()).unwrap();
    let captured = browser
        .capture()
        .expect("color normalization must not mutate the target and invalidate every fence");
    let snapshot = serde_json::to_value(&captured.observation).unwrap();
    assert_eq!(
        text(&snapshot, "Low alpha amount")[0]["color"]["rgba"],
        json!([1, 2, 3, 26])
    );
    assert!(!captured.screenshot.bytes.is_empty());
    browser.close().unwrap();
}

#[test]
#[ignore = "requires the local Chromium executable"]
fn color_paint_eligibility_crosses_embedding_frames_and_boxless_text_owners() {
    let _serial = REAL_BROWSER.lock().unwrap();
    let server = FixtureServer::with_body(include_str!(
        "../../../../tests/browser/color-eligibility.html"
    ));
    let mut browser = launch_headless();
    browser.navigate(&server.url()).unwrap();
    let snapshot = observation(&browser);
    for label in [
        "Opacity frame",
        "Hidden frame",
        "Inert frame",
        "Clipped frame",
        "Clip path amount",
        "Legacy clip amount",
    ] {
        assert!(
            text(&snapshot, label).is_empty(),
            "unpainted {label}: {snapshot}"
        );
    }
    for name in [
        "Opacity choice",
        "Hidden choice",
        "Inert choice",
        "Clipped choice",
    ] {
        assert!(
            !snapshot["colors"]
                .as_array()
                .unwrap()
                .iter()
                .any(|entry| entry["name"] == name),
            "unpainted control {name}"
        );
    }
    assert_eq!(
        text(&snapshot, "Contents amount")[0]["color"]["rgba"],
        json!([185, 28, 28, 255])
    );
    assert_eq!(
        text(&snapshot, "Painted hidden frame")[0]["channel"],
        "painted_aria_hidden"
    );
    assert_eq!(
        text(&snapshot, "Unverifiable clip amount")[0]["paint_complete"],
        false
    );
    assert_eq!(
        text(&snapshot, "Partial clip amount")[0]["color"]["rgba"],
        json!([185, 28, 28, 255])
    );
    assert!(
        !snapshot["visible_text"]
            .as_str()
            .unwrap()
            .contains("Contents amount"),
        "existing TextVisible eligibility remains unchanged"
    );
    browser.close().unwrap();
}

#[test]
#[ignore = "requires the local Chromium executable"]
fn color_inventory_and_native_normalization_fail_closed() {
    let _serial = REAL_BROWSER.lock().unwrap();
    let cells = "<span>A</span>".repeat(513);
    let crowded = format!(
        "<!doctype html><main style='display:grid;grid-template-columns:repeat(30,10px);font:1px system-ui'>{cells}</main>"
    );
    let server = FixtureServer::with_body(&crowded);
    let mut browser = launch_headless();
    browser.navigate(&server.url()).unwrap();
    let snapshot = observation(&browser);
    assert!(
        !snapshot
            .get("colors_complete")
            .and_then(Value::as_bool)
            .unwrap_or(false)
    );
    assert_eq!(snapshot["colors"].as_array().unwrap().len(), 512);
    browser.close().unwrap();

    let scopes = "<fieldset aria-label='Other' style='height:1px;min-width:0;margin:0;padding:0;border:0'></fieldset>".repeat(513);
    let crowded = format!(
        "<!doctype html><main style='display:grid;grid-template-columns:repeat(30,10px);font:1px system-ui'>{scopes}</main>"
    );
    let server = FixtureServer::with_body(&crowded);
    let mut browser = launch_headless();
    browser.navigate(&server.url()).unwrap();
    let snapshot = observation(&browser);
    assert!(
        !snapshot
            .get("colors_complete")
            .and_then(Value::as_bool)
            .unwrap_or(false)
    );
    assert_eq!(snapshot["color_scopes"].as_array().unwrap().len(), 512);
    browser.close().unwrap();

    let server = FixtureServer::with_body(
        "<!doctype html><p style='color:#b91c1c'>Amount</p><script>window.OffscreenCanvas=undefined</script>",
    );
    let mut browser = launch_headless();
    browser.navigate(&server.url()).unwrap();
    let snapshot = observation(&browser);
    assert_eq!(
        text(&snapshot, "Amount")[0]["color"]["raw"],
        "rgb(185, 28, 28)"
    );
    assert_eq!(text(&snapshot, "Amount")[0]["color"]["rgba"], Value::Null);
    browser.close().unwrap();
}

#[test]
#[ignore = "requires the local Chromium executable"]
fn color_clip_geometry_preserves_unverifiable_owners_and_excludes_clipped_scopes() {
    let _serial = REAL_BROWSER.lock().unwrap();
    let server = FixtureServer::with_body(include_str!(
        "../../../../tests/browser/color-eligibility.html"
    ));
    let mut browser = launch_headless();
    browser.navigate(&server.url()).unwrap();
    let snapshot = observation(&browser);
    assert_eq!(
        text(&snapshot, "Inline clip amount").len(),
        1,
        "unverifiable paint must retain its owner"
    );
    assert_eq!(
        text(&snapshot, "Inline clip amount")[0]["paint_complete"],
        false
    );
    assert_eq!(
        text(&snapshot, "Boxless clip amount")[0]["paint_complete"],
        true
    );
    assert_eq!(
        text(&snapshot, "Boxless clip amount")[0]["color"]["rgba"],
        json!([185, 28, 28, 255])
    );
    assert_eq!(
        snapshot["color_scopes"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|scope| scope["name"] == "Filtered checking")
            .count(),
        1
    );
    browser.close().unwrap();
}

#[test]
#[ignore = "requires the local Chromium executable"]
fn color_paint_checks_contents_frames_percentage_and_affine_clips() {
    let _serial = REAL_BROWSER.lock().unwrap();
    for (body, owners, scopes) in [
        (
            r#"<p><span style="display:contents;visibility:hidden;color:#b91c1c">Amount</span></p>"#,
            0,
            0,
        ),
        (
            r#"<p style="opacity:0"><span style="display:contents;color:#b91c1c">Amount</span></p>"#,
            0,
            0,
        ),
        (
            r#"<iframe style="width:300px;height:100px;border:0;clip-path:inset(100%)" srcdoc="<p style='color:#b91c1c'>Amount</p>"></iframe>"#,
            0,
            0,
        ),
        (
            r#"<div style="position:relative;width:600px;height:400px;clip-path:inset(100%)"><p style="position:absolute;left:300px;top:150px;margin:0;color:#b91c1c">Amount</p></div>"#,
            0,
            0,
        ),
        (
            r#"<div style="zoom:2;width:200px;clip-path:inset(0px 0px 0px 100px)"><p style="margin:0;padding-left:120px;color:#b91c1c">Amount</p></div>"#,
            1,
            0,
        ),
        (
            r#"<fieldset aria-label="Checking"><p style="color:#b91c1c">Amount</p></fieldset><iframe style="opacity:0;width:400px;height:100px;border:0" srcdoc="<fieldset aria-label=Checking><p>Other</p></fieldset>"></iframe>"#,
            1,
            1,
        ),
    ] {
        let fixture = format!(
            "<!doctype html><body style='margin:40px;font:16px system-ui'><h1>Ready</h1>{body}</body>"
        );
        let server = FixtureServer::with_body(&fixture);
        let mut browser = launch_headless();
        browser.navigate(&server.url()).unwrap();
        let snapshot = observation(&browser);
        assert_eq!(snapshot["colors_complete"], true, "{body}: {snapshot}");
        let matches = text(&snapshot, "Amount");
        assert_eq!(matches.len(), owners, "{body}: {snapshot}");
        if let Some(owner) = matches.first() {
            assert_eq!(owner["paint_complete"], true);
            assert_eq!(owner["color"]["rgba"], json!([185, 28, 28, 255]));
        }
        assert_eq!(
            snapshot["color_scopes"]
                .as_array()
                .into_iter()
                .flatten()
                .filter(|scope| scope["name"] == "Checking")
                .count(),
            scopes,
            "{body}: {snapshot}"
        );
        browser.close().unwrap();
    }
}
