mod endpoint;
pub mod launch;
mod page;
mod transport;

pub use endpoint::{Endpoint, EndpointError};
pub use launch::{GOOGLE_CHROME_MACOS, LaunchError, LaunchRequest, launch_dedicated_chrome};

use endpoint::Endpoint as ChromeEndpoint;
use manuvra_runtime::{
    AdapterArtifact, AdapterContext, AdapterError, AdapterOperation, AdapterReply, AdapterSession,
    TargetAdapter, TargetDescriptor,
};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use transport::{CdpClient, JournalEvent, is_log_event};

const DISCOVERY_TIMEOUT: Duration = Duration::from_millis(200);
const CHROME_OWNER: &str = "Chrome";

pub struct ChromeAdapter {
    endpoints: Vec<ChromeEndpoint>,
    state: Mutex<State>,
}

#[derive(Default)]
struct State {
    next_generation: u64,
    targets: HashMap<String, TargetRecord>,
    sessions: HashMap<String, SessionCursor>,
    diagnostics: HashMap<String, String>,
    timings: HashMap<String, Vec<Value>>,
}

struct TargetRecord {
    descriptor: TargetDescriptor,
    endpoint: ChromeEndpoint,
    websocket_path: String,
    source_id: String,
    present: bool,
    observation: Option<Arc<CdpClient>>,
    raw: Option<Arc<CdpClient>>,
}

struct SessionCursor {
    target_id: String,
    cursor: u64,
    reference_epoch: u64,
    issued_refs: HashSet<String>,
}

struct DiscoveredTarget {
    target_id: String,
    source_id: String,
    endpoint: ChromeEndpoint,
    websocket_path: String,
    title: Option<String>,
}

struct DiscoveryRound {
    targets: Vec<DiscoveredTarget>,
    diagnostics: HashMap<String, String>,
    reachable: HashSet<ChromeEndpoint>,
}

impl ChromeAdapter {
    pub fn from_env() -> Result<Self, EndpointError> {
        let configured = std::env::var("MANUVRA_CHROME_ENDPOINTS").ok();
        Endpoint::configured(configured.as_deref()).map(Self::new)
    }

    pub fn new(endpoints: Vec<Endpoint>) -> Self {
        Self {
            endpoints,
            state: Mutex::new(State {
                next_generation: 1,
                ..State::default()
            }),
        }
    }

    fn refresh(&self) -> Vec<TargetDescriptor> {
        self.refresh_until(None).unwrap_or_default()
    }

    fn refresh_until(
        &self,
        deadline: Option<Instant>,
    ) -> Result<Vec<TargetDescriptor>, AdapterError> {
        let discovered = self.discover_until(deadline)?;
        let mut state = self.state.lock().expect("Chrome adapter state");
        apply_discovered_targets(&mut state, discovered);
        Ok(present_descriptors(&state))
    }

    fn discover_until(&self, deadline: Option<Instant>) -> Result<DiscoveryRound, AdapterError> {
        let mut discovered = DiscoveryRound {
            targets: Vec::new(),
            diagnostics: HashMap::new(),
            reachable: HashSet::new(),
        };
        for endpoint in &self.endpoints {
            let timeout = discovery_budget(deadline)?;
            match discover(endpoint, timeout) {
                Ok(targets) => {
                    discovered
                        .diagnostics
                        .insert(endpoint.label(), "reachable".to_owned());
                    discovered.reachable.insert(endpoint.clone());
                    discovered.targets.extend(targets);
                }
                Err(error) => {
                    discovered
                        .diagnostics
                        .insert(endpoint.label(), endpoint_status(&error));
                }
            }
        }
        Ok(discovered)
    }

    fn connection(&self, target_id: &str, raw: bool) -> Result<Arc<CdpClient>, AdapterError> {
        let mut state = self.state.lock().expect("Chrome adapter state");
        if retire_disconnected(&mut state, target_id, raw) {
            return Err(adapter_error(
                "target_stale",
                "Chrome connection incarnation changed",
            ));
        }
        open_or_reuse_client(&mut state, target_id, raw)
    }

    fn validate_generation(&self, context: &AdapterContext) -> Result<(), AdapterError> {
        let state = self.state.lock().expect("Chrome adapter state");
        match state.targets.get(&context.target_id) {
            Some(target)
                if target.present && target.descriptor.generation == context.target_generation =>
            {
                Ok(())
            }
            Some(_) => Err(adapter_error(
                "target_stale",
                "Chrome target generation changed",
            )),
            None => Err(adapter_error(
                "target_not_found",
                "Chrome target is unavailable",
            )),
        }
    }

    fn evidence(&self, context: &AdapterContext, operation: &AdapterOperation) -> AdapterReply {
        match operation
            .input
            .get("kind")
            .and_then(Value::as_str)
            .unwrap_or_default()
        {
            "diagnostics" => self.diagnostics_reply(context),
            "timings" => self.timings_reply(context),
            kind => self.journal_evidence(context, kind),
        }
    }

    fn journal_evidence(&self, context: &AdapterContext, kind: &str) -> AdapterReply {
        let client = match self.connection(&context.target_id, false) {
            Ok(client) => client,
            Err(error) => return rejected(error),
        };
        let cursor = self.session_cursor(context);
        let snapshot = client.snapshot_since(cursor);
        if snapshot.overflowed {
            return rejected(adapter_error(
                "observation_failed",
                "Chrome evidence journal overflowed; partial evidence was not published",
            ));
        }
        let events = snapshot
            .events
            .iter()
            .filter(|event| (kind == "logs") == is_log_event(event))
            .map(public_event)
            .collect::<Vec<_>>();
        artifact_reply(
            kind,
            json!({
                "kind": kind,
                "target_id": context.target_id,
                "complete": true,
                "dropped": 0,
                "start_cursor": cursor,
                "end_cursor": snapshot.last_cursor,
                "events": events,
            }),
        )
    }

