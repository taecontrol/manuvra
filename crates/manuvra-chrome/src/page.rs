use crate::transport::{
    CdpClient, CommandFailure, CommandOutcome, JournalEvent, event_method, event_params,
    is_relevant_event,
};
use base64::Engine;
use manuvra_runtime::{
    AdapterContext, AdapterError, AdapterOperation, AdapterReply, AdapterTiming,
};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

const QUIET_WINDOW: Duration = Duration::from_millis(50);
const CLICK_FOLLOWING_DOCUMENT_WATCH: Duration = Duration::from_millis(400);
type PageResult<T> = Result<T, Box<AdapterReply>>;

pub fn prepare(
    client: &CdpClient,
    context: &AdapterContext,
    operation: &AdapterOperation,
    cancellation: Arc<AtomicBool>,
) -> Result<AdapterOperation, AdapterError> {
    let started = Instant::now();
    let Some(locator) = operation.input.get("locator") else {
        return Ok(operation.clone());
    };
    let prepared = match locator.get("kind").and_then(Value::as_str) {
        Some("semantic") => resolve_semantic(client, context, locator, cancellation)?,
        Some("ref") => resolve_reference(client, context, locator, cancellation)?,
        Some("point") => resolve_point(client, context, locator, cancellation)?,
        _ => return Err(error("invalid_request", "invalid Chrome locator")),
    };
    let mut operation = operation.clone();
    operation.prepared = Some(json!({
        "target": prepared,
        "preflight_ms": millis(started.elapsed()),
    }));
    Ok(operation)
}

pub fn observe(
    client: &CdpClient,
    context: &AdapterContext,
    operation: &AdapterOperation,
    cancellation: Arc<AtomicBool>,
) -> AdapterReply {
    match operation.command.as_str() {
        "observe.screenshot" => screenshot_reply(client, context, cancellation),
        "observe.query" => query_reply(client, context, operation, cancellation),
        "observe.tree" => tree_reply(client, context, cancellation),
        _ => rejected("command_unsupported", "unsupported Chrome observation"),
    }
}

pub fn mutate(
    observation: &CdpClient,
    raw: Option<&CdpClient>,
    context: &AdapterContext,
    operation: &AdapterOperation,
    cancellation: Arc<AtomicBool>,
) -> AdapterReply {
    observation.set_action_sequence(context.action_sequence);
    let fence = observation.cursor();
    let dispatch_started = Instant::now();
    let dispatched = dispatch(observation, raw, context, operation, cancellation.clone());
    let dispatch_ms = millis(dispatch_started.elapsed());
    let (response, expected_loader) = match dispatched {
        Ok(value) => value,
        Err(reply) => {
            observation.set_action_sequence(0);
            return *reply;
        }
    };
    let stabilize_started = Instant::now();
    if let Err(reply) = wait_for_quiet(
        observation,
        fence,
        expected_loader.as_deref(),
        context.deadline,
        &cancellation,
    ) {
        observation.set_action_sequence(0);
        return *reply;
    }
    let stabilize_ms = millis(stabilize_started.elapsed());
    let capture_started = Instant::now();
    let capture = capture_fenced(observation, context, cancellation);
    observation.set_action_sequence(0);
    match capture {
        Ok(capture) => {
            let mut reply = AdapterReply::confirmed(response, Some(capture.bytes));
            reply.screenshot_width = Some(capture.width);
            reply.screenshot_height = Some(capture.height);
            reply.frame_signature = Some(capture.signature);
            reply.timing = AdapterTiming {
                preflight_ms: operation
                    .prepared
                    .as_ref()
                    .and_then(|value| value.get("preflight_ms"))
                    .and_then(Value::as_u64)
                    .unwrap_or(0),
                dispatch_ms,
                stabilize_ms,
                capture_ms: millis(capture_started.elapsed()),
            };
            reply.already_settled = true;
            reply
        }
        Err(reply) => *reply,
    }
}

pub fn raw_query(
    client: &CdpClient,
    context: &AdapterContext,
    operation: &AdapterOperation,
    cancellation: Arc<AtomicBool>,
) -> AdapterReply {
    let method = operation
        .input
        .get("method")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let params = operation
        .input
        .get("params")
        .cloned()
        .unwrap_or_else(|| json!({}));
    outcome_reply(client.command(method, params, context.deadline, cancellation))
}

fn dispatch(
    observation: &CdpClient,
    raw: Option<&CdpClient>,
    context: &AdapterContext,
    operation: &AdapterOperation,
    cancellation: Arc<AtomicBool>,
) -> PageResult<(Value, Option<String>)> {
    match operation.command.as_str() {
        "action.click" => dispatch_click(observation, context, operation, cancellation),
        "action.type" => dispatch_type(observation, context, operation, cancellation),
        "action.press" => dispatch_press(observation, context, operation, cancellation),
        "action.scroll" => dispatch_scroll(observation, context, operation, cancellation),
        "action.navigate" => dispatch_navigate(observation, context, operation, cancellation),
        "raw.cdp" => dispatch_raw(
            raw.ok_or_else(|| {
                Box::new(rejected(
                    "capability_unavailable",
                    "raw CDP connection unavailable",
                ))
            })?,
            context,
            operation,
            cancellation,
        ),
        _ => Err(Box::new(rejected(
            "command_unsupported",
            "unsupported Chrome action",
        ))),
    }
}

fn dispatch_click(
    client: &CdpClient,
    context: &AdapterContext,
    operation: &AdapterOperation,
    cancellation: Arc<AtomicBool>,
) -> PageResult<(Value, Option<String>)> {
    let fence = client.cursor();
    let main_document = page_main_frame(client, context, cancellation.clone());
    let target = prepared_target(operation)?;
    let button = operation
        .input
        .get("button")
        .and_then(Value::as_str)
        .unwrap_or("left");
    let count = operation
        .input
        .get("count")
        .and_then(Value::as_u64)
        .unwrap_or(1);
    let mut effects_possible = false;
    for event_type in ["mousePressed", "mouseReleased"] {
        command_result_after(
            client,
            "Input.dispatchMouseEvent",
            json!({
                "type": event_type,
                "x": target.x,
                "y": target.y,
                "button": button,
                "clickCount": count,
            }),
            context,
            cancellation.clone(),
            effects_possible,
        )?;
        effects_possible = true;
    }
    let loader =
        watch_for_following_document(client, context, fence, main_document, &cancellation)?;
    Ok((json!({"dispatched": "click"}), loader))
}

