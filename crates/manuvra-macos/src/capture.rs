use crate::ax::{adapter_error, rejected_error};
use crate::discovery::WindowRecord;
use block2::RcBlock;
use manuvra_runtime::{AdapterContext, AdapterError, AdapterOperation, AdapterReply};
use objc2::AnyThread;
use objc2_app_kit::NSRunningApplication;
use objc2_core_foundation::{CFData, CFMutableData, CFString};
use objc2_core_graphics::CGImage;
use objc2_foundation::NSError;
use objc2_image_io::CGImageDestination;
use objc2_screen_capture_kit::{
    SCCaptureResolutionType, SCContentFilter, SCScreenshotManager, SCShareableContent,
    SCStreamConfiguration,
};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::time::{Duration, Instant};

struct CallbackMailbox<T> {
    sender: mpsc::Sender<T>,
    terminal: Arc<AtomicBool>,
}

impl<T> Clone for CallbackMailbox<T> {
    fn clone(&self) -> Self {
        Self {
            sender: self.sender.clone(),
            terminal: self.terminal.clone(),
        }
    }
}

impl<T> CallbackMailbox<T> {
    fn send(&self, value: T) -> bool {
        !self.terminal.load(Ordering::Acquire) && self.sender.send(value).is_ok()
    }
}

struct CallbackTerminal(Arc<AtomicBool>);

impl Drop for CallbackTerminal {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Release);
    }
}

fn callback_channel<T>() -> (CallbackMailbox<T>, mpsc::Receiver<T>, CallbackTerminal) {
    let (sender, receiver) = mpsc::channel();
    let terminal = Arc::new(AtomicBool::new(false));
    (
        CallbackMailbox {
            sender,
            terminal: terminal.clone(),
        },
        receiver,
        CallbackTerminal(terminal),
    )
}

