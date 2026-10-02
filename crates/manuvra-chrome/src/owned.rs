#[cfg(any(target_os = "linux", target_os = "macos"))]
use crate::endpoint::Endpoint;
use crate::input::{
    InputCancellation, PerformError, PerformFact, PreparedInput, PreparedOperation,
};
use crate::observation::Observation;
use crate::page::{self, Screenshot};
use crate::transport::{CdpClient, CommandFailure};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::HashSet;
use std::env;
use std::fs;
#[cfg(any(target_os = "linux", target_os = "macos"))]
use std::path::Path;
use std::path::PathBuf;
use std::process::Child;
#[cfg(any(target_os = "linux", target_os = "macos"))]
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::thread;
use std::time::{Duration, Instant};
#[cfg(any(target_os = "linux", target_os = "macos"))]
use std::time::{SystemTime, UNIX_EPOCH};

#[cfg(target_os = "macos")]
#[path = "owned/darwin.rs"]
mod platform;
#[cfg(target_os = "linux")]
#[path = "owned/linux.rs"]
mod platform;

/// Executable and signal mechanics that are identical on every supported
/// platform. Process identity and group ownership stay in the siblings.
#[cfg(any(target_os = "linux", target_os = "macos"))]
mod process {
    use super::safe_error;
    use std::env;
    use std::ffi::OsStr;
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use std::path::{Path, PathBuf};
    use std::process::Child;
    use std::thread;
    use std::time::{Duration, Instant};

    pub(super) const GRACEFUL_TIMEOUT: Duration = Duration::from_secs(2);
    pub(super) const FORCED_TIMEOUT: Duration = Duration::from_secs(1);

    pub(super) fn find_on_path(name: &str, search_path: Option<&OsStr>) -> Option<PathBuf> {
        env::split_paths(search_path?).find_map(|directory| usable_binary(&directory.join(name)))
    }

    pub(super) fn usable_binary(path: &Path) -> Option<PathBuf> {
        let metadata = path.metadata().ok()?;
        if !metadata.is_file() {
            return None;
        }
        (metadata.permissions().mode() & 0o111 != 0)
            .then(|| fs::canonicalize(path).ok())
            .flatten()
    }

    pub(super) fn wait_for_process_exit(
        child: &mut Child,
        timeout: Duration,
    ) -> Result<bool, String> {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            if child
                .try_wait()
                .map_err(|error| safe_error(&error.to_string()))?
                .is_some()
            {
                return Ok(true);
            }
            thread::sleep(Duration::from_millis(20));
        }
        Ok(false)
    }

    pub(super) fn pid_t(id: u32) -> Result<i32, String> {
        id.try_into()
            .map_err(|_| "process id does not fit pid_t".to_owned())
    }

    /// Signals one process; a process that no longer exists is not an error.
    pub(super) fn signal_pid(pid: u32, signal: i32) -> Result<(), String> {
        send_signal(pid_t(pid)?, signal)
    }

    /// Signals every member of a process group; an empty group is not an error.
    pub(super) fn signal_group(process_group: u32, signal: i32) -> Result<(), String> {
        send_signal(-pid_t(process_group)?, signal)
    }

    fn send_signal(target: i32, signal: i32) -> Result<(), String> {
        if unsafe { libc::kill(target, signal) } == 0 {
            return Ok(());
        }
        let error = std::io::Error::last_os_error();
        if error.raw_os_error() == Some(libc::ESRCH) {
            Ok(())
        } else {
            Err(safe_error(&error.to_string()))
        }
    }
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
mod platform {
    use std::process::Child;

    pub struct BrowserOwnership;

    pub fn terminate(child: &mut Child, _ownership: &mut BrowserOwnership) -> Result<(), String> {
        child.kill().map_err(|error| error.to_string())?;
        child.wait().map_err(|error| error.to_string())?;
        Ok(())
    }
}

const SNAPSHOT: &str = include_str!("snapshot.js");
const MASKING: &str = include_str!("masking.js");
const CAPTURE_ATTEMPTS: usize = 3;
#[cfg(any(target_os = "linux", target_os = "macos"))]
const COVERAGE_PROBE: &str = r#"(() => {
  if (window.__manuvraCoverageProbeInstalled) return;
  window.__manuvraCoverageProbeInstalled = true;
  window.__manuvraClosedShadowRoots = 0;
  window.__manuvraClosedShadowHosts = new WeakSet();
  const original = Element.prototype.attachShadow;
  Element.prototype.attachShadow = function(init) {
    const root = original.call(this, init);
    if (init?.mode === 'closed') {
      window.__manuvraClosedShadowRoots += 1;
      window.__manuvraClosedShadowHosts.add(this);
    }
    return root;
  };
})()"#;
const START_TIMEOUT: Duration = Duration::from_secs(15);
const COMMAND_TIMEOUT: Duration = Duration::from_secs(10);
const QUIET_WINDOW: Duration = Duration::from_millis(150);