fn dispatch_type(
    client: &CdpClient,
    context: &AdapterContext,
    operation: &AdapterOperation,
    cancellation: Arc<AtomicBool>,
) -> PageResult<(Value, Option<String>)> {
    let target = prepared_target(operation)?;
    let backend_id = target.backend_id.ok_or_else(|| {
        Box::new(rejected(
            "capability_unavailable",
            "typing requires an element target",
        ))
    })?;
    focus_backend(
        client,
        context,
        backend_id,
        operation
            .input
            .get("replace")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        cancellation.clone(),
    )?;
    let text = operation
        .input
        .get("text")
        .and_then(Value::as_str)
        .unwrap_or_default();
    command_result_after(
        client,
        "Input.insertText",
        json!({"text": text}),
        context,
        cancellation,
        true,
    )?;
    Ok((json!({"dispatched": "type"}), None))
}

fn dispatch_press(
    client: &CdpClient,
    context: &AdapterContext,
    operation: &AdapterOperation,
    cancellation: Arc<AtomicBool>,
) -> PageResult<(Value, Option<String>)> {
    let mut effects_possible = false;
    if operation.input.get("locator").is_some() {
        let target = prepared_target(operation)?;
        let backend_id = target.backend_id.ok_or_else(|| {
            Box::new(rejected(
                "capability_unavailable",
                "key target is not an element",
            ))
        })?;
        focus_backend(client, context, backend_id, false, cancellation.clone())?;
        effects_possible = true;
    }
    let key = operation
        .input
        .get("key")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let key_data = normalized_key(key).ok_or_else(|| {
        Box::new(rejected(
            "capability_unavailable",
            "unsupported normalized Chrome key",
        ))
    })?;
    for event_type in ["rawKeyDown", "keyUp"] {
        command_result_after(
            client,
            "Input.dispatchKeyEvent",
            json!({
                "type": event_type,
                "key": key_data.key,
                "code": key_data.code,
                "windowsVirtualKeyCode": key_data.virtual_code,
                "text": if event_type == "rawKeyDown" { key_data.text } else { "" },
            }),
            context,
            cancellation.clone(),
            effects_possible,
        )?;
        effects_possible = true;
    }
    Ok((json!({"dispatched": "press"}), None))
}

fn dispatch_scroll(
    client: &CdpClient,
    context: &AdapterContext,
    operation: &AdapterOperation,
    cancellation: Arc<AtomicBool>,
) -> PageResult<(Value, Option<String>)> {
    let target = if operation.input.get("locator").is_some() {
        prepared_target(operation)?
    } else {
        viewport_center(client, context, cancellation.clone())?
    };
    command_result(
        client,
        "Input.dispatchMouseEvent",
        json!({
            "type": "mouseWheel",
            "x": target.x,
            "y": target.y,
            "deltaX": operation.input.get("delta_x").cloned().unwrap_or(json!(0)),
            "deltaY": operation.input.get("delta_y").cloned().unwrap_or(json!(0)),
        }),
        context,
        cancellation,
    )?;
    Ok((json!({"dispatched": "scroll"}), None))
}

fn dispatch_navigate(
    client: &CdpClient,
    context: &AdapterContext,
    operation: &AdapterOperation,
    cancellation: Arc<AtomicBool>,
) -> PageResult<(Value, Option<String>)> {
    let response = command_result(
        client,
        "Page.navigate",
        json!({"url": operation.input.get("url").cloned().unwrap_or(Value::Null)}),
        context,
        cancellation,
    )?;
    if response.get("errorText").is_some() {
        return Err(Box::new(rejected(
            "backend_rejected",
            "Chrome navigation was rejected",
        )));
    }
    let loader = response
        .get("loaderId")
        .and_then(Value::as_str)
        .map(str::to_owned);
    Ok((response, loader))
}

fn dispatch_raw(
    client: &CdpClient,
    context: &AdapterContext,
    operation: &AdapterOperation,
    cancellation: Arc<AtomicBool>,
) -> PageResult<(Value, Option<String>)> {
    let method = operation
        .input
        .get("method")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let params = operation
        .input
        .get("params")
        .cloned()
        .unwrap_or_else(|| json!({}));
    match client.command(method, params, context.deadline, cancellation) {
        CommandOutcome::Confirmed(response) => Ok((response, None)),
        outcome => Err(Box::new(outcome_reply(outcome))),
    }
}

fn focus_backend(
    client: &CdpClient,
    context: &AdapterContext,
    backend_id: u64,
    replace: bool,
    cancellation: Arc<AtomicBool>,
) -> PageResult<()> {
    let resolved = command_result(
        client,
        "DOM.resolveNode",
        json!({"backendNodeId": backend_id}),
        context,
        cancellation.clone(),
    )?;
    let object_id = resolved
        .pointer("/object/objectId")
        .and_then(Value::as_str)
        .ok_or_else(|| {
            Box::new(rejected(
                "element_stale",
                "Chrome element no longer resolves",
            ))
        })?;
    command_result(
        client,
        "Runtime.callFunctionOn",
        json!({
            "objectId": object_id,
            "functionDeclaration": "function(replace){this.focus();if(replace&&typeof this.select==='function'){this.select();}}",
            "arguments": [{"value": replace}],
            "returnByValue": true,
        }),
        context,
        cancellation,
    )?;
    Ok(())
}

fn screenshot_reply(
    client: &CdpClient,
    context: &AdapterContext,
    cancellation: Arc<AtomicBool>,
) -> AdapterReply {
    match capture_once(client, context, cancellation) {
        Ok(capture) => {
            let mut reply = AdapterReply::confirmed(json!({}), Some(capture.bytes));
            reply.screenshot_width = Some(capture.width);
            reply.screenshot_height = Some(capture.height);
            reply.frame_signature = Some(capture.signature);
            reply
        }
        Err(reply) => *reply,
    }
}

fn query_reply(
    client: &CdpClient,
    context: &AdapterContext,
    operation: &AdapterOperation,
    cancellation: Arc<AtomicBool>,
) -> AdapterReply {
    let semantic = operation
        .input
        .get("semantic")
        .cloned()
        .unwrap_or(Value::Null);
    let limit = operation
        .input
        .get("limit")
        .and_then(Value::as_u64)
        .unwrap_or(5) as usize;
    match ax_nodes(client, context, cancellation.clone())
        .and_then(|nodes| matching_nodes(client, context, &semantic, nodes, cancellation))
    {
        Ok(matches) => {
            let split = matches.len().min(limit);
            AdapterReply::confirmed(
                json!({"matches": &matches[..split], "overflow": &matches[split..]}),
                None,
            )
        }
        Err(error) => rejected(
            &error.code,
            error.message.as_deref().unwrap_or("Chrome query failed"),
        ),
    }
}

