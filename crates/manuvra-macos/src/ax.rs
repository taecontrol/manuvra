use crate::discovery::{WindowBounds, WindowRecord};
use manuvra_runtime::{AdapterContext, AdapterError, AdapterOperation, AdapterReply};
use objc2_core_foundation::{
    CFArray, CFBoolean, CFDictionary, CFNull, CFNumber, CFRange, CFRetained, CFString, CFType,
    CFURL, CGPoint, CGRect, CGSize, Type,
};
use serde_json::{Map, Value, json};
use std::collections::{HashMap, VecDeque};
use std::ffi::c_void;
use std::ptr::{self, NonNull};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::Duration;

const AX_SUCCESS: i32 = 0;
const MAX_TREE_NODES: usize = 10_000;
const MAX_RAW_DEPTH: usize = 32;
const MAX_RAW_ITEMS: usize = 1024;
const BOUNDS_TOLERANCE: f64 = 2.0;
const MAX_REFERENCE_ENTRIES: usize = MAX_TREE_NODES * 4;

#[derive(Default)]
pub(crate) struct ReferenceStore {
    issued: HashMap<String, IssuedReference>,
    order: VecDeque<String>,
    epochs: HashMap<String, (String, u64)>,
    next_id: u64,
}

struct IssuedReference {
    session_id: String,
    namespace: String,
    epoch: u64,
    element: Element,
}

impl ReferenceStore {
    pub(crate) fn close_session(&mut self, session_id: &str) {
        self.epochs.remove(session_id);
        self.issued
            .retain(|_, issued| issued.session_id != session_id);
        self.order
            .retain(|reference| self.issued.contains_key(reference));
    }

    fn begin_epoch(&mut self, context: &AdapterContext) {
        let authority = (context.reference_namespace.clone(), context.reference_epoch);
        if self.epochs.get(&context.session_id) == Some(&authority) {
            return;
        }
        self.close_session(&context.session_id);
        self.epochs.insert(context.session_id.clone(), authority);
    }

    fn issue(&mut self, context: &AdapterContext, element: &Element) -> (String, String) {
        self.begin_epoch(context);
        if let Some((reference, _)) = self.issued.iter().find(|(_, issued)| {
            issued.session_id == context.session_id
                && issued.namespace == context.reference_namespace
                && issued.epoch == context.reference_epoch
                && unsafe { CFEqual(issued.element.as_ptr(), element.as_ptr()) }
        }) {
            let backend_id = reference
                .rsplit_once('_')
                .map(|(_, backend)| backend)
                .unwrap_or_default()
                .to_owned();
            return (backend_id, reference.clone());
        }
        self.next_id = self.next_id.saturating_add(1);
        let backend_id = format!("m{:x}", self.next_id);
        let reference = element_ref(context, &backend_id);
        self.issued.insert(
            reference.clone(),
            IssuedReference {
                session_id: context.session_id.clone(),
                namespace: context.reference_namespace.clone(),
                epoch: context.reference_epoch,
                element: element.clone(),
            },
        );
        self.order.push_back(reference.clone());
        while self.issued.len() > MAX_REFERENCE_ENTRIES {
            if let Some(oldest) = self.order.pop_front() {
                self.issued.remove(&oldest);
            }
        }
        (backend_id, reference)
    }

    fn resolve(
        &self,
        context: &AdapterContext,
        value: Option<&Value>,
    ) -> Result<Element, AdapterError> {
        let reference = value
            .and_then(Value::as_str)
            .ok_or_else(|| adapter_error("invalid_request", "element ref is required"))?;
        let prefix = format!(
            "e_{}_{}_",
            context.reference_namespace, context.reference_epoch
        );
        if !reference.starts_with(&prefix) {
            return Err(adapter_error("element_stale", "element ref epoch is stale"));
        }
        self.issued
            .get(reference)
            .filter(|issued| {
                issued.session_id == context.session_id
                    && issued.namespace == context.reference_namespace
                    && issued.epoch == context.reference_epoch
            })
            .map(|issued| issued.element.clone())
            .ok_or_else(|| {
                adapter_error(
                    "element_stale",
                    "element ref was not issued for this session and epoch",
                )
            })
    }
}

pub(crate) fn invoke(
    record: &WindowRecord,
    context: &AdapterContext,
    operation: &AdapterOperation,
    cancellation: Arc<AtomicBool>,
    references: &mut ReferenceStore,
) -> AdapterReply {
    if cancellation.load(Ordering::SeqCst) {
        return rejected("cancelled", "operation was cancelled before AX dispatch");
    }
    let window = match exact_window(record) {
        Ok(window) => window,
        Err(error) => return rejected_error(error),
    };
    let result = match operation.command.as_str() {
        "observe.query" => query(&window, context, operation, cancellation, references),
        "observe.tree" => tree(&window, context, cancellation, references),
        "raw.ax.get" => raw_get(&window, context, operation, references),
        _ => Err(adapter_error(
            "capability_unavailable",
            "operation is not implemented by the AX path",
        )),
    };
    match result {
        Ok(response) => AdapterReply::confirmed(response, None),
        Err(error) => rejected_error(error),
    }
}

pub(crate) struct PreparedAx {
    element: Option<Element>,
}

pub(crate) fn prepare_mutation(
    record: &WindowRecord,
    context: &AdapterContext,
    operation: &AdapterOperation,
    references: &ReferenceStore,
) -> Result<PreparedAx, AdapterError> {
    let window = exact_window(record)?;
    let element = mutation_element(&window, context, operation, references)?;
    if let Some(element) = &element {
        validate_descendant(&window, element)?;
    }
    if context.mode.as_str() == "background" || operation.command.starts_with("raw.ax.") {
        validate_background_mutation(operation, element.as_ref())?;
    }
    if operation.command == "raw.ax.set" {
        let value = operation
            .input
            .get("value")
            .ok_or_else(|| adapter_error("invalid_request", "AX value is required"))?;
        let mut items = 0usize;
        let _ = decode_ax_value(context, references, value, 0, &mut items)?;
    }
    Ok(PreparedAx { element })
}

fn mutation_element(
    window: &Element,
    context: &AdapterContext,
    operation: &AdapterOperation,
    references: &ReferenceStore,
) -> Result<Option<Element>, AdapterError> {
    let element = match operation.command.as_str() {
        "action.click" | "action.type"
            if operation
                .input
                .get("locator")
                .and_then(|locator| locator.get("kind"))
                .and_then(Value::as_str)
                != Some("point") =>
        {
            Some(resolve_locator(window, context, operation, references)?)
        }
        "action.press" | "action.scroll" if operation.input.get("locator").is_some() => {
            if operation
                .input
                .get("locator")
                .and_then(|locator| locator.get("kind"))
                .and_then(Value::as_str)
                == Some("point")
            {
                None
            } else {
                Some(resolve_locator(window, context, operation, references)?)
            }
        }
        "raw.ax.set" | "raw.ax.perform" => {
            Some(references.resolve(context, operation.input.get("ref"))?)
        }
        _ => None,
    };
    Ok(element)
}

fn validate_background_mutation(
    operation: &AdapterOperation,
    element: Option<&Element>,
) -> Result<(), AdapterError> {
    match operation.command.as_str() {
        "action.click" => validate_background_click(operation, element),
        "action.type" => validate_background_type(operation, element),
        "raw.ax.set" => validate_background_set(operation, required_element(element)?),
        "raw.ax.perform" => validate_background_perform(operation, required_element(element)?),
        _ => Ok(()),
    }
}

fn required_element(element: Option<&Element>) -> Result<&Element, AdapterError> {
    element.ok_or_else(|| adapter_error("element_not_found", "mutation element is absent"))
}

fn validate_background_click(
    operation: &AdapterOperation,
    element: Option<&Element>,
) -> Result<(), AdapterError> {
    if locator_kind(operation) == Some("point") {
        return Err(adapter_error(
            "foreground_required",
            "point clicks require explicit foreground mode",
        ));
    }
    if operation.input.get("button").and_then(Value::as_str) != Some("left")
        || operation.input.get("count").and_then(Value::as_u64) != Some(1)
    {
        return Err(adapter_error(
            "foreground_required",
            "background click supports only one left AXPress",
        ));
    }
    require_native_capability(
        required_element(element)?.supports_action("AXPress")?,
        "AXPress",
    )
}