    fn session_cursor(&self, context: &AdapterContext) -> u64 {
        self.state
            .lock()
            .expect("Chrome adapter state")
            .sessions
            .get(&context.session_id)
            .filter(|session| session.target_id == context.target_id)
            .map(|session| session.cursor)
            .unwrap_or(0)
    }

    fn diagnostics_reply(&self, context: &AdapterContext) -> AdapterReply {
        let state = self.state.lock().expect("Chrome adapter state");
        let target = state.targets.get(&context.target_id);
        artifact_reply(
            "diagnostics",
            json!({
                "kind": "diagnostics",
                "complete": true,
                "target_id": context.target_id,
                "target_generation": context.target_generation,
                "source_target_id": target.map(|target| target.source_id.clone()),
                "endpoints": state.diagnostics,
            }),
        )
    }

    fn timings_reply(&self, context: &AdapterContext) -> AdapterReply {
        let state = self.state.lock().expect("Chrome adapter state");
        artifact_reply(
            "timings",
            json!({
                "kind": "timings",
                "complete": true,
                "target_id": context.target_id,
                "entries": state.timings.get(&context.session_id).cloned().unwrap_or_default(),
            }),
        )
    }

    fn record_timing(
        &self,
        context: &AdapterContext,
        operation: &AdapterOperation,
        reply: &AdapterReply,
    ) {
        let mut state = self.state.lock().expect("Chrome adapter state");
        let entries = state.timings.entry(context.session_id.clone()).or_default();
        if entries.len() < 10_000 {
            entries.push(json!({
                "action_sequence": context.action_sequence,
                "command": operation.command,
                "preflight_ms": reply.timing.preflight_ms,
                "dispatch_ms": reply.timing.dispatch_ms,
                "stabilize_ms": reply.timing.stabilize_ms,
                "capture_ms": reply.timing.capture_ms,
            }));
        }
    }

    fn register_observation_refs(&self, context: &AdapterContext, reply: &AdapterReply) {
        if reply.delivery != manuvra_runtime::AdapterDelivery::Confirmed {
            return;
        }
        let mut issued = HashSet::new();
        collect_refs(&reply.response, &mut issued);
        let mut state = self.state.lock().expect("Chrome adapter state");
        let Some(session) = state.sessions.get_mut(&context.session_id) else {
            return;
        };
        if session.target_id != context.target_id {
            return;
        }
        if session.reference_epoch != context.reference_epoch {
            session.reference_epoch = context.reference_epoch;
            session.issued_refs.clear();
        }
        session.issued_refs.extend(issued);
    }

    fn validate_issued_ref(
        &self,
        context: &AdapterContext,
        operation: &AdapterOperation,
    ) -> Result<(), AdapterError> {
        let Some(locator) = operation.input.get("locator") else {
            return Ok(());
        };
        if locator.get("kind").and_then(Value::as_str) != Some("ref") {
            return Ok(());
        }
        let reference = locator
            .get("ref")
            .and_then(Value::as_str)
            .ok_or_else(|| adapter_error("invalid_request", "missing element reference"))?;
        let state = self.state.lock().expect("Chrome adapter state");
        let issued = state
            .sessions
            .get(&context.session_id)
            .is_some_and(|session| {
                session.target_id == context.target_id
                    && session.reference_epoch == context.reference_epoch
                    && session.issued_refs.contains(reference)
            });
        issued
            .then_some(())
            .ok_or_else(|| adapter_error("element_stale", "element ref was not issued here"))
    }

    fn dispatch_invoke(
        &self,
        context: &AdapterContext,
        operation: &AdapterOperation,
        cancellation: Arc<AtomicBool>,
    ) -> AdapterReply {
        match operation.command.as_str() {
            "observe.evidence" => self.evidence(context, operation),
            command if command.starts_with("observe.") => {
                self.invoke_observe(context, operation, cancellation)
            }
            "raw.cdp" if is_raw_query(operation) => {
                self.invoke_raw_query(context, operation, cancellation)
            }
            _ => self.invoke_mutation(context, operation, cancellation),
        }
    }

    fn invoke_observe(
        &self,
        context: &AdapterContext,
        operation: &AdapterOperation,
        cancellation: Arc<AtomicBool>,
    ) -> AdapterReply {
        let observation = match self.connection(&context.target_id, false) {
            Ok(client) => client,
            Err(error) => return rejected(error),
        };
        let reply = page::observe(&observation, context, operation, cancellation);
        if matches!(operation.command.as_str(), "observe.query" | "observe.tree") {
            self.register_observation_refs(context, &reply);
        }
        reply
    }

    fn invoke_raw_query(
        &self,
        context: &AdapterContext,
        operation: &AdapterOperation,
        cancellation: Arc<AtomicBool>,
    ) -> AdapterReply {
        if let Err(error) = self.connection(&context.target_id, false) {
            return rejected(error);
        }
        let raw = match self.connection(&context.target_id, true) {
            Ok(client) => client,
            Err(error) => return rejected(error),
        };
        page::raw_query(&raw, context, operation, cancellation)
    }

    fn invoke_mutation(
        &self,
        context: &AdapterContext,
        operation: &AdapterOperation,
        cancellation: Arc<AtomicBool>,
    ) -> AdapterReply {
        let observation = match self.connection(&context.target_id, false) {
            Ok(client) => client,
            Err(error) => return rejected(error),
        };
        let raw = match raw_client(self, context, operation) {
            Ok(client) => client,
            Err(error) => return rejected(error),
        };
        let reply = page::mutate(
            &observation,
            raw.as_deref(),
            context,
            operation,
            cancellation,
        );
        self.record_timing(context, operation, &reply);
        reply
    }
}

