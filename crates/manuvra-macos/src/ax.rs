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
        if let Some(existing) = self.existing_reference(context, element) {
            return existing;
        }
        self.issue_new(context, element)
    }

    fn existing_reference(
        &self,
        context: &AdapterContext,
        element: &Element,
    ) -> Option<(String, String)> {
        let (reference, _) = self
            .issued
            .iter()
            .find(|(_, issued)| issued.matches_element(context, element))?;
        Some((backend_id_from_reference(reference), reference.clone()))
    }

    fn issue_new(&mut self, context: &AdapterContext, element: &Element) -> (String, String) {
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
        self.evict_overflow();
        (backend_id, reference)
    }

    fn evict_overflow(&mut self) {
        while self.issued.len() > MAX_REFERENCE_ENTRIES {
            if let Some(oldest) = self.order.pop_front() {
                self.issued.remove(&oldest);
            }
        }
    }

    fn resolve(
        &self,
        context: &AdapterContext,
        value: Option<&Value>,
    ) -> Result<Element, AdapterError> {
        let reference = required_element_ref(value)?;
        require_current_ref_prefix(context, reference)?;
        self.issued
            .get(reference)
            .filter(|issued| issued.matches_session(context))
            .map(|issued| issued.element.clone())
            .ok_or_else(|| {
                adapter_error(
                    "element_stale",
                    "element ref was not issued for this session and epoch",
                )
            })
    }
}

impl IssuedReference {
    fn matches_session(&self, context: &AdapterContext) -> bool {
        self.session_id == context.session_id
            && self.namespace == context.reference_namespace
            && self.epoch == context.reference_epoch
    }

    fn matches_element(&self, context: &AdapterContext, element: &Element) -> bool {
        self.matches_session(context) && unsafe { CFEqual(self.element.as_ptr(), element.as_ptr()) }
    }
}

fn required_element_ref(value: Option<&Value>) -> Result<&str, AdapterError> {
    value
        .and_then(Value::as_str)
        .ok_or_else(|| adapter_error("invalid_request", "element ref is required"))
}

fn require_current_ref_prefix(
    context: &AdapterContext,
    reference: &str,
) -> Result<(), AdapterError> {
    let prefix = format!(
        "e_{}_{}_",
        context.reference_namespace, context.reference_epoch
    );
    if reference.starts_with(&prefix) {
        Ok(())
    } else {
        Err(adapter_error("element_stale", "element ref epoch is stale"))
    }
}

fn backend_id_from_reference(reference: &str) -> String {
    reference
        .rsplit_once('_')
        .map(|(_, backend)| backend)
        .unwrap_or_default()
        .to_owned()
}

pub(crate) fn invoke(
    record: &WindowRecord,
    context: &AdapterContext,
    operation: &AdapterOperation,
    cancellation: Arc<AtomicBool>,
    references: &mut ReferenceStore,
) -> AdapterReply {
    if let Some(reply) =
        cancelled_reply(&cancellation, "operation was cancelled before AX dispatch")
    {
        return reply;
    }
    ax_value_reply(dispatch_ax_read(
        record,
        context,
        operation,
        cancellation,
        references,
    ))
}

fn cancelled_reply(cancellation: &AtomicBool, message: &str) -> Option<AdapterReply> {
    cancellation
        .load(Ordering::SeqCst)
        .then(|| rejected("cancelled", message))
}

fn dispatch_ax_read(
    record: &WindowRecord,
    context: &AdapterContext,
    operation: &AdapterOperation,
    cancellation: Arc<AtomicBool>,
    references: &mut ReferenceStore,
) -> Result<Value, AdapterError> {
    ax_read_command(
        exact_window(record)?,
        context,
        operation,
        cancellation,
        references,
    )
}

fn ax_read_command(
    window: Element,
    context: &AdapterContext,
    operation: &AdapterOperation,
    cancellation: Arc<AtomicBool>,
    references: &mut ReferenceStore,
) -> Result<Value, AdapterError> {
    if operation.command.starts_with("observe.") {
        return ax_observe(&window, context, operation, cancellation, references);
    }
    raw_get_or_unsupported(&window, context, operation, references)
}

fn raw_get_or_unsupported(
    window: &Element,
    context: &AdapterContext,
    operation: &AdapterOperation,
    references: &mut ReferenceStore,
) -> Result<Value, AdapterError> {
    if operation.command == "raw.ax.get" {
        raw_get(window, context, operation, references)
    } else {
        Err(adapter_error(
            "capability_unavailable",
            "operation is not implemented by the AX path",
        ))
    }
}

fn ax_observe(
    window: &Element,
    context: &AdapterContext,
    operation: &AdapterOperation,
    cancellation: Arc<AtomicBool>,
    references: &mut ReferenceStore,
) -> Result<Value, AdapterError> {
    if operation.command == "observe.query" {
        query(window, context, operation, cancellation, references)
    } else {
        tree(window, context, cancellation, references)
    }
}

fn ax_value_reply(result: Result<Value, AdapterError>) -> AdapterReply {
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
    constrain_prepared_element(&window, context, operation, references, element.as_ref())?;
    Ok(PreparedAx { element })
}

fn constrain_prepared_element(
    window: &Element,
    context: &AdapterContext,
    operation: &AdapterOperation,
    references: &ReferenceStore,
    element: Option<&Element>,
) -> Result<(), AdapterError> {
    validate_prepared_descendant(window, element)?;
    remaining_prepared_constraints(context, operation, references, element)
}

fn remaining_prepared_constraints(
    context: &AdapterContext,
    operation: &AdapterOperation,
    references: &ReferenceStore,
    element: Option<&Element>,
) -> Result<(), AdapterError> {
    constrain_background_or_raw(context, operation, element)?;
    decode_prepared_raw_set(context, operation, references)
}

fn validate_prepared_descendant(
    window: &Element,
    element: Option<&Element>,
) -> Result<(), AdapterError> {
    let Some(element) = element else {
        return Ok(());
    };
    validate_descendant(window, element)
}

fn constrain_background_or_raw(
    context: &AdapterContext,
    operation: &AdapterOperation,
    element: Option<&Element>,
) -> Result<(), AdapterError> {
    if !background_or_raw(context, operation) {
        return Ok(());
    }
    validate_background_mutation(operation, element)
}

fn background_or_raw(context: &AdapterContext, operation: &AdapterOperation) -> bool {
    context.mode.as_str() == "background" || operation.command.starts_with("raw.ax.")
}

fn decode_prepared_raw_set(
    context: &AdapterContext,
    operation: &AdapterOperation,
    references: &ReferenceStore,
) -> Result<(), AdapterError> {
    if operation.command != "raw.ax.set" {
        return Ok(());
    }
    let value = operation
        .input
        .get("value")
        .ok_or_else(|| adapter_error("invalid_request", "AX value is required"))?;
    let mut items = 0usize;
    decode_ax_value(context, references, value, 0, &mut items).map(|_| ())
}

fn mutation_element(
    window: &Element,
    context: &AdapterContext,
    operation: &AdapterOperation,
    references: &ReferenceStore,
) -> Result<Option<Element>, AdapterError> {
    if let Some(element) = action_mutation_element(window, context, operation, references)? {
        return Ok(Some(element));
    }
    raw_mutation_element(context, operation, references)
}

fn action_mutation_element(
    window: &Element,
    context: &AdapterContext,
    operation: &AdapterOperation,
    references: &ReferenceStore,
) -> Result<Option<Element>, AdapterError> {
    match operation.command.as_str() {
        "action.click" | "action.type" => {
            click_or_type_element(window, context, operation, references)
        }
        "action.press" | "action.scroll" => {
            press_or_scroll_element(window, context, operation, references)
        }
        _ => Ok(None),
    }
}