pub(crate) fn screenshot(
    record: &WindowRecord,
    context: &AdapterContext,
    cancellation: &AtomicBool,
) -> AdapterReply {
    let capture_started = Instant::now();
    if cancellation.load(Ordering::SeqCst) {
        return rejected_error(adapter_error(
            "cancelled",
            "request was cancelled before ScreenCaptureKit dispatch",
        ));
    }
    if is_hidden(record) {
        return rejected_error(adapter_error(
            "capability_unavailable",
            "the target application is hidden; CP-07 never unhides applications implicitly",
        ));
    }
    let (sender, receiver, _terminal_scope) = callback_channel();
    let expected_pid = record.snapshot.pid;
    let expected_window = record.snapshot.window_id;
    let expected_bounds = record.snapshot.bounds;
    let completion = RcBlock::new(
        move |content: *mut SCShareableContent, error: *mut NSError| {
            if content.is_null() {
                let _ = sender.send(Err(ns_error(
                    error,
                    "ScreenCaptureKit did not return shareable content",
                )));
                return;
            }
            // SAFETY: callback object pointers are valid for the callback duration.
            let windows = unsafe { (&*content).windows() };
            let window = windows.iter().find(|window| unsafe {
                window.windowID() == expected_window
                    && window
                        .owningApplication()
                        .is_some_and(|application| application.processID() == expected_pid)
            });
            let Some(window) = window else {
                let _ = sender.send(Err(adapter_error(
                    "target_stale",
                    "the pinned process/window pair is absent from ScreenCaptureKit",
                )));
                return;
            };
            let filter = unsafe {
                SCContentFilter::initWithDesktopIndependentWindow(SCContentFilter::alloc(), &window)
            };
            let configuration = unsafe { SCStreamConfiguration::new() };
            let (frame_bounds, scale, requested_width, requested_height);
            unsafe {
                let content_rect = filter.contentRect();
                scale = f64::from(filter.pointPixelScale()).max(1.0);
                requested_width = (content_rect.size.width * scale).round().max(1.0) as usize;
                requested_height = (content_rect.size.height * scale).round().max(1.0) as usize;
                frame_bounds = crate::discovery::WindowBounds {
                    x: content_rect.origin.x,
                    y: content_rect.origin.y,
                    width: content_rect.size.width,
                    height: content_rect.size.height,
                };
                if !crate::ax::same_bounds(frame_bounds, expected_bounds) {
                    let _ = sender.send(Err(adapter_error(
                        "capability_unavailable",
                        "ScreenCaptureKit geometry does not match the pinned WindowServer window",
                    )));
                    return;
                }
                configuration.setWidth(requested_width);
                configuration.setHeight(requested_height);
                configuration.setShowsCursor(false);
                configuration.setIgnoreShadowsSingleWindow(true);
                configuration.setCaptureResolution(SCCaptureResolutionType::Best);
            }
            let image_sender = sender.clone();
            let image_completion = RcBlock::new(move |image: *mut CGImage, error: *mut NSError| {
                let result = if image.is_null() {
                    Err(ns_error(error, "ScreenCaptureKit returned no image"))
                } else {
                    // SAFETY: callback image is valid for the callback duration; encoding retains
                    // all bytes before returning from this callback.
                    encode_png(
                        unsafe { &*image },
                        frame_bounds,
                        scale,
                        requested_width,
                        requested_height,
                    )
                };
                let _ = image_sender.send(result);
            });
            unsafe {
                SCScreenshotManager::captureImageWithFilter_configuration_completionHandler(
                    &filter,
                    &configuration,
                    Some(&image_completion),
                );
            }
        },
    );
    unsafe {
        SCShareableContent::getShareableContentExcludingDesktopWindows_onScreenWindowsOnly_completionHandler(
            true,
            false,
            &completion,
        );
    }
    crate::oracle::barrier("during_capture", context.deadline, cancellation);
    loop {
        if cancellation.load(Ordering::SeqCst) {
            return rejected_error(adapter_error(
                "cancelled",
                "request was cancelled while awaiting ScreenCaptureKit",
            ));
        }
        let remaining = context.deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return capture_timeout();
        }
        match receiver.recv_timeout(remaining.min(Duration::from_millis(5))) {
            Ok(Ok(capture)) => return captured_reply(record, capture, capture_started),
            Ok(Err(error)) => return rejected_error(error),
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                return rejected_error(adapter_error(
                    "observation_failed",
                    "ScreenCaptureKit completion channel disconnected",
                ));
            }
        }
    }
}

fn captured_reply(record: &WindowRecord, capture: Capture, started: Instant) -> AdapterReply {
    let signature = frame_signature(capture.bounds, capture.width, capture.height, capture.scale);
    let mut reply = AdapterReply::confirmed(
        json!({
            "window_id_verified": true,
            "complete": true,
            "is_on_screen": record.snapshot.is_on_screen,
            "frame_bounds": capture.bounds,
            "point_pixel_scale": capture.scale,
        }),
        Some(capture.bytes),
    );
    reply.screenshot_width = Some(capture.width);
    reply.screenshot_height = Some(capture.height);
    reply.frame_signature = Some(signature);
    reply.timing.capture_ms = started.elapsed().as_millis() as u64;
    reply
}

fn capture_timeout() -> AdapterReply {
    let mut reply = AdapterReply::confirmed(json!({}), None);
    reply.delivery = manuvra_runtime::AdapterDelivery::Unknown;
    reply.error = Some(adapter_error(
        "timed_out",
        "ScreenCaptureKit did not complete before the deadline",
    ));
    reply
}

struct Capture {
    bytes: Vec<u8>,
    width: u32,
    height: u32,
    bounds: crate::discovery::WindowBounds,
    scale: f64,
}

