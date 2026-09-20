#![cfg(target_os = "linux")]

use manuvra_chrome::{
    BrowserConfig, InputCancellation, Observation, OwnedBrowser, PerformError, PreparedInput,
    PreparedOperation,
};
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;

const FIXTURE: &str = include_str!("../../../tests/fixtures/browser-adversarial.html");
const INPUT_FIXTURE: &str = include_str!("../../../tests/fixtures/browser-input-strategies.html");

struct FixtureServer {
    address: std::net::SocketAddr,
    stop: Arc<AtomicBool>,
    worker: Option<thread::JoinHandle<()>>,
}

impl FixtureServer {
    fn start() -> Self {
        Self::with_body(FIXTURE)
    }

    fn with_body(body: &'static str) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let address = listener.local_addr().unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let worker_stop = Arc::clone(&stop);
        let worker = thread::spawn(move || {
            while !worker_stop.load(Ordering::Relaxed) {
                match listener.accept() {
                    Ok((mut stream, _)) => serve_fixture(&mut stream, body),
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(std::time::Duration::from_millis(5));
                    }
                    Err(error) => panic!("fixture server failed: {error}"),
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
        format!("http://{}/", self.address)
    }
}

impl Drop for FixtureServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Ok(mut stream) = TcpStream::connect(self.address) {
            let _ = stream.write_all(b"GET /shutdown HTTP/1.1\r\n\r\n");
        }
        self.worker.take().unwrap().join().unwrap();
    }
}

fn serve_fixture(stream: &mut TcpStream, body: &str) {
    let mut request = [0_u8; 2048];
    let _ = stream.read(&mut request);
    let response = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        body.len(),
        body
    );
    stream.write_all(response.as_bytes()).unwrap();
}

fn prepared(
    observation: &Observation,
    name: &str,
    operation: PreparedOperation,
    text: Option<&str>,
    option_node_id: Option<u64>,
    sequence: u64,
) -> PreparedInput {
    let element = observation
        .elements
        .iter()
        .find(|element| element.name == name)
        .unwrap_or_else(|| panic!("missing observed target {name}"));
    PreparedInput {
        document_id: observation.document_id.clone(),
        node_id: element.node_id,
        operation,
        text: text.map(str::to_owned),
        previous_text: Some(element.value.clone()),
        option_node_id,
        combobox: element.role == "combobox" && operation == PreparedOperation::TypeText,
        action_sequence: sequence,
    }
}

#[test]
#[ignore = "requires the local Chromium executable"]
fn production_snapshot_and_masking_cover_truncation_split_nodes_and_zero_masks() {
    let server = FixtureServer::start();
    let mut browser = OwnedBrowser::launch(BrowserConfig {
        explicit_binary: None,
        headless: true,
        width: 1120,
        height: 780,
        inherit_process_group: false,
    })
    .unwrap();
    browser.navigate(&server.url()).unwrap();

    let observed = browser.observe().unwrap();
    assert!(!observed.coverage.viewport_complete);
    for gap in [
        "visible_text_truncated",
        "covered_text_truncated",
        "dialog_text_truncated",
    ] {
        assert!(observed.coverage.gaps.iter().any(|actual| actual == gap));
    }
    assert!(observed.visible_text.len() <= 8000);
    assert!(observed.covered_text.len() <= 8000);
    assert!(observed.dialog_texts["Long dialog"].len() <= 8000);

    let plain = browser.capture().unwrap();
    let masked = browser.capture_redacted(&["split-secret".into()]).unwrap();
    assert!(masked.redaction.verifies(1));
    assert_eq!(masked.redaction.matched_values, 1);
    assert!(masked.redaction.mask_count >= 1);
    assert_ne!(plain.screenshot.bytes, masked.screenshot.bytes);

    let absent = browser
        .capture_redacted(&["not-rendered-anywhere".into()])
        .unwrap();
    assert!(absent.redaction.verifies(1));
    assert_eq!(absent.redaction.matched_values, 0);
    assert_eq!(absent.redaction.mask_count, 0);
    browser.close().unwrap();
}