#[derive(Debug, Clone)]
pub struct BrowserConfig {
    pub explicit_binary: Option<PathBuf>,
    pub headless: bool,
    pub width: u16,
    pub height: u16,
    pub inherit_process_group: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BrowserProvenance {
    pub browser_path: String,
    pub browser_version: String,
    pub viewport: ProvenanceViewport,
    pub display_mode: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProvenanceViewport {
    pub width: u16,
    pub height: u16,
}

pub struct CapturedPage {
    pub observation: Observation,
    pub screenshot: Screenshot,
    pub redaction: RedactionProof,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RedactionProof {
    pub sensitive_values_checked: usize,
    pub matched_values: usize,
    pub mask_count: usize,
}

impl RedactionProof {
    pub fn verifies(&self, sensitive_values: usize) -> bool {
        self.sensitive_values_checked == sensitive_values
            && self.matched_values <= sensitive_values
            && self.mask_count >= self.matched_values
    }
}

pub struct OwnedBrowser {
    child: Child,
    profile: PathBuf,
    client: Arc<CdpClient>,
    provenance: BrowserProvenance,
    ownership: platform::BrowserOwnership,
    lifecycle: Lifecycle,
}

#[derive(Debug, thiserror::Error)]
pub enum BrowserError {
    #[error("unsupported platform")]
    UnsupportedPlatform,
    #[error("Chromium executable is unavailable")]
    Unavailable,
    #[error("Chromium launch failed: {0}")]
    Launch(String),
    #[error("Chromium control failed: {0}")]
    Control(String),
    #[error("Chromium observation was invalid: {0}")]
    InvalidObservation(String),
}

impl OwnedBrowser {
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    pub fn launch(config: BrowserConfig) -> Result<Self, BrowserError> {
        PreparedBrowser::new(config)?.launch()
    }

    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    pub fn launch(_config: BrowserConfig) -> Result<Self, BrowserError> {
        Err(BrowserError::UnsupportedPlatform)
    }

    pub fn provenance(&self) -> &BrowserProvenance {
        &self.provenance
    }

    pub fn navigate(&self, url: &str) -> Result<(), BrowserError> {
        let fence = self.client.cursor();
        let navigated = command(&self.client, "Page.navigate", json!({"url": url}))?;
        require_committed_navigation(&navigated)?;
        wait_for_document(&self.client, fence)
    }

    pub fn observe(&self) -> Result<Observation, BrowserError> {
        let value = evaluate(&self.client, SNAPSHOT)?;
        let mut observation: Observation = serde_json::from_value(value)
            .map_err(|error| BrowserError::InvalidObservation(error.to_string()))?;
        crate::observation::mark_closed_shadow_focus(&mut observation, |method, params| {
            command(&self.client, method, params)
        })?;
        // A page session reports `window.open` as `Page.windowOpen`; the popup
        // itself is another target Manuvra neither observes nor controls.
        if self.client.has_received("Page.windowOpen") {
            observation.coverage.gaps.push("popup".into());
            observation.coverage.gaps.sort();
            observation.coverage.gaps.dedup();
        }
        Ok(observation)
    }

    pub fn perform(
        &self,
        input: PreparedInput,
        cancellation: &InputCancellation,
    ) -> Result<PerformFact, PerformError> {
        let operation = input.operation;
        let fence = self.client.cursor();
        let fact = crate::input::perform(&self.client, input, cancellation)?;
        settle_after_input(&self.client, operation, fence)
            .map_err(|error| PerformError::Uncertain(error.to_string()))?;
        Ok(fact)
    }

    pub fn capture(&self) -> Result<CapturedPage, BrowserError> {
        self.capture_fenced(None)
    }

    pub fn capture_redacted(&self, sensitive: &[String]) -> Result<CapturedPage, BrowserError> {
        if sensitive.is_empty() {
            return self.capture();
        }
        self.capture_fenced(Some(Masking {
            expression: masking_script(sensitive)?,
            values: sensitive.len(),
        }))
    }

    /// Retries until the observation and screenshot come from one unchanged page.
    fn capture_fenced(&self, masking: Option<Masking>) -> Result<CapturedPage, BrowserError> {
        for _ in 0..CAPTURE_ATTEMPTS {
            if let Some(captured) = self.capture_once(masking.as_ref())? {
                return Ok(captured);
            }
            thread::sleep(QUIET_WINDOW);
        }
        Err(BrowserError::Control(
            "page changed throughout screenshot fencing".into(),
        ))
    }

    /// Masks are placed inside the fence on every attempt, so a secret that moves
    /// before the screenshot invalidates the attempt instead of escaping its mask.
    fn capture_once(
        &self,
        masking: Option<&Masking>,
    ) -> Result<Option<CapturedPage>, BrowserError> {
        let _mutations = DomMutationWatch::start(&self.client)?;
        let fence = self.client.cursor();
        let masks = install_masks(&self.client, masking)?;
        let observation = self.observe()?;
        let screenshot =
            page::capture_screenshot(&self.client, deadline(), Arc::new(AtomicBool::new(false)))
                .map_err(|error| BrowserError::Control(error.to_string()))?;
        if page_changed_since(&self.client, fence) {
            return Ok(None);
        }
        Ok(Some(CapturedPage {
            observation,
            screenshot,
            redaction: masks.proof.clone(),
        }))
    }

    /// Terminates the browser at most once, then removes its profile. A failed
    /// profile removal is retried without signalling the reaped process again.
    pub fn close(&mut self) -> Result<(), BrowserError> {
        if self.lifecycle == Lifecycle::Running {
            platform::terminate(&mut self.child, &mut self.ownership)
                .map_err(BrowserError::Control)?;
            self.lifecycle = Lifecycle::Terminated;
        }
        if self.lifecycle == Lifecycle::Terminated {
            fs::remove_dir_all(&self.profile)
                .map_err(|error| BrowserError::Control(safe_error(&error.to_string())))?;
            self.lifecycle = Lifecycle::Closed;
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Lifecycle {
    Running,
    Terminated,
    Closed,
}

fn require_committed_navigation(navigated: &Value) -> Result<(), BrowserError> {
    match navigated.get("errorText").and_then(Value::as_str) {
        Some(error) if !error.is_empty() => Err(BrowserError::Control(format!(
            "navigation failed: {}",
            safe_error(error)
        ))),
        _ => Ok(()),
    }
}

#[derive(Deserialize)]
struct MaskingEvidence {
    verified: bool,
    sensitive_values_checked: usize,
    matched_values: usize,
    mask_count: usize,
}

fn masking_proof(value: Value, expected_values: usize) -> Result<RedactionProof, BrowserError> {
    let evidence: MaskingEvidence = serde_json::from_value(value)
        .map_err(|_| BrowserError::Control("redaction_unverifiable".into()))?;
    let proof = RedactionProof {
        sensitive_values_checked: evidence.sensitive_values_checked,
        matched_values: evidence.matched_values,
        mask_count: evidence.mask_count,
    };
    (evidence.verified && proof.verifies(expected_values))
        .then_some(proof)
        .ok_or_else(|| BrowserError::Control("redaction_unverifiable".into()))
}

struct Masking {
    expression: String,
    values: usize,
}

/// Masks placed for one capture attempt; they are removed when dropped.
struct InstalledMasks<'a> {
    client: Option<&'a CdpClient>,
    proof: RedactionProof,
}

impl Drop for InstalledMasks<'_> {
    fn drop(&mut self) {
        if let Some(client) = self.client {
            let _ = evaluate(client, "window.__manuvraRemoveMasks?.(); true");
        }
    }
}

fn install_masks<'a>(
    client: &'a CdpClient,
    masking: Option<&Masking>,
) -> Result<InstalledMasks<'a>, BrowserError> {
    let mut masks = InstalledMasks {
        client: None,
        proof: RedactionProof {
            sensitive_values_checked: 0,
            matched_values: 0,
            mask_count: 0,
        },
    };
    if let Some(masking) = masking {
        masks.client = Some(client);
        masks.proof = masking_proof(evaluate(client, &masking.expression)?, masking.values)?;
    }
    Ok(masks)
}

/// Chrome reports DOM mutations only for nodes the client has requested, so a
/// capture requests the whole pierced document before its fence and releases
/// those bindings afterwards to keep later waits insensitive to DOM churn.
struct DomMutationWatch<'a> {
    client: &'a CdpClient,
}

impl<'a> DomMutationWatch<'a> {
    fn start(client: &'a CdpClient) -> Result<Self, BrowserError> {
        let watch = Self { client };
        command(
            client,
            "DOM.getDocument",
            json!({"depth": -1, "pierce": true}),
        )?;
        Ok(watch)
    }
}

impl Drop for DomMutationWatch<'_> {
    fn drop(&mut self) {
        let _ = command(self.client, "DOM.disable", json!({}));
        let _ = command(self.client, "DOM.enable", json!({}));
    }
}

fn page_changed_since(client: &CdpClient, fence: u64) -> bool {
    let snapshot = client.snapshot_since(fence);
    snapshot.overflowed
        || snapshot
            .events
            .iter()
            .any(|event| crate::transport::is_relevant_event(event) && !is_mask_insertion(event))
}

