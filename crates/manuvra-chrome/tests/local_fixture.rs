#![cfg(any(target_os = "linux", target_os = "macos"))]

use manuvra_chrome::{
    BrowserConfig, BrowserError, Element, InputCancellation, Observation, OwnedBrowser,
    PerformError, PreparedInput, PreparedOperation,
};
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

const FIXTURE: &str = include_str!("../../../tests/browser/adversarial.html");
const INPUT_FIXTURE: &str = include_str!("../../../tests/browser/input-strategies.html");
const HOVER_FIXTURE: &str = include_str!("../../../tests/browser/hover-reveal.html");
const FOCUS_FIXTURE: &str = include_str!("../../../tests/browser/focus.html");
const KEYBOARD_FIXTURE: &str = include_str!("../../../tests/browser/keyboard-focus.html");
const ACTIVATION_FIXTURE: &str = include_str!("../../../tests/browser/keyboard-activation.html");
const WIDGET_FIXTURE: &str = include_str!("../../../tests/browser/keyboard-widgets.html");
const SUBMIT_FIXTURE: &str = include_str!("../../../tests/browser/keyboard-submit.html");
const REVALIDATION_FIXTURE: &str =
    include_str!("../../../tests/browser/keyboard-revalidation.html");
const EDITING_FIXTURE: &str = include_str!("../../../tests/browser/keyboard-editing.html");
static REAL_BROWSER: Mutex<()> = Mutex::new(());

/// The owned browser's process group and profile, observed from outside the crate.
struct BrowserLifecycle {
    process_group: i32,
    profile: PathBuf,
}