#[test]
#[ignore = "requires the local Chromium executable"]
fn production_input_strategies_cover_native_and_bounded_fallback_paths() {
    let server = FixtureServer::with_body(INPUT_FIXTURE);
    let mut browser = OwnedBrowser::launch(BrowserConfig {
        explicit_binary: None,
        headless: true,
        width: 1120,
        height: 780,
        inherit_process_group: false,
    })
    .unwrap();
    browser.navigate(&server.url()).unwrap();
    let cancellation = InputCancellation::default();
    let mut sequence = 1;

    let observed = browser.observe().unwrap();
    let native = observed
        .elements
        .iter()
        .find(|element| element.name == "Native choice")
        .unwrap();
    let option = native
        .select_options
        .iter()
        .find(|option| option.label == "Wanted")
        .unwrap();
    let other_option = observed
        .elements
        .iter()
        .find(|element| element.name == "Other native choice")
        .unwrap()
        .select_options[0]
        .node_id;
    assert!(matches!(
        browser.perform(
            prepared(
                &observed,
                "Native choice",
                PreparedOperation::Select,
                Some("Wanted"),
                Some(other_option),
                sequence,
            ),
            &cancellation,
        ),
        Err(PerformError::Uncertain(reason)) if reason == "observed select option was unavailable"
    ));
    sequence += 1;
    let fact = browser
        .perform(
            prepared(
                &observed,
                "Native choice",
                PreparedOperation::Select,
                Some("Wanted"),
                Some(option.node_id),
                sequence,
            ),
            &cancellation,
        )
        .unwrap();
    sequence += 1;
    assert_eq!(fact.readback_matches, Some(true));
    assert_eq!(
        fact.suboperations,
        ["select_option", "input_event", "change_event"]
    );
    let observed = browser.observe().unwrap();
    assert_eq!(
        observed
            .elements
            .iter()
            .find(|element| element.name == "Native choice")
            .unwrap()
            .value,
        "wanted-id"
    );
    assert!(observed.visible_text.contains("input change"));

    let fact = browser
        .perform(
            prepared(
                &observed,
                "Search choice",
                PreparedOperation::TypeText,
                Some("Wanted"),
                None,
                sequence,
            ),
            &cancellation,
        )
        .unwrap();
    sequence += 1;
    assert_eq!(fact.readback_matches, Some(true));
    let observed = browser.observe().unwrap();
    assert!(
        observed
            .elements
            .iter()
            .any(|element| { element.role == "option" && element.name == "Wanted option" })
    );
    browser
        .perform(
            prepared(
                &observed,
                "Wanted option",
                PreparedOperation::Click,
                None,
                None,
                sequence,
            ),
            &cancellation,
        )
        .unwrap();
    sequence += 1;
    let observed = browser.observe().unwrap();
    assert!(observed.visible_text.contains("Selected option"));
    assert!(observed.visible_text.contains("0"));

    let fact = browser
        .perform(
            prepared(
                &observed,
                "Keyboard only",
                PreparedOperation::TypeText,
                Some("Wanted"),
                None,
                sequence,
            ),
            &cancellation,
        )
        .unwrap();
    sequence += 1;
    assert_eq!(fact.readback_matches, Some(true));
    assert!(
        fact.suboperations
            .iter()
            .any(|item| item == "dispatch_key_events")
    );

    let fact = browser
        .perform(
            prepared(
                &browser.observe().unwrap(),
                "Garbled",
                PreparedOperation::TypeText,
                Some("Wanted"),
                None,
                sequence,
            ),
            &cancellation,
        )
        .unwrap();
    sequence += 1;
    assert_eq!(fact.readback_matches, Some(false));
    assert!(
        !fact
            .suboperations
            .iter()
            .any(|item| item == "dispatch_key_events")
    );

    let observed = browser.observe().unwrap();
    let fact = browser
        .perform(
            prepared(
                &observed,
                "Date",
                PreparedOperation::SetValue,
                Some("2026-09-20"),
                None,
                sequence,
            ),
            &cancellation,
        )
        .unwrap();
    sequence += 1;
    assert_eq!(fact.readback_matches, Some(true));
    assert!(
        browser
            .observe()
            .unwrap()
            .visible_text
            .contains("input change")
    );

    for target in ["Shadow control", "Slotted control", "Frame control"] {
        let observed = browser.observe().unwrap();
        browser
            .perform(
                prepared(
                    &observed,
                    target,
                    PreparedOperation::Click,
                    None,
                    None,
                    sequence,
                ),
                &cancellation,
            )
            .unwrap();
        sequence += 1;
    }
    let observed = browser.observe().unwrap();
    assert!(observed.visible_text.contains("clicked"));
    assert!(observed.visible_text.contains("Slotted clicked"));
    for gap in ["cross_origin_frame", "closed_shadow_root", "canvas"] {
        assert!(observed.coverage.gaps.iter().any(|actual| actual == gap));
    }
    assert!(
        observed
            .elements
            .iter()
            .any(|element| { element.input_type.as_deref() == Some("file") })
    );

    browser
        .perform(
            prepared(
                &observed,
                "Open popup",
                PreparedOperation::Click,
                None,
                None,
                sequence,
            ),
            &cancellation,
        )
        .unwrap();
    sequence += 1;
    let observed = browser.observe().unwrap();
    assert!(observed.coverage.gaps.iter().any(|gap| gap == "popup"));

    assert!(
        !observed
            .elements
            .iter()
            .any(|element| element.name == "Below viewport")
    );
    let mut observed = observed;
    for _ in 0..4 {
        if observed
            .elements
            .iter()
            .any(|element| element.name == "Below viewport")
        {
            break;
        }
        let scroll = PreparedInput {
            document_id: observed.document_id.clone(),
            node_id: 0,
            operation: PreparedOperation::ScrollDown,
            text: None,
            previous_text: None,
            option_node_id: None,
            combobox: false,
            action_sequence: sequence,
        };
        browser.perform(scroll, &cancellation).unwrap();
        sequence += 1;
        observed = browser.observe().unwrap();
    }
    assert!(
        observed
            .elements
            .iter()
            .any(|element| element.name == "Below viewport")
    );

    let stale = prepared(
        &observed,
        "Below viewport",
        PreparedOperation::Click,
        None,
        None,
        sequence + 1,
    );
    browser.navigate(&server.url()).unwrap();
    assert!(matches!(
        browser.perform(stale, &cancellation),
        Err(PerformError::Rejected(reason)) if reason == "document_changed" || reason == "target_missing"
    ));
    browser.close().unwrap();
}