fn collect_refs(value: &Value, output: &mut HashSet<String>) {
    match value {
        Value::Object(entries) => {
            if let Some(reference) = entries.get("ref").and_then(Value::as_str) {
                output.insert(reference.to_owned());
            }
            for value in entries.values() {
                collect_refs(value, output);
            }
        }
        Value::Array(entries) => {
            for value in entries {
                collect_refs(value, output);
            }
        }
        _ => {}
    }
}

impl TargetAdapter for ChromeAdapter {
    fn targets(&self) -> Vec<TargetDescriptor> {
        self.refresh()
    }

    fn targets_until(&self, deadline: Instant) -> Result<Vec<TargetDescriptor>, AdapterError> {
        self.refresh_until(Some(deadline))
    }

    fn diagnostics(&self) -> Value {
        let _ = self.refresh();
        let state = self.state.lock().expect("Chrome adapter state");
        json!({
            "kind": "chrome",
            "endpoints": state.diagnostics,
            "targets": state.targets.values().filter(|target| target.present).count(),
        })
    }

    fn session_opened(
        &self,
        session: &AdapterSession,
        deadline: Instant,
    ) -> Result<(), AdapterError> {
        if Instant::now() >= deadline {
            return Err(adapter_error(
                "timed_out",
                "deadline elapsed before Chrome session initialization",
            ));
        }
        let cursor = self
            .state
            .lock()
            .expect("Chrome adapter state")
            .targets
            .get(&session.target_id)
            .and_then(|target| target.observation.as_ref())
            .map(|client| client.cursor())
            .unwrap_or(0);
        let mut state = self.state.lock().expect("Chrome adapter state");
        state.sessions.insert(
            session.session_id.clone(),
            SessionCursor {
                target_id: session.target_id.clone(),
                cursor,
                reference_epoch: 0,
                issued_refs: HashSet::new(),
            },
        );
        state.timings.insert(session.session_id.clone(), Vec::new());
        if Instant::now() >= deadline {
            state.sessions.remove(&session.session_id);
            state.timings.remove(&session.session_id);
            return Err(adapter_error(
                "timed_out",
                "deadline elapsed during Chrome session initialization",
            ));
        }
        Ok(())
    }

    fn session_closed(&self, session: &AdapterSession) {
        let mut state = self.state.lock().expect("Chrome adapter state");
        state.sessions.remove(&session.session_id);
        state.timings.remove(&session.session_id);
        let target_still_in_use = state
            .sessions
            .values()
            .any(|active| active.target_id == session.target_id);
        if !target_still_in_use && let Some(target) = state.targets.get_mut(&session.target_id) {
            target.observation = None;
            target.raw = None;
        }
    }

    fn prepare(
        &self,
        context: &AdapterContext,
        operation: &AdapterOperation,
        cancellation: Arc<AtomicBool>,
    ) -> Result<AdapterOperation, AdapterError> {
        self.validate_generation(context)?;
        if operation.command.starts_with("action.") && operation.input.get("locator").is_some() {
            self.validate_issued_ref(context, operation)?;
            let client = self.connection(&context.target_id, false)?;
            return page::prepare(&client, context, operation, cancellation);
        }
        Ok(operation.clone())
    }

    fn invoke(
        &self,
        context: &AdapterContext,
        operation: &AdapterOperation,
        cancellation: Arc<AtomicBool>,
    ) -> AdapterReply {
        if let Err(error) = self.validate_generation(context) {
            return rejected(error);
        }
        self.dispatch_invoke(context, operation, cancellation)
    }
}

fn is_raw_query(operation: &AdapterOperation) -> bool {
    operation.input.get("intent").and_then(Value::as_str) == Some("query")
}

fn raw_client(
    adapter: &ChromeAdapter,
    context: &AdapterContext,
    operation: &AdapterOperation,
) -> Result<Option<Arc<CdpClient>>, AdapterError> {
    if operation.command != "raw.cdp" {
        return Ok(None);
    }
    adapter.connection(&context.target_id, true).map(Some)
}

fn discovery_budget(deadline: Option<Instant>) -> Result<Duration, AdapterError> {
    let timeout = deadline
        .map(|deadline| deadline.saturating_duration_since(Instant::now()))
        .unwrap_or(DISCOVERY_TIMEOUT)
        .min(DISCOVERY_TIMEOUT);
    if timeout.is_zero() {
        return Err(adapter_error(
            "timed_out",
            "target discovery deadline elapsed",
        ));
    }
    Ok(timeout)
}

fn endpoint_status(error: &str) -> String {
    if endpoint::connection_refused_text(error) {
        "refused".to_owned()
    } else {
        bounded(error)
    }
}

fn retire_disconnected(state: &mut State, target_id: &str, raw: bool) -> bool {
    let disconnected = state
        .targets
        .get(target_id)
        .filter(|target| target.present)
        .and_then(|target| {
            if raw {
                target.raw.as_ref()
            } else {
                target.observation.as_ref()
            }
        })
        .is_some_and(|client| client.is_disconnected());
    if disconnected {
        let generation = state.next_generation;
        state.next_generation = state.next_generation.saturating_add(1);
        if let Some(target) = state.targets.get_mut(target_id) {
            target.descriptor.generation = generation;
            target.observation = None;
            target.raw = None;
        }
    }
    disconnected
}

fn open_or_reuse_client(
    state: &mut State,
    target_id: &str,
    raw: bool,
) -> Result<Arc<CdpClient>, AdapterError> {
    let target = state
        .targets
        .get_mut(target_id)
        .filter(|target| target.present)
        .ok_or_else(|| adapter_error("target_not_found", "Chrome target is unavailable"))?;
    let slot = if raw {
        &mut target.raw
    } else {
        &mut target.observation
    };
    if let Some(client) = slot {
        return Ok(client.clone());
    }
    let url = target
        .endpoint
        .websocket_url(&target.websocket_path)
        .map_err(|error| adapter_error("capability_unavailable", &error.to_string()))?;
    let client = CdpClient::connect(url, !raw)
        .map_err(|error| adapter_error("capability_unavailable", &error))?;
    *slot = Some(client.clone());
    Ok(client)
}

