use crate::discovery::WindowRecord;
use crate::permissions::PermissionSnapshot;
use manuvra_runtime::{
    AdapterArtifact, AdapterContext, AdapterDelivery, AdapterOperation, AdapterReply,
    AdapterSession,
};
use serde_json::{Value, json};
use std::collections::{HashMap, VecDeque};

const JOURNAL_LIMIT: usize = 256;
const TIMING_LIMIT: usize = 100;

#[derive(Default)]
pub(crate) struct EvidenceState {
    sessions: HashMap<String, SessionEvidence>,
}

struct SessionEvidence {
    target_id: String,
    next_cursor: u64,
    dropped: u64,
    log_dropped: u64,
    timing_dropped: u64,
    events: VecDeque<Value>,
    logs: VecDeque<Value>,
    timings: VecDeque<Value>,
}

impl EvidenceState {
    pub fn opened(&mut self, session: &AdapterSession) {
        self.sessions.insert(
            session.session_id.clone(),
            SessionEvidence {
                target_id: session.target_id.clone(),
                next_cursor: 1,
                dropped: 0,
                log_dropped: 0,
                timing_dropped: 0,
                events: VecDeque::new(),
                logs: VecDeque::new(),
                timings: VecDeque::new(),
            },
        );
    }

    pub fn closed(&mut self, session: &AdapterSession) {
        self.sessions.remove(&session.session_id);
    }

    pub fn record(
        &mut self,
        context: &AdapterContext,
        operation: &AdapterOperation,
        reply: &AdapterReply,
    ) {
        let Some(session) = self.sessions.get_mut(&context.session_id) else {
            return;
        };
        push_bounded(
            &mut session.logs,
            json!({
                "action_sequence": context.action_sequence,
                "command": operation.command,
                "delivery": delivery_name(&reply.delivery),
                "interrupted": reply.interrupted,
                "error_code": reply.error.as_ref().map(|error| error.code.as_str()),
                "native_error": reply.error.as_ref().and_then(|error| error.details.as_ref()),
            }),
            crate::seam::journal_limit("logs", JOURNAL_LIMIT),
            Some(&mut session.log_dropped),
        );
        push_bounded(
            &mut session.timings,
            json!({
                "action_sequence": context.action_sequence,
                "command": operation.command,
                "preflight_ms": reply.timing.preflight_ms,
                "dispatch_ms": reply.timing.dispatch_ms,
                "stabilize_ms": reply.timing.stabilize_ms,
                "capture_ms": reply.timing.capture_ms,
            }),
            crate::seam::journal_limit("timings", TIMING_LIMIT),
            Some(&mut session.timing_dropped),
        );
        let event_count = reply
            .response
            .get("ax_events")
            .and_then(Value::as_u64)
            .unwrap_or(0);
        if event_count > 0 {
            let cursor = session.next_cursor;
            session.next_cursor = session.next_cursor.saturating_add(1);
            push_bounded(
                &mut session.events,
                json!({
                    "cursor": cursor,
                    "action_sequence": context.action_sequence,
                    "kind": "AXObserver.quiet_period",
                    "notifications": event_count,
                    "command": operation.command,
                }),
                crate::seam::journal_limit("events", JOURNAL_LIMIT),
                Some(&mut session.dropped),
            );
        }
    }

    pub fn reply(
        &self,
        record: &WindowRecord,
        context: &AdapterContext,
        operation: &AdapterOperation,
    ) -> AdapterReply {
        let kind = operation
            .input
            .get("kind")
            .and_then(Value::as_str)
            .unwrap_or("diagnostics");
        let session = self.sessions.get(&context.session_id);
        let value = match kind {
            "events" => json!({
                "kind": "events",
                "complete": session.is_none_or(|session| session.dropped == 0),
                "target_id": context.target_id,
                "start_cursor": session.and_then(|session| session.events.front()).and_then(|event| event.get("cursor")).cloned(),
                "end_cursor": session.and_then(|session| session.events.back()).and_then(|event| event.get("cursor")).cloned(),
                "dropped": session.map(|session| session.dropped).unwrap_or(0),
                "events": session.map(|session| session.events.iter().cloned().collect::<Vec<_>>()).unwrap_or_default(),
            }),
            "logs" => json!({
                "kind": "logs",
                "complete": session.is_none_or(|session| session.log_dropped == 0),
                "target_id": context.target_id,
                "dropped": session.map(|session| session.log_dropped).unwrap_or(0),
                "events": session.map(|session| session.logs.iter().cloned().collect::<Vec<_>>()).unwrap_or_default(),
            }),
            "timings" => json!({
                "kind": "timings",
                "complete": session.is_none_or(|session| session.timing_dropped == 0),
                "dropped": session.map(|session| session.timing_dropped).unwrap_or(0),
                "target_id": context.target_id,
                "entries": session.map(|session| session.timings.iter().cloned().collect::<Vec<_>>()).unwrap_or_default(),
            }),
            _ => json!({
                "kind": "diagnostics",
                "complete": true,
                "target_id": context.target_id,
                "target_generation": context.target_generation,
                "session_target_matches": session.is_some_and(|session| session.target_id == context.target_id),
                "window": {
                    "application": record.snapshot.owner,
                    "title": record.snapshot.title,
                    "is_on_screen": record.snapshot.is_on_screen,
                },
                "permissions": PermissionSnapshot::current().diagnostics(),
            }),
        };
        artifact_reply(kind, value)
    }
}

