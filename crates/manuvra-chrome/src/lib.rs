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
        let mut discovered = Vec::new();
        let mut diagnostics = HashMap::new();
        let mut reachable = HashSet::new();
        for endpoint in &self.endpoints {
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
            match discover(endpoint, timeout) {
                Ok(targets) => {
                    diagnostics.insert(endpoint.label(), "reachable".to_owned());
                    reachable.insert(endpoint.clone());
                    discovered.extend(targets);
                }
                Err(error) => {
                    let status = if endpoint::connection_refused_text(&error) {
                        "refused".to_owned()
                    } else {
                        bounded(&error)
                    };
                    diagnostics.insert(endpoint.label(), status);
                }
            }
        }
        let mut state = self.state.lock().expect("Chrome adapter state");
        let seen = discovered
            .iter()
            .map(|target| target.target_id.clone())
            .collect::<HashSet<_>>();
        for target in discovered {
            refresh_target(&mut state, target);
        }
        for target in state.targets.values_mut() {
            if reachable.contains(&target.endpoint) {
                target.present = seen.contains(&target.descriptor.target_id);
            }
        }
        state.diagnostics = diagnostics;
        Ok(state
            .targets
            .values()
            .filter(|target| target.present)
            .map(|target| target.descriptor.clone())
            .collect())
    }

    fn connection(&self, target_id: &str, raw: bool) -> Result<Arc<CdpClient>, AdapterError> {
        let mut state = self.state.lock().expect("Chrome adapter state");
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
            return Err(adapter_error(
                "target_stale",
                "Chrome connection incarnation changed",
            ));
        }
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
        let kind = operation
            .input
            .get("kind")
            .and_then(Value::as_str)
            .unwrap_or_default();
        if kind == "diagnostics" {
            return self.diagnostics_reply(context);
        }
        if kind == "timings" {
            return self.timings_reply(context);
        }
        let client = match self.connection(&context.target_id, false) {
            Ok(client) => client,
            Err(error) => return rejected(error),
        };
        let cursor = self
            .state
            .lock()
            .expect("Chrome adapter state")
            .sessions
            .get(&context.session_id)
            .filter(|session| session.target_id == context.target_id)
            .map(|session| session.cursor)
            .unwrap_or(0);
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
        if operation.command == "observe.evidence" {
            return self.evidence(context, operation);
        }
        let observation = match self.connection(&context.target_id, false) {
            Ok(client) => client,
            Err(error) => return rejected(error),
        };
        if operation.command.starts_with("observe.") {
            let reply = page::observe(&observation, context, operation, cancellation);
            if matches!(operation.command.as_str(), "observe.query" | "observe.tree") {
                self.register_observation_refs(context, &reply);
            }
            return reply;
        }
        if operation.command == "raw.cdp"
            && operation.input.get("intent").and_then(Value::as_str) == Some("query")
        {
            let raw = match self.connection(&context.target_id, true) {
                Ok(client) => client,
                Err(error) => return rejected(error),
            };
            return page::raw_query(&raw, context, operation, cancellation);
        }
        let raw = if operation.command == "raw.cdp" {
            match self.connection(&context.target_id, true) {
                Ok(client) => Some(client),
                Err(error) => return rejected(error),
            }
        } else {
            None
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
}