fn validate_background_type(
    operation: &AdapterOperation,
    element: Option<&Element>,
) -> Result<(), AdapterError> {
    if locator_kind(operation) == Some("point") {
        return Err(adapter_error(
            "foreground_required",
            "point typing requires explicit foreground mode",
        ));
    }
    if required_element(element)?.attribute_settable("AXValue")? {
        Ok(())
    } else {
        Err(adapter_error(
            "foreground_required",
            "the exact AX element does not expose settable AXValue",
        ))
    }
}

fn validate_background_set(
    operation: &AdapterOperation,
    element: &Element,
) -> Result<(), AdapterError> {
    let attribute = required_string(&operation.input, "attribute")?;
    if element.attribute_settable(attribute)? {
        Ok(())
    } else {
        Err(adapter_error(
            "capability_unavailable",
            "the exact raw AX attribute is not settable",
        ))
    }
}

fn validate_background_perform(
    operation: &AdapterOperation,
    element: &Element,
) -> Result<(), AdapterError> {
    let action = required_string(&operation.input, "action")?;
    if element.supports_action(action)? {
        Ok(())
    } else {
        Err(adapter_error(
            "capability_unavailable",
            "the exact raw AX action is not advertised",
        ))
    }
}

fn require_native_capability(available: bool, capability: &str) -> Result<(), AdapterError> {
    if available {
        Ok(())
    } else {
        Err(adapter_error(
            "foreground_required",
            &format!("the exact AX element does not advertise {capability}"),
        ))
    }
}

fn locator_kind(operation: &AdapterOperation) -> Option<&str> {
    operation
        .input
        .get("locator")
        .and_then(|locator| locator.get("kind"))
        .and_then(Value::as_str)
}

pub(crate) fn invoke_prepared(
    record: &WindowRecord,
    context: &AdapterContext,
    operation: &AdapterOperation,
    prepared: &PreparedAx,
    cancellation: Arc<AtomicBool>,
    references: &ReferenceStore,
) -> AdapterReply {
    if cancellation.load(Ordering::SeqCst) {
        return rejected("cancelled", "operation was cancelled before AX dispatch");
    }
    if context.mode.as_str() == "foreground" && operation.command.starts_with("action.") {
        return crate::foreground::invoke_prepared(
            record,
            context,
            operation,
            prepared.element.as_ref(),
            cancellation,
        );
    }
    let result = match operation.command.as_str() {
        "action.click" => prepared
            .element
            .as_ref()
            .ok_or_else(|| adapter_error("element_not_found", "prepared click element is absent"))
            .and_then(|element| element.perform("AXPress"))
            .map(|()| json!({"performed": "AXPress", "effective_mode": "background"})),
        "action.type" => prepared
            .element
            .as_ref()
            .ok_or_else(|| adapter_error("element_not_found", "prepared type element is absent"))
            .and_then(|element| type_prepared(element, operation, false)),
        "raw.ax.set" => prepared
            .element
            .as_ref()
            .ok_or_else(|| adapter_error("element_not_found", "prepared raw element is absent"))
            .and_then(|element| raw_set_prepared(element, context, operation, references)),
        "raw.ax.perform" => prepared
            .element
            .as_ref()
            .ok_or_else(|| adapter_error("element_not_found", "prepared raw element is absent"))
            .and_then(|element| raw_perform_prepared(element, operation)),
        _ => Err(adapter_error(
            "capability_unavailable",
            "prepared mutation command is unsupported",
        )),
    };
    match result {
        Ok(response) => AdapterReply::confirmed(response, None),
        Err(error) => rejected_error(error),
    }
}

fn type_prepared(
    element: &Element,
    operation: &AdapterOperation,
    collapse_selection: bool,
) -> Result<Value, AdapterError> {
    let requested = required_string(&operation.input, "text")?;
    let replace = operation
        .input
        .get("replace")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let text = if replace {
        requested.to_owned()
    } else {
        let current = element
            .attribute("AXValue")
            .ok()
            .and_then(|value| value.downcast::<CFString>().ok())
            .map(|value| value.to_string())
            .unwrap_or_default();
        format!("{current}{requested}")
    };
    crate::oracle::record(
        "ax_value_set",
        json!({"replace": replace, "characters": requested.chars().count()}),
    );
    element.set_string("AXValue", &text)?;
    if collapse_selection
        && let Some((text_end, attempts)) = collapse_selection_at_text_end(element, &text)?
    {
        crate::oracle::record(
            "ax_selection_collapsed",
            json!({"utf16_location": text_end.location, "attempts": attempts}),
        );
    }
    Ok(json!({"characters": requested.chars().count(), "effective_mode": "background"}))
}

fn text_end_range(text: &str) -> CFRange {
    CFRange {
        location: text.encode_utf16().count() as _,
        length: 0,
    }
}

fn collapse_selection_at_text_end(
    element: &Element,
    text: &str,
) -> Result<Option<(CFRange, usize)>, AdapterError> {
    if !element.attribute_settable("AXSelectedTextRange")? {
        return Ok(None);
    }
    let expected = text_end_range(text);
    let range = ax_struct(4, &expected)?;
    for attempt in 1..=5 {
        thread::park_timeout(Duration::from_millis(10));
        element.set_cf(
            "AXSelectedTextRange",
            CFRetained::as_ptr(&range).as_ptr().cast(),
        )?;
        thread::park_timeout(Duration::from_millis(10));
        let observed = selected_text_range(element)?;
        if observed.location == expected.location && observed.length == expected.length {
            return Ok(Some((expected, attempt)));
        }
    }
    Err(adapter_error(
        "dispatch_failed",
        "AXSelectedTextRange did not retain the requested insertion point",
    ))
}

fn selected_text_range(element: &Element) -> Result<CFRange, AdapterError> {
    let value = element.attribute("AXSelectedTextRange")?;
    let mut range = CFRange {
        location: 0,
        length: 0,
    };
    ax_value_into(CFRetained::as_ptr(&value).as_ptr().cast(), 4, &mut range)?;
    Ok(range)
}

pub(crate) fn type_if_settable(
    element: &Element,
    operation: &AdapterOperation,
) -> Result<Option<Value>, AdapterError> {
    let settable = element.attribute_settable("AXValue")?;
    crate::oracle::record("ax_value_settable", json!({"settable": settable}));
    settable
        .then(|| type_prepared(element, operation, true))
        .transpose()
}

fn raw_set_prepared(
    element: &Element,
    context: &AdapterContext,
    operation: &AdapterOperation,
    references: &ReferenceStore,
) -> Result<Value, AdapterError> {
    let attribute = required_string(&operation.input, "attribute")?;
    let value = operation
        .input
        .get("value")
        .ok_or_else(|| adapter_error("invalid_request", "AX value is required"))?;
    let mut items = 0usize;
    let value = decode_ax_value(context, references, value, 0, &mut items)?;
    element.set_cf(attribute, CFRetained::as_ptr(&value).as_ptr().cast())?;
    Ok(json!({"attribute": attribute, "set": true}))
}

fn raw_perform_prepared(
    element: &Element,
    operation: &AdapterOperation,
) -> Result<Value, AdapterError> {
    let action = required_string(&operation.input, "action")?;
    element.perform(action)?;
    Ok(json!({"action": action, "performed": true}))
}

pub(crate) fn exact_window(record: &WindowRecord) -> Result<Element, AdapterError> {
    let application = Element::application(record.snapshot.pid)?;
    let candidates = application_windows(&application)
        .into_iter()
        .filter_map(|window| {
            let bounds = window.bounds().ok()?;
            Some((window, bounds))
        })
        .collect::<Vec<_>>();
    crate::oracle::record(
        "ax_window_resolution",
        json!({
            "expected_bounds": record.snapshot.bounds,
            "candidate_bounds": candidates.iter().map(|(_, bounds)| bounds).collect::<Vec<_>>(),
        }),
    );
    let matches = candidates
        .iter()
        .filter(|(_, bounds)| close_enough(*bounds, record.snapshot.bounds))
        .collect::<Vec<_>>();
    match matches.len() {
        1 => Ok(matches[0].0.clone()),
        0 => Err(adapter_error(
            "target_not_found",
            &format!(
                "no AX window matches pinned bounds {:?}; AX candidates are {:?}",
                record.snapshot.bounds,
                candidates
                    .iter()
                    .map(|(_, bounds)| *bounds)
                    .collect::<Vec<_>>()
            ),
        )),
        _ => Err(adapter_error(
            "capability_unavailable",
            "more than one AX window matches the pinned WindowServer bounds",
        )),
    }
}

