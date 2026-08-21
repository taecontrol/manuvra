use crate::model::{
    AdapterContext, AdapterDelivery, AdapterOperation, AdapterReply, TargetAdapter,
    TargetDescriptor,
};
use serde_json::{Value, json};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

const ONE_PIXEL_PNG: &[u8] = &[
    137, 80, 78, 71, 13, 10, 26, 10, 0, 0, 0, 13, 73, 72, 68, 82, 0, 0, 0, 1, 0, 0, 0, 1, 8, 6, 0,
    0, 0, 31, 21, 196, 137, 0, 0, 0, 13, 73, 68, 65, 84, 8, 215, 99, 248, 207, 192, 240, 31, 0, 5,
    0, 1, 255, 137, 153, 61, 29, 0, 0, 0, 0, 73, 69, 78, 68, 174, 66, 96, 130,
];

#[derive(Default)]
pub struct FakeAdapter;

impl TargetAdapter for FakeAdapter {
    fn targets(&self) -> Vec<TargetDescriptor> {
        vec![fake_chrome(), fake_macos()]
    }

    fn setup_permissions(
        &self,
        _deadline: std::time::Instant,
    ) -> Option<Result<Value, crate::model::AdapterError>> {
        let granted = json!({
            "before_granted": true,
            "prompt_requested": false,
            "settings_opened": false,
            "granted": true,
            "freshly_granted": false,
            "residual": false
        });
        Some(Ok(json!({
            "permissions": {
                "accessibility": granted,
                "screen_recording": granted,
                "post_event": granted
            }
        })))
    }

    fn invoke(
        &self,
        context: &AdapterContext,
        operation: &AdapterOperation,
        cancellation: Arc<AtomicBool>,
    ) -> AdapterReply {
        let behavior = fake_behavior(operation);
        assert!(behavior != "panic", "injected fake adapter panic");
        if operation.command == "observe.screenshot" {
            thread::sleep(Duration::from_millis(20));
        }
        if behavior == "block" {
            return block_until_cancelled(context.deadline, cancellation);
        }
        if behavior == "reject" {
            return AdapterReply {
                delivery: AdapterDelivery::Rejected,
                response: json!({"fake": "rejected"}),
                screenshot: None,
                screenshot_width: None,
                screenshot_height: None,
                frame_signature: None,
                artifact: None,
                error: None,
                timing: Default::default(),
                already_settled: false,
                relevant_event_after_ms: None,
                continuous_events: false,
                capture_race_once: false,
                interrupted: false,
            };
        }
        if behavior == "ambiguous" {
            return AdapterReply {
                delivery: AdapterDelivery::Unknown,
                response: json!({"fake": "transport_disconnected"}),
                screenshot: None,
                screenshot_width: None,
                screenshot_height: None,
                frame_signature: None,
                artifact: None,
                error: None,
                timing: Default::default(),
                already_settled: false,
                relevant_event_after_ms: None,
                continuous_events: false,
                capture_race_once: false,
                interrupted: false,
            };
        }

        let response = fake_response(context, operation);
        let mut reply = AdapterReply::confirmed(response, Some(ONE_PIXEL_PNG.to_vec()));
        reply.screenshot_width = Some(1);
        reply.screenshot_height = Some(1);
        reply.frame_signature = Some("fake-1x1".to_owned());
        reply.continuous_events = behavior == "continuous";
        reply.capture_race_once = behavior == "race";
        reply.interrupted = behavior == "interrupt";
        reply.relevant_event_after_ms = (behavior == "cascade").then_some(10);
        reply
    }
}

