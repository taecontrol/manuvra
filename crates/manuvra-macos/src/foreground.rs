use crate::ax::{self, adapter_error, rejected_error};
use crate::discovery::WindowRecord;
use manuvra_runtime::{
    AdapterContext, AdapterDelivery, AdapterError, AdapterOperation, AdapterReply,
};
use objc2_app_kit::{NSApplicationActivationOptions, NSRunningApplication};
use objc2_core_foundation::{CFRetained, CGPoint};
use objc2_core_graphics::{
    CGEvent, CGEventFlags, CGEventSource, CGEventSourceStateID, CGEventTapLocation, CGEventType,
    CGMouseButton, CGScrollEventUnit,
};
use serde_json::{Value, json};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

pub(crate) fn invoke_prepared(
    record: &WindowRecord,
    context: &AdapterContext,
    operation: &AdapterOperation,
    element: Option<&ax::Element>,
    cancellation: Arc<AtomicBool>,
) -> AdapterReply {
    invoke_with_element(record, context, operation, element, cancellation)
}

fn invoke_with_element(
    record: &WindowRecord,
    context: &AdapterContext,
    operation: &AdapterOperation,
    element: Option<&ax::Element>,
    cancellation: Arc<AtomicBool>,
) -> AdapterReply {
    if let Err((error, activated)) =
        acquire_exact_foreground(record, context.deadline, &cancellation)
    {
        return foreground_acquisition_failure(error, activated);
    }
    dispatch_owned_foreground(record, context, operation, element, cancellation)
}

fn dispatch_owned_foreground(
    record: &WindowRecord,
    context: &AdapterContext,
    operation: &AdapterOperation,
    element: Option<&ax::Element>,
    cancellation: Arc<AtomicBool>,
) -> AdapterReply {
    if let Some(reply) = refuse_owned_foreground(record, context, &cancellation) {
        return reply;
    }
    verify_post_dispatch_ownership(
        record,
        dispatch_foreground_command(record, context, operation, element, &cancellation),
    )
}

fn refuse_owned_foreground(
    record: &WindowRecord,
    context: &AdapterContext,
    cancellation: &AtomicBool,
) -> Option<AdapterReply> {
    if !crate::permissions::PermissionSnapshot::current().post_event {
        return Some(permission_lost_after_foreground_ownership());
    }
    crate::oracle::barrier(
        "after_foreground_proof_before_input",
        context.deadline,
        cancellation,
    );
    lost_before_input(record, cancellation)
}

fn lost_before_input(record: &WindowRecord, cancellation: &AtomicBool) -> Option<AdapterReply> {
    if cancellation.load(Ordering::SeqCst) {
        return Some(interrupted_before_dispatch(
            "operation was cancelled after foreground activation",
        ));
    }
    (!owns_exact(record)).then(|| {
        interrupted_before_dispatch(
            "foreground ownership was lost after proof and before input dispatch",
        )
    })
}

fn dispatch_foreground_command(
    record: &WindowRecord,
    context: &AdapterContext,
    operation: &AdapterOperation,
    element: Option<&ax::Element>,
    cancellation: &AtomicBool,
) -> AdapterReply {
    match operation.command.as_str() {
        "action.click" => click(record, context, operation, element, cancellation),
        "action.type" => type_text(record, context, operation, element, cancellation),
        "action.press" => press(record, context, operation, element, cancellation),
        "action.scroll" => scroll(record, context, operation, element, cancellation),
        _ => rejected_error(adapter_error(
            "capability_unavailable",
            "operation has no foreground implementation",
        )),
    }
}

fn permission_lost_after_foreground_ownership() -> AdapterReply {
    let mut error = crate::permission_error(crate::permissions::MissingPermission::PostEvent);
    error.code = "interrupted".to_owned();
    error.message = Some(
        "Post Event permission was revoked after foreground ownership was acquired; no input event was dispatched. Observe the target, re-enable System Settings > Privacy & Security > Accessibility, then run manuvra doctor"
            .to_owned(),
    );
    let mut reply = AdapterReply::confirmed(Value::Null, None);
    reply.delivery = AdapterDelivery::Unknown;
    reply.interrupted = true;
    reply.error = Some(error);
    reply
}

fn verify_post_dispatch_ownership(record: &WindowRecord, mut reply: AdapterReply) -> AdapterReply {
    if reply.delivery == AdapterDelivery::Confirmed && !reply.interrupted && !owns_exact(record) {
        reply.interrupted = true;
        reply.error = Some(adapter_error(
            "interrupted",
            "the exact target lost foreground ownership after input dispatch",
        ));
    }
    reply
}

fn foreground_acquisition_failure(error: AdapterError, activated: bool) -> AdapterReply {
    if !activated || error.code == "cancelled" {
        return rejected_error(error);
    }
    let mut reply = AdapterReply::confirmed(Value::Null, None);
    reply.delivery = AdapterDelivery::Unknown;
    reply.error = Some(error);
    reply
}

fn acquire_exact_foreground(
    record: &WindowRecord,
    deadline: Instant,
    cancellation: &AtomicBool,
) -> Result<(), (AdapterError, bool)> {
    if cancellation.load(Ordering::SeqCst) {
        return Err((
            adapter_error("cancelled", "operation was cancelled before activation"),
            false,
        ));
    }
    activate_or_keep_foreground(record, deadline, cancellation)
}

fn activate_or_keep_foreground(
    record: &WindowRecord,
    deadline: Instant,
    cancellation: &AtomicBool,
) -> Result<(), (AdapterError, bool)> {
    if owns_exact(record) {
        crate::oracle::barrier("during_foreground_ownership", deadline, cancellation);
        return Ok(());
    }
    activate_and_prove(record, deadline, cancellation)
}

fn activate_and_prove(
    record: &WindowRecord,
    deadline: Instant,
    cancellation: &AtomicBool,
) -> Result<(), (AdapterError, bool)> {
    activate_target(record)?;
    prove_after_activation(record, deadline, cancellation)
}

