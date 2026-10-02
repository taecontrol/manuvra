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
        scroll_region: None,
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
        scroll_region: None,
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
            scroll_region: None,
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
                scroll_region: None,
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
                scroll_region: None,
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
  document.querySelector('#churn').addEventListener('click', () => setTimeout(async () => {
    for (let index = 0; index < 1500; index += 1) {
      const frame = document.createElement('iframe'); document.body.append(frame); frame.remove();
      // Yield without reducing the event count: this fixture tests journal eviction, not a blocked renderer.
      if (index % 10 === 9) await new Promise(resolve => setTimeout(resolve, 0));
    }
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
                scroll_region: None,
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
                    scroll_region: None,
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
                scroll_region: None,
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
                scroll_region: None,
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
                    scroll_region: None,
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
                scroll_region: None,
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

#[test]
#[ignore = "requires the local Chromium executable"]
fn clipped_options_and_text_follow_the_visible_fold() {
    let _serial = REAL_BROWSER.lock().unwrap();
    let server = FixtureServer::with_body(include_str!(
        "../../../tests/browser/scroll-popup-list.html"
    ));
    let mut browser = launch_headless();
    browser.navigate(&server.url()).unwrap();
    let observed = browser.observe().unwrap();
    browser
        .perform(
            prepared(
                &observed,
                "Category",
                PreparedOperation::Click,
                None,
                None,
                1,
            ),
            &InputCancellation::default(),
        )
        .unwrap();
    let observed = browser.observe().unwrap();
    let options: Vec<_> = observed
        .elements
        .iter()
        .filter(|e| e.role == "option")
        .map(|e| e.name.as_str())
        .collect();
    assert_eq!(
        options,
        (1..=11).map(|i| format!("Option {i}")).collect::<Vec<_>>()
    );
    assert!(has_line(&observed, "Option 11"));
    for i in 12..=50 {
        assert!(!has_line(&observed, &format!("Option {i}")));
        assert!(
            !observed
                .covered_text
                .lines()
                .any(|line| line == format!("Option {i}"))
        );
    }
    browser.close().unwrap();
}

#[test]
#[ignore = "requires the local Chromium executable"]
fn clipping_respects_containing_blocks_and_text_intersection() {
    let _serial = REAL_BROWSER.lock().unwrap();
    let server = FixtureServer::with_body(
        r#"<!doctype html><style>
        body{margin:0}.clip{overflow:hidden;width:200px;height:30px}
        .row{height:20px;margin:0;display:block} .fixed{position:fixed;top:200px}
        .absolute{position:absolute;top:250px}
        </style><div class="clip"><button class="row">First</button><button class="row">Fold</button><button class="row">Clipped</button>
        <button class="fixed">Fixed</button><button class="absolute">Absolute</button><p>Invisible text</p></div>"#,
    );
    let mut browser = launch_headless();
    browser.navigate(&server.url()).unwrap();
    let observed = browser.observe().unwrap();
    let names: Vec<_> = observed.elements.iter().map(|e| e.name.as_str()).collect();
    assert_eq!(names, ["First", "Fixed", "Absolute"]);
    assert!(has_line(&observed, "Fold"));
    assert!(!has_line(&observed, "Clipped"));
    assert!(!observed.visible_text.contains("Invisible text"));
    assert!(!observed.covered_text.contains("Invisible text"));

    for (page, absent, retained) in [
        (
            r#"<!doctype html><style>body{margin:0;overflow:hidden}button,span{position:fixed;width:100px;height:20px}</style><button style="left:-200px;top:40px">LEFT ROOT GOAL</button><button style="left:40px;top:-100px">ABOVE ROOT GOAL</button><span style="left:-200px;top:40px">LEFT ROOT TEXT</span><span style="left:40px;top:-100px">ABOVE ROOT TEXT</span>"#,
            "ROOT TEXT",
            None,
        ),
        (
            r#"<!doctype html><style>body{margin:0}#clip{width:80px;height:20px;overflow:hidden;font:12px monospace;line-height:20px}</style><div id="clip">FIRST SECOND THIRD FOURTH FIFTH</div>"#,
            "",
            Some("FIRST SECOND THIRD FOURTH FIFTH"),
        ),
        (
            r#"<!doctype html><style>body{margin:0}#clip{position:relative;width:300px;height:100px;overflow:hidden}span{position:absolute;left:20px;top:100px;white-space:nowrap;font:12px monospace;line-height:12px}</style><div id="clip"><span id="text">TANGENT TEXT</span></div><script>let range=document.createRange();range.selectNodeContents(text);let measuredTop=range.getBoundingClientRect().top;let edge=clip.getBoundingClientRect().bottom;text.style.top=(100+edge-measuredTop)+'px';const out=document.createElement('output');out.textContent='Range edges: '+range.getBoundingClientRect().top+' / '+edge;document.body.append(out);</script>"#,
            "TANGENT TEXT",
            Some("Range edges: 100 / 100"),
        ),
    ] {
        let server = FixtureServer::with_body(page);
        browser.navigate(&server.url()).unwrap();
        let observed = browser.observe().unwrap();
        assert!(observed.elements.is_empty());
        if !absent.is_empty() {
            assert!(
                !observed.visible_text.contains(absent),
                "{}",
                observed.visible_text
            );
            assert!(!observed.covered_text.contains(absent));
        }
        if let Some(retained) = retained {
            assert!(
                observed.visible_text.contains(retained),
                "{}",
                observed.visible_text
            );
        }
    }
    browser.close().unwrap();
}

fn prepared_scroll(observed: &Observation, up: bool, sequence: u64) -> PreparedInput {
    PreparedInput {
        document_id: observed.document_id.clone(),
        node_id: 0,
        operation: if up {
            PreparedOperation::ScrollUp
        } else {
            PreparedOperation::ScrollDown
        },
        text: None,
        previous_text: None,
        option_node_id: None,
        combobox: false,
        action_sequence: sequence,
        focus_anchor: None,
        scroll_region: Some(observed.scroll_regions[0].clone()),
    }
}

#[test]
#[ignore = "requires the local Chromium executable"]
fn region_wheel_moves_the_app_shell_table_and_keeps_the_document_still() {
    let _serial = REAL_BROWSER.lock().unwrap();
    let server = FixtureServer::with_body(include_str!(
        "../../../tests/browser/scroll-app-shell-table.html"
    ));
    let mut browser = launch_headless();
    browser.navigate(&server.url()).unwrap();
    let observed = browser.observe().unwrap();
    assert_eq!(observed.viewport.scroll_y, 0.0);
    assert_eq!(observed.scroll_regions.len(), 1);
    let region = &observed.scroll_regions[0];
    assert_eq!(region.name, "Localities");
    assert!(region.can_scroll_down);
    assert!(!region.can_scroll_up);
    browser
        .perform(
            prepared_scroll(&observed, false, 1),
            &InputCancellation::default(),
        )
        .unwrap();
    let after = browser.observe().unwrap();
    assert!(after.scroll_regions[0].scroll_top > 0.0);
    assert_eq!(after.viewport.scroll_y, 0.0);
    browser.close().unwrap();
}

#[test]
#[ignore = "requires the local Chromium executable"]
fn region_wheel_rejects_covered_and_disconnected_regions() {
    let _serial = REAL_BROWSER.lock().unwrap();
    let body = r#"<!doctype html><style>body{margin:0;overflow:hidden}.region{height:150px;width:200px;overflow:auto}.cover{position:fixed;inset:0;background:white;z-index:10}</style><div class="region" aria-label="Rows"><div style="height:1000px">Rows</div></div><div class="cover"><button onclick="document.querySelector('.region').remove()">Remove region</button></div>"#;
    let server = FixtureServer::with_body(body);
    let mut browser = launch_headless();
    browser.navigate(&server.url()).unwrap();
    let observed = browser.observe().unwrap();
    let result = browser.perform(
        prepared_scroll(&observed, false, 1),
        &InputCancellation::default(),
    );
    assert!(matches!(result,Err(PerformError::Rejected(reason)) if reason=="covered"));
    assert_eq!(browser.observe().unwrap().scroll_regions[0].scroll_top, 0.0);
    browser
        .perform(
            prepared(
                &observed,
                "Remove region",
                PreparedOperation::Click,
                None,
                None,
                2,
            ),
            &InputCancellation::default(),
        )
        .unwrap();
    assert!(
        matches!(browser.perform(prepared_scroll(&observed,false,3),&InputCancellation::default()),Err(PerformError::Rejected(reason)) if reason=="target_missing")
    );
    browser.close().unwrap();
}

#[test]
#[ignore = "requires the local Chromium executable"]
fn wheel_uses_the_in_window_height_and_exposes_every_row_center() {
    let _serial = REAL_BROWSER.lock().unwrap();
    let server = FixtureServer::with_body(
        r#"<!doctype html><style>body{margin:0;overflow:hidden}#region{position:absolute;top:600px;left:20px;height:300px;width:200px;overflow:auto}button{display:block;height:30px;width:100px;margin:0}</style><div id="region" aria-label="Rows"></div><textarea style="height:20px">long
text
on
many
lines</textarea><script>for(let i=1;i<=30;i++){const b=document.createElement('button');b.textContent='Row '+i;region.append(b)}const tail=document.createElement('div');tail.style.height='120px';region.append(tail);</script>"#,
    );
    let mut browser = launch_headless();
    browser.navigate(&server.url()).unwrap();
    let mut seen = std::collections::BTreeSet::new();
    for sequence in 1..=12 {
        let observed = browser.observe().unwrap();
        assert_eq!(observed.scroll_regions.len(), 1);
        let region = &observed.scroll_regions[0];
        assert_eq!(region.rect.height, 180.0);
        for element in &observed.elements {
            if element.name.starts_with("Row ") {
                seen.insert(element.name.clone());
            }
        }
        if !region.can_scroll_down {
            break;
        }
        let before = region.scroll_top;
        browser
            .perform(
                prepared_scroll(&observed, false, sequence),
                &InputCancellation::default(),
            )
            .unwrap();
        let after = browser.observe().unwrap();
        assert!(after.scroll_regions[0].scroll_top - before <= 173.0);
        assert_eq!(after.viewport.scroll_y, 0.0);
    }
    assert_eq!(seen, (1..=30).map(|i| format!("Row {i}")).collect());
    browser.close().unwrap();
}

#[test]
#[ignore = "requires the local Chromium executable"]
fn region_end_tolerance_excludes_the_last_fractional_pixel() {
    let _serial = REAL_BROWSER.lock().unwrap();
    let server = FixtureServer::with_body(
        r#"<!doctype html><div id="region" aria-label="Rows" style="height:100px;width:200px;overflow:auto"><div style="height:200.5px">Content</div></div><script>region.scrollTop=region.scrollHeight-region.clientHeight-1;</script>"#,
    );
    let mut browser = launch_headless();
    browser.navigate(&server.url()).unwrap();
    let observed = browser.observe().unwrap();
    let region = &observed.scroll_regions[0];
    assert!(region.can_scroll_up);
    assert!(!region.can_scroll_down);
    assert!(
        matches!(browser.perform(prepared_scroll(&observed,false,1),&InputCancellation::default()),Err(PerformError::Rejected(reason)) if reason=="scroll_region_at_end")
    );
    browser.close().unwrap();
}

#[test]
#[ignore = "requires the local Chromium executable"]
fn overlays_include_radix_shapes_and_exclude_document_disclosures_and_banners() {
    let _serial = REAL_BROWSER.lock().unwrap();
    let server = FixtureServer::with_body(include_str!(
        "../../../tests/browser/scroll-overlay-variants.html"
    ));
    let mut browser = launch_headless();
    for (mode, overlay) in [
        ("select", true),
        ("radix", true),
        ("banner", false),
        ("menu", false),
        ("accordion", false),
    ] {
        browser
            .navigate(&format!("{}?mode={mode}", server.url()))
            .unwrap();
        let observed = browser.observe().unwrap();
        assert_eq!(observed.overlay.is_some(), overlay, "{mode}");
        assert_eq!(
            observed.scroll_regions[0].overlay.is_some(),
            overlay,
            "{mode}"
        );
        if overlay {
            assert_eq!(observed.overlay.unwrap().name, "Choices");
        }
    }
    browser.close().unwrap();
}

#[test]
#[ignore = "requires the local Chromium executable"]
fn popup_wheel_moves_its_list_and_keeps_the_popup_open_over_a_scrollable_page() {
    let _serial = REAL_BROWSER.lock().unwrap();
    let server = FixtureServer::with_body(include_str!(
        "../../../tests/browser/scroll-popup-list.html"
    ));
    let mut browser = launch_headless();
    browser.navigate(&server.url()).unwrap();
    let observed = browser.observe().unwrap();
    browser
        .perform(
            prepared(
                &observed,
                "Category",
                PreparedOperation::Click,
                None,
                None,
                1,
            ),
            &InputCancellation::default(),
        )
        .unwrap();
    let observed = browser.observe().unwrap();
    assert_eq!(observed.overlay.as_ref().unwrap().name, "Choose category");
    assert_eq!(
        observed.scroll_regions[0].overlay.as_deref(),
        Some("Choose category")
    );
    browser
        .perform(
            prepared_scroll(&observed, false, 2),
            &InputCancellation::default(),
        )
        .unwrap();
    let after = browser.observe().unwrap();
    assert_eq!(after.viewport.scroll_y, 0.0);
    assert!(after.overlay.is_some());
    assert!(after.scroll_regions[0].scroll_top > 0.0);
    browser.close().unwrap();
}

#[test]
#[ignore = "requires the local Chromium executable"]
fn dialog_wheel_avoids_a_movable_inner_list_at_the_body_center() {
    let _serial = REAL_BROWSER.lock().unwrap();
    let server = FixtureServer::with_body(include_str!(
        "../../../tests/browser/scroll-dialog-body.html"
    ));
    let mut browser = launch_headless();
    browser
        .navigate(&format!("{}?inner", server.url()))
        .unwrap();
    let observed = browser.observe().unwrap();
    assert_eq!(observed.scroll_regions.len(), 2);
    assert_eq!(observed.scroll_regions[0].name, "Review terms");
    browser
        .perform(
            prepared_scroll(&observed, false, 1),
            &InputCancellation::default(),
        )
        .unwrap();
    let after = browser.observe().unwrap();
    assert!(after.scroll_regions[0].scroll_top > 0.0);
    if let Some(inner) = after.scroll_regions.iter().find(|r| r.name == "Inner list") {
        assert_eq!(inner.scroll_top, 0.0);
    }
    assert_eq!(after.viewport.scroll_y, 0.0);
    browser.close().unwrap();
}

#[test]
#[ignore = "requires the local Chromium executable"]
fn scroll_readback_distinguishes_movement_no_effect_and_dialog_chaining() {
    let _serial = REAL_BROWSER.lock().unwrap();
    let server = FixtureServer::with_body(include_str!(
        "../../../tests/browser/scroll-app-shell-table.html"
    ));
    let mut browser = launch_headless();
    for cancel in [false, true] {
        browser
            .navigate(&format!(
                "{}{}",
                server.url(),
                if cancel { "?cancel-wheel" } else { "" }
            ))
            .unwrap();
        let observed = browser.observe().unwrap();
        let fact = browser
            .perform(
                prepared_scroll(&observed, false, 1),
                &InputCancellation::default(),
            )
            .unwrap();
        assert_eq!(fact.scroll_readback.len(), 2);
        let region = &fact.scroll_readback[0];
        assert_eq!(region.name.as_deref(), Some("Localities"));
        assert!(!region.document);
        assert_eq!(region.before == region.after, cancel);
        let document = &fact.scroll_readback[1];
        assert!(document.document);
        assert!(document.name.is_none());
        assert_eq!(document.before, document.after);
    }
    let dialog = FixtureServer::with_body(include_str!(
        "../../../tests/browser/scroll-dialog-body.html"
    ));
    browser
        .navigate(&format!("{}?inner&inner-end", dialog.url()))
        .unwrap();
    let observed = browser.observe().unwrap();
    assert!(!observed.scroll_regions[1].can_scroll_down);
    let fact = browser
        .perform(
            prepared_scroll(&observed, false, 2),
            &InputCancellation::default(),
        )
        .unwrap();
    assert_eq!(
        fact.scroll_readback[0].name.as_deref(),
        Some("Review terms")
    );
    assert!(fact.scroll_readback[0].after > fact.scroll_readback[0].before);
    assert!(fact.scroll_readback.last().unwrap().document);
    browser.close().unwrap();
}

fn framed_scroll_page(frame_height: u32, region_height: u32) -> &'static str {
    let child = format!(r#"<!doctype html><style>body{{margin:0}}#region{{height:{region_height}px;width:180px;overflow:auto}}button{{display:block;height:30px;width:150px;margin:0}}</style><div role="dialog" aria-modal="true" aria-label="Frame dialog" style="height:180px"><div id="region" aria-label="Frame rows"></div></div><script>for(let i=1;i<=40;i++){{const b=document.createElement('button');b.textContent='Row '+i;region.append(b)}}const tail=document.createElement('div');tail.style.height='500px';region.append(tail);</script>"#).replace('"', "&quot;");
    Box::leak(format!(r#"<!doctype html><style>body{{margin:0;height:2000px}}iframe{{position:absolute;left:20px;top:20px;width:220px;height:{frame_height}px;border:0}}#cover{{display:none;position:fixed;left:20px;top:20px;width:220px;height:{frame_height}px;background:white;z-index:10}}.control{{position:absolute;top:300px}}</style><iframe id="frame" srcdoc="{child}"></iframe><div id="cover"></div><button class="control" onclick="frame.style.left='600px'">Move frame</button><button class="control" style="left:200px" onclick="cover.style.display='block'">Cover frame</button>"#).into_boxed_str())
}

#[test]
#[ignore = "requires the local Chromium executable"]
fn region_wheel_revalidates_the_current_frame_chain_and_outer_hit_target() {
    let _serial = REAL_BROWSER.lock().unwrap();
    let server = FixtureServer::with_body(framed_scroll_page(200, 180));
    let mut browser = launch_headless();
    for (control, moved) in [("Move frame", true), ("Cover frame", false)] {
        browser.navigate(&server.url()).unwrap();
        let observed = browser.observe().unwrap();
        assert_eq!(observed.scroll_regions.len(), 1);
        browser
            .perform(
                prepared(&observed, control, PreparedOperation::Click, None, None, 1),
                &InputCancellation::default(),
            )
            .unwrap();
        let result = browser.perform(
            prepared_scroll(&observed, false, 2),
            &InputCancellation::default(),
        );
        if moved {
            result.unwrap();
        } else {
            assert!(matches!(result, Err(PerformError::Rejected(reason)) if reason == "covered"));
        }
        let after = browser.observe().unwrap();
        assert_eq!(after.viewport.scroll_y, 0.0);
        assert_eq!(after.scroll_regions[0].scroll_top > 0.0, moved);
    }
    browser.close().unwrap();
}

#[test]
#[ignore = "requires the local Chromium executable"]
fn frame_viewport_bounds_the_wheel_and_preserves_overlap_in_both_directions() {
    let _serial = REAL_BROWSER.lock().unwrap();
    let server = FixtureServer::with_body(framed_scroll_page(100, 600));
    let mut browser = launch_headless();
    browser.navigate(&server.url()).unwrap();
    let mut seen = std::collections::BTreeSet::new();
    for sequence in 1..=30 {
        let observed = browser.observe().unwrap();
        let region = &observed.scroll_regions[0];
        assert_eq!(region.rect.height, 100.0);
        for element in &observed.elements {
            if element.name.starts_with("Row ") {
                seen.insert(element.name.clone());
            }
        }
        if !region.can_scroll_down {
            break;
        }
        let before = region.scroll_top;
        browser
            .perform(
                prepared_scroll(&observed, false, sequence),
                &InputCancellation::default(),
            )
            .unwrap();
        let after = browser.observe().unwrap();
        assert!(after.scroll_regions[0].scroll_top - before <= 93.0);
        assert_eq!(after.viewport.scroll_y, 0.0);
    }
    assert_eq!(seen, (1..=40).map(|i| format!("Row {i}")).collect());
    let observed = browser.observe().unwrap();
    let before = observed.scroll_regions[0].scroll_top;
    browser
        .perform(
            prepared_scroll(&observed, true, 31),
            &InputCancellation::default(),
        )
        .unwrap();
    let after = browser.observe().unwrap();
    assert!((before - after.scroll_regions[0].scroll_top - 92.0).abs() <= 1.0);
    assert_eq!(after.viewport.scroll_y, 0.0);
    browser.close().unwrap();
}

#[test]
#[ignore = "requires the local Chromium executable"]
fn stale_region_document_is_rejected() {
    let _serial = REAL_BROWSER.lock().unwrap();
    let server = FixtureServer::with_body(
        r#"<!doctype html><div aria-label="Rows" style="height:100px;width:200px;overflow:auto"><div style="height:1000px">Rows</div></div>"#,
    );
    let mut browser = launch_headless();
    browser.navigate(&server.url()).unwrap();
    let observed = browser.observe().unwrap();
    let mut input = prepared_scroll(&observed, false, 1);
    input.document_id = "old-document".into();
    assert!(
        matches!(browser.perform(input,&InputCancellation::default()),Err(PerformError::Rejected(reason)) if reason=="document_changed")
    );
    browser.close().unwrap();
}
#[test]
#[ignore = "requires the local Chromium executable"]
fn native_popover_is_scroll_scope() {
    let _serial = REAL_BROWSER.lock().unwrap();
    let server = FixtureServer::with_body(
        r#"<!doctype html><body style="height:3000px"><div id="popup" popover aria-label="Native choices"><div aria-label="Choices" style="height:100px;width:200px;overflow:auto"><div style="height:1000px">Rows</div></div></div><script>popup.showPopover()</script></body>"#,
    );
    let mut browser = launch_headless();
    browser.navigate(&server.url()).unwrap();
    let observed = browser.observe().unwrap();
    assert_eq!(
        observed.overlay.as_ref().map(|o| o.name.as_str()),
        Some("Native choices")
    );
    assert_eq!(
        observed.scroll_regions[0].overlay.as_deref(),
        Some("Native choices")
    );
    browser.close().unwrap();
}
#[test]
#[ignore = "requires the local Chromium executable"]
fn last_visible_overlay_owns_scope() {
    let _serial = REAL_BROWSER.lock().unwrap();
    let server = FixtureServer::with_body(
        r#"<!doctype html><div role="dialog" aria-modal="true" aria-label="Outer" style="position:fixed;left:20px;top:20px;width:400px;height:600px;background:white"><div role="dialog" aria-modal="true" aria-label="Inner" style="position:absolute;left:20px;top:20px;width:150px;height:100px;background:gray">Inner</div></div>"#,
    );
    let mut browser = launch_headless();
    browser.navigate(&server.url()).unwrap();
    let observed = browser.observe().unwrap();
    assert_eq!(
        observed.overlay.as_ref().map(|o| o.name.as_str()),
        Some("Inner")
    );
    browser.close().unwrap();
}
#[test]
#[ignore = "requires the local Chromium executable"]
fn covered_overlay_is_excluded() {
    let _serial = REAL_BROWSER.lock().unwrap();
    let server = FixtureServer::with_body(
        r#"<!doctype html><body style="height:3000px"><div role="dialog" aria-modal="true" aria-label="Covered choices" style="position:fixed;left:20px;top:20px;width:400px;height:600px;background:white">Choices</div><div style="position:fixed;inset:0;background:white;z-index:10">Cover</div></body>"#,
    );
    let mut browser = launch_headless();
    browser.navigate(&server.url()).unwrap();
    let observed = browser.observe().unwrap();
    assert!(observed.overlay.is_none());
    browser.close().unwrap();
}
#[test]
#[ignore = "requires the local Chromium executable"]
fn readback_rejects_detached_target() {
    let _serial = REAL_BROWSER.lock().unwrap();
    let server = FixtureServer::with_body(
        r#"<!doctype html><div id="region" aria-label="Rows" style="height:100px;width:200px;overflow:auto"><div style="height:1000px">Rows</div></div><script>region.addEventListener('wheel',e=>{e.preventDefault();region.remove()},{passive:false})</script>"#,
    );
    let mut browser = launch_headless();
    browser.navigate(&server.url()).unwrap();
    let observed = browser.observe().unwrap();
    assert!(matches!(
        browser.perform(
            prepared_scroll(&observed, false, 1),
            &InputCancellation::default()
        ),
        Err(PerformError::Uncertain(_))
    ));
    browser.close().unwrap();
}
#[test]
#[ignore = "requires the local Chromium executable"]
fn readback_waits_for_stable_frames() {
    let _serial = REAL_BROWSER.lock().unwrap();
    let server = FixtureServer::with_body(
        r#"<!doctype html><div id="region" aria-label="Rows" style="height:100px;width:200px;overflow:auto"><div style="height:1000px">Rows</div></div><script>region.addEventListener('wheel',e=>{e.preventDefault();let n=0;const move=()=>{region.scrollTop+=50;if(++n<4)requestAnimationFrame(move)};requestAnimationFrame(move)},{passive:false})</script>"#,
    );
    let mut browser = launch_headless();
    browser.navigate(&server.url()).unwrap();
    let observed = browser.observe().unwrap();
    let fact = browser
        .perform(
            prepared_scroll(&observed, false, 1),
            &InputCancellation::default(),
        )
        .unwrap();
    assert_eq!(fact.scroll_readback[0].after, 200.0);
    browser.close().unwrap();
}
#[test]
#[ignore = "requires the local Chromium executable"]
fn window_distance_remains_three_quarters_height() {
    let _serial = REAL_BROWSER.lock().unwrap();
    let server =
        FixtureServer::with_body(r#"<!doctype html><body style="height:3000px">Page</body>"#);
    let mut browser = launch_headless();
    browser.navigate(&server.url()).unwrap();
    let observed = browser.observe().unwrap();
    let input = PreparedInput {
        document_id: observed.document_id.clone(),
        node_id: 0,
        operation: PreparedOperation::ScrollDown,
        text: None,
        previous_text: None,
        option_node_id: None,
        combobox: false,
        action_sequence: 1,
        focus_anchor: None,
        scroll_region: None,
    };
    browser
        .perform(input, &InputCancellation::default())
        .unwrap();
    assert_eq!(browser.observe().unwrap().viewport.scroll_y, 585.0);
    browser.close().unwrap();
}

#[test]
#[ignore = "requires the local Chromium executable"]
fn scroll_region_snapshot_bounds_names_lists_and_fractional_state() {
    let _serial = REAL_BROWSER.lock().unwrap();
    let server = FixtureServer::with_body(
        r#"<!doctype html><style>body{margin:0}#regions{display:flex;flex-wrap:wrap;width:600px}.region{height:20px;width:100px;overflow:auto}.content{height:100px}</style><div style="height:20px;overflow:visible"><div class="content">Not user scrollable</div></div><div style="height:20px;overflow:auto">Fits</div><div class="region" aria-hidden="true"><div class="content">Hidden</div></div><div id="regions"></div><script>for(let i=0;i<25;i++){const e=document.createElement('div');e.className='region';if(i===0)e.setAttribute('aria-label','A'.repeat(150));e.innerHTML='<div class="content"></div>';regions.append(e)}regions.children[1].scrollTop=1;</script>"#,
    );
    let mut browser = launch_headless();
    browser.navigate(&server.url()).unwrap();
    let observed = browser.observe().unwrap();
    assert_eq!(observed.scroll_regions.len(), 20);
    assert!(observed.scroll_regions_truncated);
    assert_eq!(observed.scroll_regions[0].name, "A".repeat(120));
    assert_eq!(observed.scroll_regions[1].name, "Scrollable area");
    assert!(!observed.scroll_regions[1].can_scroll_up);
    assert!(observed.scroll_regions.iter().all(|r| r.can_scroll_down));
    browser.close().unwrap();
}

#[test]
#[ignore = "requires the local Chromium executable"]
fn nested_region_readback_captures_target_ancestors_and_document() {
    let _serial = REAL_BROWSER.lock().unwrap();
    let server = FixtureServer::with_body(
        r#"<!doctype html><style>body{margin:0}#outer{height:200px;width:200px;overflow:auto}#inner{height:100px;width:180px;overflow:auto}</style><div id="outer" aria-label="Outer rows"><div style="height:120px"></div><div id="inner" aria-label="Inner rows"><div style="height:1000px"></div></div><div style="height:1000px"></div></div><script>outer.scrollTop=100;inner.scrollTop=100;</script>"#,
    );
    let mut browser = launch_headless();
    browser.navigate(&server.url()).unwrap();
    let observed = browser.observe().unwrap();
    assert_eq!(
        observed.scroll_regions[1].parent_node_id,
        Some(observed.scroll_regions[0].node_id)
    );
    let mut input = prepared_scroll(&observed, false, 1);
    input.scroll_region = Some(observed.scroll_regions[1].clone());
    let fact = browser
        .perform(input, &InputCancellation::default())
        .unwrap();
    assert_eq!(fact.scroll_readback.len(), 3);
    assert_eq!(fact.scroll_readback[0].name.as_deref(), Some("Inner rows"));
    assert_eq!(fact.scroll_readback[0].before, 100.0);
    assert!(fact.scroll_readback[0].after > 100.0);
    assert_eq!(fact.scroll_readback[1].name.as_deref(), Some("Outer rows"));
    assert_eq!(fact.scroll_readback[1].before, 100.0);
    assert_eq!(fact.scroll_readback[1].after, 100.0);
    assert!(fact.scroll_readback[2].document);
    assert_eq!(fact.scroll_readback[2].before, 0.0);
    assert_eq!(fact.scroll_readback[2].after, 0.0);
    browser.close().unwrap();
}

#[test]
#[ignore = "requires the local Chromium executable"]
fn native_modal_owns_scroll_scope_without_outside_inert_attributes() {
    let _serial = REAL_BROWSER.lock().unwrap();
    let server = FixtureServer::with_body(
        r#"<!doctype html><button>Outside</button><dialog id="modal" aria-label="Native modal"><div aria-label="Rows" style="height:100px;width:200px;overflow:auto"><div style="height:1000px">Rows</div></div></dialog><script>modal.showModal()</script>"#,
    );
    let mut browser = launch_headless();
    browser.navigate(&server.url()).unwrap();
    let observed = browser.observe().unwrap();
    assert_eq!(
        observed.overlay.as_ref().map(|o| o.name.as_str()),
        Some("Native modal")
    );
    browser.close().unwrap();
}

#[test]
#[ignore = "requires the local Chromium executable"]
fn readback_rejects_a_replaced_frame_document_after_the_wheel() {
    let _serial = REAL_BROWSER.lock().unwrap();
    let body = framed_scroll_page(200,180).replace("region.append(tail);", "region.append(tail);region.addEventListener('wheel',()=>frameElement.srcdoc='<p>Replacement</p>',{once:true});");
    let server = FixtureServer::with_body(Box::leak(body.into_boxed_str()));
    let mut browser = launch_headless();
    browser.navigate(&server.url()).unwrap();
    let observed = browser.observe().unwrap();
    assert!(matches!(
        browser.perform(
            prepared_scroll(&observed, false, 1),
            &InputCancellation::default()
        ),
        Err(PerformError::Uncertain(_))
    ));
    browser.close().unwrap();
}

#[test]
#[ignore = "requires the local Chromium executable"]
fn readback_rejects_stale_capture_identity_and_never_settled_positions() {
    let _serial = REAL_BROWSER.lock().unwrap();
    let mut browser = launch_headless();
    for reaction in [
        "window.__manuvra.scrollReadback.documentId='old-document'",
        "const move=()=>{region.scrollTop=region.scrollTop===100?101:100;requestAnimationFrame(move)};requestAnimationFrame(move)",
    ] {
        let body = format!(
            r#"<!doctype html><div id="region" aria-label="Rows" style="height:100px;width:200px;overflow:auto"><div style="height:1000px">Rows</div></div><script>region.addEventListener('wheel',e=>{{e.preventDefault();{reaction}}},{{passive:false}})</script>"#
        );
        let server = FixtureServer::with_body(Box::leak(body.into_boxed_str()));
        browser.navigate(&server.url()).unwrap();
        let observed = browser.observe().unwrap();
        assert!(
            matches!(
                browser.perform(
                    prepared_scroll(&observed, false, 1),
                    &InputCancellation::default()
                ),
                Err(PerformError::Uncertain(_))
            ),
            "{reaction}"
        );
    }
    browser.close().unwrap();
}

#[test]
#[ignore = "requires the local Chromium executable"]
fn direct_text_clips_to_its_own_parent_overflow_box() {
    let _serial = REAL_BROWSER.lock().unwrap();
    let server = FixtureServer::with_body(
        r#"<!doctype html><style>body{margin:0}.region{height:60px;width:300px;overflow:auto;line-height:20px}</style><div class="region"><div style="height:100px">Visible block</div>CLIPPED SENTINEL</div><div class="region" aria-hidden="true"><div style="height:100px">Covered block</div>COVERED SENTINEL</div>"#,
    );
    let mut browser = launch_headless();
    browser.navigate(&server.url()).unwrap();
    let observed = browser.observe().unwrap();
    assert!(observed.visible_text.contains("Visible block"));
    assert!(!observed.visible_text.contains("CLIPPED SENTINEL"));
    assert!(!observed.covered_text.contains("COVERED SENTINEL"));
    browser.close().unwrap();
}

#[test]
#[ignore = "requires the local Chromium executable"]
fn scaled_frame_wheel_reaches_the_selected_region_and_reports_its_movement() {
    let _serial = REAL_BROWSER.lock().unwrap();
    let child = r#"<!doctype html><style>body{margin:0}#target,#wrong{position:absolute;left:0;height:150px;overflow:auto;scrollbar-width:none}#target{top:100px;width:500px}#wrong{top:300px;width:600px}</style><div id="target" aria-label="Target"><div style="height:1000px">Rows</div></div><textarea id="wrong" aria-label="Wrong"></textarea><output id="result">Wrong: 0</output><script>wrong.value='Wrong\n'.repeat(100);wrong.addEventListener('scroll',()=>result.textContent='Wrong: '+wrong.scrollTop)</script>"#.replace('"',"&quot;");
    let mut browser = launch_headless();
    for (frame_transform, ancestor_transform) in [
        ("scale(.5)", "none"),
        ("none", "scale(.5)"),
        ("rotate(20deg) scale(.5)", "none"),
        ("scale(2)", "none"),
        ("none;rotate:20deg;scale:.5", "none"),
        ("none;scale:.5", "none"),
        ("none;scale:.5 1", "none"),
        ("none;zoom:.5", "none"),
        ("scale3d(.5,.5,.5)", "none"),
        ("none;scale:.5 .5 .5", "none"),
        (
            "rotate(20deg) scale(.5);padding:50px;border:20px solid black",
            "none",
        ),
        ("rotate(-20deg) scale(.5)", "none"),
        (
            "rotate(-20deg) scale(.5);padding:50px;border:20px solid black",
            "none",
        ),
        ("rotate(20deg);scale:.5 1", "none"),
    ] {
        let body = format!(
            r#"<!doctype html><style>body{{margin:0;overflow:hidden}}#wrapper{{transform:{ancestor_transform};transform-origin:top left}}iframe{{width:600px;height:600px;border:0;transform:{frame_transform};transform-origin:top left}}</style><div id="wrapper"><iframe srcdoc="{child}"></iframe></div>"#
        );
        let server = FixtureServer::with_body(Box::leak(body.into_boxed_str()));
        browser.navigate(&server.url()).unwrap();
        let observed = browser.observe().unwrap();
        assert_eq!(observed.scroll_regions.len(), 1);
        if frame_transform == "scale(.5)" && ancestor_transform == "none" {
            assert_eq!(observed.scroll_regions[0].rect.width, 250.0);
            assert_eq!(observed.scroll_regions[0].rect.height, 75.0);
        }
        if frame_transform == "rotate(20deg) scale(.5)" {
            // The main viewport cuts the rotated top-left corner at x=0.
            let angle = 20_f64.to_radians();
            let height = (500.0 * angle.sin() + 250.0 * angle.cos()) / 2.0 - 50.0 / angle.cos();
            assert!(
                (observed.scroll_regions[0].rect.height - height).abs() < 0.01,
                "{:?}",
                observed.scroll_regions[0].rect
            );
        }
        if frame_transform.contains("padding:50px") {
            let angle = 20_f64.to_radians();
            let expected_y = if frame_transform.starts_with("rotate(-") {
                0.0
            } else {
                (70.0 * angle.sin() + 170.0 * angle.cos()) / 2.0
            };
            assert!(
                (observed.scroll_regions[0].rect.y - expected_y).abs() < 0.01,
                "{:?}",
                observed.scroll_regions[0].rect
            );
            if frame_transform.starts_with("rotate(-") {
                let expected_x = (70.0 * angle.cos() + 170.0 * angle.sin()) / 2.0;
                assert!((observed.scroll_regions[0].rect.x - expected_x).abs() < 0.01);
                assert!(
                    (observed.scroll_regions[0].rect.height
                        - (320.0 * angle.cos() - 70.0 * angle.sin()) / 2.0)
                        .abs()
                        < 0.01
                );
            }
        }
        let fact = browser
            .perform(
                prepared_scroll(&observed, false, 1),
                &InputCancellation::default(),
            )
            .unwrap();
        assert!(
            (fact.scroll_readback[0].after - 142.0).abs() <= 1.0,
            "{frame_transform} / {ancestor_transform}: {fact:?}"
        );
        let after = browser.observe().unwrap();
        assert!(after.visible_text.contains("Wrong: 0"));
        assert_eq!(after.viewport.scroll_y, 0.0);
    }
    browser.close().unwrap();
}

#[test]
#[ignore = "requires the local Chromium executable"]
fn open_shadow_scroll_reaches_the_region() {
    let _serial = REAL_BROWSER.lock().unwrap();
    let server = FixtureServer::with_body(
        r#"<!doctype html><div id="host"></div><script>host.attachShadow({mode:'open'}).innerHTML='<div aria-label="Shadow rows" style="height:100px;width:200px;overflow:auto"><div style="height:1000px">Rows</div></div>'</script>"#,
    );
    let mut browser = launch_headless();
    browser.navigate(&server.url()).unwrap();
    let observed = browser.observe().unwrap();
    assert_eq!(observed.scroll_regions.len(), 1);
    browser
        .perform(
            prepared_scroll(&observed, false, 1),
            &InputCancellation::default(),
        )
        .unwrap();
    assert!(browser.observe().unwrap().scroll_regions[0].scroll_top > 0.0);
    browser.close().unwrap();
}
#[test]
#[ignore = "requires the local Chromium executable"]
fn nested_frame_scroll_checks_every_outer_hit() {
    let _serial = REAL_BROWSER.lock().unwrap();
    let child=r#"<!doctype html><body style="margin:0"><div aria-label="Nested rows" style="height:100px;width:180px;overflow:auto"><div style="height:1000px">Rows</div></div>"#.replace('"',"&quot;");
    let parent=format!(r#"<!doctype html><body style="margin:0"><iframe style="position:absolute;left:30px;top:30px;width:200px;height:120px;border:0" srcdoc="{child}"></iframe>"#).replace('&',"&amp;").replace('"',"&quot;");
    let page = format!(
        r#"<!doctype html><iframe style="position:absolute;left:50px;top:50px;width:300px;height:200px;border:0" srcdoc="{parent}"></iframe><div style="position:absolute;left:50px;top:50px;width:300px;height:200px;background:white;z-index:20">Cover</div>"#
    );
    let server = FixtureServer::with_body(Box::leak(page.into_boxed_str()));
    let mut browser = launch_headless();
    browser.navigate(&server.url()).unwrap();
    let observed = browser.observe().unwrap();
    assert_eq!(observed.scroll_regions.len(), 1);
    assert!(matches!(
        browser.perform(
            prepared_scroll(&observed, false, 1),
            &InputCancellation::default()
        ),
        Err(PerformError::Rejected(_))
    ));
    browser.close().unwrap();
}
#[test]
#[ignore = "requires the local Chromium executable"]
fn frame_width_bounds_region_geometry() {
    let _serial = REAL_BROWSER.lock().unwrap();
    let body = framed_scroll_page(200, 180).replace("width:180px", "width:600px");
    let server = FixtureServer::with_body(Box::leak(body.into_boxed_str()));
    let mut browser = launch_headless();
    browser.navigate(&server.url()).unwrap();
    let observed = browser.observe().unwrap();
    assert_eq!(observed.scroll_regions.len(), 1);
    assert_eq!(observed.scroll_regions[0].rect.width, 220.0);
    browser.close().unwrap();
}
#[test]
#[ignore = "requires the local Chromium executable"]
fn covered_frame_does_not_publish_child_overlay() {
    let _serial = REAL_BROWSER.lock().unwrap();
    let child=r#"<!doctype html><body style="margin:0"><div role="dialog" aria-modal="true" aria-label="Covered nested choices" style="height:100px;width:180px;background:white">Choices</div>"#.replace('"',"&quot;");
    let body = format!(
        r#"<!doctype html><iframe style="position:absolute;left:50px;top:50px;width:300px;height:200px;border:0" srcdoc="{child}"></iframe><div style="position:fixed;left:50px;top:50px;width:300px;height:200px;z-index:10;background:white">Cover</div>"#
    );
    let server = FixtureServer::with_body(Box::leak(body.into_boxed_str()));
    let mut browser = launch_headless();
    browser.navigate(&server.url()).unwrap();
    assert!(browser.observe().unwrap().overlay.is_none());
    browser.close().unwrap();
}
#[test]
#[ignore = "requires the local Chromium executable"]
fn zero_height_region_rejects_wheel() {
    let _serial = REAL_BROWSER.lock().unwrap();
    let server = FixtureServer::with_body(
        r#"<!doctype html><div id="region" aria-label="Rows" style="height:100px;width:200px;overflow:auto;border:10px solid black"><div style="height:1000px">Rows</div></div><button onclick="region.style.height='0px'">Hide rows</button>"#,
    );
    let mut browser = launch_headless();
    browser.navigate(&server.url()).unwrap();
    let observed = browser.observe().unwrap();
    browser
        .perform(
            prepared(
                &observed,
                "Hide rows",
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
            prepared_scroll(&observed, false, 2),
            &InputCancellation::default()
        ),
        Err(PerformError::Rejected(_))
    ));
    browser.close().unwrap();
}

#[test]
#[ignore = "requires the local Chromium executable"]
fn an_offscreen_frame_has_no_scroll_region_or_visible_child_text() {
    let _serial = REAL_BROWSER.lock().unwrap();
    let body = framed_scroll_page(200, 180).replace(
        "border:0",
        "border:0;transform:rotate(20deg);transform-origin:top left",
    ) + "<script>frame.style.left=(innerWidth+70)+'px'</script>";
    let server = FixtureServer::with_body(Box::leak(body.into_boxed_str()));
    let mut browser = launch_headless();
    browser.navigate(&server.url()).unwrap();
    let observed = browser.observe().unwrap();
    assert!(observed.scroll_regions.is_empty());
    assert!(!observed.visible_text.contains("Row "));
    assert!(!observed.elements.iter().any(|e| e.name.starts_with("Row ")));
    browser.close().unwrap();
}

#[test]
#[ignore = "requires the local Chromium executable"]
fn unverifiable_frame_geometry_cannot_publish_window_scroll_authority() {
    let _serial = REAL_BROWSER.lock().unwrap();
    let mut browser = launch_headless();
    for css in [
        "transform:rotateY(30deg)",
        "perspective:500px",
        "rotate:x 20deg",
        "transform:perspective(500px)",
        "transform:scale(0)",
    ] {
        let body = framed_scroll_page(200, 180).replace("border:0", &format!("border:0;{css}"));
        let server = FixtureServer::with_body(Box::leak(body.into_boxed_str()));
        browser.navigate(&server.url()).unwrap();
        assert!(browser.observe().is_err(), "{css}");
    }
    browser.close().unwrap();
}

#[test]
#[ignore = "requires the local Chromium executable"]
fn transformed_region_clipping_uses_client_dimensions_in_the_same_coordinate_space() {
    let _serial = REAL_BROWSER.lock().unwrap();
    let mut browser = launch_headless();
    for (transform, expected_height) in [
        ("scale(.5)", 75.0),
        ("scale(2)", 300.0),
        ("none;zoom:.5", 75.0),
        ("none;scale:.5", 75.0),
        ("scale3d(.5,.5,.5)", 75.0),
        ("none;scale:.5 .5 .5", 75.0),
    ] {
        let body = format!(
            r#"<!doctype html><style>body{{margin:0;overflow:hidden}}#region{{position:absolute;top:40px;left:40px;width:300px;height:150px;border:10px solid black;scrollbar-width:none;overflow:auto;transform:{transform};transform-origin:top left}}button{{display:block;height:30px;width:200px;margin:0;line-height:30px;padding:0}}</style><div id="region" aria-label="Rows"></div><script>for(let i=1;i<=12;i++){{const b=document.createElement('button');b.textContent='Row '+i;region.append(b)}}</script>"#
        );
        let server = FixtureServer::with_body(Box::leak(body.into_boxed_str()));
        browser.navigate(&server.url()).unwrap();
        let observed = browser.observe().unwrap();
        let names: Vec<_> = observed.elements.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(
            names,
            ["Row 1", "Row 2", "Row 3", "Row 4", "Row 5"],
            "{transform}"
        );
        assert!(!observed.visible_text.contains("Row 6"));
        assert_eq!(observed.scroll_regions[0].rect.height, expected_height);
        let origin = if transform == "none;zoom:.5" {
            25.0
        } else if transform == "scale(2)" {
            60.0
        } else {
            45.0
        };
        assert_eq!(observed.scroll_regions[0].rect.x, origin);
        assert_eq!(observed.scroll_regions[0].rect.y, origin);
        assert_eq!(observed.scroll_regions[0].rect.width, expected_height * 2.0);
        let fact = browser
            .perform(
                prepared_scroll(&observed, false, 1),
                &InputCancellation::default(),
            )
            .unwrap();
        assert!((fact.scroll_readback[0].after - 142.0).abs() <= 1.0);
        assert_eq!(browser.observe().unwrap().viewport.scroll_y, 0.0);
    }
    browser.close().unwrap();
}

#[test]
#[ignore = "requires the local Chromium executable"]
fn nested_transformed_frames_compose_geometry_and_hit_coordinates() {
    let _serial = REAL_BROWSER.lock().unwrap();
    let child=r#"<!doctype html><body style="margin:0"><div aria-label="Nested rows" style="height:100px;width:180px;overflow:auto"><div style="height:1000px">Rows</div></div>"#.replace('"',"&quot;");
    let parent=format!(r#"<!doctype html><body style="margin:0"><iframe style="position:absolute;left:30px;top:30px;width:200px;height:120px;border:0;transform:rotate(20deg);transform-origin:top left" srcdoc="{child}"></iframe>"#).replace('&',"&amp;").replace('"',"&quot;");
    let page = format!(
        r#"<!doctype html><iframe style="position:absolute;left:600px;top:50px;width:300px;height:200px;border:0;transform:scale(.5);transform-origin:top left" srcdoc="{parent}"></iframe>"#
    );
    let server = FixtureServer::with_body(Box::leak(page.into_boxed_str()));
    let mut browser = launch_headless();
    browser.navigate(&server.url()).unwrap();
    let observed = browser.observe().unwrap();
    assert_eq!(observed.scroll_regions.len(), 1);
    let fact = browser
        .perform(
            prepared_scroll(&observed, false, 1),
            &InputCancellation::default(),
        )
        .unwrap();
    assert!((fact.scroll_readback[0].after - 92.0).abs() <= 1.0);
    assert_eq!(fact.scroll_readback.last().unwrap().after, 0.0);
    browser.close().unwrap();
}
#[test]
#[ignore = "requires the local Chromium executable"]
fn nested_frame_viewports_bound_both_region_dimensions() {
    let _serial = REAL_BROWSER.lock().unwrap();
    let child=r#"<!doctype html><body style="margin:0"><div aria-label="Nested rows" style="height:100px;width:180px;overflow:auto"><div style="height:1000px">Rows</div></div>"#.replace('"',"&quot;");
    let parent=format!(r#"<!doctype html><body style="margin:0"><iframe style="position:absolute;left:30px;top:30px;width:200px;height:120px;border:0" srcdoc="{child}"></iframe>"#).replace('&',"&amp;").replace('"',"&quot;");
    let page = format!(
        r#"<!doctype html><iframe style="position:absolute;left:50px;top:50px;width:100px;height:80px;border:0" srcdoc="{parent}"></iframe>"#
    );
    let server = FixtureServer::with_body(Box::leak(page.into_boxed_str()));
    let mut browser = launch_headless();
    browser.navigate(&server.url()).unwrap();
    let observed = browser.observe().unwrap();
    assert_eq!(observed.scroll_regions.len(), 1);
    assert_eq!(observed.scroll_regions[0].rect.width, 70.0);
    assert_eq!(observed.scroll_regions[0].rect.height, 50.0);

    let child=r#"<!doctype html><body style="margin:0"><div aria-label="Nested rows" style="height:100px;width:180px;overflow:auto"><button>OFF MAIN GOAL</button><div style="height:1000px">OFF MAIN TEXT</div></div>"#.replace('"',"&quot;");
    let parent=format!(r#"<!doctype html><body style="margin:0"><iframe style="position:absolute;left:30px;top:30px;width:200px;height:120px;border:0" srcdoc="{child}"></iframe>"#).replace('&',"&amp;").replace('"',"&quot;");
    let page = format!(
        r#"<!doctype html><iframe style="position:absolute;left:1100px;top:50px;width:100px;height:80px;border:0" srcdoc="{parent}"></iframe>"#
    );
    let server = FixtureServer::with_body(Box::leak(page.into_boxed_str()));
    browser.navigate(&server.url()).unwrap();
    let observed = browser.observe().unwrap();
    assert!(observed.scroll_regions.is_empty());
    assert!(!observed.visible_text.contains("OFF MAIN"));
    assert!(
        !observed
            .elements
            .iter()
            .any(|e| e.name.contains("OFF MAIN"))
    );
    browser.close().unwrap();
}

#[test]
#[ignore = "requires the local Chromium executable"]
fn transformed_visible_child_overlay_uses_global_hit_coordinates() {
    let _serial = REAL_BROWSER.lock().unwrap();
    let child=r#"<!doctype html><body style="margin:0"><div role="dialog" aria-modal="true" aria-label="Covered nested choices" style="position:absolute;top:100px;left:100px;height:100px;width:180px;background:white">Choices</div>"#.replace('"',"&quot;");
    let body = format!(
        r#"<!doctype html><iframe style="position:absolute;left:50px;top:50px;width:300px;height:200px;border:0;transform:scale(.5);transform-origin:top left" srcdoc="{child}"></iframe>"#
    );
    let server = FixtureServer::with_body(Box::leak(body.into_boxed_str()));
    let mut browser = launch_headless();
    browser.navigate(&server.url()).unwrap();
    assert!(browser.observe().unwrap().overlay.is_some());
    browser.close().unwrap();
}

#[test]
#[ignore = "requires the local Chromium executable"]
fn unverifiable_client_geometry_cannot_publish_scroll_authority() {
    let _serial = REAL_BROWSER.lock().unwrap();
    let mut browser = launch_headless();
    for css in [
        "transform:rotateY(30deg)",
        "perspective:500px",
        "rotate:x 20deg",
        "transform:perspective(500px)",
        "transform:scale(0)",
        "transform:matrix3d(1,0,0,0,0,1,0,0,0,0,1,0,0,0,0,2)",
    ] {
        let body = format!(
            r#"<!doctype html><div style="height:150px;width:300px;overflow:auto;{css}"><div style="height:1000px">Rows</div></div>"#
        );
        let server = FixtureServer::with_body(Box::leak(body.into_boxed_str()));
        browser.navigate(&server.url()).unwrap();
        assert!(browser.observe().is_err(), "{css}");
    }
    browser.close().unwrap();
}

#[test]
#[ignore = "requires the local Chromium executable"]
fn rotated_clipping_excludes_controls_and_text_outside_the_client_box() {
    let _serial = REAL_BROWSER.lock().unwrap();
    let mut browser = launch_headless();
    for angle in [30, 45, -30] {
        let body = format!(
            r#"<!doctype html><style>body{{margin:0;overflow:hidden}}#region{{position:absolute;left:400px;top:200px;width:300px;height:150px;overflow:auto;scrollbar-width:none;transform:rotate({angle}deg);transform-origin:top left}}button,span{{position:absolute;left:180px;width:90px;height:20px}}button{{font-size:8px}}span{{left:100px}}</style><div id="region" aria-label="Rotated"><div style="height:1000px"></div><button style="top:40px">VISIBLE GOAL</button><button style="top:170px">CLIPPED GOAL</button><span style="top:200px">CLIPPED TEXT</span><span aria-hidden="true" style="top:230px">CLIPPED COVERED</span><span style="top:140px">PARTLY VISIBLE TEXT</span><button style="left:260px;top:40px">RIGHT EDGE GOAL</button><button style="left:-80px;top:40px">LEFT EDGE GOAL</button><span style="left:-250px;top:40px">LEFT OUTSIDE TEXT</span><span style="left:400px;top:40px">RIGHT OUTSIDE TEXT</span><span style="top:-100px">ABOVE OUTSIDE TEXT</span></div>"#
        );
        let server = FixtureServer::with_body(Box::leak(body.into_boxed_str()));
        browser.navigate(&server.url()).unwrap();
        let observed = browser.observe().unwrap();
        assert_eq!(
            observed
                .elements
                .iter()
                .map(|e| e.name.as_str())
                .collect::<Vec<_>>(),
            ["VISIBLE GOAL"],
            "{angle}"
        );
        if angle != 45 {
            assert!(!observed.visible_text.contains("CLIPPED GOAL"), "{angle}");
        }
        assert!(!observed.visible_text.contains("CLIPPED TEXT"), "{angle}");
        for text in [
            "LEFT OUTSIDE TEXT",
            "RIGHT OUTSIDE TEXT",
            "ABOVE OUTSIDE TEXT",
        ] {
            assert!(!observed.visible_text.contains(text), "{angle}: {text}");
        }
        assert!(!observed.covered_text.contains("CLIPPED"), "{angle}");
        assert!(
            observed.visible_text.contains("PARTLY VISIBLE TEXT"),
            "{angle}"
        );
        let fact = browser
            .perform(
                prepared_scroll(&observed, false, 1),
                &InputCancellation::default(),
            )
            .unwrap();
        assert!((fact.scroll_readback[0].after - 142.0).abs() <= 1.0);
        assert!(
            browser
                .observe()
                .unwrap()
                .elements
                .iter()
                .any(|e| e.name == "CLIPPED GOAL")
        );
    }
    browser.close().unwrap();
}

#[test]
#[ignore = "requires the local Chromium executable"]
fn partially_visible_regions_keep_overlapping_views_at_small_heights() {
    let _serial = REAL_BROWSER.lock().unwrap();
    let mut browser = launch_headless();
    for (height, distance) in [(4, 2.0), (20, 12.0), (40, 32.0), (48, 40.0), (150, 142.0)] {
        let body = format!(
            r#"<!doctype html><style>body{{margin:0;overflow:hidden}}#region{{position:fixed;left:40px;top:calc(100vh - {height}px);width:200px;height:300px;overflow:auto}}button{{display:block;height:2px;width:150px;margin:0;padding:0;border:0;font-size:1px}}</style><div id="region" aria-label="Rows"></div><script>for(let i=1;i<=1000;i++){{let b=document.createElement('button');b.textContent='Option '+i;region.append(b)}}</script>"#
        );
        let server = FixtureServer::with_body(Box::leak(body.into_boxed_str()));
        browser.navigate(&server.url()).unwrap();
        let observed = browser.observe().unwrap();
        assert_eq!(observed.scroll_regions[0].rect.height, height as f64);
        let last_visible = observed.elements.last().unwrap().name.clone();
        let fact = browser
            .perform(
                prepared_scroll(&observed, false, 1),
                &InputCancellation::default(),
            )
            .unwrap();
        assert!(
            (fact.scroll_readback[0].after - distance).abs() <= 0.01,
            "height {height}: {fact:?}"
        );
        let after = browser.observe().unwrap();
        assert!(
            after.elements.iter().any(|e| e.name == last_visible),
            "height {height}"
        );
        assert_eq!(after.viewport.scroll_y, 0.0);
    }
    browser.close().unwrap();
}

#[test]
#[ignore = "requires the local Chromium executable"]
fn nested_zoomed_frames_accumulate_native_wheel_units() {
    let _serial = REAL_BROWSER.lock().unwrap();
    let child=r#"<!doctype html><body style="margin:0"><div aria-label="Nested rows" style="height:100px;width:180px;overflow:auto"><div style="height:1000px">Rows</div></div>"#.replace('"',"&quot;");
    let parent=format!(r#"<!doctype html><body style="margin:0"><iframe style="position:absolute;left:30px;top:30px;width:200px;height:120px;border:0;transform:none;zoom:.5;transform-origin:top left" srcdoc="{child}"></iframe>"#).replace('&',"&amp;").replace('"',"&quot;");
    let page = format!(
        r#"<!doctype html><iframe style="position:absolute;left:600px;top:50px;width:300px;height:200px;border:0;transform:none;zoom:.5;transform-origin:top left" srcdoc="{parent}"></iframe>"#
    );
    let server = FixtureServer::with_body(Box::leak(page.into_boxed_str()));
    let mut browser = launch_headless();
    browser.navigate(&server.url()).unwrap();
    let observed = browser.observe().unwrap();
    assert_eq!(observed.scroll_regions.len(), 1);
    let fact = browser
        .perform(
            prepared_scroll(&observed, false, 1),
            &InputCancellation::default(),
        )
        .unwrap();
    assert!((fact.scroll_readback[0].after - 92.0).abs() <= 1.0);
    assert_eq!(fact.scroll_readback.last().unwrap().after, 0.0);
    browser.close().unwrap();
}

#[test]
#[ignore = "requires the local Chromium executable"]
fn slotted_controls_follow_their_painted_scroll_ancestors() {
    let _serial = REAL_BROWSER.lock().unwrap();
    let mut browser = launch_headless();
    for transform in ["none", "scale(.5)"] {
        let body = format!(
            r#"<!doctype html><style>body{{margin:0;overflow:hidden}}button{{display:block;height:30px;width:220px;padding:0;border:0}}</style><div id="host"></div><p id="result">Selected: none</p><script>for(let i=1;i<=30;i++){{let b=document.createElement('button');b.textContent='Slotted Row '+i;b.onclick=()=>result.textContent='Selected: '+i;host.append(b)}}host.attachShadow({{mode:'open'}}).innerHTML='<div aria-label="Slotted rows" style="height:60px;width:220px;overflow-y:auto;overflow-x:hidden;transform:{transform};transform-origin:top left"><slot></slot></div>'</script>"#
        );
        let server = FixtureServer::with_body(Box::leak(body.into_boxed_str()));
        browser.navigate(&server.url()).unwrap();
        let observed = browser.observe().unwrap();
        assert_eq!(observed.scroll_regions.len(), 1);
        assert_eq!(
            observed
                .elements
                .iter()
                .map(|e| e.name.as_str())
                .collect::<Vec<_>>(),
            ["Slotted Row 1", "Slotted Row 2"],
            "{transform}"
        );
        assert!(
            !observed.visible_text.contains("Slotted Row 3"),
            "{transform}"
        );
        let fact = browser
            .perform(
                prepared_scroll(&observed, false, 1),
                &InputCancellation::default(),
            )
            .unwrap();
        assert!(
            (fact.scroll_readback[0].after - 52.0).abs() <= 1.0,
            "{transform}: {fact:?}"
        );
        assert_eq!(fact.scroll_readback.last().unwrap().after, 0.0);
        let after = browser.observe().unwrap();
        assert!(
            after.elements.iter().any(|e| e.name == "Slotted Row 3"),
            "{transform}"
        );
        browser
            .perform(
                prepared(
                    &after,
                    "Slotted Row 3",
                    PreparedOperation::Click,
                    None,
                    None,
                    2,
                ),
                &InputCancellation::default(),
            )
            .unwrap();
        assert!(
            browser
                .observe()
                .unwrap()
                .visible_text
                .contains("Selected: 3")
        );
    }
    let server = FixtureServer::with_body(
        r#"<!doctype html><style>body{margin:0;overflow:hidden}button{display:block;height:30px;width:180px;padding:0;border:0}</style><div id="host"><div id="inner" aria-label="Inner slotted rows" style="height:200px;width:220px;overflow:auto"></div></div><script>for(let i=1;i<=30;i++){let b=document.createElement('button');b.textContent='Nested row '+i;inner.append(b)}host.attachShadow({mode:'open'}).innerHTML='<div id="outer" aria-label="Outer slotted rows" style="height:60px;width:240px;overflow:auto"><slot></slot></div>';host.shadowRoot.getElementById('outer').scrollTop=140</script>"#,
    );
    browser.navigate(&server.url()).unwrap();
    let observed = browser.observe().unwrap();
    assert_eq!(
        observed
            .scroll_regions
            .iter()
            .map(|r| r.name.as_str())
            .collect::<Vec<_>>(),
        ["Outer slotted rows", "Inner slotted rows"]
    );
    assert_eq!(
        observed.scroll_regions[1].parent_node_id,
        Some(observed.scroll_regions[0].node_id)
    );
    let mut input = prepared_scroll(&observed, false, 1);
    input.scroll_region = Some(observed.scroll_regions[1].clone());
    let fact = browser
        .perform(input, &InputCancellation::default())
        .unwrap();
    assert_eq!(fact.scroll_readback.len(), 3);
    assert!((fact.scroll_readback[0].after - 52.0).abs() <= 1.0);
    assert_eq!(
        fact.scroll_readback[1].name.as_deref(),
        Some("Outer slotted rows")
    );
    assert_eq!(fact.scroll_readback[1].before, 140.0);
    assert_eq!(fact.scroll_readback[1].after, 140.0);
    assert_eq!(fact.scroll_readback[2].after, 0.0);

    let server = FixtureServer::with_body(
        r#"<!doctype html><body style="margin:0;overflow:hidden"><div id="host"><div id="inner" aria-label="Scaled inner" style="height:200px;width:220px;overflow:auto;scrollbar-width:none"><div style="height:1000px">Rows</div></div></div><script>host.attachShadow({mode:'open'}).innerHTML='<div id="outer" aria-label="Scaled outer" style="height:60px;width:240px;overflow:auto;scrollbar-width:none;transform:scale(.5);transform-origin:top left"><slot></slot></div>';host.shadowRoot.getElementById('outer').scrollTop=140</script>"#,
    );
    browser.navigate(&server.url()).unwrap();
    let observed = browser.observe().unwrap();
    let inner = observed
        .scroll_regions
        .iter()
        .find(|r| r.name == "Scaled inner")
        .unwrap();
    assert!((inner.rect.width - 110.0).abs() < 0.01);
    assert!((inner.rect.height - 30.0).abs() < 0.01);
    let mut input = prepared_scroll(&observed, false, 1);
    input.scroll_region = Some(inner.clone());
    let fact = browser
        .perform(input, &InputCancellation::default())
        .unwrap();
    assert!((fact.scroll_readback[0].after - 52.0).abs() <= 1.0);
    assert_eq!(fact.scroll_readback[1].after, 140.0);
    assert_eq!(fact.scroll_readback[2].after, 0.0);
    let server = FixtureServer::with_body(
        r#"<!doctype html><div id="host"><div role="treeitem" tabindex="0" aria-label="One">One</div><div id="two" role="treeitem" tabindex="0" aria-label="Two">Two</div></div><script>host.attachShadow({mode:'open'}).innerHTML='<div role="tree"><slot></slot></div>';two.focus()</script>"#,
    );
    browser.navigate(&server.url()).unwrap();
    let observed = browser.observe().unwrap();
    let focus = observed.focus_anchor.as_ref().unwrap();
    assert_eq!(focus.role, "treeitem");
    assert_eq!(focus.name, "Two");
    assert_eq!(focus.position, Some(2));
    browser.close().unwrap();
}

#[test]
#[ignore = "requires the local Chromium executable"]
fn rotated_viewport_clipping_keeps_each_control_column_overlapping() {
    let _serial = REAL_BROWSER.lock().unwrap();
    let mut browser = launch_headless();
    let server = FixtureServer::with_body(
        r#"<!doctype html><style>body{margin:0;overflow:hidden}#region{position:absolute;left:-100px;top:100px;width:300px;height:300px;overflow-y:auto;overflow-x:hidden;transform:rotate(45deg);transform-origin:top left;scrollbar-width:none}button{position:absolute;left:140px;width:80px;height:20px;padding:0;border:0}</style><div id="region" aria-label="Diagonal rows"><div style="height:1000px"></div></div><script>for(let i=1;i<=30;i++){let b=document.createElement('button');b.textContent='T'+i;b.style.top=(30*(i-1))+'px';region.append(b)}</script>"#,
    );
    browser.navigate(&server.url()).unwrap();
    let mut observed = browser.observe().unwrap();
    let mut seen = std::collections::BTreeSet::new();
    let mut previous = std::collections::BTreeSet::new();
    for ordinal in 1..=8 {
        let current = observed
            .elements
            .iter()
            .map(|e| e.name.clone())
            .collect::<std::collections::BTreeSet<_>>();
        if ordinal > 1 {
            assert!(
                !previous.is_disjoint(&current),
                "consecutive views must share controls: {previous:?} / {current:?}"
            );
        }
        seen.extend(current.iter().cloned());
        previous = current;
        let fact = browser
            .perform(
                prepared_scroll(&observed, false, ordinal),
                &InputCancellation::default(),
            )
            .unwrap();
        assert_eq!(fact.scroll_readback.last().unwrap().after, 0.0);
        observed = browser.observe().unwrap();
    }
    seen.extend(observed.elements.iter().map(|e| e.name.clone()));
    for i in 1..=10 {
        assert!(
            seen.contains(&format!("T{i}")),
            "a reachable control was skipped: T{i}; views={seen:?}"
        );
    }

    let baseline = r#"<!doctype html><style>body{margin:0;overflow:hidden}#region{position:absolute;left:-100px;top:100px;width:300px;height:300px;overflow:auto;transform:rotate(45deg);transform-origin:top left;scrollbar-width:none}#target{position:absolute;left:140px;top:30px;width:80px;height:20px}#host{position:absolute;left:140px;top:10px;width:10px;height:10px}</style><div id="region" aria-label="Diagonal"><div style="height:1000px"></div><button id="target">Target</button>SHADOW</div><p id="result">Not selected</p>"#;
    let mut distances = Vec::new();
    let plain = baseline.replace("SHADOW", "");
    for (index,html) in [
        plain.clone(),
        format!(r#"{plain}<button style="position:fixed;left:95px;top:95px;width:10px;height:10px">Unrelated</button>"#),
        plain.replace("<button id=\"target\">",r#"<button style="visibility:hidden;position:absolute;left:138px;top:0;width:8px;height:10px">Hidden</button><button id="target">"#),
        baseline.replace("SHADOW",r#"<div id="host"></div><script>host.attachShadow({mode:'open'}).innerHTML='<button style="width:10px;height:10px;padding:0;border:0" onclick="result.textContent=\'Shadow selected\'">Shadow target</button>'</script>"#),
    ].into_iter().enumerate() {
        let server=FixtureServer::with_body(Box::leak(html.into_boxed_str()));
        browser.navigate(&server.url()).unwrap();
        let observed=browser.observe().unwrap();
        let fact=browser.perform(prepared_scroll(&observed,false,1), &InputCancellation::default()).unwrap();
        distances.push(fact.scroll_readback[0].after);
        if index==3 {
            let mut observed=browser.observe().unwrap();
            let mut clicked=false;
            for ordinal in 2..=8 {
                // Require a centre actually inside the window, then prove the
                // native hit and effect rather than trusting a partial candidate.
                if observed.elements.iter().any(|e|e.name=="Shadow target" && e.rect.x+e.rect.width/2.0>1.0) {
                    browser.perform(prepared(&observed,"Shadow target",PreparedOperation::Click,None,None,ordinal), &InputCancellation::default()).unwrap();
                    clicked=true;break;
                }
                browser.perform(prepared_scroll(&observed,false,ordinal), &InputCancellation::default()).unwrap();
                observed=browser.observe().unwrap();
            }
            assert!(clicked,"reachable shadow control was skipped");
            assert!(browser.observe().unwrap().visible_text.contains("Shadow selected"));
        }
    }
    assert_eq!(
        distances[0], distances[1],
        "unrelated control: {distances:?}"
    );
    assert_eq!(distances[0], distances[2], "hidden control: {distances:?}");
    assert!(
        distances[3] + 5.0 < distances[0],
        "shadow column must constrain overlap: {distances:?}"
    );
    browser.close().unwrap();
}

#[test]
#[ignore = "requires the local Chromium executable"]
fn boxless_overflow_wrappers_keep_painted_controls_and_text() {
    let _serial = REAL_BROWSER.lock().unwrap();
    let mut browser = launch_headless();
    for css in [
        "overflow:hidden",
        "overflow:clip",
        "display:contents;overflow:hidden",
        "display:inline-block;overflow:hidden",
    ] {
        let body = format!(
            r#"<!doctype html><style>body{{margin:0}}</style><span style="{css}"><button onclick="this.textContent='Selected goal'">Visible goal</button></span>"#
        );
        let server = FixtureServer::with_body(Box::leak(body.into_boxed_str()));
        browser.navigate(&server.url()).unwrap();
        let observed = browser.observe().unwrap();
        assert_eq!(observed.elements.len(), 1, "{css}");
        assert!(observed.visible_text.contains("Visible goal"), "{css}");
        browser
            .perform(
                prepared(
                    &observed,
                    "Visible goal",
                    PreparedOperation::Click,
                    None,
                    None,
                    1,
                ),
                &InputCancellation::default(),
            )
            .unwrap();
        assert!(
            browser
                .observe()
                .unwrap()
                .visible_text
                .contains("Selected goal"),
            "{css}"
        );
    }
    let server = FixtureServer::with_body(
        r#"<!doctype html><span style="display:inline-block;width:1px;height:1px;overflow:hidden"><button>Actually clipped</button></span>"#,
    );
    browser.navigate(&server.url()).unwrap();
    let observed = browser.observe().unwrap();
    assert!(observed.elements.is_empty());
    assert!(!observed.visible_text.contains("Actually clipped"));
    let server = FixtureServer::with_body(
        r#"<!doctype html><body style="margin:0"><svg width="0" height="0" style="overflow:hidden"><foreignObject width="200" height="200"><button>Zero viewport clipped</button></foreignObject></svg>"#,
    );
    browser.navigate(&server.url()).unwrap();
    let observed = browser.observe().unwrap();
    assert!(observed.elements.is_empty());
    assert!(!observed.visible_text.contains("Zero viewport clipped"));
    browser.close().unwrap();
}

#[test]
#[ignore = "requires the local Chromium executable"]
fn native_svg_coordinates_clip_html_regions_and_preserve_visible_clicks() {
    let _serial = REAL_BROWSER.lock().unwrap();
    let mut browser = launch_headless();
    for (viewbox, svg_css, foreign_transform, height, top, initially_visible) in [
        (600, "none", "", 50.0, 160, false),
        (150, "none", "", 200.0, 70, true),
        (150, "scale(.5)", "", 100.0, 70, true),
        (150, "none", "scale(.5,.75)", 150.0, 70, true),
    ] {
        let body = format!(
            r#"<!doctype html><style>body{{margin:0;overflow:hidden}}</style><svg width="300" height="300" viewBox="0 0 {viewbox} {viewbox}" style="transform:{svg_css};transform-origin:top left"><foreignObject width="{viewbox}" height="{viewbox}" transform="{foreign_transform}"><div xmlns="http://www.w3.org/1999/xhtml" aria-label="SVG rows" style="width:100px;height:100px;overflow:auto;position:relative;scrollbar-width:none"><div style="height:1000px"></div><button style="position:absolute;top:{top}px;left:5px;width:70px;height:20px;font-size:8px" onclick="result.textContent='Selected goal'">SVG goal</button></div></foreignObject></svg><p id="result">Selected: none</p>"#
        );
        let server = FixtureServer::with_body(Box::leak(body.into_boxed_str()));
        browser.navigate(&server.url()).unwrap();
        let observed = browser.observe().unwrap();
        assert_eq!(observed.scroll_regions.len(), 1);
        assert!(
            (observed.scroll_regions[0].rect.height - height).abs() < 0.01,
            "{viewbox}/{svg_css}/{foreign_transform}: {:?}",
            observed.scroll_regions
        );
        assert_eq!(
            observed.elements.iter().any(|e| e.name == "SVG goal"),
            initially_visible
        );
        assert_eq!(
            observed.visible_text.contains("SVG goal"),
            initially_visible
        );
        let clickable = if initially_visible {
            observed
        } else {
            let fact = browser
                .perform(
                    prepared_scroll(&observed, false, 1),
                    &InputCancellation::default(),
                )
                .unwrap();
            assert!((fact.scroll_readback[0].after - 92.0).abs() <= 1.0);
            assert_eq!(fact.scroll_readback.last().unwrap().after, 0.0);
            browser.observe().unwrap()
        };
        browser
            .perform(
                prepared(
                    &clickable,
                    "SVG goal",
                    PreparedOperation::Click,
                    None,
                    None,
                    2,
                ),
                &InputCancellation::default(),
            )
            .unwrap();
        assert!(
            browser
                .observe()
                .unwrap()
                .visible_text
                .contains("Selected goal")
        );
    }
    let server = FixtureServer::with_body(
        r#"<!doctype html><body style="margin:0"><svg width="300" height="300"><g style="overflow:hidden"><foreignObject width="300" height="300"><button xmlns="http://www.w3.org/1999/xhtml" onclick="this.textContent='Selected goal'">Visible SVG group goal</button></foreignObject></g></svg>"#,
    );
    browser.navigate(&server.url()).unwrap();
    let observed = browser.observe().unwrap();
    assert!(observed.visible_text.contains("Visible SVG group goal"));
    browser
        .perform(
            prepared(
                &observed,
                "Visible SVG group goal",
                PreparedOperation::Click,
                None,
                None,
                1,
            ),
            &InputCancellation::default(),
        )
        .unwrap();
    assert!(
        browser
            .observe()
            .unwrap()
            .visible_text
            .contains("Selected goal")
    );
    browser.close().unwrap();
}

#[test]
#[ignore = "requires the local Chromium executable"]
fn overflow_clip_margin_preserves_the_browser_clip_edge() {
    let _serial = REAL_BROWSER.lock().unwrap();
    let mut browser = launch_headless();
    for (overflow, margin, padding, visible, text_visible) in [
        ("clip", "100px", 0, true, true),
        ("hidden", "100px", 0, false, false),
        ("clip", "100px", 20, true, true),
        ("clip", "border-box", 20, true, true),
        ("clip", "content-box", 20, false, false),
        ("clip", "0px", 20, false, true),
    ] {
        let body = format!(
            r#"<!doctype html><body style="margin:0;overflow:hidden"><div style="width:200px;height:40px;padding:{padding}px;border:{border}px solid;overflow:{overflow};overflow-clip-margin:{margin};position:relative"><button style="position:absolute;top:70px;left:10px;width:150px;height:30px" onclick="this.textContent='Selected goal'">Visible margin goal</button></div>"#,
            border = if padding > 0 { 10 } else { 0 }
        );
        let server = FixtureServer::with_body(Box::leak(body.into_boxed_str()));
        browser.navigate(&server.url()).unwrap();
        let observed = browser.observe().unwrap();
        assert_eq!(
            observed
                .elements
                .iter()
                .any(|e| e.name == "Visible margin goal"),
            visible,
            "{overflow}/{margin}/{padding}"
        );
        assert_eq!(
            observed.visible_text.contains("Visible margin goal"),
            text_visible,
            "{overflow}/{margin}/{padding}: {}",
            observed.visible_text
        );
        if visible {
            browser
                .perform(
                    prepared(
                        &observed,
                        "Visible margin goal",
                        PreparedOperation::Click,
                        None,
                        None,
                        1,
                    ),
                    &InputCancellation::default(),
                )
                .unwrap();
            assert!(
                browser
                    .observe()
                    .unwrap()
                    .visible_text
                    .contains("Selected goal")
            );
        }
    }
    browser.close().unwrap();
}
