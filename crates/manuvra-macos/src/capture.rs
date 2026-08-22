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
    if let Some(reply) = screenshot_preflight(record, cancellation) {
        return reply;
    }
    await_shareable_capture(record, context, cancellation, capture_started)
}

fn screenshot_preflight(record: &WindowRecord, cancellation: &AtomicBool) -> Option<AdapterReply> {
    if cancellation.load(Ordering::SeqCst) {
        return Some(rejected_error(adapter_error(
            "cancelled",
            "request was cancelled before ScreenCaptureKit dispatch",
        )));
    }
    is_hidden(record).then(|| {
        rejected_error(adapter_error(
            "capability_unavailable",
            "the target application is hidden; CP-07 never unhides applications implicitly",
        ))
    })
}

fn await_shareable_capture(
    record: &WindowRecord,
    context: &AdapterContext,
    cancellation: &AtomicBool,
    capture_started: Instant,
) -> AdapterReply {
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
            let window = windows.iter().find(|window| {
                shareable_window_matches(
                    unsafe { window.windowID() },
                    unsafe {
                        window
                            .owningApplication()
                            .map(|application| application.processID())
                    },
                    expected_window,
                    expected_pid,
                )
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
            let Some((frame_bounds, scale, requested_width, requested_height)) =
                configured_capture_frame(&filter, expected_bounds)
            else {
                let _ = sender.send(Err(adapter_error(
                    "capability_unavailable",
                    "ScreenCaptureKit geometry does not match the pinned WindowServer window",
                )));
                return;
            };
            unsafe {
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
    recv_capture(record, context, cancellation, capture_started, &receiver)
}

fn recv_capture(
    record: &WindowRecord,
    context: &AdapterContext,
    cancellation: &AtomicBool,
    capture_started: Instant,
    receiver: &mpsc::Receiver<Result<Capture, AdapterError>>,
) -> AdapterReply {
    loop {
        if let Some(reply) =
            capture_wait_tick(record, context, cancellation, capture_started, receiver)
        {
            return reply;
        }
    }
}

fn capture_wait_tick(
    record: &WindowRecord,
    context: &AdapterContext,
    cancellation: &AtomicBool,
    capture_started: Instant,
    receiver: &mpsc::Receiver<Result<Capture, AdapterError>>,
) -> Option<AdapterReply> {
    if cancellation.load(Ordering::SeqCst) {
        return Some(rejected_error(adapter_error(
            "cancelled",
            "request was cancelled while awaiting ScreenCaptureKit",
        )));
    }
    recv_capture_once(record, context, capture_started, receiver)
}

fn recv_capture_once(
    record: &WindowRecord,
    context: &AdapterContext,
    capture_started: Instant,
    receiver: &mpsc::Receiver<Result<Capture, AdapterError>>,
) -> Option<AdapterReply> {
    let remaining = context.deadline.saturating_duration_since(Instant::now());
    if remaining.is_zero() {
        return Some(capture_timeout());
    }
    match receiver.recv_timeout(remaining.min(Duration::from_millis(5))) {
        Ok(Ok(capture)) => Some(captured_reply(record, capture, capture_started)),
        Ok(Err(error)) => Some(rejected_error(error)),
        Err(mpsc::RecvTimeoutError::Timeout) => None,
        Err(mpsc::RecvTimeoutError::Disconnected) => Some(rejected_error(adapter_error(
            "observation_failed",
            "ScreenCaptureKit completion channel disconnected",
        ))),
    }
}

fn shareable_window_matches(
    window_id: u32,
    pid: Option<i32>,
    expected_window: u32,
    expected_pid: i32,
) -> bool {
    window_id == expected_window && pid == Some(expected_pid)
}

fn configured_capture_frame(
    filter: &SCContentFilter,
    expected_bounds: crate::discovery::WindowBounds,
) -> Option<(crate::discovery::WindowBounds, f64, usize, usize)> {
    let content_rect = unsafe { filter.contentRect() };
    let scale = f64::from(unsafe { filter.pointPixelScale() }).max(1.0);
    capture_frame_if_pinned(
        crate::discovery::WindowBounds {
            x: content_rect.origin.x,
            y: content_rect.origin.y,
            width: content_rect.size.width,
            height: content_rect.size.height,
        },
        scale,
        expected_bounds,
    )
}

fn capture_frame_if_pinned(
    frame_bounds: crate::discovery::WindowBounds,
    scale: f64,
    expected_bounds: crate::discovery::WindowBounds,
) -> Option<(crate::discovery::WindowBounds, f64, usize, usize)> {
    crate::ax::same_bounds(frame_bounds, expected_bounds).then(|| {
        (
            frame_bounds,
            scale,
            (frame_bounds.width * scale).round().max(1.0) as usize,
            (frame_bounds.height * scale).round().max(1.0) as usize,
        )
    })
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
    let bytes = encode_png_bytes(image)?;
    png_frame(
        bytes,
        image,
        bounds,
        scale,
        requested_width,
        requested_height,
    )
}

fn encode_png_bytes(image: &CGImage) -> Result<Vec<u8>, AdapterError> {
    png_bytes_from_data(allocate_png_data()?, image)
}

fn png_bytes_from_data(
    data: objc2_core_foundation::CFRetained<CFMutableData>,
    image: &CGImage,
) -> Result<Vec<u8>, AdapterError> {
    write_png_image(&data, image)?;
    let data: &CFData = &data;
    Ok(data.to_vec())
}

fn allocate_png_data() -> Result<objc2_core_foundation::CFRetained<CFMutableData>, AdapterError> {
    CFMutableData::new(None, 0)
        .ok_or_else(|| adapter_error("observation_failed", "could not allocate PNG data"))
}

fn write_png_image(data: &CFMutableData, image: &CGImage) -> Result<(), AdapterError> {
    let destination = png_destination(data)?;
    unsafe {
        destination.add_image(image, None);
    }
    finalize_png(&destination)
}

fn png_destination(
    data: &CFMutableData,
) -> Result<objc2_core_foundation::CFRetained<CGImageDestination>, AdapterError> {
    let png = CFString::from_str("public.png");
    unsafe { CGImageDestination::with_data(data, &png, 1, None) }
        .ok_or_else(|| adapter_error("observation_failed", "could not create PNG encoder"))
}

fn finalize_png(destination: &CGImageDestination) -> Result<(), AdapterError> {
    unsafe { destination.finalize() }
        .then_some(())
        .ok_or_else(|| adapter_error("observation_failed", "ImageIO could not finalize the PNG"))
}

fn png_frame(
    bytes: Vec<u8>,
    image: &CGImage,
    bounds: crate::discovery::WindowBounds,
    scale: f64,
    requested_width: usize,
    requested_height: usize,
) -> Result<Capture, AdapterError> {
    assembled_png(
        bytes,
        png_width(image)?,
        image,
        bounds,
        scale,
        requested_width,
        requested_height,
    )
}

fn assembled_png(
    bytes: Vec<u8>,
    width: u32,
    image: &CGImage,
    bounds: crate::discovery::WindowBounds,
    scale: f64,
    requested_width: usize,
    requested_height: usize,
) -> Result<Capture, AdapterError> {
    capture_from_png(
        bytes,
        width,
        png_height(image)?,
        bounds,
        scale,
        requested_width,
        requested_height,
    )
}

fn capture_from_png(
    bytes: Vec<u8>,
    width: u32,
    height: u32,
    bounds: crate::discovery::WindowBounds,
    scale: f64,
    requested_width: usize,
    requested_height: usize,
) -> Result<Capture, AdapterError> {
    reject_empty_or_mismatched_png(width, height, requested_width, requested_height)?;
    Ok(Capture {
        bytes,
        width,
        height,
        bounds,
        scale,
    })
}

fn png_width(image: &CGImage) -> Result<u32, AdapterError> {
    png_dimension(CGImage::width(Some(image)), "width")
}

fn png_height(image: &CGImage) -> Result<u32, AdapterError> {
    png_dimension(CGImage::height(Some(image)), "height")
}

fn png_dimension(value: usize, name: &str) -> Result<u32, AdapterError> {
    u32::try_from(value).map_err(|_| {
        adapter_error(
            "observation_failed",
            &format!("captured {name} overflowed u32"),
        )
    })
}

fn reject_empty_or_mismatched_png(
    width: u32,
    height: u32,
    requested_width: usize,
    requested_height: usize,
) -> Result<(), AdapterError> {
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
    Ok(())
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
    Some(FrameAuthority {
        signature: reply.frame_signature.clone()?,
        bounds: frame_bounds(reply.response.get("frame_bounds")?)?,
        width: reply.screenshot_width?,
        height: reply.screenshot_height?,
        scale: number_field(&reply.response, "point_pixel_scale")?,
    })
}

fn frame_bounds(value: &Value) -> Option<crate::discovery::WindowBounds> {
    Some(crate::discovery::WindowBounds {
        x: number_field(value, "x")?,
        y: number_field(value, "y")?,
        width: number_field(value, "width")?,
        height: number_field(value, "height")?,
    })
}

fn number_field(value: &Value, field: &str) -> Option<f64> {
    value.get(field)?.as_f64()
}

pub(crate) fn prepare_point(
    record: &WindowRecord,
    context: &AdapterContext,
    operation: &AdapterOperation,
    authority: Option<&FrameAuthority>,
    cancellation: &AtomicBool,
) -> Result<AdapterOperation, AdapterError> {
    let Some(locator) = point_locator(&operation.input) else {
        return Ok(operation.clone());
    };
    prepare_point_locator(record, context, operation, locator, authority, cancellation)
}

fn point_locator(input: &Value) -> Option<&Value> {
    input
        .get("locator")
        .filter(|locator| locator.get("kind").and_then(Value::as_str) == Some("point"))
}

fn prepare_point_locator(
    record: &WindowRecord,
    context: &AdapterContext,
    operation: &AdapterOperation,
    locator: &Value,
    authority: Option<&FrameAuthority>,
    cancellation: &AtomicBool,
) -> Result<AdapterOperation, AdapterError> {
    validate_and_prepare_point(
        record,
        context,
        operation,
        locator,
        required_frame_authority(authority)?,
        cancellation,
    )
}

fn validate_and_prepare_point(
    record: &WindowRecord,
    context: &AdapterContext,
    operation: &AdapterOperation,
    locator: &Value,
    authority: &FrameAuthority,
    cancellation: &AtomicBool,
) -> Result<AdapterOperation, AdapterError> {
    validate_frame_authority(record, context, authority, cancellation)?;
    apply_prepared_point(operation, locator, authority)
}

fn required_frame_authority(
    authority: Option<&FrameAuthority>,
) -> Result<&FrameAuthority, AdapterError> {
    authority.ok_or_else(|| {
        adapter_error(
            "frame_stale",
            "the session has no matching native screenshot frame authority",
        )
    })
}

fn apply_prepared_point(
    operation: &AdapterOperation,
    locator: &Value,
    authority: &FrameAuthority,
) -> Result<AdapterOperation, AdapterError> {
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
    require_matching_frame(
        context.frame_token.as_deref(),
        authority,
        &current_frame_facts(record, context, cancellation)?,
    )
}

fn require_matching_frame(
    frame_token: Option<&str>,
    authority: &FrameAuthority,
    current: &FrameFacts,
) -> Result<(), AdapterError> {
    if frame_authority_is_current(frame_token, authority, current) {
        Ok(())
    } else {
        Err(adapter_error(
            "frame_stale",
            "the native window geometry or scale changed after the screenshot",
        ))
    }
}

fn frame_authority_is_current(
    frame_token: Option<&str>,
    authority: &FrameAuthority,
    current: &FrameFacts,
) -> bool {
    let signature = frame_token.and_then(|token| token.rsplit('_').next());
    let current_signature =
        frame_signature(current.bounds, current.width, current.height, current.scale);
    signature == Some(authority.signature.as_str())
        && current_signature == authority.signature
        && crate::ax::same_bounds(current.bounds, authority.bounds)
        && current.width == authority.width
        && current.height == authority.height
        && (current.scale - authority.scale).abs() <= f64::EPSILON
}

fn point_coordinates(locator: &Value) -> Result<(f64, f64), AdapterError> {
    Ok((
        required_point_coord(locator, "x")?,
        required_point_coord(locator, "y")?,
    ))
}

fn required_point_coord(locator: &Value, field: &str) -> Result<f64, AdapterError> {
    locator
        .get(field)
        .and_then(Value::as_f64)
        .ok_or_else(|| adapter_error("invalid_request", &format!("point {field} is required")))
}

fn validate_point_bounds(x: f64, y: f64, authority: &FrameAuthority) -> Result<(), AdapterError> {
    if point_in_frame(x, y, authority.width, authority.height) {
        Ok(())
    } else {
        Err(adapter_error(
            "element_not_found",
            "point lies outside the exact screenshot pixel bounds",
        ))
    }
}

fn point_in_frame(x: f64, y: f64, width: u32, height: u32) -> bool {
    x >= 0.0 && y >= 0.0 && x < f64::from(width) && y < f64::from(height)
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
    recv_frame_facts(record, context, cancellation)
}

fn recv_frame_facts(
    record: &WindowRecord,
    context: &AdapterContext,
    cancellation: &AtomicBool,
) -> Result<FrameFacts, AdapterError> {
    let (sender, receiver, _terminal_scope) = callback_channel();
    request_frame_facts(record, sender);
    wait_frame_facts(context, cancellation, &receiver)
}

fn wait_frame_facts(
    context: &AdapterContext,
    cancellation: &AtomicBool,
    receiver: &mpsc::Receiver<Result<FrameFacts, AdapterError>>,
) -> Result<FrameFacts, AdapterError> {
    loop {
        if let Some(result) = frame_facts_wait_tick(context, cancellation, receiver) {
            return result;
        }
    }
}

fn request_frame_facts(
    record: &WindowRecord,
    sender: CallbackMailbox<Result<FrameFacts, AdapterError>>,
) {
    let expected_pid = record.snapshot.pid;
    let expected_window = record.snapshot.window_id;
    let completion = RcBlock::new(
        move |content: *mut SCShareableContent, error: *mut NSError| {
            let _ = sender.send(frame_facts_from_content(
                content,
                error,
                expected_pid,
                expected_window,
            ));
        },
    );
    unsafe {
        SCShareableContent::getShareableContentExcludingDesktopWindows_onScreenWindowsOnly_completionHandler(
            true,
            false,
            &completion,
        );
    }
}

fn frame_facts_from_content(
    content: *mut SCShareableContent,
    error: *mut NSError,
    expected_pid: i32,
    expected_window: u32,
) -> Result<FrameFacts, AdapterError> {
    if content.is_null() {
        return Err(ns_error(
            error,
            "ScreenCaptureKit did not return shareable content",
        ));
    }
    frame_facts_from_windows(unsafe { &*content }, expected_pid, expected_window)
}

fn frame_facts_from_windows(
    content: &SCShareableContent,
    expected_pid: i32,
    expected_window: u32,
) -> Result<FrameFacts, AdapterError> {
    let windows = unsafe { content.windows() };
    let window = windows.iter().find(|window| {
        shareable_window_matches(
            unsafe { window.windowID() },
            unsafe {
                window
                    .owningApplication()
                    .map(|application| application.processID())
            },
            expected_window,
            expected_pid,
        )
    });
    let Some(window) = window else {
        return Err(adapter_error(
            "target_stale",
            "the pinned process/window pair is absent from ScreenCaptureKit",
        ));
    };
    frame_facts_from_window(&window)
}

fn frame_facts_from_window(
    window: &objc2_screen_capture_kit::SCWindow,
) -> Result<FrameFacts, AdapterError> {
    let filter = unsafe {
        SCContentFilter::initWithDesktopIndependentWindow(SCContentFilter::alloc(), window)
    };
    Ok(frame_facts_from_filter(&filter))
}

fn frame_facts_from_filter(filter: &SCContentFilter) -> FrameFacts {
    let content_rect = unsafe { filter.contentRect() };
    let scale = crate::seam::frame_scale(f64::from(unsafe { filter.pointPixelScale() }).max(1.0));
    FrameFacts {
        bounds: crate::discovery::WindowBounds {
            x: content_rect.origin.x,
            y: content_rect.origin.y,
            width: content_rect.size.width,
            height: content_rect.size.height,
        },
        width: (content_rect.size.width * scale).round().max(1.0) as u32,
        height: (content_rect.size.height * scale).round().max(1.0) as u32,
        scale,
    }
}

fn frame_facts_wait_tick(
    context: &AdapterContext,
    cancellation: &AtomicBool,
    receiver: &mpsc::Receiver<Result<FrameFacts, AdapterError>>,
) -> Option<Result<FrameFacts, AdapterError>> {
    if cancellation.load(Ordering::SeqCst) {
        return Some(Err(adapter_error(
            "cancelled",
            "request was cancelled during frame-authority validation",
        )));
    }
    recv_frame_facts_once(context, receiver)
}

fn recv_frame_facts_once(
    context: &AdapterContext,
    receiver: &mpsc::Receiver<Result<FrameFacts, AdapterError>>,
) -> Option<Result<FrameFacts, AdapterError>> {
    let remaining = context.deadline.saturating_duration_since(Instant::now());
    if remaining.is_zero() {
        return Some(Err(adapter_error(
            "timed_out",
            "frame-authority validation did not complete before the deadline",
        )));
    }
    match receiver.recv_timeout(remaining.min(Duration::from_millis(5))) {
        Ok(result) => Some(result),
        Err(mpsc::RecvTimeoutError::Timeout) => None,
        Err(mpsc::RecvTimeoutError::Disconnected) => Some(Err(adapter_error(
            "observation_failed",
            "frame-authority completion channel disconnected",
        ))),
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
    fn screenshot_and_frame_token_keep_public_results() {
        let bounds = crate::discovery::WindowBounds {
            x: 10.0,
            y: 20.0,
            width: 300.0,
            height: 200.0,
        };
        let mut reply = AdapterReply::confirmed(
            json!({
                "frame_bounds": bounds,
                "point_pixel_scale": 2.0,
            }),
            Some(vec![1, 2, 3]),
        );
        reply.screenshot_width = Some(600);
        reply.screenshot_height = Some(400);
        let signature = frame_signature(bounds, 600, 400, 2.0);
        reply.frame_signature = Some(signature.clone());
        let authority = authority(&reply).expect("complete screenshot reply has authority");
        assert_eq!(authority.width, 600);
        assert_eq!(authority.height, 400);
        assert_eq!(authority.scale, 2.0);
        assert_eq!(authority.bounds, bounds);

        let current = FrameFacts {
            bounds,
            width: 600,
            height: 400,
            scale: 2.0,
        };
        let token = format!("frame_{signature}");
        assert!(frame_authority_is_current(
            Some(token.as_str()),
            &authority,
            &current
        ));
        assert!(!frame_authority_is_current(
            Some("frame_other"),
            &authority,
            &current
        ));
        assert!(!frame_authority_is_current(None, &authority, &current));
        let mut moved = current;
        moved.bounds.x = 99.0;
        assert!(!frame_authority_is_current(
            Some("frame_sigsigsigsigsig0"),
            &authority,
            &moved
        ));

        assert!(point_in_frame(0.0, 0.0, 600, 400));
        assert!(!point_in_frame(-1.0, 0.0, 600, 400));
        assert!(!point_in_frame(600.0, 0.0, 600, 400));
        assert!(validate_point_bounds(10.0, 10.0, &authority).is_ok());
        assert_eq!(
            validate_point_bounds(600.0, 10.0, &authority)
                .unwrap_err()
                .code,
            "element_not_found"
        );
        assert_eq!(
            required_frame_authority(None).err().unwrap().code,
            "frame_stale"
        );
        let click = AdapterOperation::new(
            "action.click".to_owned(),
            json!({"locator": {"kind": "ref", "ref": "e_n_1"}}),
        );
        assert!(point_locator(&click.input).is_none());
        let point = AdapterOperation::new(
            "action.click".to_owned(),
            json!({"locator": {"kind": "point", "x": 12.0, "y": 34.0}}),
        );
        let prepared =
            apply_prepared_point(&point, point_locator(&point.input).unwrap(), &authority).unwrap();
        assert_eq!(prepared.prepared.unwrap()["global_x"], 16.0);

        assert!(reject_empty_or_mismatched_png(600, 400, 600, 400).is_ok());
        assert_eq!(
            reject_empty_or_mismatched_png(0, 400, 600, 400)
                .unwrap_err()
                .code,
            "observation_failed"
        );
        assert_eq!(
            reject_empty_or_mismatched_png(600, 400, 601, 400)
                .unwrap_err()
                .code,
            "observation_failed"
        );
        assert!(shareable_window_matches(7, Some(42), 7, 42));
        assert!(!shareable_window_matches(7, None, 7, 42));
        assert!(capture_frame_if_pinned(bounds, 2.0, bounds).is_some());
        assert!(
            capture_frame_if_pinned(
                crate::discovery::WindowBounds { x: 99.0, ..bounds },
                2.0,
                bounds
            )
            .is_none()
        );

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
                pid: 1,
                window_id: 1,
                owner: "Fixture".to_owned(),
                title: None,
                bounds,
                is_on_screen: true,
            },
            present: true,
        };
        let cancelled = screenshot_preflight(&record, &AtomicBool::new(true)).unwrap();
        assert_eq!(cancelled.error.unwrap().code, "cancelled");

        let (sender, receiver) = mpsc::channel();
        drop(sender);
        let context = manuvra_runtime::AdapterContext {
            session_id: "s".to_owned(),
            target_id: "macos_test".to_owned(),
            target_generation: 1,
            action_sequence: 1,
            reference_namespace: "n".to_owned(),
            reference_epoch: 1,
            frame_token: None,
            mode: manuvra_runtime::ExecutionMode::Background,
            deadline: Instant::now() + Duration::from_secs(1),
        };
        assert_eq!(
            recv_capture_once(&record, &context, Instant::now(), &receiver)
                .unwrap()
                .error
                .unwrap()
                .code,
            "observation_failed"
        );
        let (sender, receiver) = mpsc::channel();
        drop(sender);
        assert_eq!(
            recv_capture(
                &record,
                &context,
                &AtomicBool::new(false),
                Instant::now(),
                &receiver
            )
            .error
            .unwrap()
            .code,
            "observation_failed"
        );
        let (_sender, receiver) = mpsc::channel();
        let soon = manuvra_runtime::AdapterContext {
            deadline: Instant::now() + Duration::from_millis(12),
            ..context.clone()
        };
        assert_eq!(
            recv_capture(
                &record,
                &soon,
                &AtomicBool::new(false),
                Instant::now(),
                &receiver
            )
            .error
            .unwrap()
            .code,
            "timed_out"
        );
        let (sender, receiver) = mpsc::channel();
        sender
            .send(Err(adapter_error("target_stale", "missing")))
            .unwrap();
        assert_eq!(
            recv_capture_once(&record, &context, Instant::now(), &receiver)
                .unwrap()
                .error
                .unwrap()
                .code,
            "target_stale"
        );
        let expired = manuvra_runtime::AdapterContext {
            deadline: Instant::now(),
            ..context.clone()
        };
        assert_eq!(
            recv_capture_once(&record, &expired, Instant::now(), &receiver)
                .unwrap()
                .error
                .unwrap()
                .code,
            "timed_out"
        );
        assert_eq!(
            capture_wait_tick(
                &record,
                &context,
                &AtomicBool::new(true),
                Instant::now(),
                &receiver
            )
            .unwrap()
            .error
            .unwrap()
            .code,
            "cancelled"
        );
        assert!(super::authority(&AdapterReply::confirmed(json!({}), None)).is_none());
        let (sender, receiver) = mpsc::channel();
        drop(sender);
        assert_eq!(
            recv_frame_facts_once(&context, &receiver)
                .unwrap()
                .err()
                .unwrap()
                .code,
            "observation_failed"
        );
        let (sender, receiver) = mpsc::channel();
        drop(sender);
        assert_eq!(
            wait_frame_facts(&context, &AtomicBool::new(false), &receiver)
                .err()
                .unwrap()
                .code,
            "observation_failed"
        );
        let (_sender, receiver) = mpsc::channel();
        let soon = manuvra_runtime::AdapterContext {
            deadline: Instant::now() + Duration::from_millis(12),
            ..context.clone()
        };
        assert_eq!(
            wait_frame_facts(&soon, &AtomicBool::new(false), &receiver)
                .err()
                .unwrap()
                .code,
            "timed_out"
        );
        let expired_facts = manuvra_runtime::AdapterContext {
            deadline: Instant::now(),
            ..context.clone()
        };
        assert_eq!(
            recv_frame_facts_once(&expired_facts, &receiver)
                .unwrap()
                .err()
                .unwrap()
                .code,
            "timed_out"
        );
        assert_eq!(
            frame_facts_wait_tick(&context, &AtomicBool::new(true), &receiver)
                .unwrap()
                .err()
                .unwrap()
                .code,
            "cancelled"
        );
        let passthrough =
            prepare_point(&record, &context, &click, None, &AtomicBool::new(false)).unwrap();
        assert!(passthrough.prepared.is_none());
        assert_eq!(
            required_point_coord(&json!({"kind": "point"}), "x")
                .unwrap_err()
                .code,
            "invalid_request"
        );
        crate::test_oracles::write(
            "frame-token.json",
            &json!({
                "schema": "manuvra/cp-07-boundary-oracle@1",
                "case": "native_frame_token",
                "current_token_matches": true,
                "stale_token": "frame_stale",
                "point_outside_bounds": "element_not_found",
                "missing_authority": "frame_stale",
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
