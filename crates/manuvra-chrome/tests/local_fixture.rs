#![cfg(any(target_os = "linux", target_os = "macos"))]

use manuvra_chrome::{
    BrowserConfig, InputCancellation, Observation, OwnedBrowser, PerformError, PreparedInput,
    PreparedOperation,
};
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
#[cfg(target_os = "macos")]
use std::path::PathBuf;
#[cfg(target_os = "macos")]
use std::process::Command;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
#[cfg(target_os = "macos")]
use std::time::{Duration, Instant};

const FIXTURE: &str = include_str!("../../../tests/fixtures/browser-adversarial.html");
const INPUT_FIXTURE: &str = include_str!("../../../tests/fixtures/browser-input-strategies.html");
static REAL_BROWSER: Mutex<()> = Mutex::new(());

#[cfg(target_os = "macos")]
struct BrowserLifecycle {
    process_group: i32,
    profile: PathBuf,
}

#[cfg(target_os = "macos")]
impl BrowserLifecycle {
    fn observe(browser: &OwnedBrowser) -> Self {
        let path = &browser.provenance().browser_path;
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            let rows = process_rows();
            if let Some((pid, process_group, command)) = rows.iter().find(|(_, _, command)| {
                command.contains(path) && command.contains("--remote-debugging-port=0")
            }) {
                let profile = command
                    .split_whitespace()
                    .find_map(|argument| argument.strip_prefix("--user-data-dir="))
                    .map(PathBuf::from)
                    .expect("owned Chrome command has an isolated profile");
                assert_eq!(pid, process_group, "standalone Chrome must lead its group");
                assert!(profile.is_dir());
                let owned = format!("--user-data-dir={}", profile.display());
                let observed: Vec<_> = rows
                    .iter()
                    .filter(|(_, _, command)| command.contains(&owned))
                    .collect();
                if observed.len() > 1 {
                    assert!(observed.iter().all(|(_, group, _)| group == process_group));
                    eprintln!(
                        "owned Chrome group observed: pgid={process_group} members={} profile={}",
                        observed.len(),
                        profile.display()
                    );
                    return Self {
                        process_group: *process_group,
                        profile,
                    };
                }
            }
            assert!(
                Instant::now() < deadline,
                "Chrome helper group was not observable"
            );
            thread::sleep(Duration::from_millis(20));
        }
    }

    fn assert_cleaned(self) {
        assert!(
            !self.profile.exists(),
            "owned Chrome profile survived close"
        );
        assert_eq!(unsafe { libc::kill(-self.process_group, 0) }, -1);
        assert_eq!(
            std::io::Error::last_os_error().raw_os_error(),
            Some(libc::ESRCH),
            "owned Chrome group survived close"
        );
        eprintln!(
            "owned Chrome cleanup observed: pgid={} removed profile={}",
            self.process_group,
            self.profile.display()
        );
    }
}

#[cfg(target_os = "macos")]
fn process_rows() -> Vec<(i32, i32, String)> {
    let output = Command::new("ps")
        .args(["-axo", "pid=,pgid=,command="])
        .output()
        .expect("inspect Chrome process group");
    assert!(output.status.success());
    String::from_utf8(output.stdout)
        .unwrap()
        .lines()
        .filter_map(|line| {
            let mut fields = line.split_whitespace();
            let pid = fields.next()?.parse().ok()?;
            let process_group = fields.next()?.parse().ok()?;
            Some((pid, process_group, fields.collect::<Vec<_>>().join(" ")))
        })
        .collect()
}

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
    let _serial = REAL_BROWSER.lock().unwrap();
    let server = FixtureServer::start();
    let mut browser = OwnedBrowser::launch(BrowserConfig {
        explicit_binary: None,
        headless: true,
        width: 1120,
        height: 780,
        inherit_process_group: false,
    })
    .unwrap();
    assert_ne!(browser.provenance().browser_version, "unknown");
    #[cfg(target_os = "macos")]
    let lifecycle = BrowserLifecycle::observe(&browser);
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
    eprintln!("real Chrome CDP snapshot, screenshot, and masking fixture completed");
    browser.close().unwrap();
    #[cfg(target_os = "macos")]
    lifecycle.assert_cleaned();
}

#[test]
#[ignore = "requires the local Chromium executable"]
fn production_input_strategies_cover_native_and_bounded_fallback_paths() {
    let _serial = REAL_BROWSER.lock().unwrap();
    let server = FixtureServer::with_body(INPUT_FIXTURE);
    let mut browser = OwnedBrowser::launch(BrowserConfig {
        explicit_binary: None,
        headless: true,
        width: 1120,
        height: 780,
        inherit_process_group: false,
    })
    .unwrap();
    assert_ne!(browser.provenance().browser_version, "unknown");
    #[cfg(target_os = "macos")]
    let lifecycle = BrowserLifecycle::observe(&browser);
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
    eprintln!("real Chrome CDP input and readback fixture completed");
    browser.close().unwrap();
    #[cfg(target_os = "macos")]
    lifecycle.assert_cleaned();
}