pub(crate) fn focus_exact_window(record: &WindowRecord) -> Result<(), AdapterError> {
    let application = Element::application(record.snapshot.pid)?;
    let window = exact_window(record)?;
    window.perform("AXRaise")?;
    window.set_bool("AXMain", true)?;
    application.set_element("AXFocusedWindow", &window)
}

pub(crate) fn application_window_bounds(pid: i32) -> Result<Vec<WindowBounds>, AdapterError> {
    let application = Element::application(pid)?;
    Ok(application_windows(&application)
        .into_iter()
        .filter_map(|window| window.bounds().ok())
        .collect())
}

#[cfg(debug_assertions)]
pub(crate) fn focused_window_snapshot() -> Value {
    focused_window_snapshot_result().unwrap_or_else(|error| {
        json!({
            "available": false,
            "error_code": error.code,
        })
    })
}

#[cfg(debug_assertions)]
fn focused_window_snapshot_result() -> Result<Value, AdapterError> {
    let raw = unsafe { AXUIElementCreateSystemWide() };
    let system = NonNull::new(raw.cast_mut())
        .map(Element)
        .ok_or_else(|| adapter_error("observation_failed", "AX system-wide element was null"))?;
    let focused_application = element_attribute(&system, "AXFocusedApplication")?;
    let mut pid = 0;
    let status = unsafe { AXUIElementGetPid(focused_application.as_ptr(), &mut pid) };
    if status != AX_SUCCESS {
        return Err(ax_error(
            status,
            "read focused application pid",
            "AXFocusedApplication".to_owned(),
        ));
    }
    let focused_window = element_attribute(&focused_application, "AXFocusedWindow")?;
    let bounds = focused_window.bounds().ok();
    Ok(json!({
        "available": true,
        "application_pid": pid,
        "window_title": focused_window.string("AXTitle"),
        "window_bounds": bounds,
    }))
}

fn element_attribute(element: &Element, name: &str) -> Result<Element, AdapterError> {
    let value = element.attribute(name)?;
    if unsafe { CFGetTypeID(Some(&*value)) } != unsafe { AXUIElementGetTypeID() } {
        return Err(adapter_error(
            "observation_failed",
            "focused AX attribute was not an element",
        ));
    }
    Ok(Element(CFRetained::into_raw(value).cast()))
}

fn application_windows(application: &Element) -> Vec<Element> {
    let mut windows = Vec::new();
    for attribute in ["AXWindows", "AXChildren"] {
        for candidate in application.elements(attribute).unwrap_or_default() {
            let role = candidate.string("AXRole");
            let is_window = matches!(role.as_deref(), Some("AXWindow" | "AXDialog" | "AXSheet"));
            let duplicate = windows
                .iter()
                .any(|window: &Element| unsafe { CFEqual(window.as_ptr(), candidate.as_ptr()) });
            if is_window && !duplicate {
                windows.push(candidate);
            }
        }
    }
    windows
}

pub(crate) fn same_bounds(left: WindowBounds, right: WindowBounds) -> bool {
    close_enough(left, right)
}

pub(crate) fn observer_elements(record: &WindowRecord) -> Result<Vec<Element>, AdapterError> {
    let root = exact_window(record)?;
    let mut elements = vec![Element::application(record.snapshot.pid)?];
    let mut stack = vec![root];
    while let Some(element) = stack.pop() {
        if elements.len() >= MAX_TREE_NODES {
            return Err(adapter_error(
                "observation_failed",
                "AX observer element set exceeded the node bound",
            ));
        }
        stack.extend(element.elements("AXChildren").unwrap_or_default());
        elements.push(element);
    }
    Ok(elements)
}

pub(crate) fn exact_window_is_main(record: &WindowRecord) -> Result<bool, AdapterError> {
    let value = exact_window(record)?.attribute("AXMain")?;
    value
        .downcast::<CFBoolean>()
        .ok()
        .map(|value| value.as_bool())
        .ok_or_else(|| adapter_error("observation_failed", "AXMain was not a boolean"))
}

pub(crate) fn exact_window_is_focused(record: &WindowRecord) -> Result<bool, AdapterError> {
    let application = Element::application(record.snapshot.pid)?;
    let focused = element_attribute(&application, "AXFocusedWindow")?;
    let exact = exact_window(record)?;
    Ok(unsafe { CFEqual(focused.as_ptr(), exact.as_ptr()) })
}

pub(crate) fn exact_window_is_minimized(record: &WindowRecord) -> Result<bool, AdapterError> {
    let value = exact_window(record)?.attribute("AXMinimized")?;
    value
        .downcast::<CFBoolean>()
        .ok()
        .map(|value| value.as_bool())
        .ok_or_else(|| adapter_error("observation_failed", "AXMinimized was not a boolean"))
}

pub(crate) fn application_is_frontmost(pid: i32) -> Result<bool, AdapterError> {
    let value = Element::application(pid)?.attribute("AXFrontmost")?;
    value
        .downcast::<CFBoolean>()
        .ok()
        .map(|value| value.as_bool())
        .ok_or_else(|| adapter_error("observation_failed", "AXFrontmost was not a boolean"))
}

pub(crate) fn application_is_hidden(pid: i32) -> Result<bool, AdapterError> {
    let value = Element::application(pid)?.attribute("AXHidden")?;
    value
        .downcast::<CFBoolean>()
        .ok()
        .map(|value| value.as_bool())
        .ok_or_else(|| adapter_error("observation_failed", "AXHidden was not a boolean"))
}

fn close_enough(left: WindowBounds, right: WindowBounds) -> bool {
    (left.x - right.x).abs() <= BOUNDS_TOLERANCE
        && (left.y - right.y).abs() <= BOUNDS_TOLERANCE
        && (left.width - right.width).abs() <= BOUNDS_TOLERANCE
        && (left.height - right.height).abs() <= BOUNDS_TOLERANCE
}

fn query(
    window: &Element,
    context: &AdapterContext,
    operation: &AdapterOperation,
    cancellation: Arc<AtomicBool>,
    references: &mut ReferenceStore,
) -> Result<Value, AdapterError> {
    let semantic = operation
        .input
        .get("semantic")
        .and_then(Value::as_object)
        .ok_or_else(|| adapter_error("invalid_request", "semantic locator is required"))?;
    let limit = operation
        .input
        .get("limit")
        .and_then(Value::as_u64)
        .unwrap_or(5)
        .clamp(1, 5) as usize;
    let mut matches = Vec::new();
    walk(window, cancellation, |_, element| {
        if semantic_matches(element, semantic) {
            let (backend_id, _) = references.issue(context, element);
            matches.push(match_entry(&backend_id, element));
        }
        true
    })?;
    let overflow = if matches.len() > limit {
        matches.split_off(limit)
    } else {
        Vec::new()
    };
    Ok(json!({"matches": matches, "overflow": overflow}))
}

fn tree(
    window: &Element,
    context: &AdapterContext,
    cancellation: Arc<AtomicBool>,
    references: &mut ReferenceStore,
) -> Result<Value, AdapterError> {
    let mut nodes = Vec::new();
    walk(window, cancellation, |_, element| {
        let (backend_id, reference) = references.issue(context, element);
        let mut node = public_element(element);
        node.insert("ref".to_owned(), json!(reference));
        node.insert("backend_id".to_owned(), json!(backend_id));
        nodes.push(Value::Object(node));
        nodes.len() < MAX_TREE_NODES
    })?;
    if nodes.len() >= MAX_TREE_NODES {
        return Err(adapter_error(
            "observation_failed",
            "AX tree exceeded the complete-tree node bound",
        ));
    }
    let node_count = nodes.len();
    Ok(json!({
        "tree": {
            "complete": true,
            "target_id": context.target_id,
            "nodes": nodes,
        },
        "node_count": node_count,
    }))
}

fn raw_get(
    window: &Element,
    context: &AdapterContext,
    operation: &AdapterOperation,
    references: &mut ReferenceStore,
) -> Result<Value, AdapterError> {
    let element = references.resolve(context, operation.input.get("ref"))?;
    let attribute = required_string(&operation.input, "attribute")?;
    let value = element.attribute(attribute)?;
    let mut items = 0usize;
    Ok(json!({
        "attribute": attribute,
        "value": encode_ax_value(window, context, references, &value, 0, &mut items)?,
    }))
}