fn prove_after_activation(
    record: &WindowRecord,
    deadline: Instant,
    cancellation: &AtomicBool,
) -> Result<(), (AdapterError, bool)> {
    ax::focus_exact_window(record).map_err(|error| (error, true))?;
    wait_after_focus(record, deadline, cancellation)
}

fn wait_after_focus(
    record: &WindowRecord,
    deadline: Instant,
    cancellation: &AtomicBool,
) -> Result<(), (AdapterError, bool)> {
    crate::oracle::barrier("during_foreground_ownership", deadline, cancellation);
    wait_for_foreground_proof(record, deadline, cancellation)
}

fn activate_target(record: &WindowRecord) -> Result<(), (AdapterError, bool)> {
    let application = running_application(record)?;
    ensure_foreground_visible(record, &application)?;
    application
        .activateWithOptions(NSApplicationActivationOptions::empty())
        .then_some(())
        .ok_or_else(|| {
            (
                adapter_error(
                    "dispatch_failed",
                    "AppKit rejected target application activation",
                ),
                false,
            )
        })
}

fn running_application(
    record: &WindowRecord,
) -> Result<objc2::rc::Retained<NSRunningApplication>, (AdapterError, bool)> {
    NSRunningApplication::runningApplicationWithProcessIdentifier(record.snapshot.pid)
        .ok_or_else(|| {
            adapter_error(
                "target_not_found",
                "target application is no longer running",
            )
        })
        .map_err(|error| (error, false))
}

fn ensure_foreground_visible(
    record: &WindowRecord,
    application: &NSRunningApplication,
) -> Result<(), (AdapterError, bool)> {
    if foreground_target_is_hidden(record, application.isHidden()) {
        Err((
            adapter_error(
                "capability_unavailable",
                "the target application is hidden; CP-07 never unhides applications implicitly",
            ),
            false,
        ))
    } else {
        Ok(())
    }
}

fn foreground_target_is_hidden(record: &WindowRecord, application_hidden: bool) -> bool {
    application_hidden
        || ax::application_is_hidden(record.snapshot.pid).unwrap_or(false)
        || offscreen_and_not_minimized(record)
}

fn offscreen_and_not_minimized(record: &WindowRecord) -> bool {
    !record.snapshot.is_on_screen && !ax::exact_window_is_minimized(record).unwrap_or(false)
}

fn wait_for_foreground_proof(
    record: &WindowRecord,
    deadline: Instant,
    cancellation: &AtomicBool,
) -> Result<(), (AdapterError, bool)> {
    let proof_deadline = deadline.min(Instant::now() + Duration::from_millis(500));
    while Instant::now() < proof_deadline {
        if let Some(result) = foreground_proof_tick(record, cancellation) {
            return result;
        }
        thread::park_timeout(Duration::from_millis(5));
    }
    Err((
        adapter_error(
            "observation_failed",
            "the exact pinned window did not become frontmost before input dispatch",
        ),
        true,
    ))
}

fn foreground_proof_tick(
    record: &WindowRecord,
    cancellation: &AtomicBool,
) -> Option<Result<(), (AdapterError, bool)>> {
    if cancellation.load(Ordering::SeqCst) {
        return Some(Err((
            adapter_error(
                "cancelled",
                "operation was cancelled while establishing foreground ownership",
            ),
            true,
        )));
    }
    foreground_proof_outcome(foreground_owned(record))
}

fn foreground_proof_outcome(
    owned: Result<bool, (AdapterError, bool)>,
) -> Option<Result<(), (AdapterError, bool)>> {
    match owned {
        Ok(true) => Some(Ok(())),
        Ok(false) => None,
        Err(error) => Some(Err(error)),
    }
}

fn foreground_owned(record: &WindowRecord) -> Result<bool, (AdapterError, bool)> {
    and_proof_results(
        ax::application_is_frontmost(record.snapshot.pid).map_err(|error| (error, true)),
        exact_window_owned(record),
    )
}

fn exact_window_owned(record: &WindowRecord) -> Result<bool, (AdapterError, bool)> {
    and_proof_results(
        ax::exact_window_is_main(record).map_err(|error| (error, true)),
        ax::exact_window_is_focused(record).map_err(|error| (error, true)),
    )
}

fn and_proof_results(
    first: Result<bool, (AdapterError, bool)>,
    second: Result<bool, (AdapterError, bool)>,
) -> Result<bool, (AdapterError, bool)> {
    let first = first?;
    let second = second?;
    Ok(first && second)
}

pub(crate) fn owns_exact(record: &WindowRecord) -> bool {
    ax::application_is_frontmost(record.snapshot.pid).unwrap_or(false)
        && ax::exact_window_is_main(record).unwrap_or(false)
        && ax::exact_window_is_focused(record).unwrap_or(false)
}

fn click(
    record: &WindowRecord,
    context: &AdapterContext,
    operation: &AdapterOperation,
    prepared_element: Option<&ax::Element>,
    cancellation: &AtomicBool,
) -> AdapterReply {
    let point = match click_point(record, context, operation, prepared_element) {
        Ok(point) => point,
        Err(error) => return rejected_error(error),
    };
    dispatch_click_events(record, operation, point, cancellation)
}

fn dispatch_click_events(
    record: &WindowRecord,
    operation: &AdapterOperation,
    point: (f64, f64),
    cancellation: &AtomicBool,
) -> AdapterReply {
    let button = mouse_button(operation.input.get("button").and_then(Value::as_str));
    let (down_type, up_type) = mouse_event_types(button);
    match mouse_events(point, button, down_type, up_type) {
        Ok(events) => dispatch_mouse_clicks(
            record,
            cancellation,
            &events.0,
            &events.1,
            click_count(operation),
        ),
        Err(error) => rejected_error(error),
    }
}