fn click_or_type_element(
    window: &Element,
    context: &AdapterContext,
    operation: &AdapterOperation,
    references: &ReferenceStore,
) -> Result<Option<Element>, AdapterError> {
    if locator_kind(operation) == Some("point") {
        return Ok(None);
    }
    resolve_locator(window, context, operation, references).map(Some)
}

fn press_or_scroll_element(
    window: &Element,
    context: &AdapterContext,
    operation: &AdapterOperation,
    references: &ReferenceStore,
) -> Result<Option<Element>, AdapterError> {
    if operation.input.get("locator").is_none() {
        return Ok(None);
    }
    click_or_type_element(window, context, operation, references)
}

fn raw_mutation_element(
    context: &AdapterContext,
    operation: &AdapterOperation,
    references: &ReferenceStore,
) -> Result<Option<Element>, AdapterError> {
    match operation.command.as_str() {
        "raw.ax.set" | "raw.ax.perform" => references
            .resolve(context, operation.input.get("ref"))
            .map(Some),
        _ => Ok(None),
    }
}

fn validate_background_mutation(
    operation: &AdapterOperation,
    element: Option<&Element>,
) -> Result<(), AdapterError> {
    match operation.command.as_str() {
        "action.click" => validate_background_click(operation, element),
        "action.type" => validate_background_type(operation, element),
        "raw.ax.set" | "raw.ax.perform" => validate_background_raw(operation, element),
        _ => Ok(()),
    }
}

fn validate_background_raw(
    operation: &AdapterOperation,
    element: Option<&Element>,
) -> Result<(), AdapterError> {
    let element = required_element(element)?;
    if operation.command == "raw.ax.set" {
        validate_background_set(operation, element)
    } else {
        validate_background_perform(operation, element)
    }
}

fn required_element(element: Option<&Element>) -> Result<&Element, AdapterError> {
    element.ok_or_else(|| adapter_error("element_not_found", "mutation element is absent"))
}

fn validate_background_click(
    operation: &AdapterOperation,
    element: Option<&Element>,
) -> Result<(), AdapterError> {
    background_click_policy(operation)?;
    require_advertised_action(required_element(element)?, "AXPress")
}

fn background_click_policy(operation: &AdapterOperation) -> Result<(), AdapterError> {
    if locator_kind(operation) == Some("point") {
        return Err(adapter_error(
            "foreground_required",
            "point clicks require explicit foreground mode",
        ));
    }
    require_single_left_click(operation)
}

fn require_single_left_click(operation: &AdapterOperation) -> Result<(), AdapterError> {
    if operation.input.get("button").and_then(Value::as_str) == Some("left")
        && operation.input.get("count").and_then(Value::as_u64) == Some(1)
    {
        Ok(())
    } else {
        Err(adapter_error(
            "foreground_required",
            "background click supports only one left AXPress",
        ))
    }
}

fn validate_background_type(
    operation: &AdapterOperation,
    element: Option<&Element>,
) -> Result<(), AdapterError> {
    reject_point_background(operation, "point typing requires explicit foreground mode")?;
    require_settable_value(required_element(element)?)
}

fn reject_point_background(
    operation: &AdapterOperation,
    message: &str,
) -> Result<(), AdapterError> {
    if locator_kind(operation) == Some("point") {
        Err(adapter_error("foreground_required", message))
    } else {
        Ok(())
    }
}

fn require_settable_value(element: &Element) -> Result<(), AdapterError> {
    if element.attribute_settable("AXValue")? {
        Ok(())
    } else {
        Err(adapter_error(
            "foreground_required",
            "the exact AX element does not expose settable AXValue",
        ))
    }
}

fn require_advertised_action(element: &Element, action: &str) -> Result<(), AdapterError> {
    require_native_capability(element.supports_action(action)?, action)
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
    if let Some(reply) =
        cancelled_reply(&cancellation, "operation was cancelled before AX dispatch")
    {
        return reply;
    }
    if is_foreground_action(context, operation) {
        return crate::foreground::invoke_prepared(
            record,
            context,
            operation,
            prepared.element.as_ref(),
            cancellation,
        );
    }
    ax_value_reply(invoke_ax_mutation(context, operation, prepared, references))
}

fn is_foreground_action(context: &AdapterContext, operation: &AdapterOperation) -> bool {
    context.mode.as_str() == "foreground" && operation.command.starts_with("action.")
}

fn invoke_ax_mutation(
    context: &AdapterContext,
    operation: &AdapterOperation,
    prepared: &PreparedAx,
    references: &ReferenceStore,
) -> Result<Value, AdapterError> {
    match operation.command.as_str() {
        "action.click" => prepared_click(prepared),
        "action.type" => prepared_type(prepared, operation),
        "raw.ax.set" => prepared_raw_set(prepared, context, operation, references),
        "raw.ax.perform" => prepared_raw_perform(prepared, operation),
        _ => Err(adapter_error(
            "capability_unavailable",
            "prepared mutation command is unsupported",
        )),
    }
}

fn prepared_element<'a>(
    prepared: &'a PreparedAx,
    missing: &str,
) -> Result<&'a Element, AdapterError> {
    prepared
        .element
        .as_ref()
        .ok_or_else(|| adapter_error("element_not_found", missing))
}

fn prepared_click(prepared: &PreparedAx) -> Result<Value, AdapterError> {
    prepared_element(prepared, "prepared click element is absent")?.perform("AXPress")?;
    Ok(json!({"performed": "AXPress", "effective_mode": "background"}))
}

fn prepared_type(
    prepared: &PreparedAx,
    operation: &AdapterOperation,
) -> Result<Value, AdapterError> {
    type_prepared(
        prepared_element(prepared, "prepared type element is absent")?,
        operation,
        false,
    )
}

fn prepared_raw_set(
    prepared: &PreparedAx,
    context: &AdapterContext,
    operation: &AdapterOperation,
    references: &ReferenceStore,
) -> Result<Value, AdapterError> {
    raw_set_prepared(
        prepared_element(prepared, "prepared raw element is absent")?,
        context,
        operation,
        references,
    )
}

fn prepared_raw_perform(
    prepared: &PreparedAx,
    operation: &AdapterOperation,
) -> Result<Value, AdapterError> {
    raw_perform_prepared(
        prepared_element(prepared, "prepared raw element is absent")?,
        operation,
    )
}

fn type_prepared(
    element: &Element,
    operation: &AdapterOperation,
    collapse_selection: bool,
) -> Result<Value, AdapterError> {
    write_typed_value(
        element,
        required_string(&operation.input, "text")?,
        operation
            .input
            .get("replace")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        collapse_selection,
    )
}

fn write_typed_value(
    element: &Element,
    requested: &str,
    replace: bool,
    collapse_selection: bool,
) -> Result<Value, AdapterError> {
    let text = typed_ax_value(element, requested, replace);
    crate::oracle::record(
        "ax_value_set",
        json!({"replace": replace, "characters": requested.chars().count()}),
    );
    apply_typed_value(element, text, requested, collapse_selection)
}

fn apply_typed_value(
    element: &Element,
    text: String,
    requested: &str,
    collapse_selection: bool,
) -> Result<Value, AdapterError> {
    element.set_string("AXValue", &text)?;
    finish_typed_value(element, text, requested, collapse_selection)
}

fn finish_typed_value(
    element: &Element,
    text: String,
    requested: &str,
    collapse_selection: bool,
) -> Result<Value, AdapterError> {
    maybe_collapse_typed_selection(element, &text, collapse_selection)?;
    Ok(json!({"characters": requested.chars().count(), "effective_mode": "background"}))
}

fn typed_ax_value(element: &Element, requested: &str, replace: bool) -> String {
    if replace {
        requested.to_owned()
    } else {
        format!("{}{requested}", current_ax_string(element))
    }
}