fn resolve_locator(
    window: &Element,
    context: &AdapterContext,
    operation: &AdapterOperation,
    references: &ReferenceStore,
) -> Result<Element, AdapterError> {
    let locator = operation
        .input
        .get("locator")
        .and_then(Value::as_object)
        .ok_or_else(|| adapter_error("invalid_request", "AX locator is required"))?;
    match locator.get("kind").and_then(Value::as_str) {
        Some("ref") => references.resolve(context, locator.get("ref")),
        Some("semantic") => {
            let mut found = Vec::new();
            walk(window, Arc::new(AtomicBool::new(false)), |_, element| {
                if semantic_matches(element, locator) {
                    found.push(element.clone());
                }
                found.len() < 2
            })?;
            match found.len() {
                1 => Ok(found.remove(0)),
                0 => Err(adapter_error(
                    "element_not_found",
                    "AX locator matched no element",
                )),
                _ => Err(adapter_error(
                    "ambiguous_target",
                    "AX locator matched more than one element",
                )),
            }
        }
        Some("point") => Err(adapter_error(
            "foreground_required",
            "point locators require the foreground CGEvent path",
        )),
        _ => Err(adapter_error("invalid_request", "unknown AX locator kind")),
    }
}

fn walk(
    root: &Element,
    cancellation: Arc<AtomicBool>,
    mut visit: impl FnMut(&str, &Element) -> bool,
) -> Result<(), AdapterError> {
    let mut stack = vec![("0".to_owned(), root.clone())];
    let mut visited = 0usize;
    while let Some((path, element)) = stack.pop() {
        if cancellation.load(Ordering::SeqCst) {
            return Err(adapter_error("cancelled", "AX traversal was cancelled"));
        }
        visited += 1;
        if visited > MAX_TREE_NODES || !visit(&path, &element) {
            break;
        }
        let children = element.elements("AXChildren").unwrap_or_default();
        for (index, child) in children.into_iter().enumerate().rev() {
            stack.push((format!("{path}-{index}"), child));
        }
    }
    Ok(())
}

fn validate_descendant(window: &Element, requested: &Element) -> Result<(), AdapterError> {
    let mut found = false;
    walk(window, Arc::new(AtomicBool::new(false)), |_, element| {
        found = unsafe { CFEqual(element.as_ptr(), requested.as_ptr()) };
        !found
    })?;
    found.then_some(()).ok_or_else(|| {
        adapter_error(
            "element_stale",
            "retained AX element is no longer in the pinned exact-window tree",
        )
    })
}

fn semantic_matches(element: &Element, semantic: &Map<String, Value>) -> bool {
    requested_field_matches(semantic, "role", || {
        element.string("AXRole").map(|role| public_role(&role))
    }) && requested_field_matches(semantic, "name", || {
        element
            .string("AXTitle")
            .or_else(|| element.string("AXDescription"))
    }) && requested_field_matches(semantic, "text", || element.string("AXValue"))
        && requested_field_matches(semantic, "identifier", || element.string("AXIdentifier"))
}

fn requested_field_matches(
    semantic: &Map<String, Value>,
    field: &str,
    read: impl FnOnce() -> Option<String>,
) -> bool {
    let Some(expected) = semantic.get(field).and_then(Value::as_str) else {
        return true;
    };
    read().is_some_and(|actual| actual == expected)
}

fn match_entry(path: &str, element: &Element) -> Value {
    let public = public_element(element);
    json!({
        "backend_id": path,
        "role": public.get("role"),
        "name": public.get("name"),
        "text": public.get("text"),
        "identifier": public.get("identifier"),
    })
}

fn public_element(element: &Element) -> Map<String, Value> {
    let mut value = Map::new();
    value.insert(
        "role".to_owned(),
        element
            .string("AXRole")
            .map(|role| json!(public_role(&role)))
            .unwrap_or(Value::Null),
    );
    value.insert(
        "name".to_owned(),
        element
            .string("AXTitle")
            .or_else(|| element.string("AXDescription"))
            .map(Value::String)
            .unwrap_or(Value::Null),
    );
    value.insert(
        "text".to_owned(),
        element
            .string("AXValue")
            .map(Value::String)
            .unwrap_or(Value::Null),
    );
    value.insert(
        "identifier".to_owned(),
        element
            .string("AXIdentifier")
            .map(Value::String)
            .unwrap_or(Value::Null),
    );
    if let Ok(bounds) = element.bounds() {
        value.insert("bounds".to_owned(), json!(bounds));
    }
    value
}

fn public_role(role: &str) -> String {
    match role {
        "AXButton" | "AXMenuButton" => "button",
        "AXTextField" | "AXTextArea" | "AXComboBox" => "textbox",
        "AXStaticText" => "text",
        "AXCheckBox" => "checkbox",
        "AXRadioButton" => "radio",
        "AXWindow" => "window",
        "AXMenuItem" => "menuitem",
        "AXLink" => "link",
        _ => role.strip_prefix("AX").unwrap_or(role),
    }
    .to_ascii_lowercase()
}

fn element_ref(context: &AdapterContext, backend_id: &str) -> String {
    format!(
        "e_{}_{}_{}",
        context.reference_namespace, context.reference_epoch, backend_id
    )
}

fn required_string<'a>(input: &'a Value, field: &str) -> Result<&'a str, AdapterError> {
    input
        .get(field)
        .and_then(Value::as_str)
        .ok_or_else(|| adapter_error("invalid_request", &format!("{field} is required")))
}

fn encode_ax_value(
    window: &Element,
    context: &AdapterContext,
    references: &mut ReferenceStore,
    value: &CFRetained<CFType>,
    depth: usize,
    items: &mut usize,
) -> Result<Value, AdapterError> {
    raw_item(depth, items)?;
    if let Some(encoded) = encode_ax_scalar(value)? {
        return Ok(encoded);
    }
    if let Some(encoded) = encode_ax_collection(window, context, references, value, depth, items)? {
        return Ok(encoded);
    }
    encode_ax_native(window, context, references, value)
}

fn encode_ax_scalar(value: &CFRetained<CFType>) -> Result<Option<Value>, AdapterError> {
    if value.downcast_ref::<CFNull>().is_some() {
        return Ok(Some(json!({"type": "null", "value": null})));
    }
    if let Some(value) = value.downcast_ref::<CFString>() {
        return Ok(Some(json!({"type": "string", "value": value.to_string()})));
    }
    if let Some(value) = value.downcast_ref::<CFURL>() {
        return Ok(Some(
            json!({"type": "url", "value": value.string().to_string()}),
        ));
    }
    if let Some(value) = value.downcast_ref::<CFBoolean>() {
        return Ok(Some(json!({"type": "boolean", "value": value.as_bool()})));
    }
    if let Some(value) = value.downcast_ref::<CFNumber>() {
        let number = value
            .as_i64()
            .map(Value::from)
            .or_else(|| value.as_f64().map(Value::from))
            .ok_or_else(|| adapter_error("raw_value_unsupported", "CFNumber had no JSON value"))?;
        return Ok(Some(json!({"type": "number", "value": number})));
    }
    Ok(None)
}

fn encode_ax_collection(
    window: &Element,
    context: &AdapterContext,
    references: &mut ReferenceStore,
    value: &CFRetained<CFType>,
    depth: usize,
    items: &mut usize,
) -> Result<Option<Value>, AdapterError> {
    if let Some(value) = value.downcast_ref::<CFArray>() {
        return encode_ax_array(window, context, references, value, depth, items).map(Some);
    }
    if let Some(value) = value.downcast_ref::<CFDictionary>() {
        return encode_ax_dictionary(window, context, references, value, depth, items).map(Some);
    }
    Ok(None)
}

fn encode_ax_array(
    window: &Element,
    context: &AdapterContext,
    references: &mut ReferenceStore,
    value: &CFArray,
    depth: usize,
    items: &mut usize,
) -> Result<Value, AdapterError> {
    // SAFETY: Accessibility arrays contain CoreFoundation objects and are immutable here.
    let value: &CFArray<CFType> = unsafe { &*(value as *const CFArray).cast() };
    let entries = value
        .to_vec()
        .iter()
        .map(|entry| encode_ax_value(window, context, references, entry, depth + 1, items))
        .collect::<Result<Vec<_>, _>>()?;
    Ok(json!({"type": "array", "value": entries}))
}

fn encode_ax_dictionary(
    window: &Element,
    context: &AdapterContext,
    references: &mut ReferenceStore,
    value: &CFDictionary,
    depth: usize,
    items: &mut usize,
) -> Result<Value, AdapterError> {
    // SAFETY: CoreFoundation dictionaries retain opaque CF object keys and values.
    let value: &CFDictionary<CFType, CFType> = unsafe { &*(value as *const CFDictionary).cast() };
    let (keys, values) = value.to_vecs();
    let mut entries = Map::new();
    for (key, value) in keys.iter().zip(&values) {
        let (key, value) =
            encode_ax_dictionary_entry(window, context, references, key, value, depth, items)?;
        entries.insert(key, value);
    }
    Ok(json!({"type": "dictionary", "value": entries}))
}