fn fake_response(context: &AdapterContext, operation: &AdapterOperation) -> Value {
    match operation.command.as_str() {
        "observe.query" if query_name(operation) == Some("Missing") => {
            json!({"matches": [], "overflow": []})
        }
        "observe.query" if query_name(operation) == Some("Ambiguous") => json!({
            "matches": [
                fake_match(operation, "1"),
                fake_match(operation, "2")
            ],
            "overflow": []
        }),
        "observe.query" => json!({
            "matches": [{
                "backend_id": "1", "role": "button", "name": operation.input
                    .get("semantic").and_then(|value| value.get("name")).cloned(),
                "text": null, "identifier": null
            }],
            "overflow": []
        }),
        "observe.tree" => json!({
            "tree": {
                "complete": true,
                "target_id": context.target_id,
                "nodes": [
                    {"ref": fake_ref(context, "1"), "role": "window", "name": "Fake Target"},
                    {"ref": fake_ref(context, "2"), "role": "button", "name": "Save"},
                    {"ref": fake_ref(context, "3"), "role": "textbox", "name": "Email"}
                ]
            },
            "node_count": 3
        }),
        "observe.evidence" => json!({"kind": operation.input.get("kind")}),
        _ => json!({
            "fake": "confirmed",
            "command": operation.command,
            "target": context.target_id,
            "action_sequence": context.action_sequence,
        }),
    }
}

fn query_name(operation: &AdapterOperation) -> Option<&str> {
    operation.input.get("semantic")?.get("name")?.as_str()
}

fn fake_match(operation: &AdapterOperation, backend: &str) -> Value {
    json!({
        "backend_id": backend,
        "role": "button",
        "name": query_name(operation),
        "text": null,
        "identifier": null
    })
}

fn fake_ref(context: &AdapterContext, backend: &str) -> String {
    format!(
        "e_{}_{}_{}",
        context.reference_namespace, context.reference_epoch, backend
    )
}

fn fake_chrome() -> TargetDescriptor {
    TargetDescriptor {
        target_id: "chrome_fake_1".to_owned(),
        generation: 1,
        kind: "chrome".to_owned(),
        owner: "Chrome".to_owned(),
        title: Some("Fake Chrome".to_owned()),
        capabilities: vec![
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
        .collect(),
    }
}

fn fake_macos() -> TargetDescriptor {
    TargetDescriptor {
        target_id: "macos_fake_1".to_owned(),
        generation: 1,
        kind: "macos".to_owned(),
        owner: "Fake".to_owned(),
        title: Some("Fake Target".to_owned()),
        capabilities: vec![
            "common.click",
            "common.type",
            "common.press",
            "common.scroll",
            "observation.query",
            "observation.screenshot",
            "observation.tree",
            "observation.evidence",
            "raw.ax.get",
            "raw.ax.set",
            "raw.ax.perform",
        ]
        .into_iter()
        .map(str::to_owned)
        .collect(),
    }
}

fn fake_behavior(operation: &AdapterOperation) -> String {
    operation
        .input
        .get("method")
        .and_then(Value::as_str)
        .and_then(|method| method.strip_prefix("Fake."))
        .unwrap_or("confirmed")
        .to_ascii_lowercase()
}

fn block_until_cancelled(deadline: Instant, cancellation: Arc<AtomicBool>) -> AdapterReply {
    while Instant::now() < deadline {
        if cancellation.load(Ordering::SeqCst) {
            return AdapterReply {
                delivery: AdapterDelivery::Unknown,
                response: json!({"fake": "cancelled"}),
                screenshot: None,
                screenshot_width: None,
                screenshot_height: None,
                frame_signature: None,
                artifact: None,
                error: None,
                timing: Default::default(),
                already_settled: false,
                relevant_event_after_ms: None,
                continuous_events: false,
                capture_race_once: false,
                interrupted: false,
            };
        }
        thread::sleep(Duration::from_millis(2));
    }
    AdapterReply {
        delivery: AdapterDelivery::Unknown,
        response: json!({"fake": "deadline"}),
        screenshot: None,
        screenshot_width: None,
        screenshot_height: None,
        frame_signature: None,
        artifact: None,
        error: None,
        timing: Default::default(),
        already_settled: false,
        relevant_event_after_ms: None,
        continuous_events: false,
        capture_race_once: false,
        interrupted: false,
    }
}