fn current_ax_string(element: &Element) -> String {
    element
        .attribute("AXValue")
        .ok()
        .and_then(|value| value.downcast::<CFString>().ok())
        .map(|value| value.to_string())
        .unwrap_or_default()
}

fn maybe_collapse_typed_selection(
    element: &Element,
    text: &str,
    collapse_selection: bool,
) -> Result<(), AdapterError> {
    if !collapse_selection {
        return Ok(());
    }
    record_collapsed_selection(collapse_selection_at_text_end(element, text)?)
}

fn record_collapsed_selection(result: Option<(CFRange, usize)>) -> Result<(), AdapterError> {
    let Some((text_end, attempts)) = result else {
        return Ok(());
    };
    crate::oracle::record(
        "ax_selection_collapsed",
        json!({"utf16_location": text_end.location, "attempts": attempts}),
    );
    Ok(())
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
    try_collapse_selection(element, text)
}

fn try_collapse_selection(
    element: &Element,
    text: &str,
) -> Result<Option<(CFRange, usize)>, AdapterError> {
    let expected = text_end_range(text);
    retry_collapse(element, expected, ax_struct(4, &expected)?)
}

fn retry_collapse(
    element: &Element,
    expected: CFRange,
    range: CFRetained<CFType>,
) -> Result<Option<(CFRange, usize)>, AdapterError> {
    (1..=5)
        .find_map(|attempt| collapse_attempt(element, &range, expected, attempt))
        .unwrap_or_else(|| {
            Err(adapter_error(
                "dispatch_failed",
                "AXSelectedTextRange did not retain the requested insertion point",
            ))
        })
}

fn collapse_attempt(
    element: &Element,
    range: &CFRetained<CFType>,
    expected: CFRange,
    attempt: usize,
) -> Option<Result<Option<(CFRange, usize)>, AdapterError>> {
    selection_matches(element, range, expected).map_or_else(
        |error| Some(Err(error)),
        |matched| matched.then_some(Ok(Some((expected, attempt)))),
    )
}

fn selection_matches(
    element: &Element,
    range: &CFRetained<CFType>,
    expected: CFRange,
) -> Result<bool, AdapterError> {
    thread::park_timeout(Duration::from_millis(10));
    element.set_cf(
        "AXSelectedTextRange",
        CFRetained::as_ptr(range).as_ptr().cast(),
    )?;
    observed_range_matches(element, expected)
}

fn observed_range_matches(element: &Element, expected: CFRange) -> Result<bool, AdapterError> {
    thread::park_timeout(Duration::from_millis(10));
    Ok(range_equals(selected_text_range(element)?, expected))
}

fn range_equals(observed: CFRange, expected: CFRange) -> bool {
    observed.location == expected.location && observed.length == expected.length
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
    set_raw_attribute(
        element,
        required_string(&operation.input, "attribute")?,
        context,
        operation,
        references,
    )
}

fn set_raw_attribute(
    element: &Element,
    attribute: &str,
    context: &AdapterContext,
    operation: &AdapterOperation,
    references: &ReferenceStore,
) -> Result<Value, AdapterError> {
    set_decoded_attribute(
        element,
        attribute,
        decode_raw_set_value(context, operation, references)?,
    )
}

fn decode_raw_set_value(
    context: &AdapterContext,
    operation: &AdapterOperation,
    references: &ReferenceStore,
) -> Result<CFRetained<CFType>, AdapterError> {
    let value = operation
        .input
        .get("value")
        .ok_or_else(|| adapter_error("invalid_request", "AX value is required"))?;
    let mut items = 0usize;
    decode_ax_value(context, references, value, 0, &mut items)
}

fn set_decoded_attribute(
    element: &Element,
    attribute: &str,
    value: CFRetained<CFType>,
) -> Result<Value, AdapterError> {
    element.set_cf(attribute, CFRetained::as_ptr(&value).as_ptr().cast())?;
    Ok(json!({"attribute": attribute, "set": true}))
}

fn raw_perform_prepared(
    element: &Element,
    operation: &AdapterOperation,
) -> Result<Value, AdapterError> {
    perform_raw_action(element, required_string(&operation.input, "action")?)
}

fn perform_raw_action(element: &Element, action: &str) -> Result<Value, AdapterError> {
    element.perform(action)?;
    Ok(json!({"action": action, "performed": true}))
}

pub(crate) fn exact_window(record: &WindowRecord) -> Result<Element, AdapterError> {
    let application = Element::application(record.snapshot.pid)?;
    unique_bounds_match(window_candidates(&application), record.snapshot.bounds)
}

fn window_candidates(application: &Element) -> Vec<(Element, WindowBounds)> {
    application_windows(application)
        .into_iter()
        .filter_map(|window| {
            let bounds = window.bounds().ok()?;
            Some((window, bounds))
        })
        .collect()
}

fn unique_bounds_match(
    candidates: Vec<(Element, WindowBounds)>,
    expected: WindowBounds,
) -> Result<Element, AdapterError> {
    crate::oracle::record(
        "ax_window_resolution",
        json!({
            "expected_bounds": expected,
            "candidate_bounds": candidates.iter().map(|(_, bounds)| bounds).collect::<Vec<_>>(),
        }),
    );
    select_unique_window(
        candidates
            .iter()
            .filter(|(_, bounds)| close_enough(*bounds, expected))
            .map(|(window, _)| window.clone())
            .collect(),
        expected,
        &candidates,
    )
}