fn tree_reply(
    client: &CdpClient,
    context: &AdapterContext,
    cancellation: Arc<AtomicBool>,
) -> AdapterReply {
    match ax_nodes(client, context, cancellation) {
        Ok(nodes) => {
            let normalized = nodes
                .iter()
                .filter_map(|node| {
                    let mut public = public_node(context, node)?;
                    public.as_object_mut()?.remove("backend_id");
                    Some(public)
                })
                .collect::<Vec<_>>();
            let count = normalized.len();
            AdapterReply::confirmed(
                json!({
                    "tree": {"complete": true, "target_id": context.target_id, "nodes": normalized},
                    "node_count": count,
                }),
                None,
            )
        }
        Err(error) => rejected(
            &error.code,
            error.message.as_deref().unwrap_or("Chrome tree failed"),
        ),
    }
}

fn resolve_semantic(
    client: &CdpClient,
    context: &AdapterContext,
    semantic: &Value,
    cancellation: Arc<AtomicBool>,
) -> Result<Value, AdapterError> {
    let nodes = ax_nodes(client, context, cancellation.clone())?;
    let matches = matching_nodes(client, context, semantic, nodes, cancellation.clone())?;
    match matches.as_slice() {
        [] => Err(error(
            "element_not_found",
            "Chrome semantic locator matched no element",
        )),
        [node] => prepare_backend(client, context, node, cancellation),
        _ => Err(error(
            "ambiguous_target",
            "Chrome semantic locator matched multiple elements",
        )),
    }
}

fn resolve_reference(
    client: &CdpClient,
    context: &AdapterContext,
    locator: &Value,
    cancellation: Arc<AtomicBool>,
) -> Result<Value, AdapterError> {
    let reference = locator
        .get("ref")
        .and_then(Value::as_str)
        .ok_or_else(|| error("invalid_request", "missing element reference"))?;
    let nodes = ax_nodes(client, context, cancellation.clone())?;
    let node = nodes.iter().find_map(|node| {
        let public = public_node(context, node)?;
        (public.get("ref").and_then(Value::as_str) == Some(reference)).then_some(public)
    });
    let node = node.ok_or_else(|| {
        error(
            "element_stale",
            "Chrome element is detached or belongs to a different frame",
        )
    })?;
    prepare_backend(client, context, &node, cancellation)
}

fn resolve_point(
    client: &CdpClient,
    context: &AdapterContext,
    locator: &Value,
    cancellation: Arc<AtomicBool>,
) -> Result<Value, AdapterError> {
    let layout = layout(client, context, cancellation)?;
    let signature = layout_signature(&layout);
    let token = locator
        .get("frame_token")
        .and_then(Value::as_str)
        .unwrap_or_default();
    if !token.ends_with(&signature) {
        return Err(error("frame_stale", "Chrome viewport geometry changed"));
    }
    let viewport = layout.get("visualViewport").unwrap_or(&Value::Null);
    let width = viewport
        .get("clientWidth")
        .and_then(Value::as_f64)
        .unwrap_or(0.0);
    let height = viewport
        .get("clientHeight")
        .and_then(Value::as_f64)
        .unwrap_or(0.0);
    let x = locator.get("x").and_then(Value::as_f64).unwrap_or(-1.0);
    let y = locator.get("y").and_then(Value::as_f64).unwrap_or(-1.0);
    if x < 0.0 || y < 0.0 || x >= width || y >= height {
        return Err(error(
            "element_not_found",
            "Chrome point is outside the current viewport",
        ));
    }
    Ok(json!({
        "x": x,
        "y": y,
        "backend_id": null,
    }))
}

fn ax_nodes(
    client: &CdpClient,
    context: &AdapterContext,
    cancellation: Arc<AtomicBool>,
) -> Result<Vec<Value>, AdapterError> {
    let tree = command_preflight(
        client,
        "Page.getFrameTree",
        json!({}),
        context,
        cancellation.clone(),
    )?;
    let mut frames = Vec::new();
    collect_frame_ids(tree.get("frameTree").unwrap_or(&Value::Null), &mut frames);
    if frames.is_empty() {
        return Err(error(
            "observation_failed",
            "Chrome returned no page frame tree",
        ));
    }
    let mut combined = Vec::new();
    let mut seen_nodes = HashSet::new();
    for frame_id in frames {
        let result = command_preflight(
            client,
            "Accessibility.getFullAXTree",
            json!({"frameId": frame_id}),
            context,
            cancellation.clone(),
        )?;
        let nodes = result
            .get("nodes")
            .and_then(Value::as_array)
            .ok_or_else(|| {
                error(
                    "observation_failed",
                    "Chrome returned no complete accessibility tree for an attached frame",
                )
            })?;
        for node in nodes {
            let owning_frame = node
                .get("frameId")
                .and_then(Value::as_str)
                .unwrap_or(&frame_id)
                .to_owned();
            let identity = node
                .get("backendDOMNodeId")
                .and_then(Value::as_u64)
                .map(|backend| format!("{owning_frame}:backend:{backend}"))
                .or_else(|| {
                    node.get("nodeId")
                        .and_then(Value::as_str)
                        .map(|node_id| format!("{owning_frame}:ax:{node_id}"))
                })
                .unwrap_or_else(|| format!("{owning_frame}:index:{}", combined.len()));
            if !seen_nodes.insert(identity) {
                continue;
            }
            let mut node = node.clone();
            node["computerUseFrameId"] = Value::String(owning_frame);
            combined.push(node);
        }
    }
    Ok(combined)
}

fn collect_frame_ids(tree: &Value, output: &mut Vec<String>) {
    if let Some(frame_id) = tree.pointer("/frame/id").and_then(Value::as_str) {
        output.push(frame_id.to_owned());
    }
    if let Some(children) = tree.get("childFrames").and_then(Value::as_array) {
        for child in children {
            collect_frame_ids(child, output);
        }
    }
}

fn matching_nodes(
    client: &CdpClient,
    context: &AdapterContext,
    semantic: &Value,
    nodes: Vec<Value>,
    cancellation: Arc<AtomicBool>,
) -> Result<Vec<Value>, AdapterError> {
    let tree = AxTree::index(&nodes);
    let mut matches = Vec::new();
    for node in &nodes {
        let Some(mut public) = public_node(context, node) else {
            continue;
        };
        if semantic.get("identifier").is_some() || semantic.get("text").is_some() {
            attach_dom_fields(
                client,
                context,
                semantic,
                node,
                &mut public,
                cancellation.clone(),
            )?;
        }
        if semantic_matches(semantic, &public) && ancestor_scope_matches(semantic, node, &tree) {
            matches.push(public);
        }
    }
    Ok(matches)
}