fn is_mask_insertion(event: &crate::transport::JournalEvent) -> bool {
    crate::transport::event_method(event) == Some("DOM.childNodeInserted")
        && crate::transport::event_params(event)
            .pointer("/node/attributes")
            .and_then(Value::as_array)
            .is_some_and(|attributes| attributes.iter().any(|name| name == "data-manuvra-mask"))
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
struct PreparedBrowser {
    binary: PathBuf,
    profile: PathBuf,
    config: BrowserConfig,
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
impl PreparedBrowser {
    fn new(config: BrowserConfig) -> Result<Self, BrowserError> {
        let binary = platform::discover_binary(
            config.explicit_binary.as_deref(),
            env::var_os("MANUVRA_BROWSER").map(PathBuf::from),
            env::var_os("PATH"),
        )?;
        let profile = private_profile()?;
        Ok(Self {
            binary,
            profile,
            config,
        })
    }

    fn launch(self) -> Result<OwnedBrowser, BrowserError> {
        self.spawn()?.connect_endpoint()
    }

    fn spawn(self) -> Result<StartingBrowser, BrowserError> {
        let command = browser_command(&self.binary, &self.profile, &self.config);
        let (child, ownership) = match platform::spawn(command, self.config.inherit_process_group) {
            Ok(spawned) => spawned,
            Err(error) => {
                let _ = fs::remove_dir_all(&self.profile);
                return Err(BrowserError::Launch(safe_error(&error)));
            }
        };
        Ok(StartingBrowser {
            prepared: Some(self),
            child: Some(child),
            ownership: Some(ownership),
        })
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
struct StartingBrowser {
    prepared: Option<PreparedBrowser>,
    child: Option<Child>,
    ownership: Option<platform::BrowserOwnership>,
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
impl StartingBrowser {
    fn connect_endpoint(mut self) -> Result<OwnedBrowser, BrowserError> {
        let profile = self.prepared().profile.clone();
        let endpoint = wait_for_endpoint(self.child_mut(), &profile, START_TIMEOUT)?;
        self.connect_page(endpoint)
    }
    fn connect_page(mut self, endpoint: Endpoint) -> Result<OwnedBrowser, BrowserError> {
        let page = wait_for_page(self.child_mut(), &endpoint, START_TIMEOUT)?;
        self.connect_client(page)
    }
    fn connect_client(self, page: (String, String)) -> Result<OwnedBrowser, BrowserError> {
        let client = CdpClient::connect(page.0, true).map_err(BrowserError::Control)?;
        self.finish(client, page.1)
    }
    fn finish(
        mut self,
        client: Arc<CdpClient>,
        version: String,
    ) -> Result<OwnedBrowser, BrowserError> {
        install_coverage_probe(&client)?;
        set_viewport(
            &client,
            self.prepared().config.width,
            self.prepared().config.height,
        )?;
        let prepared = take_prepared(&mut self.prepared);
        Ok(OwnedBrowser {
            child: take_child(&mut self.child),
            profile: prepared.profile,
            client,
            provenance: BrowserProvenance {
                browser_path: prepared.binary.to_string_lossy().into_owned(),
                browser_version: version,
                viewport: ProvenanceViewport {
                    width: prepared.config.width,
                    height: prepared.config.height,
                },
                display_mode: display_mode(prepared.config.headless).into(),
            },
            ownership: take_ownership(&mut self.ownership),
            lifecycle: Lifecycle::Running,
        })
    }
    fn prepared(&self) -> &PreparedBrowser {
        self.prepared.as_ref().expect("prepared browser")
    }

    fn child_mut(&mut self) -> &mut Child {
        self.child.as_mut().expect("Chromium child")
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn display_mode(headless: bool) -> &'static str {
    if headless { "headless" } else { "headed" }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
impl Drop for StartingBrowser {
    fn drop(&mut self) {
        cleanup_starting_child(self.child.as_mut(), self.ownership.as_mut());
        cleanup_starting_profile(self.prepared.as_ref());
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn take_prepared(value: &mut Option<PreparedBrowser>) -> PreparedBrowser {
    value.take().expect("prepared browser")
}
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn take_child(value: &mut Option<Child>) -> Child {
    value.take().expect("Chromium child")
}
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn take_ownership(value: &mut Option<platform::BrowserOwnership>) -> platform::BrowserOwnership {
    value.take().expect("Chromium ownership")
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn cleanup_starting_child(
    child: Option<&mut Child>,
    ownership: Option<&mut platform::BrowserOwnership>,
) {
    if let (Some(child), Some(ownership)) = (child, ownership) {
        let _ = platform::terminate(child, ownership);
    }
}
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn cleanup_starting_profile(prepared: Option<&PreparedBrowser>) {
    if let Some(prepared) = prepared {
        let _ = fs::remove_dir_all(&prepared.profile);
    }
}

impl Drop for OwnedBrowser {
    fn drop(&mut self) {
        let _ = self.close();
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn browser_command(binary: &Path, profile: &Path, config: &BrowserConfig) -> Command {
    let mut command = Command::new(binary);
    command
        .arg("--remote-debugging-address=127.0.0.1")
        .arg("--remote-debugging-port=0")
        .arg(format!("--user-data-dir={}", profile.display()))
        .arg("--no-first-run")
        .arg("--no-default-browser-check")
        .arg("--disable-background-networking")
        .arg(format!("--window-size={},{}", config.width, config.height))
        .env_remove("TYPESAFE_API_KEY")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    if config.headless {
        // Headless presents a desktop mouse so hover/pointer media queries match the input
        // Manuvra dispatches, including controls revealed only under @media (hover: hover).
        command
            .arg("--headless=new")
            .arg("--disable-gpu")
            .arg("--blink-settings=primaryHoverType=2,availableHoverTypes=2,primaryPointerType=4,availablePointerTypes=4");
    }
    command.arg("about:blank");
    command
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn private_profile() -> Result<PathBuf, BrowserError> {
    use std::os::unix::fs::DirBuilderExt;
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|error| BrowserError::Launch(error.to_string()))?
        .as_nanos();
    let path = env::temp_dir().join(format!("manuvra-chromium-{}-{stamp}", std::process::id()));
    fs::DirBuilder::new()
        .mode(0o700)
        .create(&path)
        .map_err(|error| BrowserError::Launch(error.to_string()))?;
    Ok(path)
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn wait_for_endpoint(
    child: &mut Child,
    profile: &Path,
    timeout: Duration,
) -> Result<Endpoint, BrowserError> {
    let deadline = Instant::now() + timeout;
    let path = profile.join("DevToolsActivePort");
    loop {
        if let Ok(contents) = fs::read_to_string(&path)
            && let Some(port) = contents.lines().next()
        {
            return Endpoint::parse(&format!("127.0.0.1:{port}"))
                .map_err(|error| BrowserError::Launch(error.to_string()));
        }
        if child
            .try_wait()
            .map_err(|error| BrowserError::Launch(safe_error(&error.to_string())))?
            .is_some()
        {
            return Err(BrowserError::Launch(
                "Chromium exited before the CDP endpoint became ready".into(),
            ));
        }
        if Instant::now() >= deadline {
            return Err(BrowserError::Launch(
                "CDP endpoint did not become ready".into(),
            ));
        }
        thread::sleep(Duration::from_millis(50));
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn wait_for_page(
    child: &mut Child,
    endpoint: &Endpoint,
    timeout: Duration,
) -> Result<(String, String), BrowserError> {
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(page) = ready_page(endpoint) {
            return Ok(page);
        }
        require_browser_running(child, "Chromium exited before a CDP target became ready")?;
        if Instant::now() >= deadline {
            return Err(BrowserError::Launch("CDP page did not become ready".into()));
        }
        thread::sleep(Duration::from_millis(50));
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn ready_page(endpoint: &Endpoint) -> Option<(String, String)> {
    let items = endpoint
        .get_json("/json/list", Duration::from_millis(300))
        .ok()?;
    page_websocket_url(&items).map(|url| (url, browser_version(endpoint)))
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn browser_exited(child: &mut Child) -> Result<bool, BrowserError> {
    child
        .try_wait()
        .map(|status| status.is_some())
        .map_err(|error| BrowserError::Launch(safe_error(&error.to_string())))
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn require_browser_running(child: &mut Child, message: &str) -> Result<(), BrowserError> {
    (!browser_exited(child)?)
        .then_some(())
        .ok_or_else(|| BrowserError::Launch(message.into()))
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn page_websocket_url(items: &Value) -> Option<String> {
    items.as_array()?.iter().find_map(|item| {
        (item.get("type").and_then(Value::as_str) == Some("page"))
            .then(|| {
                item.get("webSocketDebuggerUrl")
                    .and_then(Value::as_str)
                    .map(str::to_owned)
            })
            .flatten()
    })
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn browser_version(endpoint: &Endpoint) -> String {
    endpoint
        .get_json("/json/version", Duration::from_millis(300))
        .ok()
        .and_then(|value| {
            value
                .get("Browser")
                .and_then(Value::as_str)
                .map(str::to_owned)
        })
        .unwrap_or_else(|| "unknown".into())
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn set_viewport(client: &CdpClient, width: u16, height: u16) -> Result<(), BrowserError> {
    command(
        client,
        "Emulation.setDeviceMetricsOverride",
        json!({"width": width,"height": height,"deviceScaleFactor": 1,"mobile": false}),
    )?;
    command(
        client,
        "Emulation.setFocusEmulationEnabled",
        json!({"enabled": true}),
    )?;
    Ok(())
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn install_coverage_probe(client: &CdpClient) -> Result<(), BrowserError> {
    command(
        client,
        "Page.addScriptToEvaluateOnNewDocument",
        json!({"source": COVERAGE_PROBE}),
    )?;
    evaluate(client, COVERAGE_PROBE).map(|_| ())
}

fn wait_for_document(client: &CdpClient, mut cursor: u64) -> Result<(), BrowserError> {
    let end = Instant::now() + START_TIMEOUT;
    let mut quiet_since = None;
    loop {
        let ready = evaluate(client, "document.readyState")?.as_str() == Some("complete");
        let snapshot = client.snapshot_since(cursor);
        require_navigation_journal(&snapshot)?;
        update_quiet_since(&mut quiet_since, ready, &snapshot.events);
        cursor = snapshot.last_cursor;
        if quiet_since.is_some_and(|since| since.elapsed() >= QUIET_WINDOW) {
            return Ok(());
        }
        if Instant::now() >= end {
            return Err(BrowserError::Control("page did not become quiet".into()));
        }
        client.wait_for_journal_change(cursor, Duration::from_millis(25));
    }
}

/// Lets the page react to performed input before the next observation.
fn settle_after_input(
    client: &CdpClient,
    operation: PreparedOperation,
    fence: u64,
) -> Result<(), BrowserError> {
    match operation {
        PreparedOperation::Click | PreparedOperation::PressKey(_) => {
            settle_input_navigation(client, fence)
        }
        // Hover styles apply on a later rendering frame, and an opacity transition that reveals
        // controls starts from zero, so an immediate observation still finds them hidden. A fixed
        // window, unlike a quiet-journal wait, cannot turn a performed hover into an error on a
        // page that never stops changing.
        PreparedOperation::Hover | PreparedOperation::ScrollUp | PreparedOperation::ScrollDown => {
            thread::sleep(QUIET_WINDOW);
            Ok(())
        }
        PreparedOperation::TypeText | PreparedOperation::Select | PreparedOperation::SetValue => {
            Ok(())
        }
    }
}

/// A navigation started by the input may commit long after the input returns,
/// so any navigation signal holds the next observation until loading stops.
fn settle_input_navigation(client: &CdpClient, fence: u64) -> Result<(), BrowserError> {
    let end = Instant::now() + QUIET_WINDOW;
    loop {
        let snapshot = client.snapshot_since(fence);
        require_navigation_journal(&snapshot)?;
        if snapshot.events.iter().any(starts_navigation) {
            wait_for_loading(client, fence)?;
            return wait_for_document(client, fence);
        }
        if Instant::now() >= end {
            return Ok(());
        }
        client.wait_for_journal_change(snapshot.last_cursor, Duration::from_millis(25));
    }
}

fn starts_navigation(event: &crate::transport::JournalEvent) -> bool {
    matches!(
        crate::transport::event_method(event),
        Some(
            "Page.frameRequestedNavigation"
                | "Page.frameStartedNavigating"
                | "Page.frameStartedLoading"
                | "Page.frameNavigated"
        )
    )
}

fn wait_for_loading(client: &CdpClient, fence: u64) -> Result<(), BrowserError> {
    let end = Instant::now() + START_TIMEOUT;
    let mut idle_since = None;
    loop {
        let snapshot = client.snapshot_since(fence);
        require_navigation_journal(&snapshot)?;
        if loading_frames(&snapshot.events).is_empty() {
            idle_since.get_or_insert_with(Instant::now);
        } else {
            idle_since = None;
        }
        if idle_since.is_some_and(|since| since.elapsed() >= QUIET_WINDOW) {
            return Ok(());
        }
        if Instant::now() >= end {
            return Err(BrowserError::Control(
                "navigation did not finish loading".into(),
            ));
        }
        client.wait_for_journal_change(snapshot.last_cursor, Duration::from_millis(25));
    }
}

fn loading_frames(events: &[crate::transport::JournalEvent]) -> HashSet<&str> {
    let mut loading = HashSet::new();
    for event in events {
        let frame = crate::transport::event_params(event)
            .get("frameId")
            .and_then(Value::as_str);
        match (crate::transport::event_method(event), frame) {
            (Some("Page.frameStartedLoading"), Some(frame)) => {
                loading.insert(frame);
            }
            (Some("Page.frameStoppedLoading"), Some(frame)) => {
                loading.remove(frame);
            }
            _ => {}
        }
    }
    loading
}

fn update_quiet_since(
    quiet_since: &mut Option<Instant>,
    ready: bool,
    events: &[crate::transport::JournalEvent],
) {
    if events.iter().any(crate::transport::is_relevant_event) {
        *quiet_since = None
    } else if ready {
        quiet_since.get_or_insert_with(Instant::now);
    }
}

fn require_navigation_journal(
    snapshot: &crate::transport::JournalSnapshot,
) -> Result<(), BrowserError> {
    if snapshot.overflowed {
        Err(BrowserError::Control(
            "CDP journal overflowed during navigation".into(),
        ))
    } else {
        Ok(())
    }
}

fn evaluate(client: &CdpClient, expression: &str) -> Result<Value, BrowserError> {
    let result = command(
        client,
        "Runtime.evaluate",
        json!({"expression": expression,"returnByValue": true,"awaitPromise": true}),
    )?;
    if result.get("exceptionDetails").is_some() {
        return Err(BrowserError::Control("page evaluation failed".into()));
    }
    Ok(result
        .pointer("/result/value")
        .cloned()
        .unwrap_or(Value::Null))
}

fn command(client: &CdpClient, method: &str, params: Value) -> Result<Value, BrowserError> {
    client
        .command(method, params, deadline(), Arc::new(AtomicBool::new(false)))
        .result()
        .map_err(|error| BrowserError::Control(command_failure(error)))
}

fn command_failure(error: CommandFailure) -> String {
    match error {
        CommandFailure::Rejected(_) => "CDP command rejected".into(),
        CommandFailure::NotSent(_) => "CDP command not sent".into(),
        CommandFailure::Unknown(_) => "CDP command outcome unknown".into(),
    }
}

fn deadline() -> Instant {
    Instant::now() + COMMAND_TIMEOUT
}

/// Applies `masking.js` to the sensitive values. They reach the page only as a
/// JSON array argument, which is inert data and never executable source.
fn masking_script(sensitive: &[String]) -> Result<String, BrowserError> {
    let values = serde_json::to_string(sensitive)
        .map_err(|error| BrowserError::Control(error.to_string()))?;
    Ok(format!("({})({values})", MASKING.trim_end()))
}

fn safe_error(message: &str) -> String {
    mask_provider_key(message, env::var("TYPESAFE_API_KEY").ok().as_deref())
}

fn mask_provider_key(message: &str, key: Option<&str>) -> String {
    match key {
        Some(key) if !key.is_empty() => message.replace(key, "<masked-key>"),
        _ => message.to_owned(),
    }
}

#[cfg(all(test, any(target_os = "linux", target_os = "macos")))]
mod tests {
    use super::*;
    use crate::transport::test_support::{ScriptedChrome, read_http_request_head};
    use std::io::{BufRead, BufReader, Write};
    use std::net::TcpListener;
    use std::process::Command;

    #[test]
    fn prepared_spawn_cleans_its_profile_on_success_and_failure() {
        let config = BrowserConfig {
            explicit_binary: None,
            headless: true,
            width: 800,
            height: 600,
            inherit_process_group: false,
        };
        let success_profile = private_profile().unwrap();
        let starting = PreparedBrowser {
            binary: PathBuf::from("/bin/sleep"),
            profile: success_profile.clone(),
            config: config.clone(),
        }
        .spawn()
        .unwrap();
        drop(starting);
        assert!(!success_profile.exists());

        let temporary = tempfile::tempdir().unwrap();
        let failed_profile = private_profile().unwrap();
        let error = PreparedBrowser {
            binary: temporary.path().to_path_buf(),
            profile: failed_profile.clone(),
            config,
        }
        .spawn()
        .err()
        .expect("invalid executable must fail to spawn");
        assert!(matches!(error, BrowserError::Launch(_)));
        assert!(!failed_profile.exists());
    }

    #[test]
    fn private_profile_is_owner_only_and_startup_exit_removes_it() {
        use std::os::unix::fs::PermissionsExt;

        let temporary = tempfile::tempdir().unwrap();
        let binary = temporary.path().join("exits-before-cdp");
        fs::write(&binary, "#!/bin/sh\nexit 17\n").unwrap();
        fs::set_permissions(&binary, fs::Permissions::from_mode(0o700)).unwrap();
        let profile = private_profile().unwrap();
        assert_eq!(
            profile.metadata().unwrap().permissions().mode() & 0o777,
            0o700
        );
        let error = PreparedBrowser {
            binary,
            profile: profile.clone(),
            config: BrowserConfig {
                explicit_binary: None,
                headless: true,
                width: 800,
                height: 600,
                inherit_process_group: false,
            },
        }
        .launch()
        .err()
        .expect("browser that exits before CDP must fail startup");
        assert!(
            matches!(error, BrowserError::Launch(message) if message.contains("before the CDP endpoint"))
        );
        assert!(!profile.exists());
    }

    #[test]
    fn missing_explicit_executable_is_structured_before_profile_creation() {
        let missing = tempfile::tempdir().unwrap().path().join("missing-browser");
        let error = PreparedBrowser::new(BrowserConfig {
            explicit_binary: Some(missing),
            headless: true,
            width: 800,
            height: 600,
            inherit_process_group: false,
        })
        .err()
        .expect("missing explicit browser must be rejected");
        assert!(matches!(error, BrowserError::Unavailable));
    }

    #[test]
    fn starting_browser_installs_probe_and_viewport_before_ownership_transfer() {
        let chrome = ScriptedChrome::start();
        chrome.reply("Page.addScriptToEvaluateOnNewDocument", json!({}));
        chrome.reply("Runtime.evaluate", json!({"result":{"value":null}}));
        chrome.reply("Emulation.setDeviceMetricsOverride", json!({}));
        chrome.reply("Emulation.setFocusEmulationEnabled", json!({}));
        let client = chrome.connect_raw();
        let profile = private_profile().unwrap();
        let mut command = Command::new("sleep");
        command.arg("30");
        let (child, ownership) = platform::spawn(command, false).unwrap();
        let starting = StartingBrowser {
            prepared: Some(PreparedBrowser {
                binary: PathBuf::from("/usr/bin/chromium"),
                profile: profile.clone(),
                config: BrowserConfig {
                    explicit_binary: None,
                    headless: true,
                    width: 800,
                    height: 600,
                    inherit_process_group: false,
                },
            }),
            child: Some(child),
            ownership: Some(ownership),
        };
        let mut browser = starting.finish(client, "Chromium Test".into()).unwrap();
        assert_eq!(
            chrome
                .commands()
                .into_iter()
                .map(|(method, _)| method)
                .collect::<Vec<_>>(),
            [
                "Page.addScriptToEvaluateOnNewDocument",
                "Runtime.evaluate",
                "Emulation.setDeviceMetricsOverride",
                "Emulation.setFocusEmulationEnabled",
            ]
        );
        assert_eq!(
            chrome.received("Page.addScriptToEvaluateOnNewDocument")[0]["params"]["source"],
            COVERAGE_PROBE
        );
        assert_eq!(
            chrome.received("Runtime.evaluate")[0]["params"]["expression"],
            COVERAGE_PROBE
        );
        assert_eq!(
            chrome.received("Emulation.setDeviceMetricsOverride")[0]["params"],
            json!({"width":800,"height":600,"deviceScaleFactor":1,"mobile":false})
        );
        assert_eq!(
            chrome.received("Emulation.setFocusEmulationEnabled")[0]["params"],
            json!({"enabled":true})
        );
        assert_eq!(browser.provenance().viewport.width, 800);
        assert_eq!(browser.provenance().browser_version, "Chromium Test");
        browser.close().unwrap();
        assert!(!profile.exists());
    }

    #[test]
    fn owned_browser_command_is_direct_loopback_ephemeral_and_scrubs_the_provider_key() {
        let temporary = tempfile::tempdir().unwrap();
        let binary = Path::new("/direct/app-bundle/browser");
        let config = BrowserConfig {
            explicit_binary: None,
            headless: false,
            width: 1120,
            height: 780,
            inherit_process_group: false,
        };
        let command = browser_command(binary, temporary.path(), &config);
        assert_eq!(command.get_program(), binary);
        assert_eq!(
            command.get_args().collect::<Vec<_>>(),
            [
                "--remote-debugging-address=127.0.0.1",
                "--remote-debugging-port=0",
                &format!("--user-data-dir={}", temporary.path().display()),
                "--no-first-run",
                "--no-default-browser-check",
                "--disable-background-networking",
                "--window-size=1120,780",
                "about:blank",
            ]
        );
        assert!(
            command
                .get_envs()
                .any(|(name, value)| { name == "TYPESAFE_API_KEY" && value.is_none() })
        );
    }

    #[test]
    fn headless_browser_command_declares_a_desktop_mouse() {
        let temporary = tempfile::tempdir().unwrap();
        let config = BrowserConfig {
            explicit_binary: None,
            headless: true,
            width: 1120,
            height: 780,
            inherit_process_group: false,
        };
        let command = browser_command(Path::new("/browser"), temporary.path(), &config);
        assert!(command.get_args().any(|arg| arg == "--blink-settings=primaryHoverType=2,availableHoverTypes=2,primaryPointerType=4,availablePointerTypes=4"));
    }

    #[test]
    fn mask_script_does_not_embed_json_as_executable_source() {
        let values = vec![
            "x'; throw new Error('leak')//".to_owned(),
            "\"]); alert(1); ([\"".to_owned(),
        ];
        let script = masking_script(&values).unwrap();
        let argument = script
            .strip_prefix(&format!("({})(", MASKING.trim_end()))
            .and_then(|rest| rest.strip_suffix(')'))
            .expect("masking.js applied to one argument");
        assert_eq!(
            serde_json::from_str::<Vec<String>>(argument).unwrap(),
            values,
            "the argument is a JSON array literal holding exactly the values"
        );
    }

    #[test]
    fn a_failed_profile_removal_is_retried_without_terminating_again() {
        let chrome = ScriptedChrome::start();
        let mut browser = browser_with_client(chrome.connect_raw());
        let profile = browser.profile.clone();
        fs::remove_dir_all(&profile).unwrap();
        assert!(matches!(browser.close(), Err(BrowserError::Control(_))));
        assert_eq!(browser.lifecycle, Lifecycle::Terminated);
        assert!(browser.child.try_wait().unwrap().is_some());

        fs::create_dir(&profile).unwrap();
        browser.close().unwrap();
        assert_eq!(browser.lifecycle, Lifecycle::Closed);
        assert!(!profile.exists());
        browser.close().unwrap();
    }

    #[test]
    fn navigation_reported_failed_by_chrome_is_an_error() {
        let chrome = ScriptedChrome::start();
        chrome.reply(
            "Page.navigate",
            json!({"frameId":"main","errorText":"net::ERR_CONNECTION_REFUSED"}),
        );
        let browser = browser_with_client(chrome.connect_raw());
        assert!(matches!(
            browser.navigate("http://127.0.0.1:9/"),
            Err(BrowserError::Control(message)) if message == "navigation failed: net::ERR_CONNECTION_REFUSED"
        ));
        assert!(
            chrome.received("Runtime.evaluate").is_empty(),
            "no wait for a document that never loaded"
        );
    }

    #[test]
    fn an_empty_or_absent_provider_key_leaves_messages_unchanged() {
        assert_eq!(
            mask_provider_key("launch failed", Some("")),
            "launch failed"
        );
        assert_eq!(mask_provider_key("launch failed", None), "launch failed");
        assert_eq!(
            mask_provider_key("bad sk-123 key", Some("sk-123")),
            "bad <masked-key> key"
        );
    }

    #[test]
    fn observation_uses_one_snapshot_round_trip_when_focus_is_absent() {
        let chrome = ScriptedChrome::start();
        chrome.reply(
            "Runtime.evaluate",
            json!({"result":{"value":snapshot_value()}}),
        );
        let browser = browser_with_client(chrome.connect_raw());
        browser.observe().unwrap();
        assert_eq!(
            chrome
                .commands()
                .iter()
                .map(|(method, _)| method.as_str())
                .collect::<Vec<_>>(),
            ["Runtime.evaluate"]
        );
        assert_eq!(
            chrome.received("Runtime.evaluate")[0]["params"]["expression"],
            SNAPSHOT
        );
    }

    #[test]
    fn observation_reports_popups_and_focus_inside_a_closed_shadow_host() {
        let chrome = ScriptedChrome::start();
        let mut snapshot = snapshot_value();
        snapshot["focus_anchor"] = json!({
            "node_id":7,"context":"main","role":"interactive","name":"",
            "in_dialog":null,"covered":true
        });
        chrome.reply("Runtime.evaluate", json!({"result":{"value":snapshot}}));
        chrome.reply(
            "Runtime.evaluate",
            json!({"result":{"type":"object","objectId":"focus-7"}}),
        );
        chrome.reply(
            "DOM.describeNode",
            json!({"node":{"shadowRoots":[{"shadowRootType":"closed"}]}}),
        );
        chrome.push_event(
            "Page.windowOpen",
            json!({"url":"http://example.test/popup"}),
        );
        let client = chrome.connect_raw();
        command(&client, "Page.enable", json!({})).unwrap();
        command(&client, "Page.enable", json!({})).unwrap();
        let observed = browser_with_client(client).observe().unwrap();
        let anchor = observed.focus_anchor.unwrap();
        assert!(!anchor.covered);
        assert_eq!(anchor.surface, Some(crate::FocusSurface::ClosedShadowRoot));
        assert_eq!(observed.coverage.gaps, ["popup"]);
        assert_eq!(chrome.received("Runtime.releaseObjectGroup").len(), 1);
    }

    #[test]
    fn cdp_observation_capture_and_navigation_helpers_are_scriptable() {
        let chrome = ScriptedChrome::start();
        chrome.reply(
            "Runtime.evaluate",
            json!({"result":{"value":snapshot_value()}}),
        );
        let mut png = b"\x89PNG\r\n\x1a\n".to_vec();
        png.extend_from_slice(&[0; 8]);
        png.extend_from_slice(&1u32.to_be_bytes());
        png.extend_from_slice(&1u32.to_be_bytes());
        chrome.reply(
            "Page.captureScreenshot",
            json!({"data":base64::Engine::encode(&base64::engine::general_purpose::STANDARD,&png)}),
        );
        let client = chrome.connect_raw();
        let browser = browser_with_client(client);
        assert_eq!(browser.observe().unwrap().title, "Fixture");
        assert_eq!(browser.capture().unwrap().screenshot.width, 1);
        assert_eq!(browser.capture_redacted(&[]).unwrap().screenshot.height, 1);
        chrome.reply("Runtime.evaluate", json!({"result":{"value":"complete"}}));
        chrome.push_event("DOM.childNodeInserted", json!({}));
        browser.navigate("http://example.test/").unwrap();
        assert!(matches!(
            command_failure(CommandFailure::Rejected(json!({}))).as_str(),
            "CDP command rejected"
        ));
        assert_eq!(
            command_failure(CommandFailure::NotSent("x".into())),
            "CDP command not sent"
        );
        assert_eq!(
            command_failure(CommandFailure::Unknown("x".into())),
            "CDP command outcome unknown"
        );
    }

    #[test]
    fn input_navigation_waits_until_every_started_frame_stops_loading() {
        let chrome = Arc::new(ScriptedChrome::start());
        chrome.reply("Runtime.evaluate", json!({"result":{"value":"complete"}}));
        let client = chrome.connect_raw();
        let fence = client.cursor();
        for (method, frame) in [
            ("Page.frameRequestedNavigation", "main"),
            ("Page.frameStartedLoading", "main"),
            ("Page.frameStartedLoading", "child"),
            ("Page.frameStoppedLoading", "child"),
        ] {
            chrome.push_event(method, json!({"frameId":frame}));
        }
        let delay = Duration::from_millis(400);
        let stopper = {
            let chrome = Arc::clone(&chrome);
            thread::spawn(move || {
                thread::sleep(delay);
                chrome.push_event("Page.frameStoppedLoading", json!({"frameId":"main"}));
            })
        };
        let started = Instant::now();
        settle_input_navigation(&client, fence).unwrap();
        assert!(started.elapsed() >= delay);
        stopper.join().unwrap();
        let events = client.snapshot_since(fence).events;
        assert!(starts_navigation(&events[0]));
        assert!(!starts_navigation(&events[3]));
        assert_eq!(loading_frames(&events[..4]), HashSet::from(["main"]));
        assert!(loading_frames(&events).is_empty());
    }

    #[test]
    fn input_settling_waits_for_the_document_only_after_a_navigation_signal() {
        let chrome = ScriptedChrome::start();
        chrome.reply("Runtime.evaluate", json!({"result":{"value":"complete"}}));
        let client = chrome.connect_raw();
        settle_input_navigation(&client, client.cursor()).unwrap();
        assert!(
            chrome.received("Runtime.evaluate").is_empty(),
            "without a navigation signal no document wait is needed"
        );

        let fence = client.cursor();
        chrome.emit_before_reply(
            "Page.enable",
            1,
            vec![("Page.frameNavigated", json!({"frame":{"id":"main"}}))],
        );
        command(&client, "Page.enable", json!({})).unwrap();
        let started = Instant::now();
        settle_input_navigation(&client, fence).unwrap();
        assert!(started.elapsed() >= QUIET_WINDOW);
        let checks = chrome.received("Runtime.evaluate");
        assert!(!checks.is_empty(), "a navigation holds for the document");
        assert!(
            checks
                .iter()
                .all(|check| check["params"]["expression"] == "document.readyState")
        );
    }

    /// Performs `operation` against a page whose final dispatch starts a
    /// navigation that stops loading only after `delay`.
    fn perform_starting_navigation(
        operation: PreparedOperation,
        revalidation: Value,
        final_dispatch: &str,
        delay: Duration,
    ) -> (PerformFact, Duration, Vec<String>) {
        let chrome = Arc::new(ScriptedChrome::start());
        chrome.reply("Runtime.evaluate", revalidation);
        chrome.reply("Runtime.evaluate", json!({"result":{"value":"complete"}}));
        chrome.emit_before_reply(
            final_dispatch,
            2,
            vec![
                ("Page.frameRequestedNavigation", json!({"frameId":"main"})),
                ("Page.frameStartedLoading", json!({"frameId":"main"})),
            ],
        );
        let mut browser = browser_with_client(chrome.connect_raw());
        let stopper = {
            let chrome = Arc::clone(&chrome);
            let final_dispatch = final_dispatch.to_owned();
            thread::spawn(move || {
                while chrome.received(&final_dispatch).len() < 2 {
                    thread::sleep(Duration::from_millis(5));
                }
                thread::sleep(delay);
                chrome.push_event("Page.frameStoppedLoading", json!({"frameId":"main"}));
            })
        };
        let started = Instant::now();
        let fact = browser
            .perform(
                PreparedInput {
                    scroll_region: None,
                    document_id: "d".into(),
                    node_id: 7,
                    operation,
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
        let elapsed = started.elapsed();
        stopper.join().unwrap();
        browser.close().unwrap();
        let methods = chrome
            .commands()
            .into_iter()
            .map(|(method, params)| match params["expression"].as_str() {
                Some("document.readyState") => "readyState".to_owned(),
                _ => method,
            })
            .collect();
        (fact, elapsed, methods)
    }

    #[test]
    fn a_confirmed_click_waits_for_the_navigation_it_started_to_settle() {
        let delay = Duration::from_millis(300);
        let (fact, elapsed, methods) = perform_starting_navigation(
            PreparedOperation::Click,
            json!({"result":{"value":{"ok":true,"x":12.0,"y":20.0}}}),
            "Input.dispatchMouseEvent",
            delay,
        );
        assert_eq!(fact.suboperations, ["mouse_press", "mouse_release"]);
        assert!(elapsed >= delay + QUIET_WINDOW, "{elapsed:?}");
        assert_eq!(
            methods[..4],
            [
                "Runtime.evaluate",
                "Input.dispatchMouseEvent",
                "Input.dispatchMouseEvent",
                "readyState",
            ]
        );
    }

    #[test]
    fn a_key_press_waits_for_the_navigation_it_started_to_settle() {
        let delay = Duration::from_millis(300);
        let (fact, elapsed, methods) = perform_starting_navigation(
            PreparedOperation::PressKey(crate::Key::Enter),
            json!({"result":{"value":{
                "document_id":"d","url":"http://example.test/","route":"/","title":"x",
                "focused":null,"focus_anchor":null,"elements":[],
                "viewport":{"width":10,"height":10,"scroll_x":0,"scroll_y":0,"document_height":10}
            }}}),
            "Input.dispatchKeyEvent",
            delay,
        );
        assert_eq!(fact.suboperations, ["key_down", "key_up"]);
        assert!(elapsed >= delay + QUIET_WINDOW, "{elapsed:?}");
        assert_eq!(
            methods[..4],
            [
                "Runtime.evaluate",
                "Input.dispatchKeyEvent",
                "Input.dispatchKeyEvent",
                "readyState",
            ]
        );
    }

    #[test]
    fn owned_perform_lets_a_hover_settle_before_the_next_observation() {
        let chrome = ScriptedChrome::start();
        chrome.reply(
            "Runtime.evaluate",
            json!({"result":{"value":{"ok":true,"x":12.0,"y":20.0}}}),
        );
        let mut browser = browser_with_client(chrome.connect_raw());
        let perform = |operation| {
            let started = Instant::now();
            let fact = browser
                .perform(
                    PreparedInput {
                        scroll_region: None,
                        document_id: "d".into(),
                        node_id: 7,
                        operation,
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
            (fact.suboperations, started.elapsed())
        };

        let (hover, hover_duration) = perform(PreparedOperation::Hover);
        assert_eq!(hover, ["mouse_move"]);
        assert!(hover_duration >= QUIET_WINDOW, "{hover_duration:?}");
        let (scroll, _) = perform(PreparedOperation::ScrollDown);
        assert_eq!(scroll, ["scroll_down"]);
        browser.close().unwrap();
    }

    #[test]
    fn unverifiable_geometry_withholds_the_screenshot() {
        let chrome = ScriptedChrome::start();
        chrome.reply(
            "Runtime.evaluate",
            json!({"result":{"value":{"unverifiable":true}}}),
        );
        let browser = browser_with_client(chrome.connect_raw());
        assert!(
            matches!(browser.capture_redacted(&["secret".into()]),Err(BrowserError::Control(message)) if message=="redaction_unverifiable")
        );
    }

    #[test]
    fn zero_mask_capture_requires_complete_redaction_proof() {
        let proof = masking_proof(
            json!({"verified":true,"sensitive_values_checked":1,"matched_values":0,"mask_count":0}),
            1,
        )
        .unwrap();
        assert!(proof.verifies(1));
        for invalid in [
            json!({"verified":true,"sensitive_values_checked":0,"matched_values":0,"mask_count":0}),
            json!({"verified":false,"sensitive_values_checked":1,"matched_values":0,"mask_count":0}),
            json!({"verified":true,"sensitive_values_checked":1,"matched_values":1,"mask_count":0}),
            json!({"masked":0}),
        ] {
            assert!(matches!(
                masking_proof(invalid, 1),
                Err(BrowserError::Control(message)) if message == "redaction_unverifiable"
            ));
        }
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn endpoint_and_page_discovery_helpers_use_owned_loopback_files_and_http() {
        let temporary = tempfile::tempdir().unwrap();
        fs::write(
            temporary.path().join("DevToolsActivePort"),
            "45678\n/devtools/browser/x\n",
        )
        .unwrap();
        let mut child = Command::new("sleep").arg("30").spawn().unwrap();
        assert_eq!(
            wait_for_endpoint(&mut child, temporary.path(), Duration::from_millis(50))
                .unwrap()
                .label(),
            "127.0.0.1:45678"
        );
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let worker = thread::spawn(move || {
            for _ in 0..2 {
                let (mut stream, _) = listener.accept().unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(2)))
                    .unwrap();
                let head = read_http_request_head(&mut stream).unwrap();
                let body = if head.starts_with("GET /json/version ") {
                    json!({"Browser":"Chromium Test"}).to_string()
                } else {
                    json!([{"type":"page","webSocketDebuggerUrl":"ws://127.0.0.1/devtools/page/1"}])
                        .to_string()
                };
                write!(
                    stream,
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                )
                .unwrap();
            }
        });
        let page = wait_for_page(
            &mut child,
            &Endpoint::parse(&address.to_string()).unwrap(),
            Duration::from_secs(1),
        );
        child.kill().unwrap();
        child.wait().unwrap();
        worker.join().unwrap();
        let page = page.unwrap();
        assert_eq!(page.0, "ws://127.0.0.1/devtools/page/1");
        assert_eq!(page.1, "Chromium Test");
    }

    fn assert_exits(pid: u32) {
        let pid = process::pid_t(pid).unwrap();
        let deadline = Instant::now() + Duration::from_secs(1);
        while unsafe { libc::kill(pid, 0) } == 0 {
            assert!(Instant::now() < deadline, "process {pid} survived close");
            thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(
            std::io::Error::last_os_error().raw_os_error(),
            Some(libc::ESRCH)
        );
    }

    #[test]
    fn close_ends_the_owned_process_group_and_removes_only_its_profile() {
        let profile = private_profile().unwrap();
        let unrelated = private_profile().unwrap();
        let chrome = ScriptedChrome::start();
        let mut command = Command::new("/bin/sh");
        command
            .args(["-c", "sleep 30 & echo $!; wait"])
            .stdout(Stdio::piped());
        let (mut child, ownership) = platform::spawn(command, false).unwrap();
        let mut line = String::new();
        BufReader::new(child.stdout.take().unwrap())
            .read_line(&mut line)
            .unwrap();
        let descendant: u32 = line.trim().parse().unwrap();
        let leader = child.id();
        let mut browser = owned_browser(child, ownership, profile.clone(), chrome.connect_raw());

        browser.close().unwrap();
        assert_exits(leader);
        assert_exits(descendant);
        assert!(!profile.exists());
        assert!(unrelated.is_dir(), "another profile is never removed");
        browser.close().unwrap();
        fs::remove_dir(unrelated).unwrap();
    }

    fn snapshot_value() -> Value {
        json!({"document_id":"d","url":"http://example.test/","route":"/","title":"Fixture","dialogs":[],"focused":null,"visible_text":"Ready","covered_text":"","dialog_texts":{},"elements":[],"viewport":{"width":800,"height":600,"scroll_x":0.0,"scroll_y":0.0,"document_height":600.0},"coverage":{"viewport_complete":true,"open_shadow_roots":true,"slots":true,"same_origin_frames":true,"gaps":[]}})
    }

    fn browser_with_client(client: Arc<CdpClient>) -> OwnedBrowser {
        let profile = tempfile::tempdir().unwrap().keep();
        let child = Command::new("sleep").arg("30").spawn().unwrap();
        let ownership = platform::ownership_for_test(&child, false);
        owned_browser(child, ownership, profile, client)
    }

    fn owned_browser(
        child: Child,
        ownership: platform::BrowserOwnership,
        profile: PathBuf,
        client: Arc<CdpClient>,
    ) -> OwnedBrowser {
        OwnedBrowser {
            child,
            profile,
            client,
            provenance: BrowserProvenance {
                browser_path: "fake".into(),
                browser_version: "fake".into(),
                viewport: ProvenanceViewport {
                    width: 800,
                    height: 600,
                },
                display_mode: "headless".into(),
            },
            ownership,
            lifecycle: Lifecycle::Running,
        }
    }
}
