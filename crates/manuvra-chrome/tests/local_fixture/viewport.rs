use super::*;
use serde_json::{Value, json};

const VIEWPORT_FIXTURE: &str = include_str!("../../../../tests/browser/viewport.html");

fn browser(width: u16, height: u16) -> OwnedBrowser {
    OwnedBrowser::launch(BrowserConfig {
        explicit_binary: None,
        headless: true,
        width,
        height,
        inherit_process_group: false,
    })
    .unwrap()
}

fn facts(observed: &Observation) -> Value {
    let value = &observed
        .elements
        .iter()
        .find(|element| element.name == "Viewport facts")
        .expect("independent DOM measurements must be visible")
        .value;
    serde_json::from_str(value).unwrap()
}

#[test]
#[ignore = "requires the local Chromium executable"]
fn headless_viewport_preserves_requested_width_when_document_overflow_changes() {
    let _serial = REAL_BROWSER
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let server = FixtureServer::with_body(VIEWPORT_FIXTURE);
    for (width, height) in [(390, 844), (320, 568), (1280, 800), (1120, 780)] {
        let mut browser = browser(width, height);
        browser
            .navigate(&format!("{}?mode=short", server.url()))
            .unwrap();
        let observed = browser.observe().unwrap();
        assert_eq!(facts(&observed)["client_width"], width);
        assert_eq!(observed.viewport.scroll_y, 0.0);
        browser
            .perform(
                prepared(
                    &observed,
                    "Extend document",
                    PreparedOperation::Click,
                    None,
                    None,
                    1,
                ),
                &InputCancellation::default(),
            )
            .unwrap();
        let observed = browser.observe().unwrap();
        let actual = facts(&observed);
        assert_eq!(actual["client_width"], width, "{width}: {actual}");
        assert_eq!(actual["inner_width"], width);
        assert_eq!(actual["scale"], 1);
        assert_eq!(actual["device_scale"], 1);
        assert_eq!(actual["hover"], true);
        assert_eq!(actual["fine_pointer"], true);
        assert_eq!(actual["touch_points"], 0);
        if width == 390 {
            assert_eq!(actual["wrapped"], false);
            let capture = browser.capture().unwrap();
            assert_eq!(
                (capture.screenshot.width, capture.screenshot.height),
                (390, 844)
            );
            if let Some(directory) = std::env::var_os("MANUVRA_FIXTURE_EVIDENCE") {
                let directory = PathBuf::from(directory);
                std::fs::create_dir_all(&directory).unwrap();
                std::fs::write(
                    directory.join("viewport-390x844.png"),
                    &capture.screenshot.bytes,
                )
                .unwrap();
                std::fs::write(
                    directory.join("viewport-390x844.json"),
                    serde_json::to_vec_pretty(&actual).unwrap(),
                )
                .unwrap();
            }
        }
        browser.close().unwrap();
    }
}

#[test]
#[ignore = "requires the local Chromium executable"]
fn headless_viewport_preserves_page_css_and_desktop_layout_edge_cases() {
    let _serial = REAL_BROWSER
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let server = FixtureServer::with_body(VIEWPORT_FIXTURE);
    let mut browser = browser(390, 844);
    for mode in ["tall", "no-meta", "custom", "stable", "overflow"] {
        browser
            .navigate(&format!("{}?mode={mode}", server.url()))
            .unwrap();
        let observed = browser.observe().unwrap();
        let actual = facts(&observed);
        assert_eq!(actual["client_width"], 390, "{mode}: {actual}");
        assert_eq!(actual["inner_width"], 390, "{mode}: {actual}");
        assert_eq!(actual["scale"], 1);
        if mode == "stable" {
            assert_eq!(actual["gutter"], "stable both-edges");
        } else {
            assert_eq!(actual["wrapped"], false);
            assert_eq!(
                actual["nested_client_width"].as_f64().unwrap(),
                actual["nested_width"].as_f64().unwrap() - 2.0
            );
        }
        if mode == "overflow" {
            assert_eq!(actual["content_width"], 600);
            browser
                .perform(
                    prepared(
                        &observed,
                        "Scroll horizontally",
                        PreparedOperation::Click,
                        None,
                        None,
                        1,
                    ),
                    &InputCancellation::default(),
                )
                .unwrap();
            assert!(
                facts(&browser.observe().unwrap())["scroll_x"]
                    .as_f64()
                    .unwrap()
                    > 0.0
            );
        }
    }
    browser.close().unwrap();
}

#[test]
#[ignore = "requires the local Chromium executable"]
fn hidden_viewport_scrollbars_keep_document_and_nested_wheel_readbacks_truthful() {
    let _serial = REAL_BROWSER
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let server = FixtureServer::with_body(VIEWPORT_FIXTURE);
    let mut browser = browser(390, 844);
    browser.navigate(&server.url()).unwrap();
    let observed = browser.observe().unwrap();
    assert_eq!(observed.scroll_regions.len(), 1);
    let fact = browser
        .perform(
            prepared_scroll(&observed, false, 1),
            &InputCancellation::default(),
        )
        .unwrap();
    assert!(fact.scroll_readback[0].after > 0.0);
    assert_eq!(fact.scroll_readback.last().unwrap().after, 0.0);
    let after = browser.observe().unwrap();
    assert!(facts(&after)["nested_scroll_top"].as_f64().unwrap() > 0.0);
    assert_eq!(after.viewport.scroll_y, 0.0);
    let mut document = prepared_scroll(&after, false, 2);
    document.scroll_region = None;
    browser
        .perform(document, &InputCancellation::default())
        .unwrap();
    let after = browser.observe().unwrap();
    assert!(after.viewport.scroll_y > 0.0);
    assert_eq!(facts(&after)["client_width"], 390);
    browser.close().unwrap();
}

#[test]
#[ignore = "requires the local Chromium executable"]
fn viewport_provenance_keeps_the_first_target_measurement_across_navigation() {
    let _serial = REAL_BROWSER
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let server = FixtureServer::with_body(VIEWPORT_FIXTURE);
    let mut browser = browser(390, 844);
    browser.navigate(&server.url()).unwrap();
    assert_eq!(
        serde_json::to_value(browser.provenance()).unwrap()["viewport"]["initial_client_width"],
        390
    );
    browser
        .navigate(&format!("{}?mode=mismatch", server.url()))
        .unwrap();
    assert_eq!(facts(&browser.observe().unwrap())["client_width"], 375);
    assert_eq!(
        serde_json::to_value(browser.provenance()).unwrap()["viewport"],
        json!({"width":390,"height":844,"initial_client_width":390})
    );
    browser.close().unwrap();
}