fn public_node(context: &AdapterContext, node: &Value) -> Option<Value> {
    if node
        .get("ignored")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        return None;
    }
    let backend_id = node.get("backendDOMNodeId")?.as_u64()?;
    let frame_id = node
        .get("computerUseFrameId")
        .and_then(Value::as_str)
        .unwrap_or("root");
    let backend_identity = format!("{}_{}", frame_tag(frame_id), backend_id);
    let role = ax_value(node.get("role"));
    let name = ax_value(node.get("name"));
    let value = ax_value(node.get("value"));
    let description = ax_value(node.get("description"));
    Some(json!({
        "backend_id": backend_identity,
        "ref": format!("e_{}_{}_{}_{}", context.reference_namespace, context.reference_epoch, frame_tag(frame_id), backend_id),
        "frame": frame_tag(frame_id),
        "role": role,
        "name": name,
        "text": value.or_else(|| name.clone()),
        "identifier": null,
        "description": description,
    }))
}

fn attach_dom_fields(
    client: &CdpClient,
    context: &AdapterContext,
    semantic: &Value,
    source: &Value,
    public: &mut Value,
    cancellation: Arc<AtomicBool>,
) -> Result<(), AdapterError> {
    let backend_id = source
        .get("backendDOMNodeId")
        .and_then(Value::as_u64)
        .ok_or_else(|| error("element_stale", "Chrome AX node has no DOM identity"))?;
    let described = command_preflight(
        client,
        "DOM.describeNode",
        json!({"backendNodeId": backend_id, "depth": 0}),
        context,
        cancellation.clone(),
    )?;
    let attributes = described
        .pointer("/node/attributes")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    for pair in attributes.chunks_exact(2) {
        if pair[0].as_str() == Some("id") {
            public["identifier"] = pair[1].clone();
            break;
        }
    }
    if semantic.get("text").is_some() {
        let resolved = command_preflight(
            client,
            "DOM.resolveNode",
            json!({"backendNodeId": backend_id}),
            context,
            cancellation.clone(),
        )?;
        let object_id = resolved
            .pointer("/object/objectId")
            .and_then(Value::as_str)
            .ok_or_else(|| error("element_stale", "Chrome element no longer resolves"))?;
        let text = command_preflight(
            client,
            "Runtime.callFunctionOn",
            json!({
                "objectId": object_id,
                "functionDeclaration": "function(){return this.textContent}",
                "returnByValue": true,
            }),
            context,
            cancellation,
        )?;
        public["text"] = text
            .pointer("/result/value")
            .cloned()
            .unwrap_or(Value::Null);
    }
    Ok(())
}