fn click_count(operation: &AdapterOperation) -> u64 {
    operation
        .input
        .get("count")
        .and_then(Value::as_u64)
        .unwrap_or(1)
        .min(3)
}

fn click_point(
    record: &WindowRecord,
    context: &AdapterContext,
    operation: &AdapterOperation,
    prepared_element: Option<&ax::Element>,
) -> Result<(f64, f64), AdapterError> {
    if locator_kind(operation) == Some("point") {
        return prepared_point(operation);
    }
    operation_element(record, context, operation, prepared_element)?
        .bounds()
        .map(center)
}

fn center(bounds: crate::discovery::WindowBounds) -> (f64, f64) {
    (
        bounds.x + bounds.width / 2.0,
        bounds.y + bounds.height / 2.0,
    )
}

fn mouse_button(button: Option<&str>) -> CGMouseButton {
    match button {
        Some("right") => CGMouseButton::Right,
        Some("middle") => CGMouseButton::Center,
        _ => CGMouseButton::Left,
    }
}

fn mouse_event_types(button: CGMouseButton) -> (CGEventType, CGEventType) {
    match button {
        CGMouseButton::Right => (CGEventType::RightMouseDown, CGEventType::RightMouseUp),
        CGMouseButton::Center => (CGEventType::OtherMouseDown, CGEventType::OtherMouseUp),
        _ => (CGEventType::LeftMouseDown, CGEventType::LeftMouseUp),
    }
}

fn mouse_events(
    point: (f64, f64),
    button: CGMouseButton,
    down_type: CGEventType,
    up_type: CGEventType,
) -> Result<(CFRetained<CGEvent>, CFRetained<CGEvent>), AdapterError> {
    let point = CGPoint::new(point.0, point.1);
    let down = CGEvent::new_mouse_event(None, down_type, point, button)
        .ok_or_else(|| adapter_error("dispatch_failed", "could not create mouse-down event"))?;
    let up = CGEvent::new_mouse_event(None, up_type, point, button)
        .ok_or_else(|| adapter_error("dispatch_failed", "could not create mouse-up event"))?;
    Ok((down, up))
}

fn dispatch_mouse_clicks(
    record: &WindowRecord,
    cancellation: &AtomicBool,
    down: &CGEvent,
    up: &CGEvent,
    count: u64,
) -> AdapterReply {
    for _ in 0..count {
        if let Some(reply) = post_owned_event_pair(
            record,
            cancellation,
            "mouse_down",
            down,
            "mouse_up",
            up,
            "mouse",
        ) {
            return reply;
        }
    }
    AdapterReply::confirmed(
        json!({"dispatched": true, "effective_mode": "foreground", "count": count}),
        None,
    )
}

fn type_text(
    record: &WindowRecord,
    context: &AdapterContext,
    operation: &AdapterOperation,
    prepared_element: Option<&ax::Element>,
    cancellation: &AtomicBool,
) -> AdapterReply {
    let click = click_for_type(record, context, operation, prepared_element, cancellation);
    if click.delivery != AdapterDelivery::Confirmed || click.interrupted {
        return click;
    }
    type_after_click(record, context, operation, prepared_element, cancellation)
}

fn click_for_type(
    record: &WindowRecord,
    context: &AdapterContext,
    operation: &AdapterOperation,
    prepared_element: Option<&ax::Element>,
    cancellation: &AtomicBool,
) -> AdapterReply {
    let mut click_operation = operation.clone();
    click_operation.input["button"] = Value::String("left".to_owned());
    click_operation.input["count"] = Value::from(1);
    click(
        record,
        context,
        &click_operation,
        prepared_element,
        cancellation,
    )
}

fn type_after_click(
    record: &WindowRecord,
    context: &AdapterContext,
    operation: &AdapterOperation,
    prepared_element: Option<&ax::Element>,
    cancellation: &AtomicBool,
) -> AdapterReply {
    focused_text_element(record, context, operation, prepared_element, cancellation).map_or_else(
        |error| {
            interrupted_after_dispatch(
                error
                    .message
                    .as_deref()
                    .unwrap_or("typed element could not be focused after mouse dispatch"),
            )
        },
        |element| typed_after_focus(record, operation, element.as_ref(), cancellation),
    )
}

fn typed_after_focus(
    record: &WindowRecord,
    operation: &AdapterOperation,
    element: Option<&ax::Element>,
    cancellation: &AtomicBool,
) -> AdapterReply {
    ax_text_reply(element, operation)
        .unwrap_or_else(|| global_text_reply(record, operation, cancellation))
}

fn focused_text_element(
    record: &WindowRecord,
    context: &AdapterContext,
    operation: &AdapterOperation,
    prepared_element: Option<&ax::Element>,
    cancellation: &AtomicBool,
) -> Result<Option<ax::Element>, AdapterError> {
    if locator_kind(operation) == Some("point") {
        return Ok(None);
    }
    let element = operation_element(record, context, operation, prepared_element)?;
    if !wait_for_element_focus(&element, context.deadline, cancellation) {
        return Err(adapter_error(
            "interrupted",
            "the exact typed element did not acquire focus after mouse dispatch",
        ));
    }
    Ok(Some(element))
}

fn ax_text_reply(
    element: Option<&ax::Element>,
    operation: &AdapterOperation,
) -> Option<AdapterReply> {
    typed_element_reply(element?, operation)
}

fn typed_element_reply(
    element: &ax::Element,
    operation: &AdapterOperation,
) -> Option<AdapterReply> {
    ax::type_if_settable(element, operation).map_or_else(
        |error| {
            Some(interrupted_after_dispatch(
                error
                    .message
                    .as_deref()
                    .unwrap_or("foreground AX text mutation failed after focus dispatch"),
            ))
        },
        typed_ok,
    )
}

fn typed_ok(response: Option<Value>) -> Option<AdapterReply> {
    let mut response = response?;
    response["effective_mode"] = Value::String("foreground".to_owned());
    response["dispatch"] = Value::String("AXValue".to_owned());
    Some(AdapterReply::confirmed(response, None))
}

