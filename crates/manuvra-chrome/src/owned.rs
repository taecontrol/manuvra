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
#[cfg(any(target_os = "linux", target_os = "macos"))]
const COVERAGE_PROBE: &str = r#"(() => {
  if (window.__manuvraCoverageProbeInstalled) return;
  window.__manuvraCoverageProbeInstalled = true;
  window.__manuvraClosedShadowRoots = 0;
  const original = Element.prototype.attachShadow;
  Element.prototype.attachShadow = function(init) {
    const root = original.call(this, init);
    if (init?.mode === 'closed') window.__manuvraClosedShadowRoots += 1;
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
    closed: bool,
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
        command(&self.client, "Page.navigate", json!({"url": url}))?;
        wait_for_document(&self.client, fence)
    }

    pub fn observe(&self) -> Result<Observation, BrowserError> {
        let value = evaluate(&self.client, SNAPSHOT)?;
        let mut observation: Observation = serde_json::from_value(value)
            .map_err(|error| BrowserError::InvalidObservation(error.to_string()))?;
        if self.client.snapshot_since(0).events.iter().any(|event| {
            matches!(
                event.message.get("method").and_then(Value::as_str),
                Some("Page.windowOpen" | "Target.targetCreated")
            )
        }) {
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
        let settle_navigation = input.operation == PreparedOperation::Click;
        let fence = self.client.cursor();
        let fact = crate::input::perform(&self.client, input, cancellation)?;
        if settle_navigation {
            settle_click_navigation(&self.client, fence)
                .map_err(|error| PerformError::Uncertain(error.to_string()))?;
        }
        Ok(fact)
    }

    pub fn capture(&self) -> Result<CapturedPage, BrowserError> {
        for _ in 0..3 {
            let fence = self.client.cursor();
            let observation = self.observe()?;
            let screenshot = page::capture_screenshot(
                &self.client,
                deadline(),
                Arc::new(AtomicBool::new(false)),
            )
            .map_err(|error| BrowserError::Control(error.to_string()))?;
            let changed = self
                .client
                .snapshot_since(fence)
                .events
                .iter()
                .any(crate::transport::is_relevant_event);
            if !changed {
                return Ok(CapturedPage {
                    observation,
                    screenshot,
                    redaction: RedactionProof {
                        sensitive_values_checked: 0,
                        matched_values: 0,
                        mask_count: 0,
                    },
                });
            }
            thread::sleep(QUIET_WINDOW);
        }
        Err(BrowserError::Control(
            "page changed throughout screenshot fencing".into(),
        ))
    }

    pub fn capture_redacted(&self, sensitive: &[String]) -> Result<CapturedPage, BrowserError> {
        if sensitive.is_empty() {
            return self.capture();
        }
        self.capture_with_masks(sensitive)
    }

    fn capture_with_masks(&self, sensitive: &[String]) -> Result<CapturedPage, BrowserError> {
        let expression = masking_script(sensitive)?;
        let proof = match masking_proof(evaluate(&self.client, &expression)?, sensitive.len()) {
            Ok(proof) => proof,
            Err(_) => return self.unverifiable_mask(),
        };
        self.capture_then_unmask(proof)
    }

    fn unverifiable_mask(&self) -> Result<CapturedPage, BrowserError> {
        let _ = evaluate(&self.client, "window.__manuvraRemoveMasks?.()");
        Err(BrowserError::Control("redaction_unverifiable".into()))
    }

    fn capture_then_unmask(&self, proof: RedactionProof) -> Result<CapturedPage, BrowserError> {
        let _masks = InstalledMasks {
            client: &self.client,
        };
        let mut captured = self.capture()?;
        captured.redaction = proof;
        Ok(captured)
    }

    pub fn close(&mut self) -> Result<(), BrowserError> {
        if self.closed {
            return Ok(());
        }
        platform::terminate(&mut self.child, &mut self.ownership).map_err(BrowserError::Control)?;
        fs::remove_dir_all(&self.profile)
            .map_err(|error| BrowserError::Control(safe_error(&error.to_string())))?;
        self.closed = true;
        Ok(())
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

struct InstalledMasks<'a> {
    client: &'a CdpClient,
}
impl Drop for InstalledMasks<'_> {
    fn drop(&mut self) {
        let _ = evaluate(self.client, "window.__manuvraRemoveMasks?.(); true");
    }
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
            closed: false,
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
        command.arg("--headless=new").arg("--disable-gpu");
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

fn settle_click_navigation(client: &CdpClient, fence: u64) -> Result<(), BrowserError> {
    let end = Instant::now() + QUIET_WINDOW;
    loop {
        let snapshot = client.snapshot_since(fence);
        require_navigation_journal(&snapshot)?;
        if snapshot.events.iter().any(|event| {
            event.message.get("method").and_then(Value::as_str) == Some("Page.frameNavigated")
        }) {
            return wait_for_document(client, fence);
        }
        if Instant::now() >= end {
            return Ok(());
        }
        client.wait_for_journal_change(snapshot.last_cursor, Duration::from_millis(25));
    }
}

fn update_quiet_since(
    quiet_since: &mut Option<Instant>,
    ready: bool,
    events: &[crate::JournalEvent],
) {
    if events.iter().any(crate::transport::is_relevant_event) {
        *quiet_since = None
    } else if ready {
        quiet_since.get_or_insert_with(Instant::now);
    }
}

fn require_navigation_journal(snapshot: &crate::JournalSnapshot) -> Result<(), BrowserError> {
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

fn masking_script(sensitive: &[String]) -> Result<String, BrowserError> {
    let values = serde_json::to_string(sensitive)
        .map_err(|error| BrowserError::Control(error.to_string()))?;
    Ok(format!(
        r#"(() => {{
      window.__manuvraRemoveMasks?.(); const values={values}; const masks=[]; const matched=new Set(); let unverifiable=false;
      const contexts=[],seen=new Set();const visit=(root,x,y)=>{{if(!root||seen.has(root))return;seen.add(root);contexts.push({{root,x,y}});if(root.defaultView?.__manuvraClosedShadowRoots>0)unverifiable=true;for(const e of root.querySelectorAll('*')){{if(e.shadowRoot)visit(e.shadowRoot,x,y);if(e.tagName==='IFRAME'||e.tagName==='FRAME'){{let child;try{{child=e.contentDocument;}}catch(_){{child=null;}}if(!child?.body){{unverifiable=true;continue;}}const r=e.getBoundingClientRect();visit(child,x+r.x,y+r.y);}}if(e.tagName==='CANVAS')unverifiable=true;}}}};visit(document,0,0);
      const cover=(rect,x,y)=>{{if(!rect||!Number.isFinite(rect.x)||rect.width<=0||rect.height<=0){{unverifiable=true;return;}}const m=document.createElement('div');Object.assign(m.style,{{position:'fixed',left:`${{rect.x+x}}px`,top:`${{rect.y+y}}px`,width:`${{rect.width}}px`,height:`${{rect.height}}px`,background:'#000',zIndex:'2147483647',pointerEvents:'none'}});document.documentElement.appendChild(m);masks.push(m);}};
      const matches=(text,coverMatch)=>{{values.forEach((value,index)=>{{if(!value)return;let start=0,found;while((found=String(text).indexOf(value,start))!==-1){{matched.add(index);coverMatch(found,found+value.length);start=found+Math.max(1,value.length);}}}});}};
      for(const context of contexts){{for(const e of context.root.querySelectorAll('*')){{if(e.matches('input,textarea,[contenteditable="true"]'))matches(String(e.value??e.innerText),()=>cover(e.getBoundingClientRect(),context.x,context.y));const view=e.ownerDocument?.defaultView||window;for(const pseudo of ['::before','::after']){{const content=view.getComputedStyle(e,pseudo).content;matches(content||'',()=>cover(e.getBoundingClientRect(),context.x,context.y));}}}}const owner=context.root.ownerDocument||context.root,walker=owner.createTreeWalker(context.root,NodeFilter.SHOW_TEXT),nodes=[];let text='',n;while(n=walker.nextNode()){{const start=text.length;text+=n.textContent||'';nodes.push({{node:n,start,end:text.length}});}}matches(text,(start,end)=>{{const first=nodes.find(item=>item.start<=start&&start<item.end),last=nodes.find(item=>item.start<end&&end<=item.end);if(!first||!last){{unverifiable=true;return;}}const range=owner.createRange();range.setStart(first.node,start-first.start);range.setEnd(last.node,end-last.start);const rects=[...range.getClientRects()];if(!rects.length)unverifiable=true;else rects.forEach(rect=>cover(rect,context.x,context.y));}});}}
      window.__manuvraRemoveMasks=()=>{{masks.forEach(m=>m.remove());delete window.__manuvraRemoveMasks;}}; return {{verified:!unverifiable,sensitive_values_checked:values.length,matched_values:matched.size,mask_count:masks.length}};
    }})()"#
    ))
}