fn frame_tag(frame_id: &str) -> String {
    let digest = Sha256::digest(frame_id.as_bytes());
    digest[..4]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn semantic_matches(semantic: &Value, node: &Value) -> bool {
    ["role", "name", "text", "identifier"]
        .into_iter()
        .all(|field| {
            semantic.get(field).is_none_or(|expected| {
                normalize(expected.as_str().unwrap_or_default())
                    == normalize(node.get(field).and_then(Value::as_str).unwrap_or_default())
            })
        })
}

fn normalize(value: &str) -> String {
    value.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn prepare_backend(
    client: &CdpClient,
    context: &AdapterContext,
    node: &Value,
    cancellation: Arc<AtomicBool>,
) -> Result<Value, AdapterError> {
    let backend_id = node
        .get("backend_id")
        .and_then(Value::as_str)
        .and_then(|value| value.rsplit('_').next())
        .and_then(|value| value.parse::<u64>().ok())
        .ok_or_else(|| error("element_stale", "Chrome element has no backend identity"))?;
    let model = command_preflight(
        client,
        "DOM.getBoxModel",
        json!({"backendNodeId": backend_id}),
        context,
        cancellation,
    )?;
    let quad = model
        .pointer("/model/content")
        .and_then(Value::as_array)
        .ok_or_else(|| {
            error(
                "capability_unavailable",
                "Chrome element has no content box",
            )
        })?;
    let (x, y) = quad_center(quad)
        .ok_or_else(|| error("capability_unavailable", "Chrome element box is invalid"))?;
    Ok(json!({"backend_id": backend_id, "x": x, "y": y}))
}

fn quad_center(quad: &[Value]) -> Option<(f64, f64)> {
    if quad.len() != 8 {
        return None;
    }
    let numbers = quad.iter().map(Value::as_f64).collect::<Option<Vec<_>>>()?;
    Some((
        numbers.iter().step_by(2).sum::<f64>() / 4.0,
        numbers.iter().skip(1).step_by(2).sum::<f64>() / 4.0,
    ))
}

fn viewport_center(
    client: &CdpClient,
    context: &AdapterContext,
    cancellation: Arc<AtomicBool>,
) -> PageResult<PreparedTarget> {
    let layout = command_result(
        client,
        "Page.getLayoutMetrics",
        json!({}),
        context,
        cancellation,
    )?;
    let viewport = layout.get("visualViewport").unwrap_or(&Value::Null);
    Ok(PreparedTarget {
        backend_id: None,
        x: viewport
            .get("clientWidth")
            .and_then(Value::as_f64)
            .unwrap_or(1.0)
            / 2.0,
        y: viewport
            .get("clientHeight")
            .and_then(Value::as_f64)
            .unwrap_or(1.0)
            / 2.0,
    })
}

fn layout(
    client: &CdpClient,
    context: &AdapterContext,
    cancellation: Arc<AtomicBool>,
) -> Result<Value, AdapterError> {
    command_preflight(
        client,
        "Page.getLayoutMetrics",
        json!({}),
        context,
        cancellation,
    )
}

struct Capture {
    bytes: Vec<u8>,
    width: u32,
    height: u32,
    signature: String,
}

fn capture_fenced(
    client: &CdpClient,
    context: &AdapterContext,
    cancellation: Arc<AtomicBool>,
) -> PageResult<Capture> {
    loop {
        let before = client.cursor();
        let capture = capture_once(client, context, cancellation.clone())?;
        let raced = client
            .snapshot_since(before)
            .events
            .iter()
            .any(is_relevant_event);
        if !raced {
            return Ok(capture);
        }
        wait_for_quiet(client, before, None, context.deadline, &cancellation)?;
    }
}

fn capture_once(
    client: &CdpClient,
    context: &AdapterContext,
    cancellation: Arc<AtomicBool>,
) -> PageResult<Capture> {
    let layout = command_result(
        client,
        "Page.getLayoutMetrics",
        json!({}),
        context,
        cancellation.clone(),
    )?;
    let result = command_result(
        client,
        "Page.captureScreenshot",
        json!({"format": "png", "fromSurface": true}),
        context,
        cancellation,
    )?;
    let encoded = result
        .get("data")
        .and_then(Value::as_str)
        .ok_or_else(|| Box::new(rejected("capture_failed", "Chrome screenshot omitted data")))?;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(encoded)
        .map_err(|_| {
            Box::new(rejected(
                "capture_failed",
                "Chrome screenshot base64 was invalid",
            ))
        })?;
    let (width, height) = png_dimensions(&bytes).ok_or_else(|| {
        Box::new(rejected(
            "capture_failed",
            "Chrome screenshot was not a valid PNG",
        ))
    })?;
    Ok(Capture {
        bytes,
        width,
        height,
        signature: layout_signature(&layout),
    })
}

fn png_dimensions(bytes: &[u8]) -> Option<(u32, u32)> {
    (bytes.len() >= 24 && &bytes[..8] == b"\x89PNG\r\n\x1a\n").then(|| {
        (
            u32::from_be_bytes(bytes[16..20].try_into().expect("PNG width")),
            u32::from_be_bytes(bytes[20..24].try_into().expect("PNG height")),
        )
    })
}

fn layout_signature(layout: &Value) -> String {
    let bytes = serde_json::to_vec(layout).unwrap_or_default();
    let digest = Sha256::digest(bytes);
    digest[..8]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn wait_for_quiet(
    client: &CdpClient,
    fence: u64,
    expected_loader: Option<&str>,
    deadline: Instant,
    cancellation: &Arc<AtomicBool>,
) -> PageResult<()> {
    let mut processed = fence;
    let mut last_relevant = Instant::now();
    let mut navigation_ready = expected_loader.is_none();
    loop {
        if cancellation.load(Ordering::SeqCst) {
            return Err(Box::new(unknown(
                "cancelled",
                "cancelled during Chrome stabilization",
            )));
        }
        if Instant::now() >= deadline {
            let message = if expected_loader.is_some() {
                "Chrome document was not ready before the deadline"
            } else {
                "Chrome did not become logically quiet before the deadline"
            };
            return Err(Box::new(unknown("stabilization_timeout", message)));
        }
        let snapshot = client.snapshot_since(processed);
        if snapshot.overflowed {
            return Err(Box::new(unknown(
                "observation_failed",
                "Chrome event journal overflowed during stabilization",
            )));
        }
        for event in &snapshot.events {
            processed = event.cursor;
            if is_relevant_event(event) {
                last_relevant = Instant::now();
            }
            if navigation_event_matches(event, expected_loader) {
                navigation_ready = true;
            }
        }
        if navigation_ready && last_relevant.elapsed() >= QUIET_WINDOW {
            return Ok(());
        }
        let quiet_remaining = QUIET_WINDOW.saturating_sub(last_relevant.elapsed());
        let deadline_remaining = deadline.saturating_duration_since(Instant::now());
        client.wait_for_journal_change(
            processed,
            quiet_remaining
                .min(deadline_remaining)
                .min(Duration::from_millis(10)),
        );
    }
}

fn watch_for_following_document(
    client: &CdpClient,
    context: &AdapterContext,
    fence: u64,
    main_document: Option<MainFrameDocument>,
    cancellation: &Arc<AtomicBool>,
) -> PageResult<Option<String>> {
    let deadline = context.deadline;
    let mut watch = FollowingDocumentWatch::new(main_document);
    let mut processed = fence;
    let watch_started = Instant::now();
    loop {
        if cancellation.load(Ordering::SeqCst) {
            return Err(Box::new(unknown(
                "cancelled",
                "cancelled while watching for a following document",
            )));
        }
        if Instant::now() >= deadline {
            return Err(Box::new(unknown(
                "stabilization_timeout",
                "Chrome document was not ready before the deadline",
            )));
        }
        let snapshot = client.snapshot_since(processed);
        if snapshot.overflowed {
            return Err(Box::new(unknown(
                "observation_failed",
                "Chrome event journal overflowed while watching for a following document",
            )));
        }
        for event in &snapshot.events {
            processed = event.cursor;
            if let Some(loader) = watch.apply(event) {
                return Ok(Some(loader));
            }
        }
        let elapsed = watch_started.elapsed();
        if short_watch_closed(elapsed, watch.awaiting_commit()) {
            return Ok(None);
        }
        let remaining = if watch.awaiting_commit() {
            deadline.saturating_duration_since(Instant::now())
        } else {
            CLICK_FOLLOWING_DOCUMENT_WATCH.saturating_sub(elapsed)
        }
        .min(deadline.saturating_duration_since(Instant::now()))
        .min(Duration::from_millis(10));
        client.wait_for_journal_change(processed, remaining);
    }
}

struct MainFrameDocument {
    id: String,
    loader_id: Option<String>,
}

fn page_main_frame(
    client: &CdpClient,
    context: &AdapterContext,
    cancellation: Arc<AtomicBool>,
) -> Option<MainFrameDocument> {
    let frame = command_preflight(
        client,
        "Page.getFrameTree",
        json!({}),
        context,
        cancellation,
    )
    .ok()?
    .pointer("/frameTree/frame")
    .cloned()?;
    Some(MainFrameDocument {
        id: frame.get("id").and_then(Value::as_str)?.to_owned(),
        loader_id: frame
            .get("loaderId")
            .and_then(Value::as_str)
            .map(str::to_owned),
    })
}

struct FollowingDocumentWatch {
    main_frame: Option<String>,
    committed_loader: Option<String>,
    awaiting_frame: Option<String>,
}

impl FollowingDocumentWatch {
    fn new(main_document: Option<MainFrameDocument>) -> Self {
        Self {
            main_frame: main_document.as_ref().map(|frame| frame.id.clone()),
            committed_loader: main_document.and_then(|frame| frame.loader_id),
            awaiting_frame: None,
        }
    }

    fn apply(&mut self, event: &JournalEvent) -> Option<String> {
        let params = event_params(event);
        match event_method(event) {
            Some("Page.frameStartedLoading") => {
                let frame_id = params.get("frameId").and_then(Value::as_str)?;
                if self.is_main_frame_id(frame_id) {
                    self.awaiting_frame = Some(frame_id.to_owned());
                }
                None
            }
            Some("Page.frameNavigated") => {
                let frame = params.get("frame")?;
                if !frame_is_main_document(frame, self.main_frame.as_deref()) {
                    return None;
                }
                self.bind_new_loader(frame.get("loaderId").and_then(Value::as_str))
            }
            Some("Page.lifecycleEvent") => {
                let frame_id = params.get("frameId").and_then(Value::as_str)?;
                if !self.is_main_frame_id(frame_id) {
                    return None;
                }
                self.bind_new_loader(params.get("loaderId").and_then(Value::as_str))
            }
            _ => None,
        }
    }

    fn bind_new_loader(&mut self, loader: Option<&str>) -> Option<String> {
        let loader = loader?;
        if self.committed_loader.as_deref() == Some(loader) {
            return None;
        }
        if self.committed_loader.is_none() && self.awaiting_frame.is_none() {
            return None;
        }
        self.awaiting_frame = None;
        Some(loader.to_owned())
    }

    fn awaiting_commit(&self) -> bool {
        self.awaiting_frame.is_some()
    }

    fn is_main_frame_id(&self, frame_id: &str) -> bool {
        self.main_frame.as_deref() == Some(frame_id)
    }
}

fn short_watch_closed(elapsed: Duration, awaiting_commit: bool) -> bool {
    elapsed >= CLICK_FOLLOWING_DOCUMENT_WATCH && !awaiting_commit
}

fn navigation_event_matches(event: &JournalEvent, expected_loader: Option<&str>) -> bool {
    let Some(expected) = expected_loader else {
        return false;
    };
    event_method(event) == Some("Page.lifecycleEvent")
        && matches!(
            event_params(event).get("name").and_then(Value::as_str),
            Some("DOMContentLoaded" | "load")
        )
        && event_params(event).get("loaderId").and_then(Value::as_str) == Some(expected)
}

fn frame_is_main_document(frame: &Value, main_frame: Option<&str>) -> bool {
    let parent = frame.get("parentId").and_then(Value::as_str).unwrap_or("");
    if !parent.is_empty() {
        return false;
    }
    match (main_frame, frame.get("id").and_then(Value::as_str)) {
        (Some(expected), Some(id)) => expected == id,
        _ => true,
    }
}

struct AxTree<'a> {
    by_id: HashMap<String, &'a Value>,
    parent: HashMap<String, String>,
}

impl<'a> AxTree<'a> {
    fn index(nodes: &'a [Value]) -> Self {
        let mut by_id = HashMap::new();
        let mut parent = HashMap::new();
        for node in nodes {
            let Some(id) = ax_node_key(node, node.get("nodeId")) else {
                continue;
            };
            by_id.insert(id.clone(), node);
            if let Some(parent_id) = ax_node_key(node, node.get("parentId")) {
                parent.insert(id.clone(), parent_id);
            }
            if let Some(children) = node.get("childIds").and_then(Value::as_array) {
                for child in children {
                    if let Some(child_id) = ax_node_key(node, Some(child)) {
                        parent.insert(child_id, id.clone());
                    }
                }
            }
        }
        Self { by_id, parent }
    }

    fn parent_node(&self, node: &Value) -> Option<&'a Value> {
        let id = ax_node_key(node, node.get("nodeId"))?;
        self.by_id.get(self.parent.get(&id)?).copied()
    }
}

fn ax_node_key(node: &Value, id: Option<&Value>) -> Option<String> {
    let frame = node
        .get("computerUseFrameId")
        .and_then(Value::as_str)
        .unwrap_or("root");
    let local = id.and_then(|value| {
        value
            .as_str()
            .map(str::to_owned)
            .or_else(|| value.as_u64().map(|number| number.to_string()))
            .or_else(|| value.as_i64().map(|number| number.to_string()))
    })?;
    Some(format!("{frame}:{local}"))
}

fn ancestor_scope_matches(semantic: &Value, node: &Value, tree: &AxTree<'_>) -> bool {
    let within_role = semantic.get("within_role").and_then(Value::as_str);
    let within_name = semantic.get("within_name").and_then(Value::as_str);
    if within_role.is_none() && within_name.is_none() {
        return true;
    }
    let mut current = tree.parent_node(node);
    while let Some(ancestor) = current {
        let role = ax_value(ancestor.get("role"));
        let name = ax_value(ancestor.get("name"));
        if exact_optional_field(within_role, role.as_deref())
            && exact_optional_field(within_name, name.as_deref())
        {
            return true;
        }
        current = tree.parent_node(ancestor);
    }
    false
}

fn exact_optional_field(expected: Option<&str>, actual: Option<&str>) -> bool {
    expected.is_none_or(|wanted| actual.is_some_and(|value| normalize(value) == normalize(wanted)))
}

fn command_preflight(
    client: &CdpClient,
    method: &str,
    params: Value,
    context: &AdapterContext,
    cancellation: Arc<AtomicBool>,
) -> Result<Value, AdapterError> {
    client
        .command(method, params, context.deadline, cancellation)
        .result()
        .map_err(|failure| match failure {
            CommandFailure::Rejected(value) => error(
                "backend_error",
                &format!(
                    "Chrome preflight rejected {method}: {}",
                    bounded_error(&value)
                ),
            ),
            CommandFailure::NotSent(message) | CommandFailure::Unknown(message) => {
                error("backend_error", &message)
            }
        })
}

fn command_result(
    client: &CdpClient,
    method: &str,
    params: Value,
    context: &AdapterContext,
    cancellation: Arc<AtomicBool>,
) -> PageResult<Value> {
    command_result_after(client, method, params, context, cancellation, false)
}

fn command_result_after(
    client: &CdpClient,
    method: &str,
    params: Value,
    context: &AdapterContext,
    cancellation: Arc<AtomicBool>,
    effects_possible: bool,
) -> PageResult<Value> {
    client
        .command(method, params, context.deadline, cancellation)
        .result()
        .map_err(|failure| Box::new(command_failure_reply(method, failure, effects_possible)))
}

fn command_failure_reply(
    method: &str,
    failure: CommandFailure,
    effects_possible: bool,
) -> AdapterReply {
    if effects_possible {
        let message = match failure {
            CommandFailure::Rejected(value) => format!(
                "Chrome rejected {method} after an earlier primitive was dispatched: {}",
                bounded_error(&value)
            ),
            CommandFailure::NotSent(message) | CommandFailure::Unknown(message) => message,
        };
        unknown("transport_ambiguous", &message)
    } else {
        match failure {
            CommandFailure::Rejected(value) => rejected(
                "backend_rejected",
                &format!("Chrome rejected {method}: {}", bounded_error(&value)),
            ),
            CommandFailure::NotSent(message) => rejected("dispatch_failed", &message),
            CommandFailure::Unknown(message) => unknown("transport_ambiguous", &message),
        }
    }
}

fn outcome_reply(outcome: CommandOutcome) -> AdapterReply {
    match outcome {
        CommandOutcome::Confirmed(response) => AdapterReply::confirmed(response, None),
        CommandOutcome::Rejected(response) => rejected(
            "raw_protocol_error",
            &format!("Chrome returned a CDP error: {}", bounded_error(&response)),
        )
        .with_response(response),
        CommandOutcome::NotSent(message) => rejected("dispatch_failed", &message),
        CommandOutcome::Unknown(message) => unknown("transport_ambiguous", &message),
    }
}

trait WithResponse {
    fn with_response(self, response: Value) -> Self;
}

impl WithResponse for AdapterReply {
    fn with_response(mut self, response: Value) -> Self {
        self.response = response;
        self
    }
}

fn bounded_error(value: &Value) -> String {
    let text = value
        .get("error")
        .cloned()
        .unwrap_or_else(|| value.clone())
        .to_string();
    text.chars().take(160).collect()
}

fn prepared_target(operation: &AdapterOperation) -> PageResult<PreparedTarget> {
    let target = operation
        .prepared
        .as_ref()
        .and_then(|value| value.get("target"))
        .ok_or_else(|| Box::new(rejected("internal_error", "Chrome action was not prepared")))?;
    Ok(PreparedTarget {
        backend_id: target.get("backend_id").and_then(Value::as_u64),
        x: target.get("x").and_then(Value::as_f64).unwrap_or(0.0),
        y: target.get("y").and_then(Value::as_f64).unwrap_or(0.0),
    })
}

struct PreparedTarget {
    backend_id: Option<u64>,
    x: f64,
    y: f64,
}

struct KeyData<'a> {
    key: &'a str,
    code: &'a str,
    text: &'a str,
    virtual_code: u32,
}