fn apply_discovered_targets(state: &mut State, discovered: DiscoveryRound) {
    let seen = discovered
        .targets
        .iter()
        .map(|target| target.target_id.clone())
        .collect::<HashSet<_>>();
    for target in discovered.targets {
        refresh_target(state, target);
    }
    for target in state.targets.values_mut() {
        if discovered.reachable.contains(&target.endpoint) {
            target.present = seen.contains(&target.descriptor.target_id);
        }
    }
    state.diagnostics = discovered.diagnostics;
}

fn present_descriptors(state: &State) -> Vec<TargetDescriptor> {
    state
        .targets
        .values()
        .filter(|target| target.present)
        .map(|target| target.descriptor.clone())
        .collect()
}

fn discover(endpoint: &Endpoint, timeout: Duration) -> Result<Vec<DiscoveredTarget>, String> {
    let value = endpoint
        .get_json("/json/list", timeout)
        .map_err(|error| error.to_string())?;
    let items = value
        .as_array()
        .ok_or_else(|| "Chrome target list was not an array".to_owned())?;
    items
        .iter()
        .filter(|item| item.get("type").and_then(Value::as_str) == Some("page"))
        .filter_map(|item| discovered_target(endpoint, item).transpose())
        .collect()
}

fn discovered_target(
    endpoint: &Endpoint,
    item: &Value,
) -> Result<Option<DiscoveredTarget>, String> {
    let Some(source_id) = item.get("id").and_then(Value::as_str) else {
        return Ok(None);
    };
    let Some(websocket) = item.get("webSocketDebuggerUrl").and_then(Value::as_str) else {
        return Ok(None);
    };
    let websocket_path = websocket_path(websocket)?;
    let target_id = opaque_target_id(endpoint, source_id);
    let title = item
        .get("title")
        .and_then(Value::as_str)
        .filter(|title| !title.is_empty())
        .map(str::to_owned);
    Ok(Some(DiscoveredTarget {
        target_id,
        source_id: source_id.to_owned(),
        endpoint: endpoint.clone(),
        websocket_path,
        title,
    }))
}

fn websocket_path(url: &str) -> Result<String, String> {
    let remainder = url
        .strip_prefix("ws://")
        .ok_or_else(|| "Chrome advertised a non-local WebSocket scheme".to_owned())?;
    let slash = remainder
        .find('/')
        .ok_or_else(|| "Chrome advertised a WebSocket without a path".to_owned())?;
    let path = &remainder[slash..];
    if !path.starts_with("/devtools/") {
        return Err("Chrome advertised an unexpected WebSocket path".to_owned());
    }
    Ok(path.to_owned())
}

fn opaque_target_id(endpoint: &Endpoint, source_id: &str) -> String {
    let digest = Sha256::digest(format!("{}\0{source_id}", endpoint.label()));
    let suffix = digest[..12]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    format!("chrome_{suffix}")
}

fn refresh_target(state: &mut State, discovered: DiscoveredTarget) {
    let replace = state
        .targets
        .get(&discovered.target_id)
        .is_none_or(|current| {
            !current.present
                || current.websocket_path != discovered.websocket_path
                || current.source_id != discovered.source_id
        });
    if replace {
        let generation = state.next_generation;
        state.next_generation = state.next_generation.saturating_add(1);
        state.targets.insert(
            discovered.target_id.clone(),
            TargetRecord {
                descriptor: TargetDescriptor {
                    target_id: discovered.target_id,
                    generation,
                    kind: "chrome".to_owned(),
                    owner: CHROME_OWNER.to_owned(),
                    title: discovered.title,
                    capabilities: chrome_capabilities(),
                },
                endpoint: discovered.endpoint,
                websocket_path: discovered.websocket_path,
                source_id: discovered.source_id,
                present: true,
                observation: None,
                raw: None,
            },
        );
    } else if let Some(target) = state.targets.get_mut(&discovered.target_id) {
        target.present = true;
        target.descriptor.owner = CHROME_OWNER.to_owned();
        target.descriptor.title = discovered.title;
    }
}

fn chrome_capabilities() -> Vec<String> {
    [
        "common.click",
        "common.type",
        "common.press",
        "common.scroll",
        "common.navigate",
        "observation.query",
        "observation.screenshot",
        "observation.tree",
        "observation.evidence",
        "raw.cdp",
    ]
    .into_iter()
    .map(str::to_owned)
    .collect()
}

fn public_event(event: &JournalEvent) -> Value {
    json!({
        "cursor": event.cursor,
        "received_ms": event.received_ms,
        "action_sequence": event.action_sequence,
        "method": event.message.get("method"),
        "params": event.message.get("params"),
    })
}

fn artifact_reply(kind: &str, value: Value) -> AdapterReply {
    let bytes = serde_json::to_vec(&value).expect("Chrome evidence JSON");
    let mut reply = AdapterReply::confirmed(json!({"kind": kind, "complete": true}), None);
    reply.artifact = Some(AdapterArtifact {
        kind: kind.to_owned(),
        extension: "json".to_owned(),
        media_type: "application/json".to_owned(),
        bytes,
    });
    reply
}

fn rejected(error: AdapterError) -> AdapterReply {
    let mut reply = AdapterReply::confirmed(Value::Null, None);
    reply.delivery = manuvra_runtime::AdapterDelivery::Rejected;
    reply.error = Some(error);
    reply
}

fn adapter_error(code: &str, message: &str) -> AdapterError {
    AdapterError {
        code: code.to_owned(),
        message: Some(message.chars().take(256).collect()),
        details: None,
    }
}