fn encode_ax_dictionary_entry(
    window: &Element,
    context: &AdapterContext,
    references: &mut ReferenceStore,
    key: &CFRetained<CFType>,
    value: &CFRetained<CFType>,
    depth: usize,
    items: &mut usize,
) -> Result<(String, Value), AdapterError> {
    let key = key.downcast_ref::<CFString>().ok_or_else(|| {
        adapter_error(
            "raw_value_unsupported",
            "raw AX dictionary contained a non-string key",
        )
    })?;
    let value = encode_ax_value(window, context, references, value, depth + 1, items)?;
    Ok((key.to_string(), value))
}

fn encode_ax_native(
    window: &Element,
    context: &AdapterContext,
    references: &mut ReferenceStore,
    value: &CFRetained<CFType>,
) -> Result<Value, AdapterError> {
    let type_id = unsafe { CFGetTypeID(Some(value)) };
    if type_id == unsafe { AXUIElementGetTypeID() } {
        return encode_ax_element(window, context, references, value);
    }
    encode_ax_struct_type(value, type_id)
}

fn encode_ax_element(
    window: &Element,
    context: &AdapterContext,
    references: &mut ReferenceStore,
    value: &CFRetained<CFType>,
) -> Result<Value, AdapterError> {
    let element = element_for_native(window, CFRetained::as_ptr(value).as_ptr())?;
    let (_, reference) = references.issue(context, &element);
    Ok(json!({"type": "element", "ref": reference}))
}

fn encode_ax_struct_type(
    value: &CFRetained<CFType>,
    type_id: usize,
) -> Result<Value, AdapterError> {
    if type_id == unsafe { AXValueGetTypeID() } {
        return encode_ax_struct(value, unsafe {
            AXValueGetType(CFRetained::as_ptr(value).as_ptr().cast())
        });
    }
    Err(adapter_error(
        "raw_value_unsupported",
        &format!("unsupported native CF type id {type_id}"),
    ))
}

fn encode_ax_struct(value: &CFRetained<CFType>, kind: i32) -> Result<Value, AdapterError> {
    let pointer = CFRetained::as_ptr(value).as_ptr().cast();
    match kind {
        1 => {
            let mut point = CGPoint::ZERO;
            ax_value_into(pointer, kind, &mut point)?;
            Ok(json!({"type": "point", "x": point.x, "y": point.y}))
        }
        2 => {
            let mut size = CGSize::ZERO;
            ax_value_into(pointer, kind, &mut size)?;
            Ok(json!({"type": "size", "width": size.width, "height": size.height}))
        }
        3 => {
            let mut rect = CGRect::ZERO;
            ax_value_into(pointer, kind, &mut rect)?;
            Ok(json!({
                "type": "rect", "x": rect.origin.x, "y": rect.origin.y,
                "width": rect.size.width, "height": rect.size.height,
            }))
        }
        4 => {
            let mut range = CFRange {
                location: 0,
                length: 0,
            };
            ax_value_into(pointer, kind, &mut range)?;
            let location = u64::try_from(range.location).map_err(|_| {
                adapter_error("raw_value_unsupported", "AX range location was negative")
            })?;
            let length = u64::try_from(range.length).map_err(|_| {
                adapter_error("raw_value_unsupported", "AX range length was negative")
            })?;
            Ok(json!({"type": "range", "location": location, "length": length}))
        }
        _ => Err(adapter_error(
            "raw_value_unsupported",
            &format!("unsupported AXValue type {kind}"),
        )),
    }
}

fn ax_value_into<T>(pointer: *const c_void, kind: i32, value: &mut T) -> Result<(), AdapterError> {
    unsafe { AXValueGetValue(pointer, kind, (value as *mut T).cast()) }
        .then_some(())
        .ok_or_else(|| adapter_error("raw_value_unsupported", "AXValue payload was malformed"))
}

fn decode_ax_value(
    context: &AdapterContext,
    references: &ReferenceStore,
    value: &Value,
    depth: usize,
    items: &mut usize,
) -> Result<CFRetained<CFType>, AdapterError> {
    raw_item(depth, items)?;
    let kind = tagged_ax_kind(value)?;
    decode_known_ax_value(context, references, kind, value, depth, items)?.ok_or_else(|| {
        adapter_error(
            "raw_value_unsupported",
            &format!("unsupported tagged AX value type {kind}"),
        )
    })
}

fn tagged_ax_kind(value: &Value) -> Result<&str, AdapterError> {
    value
        .get("type")
        .and_then(Value::as_str)
        .ok_or_else(|| adapter_error("invalid_request", "tagged AX value type is required"))
}

fn decode_known_ax_value(
    context: &AdapterContext,
    references: &ReferenceStore,
    kind: &str,
    value: &Value,
    depth: usize,
    items: &mut usize,
) -> Result<Option<CFRetained<CFType>>, AdapterError> {
    if let Some(decoded) = decode_ax_scalar(kind, value)? {
        return Ok(Some(decoded));
    }
    if let Some(decoded) = decode_ax_struct(kind, value)? {
        return Ok(Some(decoded));
    }
    if let Some(decoded) = decode_ax_reference(context, references, kind, value)? {
        return Ok(Some(decoded));
    }
    decode_ax_collection(context, references, kind, value, depth, items)
}

fn decode_ax_scalar(kind: &str, value: &Value) -> Result<Option<CFRetained<CFType>>, AdapterError> {
    let decoded = match kind {
        "null" => decode_ax_null()?,
        "boolean" => decode_ax_boolean(value)?,
        "number" => decode_ax_number(value)?,
        "string" => decode_ax_string(value)?,
        "url" => decode_ax_url(value)?,
        _ => return Ok(None),
    };
    Ok(Some(decoded))
}

fn decode_ax_null() -> Result<CFRetained<CFType>, AdapterError> {
    retain_type(unsafe { objc2_core_foundation::kCFNull }.ok_or_else(|| {
        adapter_error(
            "raw_value_unsupported",
            "CoreFoundation null is unavailable",
        )
    })?)
}

fn decode_ax_boolean(value: &Value) -> Result<CFRetained<CFType>, AdapterError> {
    retain_type(CFBoolean::new(required_bool(value, "value")?))
}

fn decode_ax_string(value: &Value) -> Result<CFRetained<CFType>, AdapterError> {
    Ok(into_type(CFString::from_str(required_string(
        value, "value",
    )?)))
}

fn decode_ax_number(value: &Value) -> Result<CFRetained<CFType>, AdapterError> {
    value
        .get("value")
        .and_then(Value::as_f64)
        .map(CFNumber::new_f64)
        .map(into_type)
        .ok_or_else(|| adapter_error("invalid_request", "number value is required"))
}

fn decode_ax_url(value: &Value) -> Result<CFRetained<CFType>, AdapterError> {
    let string = CFString::from_str(required_string(value, "value")?);
    CFURL::from_string(None, &string, None)
        .map(into_type)
        .ok_or_else(|| adapter_error("raw_value_unsupported", "URL value was invalid"))
}

fn decode_ax_struct(kind: &str, value: &Value) -> Result<Option<CFRetained<CFType>>, AdapterError> {
    let decoded = match kind {
        "point" => decode_ax_point(value)?,
        "size" => decode_ax_size(value)?,
        "rect" => decode_ax_rect(value)?,
        "range" => decode_ax_range(value)?,
        _ => return Ok(None),
    };
    Ok(Some(decoded))
}

fn decode_ax_point(value: &Value) -> Result<CFRetained<CFType>, AdapterError> {
    ax_struct(
        1,
        &CGPoint::new(required_number(value, "x")?, required_number(value, "y")?),
    )
}

fn decode_ax_size(value: &Value) -> Result<CFRetained<CFType>, AdapterError> {
    ax_struct(
        2,
        &CGSize::new(
            required_number(value, "width")?,
            required_number(value, "height")?,
        ),
    )
}

fn decode_ax_rect(value: &Value) -> Result<CFRetained<CFType>, AdapterError> {
    ax_struct(
        3,
        &CGRect::new(
            CGPoint::new(required_number(value, "x")?, required_number(value, "y")?),
            CGSize::new(
                required_number(value, "width")?,
                required_number(value, "height")?,
            ),
        ),
    )
}