fn safe_error(message: &str) -> String {
    message.replace(
        env::var("TYPESAFE_API_KEY").as_deref().unwrap_or("\0"),
        "<masked-key>",
    )
}

#[cfg(all(test, any(target_os = "linux", target_os = "macos")))]
mod tests {
    use super::*;
    use crate::transport::test_support::ScriptedChrome;
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    use std::io::{BufRead, BufReader, Write};
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    use std::net::TcpListener;
    #[cfg(target_os = "linux")]
    use std::os::unix::process::CommandExt;
    use std::process::Command;

    #[cfg(target_os = "linux")]
    #[test]
    fn discovery_prefers_explicit_then_environment_then_path() {
        let temporary = tempfile::tempdir().unwrap();
        let explicit = temporary.path().join("explicit");
        let environment = temporary.path().join("environment");
        let path_dir = temporary.path().join("bin");
        fs::create_dir(&path_dir).unwrap();
        let path_binary = path_dir.join("chromium");
        use std::os::unix::fs::PermissionsExt;
        for binary in [&explicit, &environment, &path_binary] {
            fs::write(binary, b"").unwrap();
            fs::set_permissions(binary, fs::Permissions::from_mode(0o700)).unwrap();
        }
        assert_eq!(
            platform::discover_binary_from(
                Some(&explicit),
                Some(environment.clone()),
                Some(path_dir.clone().into_os_string())
            )
            .unwrap(),
            fs::canonicalize(&explicit).unwrap()
        );
        assert_eq!(
            platform::discover_binary_from(
                None,
                Some(environment.clone()),
                Some(path_dir.clone().into_os_string())
            )
            .unwrap(),
            fs::canonicalize(&environment).unwrap()
        );
        assert_eq!(
            platform::discover_binary_from(None, None, Some(path_dir.into_os_string())).unwrap(),
            fs::canonicalize(&path_binary).unwrap()
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn discovery_rejects_a_non_executable_file() {
        let temporary = tempfile::tempdir().unwrap();
        let binary = temporary.path().join("chromium");
        let path_dir = temporary.path().join("bin");
        fs::create_dir(&path_dir).unwrap();
        let fallback = path_dir.join("chromium");
        fs::write(&binary, b"").unwrap();
        fs::write(&fallback, b"").unwrap();
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&fallback, fs::Permissions::from_mode(0o700)).unwrap();
        assert!(matches!(
            platform::discover_binary(Some(&binary), None, None),
            Err(BrowserError::Unavailable)
        ));
        assert!(matches!(
            platform::discover_binary_from(None, Some(binary), Some(path_dir.into_os_string())),
            Err(BrowserError::Unavailable)
        ));
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
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
        assert_eq!(browser.provenance().viewport.width, 800);
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
        assert!(
            command
                .get_args()
                .any(|arg| arg == "--remote-debugging-address=127.0.0.1")
        );
        assert!(
            command
                .get_args()
                .any(|arg| arg == "--remote-debugging-port=0")
        );
        assert!(
            command
                .get_envs()
                .any(|(name, value)| { name == "TYPESAFE_API_KEY" && value.is_none() })
        );
    }

    #[test]
    fn mask_script_does_not_embed_json_as_executable_source() {
        let script = masking_script(&["x'; throw new Error('leak')//".into()]).unwrap();
        assert!(script.contains("throw new Error"));
        assert!(script.contains("const values=[\"x'; throw"));
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
    fn confirmed_click_navigation_settles_before_the_next_observation() {
        let chrome = ScriptedChrome::start();
        chrome.reply("Runtime.evaluate", json!({"result":{"value":"complete"}}));
        let client = chrome.connect_raw();
        let fence = client.cursor();
        chrome.push_event("Page.frameNavigated", json!({"frame":{"id":"main"}}));
        command(&client, "Page.enable", json!({})).unwrap();

        settle_click_navigation(&client, fence).unwrap();
    }

    #[test]
    fn owned_perform_routes_confirmed_click_through_navigation_settling() {
        let chrome = ScriptedChrome::start();
        chrome.reply(
            "Runtime.evaluate",
            json!({"result":{"value":{"ok":true,"x":12.0,"y":20.0}}}),
        );
        let client = chrome.connect_raw();
        let mut browser = browser_with_client(client);
        let fact = browser
            .perform(
                PreparedInput {
                    document_id: "d".into(),
                    node_id: 7,
                    operation: PreparedOperation::Click,
                    text: None,
                    previous_text: None,
                    option_node_id: None,
                    combobox: false,
                    action_sequence: 1,
                },
                &InputCancellation::default(),
            )
            .unwrap();
        assert_eq!(fact.suboperations, ["mouse_press", "mouse_release"]);
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
                .port(),
            45678
        );
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let worker = thread::spawn(move || {
            for _ in 0..2 {
                let (mut stream, _) = listener.accept().unwrap();
                let mut line = String::new();
                BufReader::new(stream.try_clone().unwrap())
                    .read_line(&mut line)
                    .unwrap();
                let body = if line.contains("/json/version") {
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
        )
        .unwrap();
        child.kill().unwrap();
        child.wait().unwrap();
        worker.join().unwrap();
        assert_eq!(page.0, "ws://127.0.0.1/devtools/page/1");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn private_profile_and_close_remove_only_owned_process_and_directory() {
        let profile = private_profile().unwrap();
        assert!(profile.is_dir());
        let chrome = ScriptedChrome::start();
        let client = chrome.connect_raw();
        let mut command = Command::new("sleep");
        command.arg("30").process_group(0);
        let child = command.spawn().unwrap();
        let ownership = platform::ownership_for_test(&child, true);
        let mut browser = OwnedBrowser {
            child,
            profile: profile.clone(),
            client,
            provenance: BrowserProvenance {
                browser_path: "fake".into(),
                browser_version: "fake".into(),
                viewport: ProvenanceViewport {
                    width: 1,
                    height: 1,
                },
                display_mode: "headless".into(),
            },
            ownership,
            closed: false,
        };
        browser.close().unwrap();
        assert!(!profile.exists());
        browser.close().unwrap();
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn close_terminates_only_the_browser_when_it_inherits_the_host_group() {
        let profile = private_profile().unwrap();
        let chrome = ScriptedChrome::start();
        let client = chrome.connect_raw();
        let child = Command::new("sleep").arg("30").spawn().unwrap();
        let pid = child.id();
        let ownership = platform::ownership_for_test(&child, false);
        let mut browser = OwnedBrowser {
            child,
            profile,
            client,
            provenance: BrowserProvenance {
                browser_path: "fake".into(),
                browser_version: "fake".into(),
                viewport: ProvenanceViewport {
                    width: 1,
                    height: 1,
                },
                display_mode: "headless".into(),
            },
            ownership,
            closed: false,
        };
        browser.close().unwrap();
        assert!(!Path::new(&format!("/proc/{pid}")).exists());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn close_terminates_the_entire_owned_process_group() {
        let profile = private_profile().unwrap();
        let chrome = ScriptedChrome::start();
        let client = chrome.connect_raw();
        let mut command = Command::new("sh");
        command
            .args([
                "-c",
                "sh -c 'trap \"\" TERM; exec sleep 30' & echo $!; wait",
            ])
            .process_group(0)
            .stdout(Stdio::piped());
        let mut child = command.spawn().unwrap();
        let mut descendant = String::new();
        BufReader::new(child.stdout.take().unwrap())
            .read_line(&mut descendant)
            .unwrap();
        let descendant: i32 = descendant.trim().parse().unwrap();
        let ownership = platform::ownership_for_test(&child, true);
        let mut browser = OwnedBrowser {
            child,
            profile,
            client,
            provenance: BrowserProvenance {
                browser_path: "fake".into(),
                browser_version: "fake".into(),
                viewport: ProvenanceViewport {
                    width: 1,
                    height: 1,
                },
                display_mode: "headless".into(),
            },
            ownership,
            closed: false,
        };
        browser.close().unwrap();
        assert_eq!(
            unsafe { libc::kill(descendant, 0) },
            -1,
            "signal-ignoring descendant survived owned cleanup"
        );
        assert_eq!(
            std::io::Error::last_os_error().raw_os_error(),
            Some(libc::ESRCH)
        );
    }

    fn snapshot_value() -> Value {
        json!({"document_id":"d","url":"http://example.test/","route":"/","title":"Fixture","dialogs":[],"focused":null,"visible_text":"Ready","covered_text":"","dialog_texts":{},"elements":[],"viewport":{"width":800,"height":600,"scroll_x":0.0,"scroll_y":0.0,"document_height":600.0},"coverage":{"viewport_complete":true,"open_shadow_roots":true,"slots":true,"same_origin_frames":true,"gaps":[]}})
    }

    fn browser_with_client(client: Arc<CdpClient>) -> OwnedBrowser {
        let profile = tempfile::tempdir().unwrap().keep();
        let child = Command::new("sleep").arg("30").spawn().unwrap();
        let ownership = platform::ownership_for_test(&child, false);
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
            closed: false,
        }
    }
}