fn global_text_reply(
    record: &WindowRecord,
    operation: &AdapterOperation,
    cancellation: &AtomicBool,
) -> AdapterReply {
    if let Some(reply) = select_all_if_replace(record, operation, cancellation) {
        return reply;
    }
    typed_unicode_reply(record, typed_text(operation), cancellation)
}

fn select_all_if_replace(
    record: &WindowRecord,
    operation: &AdapterOperation,
    cancellation: &AtomicBool,
) -> Option<AdapterReply> {
    if !operation
        .input
        .get("replace")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        return None;
    }
    let selected = post_key_code(record, 0, Some(CGEventFlags::MaskCommand), cancellation);
    (selected.interrupted || selected.delivery != AdapterDelivery::Confirmed).then_some(selected)
}

fn typed_text(operation: &AdapterOperation) -> &str {
    operation
        .input
        .get("text")
        .and_then(Value::as_str)
        .unwrap_or_default()
}

fn typed_unicode_reply(
    record: &WindowRecord,
    text: &str,
    cancellation: &AtomicBool,
) -> AdapterReply {
    let mut reply = post_unicode(record, text, cancellation);
    reply.response = json!({
        "dispatched": true,
        "effective_mode": "foreground",
        "characters": text.chars().count(),
    });
    reply
}

fn wait_for_element_focus(
    element: &ax::Element,
    deadline: Instant,
    cancellation: &AtomicBool,
) -> bool {
    let focus_deadline = deadline.min(Instant::now() + Duration::from_millis(500));
    while Instant::now() < focus_deadline && !cancellation.load(Ordering::SeqCst) {
        if element.is_focused() {
            return true;
        }
        thread::park_timeout(Duration::from_millis(5));
    }
    false
}

fn press(
    record: &WindowRecord,
    context: &AdapterContext,
    operation: &AdapterOperation,
    prepared_element: Option<&ax::Element>,
    cancellation: &AtomicBool,
) -> AdapterReply {
    if let Some(reply) =
        focus_press_target(record, context, operation, prepared_element, cancellation)
    {
        return reply;
    }
    dispatch_press_key(record, press_key(operation), cancellation)
}

fn press_key(operation: &AdapterOperation) -> &str {
    operation
        .input
        .get("key")
        .and_then(Value::as_str)
        .unwrap_or_default()
}

fn focus_press_target(
    record: &WindowRecord,
    context: &AdapterContext,
    operation: &AdapterOperation,
    prepared_element: Option<&ax::Element>,
    cancellation: &AtomicBool,
) -> Option<AdapterReply> {
    if operation.input.get("locator").is_none() || locator_kind(operation) == Some("point") {
        return None;
    }
    focus_explicit_key_target(record, context, operation, prepared_element, cancellation)
}

fn focus_explicit_key_target(
    record: &WindowRecord,
    context: &AdapterContext,
    operation: &AdapterOperation,
    prepared_element: Option<&ax::Element>,
    cancellation: &AtomicBool,
) -> Option<AdapterReply> {
    let element = match operation_element(record, context, operation, prepared_element) {
        Ok(element) => element,
        Err(error) => return Some(rejected_error(error)),
    };
    crate::oracle::record("ax_focus_set_attempt", Value::Null);
    if let Err(error) = element.set_bool("AXFocused", true) {
        return Some(rejected_error(error));
    }
    crate::oracle::record("ax_focus_set", Value::Null);
    lost_after_key_focus(record, cancellation)
}

fn lost_after_key_focus(record: &WindowRecord, cancellation: &AtomicBool) -> Option<AdapterReply> {
    (cancellation.load(Ordering::SeqCst) || !owns_exact(record)).then(|| {
        interrupted_after_dispatch(
            "foreground ownership was lost after focusing the explicit key target",
        )
    })
}

fn dispatch_press_key(record: &WindowRecord, key: &str, cancellation: &AtomicBool) -> AdapterReply {
    if let Some(code) = key_code(key) {
        return keyed_reply(post_key_code(record, code, None, cancellation), key);
    }
    unicode_press_key(record, key, cancellation)
}

fn unicode_press_key(record: &WindowRecord, key: &str, cancellation: &AtomicBool) -> AdapterReply {
    if key.chars().count() == 1 {
        keyed_reply(post_unicode(record, key, cancellation), key)
    } else {
        rejected_error(adapter_error(
            "capability_unavailable",
            &format!("foreground key {key:?} is not mapped"),
        ))
    }
}

fn keyed_reply(mut reply: AdapterReply, key: &str) -> AdapterReply {
    reply.response = json!({"dispatched": true, "effective_mode": "foreground", "key": key});
    reply
}

fn post_key_code(
    record: &WindowRecord,
    key_code: u16,
    flags: Option<CGEventFlags>,
    cancellation: &AtomicBool,
) -> AdapterReply {
    match keyboard_events(key_code, flags) {
        Ok(events) => post_keyboard_events(record, &events.0, &events.1, cancellation),
        Err(error) => rejected_error(error),
    }
}

fn keyboard_events(
    key_code: u16,
    flags: Option<CGEventFlags>,
) -> Result<(CFRetained<CGEvent>, CFRetained<CGEvent>), AdapterError> {
    let source = CGEventSource::new(CGEventSourceStateID::HIDSystemState).ok_or_else(|| {
        adapter_error("dispatch_failed", "could not create keyboard event source")
    })?;
    let down = CGEvent::new_keyboard_event(Some(&source), key_code, true)
        .ok_or_else(|| adapter_error("dispatch_failed", "could not create key-down event"))?;
    let up = CGEvent::new_keyboard_event(Some(&source), key_code, false)
        .ok_or_else(|| adapter_error("dispatch_failed", "could not create key-up event"))?;
    apply_key_flags(&down, &up, flags);
    Ok((down, up))
}