fn delivery_name(delivery: &AdapterDelivery) -> &'static str {
    match delivery {
        AdapterDelivery::Rejected => "rejected",
        AdapterDelivery::Confirmed => "confirmed",
        AdapterDelivery::Unknown => "unknown",
    }
}

fn push_bounded(
    queue: &mut VecDeque<Value>,
    value: Value,
    limit: usize,
    mut dropped: Option<&mut u64>,
) {
    while queue.len() >= limit {
        queue.pop_front();
        if let Some(dropped) = dropped.as_deref_mut() {
            *dropped = dropped.saturating_add(1);
        }
    }
    queue.push_back(value);
}

fn artifact_reply(kind: &str, value: Value) -> AdapterReply {
    let bytes = serde_json::to_vec(&value).expect("macOS evidence JSON");
    let complete = value
        .get("complete")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let mut reply = AdapterReply::confirmed(json!({"kind": kind, "complete": complete}), None);
    reply.artifact = Some(AdapterArtifact {
        kind: kind.to_owned(),
        extension: "json".to_owned(),
        media_type: "application/json".to_owned(),
        bytes,
    });
    reply
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::discovery::{WindowBounds, WindowSnapshot};
    use manuvra_runtime::{ExecutionMode, TargetDescriptor};
    use std::time::{Duration, Instant};

    #[test]
    fn bounded_queue_counts_drops() {
        let mut queue = VecDeque::new();
        let mut dropped = 0;
        push_bounded(&mut queue, json!(1), 1, Some(&mut dropped));
        push_bounded(&mut queue, json!(2), 1, Some(&mut dropped));
        assert_eq!(queue, VecDeque::from([json!(2)]));
        assert_eq!(dropped, 1);
    }

    #[test]
    fn overflow_artifacts_and_replies_fail_closed() {
        let reply = artifact_reply("events", json!({"complete": false, "dropped": 1}));
        assert_eq!(reply.response["complete"], false);
        let artifact = reply.artifact.unwrap();
        let value: Value = serde_json::from_slice(&artifact.bytes).unwrap();
        assert_eq!(value["complete"], false);
        assert_eq!(value["dropped"], 1);
        crate::test_oracles::write(
            "evidence-overflow.json",
            &json!({
                "schema": "manuvra/cp-07-boundary-oracle@1",
                "case": "evidence_overflow_fail_closed",
                "reply": reply.response,
                "artifact": value,
            }),
        );
    }

    #[test]
    fn completed_operation_is_retained_as_a_bounded_native_log() {
        let session = AdapterSession {
            session_id: "s_logs".to_owned(),
            target_id: "macos_logs".to_owned(),
            target_generation: 1,
        };
        let context = AdapterContext {
            session_id: session.session_id.clone(),
            target_id: session.target_id.clone(),
            target_generation: session.target_generation,
            action_sequence: 1,
            reference_namespace: "n_logs".to_owned(),
            reference_epoch: 1,
            frame_token: None,
            mode: ExecutionMode::Background,
            deadline: Instant::now() + Duration::from_secs(1),
        };
        let operation = AdapterOperation::new("action.click".to_owned(), json!({}));
        let record = WindowRecord {
            descriptor: TargetDescriptor {
                target_id: session.target_id.clone(),
                generation: session.target_generation,
                kind: "macos".to_owned(),
                owner: "Fixture".to_owned(),
                title: Some("Window".to_owned()),
                capabilities: Vec::new(),
            },
            snapshot: WindowSnapshot {
                pid: 42,
                window_id: 7,
                owner: "Fixture".to_owned(),
                title: Some("Window".to_owned()),
                bounds: WindowBounds {
                    x: 0.0,
                    y: 0.0,
                    width: 100.0,
                    height: 100.0,
                },
                is_on_screen: true,
            },
            present: true,
        };
        let mut evidence = EvidenceState::default();
        evidence.opened(&session);
        evidence.record(
            &context,
            &operation,
            &AdapterReply::confirmed(json!({"performed": "AXPress"}), None),
        );
        let logs = AdapterOperation::new("observe.evidence".to_owned(), json!({"kind": "logs"}));
        let reply = evidence.reply(&record, &context, &logs);
        let artifact: Value =
            serde_json::from_slice(&reply.artifact.expect("logs artifact").bytes).unwrap();

        assert_eq!(artifact["complete"], true);
        assert_eq!(artifact["dropped"], 0);
        assert_eq!(artifact["events"].as_array().unwrap().len(), 1);
        assert_eq!(artifact["events"][0]["action_sequence"], 1);
        assert_eq!(artifact["events"][0]["command"], "action.click");
        assert_eq!(artifact["events"][0]["delivery"], "confirmed");
    }
}