fn decode_ax_range(value: &Value) -> Result<CFRetained<CFType>, AdapterError> {
    let location = required_u64(value, "location")?
        .try_into()
        .map_err(|_| adapter_error("raw_value_unsupported", "range location exceeded CFIndex"))?;
    let length = required_u64(value, "length")?
        .try_into()
        .map_err(|_| adapter_error("raw_value_unsupported", "range length exceeded CFIndex"))?;
    ax_struct(4, &CFRange { location, length })
}

fn decode_ax_reference(
    context: &AdapterContext,
    references: &ReferenceStore,
    kind: &str,
    value: &Value,
) -> Result<Option<CFRetained<CFType>>, AdapterError> {
    if kind != "element" {
        return Ok(None);
    }
    let reference = value
        .get("ref")
        .ok_or_else(|| adapter_error("invalid_request", "element ref is required"))?;
    element_at_ref_as_type(context, references, reference).map(Some)
}

fn decode_ax_collection(
    context: &AdapterContext,
    references: &ReferenceStore,
    kind: &str,
    value: &Value,
    depth: usize,
    items: &mut usize,
) -> Result<Option<CFRetained<CFType>>, AdapterError> {
    if kind == "array" {
        return decode_ax_array(context, references, value, depth, items).map(Some);
    }
    if kind == "dictionary" {
        return decode_ax_dictionary(context, references, value, depth, items).map(Some);
    }
    Ok(None)
}

fn decode_ax_array(
    context: &AdapterContext,
    references: &ReferenceStore,
    value: &Value,
    depth: usize,
    items: &mut usize,
) -> Result<CFRetained<CFType>, AdapterError> {
    let values = value
        .get("value")
        .and_then(Value::as_array)
        .ok_or_else(|| adapter_error("invalid_request", "array value is required"))?;
    let values = values
        .iter()
        .map(|value| decode_ax_value(context, references, value, depth + 1, items))
        .collect::<Result<Vec<_>, _>>()?;
    Ok(into_type(CFArray::from_retained_objects(&values)))
}

fn decode_ax_dictionary(
    context: &AdapterContext,
    references: &ReferenceStore,
    value: &Value,
    depth: usize,
    items: &mut usize,
) -> Result<CFRetained<CFType>, AdapterError> {
    let entries = value
        .get("value")
        .and_then(Value::as_object)
        .ok_or_else(|| adapter_error("invalid_request", "dictionary value is required"))?;
    let keys = entries
        .keys()
        .map(|key| CFString::from_str(key))
        .collect::<Vec<_>>();
    let values = entries
        .values()
        .map(|value| decode_ax_value(context, references, value, depth + 1, items))
        .collect::<Result<Vec<_>, _>>()?;
    let key_refs = keys.iter().map(|key| &**key).collect::<Vec<_>>();
    let value_refs = values.iter().map(|value| &**value).collect::<Vec<_>>();
    Ok(into_type(CFDictionary::from_slices(&key_refs, &value_refs)))
}

fn raw_item(depth: usize, items: &mut usize) -> Result<(), AdapterError> {
    if depth > MAX_RAW_DEPTH || *items >= MAX_RAW_ITEMS {
        return Err(adapter_error(
            "raw_value_unsupported",
            "raw AX value exceeded its depth or item bound",
        ));
    }
    *items += 1;
    Ok(())
}

fn required_number(value: &Value, field: &str) -> Result<f64, AdapterError> {
    value
        .get(field)
        .and_then(Value::as_f64)
        .ok_or_else(|| adapter_error("invalid_request", &format!("{field} is required")))
}

fn required_u64(value: &Value, field: &str) -> Result<u64, AdapterError> {
    value
        .get(field)
        .and_then(Value::as_u64)
        .ok_or_else(|| adapter_error("invalid_request", &format!("{field} is required")))
}

fn required_bool(value: &Value, field: &str) -> Result<bool, AdapterError> {
    value
        .get(field)
        .and_then(Value::as_bool)
        .ok_or_else(|| adapter_error("invalid_request", &format!("{field} is required")))
}

fn into_type<T: Type>(value: CFRetained<T>) -> CFRetained<CFType> {
    // SAFETY: every caller supplies a concrete CoreFoundation type.
    unsafe { CFRetained::cast_unchecked(value) }
}

fn retain_type<T: Type>(value: &T) -> Result<CFRetained<CFType>, AdapterError> {
    let pointer = NonNull::from(value);
    // SAFETY: the supplied singleton is a live CoreFoundation object.
    Ok(into_type(unsafe { CFRetained::retain(pointer) }))
}

fn ax_struct<T>(kind: i32, value: &T) -> Result<CFRetained<CFType>, AdapterError> {
    let raw = unsafe { AXValueCreate(kind, (value as *const T).cast()) };
    let raw = NonNull::new(raw.cast_mut().cast::<CFType>()).ok_or_else(|| {
        adapter_error(
            "raw_value_unsupported",
            "AXValueCreate rejected the tagged value",
        )
    })?;
    // SAFETY: AXValueCreate returned a +1 retained CoreFoundation object.
    Ok(unsafe { CFRetained::from_raw(raw) })
}

fn element_at_ref_as_type(
    context: &AdapterContext,
    references: &ReferenceStore,
    reference: &Value,
) -> Result<CFRetained<CFType>, AdapterError> {
    let element = references.resolve(context, Some(reference))?;
    unsafe { CFRetain(element.as_ptr()) };
    let raw = NonNull::new(element.as_ptr().cast_mut().cast::<CFType>())
        .expect("retained AX element is non-null");
    // SAFETY: the explicit CFRetain above transferred one owned reference.
    Ok(unsafe { CFRetained::from_raw(raw) })
}

fn element_for_native(root: &Element, requested: *const CFType) -> Result<Element, AdapterError> {
    let mut found = None;
    walk(root, Arc::new(AtomicBool::new(false)), |_, element| {
        if unsafe { CFEqual(element.as_ptr(), requested.cast()) } {
            found = Some(element.clone());
            false
        } else {
            true
        }
    })?;
    found.ok_or_else(|| {
        adapter_error(
            "element_stale",
            "raw AX element value does not belong to the pinned target tree",
        )
    })
}

pub(crate) struct Element(NonNull<c_void>);

// AXUIElement references are immutable CF objects; all mutation is performed by the
// Accessibility server, so retaining and sending the reference between serialized calls is safe.
impl Element {
    pub(crate) fn as_ptr(&self) -> *const c_void {
        self.0.as_ptr()
    }

    pub(crate) fn application(pid: i32) -> Result<Self, AdapterError> {
        let raw = unsafe { AXUIElementCreateApplication(pid) };
        NonNull::new(raw.cast_mut())
            .map(Self)
            .ok_or_else(|| adapter_error("target_not_found", "AX application could not be created"))
    }

    fn attribute(&self, name: &str) -> Result<CFRetained<CFType>, AdapterError> {
        let name = CFString::from_str(name);
        let mut raw = ptr::null();
        let status = unsafe {
            AXUIElementCopyAttributeValue(
                self.0.as_ptr(),
                CFRetained::as_ptr(&name).as_ptr(),
                &mut raw,
            )
        };
        if status != AX_SUCCESS {
            return Err(ax_error(status, "copy attribute", name.to_string()));
        }
        let raw: NonNull<CFType> = NonNull::new(raw.cast_mut().cast())
            .ok_or_else(|| adapter_error("observation_failed", "AX returned a null attribute"))?;
        // SAFETY: CopyAttributeValue returned a +1 retained CF object.
        Ok(unsafe { CFRetained::from_raw(raw) })
    }

    fn elements(&self, name: &str) -> Result<Vec<Element>, AdapterError> {
        let value = self.attribute(name)?;
        let array: CFRetained<CFArray<CFType>> =
            // SAFETY: AXWindows and AXChildren are documented arrays of AXUIElement values.
            unsafe { CFRetained::cast_unchecked(value) };
        array
            .iter()
            .filter_map(|value| {
                (unsafe { CFGetTypeID(Some(&*value)) } == unsafe { AXUIElementGetTypeID() }).then(
                    || {
                        let raw = CFRetained::into_raw(value).cast();
                        Element(raw)
                    },
                )
            })
            .collect::<Vec<_>>()
            .pipe(Ok)
    }

    pub(crate) fn string(&self, name: &str) -> Option<String> {
        self.attribute(name)
            .ok()?
            .downcast::<CFString>()
            .ok()
            .map(|value| value.to_string())
    }