fn apply_key_flags(down: &CGEvent, up: &CGEvent, flags: Option<CGEventFlags>) {
    if let Some(flags) = flags {
        CGEvent::set_flags(Some(down), flags);
        CGEvent::set_flags(Some(up), flags);
    }
}

fn post_keyboard_events(
    record: &WindowRecord,
    down: &CGEvent,
    up: &CGEvent,
    cancellation: &AtomicBool,
) -> AdapterReply {
    post_owned_event_pair(
        record,
        cancellation,
        "key_down",
        down,
        "key_up",
        up,
        "keyboard",
    )
    .unwrap_or_else(|| AdapterReply::confirmed(json!({"dispatched": true}), None))
}

fn post_unicode(record: &WindowRecord, text: &str, cancellation: &AtomicBool) -> AdapterReply {
    let utf16 = text.encode_utf16().collect::<Vec<_>>();
    for chunk in utf16.chunks(20) {
        if let Some(reply) = post_unicode_chunk(record, chunk, cancellation) {
            return reply;
        }
    }
    AdapterReply::confirmed(json!({"dispatched": true}), None)
}

fn post_unicode_chunk(
    record: &WindowRecord,
    chunk: &[u16],
    cancellation: &AtomicBool,
) -> Option<AdapterReply> {
    let events = match unicode_events(chunk) {
        Ok(events) => events,
        Err(error) => return Some(rejected_error(error)),
    };
    post_owned_event_pair(
        record,
        cancellation,
        "unicode_down",
        &events.0,
        "unicode_up",
        &events.1,
        "Unicode",
    )
}

fn post_owned_event_pair(
    record: &WindowRecord,
    cancellation: &AtomicBool,
    down_kind: &str,
    down: &CGEvent,
    up_kind: &str,
    up: &CGEvent,
    label: &str,
) -> Option<AdapterReply> {
    if cancellation.load(Ordering::SeqCst) || !owns_exact(record) {
        return Some(interrupted_before_dispatch(&format!(
            "foreground ownership was lost before {label} dispatch"
        )));
    }
    post_event(down_kind, down);
    post_event(up_kind, up);
    (cancellation.load(Ordering::SeqCst) || !owns_exact(record)).then(|| {
        interrupted_after_dispatch(&format!(
            "foreground ownership was lost after {label} dispatch"
        ))
    })
}

fn unicode_events(
    chunk: &[u16],
) -> Result<(CFRetained<CGEvent>, CFRetained<CGEvent>), AdapterError> {
    let (down, up) = unicode_key_events()?;
    unsafe {
        CGEvent::keyboard_set_unicode_string(Some(&down), chunk.len() as u64, chunk.as_ptr());
    }
    Ok((down, up))
}

fn unicode_key_events() -> Result<(CFRetained<CGEvent>, CFRetained<CGEvent>), AdapterError> {
    let source = CGEventSource::new(CGEventSourceStateID::HIDSystemState)
        .ok_or_else(|| adapter_error("dispatch_failed", "could not create Unicode event source"))?;
    Ok((
        CGEvent::new_keyboard_event(Some(&source), 0, true).ok_or_else(|| {
            adapter_error("dispatch_failed", "could not create Unicode key-down event")
        })?,
        CGEvent::new_keyboard_event(Some(&source), 0, false).ok_or_else(|| {
            adapter_error("dispatch_failed", "could not create Unicode key-up event")
        })?,
    ))
}

fn scroll(
    record: &WindowRecord,
    context: &AdapterContext,
    operation: &AdapterOperation,
    prepared_element: Option<&ax::Element>,
    cancellation: &AtomicBool,
) -> AdapterReply {
    let point = match scroll_point(record, context, operation, prepared_element) {
        Ok(point) => point,
        Err(error) => return rejected_error(error),
    };
    post_scroll(record, operation, point, cancellation)
}

fn post_scroll(
    record: &WindowRecord,
    operation: &AdapterOperation,
    point: (f64, f64),
    cancellation: &AtomicBool,
) -> AdapterReply {
    if cancellation.load(Ordering::SeqCst) || !owns_exact(record) {
        return interrupted_before_dispatch("foreground ownership was lost before scroll dispatch");
    }
    move_then_scroll(record, operation, point, cancellation)
}

fn move_then_scroll(
    record: &WindowRecord,
    operation: &AdapterOperation,
    point: (f64, f64),
    cancellation: &AtomicBool,
) -> AdapterReply {
    maybe_move_pointer(point);
    let Some(event) = scroll_event(operation) else {
        return rejected_error(adapter_error(
            "dispatch_failed",
            "could not create scroll event",
        ));
    };
    post_event("scroll", &event);
    finish_scroll(record, cancellation)
}

fn maybe_move_pointer(point: (f64, f64)) {
    if let Some(moved) = CGEvent::new_mouse_event(
        None,
        CGEventType::MouseMoved,
        CGPoint::new(point.0, point.1),
        CGMouseButton::Left,
    ) {
        post_event("mouse_moved", &moved);
    }
}

fn scroll_event(operation: &AdapterOperation) -> Option<CFRetained<CGEvent>> {
    let delta_x = operation
        .input
        .get("delta_x")
        .and_then(Value::as_f64)
        .unwrap_or(0.0)
        .round() as i32;
    let delta_y = operation
        .input
        .get("delta_y")
        .and_then(Value::as_f64)
        .unwrap_or(0.0)
        .round() as i32;
    CGEvent::new_scroll_wheel_event2(None, CGScrollEventUnit::Pixel, 2, -delta_y, -delta_x, 0)
}

fn finish_scroll(record: &WindowRecord, cancellation: &AtomicBool) -> AdapterReply {
    lost_after_scroll(record, cancellation).unwrap_or_else(|| {
        AdapterReply::confirmed(
            json!({"dispatched": true, "effective_mode": "foreground"}),
            None,
        )
    })
}

