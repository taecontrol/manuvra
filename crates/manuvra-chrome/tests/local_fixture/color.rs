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