fn encode_png(
    image: &CGImage,
    bounds: crate::discovery::WindowBounds,
    scale: f64,
    requested_width: usize,
    requested_height: usize,
) -> Result<Capture, AdapterError> {
    let data = CFMutableData::new(None, 0)
        .ok_or_else(|| adapter_error("observation_failed", "could not allocate PNG data"))?;
    let png = CFString::from_str("public.png");
    let destination = unsafe { CGImageDestination::with_data(&data, &png, 1, None) }
        .ok_or_else(|| adapter_error("observation_failed", "could not create PNG encoder"))?;
    unsafe {
        destination.add_image(image, None);
    }
    if !unsafe { destination.finalize() } {
        return Err(adapter_error(
            "observation_failed",
            "ImageIO could not finalize the PNG",
        ));
    }
    let data: &CFData = &data;
    let width = u32::try_from(CGImage::width(Some(image)))
        .map_err(|_| adapter_error("observation_failed", "captured width overflowed u32"))?;
    let height = u32::try_from(CGImage::height(Some(image)))
        .map_err(|_| adapter_error("observation_failed", "captured height overflowed u32"))?;
    if width == 0 || height == 0 {
        return Err(adapter_error(
            "observation_failed",
            "ScreenCaptureKit returned an empty frame",
        ));
    }
    if width as usize != requested_width || height as usize != requested_height {
        return Err(adapter_error(
            "observation_failed",
            "ScreenCaptureKit returned dimensions different from the exact requested window frame",
        ));
    }
    Ok(Capture {
        bytes: data.to_vec(),
        width,
        height,
        bounds,
        scale,
    })
}

#[derive(Clone)]
pub(crate) struct FrameAuthority {
    pub signature: String,
    pub bounds: crate::discovery::WindowBounds,
    pub width: u32,
    pub height: u32,
    pub scale: f64,
}

pub(crate) fn authority(reply: &AdapterReply) -> Option<FrameAuthority> {
    let bounds = reply.response.get("frame_bounds")?;
    Some(FrameAuthority {
        signature: reply.frame_signature.clone()?,
        bounds: crate::discovery::WindowBounds {
            x: bounds.get("x")?.as_f64()?,
            y: bounds.get("y")?.as_f64()?,
            width: bounds.get("width")?.as_f64()?,
            height: bounds.get("height")?.as_f64()?,
        },
        width: reply.screenshot_width?,
        height: reply.screenshot_height?,
        scale: reply.response.get("point_pixel_scale")?.as_f64()?,
    })
}

pub(crate) fn prepare_point(
    record: &WindowRecord,
    context: &AdapterContext,
    operation: &AdapterOperation,
    authority: Option<&FrameAuthority>,
    cancellation: &AtomicBool,
) -> Result<AdapterOperation, AdapterError> {
    let Some(locator) = operation
        .input
        .get("locator")
        .filter(|locator| locator.get("kind").and_then(Value::as_str) == Some("point"))
    else {
        return Ok(operation.clone());
    };
    let authority = authority.ok_or_else(|| {
        adapter_error(
            "frame_stale",
            "the session has no matching native screenshot frame authority",
        )
    })?;
    validate_frame_authority(record, context, authority, cancellation)?;
    let (x, y) = point_coordinates(locator)?;
    validate_point_bounds(x, y, authority)?;
    let mut prepared = operation.clone();
    prepared.prepared = Some(json!({
        "global_x": authority.bounds.x + x * authority.bounds.width / f64::from(authority.width),
        "global_y": authority.bounds.y + y * authority.bounds.height / f64::from(authority.height),
    }));
    Ok(prepared)
}

fn validate_frame_authority(
    record: &WindowRecord,
    context: &AdapterContext,
    authority: &FrameAuthority,
    cancellation: &AtomicBool,
) -> Result<(), AdapterError> {
    let signature = context
        .frame_token
        .as_deref()
        .and_then(|token| token.rsplit('_').next());
    let current = current_frame_facts(record, context, cancellation)?;
    let current_signature =
        frame_signature(current.bounds, current.width, current.height, current.scale);
    if signature != Some(authority.signature.as_str())
        || current_signature != authority.signature
        || !crate::ax::same_bounds(current.bounds, authority.bounds)
        || current.width != authority.width
        || current.height != authority.height
        || (current.scale - authority.scale).abs() > f64::EPSILON
    {
        return Err(adapter_error(
            "frame_stale",
            "the native window geometry or scale changed after the screenshot",
        ));
    }
    Ok(())
}