fn lost_after_scroll(record: &WindowRecord, cancellation: &AtomicBool) -> Option<AdapterReply> {
    (cancellation.load(Ordering::SeqCst) || !owns_exact(record))
        .then(|| interrupted_after_dispatch("foreground ownership was lost after scroll dispatch"))
}

fn scroll_point(
    record: &WindowRecord,
    context: &AdapterContext,
    operation: &AdapterOperation,
    prepared_element: Option<&ax::Element>,
) -> Result<(f64, f64), AdapterError> {
    match locator_kind(operation) {
        Some("point") => prepared_point(operation),
        Some(_) => operation_element(record, context, operation, prepared_element)?
            .bounds()
            .map(center),
        None => Ok(center(record.snapshot.bounds)),
    }
}

fn prepared_point(operation: &AdapterOperation) -> Result<(f64, f64), AdapterError> {
    prepared_global_point(
        operation
            .prepared
            .as_ref()
            .ok_or_else(|| adapter_error("frame_stale", "point operation has no prepared frame"))?,
    )
}

fn prepared_global_point(prepared: &Value) -> Result<(f64, f64), AdapterError> {
    Ok((
        prepared_coord(prepared, "global_x")?,
        prepared_coord(prepared, "global_y")?,
    ))
}

fn prepared_coord(prepared: &Value, field: &str) -> Result<f64, AdapterError> {
    prepared.get(field).and_then(Value::as_f64).ok_or_else(|| {
        adapter_error(
            "frame_stale",
            &format!(
                "prepared point {} is absent",
                field.strip_prefix("global_").unwrap_or(field)
            ),
        )
    })
}

fn operation_element(
    _record: &WindowRecord,
    _context: &AdapterContext,
    _operation: &AdapterOperation,
    prepared: Option<&ax::Element>,
) -> Result<ax::Element, AdapterError> {
    prepared
        .cloned()
        .ok_or_else(|| adapter_error("invalid_request", "foreground mutation is not prepared"))
}

fn interrupted(message: &str) -> AdapterReply {
    let mut reply = AdapterReply::confirmed(Value::Null, None);
    reply.interrupted = true;
    reply.error = Some(adapter_error("interrupted", message));
    reply
}

fn interrupted_before_dispatch(message: &str) -> AdapterReply {
    rejected_error(adapter_error("interrupted", message))
}

fn interrupted_after_dispatch(message: &str) -> AdapterReply {
    let mut reply = interrupted(message);
    reply.delivery = AdapterDelivery::Unknown;
    reply
}

fn post_event(kind: &str, event: &CGEvent) {
    crate::oracle::record("cg_event_post", json!({"event": kind}));
    CGEvent::post(CGEventTapLocation::HIDEventTap, Some(event));
}

fn locator_kind(operation: &AdapterOperation) -> Option<&str> {
    operation.input.get("locator").and_then(locator_kind_value)
}

fn locator_kind_value(locator: &Value) -> Option<&str> {
    locator.get("kind").and_then(Value::as_str)
}

fn key_code(key: &str) -> Option<u16> {
    let key = key.to_ascii_lowercase();
    editing_key_code(&key)
        .or_else(|| arrow_key_code(&key))
        .or_else(|| navigation_key_code(&key))
}

fn editing_key_code(key: &str) -> Option<u16> {
    match key {
        "enter" | "return" => Some(36),
        "tab" => Some(48),
        "space" => Some(49),
        _ => deletion_key_code(key),
    }
}

fn deletion_key_code(key: &str) -> Option<u16> {
    match key {
        "backspace" | "delete" => Some(51),
        "escape" | "esc" => Some(53),
        _ => None,
    }
}

fn arrow_key_code(key: &str) -> Option<u16> {
    match key {
        "left" | "arrowleft" => Some(123),
        "right" | "arrowright" => Some(124),
        "down" | "arrowdown" => Some(125),
        "up" | "arrowup" => Some(126),
        _ => None,
    }
}