fn normalized_key(key: &str) -> Option<KeyData<'_>> {
    const NAMED: [(&str, &str, &str, u32); 8] = [
        ("Enter", "Enter", "\r", 13),
        ("Tab", "Tab", "\t", 9),
        ("Escape", "Escape", "", 27),
        ("Backspace", "Backspace", "", 8),
        ("ArrowUp", "ArrowUp", "", 38),
        ("ArrowDown", "ArrowDown", "", 40),
        ("ArrowLeft", "ArrowLeft", "", 37),
        ("ArrowRight", "ArrowRight", "", 39),
    ];
    if let Some((_, code, text, virtual_code)) = NAMED.iter().find(|entry| entry.0 == key) {
        return Some(KeyData {
            key,
            code,
            text,
            virtual_code: *virtual_code,
        });
    }
    let mut characters = key.chars();
    let character = characters.next()?;
    if characters.next().is_some() {
        return None;
    }
    Some(KeyData {
        key,
        code: "",
        text: key,
        virtual_code: character as u32,
    })
}

fn ax_value(value: Option<&Value>) -> Option<String> {
    value?
        .get("value")?
        .as_str()
        .map(normalize)
        .filter(|value| !value.is_empty())
}

fn rejected(code: &str, message: &str) -> AdapterReply {
    let mut reply = AdapterReply::confirmed(Value::Null, None);
    reply.delivery = manuvra_runtime::AdapterDelivery::Rejected;
    reply.error = Some(error(code, message));
    reply
}