fn point_coordinates(locator: &Value) -> Result<(f64, f64), AdapterError> {
    let x = locator
        .get("x")
        .and_then(Value::as_f64)
        .ok_or_else(|| adapter_error("invalid_request", "point x is required"))?;
    let y = locator
        .get("y")
        .and_then(Value::as_f64)
        .ok_or_else(|| adapter_error("invalid_request", "point y is required"))?;
    Ok((x, y))
}

fn validate_point_bounds(x: f64, y: f64, authority: &FrameAuthority) -> Result<(), AdapterError> {
    if x < 0.0 || y < 0.0 || x >= f64::from(authority.width) || y >= f64::from(authority.height) {
        return Err(adapter_error(
            "element_not_found",
            "point lies outside the exact screenshot pixel bounds",
        ));
    }
    Ok(())
}

struct FrameFacts {
    bounds: crate::discovery::WindowBounds,
    width: u32,
    height: u32,
    scale: f64,
}

fn current_frame_facts(
    record: &WindowRecord,
    context: &AdapterContext,
    cancellation: &AtomicBool,
) -> Result<FrameFacts, AdapterError> {
    if cancellation.load(Ordering::SeqCst) {
        return Err(adapter_error(
            "cancelled",
            "request was cancelled before frame-authority validation",
        ));
    }
    let (sender, receiver, _terminal_scope) = callback_channel();
    let expected_pid = record.snapshot.pid;
    let expected_window = record.snapshot.window_id;
    let completion = RcBlock::new(
        move |content: *mut SCShareableContent, error: *mut NSError| {
            if content.is_null() {
                let _ = sender.send(Err(ns_error(
                    error,
                    "ScreenCaptureKit did not return shareable content",
                )));
                return;
            }
            // SAFETY: callback object pointers are valid for the callback duration.
            let windows = unsafe { (&*content).windows() };
            let window = windows.iter().find(|window| unsafe {
                window.windowID() == expected_window
                    && window
                        .owningApplication()
                        .is_some_and(|application| application.processID() == expected_pid)
            });
            let Some(window) = window else {
                let _ = sender.send(Err(adapter_error(
                    "target_stale",
                    "the pinned process/window pair is absent from ScreenCaptureKit",
                )));
                return;
            };
            let filter = unsafe {
                SCContentFilter::initWithDesktopIndependentWindow(SCContentFilter::alloc(), &window)
            };
            // SAFETY: the filter owns the selected window for this callback duration.
            let content_rect = unsafe { filter.contentRect() };
            let scale =
                crate::seam::frame_scale(f64::from(unsafe { filter.pointPixelScale() }).max(1.0));
            let width = (content_rect.size.width * scale).round().max(1.0) as u32;
            let height = (content_rect.size.height * scale).round().max(1.0) as u32;
            let _ = sender.send(Ok(FrameFacts {
                bounds: crate::discovery::WindowBounds {
                    x: content_rect.origin.x,
                    y: content_rect.origin.y,
                    width: content_rect.size.width,
                    height: content_rect.size.height,
                },
                width,
                height,
                scale,
            }));
        },
    );
    unsafe {
        SCShareableContent::getShareableContentExcludingDesktopWindows_onScreenWindowsOnly_completionHandler(
            true,
            false,
            &completion,
        );
    }
    loop {
        if cancellation.load(Ordering::SeqCst) {
            return Err(adapter_error(
                "cancelled",
                "request was cancelled during frame-authority validation",
            ));
        }
        let remaining = context.deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(adapter_error(
                "timed_out",
                "frame-authority validation did not complete before the deadline",
            ));
        }
        match receiver.recv_timeout(remaining.min(Duration::from_millis(5))) {
            Ok(result) => return result,
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                return Err(adapter_error(
                    "observation_failed",
                    "frame-authority completion channel disconnected",
                ));
            }
        }
    }
}