impl BrowserLifecycle {
    /// Finds this test process's owned browser by its private profile, whose name
    /// embeds the test process id, so other Chromium instances are never mistaken for it.
    fn observe() -> Self {
        let owned_prefix = format!("manuvra-chromium-{}-", std::process::id());
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            let rows: Vec<_> = process_rows()
                .into_iter()
                .filter_map(|(pid, group, command)| {
                    owned_profile(&command, &owned_prefix).map(|profile| (pid, group, profile))
                })
                .collect();
            let leader = rows.iter().find(|(pid, group, _)| pid == group);
            if let Some((_, process_group, profile)) = leader
                && rows.len() > 1
            {
                assert!(
                    rows.iter()
                        .all(|(_, group, member)| group == process_group && member == profile),
                    "owned Chrome helpers left the owned group: {rows:?}"
                );
                assert!(profile.is_dir());
                eprintln!(
                    "owned Chrome group observed: pgid={process_group} members={} profile={}",
                    rows.len(),
                    profile.display()
                );
                return Self {
                    process_group: *process_group,
                    profile: profile.clone(),
                };
            }
            assert!(
                Instant::now() < deadline,
                "owned Chrome group leader and helpers were not observable: {rows:?}"
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

fn owned_profile(command: &str, owned_prefix: &str) -> Option<PathBuf> {
    command
        .split_whitespace()
        .find_map(|argument| argument.strip_prefix("--user-data-dir="))
        .filter(|path| {
            Path::new(path)
                .file_name()
                .is_some_and(|name| name.to_string_lossy().starts_with(owned_prefix))
        })
        .map(PathBuf::from)
}

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

type SlowPath = (&'static str, Duration);

impl FixtureServer {
    fn start() -> Self {
        Self::with_body(FIXTURE)
    }

    fn with_body(body: &'static str) -> Self {
        Self::serve(body, None)
    }

    fn with_slow_path(body: &'static str, path: &'static str, delay: Duration) -> Self {
        Self::serve(body, Some((path, delay)))
    }

    fn serve(body: &'static str, slow: Option<SlowPath>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let address = listener.local_addr().unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let worker_stop = Arc::clone(&stop);
        let worker = thread::spawn(move || {
            while !worker_stop.load(Ordering::Relaxed) {
                match listener.accept() {
                    Ok((mut stream, _)) => serve_fixture(&mut stream, body, slow),
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(5));
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

fn serve_fixture(stream: &mut TcpStream, body: &str, slow: Option<SlowPath>) {
    let request = read_request(stream);
    let target = request.split_whitespace().nth(1).unwrap_or("/");
    if let Some((path, delay)) = slow
        && target.starts_with(path)
    {
        thread::sleep(delay);
    }
    let content_type = if target.split('?').next().unwrap().ends_with(".css") {
        "text/css"
    } else {
        "text/html"
    };
    let response = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: {content_type}; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        body.len(),
        body
    );
    let _ = stream.write_all(response.as_bytes());
}

fn read_request(stream: &mut TcpStream) -> String {
    stream.set_nonblocking(false).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(2)))
        .unwrap();
    let mut request = Vec::new();
    let mut chunk = [0_u8; 2048];
    while !request_complete(&request) {
        match stream.read(&mut chunk) {
            Ok(0) | Err(_) => break,
            Ok(read) => request.extend_from_slice(&chunk[..read]),
        }
    }
    String::from_utf8_lossy(&request).into_owned()
}

fn request_complete(request: &[u8]) -> bool {
    let text = String::from_utf8_lossy(request);
    let Some(end) = text.find("\r\n\r\n") else {
        return false;
    };
    let length = text[..end]
        .lines()
        .find_map(|line| {
            line.to_ascii_lowercase()
                .strip_prefix("content-length:")
                .and_then(|value| value.trim().parse::<usize>().ok())
        })
        .unwrap_or(0);
    request.len() >= end + 4 + length
}

fn launch_headless() -> OwnedBrowser {
    OwnedBrowser::launch(BrowserConfig {
        explicit_binary: None,
        headless: true,
        width: 1120,
        height: 780,
        inherit_process_group: false,
    })
    .unwrap()
}

fn has_line(observation: &Observation, expected: &str) -> bool {
    observation
        .visible_text
        .lines()
        .any(|line| line == expected)
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
        focus_anchor: None,
    }
}

fn prepared_key(
    observation: &Observation,
    key: manuvra_chrome::Key,
    sequence: u64,
) -> PreparedInput {
    PreparedInput {
        document_id: observation.document_id.clone(),
        node_id: 0,
        operation: PreparedOperation::PressKey(key),
        text: None,
        previous_text: None,
        option_node_id: None,
        combobox: false,
        action_sequence: sequence,
        focus_anchor: observation.focus_anchor.clone(),
    }
}

#[test]
#[ignore = "requires the local Chromium executable"]
fn arrow_home_end_and_enter_follow_composite_widget_state() {
    use manuvra_chrome::Key;
    let _serial = REAL_BROWSER.lock().unwrap();
    let server = FixtureServer::with_body(WIDGET_FIXTURE);
    let mut browser = OwnedBrowser::launch(BrowserConfig {
        explicit_binary: None,
        headless: true,
        width: 1120,
        height: 780,
        inherit_process_group: false,
    })
    .unwrap();
    let lifecycle = BrowserLifecycle::observe();
    browser.navigate(&server.url()).unwrap();
    let mut observed = browser.observe().unwrap();
    for (key, active_id) in [
        (Key::Tab, None),
        (Key::ArrowDown, Some("alpha")),
        (Key::ArrowDown, Some("beta")),
        (Key::Home, Some("alpha")),
        (Key::End, Some("gamma")),
        (Key::ArrowUp, Some("beta")),
        (Key::ArrowRight, Some("gamma")),
        (Key::ArrowLeft, Some("beta")),
    ] {
        browser
            .perform(
                prepared_key(&observed, key, 1),
                &InputCancellation::default(),
            )
            .unwrap();
        observed = browser.observe().unwrap();
        let anchor = observed.focus_anchor.as_ref().unwrap();
        assert_eq!(anchor.role, "combobox");
        assert_eq!(
            anchor
                .active_descendant
                .as_ref()
                .map(|item| item.id.as_str()),
            active_id
        );
    }
    browser
        .perform(
            prepared_key(&observed, Key::Enter, 2),
            &InputCancellation::default(),
        )
        .unwrap();
    observed = browser.observe().unwrap();
    assert!(observed.visible_text.contains("Selected: Beta"));
    assert_eq!(
        observed
            .focus_anchor
            .as_ref()
            .unwrap()
            .active_descendant
            .as_ref()
            .unwrap()
            .selected,
        Some(true)
    );
    browser
        .perform(
            prepared_key(&observed, Key::Tab, 3),
            &InputCancellation::default(),
        )
        .unwrap();
    observed = browser.observe().unwrap();
    assert_eq!(observed.focus_anchor.as_ref().unwrap().name, "First action");
    browser
        .perform(
            prepared_key(&observed, Key::ArrowDown, 4),
            &InputCancellation::default(),
        )
        .unwrap();
    observed = browser.observe().unwrap();
    assert_eq!(
        observed.focus_anchor.as_ref().unwrap().name,
        "Second action"
    );
    browser
        .perform(
            prepared_key(&observed, Key::ArrowUp, 5),
            &InputCancellation::default(),
        )
        .unwrap();
    observed = browser.observe().unwrap();
    assert_eq!(observed.focus_anchor.as_ref().unwrap().name, "First action");
    browser
        .perform(
            prepared_key(&observed, Key::Tab, 6),
            &InputCancellation::default(),
        )
        .unwrap();
    observed = browser.observe().unwrap();
    let shadow_anchor = observed.focus_anchor.as_ref().unwrap();
    assert_eq!(shadow_anchor.name, "Shadow choice");
    let descendant = shadow_anchor.active_descendant.as_ref().unwrap();
    assert_eq!(descendant.id, "gamma");
    assert_eq!(descendant.name, "");
    assert_eq!(descendant.role, "");
    assert_eq!(shadow_anchor.position, None);
    for (sequence, (key, position)) in [
        (Key::Tab, 1),
        (Key::ArrowDown, 2),
        (Key::ArrowDown, 3),
        (Key::Tab, 7),
        (Key::ArrowDown, 8),
    ]
    .into_iter()
    .enumerate()
    {
        browser
            .perform(
                prepared_key(&observed, key, 7 + sequence as u64),
                &InputCancellation::default(),
            )
            .unwrap();
        observed = browser.observe().unwrap();
        let anchor = observed.focus_anchor.as_ref().unwrap();
        assert_eq!(anchor.role, "treeitem", "{key:?}");
        assert_eq!(anchor.name, "", "{key:?}");
        assert_eq!(anchor.position, Some(position), "{key:?}");
    }
    browser.close().unwrap();
    lifecycle.assert_cleaned();
}

#[test]
#[ignore = "requires the local Chromium executable"]
fn enter_and_space_activate_a_focused_native_button_once_each() {
    use manuvra_chrome::Key;
    let _serial = REAL_BROWSER.lock().unwrap();
    let server = FixtureServer::with_body(ACTIVATION_FIXTURE);
    let mut browser = OwnedBrowser::launch(BrowserConfig {
        explicit_binary: None,
        headless: true,
        width: 1120,
        height: 780,
        inherit_process_group: false,
    })
    .unwrap();
    let lifecycle = BrowserLifecycle::observe();
    for key in [Key::Enter, Key::Space] {
        browser.navigate(&server.url()).unwrap();
        let observed = browser.observe().unwrap();
        browser
            .perform(
                prepared_key(&observed, Key::Tab, 1),
                &InputCancellation::default(),
            )
            .unwrap();
        let focused = browser.observe().unwrap();
        assert_eq!(focused.focus_anchor.as_ref().unwrap().name, "Save");
        browser
            .perform(
                prepared_key(&focused, key, 2),
                &InputCancellation::default(),
            )
            .unwrap();
        let after = browser.observe().unwrap();
        assert!(
            has_line(&after, "Activations: 1"),
            "{key:?}: {}",
            after.visible_text
        );
        assert_eq!(after.focus_anchor.as_ref().unwrap().name, "Save");
    }
    browser.close().unwrap();
    lifecycle.assert_cleaned();
}

#[test]
#[ignore = "requires the local Chromium executable"]
fn keyboard_popover_traps_tab_and_escape_restores_trigger_focus() {
    use manuvra_chrome::Key;
    let _serial = REAL_BROWSER.lock().unwrap();
    let server = FixtureServer::with_body(KEYBOARD_FIXTURE);
    let mut browser = OwnedBrowser::launch(BrowserConfig {
        explicit_binary: None,
        headless: true,
        width: 1120,
        height: 780,
        inherit_process_group: false,
    })
    .unwrap();
    let lifecycle = BrowserLifecycle::observe();
    browser.navigate(&server.url()).unwrap();
    let mut observed = browser.observe().unwrap();
    assert!(observed.focus_anchor.is_none());
    browser
        .perform(
            prepared_key(&observed, Key::Tab, 1),
            &InputCancellation::default(),
        )
        .unwrap();
    observed = browser.observe().unwrap();
    assert_eq!(observed.focus_anchor.as_ref().unwrap().name, "Before");
    for (key, expected) in [
        (Key::Tab, "Open breakdown"),
        (Key::ShiftTab, "Before"),
        (Key::Tab, "Open breakdown"),
    ] {
        browser
            .perform(
                prepared_key(&observed, key, 2),
                &InputCancellation::default(),
            )
            .unwrap();
        observed = browser.observe().unwrap();
        assert_eq!(observed.focus_anchor.as_ref().unwrap().name, expected);
    }
    browser
        .perform(
            prepared(
                &observed,
                "Open breakdown",
                PreparedOperation::Click,
                None,
                None,
                3,
            ),
            &InputCancellation::default(),
        )
        .unwrap();
    observed = browser.observe().unwrap();
    assert_eq!(observed.focus_anchor.as_ref().unwrap().name, "First item");
    assert_eq!(
        observed.focus_anchor.as_ref().unwrap().in_dialog.as_deref(),
        Some("Breakdown")
    );
    for (key, expected) in [
        (Key::ShiftTab, "Last item"),
        (Key::Tab, "First item"),
        (Key::Tab, "Last item"),
        (Key::Tab, "First item"),
    ] {
        browser
            .perform(
                prepared_key(&observed, key, 4),
                &InputCancellation::default(),
            )
            .unwrap();
        observed = browser.observe().unwrap();
        assert_eq!(observed.focus_anchor.as_ref().unwrap().name, expected);
    }
    browser
        .perform(
            prepared_key(&observed, Key::Escape, 5),
            &InputCancellation::default(),
        )
        .unwrap();
    observed = browser.observe().unwrap();
    assert!(observed.dialogs.is_empty());
    assert_eq!(
        observed.focus_anchor.as_ref().unwrap().name,
        "Open breakdown"
    );
    browser.close().unwrap();
    lifecycle.assert_cleaned();
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
    let lifecycle = BrowserLifecycle::observe();
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
    assert!(observed.hover_regions.is_empty());
    assert!(!observed.hover_regions_truncated);

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
    lifecycle.assert_cleaned();
}

#[test]
#[ignore = "requires the local Chromium executable"]
fn production_snapshot_observes_indexed_dialog_and_body_focus() {
    let _serial = REAL_BROWSER.lock().unwrap();
    let server = FixtureServer::with_body(FOCUS_FIXTURE);
    let mut browser = launch_headless();
    let lifecycle = BrowserLifecycle::observe();
    for (query, focused, name, role, indexed, dialog) in [
        ("button", true, "Save", "button", true, None),
        ("dialog", true, "", "interactive", false, Some("Breakdown")),
        (
            "labelled-dialog",
            true,
            "Breakdown",
            "dialog",
            false,
            Some("Breakdown"),
        ),
        (
            "native-dialog",
            true,
            "Native breakdown",
            "dialog",
            false,
            Some("Native breakdown"),
        ),
        ("shadow", true, "Shadow details", "interactive", false, None),
        ("body", false, "", "", false, None),
    ] {
        browser
            .navigate(&format!("{}?focus={query}", server.url()))
            .unwrap();
        let observed = browser.observe().unwrap();
        assert_eq!(observed.focus_anchor.is_some(), focused, "{query}");
        assert_eq!(observed.focused.is_some(), indexed, "{query}");
        if let Some(anchor) = observed.focus_anchor {
            assert_eq!(anchor.name, name, "{query}");
            assert_eq!(anchor.role, role, "{query}");
            assert_eq!(anchor.in_dialog.as_deref(), dialog, "{query}");
            assert!(anchor.covered, "{query}");
            assert_eq!(anchor.surface, None, "{query}");
            assert!(!anchor.name.contains("Descendant text"));
        }
    }
    browser.close().unwrap();
    lifecycle.assert_cleaned();
}

#[test]
#[ignore = "requires the local Chromium executable"]
fn production_snapshot_resolves_dialog_across_open_shadow_root() {
    let _serial = REAL_BROWSER.lock().unwrap();
    let server = FixtureServer::with_body(FOCUS_FIXTURE);
    let mut browser = launch_headless();
    let lifecycle = BrowserLifecycle::observe();
    browser
        .navigate(&format!("{}?focus=shadow-dialog", server.url()))
        .unwrap();
    let observed = browser.observe().unwrap();
    let anchor = observed.focus_anchor.as_ref().unwrap();
    assert_eq!(anchor.name, "Confirm");
    assert_eq!(anchor.in_dialog.as_deref(), Some("Checkout"));
    let button = observed
        .elements
        .iter()
        .find(|element| element.name == "Confirm")
        .unwrap();
    assert_eq!(button.in_dialog.as_deref(), Some("Checkout"));
    assert_eq!(observed.focused, Some(button.index));
    let labelled = observed
        .elements
        .iter()
        .find(|element| element.name == "Shadow labelled action")
        .expect("aria-labelledby resolves inside the element's shadow root");
    assert_eq!(labelled.role, "button");
    assert_eq!(labelled.in_dialog, None);
    browser.close().unwrap();
    lifecycle.assert_cleaned();
}

#[test]
#[ignore = "requires the local Chromium executable"]
fn production_snapshot_uses_aria_name_inside_shadow_dialog() {
    let _serial = REAL_BROWSER.lock().unwrap();
    let server = FixtureServer::with_body(FOCUS_FIXTURE);
    let mut browser = launch_headless();
    let lifecycle = BrowserLifecycle::observe();
    browser
        .navigate(&format!("{}?focus=shadow-dialog-pin", server.url()))
        .unwrap();
    let observed = browser.observe().unwrap();
    let anchor = observed.focus_anchor.as_ref().unwrap();
    assert_eq!(anchor.name, "PIN");
    assert_eq!(anchor.in_dialog.as_deref(), Some("Checkout"));
    assert_eq!(observed.focused, None);
    browser.close().unwrap();
    lifecycle.assert_cleaned();
}

#[test]
#[ignore = "requires the local Chromium executable"]
fn production_snapshot_marks_focus_inside_uncovered_surfaces() {
    use manuvra_chrome::FocusSurface;
    let _serial = REAL_BROWSER.lock().unwrap();
    let server = FixtureServer::with_body(FOCUS_FIXTURE);
    let mut browser = launch_headless();
    let lifecycle = BrowserLifecycle::observe();
    for (query, covered, surface) in [
        ("closed-shadow", false, Some(FocusSurface::ClosedShadowRoot)),
        (
            "declarative-shadow",
            false,
            Some(FocusSurface::ClosedShadowRoot),
        ),
        ("canvas", true, Some(FocusSurface::Canvas)),
        ("frame", true, None),
    ] {
        browser
            .navigate(&format!("{}?focus={query}", server.url()))
            .unwrap();
        let observed = browser.observe().unwrap();
        let anchor = observed.focus_anchor.as_ref().expect(query);
        assert_eq!(anchor.covered, covered, "{query}");
        assert_eq!(anchor.surface, surface, "{query}");
        assert!(!anchor.name.contains("inner"), "{query}");
    }
    let observed = browser.observe().unwrap();
    let anchor = observed.focus_anchor.as_ref().unwrap();
    assert!(anchor.context.starts_with("main/frame:"), "{anchor:?}");
    assert_eq!(anchor.name, "Frame field");
    assert_eq!(anchor.role, "textbox");
    let field = observed
        .elements
        .iter()
        .find(|element| element.name == "Frame field")
        .unwrap();
    assert_eq!(observed.focused, Some(field.index));
    assert_eq!(anchor.context, field.context);
    browser.close().unwrap();
    lifecycle.assert_cleaned();
}

#[test]
#[ignore = "requires the local Chromium executable"]
fn production_snapshot_marks_focus_inside_a_cross_origin_frame() {
    use manuvra_chrome::{FocusSurface, Key};
    let _serial = REAL_BROWSER.lock().unwrap();
    let server = FixtureServer::with_body(FOCUS_FIXTURE);
    let other_origin = FixtureServer::with_body(FOCUS_FIXTURE);
    let mut browser = launch_headless();
    let lifecycle = BrowserLifecycle::observe();
    browser
        .navigate(&format!(
            "{}?focus=cross-origin-frame&frame={}",
            server.url(),
            other_origin.url()
        ))
        .unwrap();
    let observed = browser.observe().unwrap();
    assert_eq!(observed.focus_anchor.as_ref().unwrap().name, "Before frame");
    browser
        .perform(
            prepared_key(&observed, Key::Tab, 1),
            &InputCancellation::default(),
        )
        .unwrap();
    let observed = browser.observe().unwrap();
    let anchor = observed.focus_anchor.as_ref().unwrap();
    assert_eq!(anchor.role, "iframe");
    assert!(!anchor.covered);
    assert_eq!(anchor.surface, Some(FocusSurface::CrossOriginFrame));
    browser.close().unwrap();
    lifecycle.assert_cleaned();
}

#[test]
#[ignore = "requires the local Chromium executable"]
fn enter_on_a_submit_button_settles_slow_navigation_before_the_next_capture() {
    use manuvra_chrome::Key;
    let _serial = REAL_BROWSER.lock().unwrap();
    let server =
        FixtureServer::with_slow_path(SUBMIT_FIXTURE, "/submitted", Duration::from_millis(1500));
    let mut browser = launch_headless();
    let lifecycle = BrowserLifecycle::observe();
    for (query, button) in [("", "Submit"), ("?focus=validated", "Validate and submit")] {
        browser
            .navigate(&format!("{}{query}", server.url()))
            .unwrap();
        let observed = browser.observe().unwrap();
        assert_eq!(observed.focus_anchor.as_ref().unwrap().name, button);
        browser
            .perform(
                prepared_key(&observed, Key::Enter, 1),
                &InputCancellation::default(),
            )
            .unwrap();
        let captured = browser.capture().unwrap().observation;
        assert_eq!(captured.title, "Submitted", "{button}");
        assert_eq!(captured.route, "/submitted", "{button}");
        assert!(
            has_line(&captured, "Submitted page"),
            "{button}: {}",
            captured.visible_text
        );
    }
    browser.close().unwrap();
    lifecycle.assert_cleaned();
}

#[test]
#[ignore = "requires the local Chromium executable"]
fn stale_focus_anchor_rejects_the_key_before_any_keydown_reaches_the_page() {
    use manuvra_chrome::Key;
    let _serial = REAL_BROWSER.lock().unwrap();
    let server = FixtureServer::with_body(REVALIDATION_FIXTURE);
    let mut browser = launch_headless();
    let lifecycle = BrowserLifecycle::observe();
    browser.navigate(&server.url()).unwrap();
    let stale = browser.observe().unwrap();
    assert_eq!(stale.focus_anchor.as_ref().unwrap().name, "First");
    browser
        .perform(
            prepared(
                &stale,
                "Move focus",
                PreparedOperation::Click,
                None,
                None,
                1,
            ),
            &InputCancellation::default(),
        )
        .unwrap();
    assert!(matches!(
        browser.perform(
            prepared_key(&stale, Key::Tab, 2),
            &InputCancellation::default(),
        ),
        Err(PerformError::Rejected(reason)) if reason == "focus_changed"
    ));
    let current = browser.observe().unwrap();
    assert!(
        has_line(&current, "Keydowns: 0"),
        "{}",
        current.visible_text
    );
    assert_eq!(current.focus_anchor.as_ref().unwrap().name, "Field");
    browser
        .perform(
            prepared_key(&current, Key::Tab, 3),
            &InputCancellation::default(),
        )
        .unwrap();
    let after = browser.observe().unwrap();
    assert!(has_line(&after, "Keydowns: 1"), "{}", after.visible_text);
    browser.close().unwrap();
    lifecycle.assert_cleaned();
}

#[test]
#[ignore = "requires the local Chromium executable"]
fn arrow_keys_move_the_native_caret_in_text_fields() {
    use manuvra_chrome::Key;
    let _serial = REAL_BROWSER.lock().unwrap();
    let server = FixtureServer::with_body(EDITING_FIXTURE);
    let mut browser = launch_headless();
    let lifecycle = BrowserLifecycle::observe();
    for (query, field, steps) in [
        (
            "",
            "Line",
            [(Key::ArrowLeft, "Caret: 2"), (Key::ArrowRight, "Caret: 3")],
        ),
        (
            "?field=lines",
            "Lines",
            [(Key::ArrowDown, "Caret: 3"), (Key::ArrowUp, "Caret: 0")],
        ),
    ] {
        browser
            .navigate(&format!("{}{query}", server.url()))
            .unwrap();
        let mut observed = browser.observe().unwrap();
        assert_eq!(observed.focus_anchor.as_ref().unwrap().name, field);
        for (sequence, (key, caret)) in steps.into_iter().enumerate() {
            browser
                .perform(
                    prepared_key(&observed, key, sequence as u64 + 1),
                    &InputCancellation::default(),
                )
                .unwrap();
            observed = browser.observe().unwrap();
            assert!(
                has_line(&observed, caret),
                "{key:?}: {}",
                observed.visible_text
            );
        }
    }
    browser.close().unwrap();
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
    let lifecycle = BrowserLifecycle::observe();
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
    assert!(
        has_line(&observed, "Selected option"),
        "{}",
        observed.visible_text
    );
    assert!(
        has_line(&observed, "Enter presses: 0"),
        "typing and choosing an option never presses Enter: {}",
        observed.visible_text
    );

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
    for clicked in ["Shadow clicked", "Slotted clicked", "Frame clicked"] {
        assert!(
            has_line(&observed, clicked),
            "{clicked}: {}",
            observed.visible_text
        );
    }
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
            focus_anchor: None,
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
    lifecycle.assert_cleaned();
}

fn hover_regions(observation: &Observation) -> Vec<(u64, String, Vec<String>)> {
    observation
        .hover_regions
        .iter()
        .map(|region| {
            (
                region.index,
                region.name.clone(),
                region.reveals_on_hover.clone(),
            )
        })
        .collect()
}

fn assert_no_hidden_candidates(observation: &Observation) {
    let hidden: Vec<_> = observation
        .elements
        .iter()
        .filter(|element| element.name.starts_with("Actions for"))
        .map(|element| element.name.as_str())
        .collect();
    assert!(
        hidden.is_empty(),
        "hidden controls became candidates: {hidden:?}"
    );
}

#[test]
#[ignore = "requires the local Chromium executable"]
fn production_snapshot_lists_hover_regions_and_keeps_their_controls_out_of_candidates() {
    let _serial = REAL_BROWSER.lock().unwrap();
    let server = FixtureServer::with_body(HOVER_FIXTURE);
    let mut browser = OwnedBrowser::launch(BrowserConfig {
        explicit_binary: None,
        headless: true,
        width: 1120,
        height: 1400,
        inherit_process_group: false,
    })
    .unwrap();
    let lifecycle = BrowserLifecycle::observe();
    browser.navigate(&server.url()).unwrap();

    let observed = browser.observe().unwrap();
    let expected: Vec<_> = [
        "Essentials",
        "Groceries",
        "Rent",
        "Utilities",
        "Lifestyle",
        "Dining out",
        "Streaming",
    ]
    .iter()
    .zip(1..)
    .map(|(name, index)| (index, name.to_string(), vec![format!("Actions for {name}")]))
    .collect();
    assert_eq!(hover_regions(&observed), expected);
    assert!(!observed.hover_regions_truncated);
    let mut identities: Vec<_> = observed
        .hover_regions
        .iter()
        .map(|region| region.node_id)
        .collect();
    identities.sort_unstable();
    identities.dedup();
    assert_eq!(identities.len(), expected.len());
    assert!(observed.hover_regions.iter().all(|region| {
        observed
            .elements
            .iter()
            .all(|element| element.node_id != region.node_id)
    }));
    for concealed in ["Gym", "Travel"] {
        let row_control = format!("View activity for {concealed},");
        assert!(
            observed
                .elements
                .iter()
                .any(|element| element.name.starts_with(&row_control)),
            "{concealed} row is rendered"
        );
        assert!(!observed.hover_regions.iter().any(|region| {
            region.name == concealed
                || region
                    .reveals_on_hover
                    .iter()
                    .any(|name| name.contains(concealed))
        }));
    }
    assert!(
        observed
            .elements
            .iter()
            .any(|element| element.name == "Edit assigned amount for Groceries, $400.00")
    );
    assert_no_hidden_candidates(&observed);

    browser
        .navigate(&format!("{}?many=1", server.url()))
        .unwrap();
    let capped = browser.observe().unwrap();
    assert_eq!(capped.hover_regions.len(), 20);
    assert_eq!(hover_regions(&capped)[..expected.len()], expected[..]);
    assert_eq!(
        capped.hover_regions[expected.len()].name,
        "Subscriptions",
        "regions continue in document order"
    );
    assert!(
        capped
            .hover_regions
            .iter()
            .zip(1..)
            .all(|(region, index)| region.index == index)
    );
    assert!(capped.hover_regions_truncated);
    assert!(
        capped
            .elements
            .iter()
            .any(|element| element.name == "Edit assigned amount for Subscription 16, $9.00"),
        "the rows beyond the cap are in the viewport"
    );
    assert_eq!(capped.coverage, observed.coverage);
    assert!(capped.coverage.viewport_complete);
    assert_no_hidden_candidates(&capped);
    eprintln!("real Chrome hover region fixture completed");
    browser.close().unwrap();
    lifecycle.assert_cleaned();
}

fn candidate<'a>(observation: &'a Observation, name: &str) -> Option<&'a Element> {
    observation
        .elements
        .iter()
        .find(|element| element.name == name)
}

#[test]
#[ignore = "requires the local Chromium executable"]
fn production_hover_reveals_only_its_region_controls_for_a_following_click() {
    let _serial = REAL_BROWSER.lock().unwrap();
    let server = FixtureServer::with_body(HOVER_FIXTURE);
    let mut browser = OwnedBrowser::launch(BrowserConfig {
        explicit_binary: None,
        headless: true,
        width: 1120,
        height: 1400,
        inherit_process_group: false,
    })
    .unwrap();
    let lifecycle = BrowserLifecycle::observe();
    browser.navigate(&server.url()).unwrap();
    let cancellation = InputCancellation::default();

    let observed = browser.observe().unwrap();
    assert_no_hidden_candidates(&observed);
    let region = observed
        .hover_regions
        .iter()
        .find(|region| region.name == "Groceries")
        .expect("Groceries hover region");
    let fact = browser
        .perform(
            PreparedInput {
                document_id: observed.document_id.clone(),
                node_id: region.node_id,
                operation: PreparedOperation::Hover,
                text: None,
                previous_text: None,
                option_node_id: None,
                combobox: false,
                action_sequence: 1,
                focus_anchor: None,
            },
            &cancellation,
        )
        .unwrap();
    assert_eq!(fact.suboperations, ["mouse_move"]);

    let revealed = browser.observe().unwrap();
    assert!(
        candidate(&revealed, "Actions for Groceries").is_some(),
        "the hovered region's control is a candidate"
    );
    assert!(
        candidate(&revealed, "Actions for Rent").is_none(),
        "another region's control stays hidden"
    );
    browser
        .perform(
            prepared(
                &revealed,
                "Actions for Groceries",
                PreparedOperation::Click,
                None,
                None,
                2,
            ),
            &cancellation,
        )
        .unwrap();

    let opened = browser.observe().unwrap();
    assert_eq!(
        candidate(&opened, "Actions for Groceries").and_then(|element| element.expanded),
        Some(true)
    );
    assert!(
        opened
            .elements
            .iter()
            .any(|element| element.role == "menuitem" && element.name == "Delete category…"),
        "the Groceries menu is open"
    );
    let regions: Vec<_> = opened
        .hover_regions
        .iter()
        .map(|region| region.name.as_str())
        .collect();
    assert!(!regions.contains(&"Groceries"), "{regions:?}");
    assert!(regions.contains(&"Rent"), "{regions:?}");
    eprintln!("real Chrome hover reveal fixture completed");
    browser.close().unwrap();
    lifecycle.assert_cleaned();
}

#[test]
#[ignore = "requires the local Chromium executable"]
fn headless_owned_browser_presents_a_desktop_mouse() {
    let _serial = REAL_BROWSER.lock().unwrap();
    let server = FixtureServer::with_body(
        r#"<!doctype html><body><script>
        for (const feature of ['hover: hover', 'any-hover: hover', 'pointer: fine', 'any-pointer: fine']) {
            const result = document.createElement('p');
            result.textContent = feature + '=' + matchMedia('(' + feature + ')').matches;
            document.body.appendChild(result);
        }
        </script></body>"#,
    );
    let mut browser = launch_headless();
    let lifecycle = BrowserLifecycle::observe();
    browser.navigate(&server.url()).unwrap();
    let observed = browser.observe().unwrap();
    for feature in [
        "hover: hover",
        "any-hover: hover",
        "pointer: fine",
        "any-pointer: fine",
    ] {
        assert!(
            has_line(&observed, &format!("{feature}=true")),
            "{}",
            observed.visible_text
        );
    }
    browser.close().unwrap();
    lifecycle.assert_cleaned();
}

#[test]
#[ignore = "requires the local Chromium executable"]
fn headless_hover_reveals_media_gated_row_actions_for_a_following_click() {
    let _serial = REAL_BROWSER.lock().unwrap();
    let server = FixtureServer::with_body(HOVER_FIXTURE);
    let mut browser = launch_headless();
    let lifecycle = BrowserLifecycle::observe();
    browser.navigate(&server.url()).unwrap();
    let cancellation = InputCancellation::default();
    let observed = browser.observe().unwrap();
    assert_no_hidden_candidates(&observed);
    let region = observed
        .hover_regions
        .iter()
        .find(|region| region.name == "Rent")
        .expect("Rent hover region");
    browser
        .perform(
            PreparedInput {
                document_id: observed.document_id.clone(),
                node_id: region.node_id,
                operation: PreparedOperation::Hover,
                text: None,
                previous_text: None,
                option_node_id: None,
                combobox: false,
                action_sequence: 1,
                focus_anchor: None,
            },
            &cancellation,
        )
        .unwrap();
    let revealed = browser.observe().unwrap();
    assert!(
        candidate(&revealed, "Actions for Rent").is_some(),
        "the media-gated control is a candidate after hover"
    );
    assert!(candidate(&revealed, "Actions for Groceries").is_none());
    browser
        .perform(
            prepared(
                &revealed,
                "Actions for Rent",
                PreparedOperation::Click,
                None,
                None,
                2,
            ),
            &cancellation,
        )
        .unwrap();
    let opened = browser.observe().unwrap();
    assert_eq!(
        candidate(&opened, "Actions for Rent").and_then(|element| element.expanded),
        Some(true)
    );
    assert!(has_line(&opened, "Actions menu for Rent"));
    assert!(
        opened
            .elements
            .iter()
            .any(|element| element.role == "menuitem" && element.name == "Delete category…")
    );
    browser.close().unwrap();
    lifecycle.assert_cleaned();
}

const TICKING_PAGE: &str = r#"<!doctype html><title>Ticking</title><body><p>Static</p><script>
const where = new URLSearchParams(location.search).get('tick');
const tick = (node) => { let count = 0; setInterval(() => { node.textContent = `Tick ${++count}`; }, 5); };
if (where === 'main') tick(document.body.appendChild(document.createElement('p')));
for (const mode of ['open', 'closed']) if (where === `${mode}-shadow`) {
  const root = document.body.appendChild(document.createElement('div')).attachShadow({mode});
  tick(root.appendChild(document.createElement('p')));
}
if (where === 'frame') {
  const frame = document.createElement('iframe');
  frame.srcdoc = '<p>Frame</p><script>let count = 0; setInterval(() => { document.querySelector("p").textContent = `Tick ${++count}`; }, 5);<\/script>';
  document.body.append(frame);
}
</script></body>"#;

const FENCING_FAILED: &str = "page changed throughout screenshot fencing";

#[test]
#[ignore = "requires the local Chromium executable"]
fn capture_is_refused_while_the_dom_keeps_changing_anywhere_on_the_page() {
    let _serial = REAL_BROWSER.lock().unwrap();
    let server = FixtureServer::with_body(TICKING_PAGE);
    let mut browser = launch_headless();
    let lifecycle = BrowserLifecycle::observe();
    browser.navigate(&server.url()).unwrap();
    let still = browser.capture().unwrap();
    assert!(has_line(&still.observation, "Static"));
    for (tick, redacted_failure) in [
        ("main", FENCING_FAILED),
        ("open-shadow", FENCING_FAILED),
        // Masking cannot see inside a closed shadow root, so redaction fails first.
        ("closed-shadow", "redaction_unverifiable"),
        ("frame", FENCING_FAILED),
    ] {
        browser
            .navigate(&format!("{}?tick={tick}", server.url()))
            .unwrap();
        assert!(
            matches!(browser.capture(), Err(BrowserError::Control(message)) if message == FENCING_FAILED),
            "{tick}"
        );
        assert!(
            matches!(
                browser.capture_redacted(&["Static".into()]),
                Err(BrowserError::Control(message)) if message == redacted_failure
            ),
            "{tick}"
        );
    }
    browser.close().unwrap();
    lifecycle.assert_cleaned();
}

const MOVING_SECRET_PAGE: &str = r#"<!doctype html><title>Moving secret</title>
<style>body { margin: 0; font: 20px monospace; } #secret { position: absolute; left: 10px; top: 20px; margin: 0; } #result { position: absolute; left: 10px; top: 400px; margin: 0; }</style>
<p id="secret">moving-secret</p><p id="result">Mask pending</p>
<script>
  const secret = document.querySelector('#secret'), masks = new Map(); let moved = false;
  new MutationObserver(records => {
    for (const record of records) {
      for (const node of record.addedNodes) if (node.hasAttribute?.('data-manuvra-mask')) {
        masks.set(node, node.getBoundingClientRect());
        // The first mask ever placed makes the secret move away from it once.
        if (!moved) { moved = true; secret.style.top = '200px'; }
      }
      for (const node of record.removedNodes) if (masks.has(node)) {
        const mask = masks.get(node); masks.delete(node);
        const range = document.createRange(); range.selectNodeContents(secret);
        const text = range.getBoundingClientRect();
        const covered = mask.left <= text.left && mask.top <= text.top && mask.right >= text.right && mask.bottom >= text.bottom;
        document.querySelector('#result').textContent = `Mask covered secret: ${covered}`;
      }
    }
  }).observe(document.documentElement, {childList: true});
</script>"#;

#[test]
#[ignore = "requires the local Chromium executable"]
fn a_secret_that_moves_after_masking_is_masked_again_before_the_screenshot() {
    let _serial = REAL_BROWSER.lock().unwrap();
    let server = FixtureServer::with_body(MOVING_SECRET_PAGE);
    let mut browser = launch_headless();
    let lifecycle = BrowserLifecycle::observe();
    browser.navigate(&server.url()).unwrap();
    let captured = browser.capture_redacted(&["moving-secret".into()]).unwrap();
    assert!(captured.redaction.verifies(1));
    assert_eq!(captured.redaction.matched_values, 1);
    let after = browser.observe().unwrap();
    assert!(
        has_line(&after, "Mask covered secret: true"),
        "the screenshot's masks must cover the secret where it was captured: {}",
        after.visible_text
    );
    browser.close().unwrap();
    lifecycle.assert_cleaned();
}

const UNPAINTED_SECRET_PAGE: &str = r#"<!doctype html><title>hidden-secret</title>
<style>/* hidden-secret */</style>
<script>const note = 'hidden-secret';</script>
<noscript>hidden-secret</noscript>
<template><p>hidden-secret</p></template>
<div style="display: none">hidden-secret</div>
<div hidden><p>hidden-secret</p></div>
<input type="hidden" value="hidden-secret">
<input aria-label="Collapsed" value="hidden-secret" style="width: 0; height: 0; border: 0; padding: 0">
<textarea hidden>hidden-secret</textarea>
<iframe hidden srcdoc="<p>hidden-secret</p>"></iframe>
<canvas hidden></canvas>
<p>Shown: <span style="display: contents">shown-secret</span></p>"#;

#[test]
#[ignore = "requires the local Chromium executable"]
fn text_that_is_never_painted_neither_matches_nor_blocks_redaction() {
    let _serial = REAL_BROWSER.lock().unwrap();
    let server = FixtureServer::with_body(UNPAINTED_SECRET_PAGE);
    let mut browser = launch_headless();
    let lifecycle = BrowserLifecycle::observe();
    browser.navigate(&server.url()).unwrap();
    let captured = browser
        .capture_redacted(&["hidden-secret".into(), "shown-secret".into()])
        .unwrap();
    assert!(captured.redaction.verifies(2), "{:?}", captured.redaction);
    assert_eq!(
        captured.redaction.matched_values, 1,
        "only the painted value matches"
    );
    assert!(captured.redaction.mask_count >= 1);
    browser.close().unwrap();
    lifecycle.assert_cleaned();
}

/// The frame's content box starts 50px in: a 20px border plus 30px padding.
const PADDED_FRAME_PAGE: &str = r#"<!doctype html><title>Padded frame</title>
<style>body { margin: 0; font: 16px sans-serif; } iframe { position: absolute; left: 0; top: 0; width: 300px; height: 200px; border: 20px solid #888; padding: 30px; } .result { position: absolute; left: 0; margin: 0; }</style>
<iframe title="Padded frame" srcdoc="<style>body { margin: 0; font: 20px monospace; } button { display: block; width: 120px; height: 40px; margin: 0; } p { margin: 0; }</style><button onclick='parent.document.querySelector(&quot;#framed&quot;).textContent = &quot;Framed click&quot;'>Framed button</button><p>framed-secret</p>"></iframe>
<p id="framed" class="result" style="top: 400px">Framed pending</p>
<p id="mask-result" class="result" style="top: 440px">Framed mask pending</p>
<script>
  const masks = new Map();
  new MutationObserver(records => {
    for (const record of records) {
      for (const node of record.addedNodes) if (node.hasAttribute?.('data-manuvra-mask')) masks.set(node, node.getBoundingClientRect());
      for (const node of record.removedNodes) if (masks.has(node)) {
        const mask = masks.get(node); masks.delete(node);
        const inner = document.querySelector('iframe').contentDocument, range = inner.createRange();
        range.selectNodeContents(inner.querySelector('p'));
        const text = range.getBoundingClientRect(), left = text.left + 50, top = text.top + 50;
        const covered = mask.left <= left && mask.top <= top && mask.right >= left + text.width && mask.bottom >= top + text.height;
        document.querySelector('#mask-result').textContent = `Framed mask covered: ${covered}`;
      }
    }
  }).observe(document.documentElement, {childList: true});
</script>"#;

#[test]
#[ignore = "requires the local Chromium executable"]
fn frame_geometry_starts_at_the_content_box_for_rects_clicks_and_masks() {
    let _serial = REAL_BROWSER.lock().unwrap();
    let server = FixtureServer::with_body(PADDED_FRAME_PAGE);
    let mut browser = launch_headless();
    let lifecycle = BrowserLifecycle::observe();
    browser.navigate(&server.url()).unwrap();
    let observed = browser.observe().unwrap();
    let button = candidate(&observed, "Framed button").expect("framed button");
    assert_eq!(
        (
            button.rect.x,
            button.rect.y,
            button.rect.width,
            button.rect.height
        ),
        (50.0, 50.0, 120.0, 40.0)
    );
    browser
        .perform(
            prepared(
                &observed,
                "Framed button",
                PreparedOperation::Click,
                None,
                None,
                1,
            ),
            &InputCancellation::default(),
        )
        .unwrap();
    let clicked = browser.observe().unwrap();
    assert!(
        has_line(&clicked, "Framed click"),
        "{}",
        clicked.visible_text
    );
    let captured = browser.capture_redacted(&["framed-secret".into()]).unwrap();
    assert!(captured.redaction.verifies(1));
    assert_eq!(captured.redaction.matched_values, 1);
    let after = browser.observe().unwrap();
    assert!(
        has_line(&after, "Framed mask covered: true"),
        "{}",
        after.visible_text
    );
    browser.close().unwrap();
    lifecycle.assert_cleaned();
}

#[test]
#[ignore = "requires the local Chromium executable"]
fn navigation_that_chrome_reports_as_failed_is_an_error() {
    let _serial = REAL_BROWSER.lock().unwrap();
    let closed = TcpListener::bind("127.0.0.1:0").unwrap();
    let refused = format!("http://{}/", closed.local_addr().unwrap());
    drop(closed);
    let server =
        FixtureServer::with_body("<!doctype html><title>Reachable</title><p>Reachable</p>");
    let mut browser = launch_headless();
    let lifecycle = BrowserLifecycle::observe();
    assert!(
        matches!(
            browser.navigate(&refused),
            Err(BrowserError::Control(message)) if message.starts_with("navigation failed: net::ERR_")
        ),
        "a refused connection is not a loaded document"
    );
    browser.navigate(&server.url()).unwrap();
    assert!(has_line(&browser.observe().unwrap(), "Reachable"));
    browser.close().unwrap();
    lifecycle.assert_cleaned();
}

/// Attaching and detaching 1500 frames emits about 16,500 CDP events, more than
/// the journal retains, after the input that started it has settled.
const FRAME_CHURN_PAGE: &str = r#"<!doctype html><title>Frame churn</title>
<button id="churn">Churn frames</button><button id="count">Count</button>
<p id="state">Churn idle</p><p id="counted">Count: 0</p>
<script>
  let count = 0;
  document.querySelector('#count').addEventListener('click', () => { document.querySelector('#counted').textContent = `Count: ${++count}`; });
  document.querySelector('#churn').addEventListener('click', () => setTimeout(() => {
    for (let index = 0; index < 1500; index += 1) { const frame = document.createElement('iframe'); document.body.append(frame); frame.remove(); }
    document.querySelector('#state').textContent = 'Churn finished';
  }, 500));
</script>"#;

#[test]
#[ignore = "requires the local Chromium executable"]
fn inputs_and_navigation_keep_working_after_the_journal_evicts_old_events() {
    let _serial = REAL_BROWSER.lock().unwrap();
    let server = FixtureServer::with_body(FRAME_CHURN_PAGE);
    let mut browser = launch_headless();
    let lifecycle = BrowserLifecycle::observe();
    browser.navigate(&server.url()).unwrap();
    let observed = browser.observe().unwrap();
    let cancellation = InputCancellation::default();
    browser
        .perform(
            prepared(
                &observed,
                "Churn frames",
                PreparedOperation::Click,
                None,
                None,
                1,
            ),
            &cancellation,
        )
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(30);
    let mut observed = browser.observe().unwrap();
    while !has_line(&observed, "Churn finished") {
        assert!(Instant::now() < deadline, "frame churn did not finish");
        thread::sleep(Duration::from_millis(100));
        observed = browser.observe().unwrap();
    }
    for sequence in 2..4 {
        browser
            .perform(
                prepared(
                    &observed,
                    "Count",
                    PreparedOperation::Click,
                    None,
                    None,
                    sequence,
                ),
                &cancellation,
            )
            .unwrap();
        observed = browser.observe().unwrap();
    }
    assert!(has_line(&observed, "Count: 2"), "{}", observed.visible_text);
    browser.capture().unwrap();
    browser.navigate(&server.url()).unwrap();
    assert!(has_line(&browser.observe().unwrap(), "Churn idle"));
    browser.close().unwrap();
    lifecycle.assert_cleaned();
}

#[test]
#[ignore = "requires the local Chromium executable"]
fn production_snapshot_detects_cssom_reveals_without_listing_opacity_overlays() {
    let _serial = REAL_BROWSER.lock().unwrap();
    let server = FixtureServer::with_body(include_str!(
        "../../../tests/browser/hover-reveal-patterns.html"
    ));
    let mut browser = OwnedBrowser::launch(BrowserConfig {
        explicit_binary: None,
        headless: true,
        width: 1120,
        height: 1400,
        inherit_process_group: false,
    })
    .unwrap();
    let lifecycle = BrowserLifecycle::observe();
    let expected = [
        vec![
            "p-tw4",
            "p-tw3",
            "p-wrapper-desc",
            "p-child",
            "p-self",
            "p-is-list",
            "p-where-list",
            "p-not-hover",
            "p-insertrule",
            "p-adopted",
            "p-wrapper",
            "p-shadow",
            "p-shadow-host",
            "p-iframe",
        ],
        vec![
            "p-media-fine",
            "p-nested",
            "p-nested-media",
            "p-transition",
            "p-deep",
            "p-container",
        ],
        vec![
            "q-global-link-wins",
            "n-specificity-loses",
            "p-both-important",
            "p-layered-hide",
            "p-later-layer",
            "p-inline-important-hover",
            "p-pointer-events-none",
        ],
    ];
    for (group, expected) in expected.into_iter().enumerate() {
        browser
            .navigate(&format!("{}?group={group}", server.url()))
            .unwrap();
        let observation = browser.observe().unwrap();
        let mut found: Vec<_> = observation
            .hover_regions
            .iter()
            .flat_map(|region| region.reveals_on_hover.iter().map(String::as_str))
            .collect();
        found.sort_unstable();
        let mut expected = expected;
        expected.sort_unstable();
        assert_eq!(found, expected, "group {group}");
        assert!(!observation.hover_rules_unreadable);
        assert!(!observation.hover_regions_truncated);
    }
    browser.close().unwrap();
    lifecycle.assert_cleaned();
}

#[test]
#[ignore = "requires the local Chromium executable"]
fn production_snapshot_lists_insertion_gaps_and_hover_reveals_the_selected_gap() {
    let _serial = REAL_BROWSER.lock().unwrap();
    let server =
        FixtureServer::with_body(include_str!("../../../tests/browser/insertion-gaps.html"));
    let mut browser = OwnedBrowser::launch(BrowserConfig {
        explicit_binary: None,
        headless: true,
        width: 1120,
        height: 780,
        inherit_process_group: false,
    })
    .unwrap();
    let lifecycle = BrowserLifecycle::observe();
    browser.navigate(&server.url()).unwrap();
    let observation = browser.observe().unwrap();
    assert_eq!(observation.hover_regions.len(), 3);
    let region = observation
        .hover_regions
        .iter()
        .find(|region| region.name == "between “Primer bloque” and “Segundo bloque”")
        .unwrap();
    browser
        .perform(
            PreparedInput {
                document_id: observation.document_id.clone(),
                node_id: region.node_id,
                operation: PreparedOperation::Hover,
                text: None,
                previous_text: None,
                option_node_id: None,
                combobox: false,
                action_sequence: 1,
                focus_anchor: None,
            },
            &InputCancellation::default(),
        )
        .unwrap();
    let revealed = browser.observe().unwrap();
    assert_eq!(
        revealed
            .elements
            .iter()
            .filter(|element| element.name == "+ Insertar")
            .count(),
        1
    );
    browser.close().unwrap();
    lifecycle.assert_cleaned();
}

#[test]
#[ignore = "requires the local Chromium executable"]
fn insertion_reveals_work_with_development_and_native_nested_utilities() {
    let _serial = REAL_BROWSER.lock().unwrap();
    let fixture = include_str!("../../../tests/browser/insertion-gaps.html");
    let start = fixture.find("<style>").unwrap() + "<style>".len();
    let end = fixture.find("</style>").unwrap();
    let mut browser = launch_headless();
    let lifecycle = BrowserLifecycle::observe();
    for css in [
        include_str!("../../../tests/browser/hover-utilities-dev.css"),
        include_str!("../../../tests/browser/hover-utilities-nested.css"),
    ] {
        let body =
            Box::leak(format!("{}{}{}", &fixture[..start], css, &fixture[end..]).into_boxed_str());
        let server = FixtureServer::with_body(body);
        browser.navigate(&server.url()).unwrap();
        let observation = browser.observe().unwrap();
        assert_eq!(observation.hover_regions.len(), 3);
        assert_eq!(
            observation.hover_regions[1].name,
            "between “Primer bloque” and “Segundo bloque”"
        );
    }
    browser.close().unwrap();
    lifecycle.assert_cleaned();
}

#[test]
#[ignore = "requires the local Chromium executable"]
fn unreadable_hover_stylesheets_preserve_coverage_and_legacy_regions() {
    let _serial = REAL_BROWSER.lock().unwrap();
    let css = FixtureServer::with_body(".remote:hover button { opacity: 1 }");
    let body = Box::leak(format!(r#"<!doctype html><link rel="stylesheet" href="{}remote.css"><style>button {{opacity:0}}</style><ul><li aria-label="Legacy">Legacy <button>Actions</button></li></ul><div class="remote">Unlabeled <button>Remote</button></div><p>Ready</p>"#, css.url()).into_boxed_str());
    let server = FixtureServer::with_body(body);
    let mut browser = launch_headless();
    let lifecycle = BrowserLifecycle::observe();
    browser.navigate(&server.url()).unwrap();
    let observation = browser.observe().unwrap();
    assert!(observation.hover_rules_unreadable);
    assert_eq!(observation.coverage, manuvra_chrome::Coverage::default());
    assert_eq!(observation.hover_regions.len(), 1);
    assert_eq!(observation.hover_regions[0].name, "Legacy");
    assert!(has_line(&observation, "Ready"));
    browser.close().unwrap();
    lifecycle.assert_cleaned();
}

#[test]
#[ignore = "requires the local Chromium executable"]
fn containers_match_semantic_and_repeated_items_before_and_after_hover() {
    let _serial = REAL_BROWSER.lock().unwrap();
    let server =
        FixtureServer::with_body(include_str!("../../../tests/browser/row-containers.html"));
    let mut browser = OwnedBrowser::launch(BrowserConfig {
        explicit_binary: None,
        headless: true,
        width: 1120,
        height: 1800,
        inherit_process_group: false,
    })
    .unwrap();
    let lifecycle = BrowserLifecycle::observe();
    browser.navigate(&server.url()).unwrap();
    let page = browser.observe().unwrap();
    let expected = [
        ("Edit", Some("Alpha")),
        ("Invoice 42", Some("Invoice 42")),
        ("Edit", Some("Invoice 42")),
        ("Edit", Some("Invoice 43")),
        ("Copy", Some("Production token")),
        ("Copy", Some("Staging token")),
        ("Copy", Some("Development token")),
        ("Next", Some("Step 1: Account")),
        ("Delete", None),
        ("Remove", Some("Cart item: Shoes")),
        ("Remove", Some("Cart item: Hat")),
        ("Home", Some("nav")),
        ("Settings", Some("nav")),
        ("Open", Some("Report")),
        ("Off", Some("Wifi")),
        ("On", Some("Bluetooth")),
    ];
    for (name, container) in expected {
        assert!(
            page.elements
                .iter()
                .any(|e| e.name == name && e.container.as_deref() == container),
            "missing {name} in {container:?}: {:?}",
            page.elements
        );
    }
    assert_eq!(
        page.elements
            .iter()
            .filter(|e| e.name == "Open" && e.container.as_deref() == Some("Report"))
            .count(),
        2
    );
    let regions = [
        ("Bravo", "Edit"),
        ("Sunset at the beach", "Delete"),
        ("City at night", "Delete"),
        ("between “insertion gaps” and “Primer bloque”", "+ Insertar"),
        ("between “Primer bloque” and “Segundo bloque”", "+ Insertar"),
        ("Row one", "Archive"),
        ("Row two", "Archive"),
    ];
    assert_eq!(page.hover_regions.len(), regions.len());
    for (container, name) in regions {
        let page = browser.observe().unwrap();
        let region = page
            .hover_regions
            .iter()
            .find(|region| region.name == container)
            .unwrap();
        assert_eq!(region.reveals_on_hover, [name]);
        browser
            .perform(
                PreparedInput {
                    document_id: page.document_id.clone(),
                    node_id: region.node_id,
                    operation: PreparedOperation::Hover,
                    text: None,
                    previous_text: None,
                    option_node_id: None,
                    combobox: false,
                    action_sequence: 1,
                    focus_anchor: None,
                },
                &InputCancellation::default(),
            )
            .unwrap();
        let after = browser.observe().unwrap();
        assert!(
            after
                .elements
                .iter()
                .any(|e| e.name == name && e.container.as_deref() == Some(container)),
            "revealed {name} lost {container}"
        );
    }
    browser.close().unwrap();
    lifecycle.assert_cleaned();
}

#[test]
#[ignore = "requires the local Chromium executable"]
fn container_grouping_and_twins_include_hidden_and_offscreen_controls() {
    let _serial = REAL_BROWSER.lock().unwrap();
    let server = FixtureServer::with_body(
        r#"<!doctype html><style>.row{height:50px}.hidden{opacity:0}.row:hover .hidden{opacity:1}</style>
    <ul aria-label="Rows"><div class="row">Alpha<button>Edit</button></div><div class="row">Bravo<button class="hidden">Edit</button><button class="hidden">Near</button></div></ul>
    <section aria-label="Tokens"><div class="row">Staging<button class="hidden">Copy</button></div><div class="row">Production<button class="hidden">Copy</button></div></section>
    <button>Near</button><button>Unique</button><button style="display:none">Unique</button><button>Far</button><div style="margin-top:1800px"><button>Edit</button><button>Far</button></div>"#,
    );
    let mut browser = launch_headless();
    let lifecycle = BrowserLifecycle::observe();
    browser.navigate(&server.url()).unwrap();
    let page = browser.observe().unwrap();
    assert!(candidate(&page, "Edit").unwrap().shares_name);
    assert!(!candidate(&page, "Unique").unwrap().shares_name);
    assert!(candidate(&page, "Far").unwrap().shares_name);
    assert!(candidate(&page, "Near").unwrap().shares_name);
    assert_eq!(
        page.hover_regions
            .iter()
            .map(|r| r.name.as_str())
            .collect::<Vec<_>>(),
        ["Bravo", "Staging", "Production"]
    );
    browser.close().unwrap();
    lifecycle.assert_cleaned();
}

#[test]
#[ignore = "requires the local Chromium executable"]
fn recycled_virtualized_control_changes_container_while_its_node_stays_the_same() {
    let _serial = REAL_BROWSER.lock().unwrap();
    let server =
        FixtureServer::with_body(include_str!("../../../tests/browser/virtualized-rows.html"));
    let mut browser = launch_headless();
    let lifecycle = BrowserLifecycle::observe();
    browser.navigate(&server.url()).unwrap();
    let before = browser.observe().unwrap();
    let old = candidate(&before, "Edit").unwrap();
    assert_eq!(old.container.as_deref(), Some("Alpha"));
    assert!(!old.shares_name);
    browser
        .perform(
            PreparedInput {
                document_id: before.document_id.clone(),
                node_id: 0,
                operation: PreparedOperation::ScrollDown,
                text: None,
                previous_text: None,
                option_node_id: None,
                combobox: false,
                action_sequence: 1,
                focus_anchor: None,
            },
            &InputCancellation::default(),
        )
        .unwrap();
    let after = browser.observe().unwrap();
    let new = candidate(&after, "Edit").unwrap();
    assert_eq!(new.node_id, old.node_id);
    assert_eq!(new.container.as_deref(), Some("Bravo"));
    assert!(!new.shares_name);
    browser.close().unwrap();
    lifecycle.assert_cleaned();
}

#[test]
#[ignore = "requires the local Chromium executable"]
fn a_region_with_separate_hover_points_reveals_its_second_control_only() {
    let _serial = REAL_BROWSER.lock().unwrap();
    let server = FixtureServer::with_body(
        r#"<!doctype html><style>
      fieldset{width:400px}.group{display:inline-block;padding:20px;margin:20px}
      .group button{opacity:0}.group:hover button{opacity:1}
      </style><fieldset><legend>Tools</legend><span class="group"><button>Edit</button></span><div class="group"><button>Delete</button></div></fieldset>"#,
    );
    let mut browser = launch_headless();
    let lifecycle = BrowserLifecycle::observe();
    browser.navigate(&server.url()).unwrap();
    let observed = browser.observe().unwrap();
    assert_eq!(observed.hover_regions.len(), 1);
    let region = &observed.hover_regions[0];
    assert_eq!(region.reveals_on_hover, ["Edit", "Delete"]);
    assert_eq!(region.reveal_roles, ["button", "button"]);
    assert_eq!(region.reveal_node_ids.len(), 2);
    assert_eq!(region.node_id, region.reveal_node_ids[0]);
    browser
        .perform(
            PreparedInput {
                document_id: observed.document_id,
                node_id: region.reveal_node_ids[1],
                operation: PreparedOperation::Hover,
                text: None,
                previous_text: None,
                option_node_id: None,
                combobox: false,
                action_sequence: 1,
                focus_anchor: None,
            },
            &InputCancellation::default(),
        )
        .unwrap();
    let after = browser.observe().unwrap();
    assert!(candidate(&after, "Delete").is_some());
    assert!(candidate(&after, "Edit").is_none());
    browser.close().unwrap();
    lifecycle.assert_cleaned();
}

#[test]
#[ignore = "requires the local Chromium executable"]
fn hover_reveals_respect_layer_hierarchy_conditions_and_stylesheet_roots() {
    let _serial = REAL_BROWSER.lock().unwrap();
    let server = FixtureServer::with_body(include_str!(
        "../../../tests/browser/hover-reveal-cascade.html"
    ));
    let mut browser = launch_headless();
    let lifecycle = BrowserLifecycle::observe();
    browser.navigate(&server.url()).unwrap();
    let observed = browser.observe().unwrap();
    let mut names: Vec<_> = observed
        .hover_regions
        .iter()
        .flat_map(|region| region.reveals_on_hover.iter().map(String::as_str))
        .collect();
    names.sort();
    assert_eq!(
        names,
        [
            "Important reveal",
            "Late reveal",
            "Layer reveal",
            "Negative reveal",
            "Shadow negative reveal",
            "Shadow reveal"
        ]
    );
    for wanted in [
        "Late reveal",
        "Layer reveal",
        "Negative reveal",
        "Shadow reveal",
    ] {
        let observation = browser.observe().unwrap();
        let region = observation
            .hover_regions
            .iter()
            .find(|region| region.reveals_on_hover.iter().any(|name| name == wanted))
            .unwrap();
        let offset = region
            .reveals_on_hover
            .iter()
            .position(|name| name == wanted)
            .unwrap();
        browser
            .perform(
                PreparedInput {
                    document_id: observation.document_id.clone(),
                    node_id: region.reveal_node_ids[offset],
                    operation: PreparedOperation::Hover,
                    text: None,
                    previous_text: None,
                    option_node_id: None,
                    combobox: false,
                    action_sequence: 1,
                    focus_anchor: None,
                },
                &InputCancellation::default(),
            )
            .unwrap();
        assert!(
            candidate(&browser.observe().unwrap(), wanted).is_some(),
            "{wanted}"
        );
    }
    browser.close().unwrap();
    lifecycle.assert_cleaned();
}

#[test]
#[ignore = "requires the local Chromium executable"]
fn container_labels_prefer_accessible_names_and_headings_and_never_stop_at_cells() {
    let _serial = REAL_BROWSER.lock().unwrap();
    let server = FixtureServer::with_body(
        r#"<!doctype html><span id="reference" hidden>Referenced item</span>
    <article aria-labelledby="reference" aria-label="Fallback item"><p>Painted item</p><button>Referenced action</button></article>
    <article aria-label="Accessible item"><h2>Painted heading</h2><button>Labeled action</button></article>
    <fieldset><legend>First painted segment</legend><h2>Heading item</h2><button>Heading action</button></fieldset>
    <table><tr><td>Alpha</td><td aria-label="Actions">Menu<button>Alpha action</button></td></tr>
    <tr><td>Bravo</td><td aria-label="Actions">Menu<button>Bravo action</button></td></tr></table>"#,
    );
    let mut browser = launch_headless();
    let lifecycle = BrowserLifecycle::observe();
    browser.navigate(&server.url()).unwrap();
    let observed = browser.observe().unwrap();
    for (control, container) in [
        ("Referenced action", "Referenced item"),
        ("Labeled action", "Accessible item"),
        ("Heading action", "Heading item"),
        ("Alpha action", "Alpha"),
        ("Bravo action", "Bravo"),
    ] {
        assert_eq!(
            candidate(&observed, control).unwrap().container.as_deref(),
            Some(container),
            "{control}"
        );
    }
    browser.close().unwrap();
    lifecycle.assert_cleaned();
}

#[test]
#[ignore = "requires the local Chromium executable"]
fn focused_control_keeps_its_container_in_the_snapshot() {
    let _serial = REAL_BROWSER.lock().unwrap();
    let server = FixtureServer::with_body(
        r#"<!doctype html><article aria-label="Bravo"><p>Body</p><input aria-label="Name" autofocus></article>"#,
    );
    let mut browser = launch_headless();
    let lifecycle = BrowserLifecycle::observe();
    browser.navigate(&server.url()).unwrap();
    let observation = browser.observe().unwrap();
    assert_eq!(
        observation.focus_anchor.unwrap().container.as_deref(),
        Some("Bravo")
    );
    browser.close().unwrap();
    lifecycle.assert_cleaned();
}

#[test]
#[ignore = "requires installed Chromium"]
fn opacity_increase_under_negative_hover_is_not_a_reveal() {
    let _serial = REAL_BROWSER.lock().unwrap();
    let server = FixtureServer::with_body(
        r#"<style>body{margin:24px}.row button{opacity:0}.row:not(:hover) button{opacity:1}</style><div class="row"><span>Alpha</span><button>Edit</button></div>"#,
    );
    let mut browser = launch_headless();
    let lifecycle = BrowserLifecycle::observe();
    browser.navigate(&server.url()).unwrap();
    let before = browser.observe().unwrap();
    let button = before.elements.iter().find(|e| e.name == "Edit").unwrap();
    browser
        .perform(
            PreparedInput {
                document_id: before.document_id.clone(),
                node_id: button.node_id,
                operation: PreparedOperation::Hover,
                text: None,
                previous_text: None,
                option_node_id: None,
                combobox: false,
                action_sequence: 1,
                focus_anchor: None,
            },
            &InputCancellation::default(),
        )
        .unwrap();
    let after = browser.observe().unwrap();
    assert!(after.elements.is_empty());
    assert!(after.hover_regions.is_empty());
    browser.close().unwrap();
    lifecycle.assert_cleaned();
}