fn select_unique_window(
    matches: Vec<Element>,
    expected: WindowBounds,
    candidates: &[(Element, WindowBounds)],
) -> Result<Element, AdapterError> {
    match matches.len() {
        1 => Ok(matches.into_iter().next().expect("len is 1")),
        0 => Err(adapter_error(
            "target_not_found",
            &format!(
                "no AX window matches pinned bounds {:?}; AX candidates are {:?}",
                expected,
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
    raise_main_window(
        Element::application(record.snapshot.pid)?,
        exact_window(record)?,
    )
}

fn raise_main_window(application: Element, window: Element) -> Result<(), AdapterError> {
    window.perform("AXRaise")?;
    set_main_and_focused(&application, &window)
}

fn set_main_and_focused(application: &Element, window: &Element) -> Result<(), AdapterError> {
    window.set_bool("AXMain", true)?;
    application.set_element("AXFocusedWindow", window)
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
    let focused_application = system_focused_application()?;
    focused_window_snapshot_from(
        &focused_application,
        focused_application_pid(&focused_application)?,
    )
}

fn system_wide_element() -> Result<Element, AdapterError> {
    let raw = unsafe { AXUIElementCreateSystemWide() };
    NonNull::new(raw.cast_mut())
        .map(Element)
        .ok_or_else(|| adapter_error("observation_failed", "AX system-wide element was null"))
}

fn system_focused_application() -> Result<Element, AdapterError> {
    element_attribute(&system_wide_element()?, "AXFocusedApplication")
}

fn focused_application_pid(application: &Element) -> Result<i32, AdapterError> {
    let mut pid = 0;
    require_ax_success(
        unsafe { AXUIElementGetPid(application.as_ptr(), &mut pid) },
        "read focused application pid",
        "AXFocusedApplication",
    )
    .map(|()| pid)
}

fn focused_window_snapshot_from(application: &Element, pid: i32) -> Result<Value, AdapterError> {
    let focused_window = element_attribute(application, "AXFocusedWindow")?;
    Ok(json!({
        "available": true,
        "application_pid": pid,
        "window_title": focused_window.string("AXTitle"),
        "window_bounds": focused_window.bounds().ok(),
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
        push_window_candidates(
            &mut windows,
            application.elements(attribute).unwrap_or_default(),
        );
    }
    windows
}

fn push_window_candidates(windows: &mut Vec<Element>, candidates: Vec<Element>) {
    for candidate in candidates {
        if is_window_role(candidate.string("AXRole").as_deref())
            && !contains_element(windows, &candidate)
        {
            windows.push(candidate);
        }
    }
}

fn is_window_role(role: Option<&str>) -> bool {
    matches!(role, Some("AXWindow" | "AXDialog" | "AXSheet"))
}

fn contains_element(windows: &[Element], candidate: &Element) -> bool {
    windows
        .iter()
        .any(|window| unsafe { CFEqual(window.as_ptr(), candidate.as_ptr()) })
}

pub(crate) fn same_bounds(left: WindowBounds, right: WindowBounds) -> bool {
    close_enough(left, right)
}

pub(crate) fn observer_elements(record: &WindowRecord) -> Result<Vec<Element>, AdapterError> {
    collect_observer_elements(
        Element::application(record.snapshot.pid)?,
        exact_window(record)?,
    )
}

fn collect_observer_elements(
    application: Element,
    root: Element,
) -> Result<Vec<Element>, AdapterError> {
    let mut elements = vec![application];
    let mut stack = vec![root];
    while let Some(element) = stack.pop() {
        push_observer_element(&mut elements, &mut stack, element)?;
    }
    Ok(elements)
}

fn push_observer_element(
    elements: &mut Vec<Element>,
    stack: &mut Vec<Element>,
    element: Element,
) -> Result<(), AdapterError> {
    if elements.len() >= MAX_TREE_NODES {
        return Err(adapter_error(
            "observation_failed",
            "AX observer element set exceeded the node bound",
        ));
    }
    stack.extend(element.elements("AXChildren").unwrap_or_default());
    elements.push(element);
    Ok(())
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
    let semantic = required_semantic(operation)?;
    let limit = query_limit(operation);
    let matches = collect_semantic_matches(window, context, cancellation, references, semantic)?;
    Ok(split_query_matches(matches, limit))
}

fn required_semantic(operation: &AdapterOperation) -> Result<&Map<String, Value>, AdapterError> {
    operation
        .input
        .get("semantic")
        .and_then(Value::as_object)
        .ok_or_else(|| adapter_error("invalid_request", "semantic locator is required"))
}

fn query_limit(operation: &AdapterOperation) -> usize {
    operation
        .input
        .get("limit")
        .and_then(Value::as_u64)
        .unwrap_or(5)
        .clamp(1, 5) as usize
}

fn collect_semantic_matches(
    window: &Element,
    context: &AdapterContext,
    cancellation: Arc<AtomicBool>,
    references: &mut ReferenceStore,
    semantic: &Map<String, Value>,
) -> Result<Vec<Value>, AdapterError> {
    let mut matches = Vec::new();
    walk(window, cancellation, |_, element| {
        if semantic_matches(element, semantic) {
            let (backend_id, _) = references.issue(context, element);
            matches.push(match_entry(&backend_id, element));
        }
        true
    })?;
    Ok(matches)
}

fn split_query_matches(mut matches: Vec<Value>, limit: usize) -> Value {
    let overflow = if matches.len() > limit {
        matches.split_off(limit)
    } else {
        Vec::new()
    };
    json!({"matches": matches, "overflow": overflow})
}

fn tree(
    window: &Element,
    context: &AdapterContext,
    cancellation: Arc<AtomicBool>,
    references: &mut ReferenceStore,
) -> Result<Value, AdapterError> {
    let nodes = collect_tree_nodes(window, context, cancellation, references)?;
    require_complete_tree(&nodes)?;
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

fn collect_tree_nodes(
    window: &Element,
    context: &AdapterContext,
    cancellation: Arc<AtomicBool>,
    references: &mut ReferenceStore,
) -> Result<Vec<Value>, AdapterError> {
    let mut nodes = Vec::new();
    walk(window, cancellation, |_, element| {
        nodes.push(Value::Object(tree_node(context, references, element)));
        nodes.len() < MAX_TREE_NODES
    })?;
    Ok(nodes)
}

fn tree_node(
    context: &AdapterContext,
    references: &mut ReferenceStore,
    element: &Element,
) -> Map<String, Value> {
    let (backend_id, reference) = references.issue(context, element);
    let mut node = public_element(element);
    node.insert("ref".to_owned(), json!(reference));
    node.insert("backend_id".to_owned(), json!(backend_id));
    node
}

fn require_complete_tree(nodes: &[Value]) -> Result<(), AdapterError> {
    if nodes.len() >= MAX_TREE_NODES {
        Err(adapter_error(
            "observation_failed",
            "AX tree exceeded the complete-tree node bound",
        ))
    } else {
        Ok(())
    }
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
    let locator = required_locator(operation)?;
    match locator.get("kind").and_then(Value::as_str) {
        Some("ref") => references.resolve(context, locator.get("ref")),
        Some("semantic") => resolve_semantic_locator(window, locator),
        Some("point") => Err(adapter_error(
            "foreground_required",
            "point locators require the foreground CGEvent path",
        )),
        _ => Err(adapter_error("invalid_request", "unknown AX locator kind")),
    }
}

fn required_locator(operation: &AdapterOperation) -> Result<&Map<String, Value>, AdapterError> {
    operation
        .input
        .get("locator")
        .and_then(Value::as_object)
        .ok_or_else(|| adapter_error("invalid_request", "AX locator is required"))
}

fn resolve_semantic_locator(
    window: &Element,
    locator: &Map<String, Value>,
) -> Result<Element, AdapterError> {
    let mut found = Vec::new();
    walk(window, Arc::new(AtomicBool::new(false)), |_, element| {
        if semantic_matches(element, locator) {
            found.push(element.clone());
        }
        found.len() < 2
    })?;
    unique_semantic_match(found)
}

fn unique_semantic_match(mut found: Vec<Element>) -> Result<Element, AdapterError> {
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

fn walk(
    root: &Element,
    cancellation: Arc<AtomicBool>,
    mut visit: impl FnMut(&str, &Element) -> bool,
) -> Result<(), AdapterError> {
    let mut stack = vec![("0".to_owned(), root.clone())];
    let mut visited = 0usize;
    while let Some((path, element)) = stack.pop() {
        walk_next(
            &mut stack,
            &mut visited,
            &cancellation,
            &mut visit,
            path,
            element,
        )?;
    }
    Ok(())
}

fn walk_next(
    stack: &mut Vec<(String, Element)>,
    visited: &mut usize,
    cancellation: &AtomicBool,
    visit: &mut impl FnMut(&str, &Element) -> bool,
    path: String,
    element: Element,
) -> Result<(), AdapterError> {
    if cancellation.load(Ordering::SeqCst) {
        return Err(adapter_error("cancelled", "AX traversal was cancelled"));
    }
    *visited += 1;
    if *visited <= MAX_TREE_NODES && visit(&path, &element) {
        push_children(
            stack,
            &path,
            element.elements("AXChildren").unwrap_or_default(),
        );
    }
    Ok(())
}

fn push_children(stack: &mut Vec<(String, Element)>, path: &str, children: Vec<Element>) {
    for (index, child) in children.into_iter().enumerate().rev() {
        stack.push((format!("{path}-{index}"), child));
    }
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
        && ancestor_scope_matches(element, semantic)
}

fn ancestor_scope_matches(element: &Element, semantic: &Map<String, Value>) -> bool {
    let within_role = semantic.get("within_role").and_then(Value::as_str);
    let within_name = semantic.get("within_name").and_then(Value::as_str);
    if within_role.is_none() && within_name.is_none() {
        return true;
    }
    ancestor_fields_match(&collect_ancestor_fields(element), within_role, within_name)
}

fn collect_ancestor_fields(element: &Element) -> Vec<(Option<String>, Option<String>)> {
    let mut ancestors = Vec::new();
    let mut current = element.element("AXParent");
    while let Some(ancestor) = current {
        if ancestors.len() >= MAX_TREE_NODES {
            break;
        }
        ancestors.push((
            ancestor.string("AXRole").map(|role| public_role(&role)),
            ancestor
                .string("AXTitle")
                .or_else(|| ancestor.string("AXDescription")),
        ));
        current = ancestor.element("AXParent");
    }
    ancestors
}

fn ancestor_fields_match(
    ancestors: &[(Option<String>, Option<String>)],
    within_role: Option<&str>,
    within_name: Option<&str>,
) -> bool {
    if within_role.is_none() && within_name.is_none() {
        return true;
    }
    ancestors.iter().any(|(role, name)| {
        within_role.is_none_or(|expected| role.as_deref() == Some(expected))
            && within_name.is_none_or(|expected| name.as_deref() == Some(expected))
    })
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
        "description": public.get("description"),
    })
}

fn public_element(element: &Element) -> Map<String, Value> {
    let mut value = Map::new();
    value.insert("role".to_owned(), public_role_value(element));
    value.insert("name".to_owned(), public_name_value(element));
    value.insert(
        "text".to_owned(),
        optional_string_value(element.string("AXValue")),
    );
    value.insert(
        "identifier".to_owned(),
        optional_string_value(element.string("AXIdentifier")),
    );
    value.insert(
        "description".to_owned(),
        optional_string_value(element.string("AXDescription")),
    );
    if let Ok(bounds) = element.bounds() {
        value.insert("bounds".to_owned(), json!(bounds));
    }
    value
}

fn public_role_value(element: &Element) -> Value {
    element
        .string("AXRole")
        .map(|role| json!(public_role(&role)))
        .unwrap_or(Value::Null)
}

fn public_name_value(element: &Element) -> Value {
    optional_string_value(
        element
            .string("AXTitle")
            .or_else(|| element.string("AXDescription")),
    )
}

fn optional_string_value(value: Option<String>) -> Value {
    value.map(Value::String).unwrap_or(Value::Null)
}

fn public_role(role: &str) -> String {
    named_public_role(role)
        .unwrap_or_else(|| ax_role_fallback(role))
        .to_ascii_lowercase()
}

fn named_public_role(role: &str) -> Option<&'static str> {
    match role {
        "AXButton" | "AXMenuButton" => Some("button"),
        "AXTextField" | "AXTextArea" | "AXComboBox" => Some("textbox"),
        _ => other_named_role(role),
    }
}

fn other_named_role(role: &str) -> Option<&'static str> {
    match role {
        "AXStaticText" => Some("text"),
        "AXCheckBox" => Some("checkbox"),
        "AXRadioButton" => Some("radio"),
        "AXWindow" => Some("window"),
        "AXMenuItem" => Some("menuitem"),
        "AXLink" => Some("link"),
        _ => None,
    }
}

fn ax_role_fallback(role: &str) -> &str {
    role.strip_prefix("AX").unwrap_or(role)
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
    encode_ax_known(window, context, references, value, depth, items)
}

fn encode_ax_known(
    window: &Element,
    context: &AdapterContext,
    references: &mut ReferenceStore,
    value: &CFRetained<CFType>,
    depth: usize,
    items: &mut usize,
) -> Result<Value, AdapterError> {
    if let Some(encoded) = encode_ax_scalar(value)? {
        return Ok(encoded);
    }
    encode_ax_collection_or_native(window, context, references, value, depth, items)
}

fn encode_ax_collection_or_native(
    window: &Element,
    context: &AdapterContext,
    references: &mut ReferenceStore,
    value: &CFRetained<CFType>,
    depth: usize,
    items: &mut usize,
) -> Result<Value, AdapterError> {
    if let Some(encoded) = encode_ax_collection(window, context, references, value, depth, items)? {
        return Ok(encoded);
    }
    encode_ax_native(window, context, references, value)
}

fn encode_ax_scalar(value: &CFRetained<CFType>) -> Result<Option<Value>, AdapterError> {
    if let Some(encoded) = encode_ax_null_string_or_url(value) {
        return Ok(Some(encoded));
    }
    encode_ax_boolean_or_number(value)
}

fn encode_ax_null_string_or_url(value: &CFRetained<CFType>) -> Option<Value> {
    if value.downcast_ref::<CFNull>().is_some() {
        return Some(json!({"type": "null", "value": null}));
    }
    if let Some(value) = value.downcast_ref::<CFString>() {
        return Some(json!({"type": "string", "value": value.to_string()}));
    }
    value
        .downcast_ref::<CFURL>()
        .map(|value| json!({"type": "url", "value": value.string().to_string()}))
}

fn encode_ax_boolean_or_number(value: &CFRetained<CFType>) -> Result<Option<Value>, AdapterError> {
    if let Some(value) = value.downcast_ref::<CFBoolean>() {
        return Ok(Some(json!({"type": "boolean", "value": value.as_bool()})));
    }
    encode_ax_number(value)
}

fn encode_ax_number(value: &CFRetained<CFType>) -> Result<Option<Value>, AdapterError> {
    let Some(value) = value.downcast_ref::<CFNumber>() else {
        return Ok(None);
    };
    let number = value
        .as_i64()
        .map(Value::from)
        .or_else(|| value.as_f64().map(Value::from))
        .ok_or_else(|| adapter_error("raw_value_unsupported", "CFNumber had no JSON value"))?;
    Ok(Some(json!({"type": "number", "value": number})))
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
        1 => encode_ax_point_value(pointer, kind),
        2 => encode_ax_size_value(pointer, kind),
        3 => encode_ax_rect_value(pointer, kind),
        4 => encode_ax_range_value(pointer, kind),
        _ => Err(adapter_error(
            "raw_value_unsupported",
            &format!("unsupported AXValue type {kind}"),
        )),
    }
}

fn encode_ax_point_value(pointer: *const c_void, kind: i32) -> Result<Value, AdapterError> {
    let mut point = CGPoint::ZERO;
    ax_value_into(pointer, kind, &mut point)?;
    Ok(json!({"type": "point", "x": point.x, "y": point.y}))
}

fn encode_ax_size_value(pointer: *const c_void, kind: i32) -> Result<Value, AdapterError> {
    let mut size = CGSize::ZERO;
    ax_value_into(pointer, kind, &mut size)?;
    Ok(json!({"type": "size", "width": size.width, "height": size.height}))
}

fn encode_ax_rect_value(pointer: *const c_void, kind: i32) -> Result<Value, AdapterError> {
    let mut rect = CGRect::ZERO;
    ax_value_into(pointer, kind, &mut rect)?;
    Ok(json!({
        "type": "rect", "x": rect.origin.x, "y": rect.origin.y,
        "width": rect.size.width, "height": rect.size.height,
    }))
}

fn encode_ax_range_value(pointer: *const c_void, kind: i32) -> Result<Value, AdapterError> {
    let mut range = CFRange {
        location: 0,
        length: 0,
    };
    ax_value_into(pointer, kind, &mut range)?;
    encode_ax_range_fields(range)
}

fn encode_ax_range_fields(range: CFRange) -> Result<Value, AdapterError> {
    Ok(json!({
        "type": "range",
        "location": encode_ax_index(range.location, "location")?,
        "length": encode_ax_index(range.length, "length")?,
    }))
}

fn encode_ax_index(value: isize, field: &str) -> Result<u64, AdapterError> {
    u64::try_from(value).map_err(|_| {
        adapter_error(
            "raw_value_unsupported",
            &format!("AX range {field} was negative"),
        )
    })
}

fn ax_value_into<T>(pointer: *const c_void, kind: i32, value: &mut T) -> Result<(), AdapterError> {
    unsafe { AXValueGetValue(pointer, kind, (value as *mut T).cast()) }
        .then_some(())
        .ok_or_else(|| adapter_error("raw_value_unsupported", "AXValue payload was malformed"))
}

fn decode_element_bounds(
    position: CFRetained<CFType>,
    size: CFRetained<CFType>,
) -> Result<WindowBounds, AdapterError> {
    let mut point = CGPoint::ZERO;
    let mut dimensions = CGSize::ZERO;
    require_ax_value(
        &position,
        1,
        &mut point,
        "AX window position or size was not a valid AXValue",
    )?;
    require_ax_value(
        &size,
        2,
        &mut dimensions,
        "AX window position or size was not a valid AXValue",
    )?;
    Ok(WindowBounds {
        x: point.x,
        y: point.y,
        width: dimensions.width,
        height: dimensions.height,
    })
}

fn require_ax_value<T>(
    value: &CFRetained<CFType>,
    kind: i32,
    output: &mut T,
    message: &str,
) -> Result<(), AdapterError> {
    unsafe {
        AXValueGetValue(
            CFRetained::as_ptr(value).as_ptr().cast(),
            kind,
            (output as *mut T).cast(),
        )
    }
    .then_some(())
    .ok_or_else(|| adapter_error("observation_failed", message))
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
    match decode_ax_primitive(kind, value)? {
        Some(decoded) => Ok(Some(decoded)),
        None => decode_ax_textual(kind, value),
    }
}

fn decode_ax_primitive(
    kind: &str,
    value: &Value,
) -> Result<Option<CFRetained<CFType>>, AdapterError> {
    match kind {
        "null" => decode_ax_null().map(Some),
        "boolean" => decode_ax_boolean(value).map(Some),
        "number" => decode_ax_number(value).map(Some),
        _ => Ok(None),
    }
}

fn decode_ax_textual(
    kind: &str,
    value: &Value,
) -> Result<Option<CFRetained<CFType>>, AdapterError> {
    match kind {
        "string" => decode_ax_string(value).map(Some),
        "url" => decode_ax_url(value).map(Some),
        _ => Ok(None),
    }
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
    match decode_ax_point_or_size(kind, value)? {
        Some(decoded) => Ok(Some(decoded)),
        None => decode_ax_rect_or_range(kind, value),
    }
}

fn decode_ax_point_or_size(
    kind: &str,
    value: &Value,
) -> Result<Option<CFRetained<CFType>>, AdapterError> {
    match kind {
        "point" => decode_ax_point(value).map(Some),
        "size" => decode_ax_size(value).map(Some),
        _ => Ok(None),
    }
}

fn decode_ax_rect_or_range(
    kind: &str,
    value: &Value,
) -> Result<Option<CFRetained<CFType>>, AdapterError> {
    match kind {
        "rect" => decode_ax_rect(value).map(Some),
        "range" => decode_ax_range(value).map(Some),
        _ => Ok(None),
    }
}

fn decode_ax_point(value: &Value) -> Result<CFRetained<CFType>, AdapterError> {
    ax_struct(1, &decode_point(value)?)
}

fn decode_ax_size(value: &Value) -> Result<CFRetained<CFType>, AdapterError> {
    ax_struct(2, &decode_size(value)?)
}

fn decode_ax_rect(value: &Value) -> Result<CFRetained<CFType>, AdapterError> {
    ax_struct(3, &CGRect::new(decode_point(value)?, decode_size(value)?))
}

fn decode_point(value: &Value) -> Result<CGPoint, AdapterError> {
    Ok(CGPoint::new(
        required_number(value, "x")?,
        required_number(value, "y")?,
    ))
}

fn decode_size(value: &Value) -> Result<CGSize, AdapterError> {
    Ok(CGSize::new(
        required_number(value, "width")?,
        required_number(value, "height")?,
    ))
}

fn decode_ax_range(value: &Value) -> Result<CFRetained<CFType>, AdapterError> {
    ax_struct(
        4,
        &CFRange {
            location: decode_cf_index(value, "location")?,
            length: decode_cf_index(value, "length")?,
        },
    )
}

fn decode_cf_index(value: &Value, field: &str) -> Result<isize, AdapterError> {
    required_u64(value, field)?.try_into().map_err(|_| {
        adapter_error(
            "raw_value_unsupported",
            &format!("range {field} exceeded CFIndex"),
        )
    })
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
    let values = required_array_entries(value)?;
    let values = decode_ax_entries(context, references, values, depth, items)?;
    Ok(into_type(CFArray::from_retained_objects(&values)))
}

fn required_array_entries(value: &Value) -> Result<&Vec<Value>, AdapterError> {
    value
        .get("value")
        .and_then(Value::as_array)
        .ok_or_else(|| adapter_error("invalid_request", "array value is required"))
}

fn decode_ax_entries(
    context: &AdapterContext,
    references: &ReferenceStore,
    values: &[Value],
    depth: usize,
    items: &mut usize,
) -> Result<Vec<CFRetained<CFType>>, AdapterError> {
    values
        .iter()
        .map(|value| decode_ax_value(context, references, value, depth + 1, items))
        .collect()
}

fn decode_ax_dictionary(
    context: &AdapterContext,
    references: &ReferenceStore,
    value: &Value,
    depth: usize,
    items: &mut usize,
) -> Result<CFRetained<CFType>, AdapterError> {
    let entries = required_dictionary_entries(value)?;
    let keys = entries
        .keys()
        .map(|key| CFString::from_str(key))
        .collect::<Vec<_>>();
    let values = decode_ax_entries(
        context,
        references,
        &entries.values().cloned().collect::<Vec<_>>(),
        depth,
        items,
    )?;
    let key_refs = keys.iter().map(|key| &**key).collect::<Vec<_>>();
    let value_refs = values.iter().map(|value| &**value).collect::<Vec<_>>();
    Ok(into_type(CFDictionary::from_slices(&key_refs, &value_refs)))
}

fn required_dictionary_entries(value: &Value) -> Result<&Map<String, Value>, AdapterError> {
    value
        .get("value")
        .and_then(Value::as_object)
        .ok_or_else(|| adapter_error("invalid_request", "dictionary value is required"))
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
        retain_copied_attribute(copy_attribute(self.0.as_ptr(), name)?)
    }

    fn elements(&self, name: &str) -> Result<Vec<Element>, AdapterError> {
        let value = self.attribute(name)?;
        let array: CFRetained<CFArray<CFType>> =
            // SAFETY: AXWindows and AXChildren are documented arrays of AXUIElement values.
            unsafe { CFRetained::cast_unchecked(value) };
        Ok(elements_from_array(&array))
    }

    pub(crate) fn string(&self, name: &str) -> Option<String> {
        self.attribute(name)
            .ok()?
            .downcast::<CFString>()
            .ok()
            .map(|value| value.to_string())
    }

    fn element(&self, name: &str) -> Option<Element> {
        let value = self.attribute(name).ok()?;
        (unsafe { CFGetTypeID(Some(&*value)) } == unsafe { AXUIElementGetTypeID() }).then(|| {
            let raw = CFRetained::into_raw(value).cast();
            Element(raw)
        })
    }

    pub(crate) fn bounds(&self) -> Result<WindowBounds, AdapterError> {
        decode_element_bounds(self.attribute("AXPosition")?, self.attribute("AXSize")?)
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
        inspect_attribute_settable(self.0.as_ptr(), attribute)
    }

    fn supports_action(&self, requested: &str) -> Result<bool, AdapterError> {
        action_names(self.0.as_ptr(), requested)
            .map(|actions| actions.iter().any(|action| action.to_string() == requested))
    }

    fn set_cf(&self, attribute: &str, value: *const c_void) -> Result<(), AdapterError> {
        require_ax_success(
            set_attribute(self.0.as_ptr(), attribute, value),
            "set attribute",
            attribute,
        )
    }

    fn perform(&self, action: &str) -> Result<(), AdapterError> {
        crate::oracle::record("ax_perform_attempt", json!({ "action": action }));
        require_ax_success(
            perform_action(self.0.as_ptr(), action),
            "perform action",
            action,
        )
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

fn inspect_attribute_settable(
    element: *const c_void,
    attribute: &str,
) -> Result<bool, AdapterError> {
    let name = CFString::from_str(attribute);
    let mut settable = false;
    let status = unsafe {
        AXUIElementIsAttributeSettable(element, CFRetained::as_ptr(&name).as_ptr(), &mut settable)
    };
    require_ax_success(status, "inspect settable attribute", attribute).map(|()| settable)
}

fn copy_attribute(element: *const c_void, name: &str) -> Result<*const c_void, AdapterError> {
    let name = CFString::from_str(name);
    let mut raw = ptr::null();
    let status = unsafe {
        AXUIElementCopyAttributeValue(element, CFRetained::as_ptr(&name).as_ptr(), &mut raw)
    };
    require_ax_success(status, "copy attribute", &name.to_string()).map(|()| raw)
}

fn retain_copied_attribute(raw: *const c_void) -> Result<CFRetained<CFType>, AdapterError> {
    let raw: NonNull<CFType> = NonNull::new(raw.cast_mut().cast())
        .ok_or_else(|| adapter_error("observation_failed", "AX returned a null attribute"))?;
    // SAFETY: CopyAttributeValue returned a +1 retained CF object.
    Ok(unsafe { CFRetained::from_raw(raw) })
}

fn elements_from_array(array: &CFArray<CFType>) -> Vec<Element> {
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
        .collect()
}

fn action_names(
    element: *const c_void,
    requested: &str,
) -> Result<CFRetained<CFArray<CFString>>, AdapterError> {
    let mut raw = ptr::null();
    let status = unsafe { AXUIElementCopyActionNames(element, &mut raw) };
    require_ax_success(status, "copy action names", requested)?;
    let raw: NonNull<CFArray<CFString>> = NonNull::new(raw.cast_mut().cast())
        .ok_or_else(|| adapter_error("observation_failed", "AX returned null action names"))?;
    // SAFETY: CopyActionNames returns a +1 retained CFArray of CFString values.
    Ok(unsafe { CFRetained::from_raw(raw) })
}

fn set_attribute(element: *const c_void, attribute: &str, value: *const c_void) -> i32 {
    let name = CFString::from_str(attribute);
    unsafe { AXUIElementSetAttributeValue(element, CFRetained::as_ptr(&name).as_ptr(), value) }
}

fn perform_action(element: *const c_void, action: &str) -> i32 {
    let action_name = CFString::from_str(action);
    unsafe { AXUIElementPerformAction(element, CFRetained::as_ptr(&action_name).as_ptr()) }
}

fn require_ax_success(status: i32, operation: &str, value: &str) -> Result<(), AdapterError> {
    (status == AX_SUCCESS)
        .then_some(())
        .ok_or_else(|| ax_error(status, operation, value.to_owned()))
}

fn ax_error(status: i32, operation: &str, value: String) -> AdapterError {
    let classification = ax_error_class(status);
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

fn ax_error_class(status: i32) -> &'static str {
    if status == -25211 {
        "permission_required"
    } else {
        "backend_rejected"
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
    fn ancestor_scope_is_exact_and_requires_a_proper_ancestor() {
        let ancestors = vec![
            (Some("group".to_owned()), Some("Primary".to_owned())),
            (Some("window".to_owned()), Some("Fixture".to_owned())),
        ];
        assert!(ancestor_fields_match(
            &ancestors,
            Some("group"),
            Some("Primary")
        ));
        assert!(ancestor_fields_match(&ancestors, Some("window"), None));
        assert!(!ancestor_fields_match(
            &ancestors,
            Some("group"),
            Some("Secondary")
        ));
        assert!(!ancestor_fields_match(&[], Some("group"), Some("Primary")));
        assert!(ancestor_fields_match(&ancestors, None, None));
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

    fn test_context(session: &str) -> AdapterContext {
        AdapterContext {
            session_id: session.to_owned(),
            target_id: "macos_test".to_owned(),
            target_generation: 1,
            action_sequence: 1,
            reference_namespace: "n_test".to_owned(),
            reference_epoch: 1,
            frame_token: None,
            mode: manuvra_runtime::ExecutionMode::Background,
            deadline: std::time::Instant::now() + std::time::Duration::from_secs(1),
        }
    }

    fn test_record() -> WindowRecord {
        WindowRecord {
            descriptor: manuvra_runtime::TargetDescriptor {
                target_id: "macos_test".to_owned(),
                generation: 1,
                kind: "macos".to_owned(),
                owner: "Fixture".to_owned(),
                title: Some("Window".to_owned()),
                capabilities: Vec::new(),
            },
            snapshot: crate::discovery::WindowSnapshot {
                pid: i32::MAX,
                window_id: 1,
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
        }
    }

    fn operation(command: &str, input: Value) -> AdapterOperation {
        AdapterOperation::new(command.to_owned(), input)
    }

    #[test]
    fn public_roles_and_locator_kinds_keep_native_adapter_results() {
        assert_eq!(public_role("AXButton"), "button");
        assert_eq!(public_role("AXMenuButton"), "button");
        assert_eq!(public_role("AXTextField"), "textbox");
        assert_eq!(public_role("AXTextArea"), "textbox");
        assert_eq!(public_role("AXComboBox"), "textbox");
        assert_eq!(public_role("AXStaticText"), "text");
        assert_eq!(public_role("AXCheckBox"), "checkbox");
        assert_eq!(public_role("AXRadioButton"), "radio");
        assert_eq!(public_role("AXWindow"), "window");
        assert_eq!(public_role("AXMenuItem"), "menuitem");
        assert_eq!(public_role("AXLink"), "link");
        assert_eq!(public_role("AXGroup"), "group");
        assert_eq!(public_role("Custom"), "custom");

        let window = Element::application(std::process::id() as i32).unwrap();
        let context = test_context("s_locator");
        let references = ReferenceStore::default();
        assert_eq!(
            resolve_locator(
                &window,
                &context,
                &operation(
                    "action.click",
                    json!({"locator": {"kind": "point", "x": 1, "y": 1}})
                ),
                &references,
            )
            .err()
            .unwrap()
            .code,
            "foreground_required"
        );
        assert_eq!(
            resolve_locator(
                &window,
                &context,
                &operation("action.click", json!({"locator": {"kind": "xpath"}})),
                &references,
            )
            .err()
            .unwrap()
            .code,
            "invalid_request"
        );
        assert_eq!(
            resolve_locator(
                &window,
                &context,
                &operation("action.click", json!({})),
                &references,
            )
            .err()
            .unwrap()
            .code,
            "invalid_request"
        );
        assert_eq!(
            resolve_locator(
                &window,
                &context,
                &operation(
                    "action.click",
                    json!({"locator": {"kind": "ref", "ref": "e_n_test_1_missing"}})
                ),
                &references,
            )
            .err()
            .unwrap()
            .code,
            "element_stale"
        );
        assert_eq!(
            unique_semantic_match(Vec::new()).err().unwrap().code,
            "element_not_found"
        );
        assert_eq!(
            unique_semantic_match(vec![window.clone(), window.clone()])
                .err()
                .unwrap()
                .code,
            "ambiguous_target"
        );
        assert!(unique_semantic_match(vec![window]).is_ok());
    }

    #[test]
    fn mutation_prepare_and_invoke_keep_locator_ax_and_background_results() {
        let window = Element::application(std::process::id() as i32).unwrap();
        let context = test_context("s_mutation");
        let mut references = ReferenceStore::default();
        let (_, reference) = references.issue(&context, &window);
        let point_click = operation(
            "action.click",
            json!({"locator": {"kind": "point", "x": 1, "y": 1}, "button": "left", "count": 1}),
        );
        assert!(
            mutation_element(&window, &context, &point_click, &references)
                .unwrap()
                .is_none()
        );
        assert_eq!(
            background_click_policy(&point_click).unwrap_err().code,
            "foreground_required"
        );
        let right_click = operation(
            "action.click",
            json!({"locator": {"kind": "ref", "ref": reference}, "button": "right", "count": 1}),
        );
        assert_eq!(
            background_click_policy(&right_click).unwrap_err().code,
            "foreground_required"
        );
        let left_click = operation(
            "action.click",
            json!({"locator": {"kind": "ref", "ref": reference}, "button": "left", "count": 1}),
        );
        assert!(background_click_policy(&left_click).is_ok());
        let press = operation("action.press", json!({"key": "enter"}));
        assert!(
            mutation_element(&window, &context, &press, &references)
                .unwrap()
                .is_none()
        );
        let raw = operation(
            "raw.ax.set",
            json!({"ref": reference, "attribute": "AXValue", "value": {"type": "string", "value": "x"}}),
        );
        assert!(
            mutation_element(&window, &context, &raw, &references)
                .unwrap()
                .is_some()
        );

        let cancelled = std::sync::Arc::new(AtomicBool::new(true));
        let reply = invoke(
            &test_record(),
            &context,
            &operation("observe.tree", json!({})),
            cancelled,
            &mut references,
        );
        assert_eq!(reply.error.as_ref().unwrap().code, "cancelled");
        assert_eq!(reply.delivery, manuvra_runtime::AdapterDelivery::Rejected);

        let live = std::sync::Arc::new(AtomicBool::new(false));
        let missing = invoke(
            &test_record(),
            &context,
            &operation("observe.tree", json!({})),
            live.clone(),
            &mut references,
        );
        assert_eq!(missing.error.as_ref().unwrap().code, "target_not_found");

        let unsupported = invoke(
            &test_record(),
            &context,
            &operation("action.click", json!({})),
            live.clone(),
            &mut references,
        );
        assert!(matches!(
            unsupported.error.as_ref().unwrap().code.as_str(),
            "target_not_found" | "capability_unavailable"
        ));

        let prepared = PreparedAx { element: None };
        let click = invoke_prepared(
            &test_record(),
            &context,
            &left_click,
            &prepared,
            live.clone(),
            &references,
        );
        assert_eq!(click.error.as_ref().unwrap().code, "element_not_found");
        let unknown = invoke_prepared(
            &test_record(),
            &context,
            &operation("observe.tree", json!({})),
            &prepared,
            live,
            &references,
        );
        assert_eq!(
            unknown.error.as_ref().unwrap().code,
            "capability_unavailable"
        );
        assert_eq!(
            prepare_mutation(&test_record(), &context, &left_click, &references)
                .err()
                .unwrap()
                .code,
            "target_not_found"
        );
        assert_eq!(query_limit(&operation("observe.query", json!({}))), 5);
        assert_eq!(
            query_limit(&operation("observe.query", json!({"limit": 9}))),
            5
        );
        assert_eq!(
            split_query_matches(vec![json!(1), json!(2), json!(3)], 2)["overflow"]
                .as_array()
                .unwrap()
                .len(),
            1
        );
        assert_eq!(
            required_semantic(&operation("observe.query", json!({})))
                .unwrap_err()
                .code,
            "invalid_request"
        );
        let empty = split_query_matches(Vec::new(), 5);
        assert_eq!(empty["matches"], json!([]));
        assert_eq!(typed_ax_value(&window, "next", true), "next");
        assert_eq!(ax_error_class(-25211), "permission_required");
        assert_eq!(ax_error_class(-25205), "backend_rejected");
        assert!(range_equals(
            CFRange {
                location: 4,
                length: 0
            },
            text_end_range("A😀Z")
        ));
        assert!(!is_window_role(Some("AXButton")));
        assert!(is_window_role(Some("AXDialog")));
        assert_eq!(
            select_unique_window(
                Vec::new(),
                WindowBounds {
                    x: 0.0,
                    y: 0.0,
                    width: 1.0,
                    height: 1.0,
                },
                &[]
            )
            .err()
            .unwrap()
            .code,
            "target_not_found"
        );
        let bounds = WindowBounds {
            x: 0.0,
            y: 0.0,
            width: 1.0,
            height: 1.0,
        };
        assert_eq!(
            select_unique_window(
                vec![window.clone(), window.clone()],
                bounds,
                &[(window.clone(), bounds), (window.clone(), bounds)]
            )
            .err()
            .unwrap()
            .code,
            "capability_unavailable"
        );
        let live = std::sync::Arc::new(AtomicBool::new(false));
        let empty_semantic = Map::new();
        assert!(semantic_matches(&window, &empty_semantic));
        assert!(ancestor_scope_matches(&window, &empty_semantic));
        let press = operation("action.press", json!({"key": "enter"}));
        assert!(validate_background_mutation(&press, None).is_ok());
        assert_eq!(
            validate_background_mutation(&point_click, None)
                .err()
                .unwrap()
                .code,
            "foreground_required"
        );
        assert_eq!(
            validate_background_mutation(&raw, None).err().unwrap().code,
            "element_not_found"
        );
        let tree = ax_observe(
            &window,
            &context,
            &operation("observe.tree", json!({})),
            live.clone(),
            &mut references,
        );
        assert!(tree.is_ok() || tree.err().map(|error| error.code).is_some());
        assert_eq!(
            query(
                &window,
                &context,
                &operation("observe.query", json!({})),
                live.clone(),
                &mut references,
            )
            .err()
            .unwrap()
            .code,
            "invalid_request"
        );
        let queried = query(
            &window,
            &context,
            &operation("observe.query", json!({"semantic": {}})),
            live,
            &mut references,
        );
        assert!(queried.is_ok() || queried.err().is_some());
        let _ = application_is_frontmost(i32::MAX);
        let _ = application_is_hidden(i32::MAX);
        assert!(exact_window_is_main(&test_record()).is_err());
        assert!(exact_window_is_focused(&test_record()).is_err());
        assert!(exact_window_is_minimized(&test_record()).is_err());
        assert!(observer_elements(&test_record()).is_err());
        assert!(focus_exact_window(&test_record()).is_err());
        let _ = focused_window_snapshot();
        assert_eq!(
            raw_get(
                &window,
                &context,
                &operation("raw.ax.get", json!({"attribute": "AXRole"})),
                &mut references,
            )
            .err()
            .unwrap()
            .code,
            "invalid_request"
        );
        let app = Element::application(std::process::id() as i32).unwrap();
        let _ = collect_observer_elements(app.clone(), app.clone());
        let _ = collect_ancestor_fields(&app);
        let _ = app.supports_action("AXPress");
        let _ = require_settable_value(&app);
        let _ = validate_background_set(
            &operation("raw.ax.set", json!({"attribute": "AXValue"})),
            &app,
        );
        let _ = validate_background_perform(
            &operation("raw.ax.perform", json!({"action": "AXPress"})),
            &app,
        );
        let _ = type_if_settable(&app, &operation("action.type", json!({"text": "x"})));
        let _ = selected_text_range(&app);
        let _ = collapse_selection_at_text_end(&app, "x");
        assert!(maybe_collapse_typed_selection(&app, "x", false).is_ok());
        let _ = maybe_collapse_typed_selection(&app, "x", true);
        let _ = constrain_background_or_raw(
            &context,
            &operation("action.press", json!({"key": "enter"})),
            None,
        );
        let _ =
            decode_prepared_raw_set(&context, &operation("action.click", json!({})), &references);
        let _ = validate_background_type(
            &operation(
                "action.type",
                json!({"locator": {"kind": "point"}, "text": "x"}),
            ),
            None,
        );
    }
}