fn unknown(code: &str, message: &str) -> AdapterReply {
    let mut reply = AdapterReply::confirmed(Value::Null, None);
    reply.delivery = manuvra_runtime::AdapterDelivery::Unknown;
    reply.error = Some(error(code, message));
    reply
}

fn error(code: &str, message: &str) -> AdapterError {
    AdapterError {
        code: code.to_owned(),
        message: Some(message.chars().take(256).collect()),
        details: None,
    }
}

fn millis(duration: Duration) -> u64 {
    duration.as_millis().min(u128::from(u64::MAX)) as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn semantic_matching_is_exact_normalized_and_conjunctive() {
        let node = json!({"role": "button", "name": "Save changes", "text": "Save changes", "identifier": "save"});
        assert!(semantic_matches(
            &json!({"role": "button", "name": "Save   changes"}),
            &node
        ));
        assert!(!semantic_matches(
            &json!({"role": "button", "name": "Save"}),
            &node
        ));
        assert!(!semantic_matches(
            &json!({"role": "link", "name": "Save changes"}),
            &node
        ));
    }

    #[test]
    fn ancestor_scope_requires_a_proper_exact_ancestor() {
        let nodes = vec![
            json!({
                "nodeId": "region-a",
                "computerUseFrameId": "root",
                "role": {"value": "region"},
                "name": {"value": "Primary"},
                "childIds": ["button-a"]
            }),
            json!({
                "nodeId": "button-a",
                "computerUseFrameId": "root",
                "parentId": "region-a",
                "backendDOMNodeId": 11,
                "role": {"value": "button"},
                "name": {"value": "Checkout"},
                "description": {"value": "Primary region checkout"}
            }),
            json!({
                "nodeId": "region-b",
                "computerUseFrameId": "root",
                "role": {"value": "region"},
                "name": {"value": "Secondary"},
                "childIds": ["button-b"]
            }),
            json!({
                "nodeId": "button-b",
                "computerUseFrameId": "root",
                "parentId": "region-b",
                "backendDOMNodeId": 12,
                "role": {"value": "button"},
                "name": {"value": "Checkout"}
            }),
        ];
        let tree = AxTree::index(&nodes);
        let scoped = json!({"role": "button", "name": "Checkout", "within_role": "region", "within_name": "Primary"});
        assert!(ancestor_scope_matches(&scoped, &nodes[1], &tree));
        assert!(!ancestor_scope_matches(&scoped, &nodes[3], &tree));
        assert!(ancestor_scope_matches(
            &json!({"within_role": "region"}),
            &nodes[1],
            &tree
        ));
        assert!(ancestor_scope_matches(
            &json!({"within_name": "Primary"}),
            &nodes[1],
            &tree
        ));
        assert!(!ancestor_scope_matches(
            &json!({"within_name": "Primary"}),
            &nodes[3],
            &tree
        ));
        assert!(!ancestor_scope_matches(
            &json!({"within_role": "button", "within_name": "Checkout"}),
            &nodes[1],
            &tree
        ));
        let context = AdapterContext {
            session_id: "s_test".to_owned(),
            target_id: "chrome_test".to_owned(),
            target_generation: 1,
            action_sequence: 1,
            reference_namespace: "n_test".to_owned(),
            reference_epoch: 1,
            frame_token: None,
            mode: manuvra_runtime::ExecutionMode::Background,
            deadline: Instant::now() + Duration::from_secs(1),
        };
        assert_eq!(
            public_node(&context, &nodes[1]).unwrap()["description"],
            "Primary region checkout"
        );
        assert_eq!(
            public_node(&context, &nodes[3]).unwrap()["description"],
            Value::Null
        );
    }

    #[test]
    fn real_frame_started_loading_has_only_frame_id() {
        let mut watch = FollowingDocumentWatch::new(Some(MainFrameDocument {
            id: "main".to_owned(),
            loader_id: Some("loader-1".to_owned()),
        }));
        assert_eq!(
            watch.apply(&journal_event(
                "Page.frameStartedLoading",
                json!({"frameId": "main"})
            )),
            None
        );
        assert!(
            watch.awaiting_commit(),
            "real CDP load-start has no loaderId and must still open a following-document wait"
        );
        assert!(
            !short_watch_closed(Duration::from_millis(400), watch.awaiting_commit()),
            "commit can arrive after the short watch once a main-frame load has started"
        );
        assert_eq!(
            watch
                .apply(&journal_event(
                    "Page.frameNavigated",
                    json!({"frame": {"id": "main", "loaderId": "loader-2"}})
                ))
                .as_deref(),
            Some("loader-2")
        );
        assert!(!watch.awaiting_commit());
        assert!(short_watch_closed(
            Duration::from_millis(400),
            watch.awaiting_commit()
        ));
    }

    #[test]
    fn fictional_loader_id_on_frame_started_loading_is_not_a_commit() {
        let mut watch = FollowingDocumentWatch::new(Some(MainFrameDocument {
            id: "main".to_owned(),
            loader_id: Some("loader-1".to_owned()),
        }));
        assert_eq!(
            watch.apply(&journal_event(
                "Page.frameStartedLoading",
                json!({"frameId": "main", "loaderId": "loader-3"})
            )),
            None
        );
        assert!(watch.awaiting_commit());
    }

    #[test]
    fn same_document_frame_navigated_is_not_a_following_document() {
        let mut watch = FollowingDocumentWatch::new(Some(MainFrameDocument {
            id: "main".to_owned(),
            loader_id: Some("loader-1".to_owned()),
        }));
        assert_eq!(
            watch.apply(&journal_event(
                "Page.frameNavigated",
                json!({"frame": {"id": "main", "loaderId": "loader-1"}})
            )),
            None
        );
        assert!(!watch.awaiting_commit());
        assert!(short_watch_closed(
            Duration::from_millis(400),
            watch.awaiting_commit()
        ));
        assert_eq!(
            watch.apply(&journal_event(
                "Page.lifecycleEvent",
                json!({"frameId": "main", "name": "load", "loaderId": "loader-1"})
            )),
            None
        );
    }

    #[test]
    fn new_main_frame_loader_without_seen_load_start_is_still_followed() {
        let mut watch = FollowingDocumentWatch::new(Some(MainFrameDocument {
            id: "main".to_owned(),
            loader_id: Some("loader-1".to_owned()),
        }));
        assert_eq!(
            watch
                .apply(&journal_event(
                    "Page.frameNavigated",
                    json!({"frame": {"id": "main", "loaderId": "loader-2"}})
                ))
                .as_deref(),
            Some("loader-2")
        );
    }

    #[test]
    fn child_frame_and_unknown_main_frame_do_not_follow_a_document() {
        let mut watch = FollowingDocumentWatch::new(Some(MainFrameDocument {
            id: "main".to_owned(),
            loader_id: Some("loader-1".to_owned()),
        }));
        assert_eq!(
            watch.apply(&journal_event(
                "Page.frameStartedLoading",
                json!({"frameId": "child"})
            )),
            None
        );
        assert!(!watch.awaiting_commit());
        assert_eq!(
            watch.apply(&journal_event(
                "Page.frameNavigated",
                json!({"frame": {"id": "child", "parentId": "main", "loaderId": "loader-iframe"}})
            )),
            None
        );

        let mut unknown = FollowingDocumentWatch::new(None);
        assert_eq!(
            unknown.apply(&journal_event(
                "Page.frameStartedLoading",
                json!({"frameId": "main"})
            )),
            None
        );
        assert!(!unknown.awaiting_commit());
        assert_eq!(
            unknown.apply(&journal_event(
                "Page.frameNavigated",
                json!({"frame": {"id": "main", "loaderId": "loader-2"}})
            )),
            None
        );
        assert_eq!(
            unknown.apply(&journal_event(
                "Page.lifecycleEvent",
                json!({"name": "load", "loaderId": "loader-old"})
            )),
            None
        );
        assert_eq!(
            unknown.apply(&journal_event(
                "Network.requestWillBeSent",
                json!({"requestId": "req-1"})
            )),
            None
        );
    }

    #[test]
    fn new_loader_lifecycle_after_real_load_start_is_a_following_document() {
        let mut watch = FollowingDocumentWatch::new(Some(MainFrameDocument {
            id: "main".to_owned(),
            loader_id: Some("loader-1".to_owned()),
        }));
        watch.apply(&journal_event(
            "Page.frameStartedLoading",
            json!({"frameId": "main"}),
        ));
        assert_eq!(
            watch
                .apply(&journal_event(
                    "Page.lifecycleEvent",
                    json!({"frameId": "main", "name": "DOMContentLoaded", "loaderId": "loader-2"})
                ))
                .as_deref(),
            Some("loader-2")
        );
    }

    fn journal_event(method: &str, params: Value) -> JournalEvent {
        JournalEvent {
            cursor: 1,
            action_sequence: 0,
            received_ms: 0,
            message: json!({"method": method, "params": params}),
        }
    }

    #[test]
    fn frame_tree_collection_is_recursive_and_ordered() {
        let tree = json!({
            "frame": {"id": "root"},
            "childFrames": [
                {"frame": {"id": "first"}},
                {"frame": {"id": "second"}, "childFrames": [
                    {"frame": {"id": "nested"}}
                ]}
            ]
        });
        let mut frames = Vec::new();
        collect_frame_ids(&tree, &mut frames);
        assert_eq!(frames, ["root", "first", "second", "nested"]);
    }

    #[test]
    fn png_dimensions_rejects_non_png_and_reads_ihdr() {
        let mut png = vec![0; 24];
        png[..8].copy_from_slice(b"\x89PNG\r\n\x1a\n");
        png[16..20].copy_from_slice(&800_u32.to_be_bytes());
        png[20..24].copy_from_slice(&600_u32.to_be_bytes());
        assert_eq!(png_dimensions(&png), Some((800, 600)));
        assert_eq!(png_dimensions(b"not a png"), None);
    }

    #[test]
    fn key_normalization_is_bounded() {
        assert_eq!(normalized_key("Enter").unwrap().virtual_code, 13);
        assert!(normalized_key("x").is_some());
        assert!(normalized_key("UnsupportedLongKey").is_none());
    }

    #[test]
    fn failure_after_confirmed_primitive_is_never_reported_not_performed() {
        let before = command_failure_reply(
            "Input.dispatchMouseEvent",
            CommandFailure::NotSent("closed".to_owned()),
            false,
        );
        assert_eq!(before.delivery, manuvra_runtime::AdapterDelivery::Rejected);
        assert_eq!(before.error.unwrap().code, "dispatch_failed");

        for failure in [
            CommandFailure::NotSent("closed".to_owned()),
            CommandFailure::Rejected(json!({"message": "rejected"})),
            CommandFailure::Unknown("unknown".to_owned()),
        ] {
            let after = command_failure_reply("Input.dispatchMouseEvent", failure, true);
            assert_eq!(after.delivery, manuvra_runtime::AdapterDelivery::Unknown);
            assert_eq!(after.error.unwrap().code, "transport_ambiguous");
        }
    }
}