    pub(crate) fn bounds(&self) -> Result<WindowBounds, AdapterError> {
        let position = self.attribute("AXPosition")?;
        let size = self.attribute("AXSize")?;
        let mut point = CGPoint::ZERO;
        let mut dimensions = CGSize::ZERO;
        if !unsafe {
            AXValueGetValue(
                CFRetained::as_ptr(&position).as_ptr().cast(),
                1,
                (&mut point as *mut CGPoint).cast(),
            )
        } || !unsafe {
            AXValueGetValue(
                CFRetained::as_ptr(&size).as_ptr().cast(),
                2,
                (&mut dimensions as *mut CGSize).cast(),
            )
        } {
            return Err(adapter_error(
                "observation_failed",
                "AX window position or size was not a valid AXValue",
            ));
        }
        Ok(WindowBounds {
            x: point.x,
            y: point.y,
            width: dimensions.width,
            height: dimensions.height,
        })
    }

    fn set_string(&self, attribute: &str, value: &str) -> Result<(), AdapterError> {
        let value = CFString::from_str(value);
        self.set_cf(attribute, CFRetained::as_ptr(&value).as_ptr().cast())
    }

    pub(crate) fn set_bool(&self, attribute: &str, value: bool) -> Result<(), AdapterError> {
        self.set_cf(attribute, CFBoolean::new(value) as *const _ as _)
    }

    fn set_element(&self, attribute: &str, value: &Element) -> Result<(), AdapterError> {
        self.set_cf(attribute, value.as_ptr().cast())
    }

    pub(crate) fn is_focused(&self) -> bool {
        self.attribute("AXFocused")
            .ok()
            .and_then(|value| value.downcast::<CFBoolean>().ok())
            .is_some_and(|value| value.as_bool())
    }

    fn attribute_settable(&self, attribute: &str) -> Result<bool, AdapterError> {
        let attribute = CFString::from_str(attribute);
        let mut settable = false;
        let status = unsafe {
            AXUIElementIsAttributeSettable(
                self.0.as_ptr(),
                CFRetained::as_ptr(&attribute).as_ptr(),
                &mut settable,
            )
        };
        (status == AX_SUCCESS)
            .then_some(settable)
            .ok_or_else(|| ax_error(status, "inspect settable attribute", attribute.to_string()))
    }

    fn supports_action(&self, requested: &str) -> Result<bool, AdapterError> {
        let mut raw = ptr::null();
        let status = unsafe { AXUIElementCopyActionNames(self.0.as_ptr(), &mut raw) };
        if status != AX_SUCCESS {
            return Err(ax_error(status, "copy action names", requested.to_owned()));
        }
        let raw: NonNull<CFArray<CFString>> = NonNull::new(raw.cast_mut().cast())
            .ok_or_else(|| adapter_error("observation_failed", "AX returned null action names"))?;
        // SAFETY: CopyActionNames returns a +1 retained CFArray of CFString values.
        let actions = unsafe { CFRetained::from_raw(raw) };
        Ok(actions.iter().any(|action| action.to_string() == requested))
    }

    fn set_cf(&self, attribute: &str, value: *const c_void) -> Result<(), AdapterError> {
        let name = CFString::from_str(attribute);
        let status = unsafe {
            AXUIElementSetAttributeValue(self.0.as_ptr(), CFRetained::as_ptr(&name).as_ptr(), value)
        };
        (status == AX_SUCCESS)
            .then_some(())
            .ok_or_else(|| ax_error(status, "set attribute", attribute.to_owned()))
    }

    fn perform(&self, action: &str) -> Result<(), AdapterError> {
        crate::oracle::record("ax_perform_attempt", json!({ "action": action }));
        let action_name = CFString::from_str(action);
        let status = unsafe {
            AXUIElementPerformAction(self.0.as_ptr(), CFRetained::as_ptr(&action_name).as_ptr())
        };
        (status == AX_SUCCESS)
            .then_some(())
            .ok_or_else(|| ax_error(status, "perform action", action.to_owned()))
    }
}

impl Clone for Element {
    fn clone(&self) -> Self {
        unsafe { CFRetain(self.0.as_ptr()) };
        Self(self.0)
    }
}

impl Drop for Element {
    fn drop(&mut self) {
        unsafe { CFRelease(self.0.as_ptr()) };
    }
}

trait Pipe: Sized {
    fn pipe<T>(self, f: impl FnOnce(Self) -> T) -> T {
        f(self)
    }
}
impl<T> Pipe for T {}

fn ax_error(status: i32, operation: &str, value: String) -> AdapterError {
    let classification = if status == -25211 {
        "permission_required"
    } else {
        "backend_rejected"
    };
    AdapterError {
        code: classification.to_owned(),
        message: Some(
            format!("AX {operation} failed for {value} with status {status}")
                .chars()
                .take(256)
                .collect(),
        ),
        details: Some(json!({
            "domain": "AXError",
            "number": status,
            "classification": classification,
            "operation": operation,
        })),
    }
}

pub(crate) fn adapter_error(code: &str, message: &str) -> AdapterError {
    AdapterError {
        code: code.to_owned(),
        message: Some(message.chars().take(256).collect()),
        details: None,
    }
}

pub(crate) fn rejected_error(error: AdapterError) -> AdapterReply {
    let mut reply = AdapterReply::confirmed(Value::Null, None);
    reply.delivery = manuvra_runtime::AdapterDelivery::Rejected;
    reply.error = Some(error);
    reply
}

fn rejected(code: &str, message: &str) -> AdapterReply {
    rejected_error(adapter_error(code, message))
}

#[link(name = "ApplicationServices", kind = "framework")]
unsafe extern "C" {
    fn AXUIElementCreateApplication(pid: i32) -> *const c_void;
    #[cfg(debug_assertions)]
    fn AXUIElementCreateSystemWide() -> *const c_void;
    #[cfg(debug_assertions)]
    fn AXUIElementGetPid(element: *const c_void, pid: *mut i32) -> i32;
    fn AXUIElementGetTypeID() -> usize;
    fn AXValueGetTypeID() -> usize;
    fn AXValueGetType(value: *const c_void) -> i32;
    fn AXValueCreate(kind: i32, value: *const c_void) -> *const c_void;
    fn AXUIElementCopyAttributeValue(
        element: *const c_void,
        attribute: *const CFString,
        value: *mut *const c_void,
    ) -> i32;
    fn AXUIElementSetAttributeValue(
        element: *const c_void,
        attribute: *const CFString,
        value: *const c_void,
    ) -> i32;
    fn AXUIElementIsAttributeSettable(
        element: *const c_void,
        attribute: *const CFString,
        settable: *mut bool,
    ) -> i32;
    fn AXUIElementCopyActionNames(
        element: *const c_void,
        names: *mut *const CFArray<CFString>,
    ) -> i32;
    fn AXUIElementPerformAction(element: *const c_void, action: *const CFString) -> i32;
    fn AXValueGetValue(value: *const c_void, value_type: i32, output: *mut c_void) -> bool;
}

#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    fn CFRetain(value: *const c_void) -> *const c_void;
    fn CFRelease(value: *const c_void);
    fn CFGetTypeID(value: Option<&CFType>) -> usize;
    fn CFEqual(left: *const c_void, right: *const c_void) -> bool;
}

#[cfg(test)]
mod tests {
    use super::*;
    use objc2_core_foundation::CFMutableArray;

    #[test]
    fn bounds_match_is_strict_enough_to_fail_closed() {
        let pinned = WindowBounds {
            x: 10.0,
            y: 20.0,
            width: 300.0,
            height: 200.0,
        };
        assert!(close_enough(pinned, pinned));
        assert!(!close_enough(pinned, WindowBounds { x: 13.0, ..pinned }));
    }

    #[test]
    fn foreground_text_selection_collapses_at_the_utf16_end() {
        let range = text_end_range("A😀Z");
        assert_eq!(range.location, 4);
        assert_eq!(range.length, 0);
    }

    #[test]
    fn retained_element_must_remain_in_the_exact_window_tree() {
        let root = Element::application(std::process::id() as i32).unwrap();
        assert!(validate_descendant(&root, &root).is_ok());
        let foreign = Element::application(1).unwrap();
        assert_eq!(
            validate_descendant(&root, &foreign).unwrap_err().code,
            "element_stale"
        );
    }