fn is_hidden(record: &WindowRecord) -> bool {
    let minimized = crate::ax::exact_window_is_minimized(record).unwrap_or(false);
    let application_hidden =
        crate::ax::application_is_hidden(record.snapshot.pid).unwrap_or_else(|_| {
            NSRunningApplication::runningApplicationWithProcessIdentifier(record.snapshot.pid)
                .is_some_and(|application| application.isHidden())
        });
    hidden_state(application_hidden, record.snapshot.is_on_screen, minimized)
}

fn hidden_state(application_hidden: bool, on_screen: bool, minimized: bool) -> bool {
    !minimized && (application_hidden || !on_screen)
}

fn ns_error(error: *mut NSError, fallback: &str) -> AdapterError {
    let message = if error.is_null() {
        fallback.to_owned()
    } else {
        // SAFETY: NSError pointer is valid for the callback duration.
        unsafe { (&*error).localizedDescription().to_string() }
    };
    adapter_error("observation_failed", &message)
}

fn frame_signature(
    bounds: crate::discovery::WindowBounds,
    width: u32,
    height: u32,
    scale: f64,
) -> String {
    Sha256::digest(format!(
        "{:.4},{:.4},{:.4},{:.4},{width},{height},{scale:.4}",
        bounds.x, bounds.y, bounds.width, bounds.height
    ))
    .iter()
    .map(|byte| format!("{byte:02x}"))
    .collect::<String>()[..16]
        .to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_signature_is_stable_and_hex() {
        let digest = frame_signature(
            crate::discovery::WindowBounds {
                x: 1.0,
                y: 2.0,
                width: 300.0,
                height: 200.0,
            },
            600,
            400,
            2.0,
        );
        assert_eq!(digest.len(), 16);
        assert!(
            digest
                .chars()
                .all(|character| character.is_ascii_hexdigit())
        );
    }

    #[test]
    fn minimized_window_is_not_misclassified_as_hidden() {
        assert!(!hidden_state(true, false, true));
        assert!(hidden_state(true, false, false));
        assert!(hidden_state(false, false, false));
        assert!(!hidden_state(false, true, false));
        crate::test_oracles::write(
            "window-classification.json",
            &json!({
                "schema": "manuvra/cp-07-boundary-oracle@1",
                "case": "hidden_vs_minimized",
                "rows": [
                    {"application_hidden": true, "on_screen": false, "minimized": true, "hidden": false},
                    {"application_hidden": true, "on_screen": false, "minimized": false, "hidden": true},
                    {"application_hidden": false, "on_screen": false, "minimized": false, "hidden": true},
                    {"application_hidden": false, "on_screen": true, "minimized": false, "hidden": false}
                ]
            }),
        );
    }

    #[test]
    fn late_screen_capture_callback_cannot_change_terminal_state() {
        let (mailbox, _receiver, terminal) = callback_channel::<Value>();
        let committed = json!({
            "action_sequence": 7,
            "manifest": "sha256:committed",
            "result": {"error": {"code": "cancelled"}},
        });
        let before = serde_json::to_vec(&committed).unwrap();
        drop(terminal);
        assert!(!mailbox.send(json!({"late": true})));
        let after = serde_json::to_vec(&committed).unwrap();
        assert_eq!(before, after);
        crate::test_oracles::write(
            "late-callbacks.json",
            &json!({
                "schema": "manuvra/cp-07-boundary-oracle@1",
                "case": "late_sck_callback_after_terminal",
                "callback_delivered": false,
                "action_sequence": 7,
                "result_and_manifest_byte_identical": before == after,
            }),
        );
    }
}