fn bounded(value: &str) -> String {
    value.chars().take(160).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use manuvra_runtime::{ExecutionMode, TargetAdapter};
    use std::thread;
    use std::time::Instant;

    #[test]
    fn advertised_websocket_authority_is_discarded() {
        assert_eq!(
            websocket_path("ws://evil.example:4444/devtools/page/abc").unwrap(),
            "/devtools/page/abc"
        );
        assert!(websocket_path("wss://evil.example/devtools/page/abc").is_err());
        assert!(websocket_path("ws://localhost/not-devtools").is_err());
    }

    #[test]
    fn opaque_target_identity_is_stable_per_endpoint_and_source() {
        let endpoint = Endpoint::parse("127.0.0.1:9222").unwrap();
        let first = opaque_target_id(&endpoint, "page-1");
        assert_eq!(first, opaque_target_id(&endpoint, "page-1"));
        assert_ne!(first, opaque_target_id(&endpoint, "page-2"));
    }

    #[test]
    fn discovered_page_uses_list_title_and_chrome_owner() {
        let endpoint = Endpoint::parse("127.0.0.1:9222").unwrap();
        let titled = discovered_target(
            &endpoint,
            &json!({
                "id": "page-1",
                "title": "Inbox",
                "type": "page",
                "webSocketDebuggerUrl": "ws://127.0.0.1:9222/devtools/page/abc"
            }),
        )
        .unwrap()
        .unwrap();
        assert_eq!(titled.title.as_deref(), Some("Inbox"));

        let empty = discovered_target(
            &endpoint,
            &json!({
                "id": "page-2",
                "title": "",
                "type": "page",
                "webSocketDebuggerUrl": "ws://127.0.0.1:9222/devtools/page/def"
            }),
        )
        .unwrap()
        .unwrap();
        assert_eq!(empty.title, None);

        let missing = discovered_target(
            &endpoint,
            &json!({
                "id": "page-3",
                "type": "page",
                "webSocketDebuggerUrl": "ws://127.0.0.1:9222/devtools/page/ghi"
            }),
        )
        .unwrap()
        .unwrap();
        assert_eq!(missing.title, None);

        let mut state = State {
            next_generation: 1,
            ..State::default()
        };
        refresh_target(&mut state, titled);
        let descriptor = &state.targets.values().next().unwrap().descriptor;
        assert_eq!(descriptor.owner, CHROME_OWNER);
        assert_eq!(descriptor.title.as_deref(), Some("Inbox"));
        assert_eq!(descriptor.generation, 1);
    }

    #[test]
    fn chrome_title_refresh_does_not_advance_generation() {
        let endpoint = Endpoint::parse("127.0.0.1:9222").unwrap();
        let mut state = State {
            next_generation: 1,
            ..State::default()
        };
        let discovered = |title: &str| DiscoveredTarget {
            target_id: "chrome_test".to_owned(),
            source_id: "source".to_owned(),
            endpoint: endpoint.clone(),
            websocket_path: "/devtools/page/one".to_owned(),
            title: Some(title.to_owned()),
        };
        refresh_target(&mut state, discovered("before"));
        refresh_target(&mut state, discovered("after"));
        let descriptor = &state.targets["chrome_test"].descriptor;
        assert_eq!(descriptor.generation, 1);
        assert_eq!(descriptor.owner, CHROME_OWNER);
        assert_eq!(descriptor.title.as_deref(), Some("after"));
    }

    #[test]
    fn websocket_incarnation_change_advances_target_generation() {
        let endpoint = Endpoint::parse("127.0.0.1:9222").unwrap();
        let mut state = State {
            next_generation: 1,
            ..State::default()
        };
        let discovered = |path: &str| DiscoveredTarget {
            target_id: "chrome_test".to_owned(),
            source_id: "source".to_owned(),
            endpoint: endpoint.clone(),
            websocket_path: path.to_owned(),
            title: None,
        };
        refresh_target(&mut state, discovered("/devtools/page/one"));
        assert_eq!(state.targets["chrome_test"].descriptor.generation, 1);
        refresh_target(&mut state, discovered("/devtools/page/two"));
        assert_eq!(state.targets["chrome_test"].descriptor.generation, 2);
        assert!(state.targets["chrome_test"].observation.is_none());
    }

    #[test]
    fn target_reappearance_advances_generation() {
        let endpoint = Endpoint::parse("127.0.0.1:9222").unwrap();
        let mut state = State {
            next_generation: 1,
            ..State::default()
        };
        let discovered = || DiscoveredTarget {
            target_id: "chrome_test".to_owned(),
            source_id: "source".to_owned(),
            endpoint: endpoint.clone(),
            websocket_path: "/devtools/page/one".to_owned(),
            title: None,
        };
        refresh_target(&mut state, discovered());
        state.targets.get_mut("chrome_test").unwrap().present = false;
        refresh_target(&mut state, discovered());

        assert_eq!(state.targets["chrome_test"].descriptor.generation, 2);
    }

    #[test]
    fn only_exact_observed_refs_are_accepted_for_current_epoch() {
        let adapter = ChromeAdapter::new(vec![]);
        adapter.state.lock().unwrap().sessions.insert(
            "session".to_owned(),
            SessionCursor {
                target_id: "target".to_owned(),
                cursor: 0,
                reference_epoch: 0,
                issued_refs: HashSet::new(),
            },
        );
        let context = AdapterContext {
            session_id: "session".to_owned(),
            target_id: "target".to_owned(),
            target_generation: 1,
            action_sequence: 0,
            reference_namespace: "namespace".to_owned(),
            reference_epoch: 4,
            frame_token: None,
            mode: ExecutionMode::Background,
            deadline: Instant::now() + Duration::from_secs(1),
        };
        let issued = "e_namespace_4_deadbeef_42";
        adapter.register_observation_refs(
            &context,
            &AdapterReply::confirmed(json!({"matches": [{"ref": issued}]}), None),
        );
        let operation = |reference: &str| {
            AdapterOperation::new(
                "action.click".to_owned(),
                json!({"locator": {"kind": "ref", "ref": reference}}),
            )
        };
        assert!(
            adapter
                .validate_issued_ref(&context, &operation(issued))
                .is_ok()
        );
        assert_eq!(
            adapter
                .validate_issued_ref(&context, &operation("e_namespace_4_deadbeef_43"))
                .unwrap_err()
                .code,
            "element_stale"
        );
        let mut later = context.clone();
        later.reference_epoch = 5;
        assert_eq!(
            adapter
                .validate_issued_ref(&later, &operation(issued))
                .unwrap_err()
                .code,
            "element_stale"
        );
    }

    #[test]
    fn chrome_diagnostics_classify_a_refused_loopback_endpoint() {
        let adapter = ChromeAdapter::new(vec![Endpoint::parse("127.0.0.1:1").unwrap()]);
        let diagnostics = adapter.diagnostics();
        assert_eq!(diagnostics["kind"], "chrome");
        assert_eq!(diagnostics["endpoints"]["127.0.0.1:1"], "refused");
    }

    #[test]
    fn doctor_warns_when_the_chrome_endpoint_is_refused() {
        use manuvra_protocol::Invocation;
        use manuvra_runtime::{InteractionModule, Runtime, RuntimeConfig};

        let adapter = ChromeAdapter::new(vec![Endpoint::parse("127.0.0.1:1").unwrap()]);
        let temp = tempfile::tempdir().unwrap();
        let runtime = Runtime::new(
            RuntimeConfig {
                temporary_root: temp.path().join("tmp"),
                config_root: temp.path().join("config"),
            },
            vec![std::sync::Arc::new(adapter)],
        )
        .unwrap();
        let reply = runtime.invoke(Invocation::new(
            "system.doctor",
            json!({}),
            "chrome-refused".to_owned(),
            2_000,
        ));
        let warnings = reply.value["warnings"].as_array().expect("doctor warnings");
        assert!(
            warnings
                .iter()
                .any(|warning| warning == "chrome_endpoint_refused"),
            "{}",
            reply.value
        );
    }

    fn install_page_fixture(chrome: &crate::transport::test_support::ScriptedChrome) {
        chrome.reply(
            "Page.getFrameTree",
            json!({"frameTree": {"frame": {"id": "main", "loaderId": "loader-1"}}}),
        );
        chrome.reply(
            "Accessibility.getFullAXTree",
            json!({"nodes": [{
                "nodeId": "1",
                "backendDOMNodeId": 42,
                "role": {"value": "button"},
                "name": {"value": "Save changes"},
                "ignored": false
            }]}),
        );
        chrome.reply(
            "DOM.getBoxModel",
            json!({"model": {"content": [10, 10, 90, 10, 90, 50, 10, 50]}}),
        );
        chrome.reply(
            "DOM.describeNode",
            json!({"node": {"attributes": ["id", "save"]}}),
        );
        chrome.reply("DOM.resolveNode", json!({"object": {"objectId": "obj-1"}}));
        chrome.reply(
            "Runtime.callFunctionOn",
            json!({"result": {"value": "Save changes"}}),
        );
        chrome.reply(
            "Page.getLayoutMetrics",
            json!({"visualViewport": {"clientWidth": 800, "clientHeight": 600}}),
        );
        chrome.reply("Page.captureScreenshot", json!({"data": fixture_png()}));
        chrome.reply("Page.navigate", json!({"loaderId": "loader-2"}));
        chrome.events_after(
            "Page.navigate",
            vec![json!({
                "method": "Page.lifecycleEvent",
                "params": {"name": "DOMContentLoaded", "loaderId": "loader-2", "frameId": "main"}
            })],
        );
        chrome.events_after(
            "Input.dispatchMouseEvent",
            vec![
                json!({"method": "Page.frameStartedLoading", "params": {"frameId": "main"}}),
                json!({
                    "method": "Page.frameNavigated",
                    "params": {"frame": {"id": "main", "loaderId": "loader-2"}}
                }),
                json!({
                    "method": "Page.lifecycleEvent",
                    "params": {"name": "load", "loaderId": "loader-2", "frameId": "main"}
                }),
            ],
        );
    }

    fn fixture_png() -> String {
        let mut png = vec![0; 24];
        png[..8].copy_from_slice(b"\x89PNG\r\n\x1a\n");
        png[16..20].copy_from_slice(&800_u32.to_be_bytes());
        png[20..24].copy_from_slice(&600_u32.to_be_bytes());
        base64::Engine::encode(&base64::engine::general_purpose::STANDARD, png)
    }

    fn live_adapter() -> (
        crate::transport::test_support::ScriptedChrome,
        ChromeAdapter,
        TargetDescriptor,
        AdapterSession,
    ) {
        let chrome = crate::transport::test_support::ScriptedChrome::start();
        install_page_fixture(&chrome);
        let adapter = ChromeAdapter::new(vec![chrome.endpoint()]);
        let targets = adapter.targets();
        assert_eq!(targets.len(), 1);
        assert_eq!(targets[0].owner, CHROME_OWNER);
        let session = AdapterSession {
            session_id: "session".to_owned(),
            target_id: targets[0].target_id.clone(),
            target_generation: targets[0].generation,
        };
        adapter
            .session_opened(&session, Instant::now() + Duration::from_secs(1))
            .unwrap();
        (
            chrome,
            adapter,
            targets.into_iter().next().unwrap(),
            session,
        )
    }

    fn live_context(target: &TargetDescriptor) -> AdapterContext {
        AdapterContext {
            session_id: "session".to_owned(),
            target_id: target.target_id.clone(),
            target_generation: target.generation,
            action_sequence: 1,
            reference_namespace: "n_test".to_owned(),
            reference_epoch: 1,
            frame_token: None,
            mode: ExecutionMode::Background,
            deadline: Instant::now() + Duration::from_secs(2),
        }
    }

    #[test]
    fn adapter_attach_prepare_and_invoke_keep_locator_frame_token_and_dispatch_results() {
        let (chrome, adapter, target, session) = live_adapter();
        let context = live_context(&target);
        let cancellation = Arc::new(AtomicBool::new(false));

        let mut stale = context.clone();
        stale.target_generation = target.generation + 1;
        assert_eq!(
            adapter
                .invoke(
                    &stale,
                    &AdapterOperation::new("observe.screenshot".to_owned(), json!({})),
                    cancellation.clone()
                )
                .error
                .unwrap()
                .code,
            "target_stale"
        );
        let mut missing = context.clone();
        missing.target_id = "chrome_missing".to_owned();
        assert_eq!(
            adapter
                .invoke(
                    &missing,
                    &AdapterOperation::new("observe.screenshot".to_owned(), json!({})),
                    cancellation.clone()
                )
                .error
                .unwrap()
                .code,
            "target_not_found"
        );

        chrome.events_after_once(
            "Page.captureScreenshot",
            vec![json!({"method": "DOM.childNodeInserted", "params": {}})],
        );
        let screenshot = adapter.invoke(
            &context,
            &AdapterOperation::new("observe.screenshot".to_owned(), json!({})),
            cancellation.clone(),
        );
        assert_eq!(
            screenshot.delivery,
            manuvra_runtime::AdapterDelivery::Confirmed
        );
        assert_eq!(screenshot.screenshot_width, Some(800));
        let frame_token = screenshot.frame_signature.clone().unwrap();

        let prepared = adapter
            .prepare(
                &context,
                &AdapterOperation::new(
                    "action.click".to_owned(),
                    json!({"locator": {"kind": "semantic", "role": "button", "name": "Save changes"}}),
                ),
                cancellation.clone(),
            )
            .unwrap();
        assert_eq!(prepared.prepared.unwrap()["target"]["backend_id"], 42);

        let point_stale = adapter.prepare(
            &context,
            &AdapterOperation::new(
                "action.click".to_owned(),
                json!({"locator": {"kind": "point", "x": 10, "y": 10, "frame_token": "wrong"}}),
            ),
            cancellation.clone(),
        );
        assert_eq!(point_stale.unwrap_err().code, "frame_stale");

        let point_outside = adapter.prepare(
            &context,
            &AdapterOperation::new(
                "action.click".to_owned(),
                json!({"locator": {"kind": "point", "x": 900, "y": 10, "frame_token": frame_token}}),
            ),
            cancellation.clone(),
        );
        assert_eq!(point_outside.unwrap_err().code, "element_not_found");

        let point = adapter
            .prepare(
                &context,
                &AdapterOperation::new(
                    "action.click".to_owned(),
                    json!({"locator": {"kind": "point", "x": 10.0, "y": 10.0, "frame_token": screenshot.frame_signature.clone().unwrap()}}),
                ),
                cancellation.clone(),
            )
            .unwrap();
        let clicked = adapter.invoke(&context, &point, cancellation.clone());
        assert_eq!(
            clicked.delivery,
            manuvra_runtime::AdapterDelivery::Confirmed
        );
        assert_eq!(clicked.response["dispatched"], "click");
        assert!(clicked.already_settled);

        let typed = adapter.invoke(
            &context,
            &{
                let mut operation = AdapterOperation::new(
                    "action.type".to_owned(),
                    json!({"text": "hi", "replace": true}),
                );
                operation.prepared =
                    Some(json!({"target": {"backend_id": 42, "x": 50.0, "y": 30.0}}));
                operation
            },
            cancellation.clone(),
        );
        assert_eq!(typed.response["dispatched"], "type");

        let pressed = adapter.invoke(
            &context,
            &{
                let mut operation = AdapterOperation::new(
                    "action.press".to_owned(),
                    json!({"key": "Enter", "locator": {"kind": "ref"}}),
                );
                operation.prepared =
                    Some(json!({"target": {"backend_id": 42, "x": 50.0, "y": 30.0}}));
                operation
            },
            cancellation.clone(),
        );
        assert_eq!(pressed.response["dispatched"], "press");

        let scrolled = adapter.invoke(
            &context,
            &{
                let mut operation = AdapterOperation::new(
                    "action.scroll".to_owned(),
                    json!({"delta_y": 40, "locator": {"kind": "ref"}}),
                );
                operation.prepared =
                    Some(json!({"target": {"backend_id": 42, "x": 50.0, "y": 30.0}}));
                operation
            },
            cancellation.clone(),
        );
        assert_eq!(scrolled.response["dispatched"], "scroll");
        assert_eq!(
            adapter
                .invoke(
                    &context,
                    &AdapterOperation::new("action.press".to_owned(), json!({"key": "Tab"})),
                    cancellation.clone(),
                )
                .response["dispatched"],
            "press"
        );
        assert_eq!(
            adapter
                .invoke(
                    &context,
                    &AdapterOperation::new("action.scroll".to_owned(), json!({"delta_x": 1})),
                    cancellation.clone(),
                )
                .response["dispatched"],
            "scroll"
        );

        let navigated = adapter.invoke(
            &context,
            &AdapterOperation::new(
                "action.navigate".to_owned(),
                json!({"url": "https://example.test/"}),
            ),
            cancellation.clone(),
        );
        assert_eq!(navigated.response["loaderId"], "loader-2");

        let query = adapter.invoke(
            &context,
            &AdapterOperation::new(
                "observe.query".to_owned(),
                json!({"semantic": {"role": "button", "name": "Save changes", "identifier": "save", "text": "Save changes"}, "limit": 5}),
            ),
            cancellation.clone(),
        );
        assert_eq!(query.response["matches"][0]["role"], "button");
        let issued = query.response["matches"][0]["ref"]
            .as_str()
            .unwrap()
            .to_owned();

        let tree = adapter.invoke(
            &context,
            &AdapterOperation::new("observe.tree".to_owned(), json!({})),
            cancellation.clone(),
        );
        assert_eq!(tree.response["node_count"], 1);

        let reused = adapter
            .prepare(
                &context,
                &AdapterOperation::new(
                    "action.click".to_owned(),
                    json!({"locator": {"kind": "ref", "ref": issued}}),
                ),
                cancellation.clone(),
            )
            .unwrap();
        assert_eq!(reused.prepared.unwrap()["target"]["backend_id"], 42);
        assert_eq!(
            adapter
                .prepare(
                    &context,
                    &AdapterOperation::new(
                        "action.click".to_owned(),
                        json!({"locator": {"kind": "ref", "ref": "e_missing"}})
                    ),
                    cancellation.clone(),
                )
                .unwrap_err()
                .code,
            "element_stale"
        );
        assert_eq!(
            adapter
                .prepare(
                    &context,
                    &AdapterOperation::new(
                        "action.click".to_owned(),
                        json!({"locator": {"kind": "unknown"}})
                    ),
                    cancellation.clone(),
                )
                .unwrap_err()
                .code,
            "invalid_request"
        );

        let raw_query = adapter.invoke(
            &context,
            &AdapterOperation::new(
                "raw.cdp".to_owned(),
                json!({"intent": "query", "method": "Runtime.evaluate", "params": {"expression": "1"}}),
            ),
            cancellation.clone(),
        );
        assert_eq!(
            raw_query.delivery,
            manuvra_runtime::AdapterDelivery::Confirmed
        );

        let raw_mutate = adapter.invoke(
            &context,
            &AdapterOperation::new(
                "raw.cdp".to_owned(),
                json!({"method": "Runtime.evaluate", "params": {}}),
            ),
            cancellation.clone(),
        );
        assert_eq!(
            raw_mutate.delivery,
            manuvra_runtime::AdapterDelivery::Confirmed
        );

        let diagnostics = adapter.invoke(
            &context,
            &AdapterOperation::new(
                "observe.evidence".to_owned(),
                json!({"kind": "diagnostics"}),
            ),
            cancellation.clone(),
        );
        assert_eq!(diagnostics.response["kind"], "diagnostics");
        let timings = adapter.invoke(
            &context,
            &AdapterOperation::new("observe.evidence".to_owned(), json!({"kind": "timings"})),
            cancellation.clone(),
        );
        assert_eq!(timings.response["kind"], "timings");
        let logs = adapter.invoke(
            &context,
            &AdapterOperation::new("observe.evidence".to_owned(), json!({"kind": "logs"})),
            cancellation.clone(),
        );
        assert_eq!(logs.response["kind"], "logs");
        let unsupported = adapter.invoke(
            &context,
            &AdapterOperation::new("observe.unknown".to_owned(), json!({})),
            cancellation,
        );
        assert_eq!(unsupported.error.unwrap().code, "command_unsupported");

        adapter.session_closed(&session);
        assert_eq!(
            adapter
                .session_opened(&session, Instant::now())
                .unwrap_err()
                .code,
            "timed_out"
        );
        assert_eq!(
            adapter.targets_until(Instant::now()).unwrap_err().code,
            "timed_out"
        );
    }

    #[test]
    fn disconnected_connection_advances_generation_instead_of_silently_retargeting() {
        let (chrome, adapter, target, _session) = live_adapter();
        let context = live_context(&target);
        let cancellation = Arc::new(AtomicBool::new(false));
        let first = adapter.invoke(
            &context,
            &AdapterOperation::new("observe.screenshot".to_owned(), json!({})),
            cancellation.clone(),
        );
        assert_eq!(first.delivery, manuvra_runtime::AdapterDelivery::Confirmed);
        chrome.disconnect();
        let deadline = Instant::now() + Duration::from_secs(3);
        let second = loop {
            let reply = adapter.invoke(
                &context,
                &AdapterOperation::new("observe.screenshot".to_owned(), json!({})),
                cancellation.clone(),
            );
            if reply
                .error
                .as_ref()
                .is_some_and(|error| error.code == "target_stale")
                || Instant::now() >= deadline
            {
                break reply;
            }
            thread::sleep(Duration::from_millis(10));
        };
        assert_eq!(second.error.unwrap().code, "target_stale");
    }

    #[test]
    fn raw_query_does_not_bypass_a_disconnected_observation_incarnation() {
        let (chrome, adapter, target, _session) = live_adapter();
        let context = live_context(&target);
        let cancellation = Arc::new(AtomicBool::new(false));
        let first = adapter.invoke(
            &context,
            &AdapterOperation::new("observe.screenshot".to_owned(), json!({})),
            cancellation.clone(),
        );
        assert_eq!(first.delivery, manuvra_runtime::AdapterDelivery::Confirmed);
        chrome.disconnect();
        let deadline = Instant::now() + Duration::from_secs(3);
        let reply = loop {
            let reply = adapter.invoke(
                &context,
                &AdapterOperation::new(
                    "raw.cdp".to_owned(),
                    json!({
                        "intent": "query",
                        "method": "Runtime.evaluate",
                        "params": {"expression": "1"}
                    }),
                ),
                cancellation.clone(),
            );
            if reply
                .error
                .as_ref()
                .is_some_and(|error| error.code == "target_stale")
                || Instant::now() >= deadline
            {
                break reply;
            }
            thread::sleep(Duration::from_millis(10));
        };
        assert_eq!(reply.error.unwrap().code, "target_stale");
    }

    #[test]
    fn empty_or_malformed_target_lists_are_recorded_without_inventing_pages() {
        let chrome = crate::transport::test_support::ScriptedChrome::start();
        chrome.http_body(b"{}".to_vec());
        let adapter = ChromeAdapter::new(vec![chrome.endpoint()]);
        assert!(adapter.targets().is_empty());
        let diagnostics = adapter.diagnostics();
        assert_ne!(
            diagnostics["endpoints"][chrome.endpoint().label()],
            "reachable"
        );
    }
}