    #[test]
    fn tagged_ax_scalars_round_trip_without_native_window_state() {
        let mut rows = Vec::new();
        for tagged in [
            json!({"type": "null", "value": null}),
            json!({"type": "boolean", "value": true}),
            json!({"type": "number", "value": 42.5}),
            json!({"type": "string", "value": "canary"}),
            json!({"type": "url", "value": "https://example.test/path"}),
        ] {
            let kind = tagged["type"].as_str().unwrap();
            let decoded = decode_ax_scalar(kind, &tagged).unwrap().unwrap();
            let encoded = encode_ax_scalar(&decoded).unwrap().unwrap();
            assert_eq!(encoded, tagged);
            rows.push(json!({"tagged_input": tagged, "tagged_output": encoded}));
        }
        crate::test_oracles::write(
            "raw-ax-scalars.json",
            &json!({
                "schema": "manuvra/cp-07-boundary-oracle@1",
                "case": "raw_ax_scalar_round_trip",
                "rows": rows,
            }),
        );
    }

    #[test]
    fn tagged_ax_structs_round_trip_every_supported_native_kind() {
        let mut rows = Vec::new();
        for (native_kind, tagged) in [
            (1, json!({"type": "point", "x": 1.5, "y": -2.0})),
            (2, json!({"type": "size", "width": 30.0, "height": 40.5})),
            (
                3,
                json!({
                    "type": "rect", "x": 1.0, "y": 2.0,
                    "width": 300.0, "height": 200.0,
                }),
            ),
            (4, json!({"type": "range", "location": 7, "length": 9})),
        ] {
            let kind = tagged["type"].as_str().unwrap();
            let decoded = decode_ax_struct(kind, &tagged).unwrap().unwrap();
            let encoded = encode_ax_struct(&decoded, native_kind).unwrap();
            assert_eq!(encoded, tagged);
            rows.push(json!({
                "native_kind": native_kind,
                "tagged_input": tagged,
                "tagged_output": encoded,
            }));
        }
        crate::test_oracles::write(
            "raw-ax-structs.json",
            &json!({
                "schema": "manuvra/cp-07-boundary-oracle@1",
                "case": "raw_ax_struct_round_trip",
                "rows": rows,
            }),
        );
    }

    #[test]
    fn tagged_ax_collections_round_trip_recursively() {
        let window = Element::application(std::process::id() as i32).unwrap();
        let context = AdapterContext {
            session_id: "s_boundary_oracle".to_owned(),
            target_id: "macos_boundary_oracle".to_owned(),
            target_generation: 1,
            action_sequence: 1,
            reference_namespace: "n_boundary_oracle".to_owned(),
            reference_epoch: 1,
            frame_token: None,
            mode: manuvra_runtime::ExecutionMode::Background,
            deadline: std::time::Instant::now() + std::time::Duration::from_secs(1),
        };
        let rows = [
            json!({
                "type": "array",
                "value": [
                    {"type": "null", "value": null},
                    {"type": "string", "value": "nested"}
                ]
            }),
            json!({
                "type": "dictionary",
                "value": {
                    "enabled": {"type": "boolean", "value": true},
                    "count": {"type": "number", "value": 2.5}
                }
            }),
        ];
        let mut references = ReferenceStore::default();
        let mut observed = Vec::new();
        for tagged in rows {
            let mut decoded_items = 0;
            let decoded =
                decode_ax_value(&context, &references, &tagged, 0, &mut decoded_items).unwrap();
            let mut encoded_items = 0;
            let encoded = encode_ax_value(
                &window,
                &context,
                &mut references,
                &decoded,
                0,
                &mut encoded_items,
            )
            .unwrap();
            assert_eq!(encoded, tagged);
            observed.push(json!({
                "tagged_input": tagged,
                "tagged_output": encoded,
                "decoded_items": decoded_items,
                "encoded_items": encoded_items,
            }));
        }
        crate::test_oracles::write(
            "raw-ax-collections.json",
            &json!({
                "schema": "manuvra/cp-07-boundary-oracle@1",
                "case": "raw_ax_collection_round_trip",
                "rows": observed,
            }),
        );
    }

    #[test]
    fn tagged_ax_validation_fails_closed_at_type_and_size_boundaries() {
        assert!(decode_ax_scalar("unknown", &json!({})).unwrap().is_none());
        assert_eq!(
            decode_ax_scalar("boolean", &json!({"type": "boolean"}))
                .unwrap_err()
                .code,
            "invalid_request"
        );
        assert_eq!(
            encode_ax_struct(&decode_ax_point(&json!({"x": 1, "y": 2})).unwrap(), 99)
                .unwrap_err()
                .code,
            "raw_value_unsupported"
        );
        let mut items = MAX_RAW_ITEMS;
        assert_eq!(
            raw_item(0, &mut items).unwrap_err().code,
            "raw_value_unsupported"
        );
        let mut items = 0;
        assert_eq!(
            raw_item(MAX_RAW_DEPTH + 1, &mut items).unwrap_err().code,
            "raw_value_unsupported"
        );
        crate::test_oracles::write(
            "raw-ax-negatives.json",
            &json!({
                "schema": "manuvra/cp-07-boundary-oracle@1",
                "case": "raw_ax_negative_boundaries",
                "rows": [
                    {"input": "unknown_scalar_type", "result": "unsupported_branch"},
                    {"input": "missing_boolean_value", "error": "invalid_request"},
                    {"input": "unknown_ax_struct_kind", "error": "raw_value_unsupported"},
                    {"input": "item_limit", "error": "raw_value_unsupported"},
                    {"input": "depth_limit", "error": "raw_value_unsupported"}
                ]
            }),
        );
    }

    #[test]
    fn tagged_ax_element_cycle_and_native_error_boundaries_are_preserved() {
        let root = Element::application(std::process::id() as i32).unwrap();
        let context = AdapterContext {
            session_id: "s_element_boundary".to_owned(),
            target_id: "macos_element_boundary".to_owned(),
            target_generation: 1,
            action_sequence: 1,
            reference_namespace: "n_element_boundary".to_owned(),
            reference_epoch: 7,
            frame_token: None,
            mode: manuvra_runtime::ExecutionMode::Background,
            deadline: std::time::Instant::now() + std::time::Duration::from_secs(1),
        };
        let mut references = ReferenceStore::default();
        let (_, reference) = references.issue(&context, &root);
        let tagged = json!({"type": "element", "ref": reference});
        let mut decoded_items = 0;
        let decoded =
            decode_ax_value(&context, &references, &tagged, 0, &mut decoded_items).unwrap();
        let mut encoded_items = 0;
        let encoded = encode_ax_value(
            &root,
            &context,
            &mut references,
            &decoded,
            0,
            &mut encoded_items,
        )
        .unwrap();
        assert_eq!(encoded, tagged);

        let stale = json!({"type": "element", "ref": "e_n_element_boundary_6_0"});
        let foreign = json!({"type": "element", "ref": "e_n_foreign_7_0"});
        let forged = json!({"type": "element", "ref": "e_n_element_boundary_7_mffff"});
        for invalid in [&stale, &foreign, &forged] {
            let mut items = 0;
            assert_eq!(
                decode_ax_value(&context, &references, invalid, 0, &mut items)
                    .unwrap_err()
                    .code,
                "element_stale"
            );
        }

        let cycle = CFMutableArray::<CFType>::with_capacity(1);
        let cycle_as_type: CFRetained<CFType> =
            unsafe { CFRetained::cast_unchecked(cycle.retain()) };
        cycle.append(&cycle_as_type);
        let mut cycle_items = 0;
        let cycle_error = encode_ax_value(
            &root,
            &context,
            &mut references,
            &cycle_as_type,
            0,
            &mut cycle_items,
        )
        .unwrap_err();
        CFMutableArray::remove_all_values(Some(cycle.as_opaque()));
        assert_eq!(cycle_error.code, "raw_value_unsupported");

        let native_error = ax_error(-25205, "copy attribute", "AXUnknown".to_owned());
        assert_eq!(native_error.code, "backend_rejected");
        assert_eq!(native_error.details.as_ref().unwrap()["domain"], "AXError");
        assert_eq!(native_error.details.as_ref().unwrap()["number"], -25205);
        assert_eq!(
            native_error.details.as_ref().unwrap()["classification"],
            "backend_rejected"
        );
        crate::test_oracles::write(
            "raw-ax-elements-negatives.json",
            &json!({
                "schema": "manuvra/cp-07-boundary-oracle@1",
                "case": "raw_ax_element_cycle_and_native_error",
                "element": {"input": tagged, "output": encoded},
                "stale_element": {"input": stale, "error": "element_stale"},
                "foreign_element": {"input": foreign, "error": "element_stale"},
                "cycle": {"error": cycle_error.code},
                "native_error": native_error.details,
            }),
        );
    }
}