fn navigation_key_code(key: &str) -> Option<u16> {
    match key {
        "home" => Some(115),
        "end" => Some(119),
        "pageup" => Some(116),
        "pagedown" => Some(121),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn common_navigation_keys_are_explicitly_mapped() {
        assert_eq!(key_code("Enter"), Some(36));
        assert_eq!(key_code("return"), Some(36));
        assert_eq!(key_code("tab"), Some(48));
        assert_eq!(key_code("space"), Some(49));
        assert_eq!(key_code("Backspace"), Some(51));
        assert_eq!(key_code("delete"), Some(51));
        assert_eq!(key_code("escape"), Some(53));
        assert_eq!(key_code("esc"), Some(53));
        assert_eq!(key_code("ArrowLeft"), Some(123));
        assert_eq!(key_code("right"), Some(124));
        assert_eq!(key_code("ArrowDown"), Some(125));
        assert_eq!(key_code("up"), Some(126));
        assert_eq!(key_code("home"), Some(115));
        assert_eq!(key_code("end"), Some(119));
        assert_eq!(key_code("pageup"), Some(116));
        assert_eq!(key_code("pagedown"), Some(121));
        assert_eq!(key_code("not-a-key"), None);
        assert_eq!(
            unicode_press_key(
                &crate::discovery::WindowRecord {
                    descriptor: manuvra_runtime::TargetDescriptor {
                        target_id: "macos_test".to_owned(),
                        generation: 1,
                        kind: "macos".to_owned(),
                        owner: "Fixture".to_owned(),
                        title: None,
                        capabilities: Vec::new(),
                    },
                    snapshot: crate::discovery::WindowSnapshot {
                        pid: 1,
                        window_id: 1,
                        owner: "Fixture".to_owned(),
                        title: None,
                        bounds: crate::discovery::WindowBounds {
                            x: 0.0,
                            y: 0.0,
                            width: 1.0,
                            height: 1.0,
                        },
                        is_on_screen: true,
                    },
                    present: true,
                },
                "unmapped",
                &AtomicBool::new(false),
            )
            .error
            .unwrap()
            .code,
            "capability_unavailable"
        );
        let mut point = AdapterOperation::new(
            "action.press".to_owned(),
            json!({"key": "enter", "locator": {"kind": "point"}}),
        );
        point.prepared = Some(json!({"global_x": 1.0, "global_y": 2.0}));
        assert_eq!(prepared_point(&point).unwrap(), (1.0, 2.0));
        assert_eq!(
            prepared_point(&AdapterOperation::new("action.press".to_owned(), json!({})))
                .unwrap_err()
                .code,
            "frame_stale"
        );
        assert_eq!(
            dispatch_foreground_command(
                &crate::discovery::WindowRecord {
                    descriptor: manuvra_runtime::TargetDescriptor {
                        target_id: "macos_test".to_owned(),
                        generation: 1,
                        kind: "macos".to_owned(),
                        owner: "Fixture".to_owned(),
                        title: None,
                        capabilities: Vec::new(),
                    },
                    snapshot: crate::discovery::WindowSnapshot {
                        pid: 1,
                        window_id: 1,
                        owner: "Fixture".to_owned(),
                        title: None,
                        bounds: crate::discovery::WindowBounds {
                            x: 0.0,
                            y: 0.0,
                            width: 1.0,
                            height: 1.0,
                        },
                        is_on_screen: true,
                    },
                    present: true,
                },
                &manuvra_runtime::AdapterContext {
                    session_id: "s".to_owned(),
                    target_id: "macos_test".to_owned(),
                    target_generation: 1,
                    action_sequence: 1,
                    reference_namespace: "n".to_owned(),
                    reference_epoch: 1,
                    frame_token: None,
                    mode: manuvra_runtime::ExecutionMode::Foreground,
                    deadline: Instant::now() + Duration::from_secs(1),
                },
                &AdapterOperation::new("observe.tree".to_owned(), json!({})),
                None,
                &AtomicBool::new(false),
            )
            .error
            .unwrap()
            .code,
            "capability_unavailable"
        );
        let press = AdapterOperation::new("action.press".to_owned(), json!({"key": "enter"}));
        let reply = dispatch_press_key(
            &crate::discovery::WindowRecord {
                descriptor: manuvra_runtime::TargetDescriptor {
                    target_id: "macos_test".to_owned(),
                    generation: 1,
                    kind: "macos".to_owned(),
                    owner: "Fixture".to_owned(),
                    title: None,
                    capabilities: Vec::new(),
                },
                snapshot: crate::discovery::WindowSnapshot {
                    pid: i32::MAX,
                    window_id: 1,
                    owner: "Fixture".to_owned(),
                    title: None,
                    bounds: crate::discovery::WindowBounds {
                        x: 0.0,
                        y: 0.0,
                        width: 1.0,
                        height: 1.0,
                    },
                    is_on_screen: true,
                },
                present: true,
            },
            press_key(&press),
            &AtomicBool::new(false),
        );
        assert_eq!(reply.error.as_ref().unwrap().code, "interrupted");
        assert_eq!(reply.delivery, AdapterDelivery::Rejected);
        let record = crate::discovery::WindowRecord {
            descriptor: manuvra_runtime::TargetDescriptor {
                target_id: "macos_test".to_owned(),
                generation: 1,
                kind: "macos".to_owned(),
                owner: "Fixture".to_owned(),
                title: None,
                capabilities: Vec::new(),
            },
            snapshot: crate::discovery::WindowSnapshot {
                pid: i32::MAX,
                window_id: 1,
                owner: "Fixture".to_owned(),
                title: None,
                bounds: crate::discovery::WindowBounds {
                    x: 10.0,
                    y: 20.0,
                    width: 300.0,
                    height: 200.0,
                },
                is_on_screen: true,
            },
            present: true,
        };
        let context = manuvra_runtime::AdapterContext {
            session_id: "s".to_owned(),
            target_id: "macos_test".to_owned(),
            target_generation: 1,
            action_sequence: 1,
            reference_namespace: "n".to_owned(),
            reference_epoch: 1,
            frame_token: None,
            mode: manuvra_runtime::ExecutionMode::Foreground,
            deadline: Instant::now() + Duration::from_secs(1),
        };
        let mut point_scroll = AdapterOperation::new(
            "action.scroll".to_owned(),
            json!({"locator": {"kind": "point"}, "delta_y": 10.0}),
        );
        point_scroll.prepared = Some(json!({"global_x": 11.0, "global_y": 22.0}));
        assert_eq!(
            scroll_point(&record, &context, &point_scroll, None).unwrap(),
            (11.0, 22.0)
        );
        assert_eq!(
            scroll_point(
                &record,
                &context,
                &AdapterOperation::new("action.scroll".to_owned(), json!({})),
                None
            )
            .unwrap(),
            (160.0, 120.0)
        );
        let mut rejected = AdapterReply::confirmed(json!({"ok": true}), None);
        rejected.delivery = AdapterDelivery::Rejected;
        let kept = verify_post_dispatch_ownership(&record, rejected);
        assert!(!kept.interrupted);
        let confirmed = verify_post_dispatch_ownership(
            &record,
            AdapterReply::confirmed(json!({"ok": true}), None),
        );
        assert!(confirmed.interrupted);
        assert!(ax_text_reply(None, &press).is_none());
        assert!(unicode_key_events().is_ok());
        assert!(
            mouse_events(
                (1.0, 2.0),
                CGMouseButton::Left,
                CGEventType::LeftMouseDown,
                CGEventType::LeftMouseUp
            )
            .is_ok()
        );
        let cancelled = AtomicBool::new(true);
        assert_eq!(
            foreground_proof_tick(&record, &cancelled)
                .unwrap()
                .unwrap_err()
                .0
                .code,
            "cancelled"
        );
        assert!(
            focus_press_target(
                &record,
                &context,
                &AdapterOperation::new("action.press".to_owned(), json!({"key": "enter"})),
                None,
                &AtomicBool::new(false),
            )
            .is_none()
        );
        assert_eq!(
            click_count(&AdapterOperation::new(
                "action.click".to_owned(),
                json!({"count": 9})
            )),
            3
        );
        assert_eq!(
            typed_text(&AdapterOperation::new(
                "action.type".to_owned(),
                json!({"text": "hi"})
            )),
            "hi"
        );
        assert!(
            select_all_if_replace(
                &record,
                &AdapterOperation::new("action.type".to_owned(), json!({})),
                &AtomicBool::new(false)
            )
            .is_none()
        );
        assert_eq!(
            click_point(&record, &context, &point_scroll, None).unwrap(),
            (11.0, 22.0)
        );
        let click_reply = click(
            &record,
            &context,
            &point_scroll,
            None,
            &AtomicBool::new(false),
        );
        assert_eq!(click_reply.error.as_ref().unwrap().code, "interrupted");
        let scroll_reply = scroll(
            &record,
            &context,
            &point_scroll,
            None,
            &AtomicBool::new(false),
        );
        assert_eq!(scroll_reply.error.as_ref().unwrap().code, "interrupted");
        let unicode_reply = post_unicode(&record, "hi", &AtomicBool::new(false));
        assert_eq!(unicode_reply.error.as_ref().unwrap().code, "interrupted");
        let mut type_operation = AdapterOperation::new(
            "action.type".to_owned(),
            json!({"locator": {"kind": "point"}, "text": "x"}),
        );
        type_operation.prepared = Some(json!({"global_x": 11.0, "global_y": 22.0}));
        let type_reply = type_text(
            &record,
            &context,
            &type_operation,
            None,
            &AtomicBool::new(false),
        );
        assert_eq!(type_reply.error.as_ref().unwrap().code, "interrupted");
        let _ = exact_window_owned(&record);
        let _ = foreground_owned(&record);
        let _ = wait_for_element_focus(
            &crate::ax::Element::application(std::process::id() as i32).unwrap(),
            Instant::now(),
            &AtomicBool::new(true),
        );
        let _ = focused_text_element(
            &record,
            &context,
            &AdapterOperation::new("action.type".to_owned(), json!({"text": "x"})),
            None,
            &AtomicBool::new(false),
        );
        let _ = focus_explicit_key_target(
            &record,
            &context,
            &AdapterOperation::new(
                "action.press".to_owned(),
                json!({"key": "enter", "locator": {"kind": "ref", "ref": "missing"}}),
            ),
            None,
            &AtomicBool::new(false),
        );
        let _ = wait_for_foreground_proof(&record, Instant::now(), &AtomicBool::new(true));
        let _ = activate_target(&record);
        assert!(foreground_target_is_hidden(&record, true));
    }

    #[test]
    fn pointer_mapping_is_explicit_and_window_centers_are_exact() {
        assert_eq!(mouse_button(Some("right")), CGMouseButton::Right);
        assert_eq!(mouse_button(Some("middle")), CGMouseButton::Center);
        assert_eq!(mouse_button(None), CGMouseButton::Left);
        assert_eq!(
            mouse_event_types(CGMouseButton::Right),
            (CGEventType::RightMouseDown, CGEventType::RightMouseUp)
        );
        assert_eq!(
            center(crate::discovery::WindowBounds {
                x: 10.0,
                y: 20.0,
                width: 300.0,
                height: 200.0,
            }),
            (160.0, 120.0)
        );
    }

    #[test]
    fn later_foreground_proof_query_error_fails_the_tick_instead_of_continuing() {
        let later = (
            adapter_error("observation_failed", "AXMain was not a boolean"),
            true,
        );
        let (error, activated) = foreground_proof_outcome(and_proof_results(Ok(false), Err(later)))
            .expect("later AX error must fail the tick")
            .expect_err("later AX error must not prove ownership");
        assert_eq!(error.code, "observation_failed");
        assert!(activated);

        let later = (
            adapter_error("observation_failed", "AXFocusedWindow was missing"),
            true,
        );
        let (error, activated) = foreground_proof_outcome(and_proof_results(
            Ok(true),
            and_proof_results(Ok(false), Err(later)),
        ))
        .expect("focused AX error must fail the tick after main is false")
        .expect_err("focused AX error must not prove ownership");
        assert_eq!(error.code, "observation_failed");
        assert!(activated);

        assert!(
            foreground_proof_outcome(and_proof_results(Ok(false), Ok(true))).is_none(),
            "false ownership without AX errors must continue"
        );
        assert!(matches!(
            foreground_proof_outcome(and_proof_results(Ok(true), Ok(true))),
            Some(Ok(()))
        ));
    }

    #[test]
    fn activation_failure_preserves_pre_and_post_activation_delivery_truth() {
        let before = foreground_acquisition_failure(
            adapter_error("capability_unavailable", "hidden"),
            false,
        );
        assert_eq!(before.delivery, AdapterDelivery::Rejected);
        assert_eq!(before.error.unwrap().code, "capability_unavailable");

        let after = foreground_acquisition_failure(
            adapter_error("observation_failed", "ownership proof failed"),
            true,
        );
        assert_eq!(after.delivery, AdapterDelivery::Unknown);
        assert_eq!(after.error.unwrap().code, "observation_failed");
    }

    #[test]
    fn permission_revocation_after_ownership_is_never_reported_as_predispatch() {
        let reply = permission_lost_after_foreground_ownership();
        assert_eq!(reply.delivery, AdapterDelivery::Unknown);
        assert!(reply.interrupted);
        assert_eq!(reply.error.as_ref().unwrap().code, "interrupted");
        assert!(
            reply
                .error
                .as_ref()
                .unwrap()
                .message
                .as_deref()
                .is_some_and(|message| message.contains("no input event was dispatched")
                    && message.contains("manuvra doctor"))
        );
    }
}
