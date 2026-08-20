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
    if !crate::permissions::PermissionSnapshot::current().post_event {
        return permission_lost_after_foreground_ownership();
    }
    crate::oracle::barrier(
        "after_foreground_proof_before_input",
        context.deadline,
        &cancellation,
    );
    if cancellation.load(Ordering::SeqCst) {
        return interrupted_before_dispatch("operation was cancelled after foreground activation");
    }
    if !owns_exact(record) {
        return interrupted_before_dispatch(
            "foreground ownership was lost after proof and before input dispatch",
        );
    }
    let reply = match operation.command.as_str() {
        "action.click" => click(record, context, operation, element, &cancellation),
        "action.type" => type_text(record, context, operation, element, &cancellation),
        "action.press" => press(record, context, operation, element, &cancellation),
        "action.scroll" => scroll(record, context, operation, element, &cancellation),
        _ => rejected_error(adapter_error(
            "capability_unavailable",
            "operation has no foreground implementation",
        )),
    };
    verify_post_dispatch_ownership(record, reply)
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
    if owns_exact(record) {
        crate::oracle::barrier("during_foreground_ownership", deadline, cancellation);
        return Ok(());
    }
    let application = running_application(record)?;
    ensure_foreground_visible(record, &application)?;
    if !application.activateWithOptions(NSApplicationActivationOptions::empty()) {
        return Err((
            adapter_error(
                "dispatch_failed",
                "AppKit rejected target application activation",
            ),
            false,
        ));
    }
    ax::focus_exact_window(record).map_err(|error| (error, true))?;
    crate::oracle::barrier("during_foreground_ownership", deadline, cancellation);
    wait_for_foreground_proof(record, deadline, cancellation)
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
    let hidden = application.isHidden()
        || ax::application_is_hidden(record.snapshot.pid).unwrap_or(false)
        || (!record.snapshot.is_on_screen
            && !ax::exact_window_is_minimized(record).unwrap_or(false));
    if hidden {
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

fn wait_for_foreground_proof(
    record: &WindowRecord,
    deadline: Instant,
    cancellation: &AtomicBool,
) -> Result<(), (AdapterError, bool)> {
    let proof_deadline = deadline.min(Instant::now() + Duration::from_millis(500));
    while Instant::now() < proof_deadline {
        if cancellation.load(Ordering::SeqCst) {
            return Err((
                adapter_error(
                    "cancelled",
                    "operation was cancelled while establishing foreground ownership",
                ),
                true,
            ));
        }
        let application_frontmost =
            ax::application_is_frontmost(record.snapshot.pid).map_err(|error| (error, true))?;
        let exact_window_main = ax::exact_window_is_main(record).map_err(|error| (error, true))?;
        let exact_window_focused =
            ax::exact_window_is_focused(record).map_err(|error| (error, true))?;
        if application_frontmost && exact_window_main && exact_window_focused {
            return Ok(());
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
    let button = mouse_button(operation.input.get("button").and_then(Value::as_str));
    let (down_type, up_type) = mouse_event_types(button);
    let events = match mouse_events(point, button, down_type, up_type) {
        Ok(events) => events,
        Err(error) => return rejected_error(error),
    };
    let count = operation
        .input
        .get("count")
        .and_then(Value::as_u64)
        .unwrap_or(1)
        .min(3);
    dispatch_mouse_clicks(record, cancellation, &events.0, &events.1, count)
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
        if cancellation.load(Ordering::SeqCst) || !owns_exact(record) {
            return interrupted_before_dispatch(
                "foreground ownership was lost before mouse dispatch",
            );
        }
        post_event("mouse_down", down);
        post_event("mouse_up", up);
        if cancellation.load(Ordering::SeqCst) || !owns_exact(record) {
            return interrupted_after_dispatch(
                "foreground ownership was lost after mouse dispatch",
            );
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
    let mut click_operation = operation.clone();
    click_operation.input["button"] = Value::String("left".to_owned());
    click_operation.input["count"] = Value::from(1);
    let click = click(
        record,
        context,
        &click_operation,
        prepared_element,
        cancellation,
    );
    if click.delivery != AdapterDelivery::Confirmed || click.interrupted {
        return click;
    }
    let focused_element =
        match focused_text_element(record, context, operation, prepared_element, cancellation) {
            Ok(element) => element,
            Err(error) => {
                return interrupted_after_dispatch(
                    error
                        .message
                        .as_deref()
                        .unwrap_or("typed element could not be focused after mouse dispatch"),
                );
            }
        };
    if let Some(reply) = ax_text_reply(focused_element.as_ref(), operation) {
        return reply;
    }
    global_text_reply(record, operation, cancellation)
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
    let element = element?;
    match ax::type_if_settable(element, operation) {
        Ok(Some(mut response)) => {
            response["effective_mode"] = Value::String("foreground".to_owned());
            response["dispatch"] = Value::String("AXValue".to_owned());
            Some(AdapterReply::confirmed(response, None))
        }
        Ok(None) => None,
        Err(error) => Some(interrupted_after_dispatch(
            error
                .message
                .as_deref()
                .unwrap_or("foreground AX text mutation failed after focus dispatch"),
        )),
    }
}

fn global_text_reply(
    record: &WindowRecord,
    operation: &AdapterOperation,
    cancellation: &AtomicBool,
) -> AdapterReply {
    if operation
        .input
        .get("replace")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        let selected = post_key_code(record, 0, Some(CGEventFlags::MaskCommand), cancellation);
        if selected.interrupted || selected.delivery != AdapterDelivery::Confirmed {
            return selected;
        }
    }
    let text = operation
        .input
        .get("text")
        .and_then(Value::as_str)
        .unwrap_or_default();
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
    if operation.input.get("locator").is_some() && locator_kind(operation) != Some("point") {
        let element = match operation_element(record, context, operation, prepared_element) {
            Ok(element) => element,
            Err(error) => return rejected_error(error),
        };
        crate::oracle::record("ax_focus_set_attempt", Value::Null);
        if let Err(error) = element.set_bool("AXFocused", true) {
            return rejected_error(error);
        }
        crate::oracle::record("ax_focus_set", Value::Null);
        if cancellation.load(Ordering::SeqCst) || !owns_exact(record) {
            return interrupted_after_dispatch(
                "foreground ownership was lost after focusing the explicit key target",
            );
        }
    }
    let key = operation
        .input
        .get("key")
        .and_then(Value::as_str)
        .unwrap_or_default();
    if let Some(key_code) = key_code(key) {
        let mut reply = post_key_code(record, key_code, None, cancellation);
        reply.response = json!({"dispatched": true, "effective_mode": "foreground", "key": key});
        return reply;
    }
    if key.chars().count() == 1 {
        let mut reply = post_unicode(record, key, cancellation);
        reply.response = json!({"dispatched": true, "effective_mode": "foreground", "key": key});
        return reply;
    }
    rejected_error(adapter_error(
        "capability_unavailable",
        &format!("foreground key {key:?} is not mapped"),
    ))
}

fn post_key_code(
    record: &WindowRecord,
    key_code: u16,
    flags: Option<CGEventFlags>,
    cancellation: &AtomicBool,
) -> AdapterReply {
    let Some(source) = CGEventSource::new(CGEventSourceStateID::HIDSystemState) else {
        return rejected_error(adapter_error(
            "dispatch_failed",
            "could not create keyboard event source",
        ));
    };
    let Some(down) = CGEvent::new_keyboard_event(Some(&source), key_code, true) else {
        return rejected_error(adapter_error(
            "dispatch_failed",
            "could not create key-down event",
        ));
    };
    let Some(up) = CGEvent::new_keyboard_event(Some(&source), key_code, false) else {
        return rejected_error(adapter_error(
            "dispatch_failed",
            "could not create key-up event",
        ));
    };
    if let Some(flags) = flags {
        CGEvent::set_flags(Some(&down), flags);
        CGEvent::set_flags(Some(&up), flags);
    }
    if cancellation.load(Ordering::SeqCst) || !owns_exact(record) {
        return interrupted_before_dispatch(
            "foreground ownership was lost before keyboard dispatch",
        );
    }
    post_event("key_down", &down);
    post_event("key_up", &up);
    if cancellation.load(Ordering::SeqCst) || !owns_exact(record) {
        return interrupted_after_dispatch("foreground ownership was lost after keyboard dispatch");
    }
    AdapterReply::confirmed(json!({"dispatched": true}), None)
}

fn post_unicode(record: &WindowRecord, text: &str, cancellation: &AtomicBool) -> AdapterReply {
    let utf16 = text.encode_utf16().collect::<Vec<_>>();
    for chunk in utf16.chunks(20) {
        let events = match unicode_events(chunk) {
            Ok(events) => events,
            Err(error) => return rejected_error(error),
        };
        if cancellation.load(Ordering::SeqCst) || !owns_exact(record) {
            return interrupted_before_dispatch(
                "foreground ownership was lost before Unicode dispatch",
            );
        }
        post_event("unicode_down", &events.0);
        post_event("unicode_up", &events.1);
        if cancellation.load(Ordering::SeqCst) || !owns_exact(record) {
            return interrupted_after_dispatch(
                "foreground ownership was lost after Unicode dispatch",
            );
        }
    }
    AdapterReply::confirmed(json!({"dispatched": true}), None)
}

fn unicode_events(
    chunk: &[u16],
) -> Result<(CFRetained<CGEvent>, CFRetained<CGEvent>), AdapterError> {
    let source = CGEventSource::new(CGEventSourceStateID::HIDSystemState)
        .ok_or_else(|| adapter_error("dispatch_failed", "could not create Unicode event source"))?;
    let down = CGEvent::new_keyboard_event(Some(&source), 0, true).ok_or_else(|| {
        adapter_error("dispatch_failed", "could not create Unicode key-down event")
    })?;
    let up = CGEvent::new_keyboard_event(Some(&source), 0, false)
        .ok_or_else(|| adapter_error("dispatch_failed", "could not create Unicode key-up event"))?;
    unsafe {
        CGEvent::keyboard_set_unicode_string(Some(&down), chunk.len() as u64, chunk.as_ptr());
    }
    Ok((down, up))
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
    if cancellation.load(Ordering::SeqCst) || !owns_exact(record) {
        return interrupted_before_dispatch("foreground ownership was lost before scroll dispatch");
    }
    if let Some(moved) = CGEvent::new_mouse_event(
        None,
        CGEventType::MouseMoved,
        CGPoint::new(point.0, point.1),
        CGMouseButton::Left,
    ) {
        post_event("mouse_moved", &moved);
    }
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
    let Some(event) =
        CGEvent::new_scroll_wheel_event2(None, CGScrollEventUnit::Pixel, 2, -delta_y, -delta_x, 0)
    else {
        return rejected_error(adapter_error(
            "dispatch_failed",
            "could not create scroll event",
        ));
    };
    post_event("scroll", &event);
    if cancellation.load(Ordering::SeqCst) || !owns_exact(record) {
        return interrupted_after_dispatch("foreground ownership was lost after scroll dispatch");
    }
    AdapterReply::confirmed(
        json!({"dispatched": true, "effective_mode": "foreground"}),
        None,
    )
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
    let prepared = operation
        .prepared
        .as_ref()
        .ok_or_else(|| adapter_error("frame_stale", "point operation has no prepared frame"))?;
    Ok((
        prepared
            .get("global_x")
            .and_then(Value::as_f64)
            .ok_or_else(|| adapter_error("frame_stale", "prepared point x is absent"))?,
        prepared
            .get("global_y")
            .and_then(Value::as_f64)
            .ok_or_else(|| adapter_error("frame_stale", "prepared point y is absent"))?,
    ))
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
    Some(match key.to_ascii_lowercase().as_str() {
        "enter" | "return" => 36,
        "tab" => 48,
        "space" => 49,
        "backspace" | "delete" => 51,
        "escape" | "esc" => 53,
        "left" | "arrowleft" => 123,
        "right" | "arrowright" => 124,
        "down" | "arrowdown" => 125,
        "up" | "arrowup" => 126,
        "home" => 115,
        "end" => 119,
        "pageup" => 116,
        "pagedown" => 121,
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn common_navigation_keys_are_explicitly_mapped() {
        assert_eq!(key_code("Enter"), Some(36));
        assert_eq!(key_code("ArrowLeft"), Some(123));
        assert_eq!(key_code("not-a-key"), None);
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
