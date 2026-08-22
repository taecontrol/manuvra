use crate::ax::{self, adapter_error};
use crate::discovery::WindowRecord;
use manuvra_runtime::AdapterError;
use objc2_core_foundation::{
    CFRetained, CFRunLoop, CFRunLoopSource, CFString, kCFRunLoopDefaultMode,
};
use std::ffi::c_void;
use std::ptr::{self, NonNull};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

const AX_SUCCESS: i32 = 0;
const AX_NOTIFICATION_UNSUPPORTED: i32 = -25205;
const QUIET_WINDOW: Duration = Duration::from_millis(50);
const APPLICATION_NOTIFICATIONS: &[&str] = &["AXFocusedWindowChanged"];
const WINDOW_NOTIFICATIONS: &[&str] = &[
    "AXValueChanged",
    "AXTitleChanged",
    "AXUIElementDestroyed",
    "AXMoved",
    "AXResized",
];
const ELEMENT_NOTIFICATIONS: &[&str] = &["AXValueChanged", "AXUIElementDestroyed"];
const TEXT_NOTIFICATIONS: &[&str] = &["AXSelectedTextChanged"];

pub(crate) struct ObservationFence {
    observer: NonNull<c_void>,
    run_loop: CFRetained<CFRunLoop>,
    source: NonNull<CFRunLoopSource>,
    state: Box<ObserverState>,
    registration_count: usize,
}

struct ObserverState {
    event_count: AtomicU64,
    terminal: AtomicBool,
}

impl ObserverState {
    fn record_notification(&self) -> bool {
        if self.terminal.load(Ordering::Acquire) {
            return false;
        }
        self.event_count.fetch_add(1, Ordering::SeqCst);
        true
    }
}

impl ObservationFence {
    pub fn install(record: &WindowRecord) -> Result<Self, AdapterError> {
        let created = create_observer(record.snapshot.pid)?;
        attach_observer(record, created)
    }

    pub fn wait_for_quiet(
        &self,
        deadline: Instant,
        cancellation: &AtomicBool,
    ) -> Result<u64, AdapterError> {
        wait_quiet(
            &self.state,
            deadline,
            cancellation,
            self.state.event_count.load(Ordering::SeqCst),
            Instant::now(),
        )
    }

    pub fn cursor(&self) -> u64 {
        self.state.event_count.load(Ordering::SeqCst)
    }

    pub fn registration_count(&self) -> usize {
        self.registration_count
    }
}

fn wait_quiet(
    state: &ObserverState,
    deadline: Instant,
    cancellation: &AtomicBool,
    mut observed: u64,
    mut last_change: Instant,
) -> Result<u64, AdapterError> {
    loop {
        if let Some(result) = quiet_tick(deadline, cancellation, observed, last_change) {
            return result;
        }
        CFRunLoop::run_in_mode(
            unsafe { kCFRunLoopDefaultMode },
            quiet_slice(deadline).as_secs_f64(),
            true,
        );
        update_quiet_cursor(state, &mut observed, &mut last_change);
    }
}

fn quiet_tick(
    deadline: Instant,
    cancellation: &AtomicBool,
    observed: u64,
    last_change: Instant,
) -> Option<Result<u64, AdapterError>> {
    if cancellation.load(Ordering::SeqCst) {
        return Some(Err(adapter_error(
            "cancelled",
            "request was cancelled while awaiting native AX quiet",
        )));
    }
    quiet_deadline(deadline, observed, last_change)
}

fn quiet_deadline(
    deadline: Instant,
    observed: u64,
    last_change: Instant,
) -> Option<Result<u64, AdapterError>> {
    if last_change.elapsed() >= QUIET_WINDOW {
        return Some(Ok(observed));
    }
    (Instant::now() >= deadline).then(|| {
        Err(adapter_error(
            "stabilization_timeout",
            "AX notifications did not become quiet before the deadline",
        ))
    })
}

fn quiet_slice(deadline: Instant) -> Duration {
    deadline
        .saturating_duration_since(Instant::now())
        .min(Duration::from_millis(5))
}

fn update_quiet_cursor(state: &ObserverState, observed: &mut u64, last_change: &mut Instant) {
    let current = state.event_count.load(Ordering::SeqCst);
    if current != *observed {
        *observed = current;
        *last_change = Instant::now();
    }
}

struct CreatedObserver {
    observer: NonNull<c_void>,
    run_loop: CFRetained<CFRunLoop>,
    source: NonNull<CFRunLoopSource>,
    state: Box<ObserverState>,
}

fn create_observer(pid: i32) -> Result<CreatedObserver, AdapterError> {
    attach_observer_source(created_observer_ptr(pid)?)
}

fn attach_observer_source(observer: NonNull<c_void>) -> Result<CreatedObserver, AdapterError> {
    let run_loop = CFRunLoop::current()
        .ok_or_else(|| adapter_error("observation_failed", "target worker has no CFRunLoop"))?;
    Ok(CreatedObserver {
        observer,
        run_loop: run_loop.clone(),
        source: observer_source(observer, &run_loop)?,
        state: Box::new(ObserverState {
            event_count: AtomicU64::new(0),
            terminal: AtomicBool::new(false),
        }),
    })
}

fn observer_source(
    observer: NonNull<c_void>,
    run_loop: &CFRunLoop,
) -> Result<NonNull<CFRunLoopSource>, AdapterError> {
    let source = NonNull::new(unsafe { AXObserverGetRunLoopSource(observer.as_ptr()) }.cast())
        .ok_or_else(|| adapter_error("observation_failed", "AXObserver has no run-loop source"))?;
    // SAFETY: the observer owns its source for the lifetime of this fence.
    run_loop.add_source(Some(unsafe { source.as_ref() }), unsafe {
        kCFRunLoopDefaultMode
    });
    Ok(source)
}

fn created_observer_ptr(pid: i32) -> Result<NonNull<c_void>, AdapterError> {
    let mut observer = ptr::null_mut();
    let status = unsafe { AXObserverCreate(pid, callback, &mut observer) };
    require_observer_status(status)?;
    NonNull::new(observer)
        .ok_or_else(|| adapter_error("observation_failed", "AXObserverCreate returned null"))
}

fn require_observer_status(status: i32) -> Result<(), AdapterError> {
    if status == AX_SUCCESS {
        Ok(())
    } else {
        Err(adapter_error(
            observer_create_error(status),
            &format!("AXObserverCreate failed with status {status}"),
        ))
    }
}

fn observer_create_error(status: i32) -> &'static str {
    if status == -25211 {
        "permission_required"
    } else {
        "observation_failed"
    }
}

fn attach_observer(
    record: &WindowRecord,
    created: CreatedObserver,
) -> Result<ObservationFence, AdapterError> {
    let CreatedObserver {
        observer,
        run_loop,
        source,
        state,
    } = created;
    let registration_count =
        register_notifications(record, observer, &*state as *const ObserverState)?;
    require_registrations(observer, &run_loop, source, registration_count)?;
    Ok(ObservationFence {
        observer,
        run_loop,
        source,
        state,
        registration_count,
    })
}

fn register_notifications(
    record: &WindowRecord,
    observer: NonNull<c_void>,
    state: *const ObserverState,
) -> Result<usize, AdapterError> {
    let elements = ax::observer_elements(record)?;
    let refcon = state.cast_mut().cast();
    Ok(elements
        .iter()
        .enumerate()
        .map(|(index, element)| add_element_notifications(observer, element, index, refcon))
        .sum())
}

fn add_element_notifications(
    observer: NonNull<c_void>,
    element: &ax::Element,
    index: usize,
    refcon: *mut c_void,
) -> usize {
    notifications(index, element)
        .into_iter()
        .filter(|notification| add_notification(observer, element, notification, refcon))
        .count()
}

fn add_notification(
    observer: NonNull<c_void>,
    element: &ax::Element,
    notification: &str,
    refcon: *mut c_void,
) -> bool {
    let notification = CFString::from_str(notification);
    let status = unsafe {
        AXObserverAddNotification(
            observer.as_ptr(),
            element.as_ptr(),
            CFRetained::as_ptr(&notification).as_ptr(),
            refcon,
        )
    };
    notification_registered(status)
}

fn notification_registered(status: i32) -> bool {
    match status {
        AX_SUCCESS => true,
        AX_NOTIFICATION_UNSUPPORTED => false,
        _ => false,
    }
}

fn require_registrations(
    observer: NonNull<c_void>,
    run_loop: &CFRunLoop,
    source: NonNull<CFRunLoopSource>,
    registration_count: usize,
) -> Result<(), AdapterError> {
    if registration_count > 0 {
        return Ok(());
    }
    run_loop.remove_source(Some(unsafe { source.as_ref() }), unsafe {
        kCFRunLoopDefaultMode
    });
    unsafe { CFRelease(observer.as_ptr()) };
    Err(adapter_error(
        "observation_failed",
        "AXObserver could not register any relevant notification",
    ))
}

fn notifications(index: usize, element: &ax::Element) -> Vec<&'static str> {
    match index {
        0 => APPLICATION_NOTIFICATIONS.to_vec(),
        1 => WINDOW_NOTIFICATIONS.to_vec(),
        _ => element_notifications(element),
    }
}

fn element_notifications(element: &ax::Element) -> Vec<&'static str> {
    let mut notifications = ELEMENT_NOTIFICATIONS.to_vec();
    if is_text_role(element.string("AXRole").as_deref()) {
        notifications.extend(TEXT_NOTIFICATIONS);
    }
    notifications
}

fn is_text_role(role: Option<&str>) -> bool {
    matches!(role, Some("AXTextArea" | "AXTextField" | "AXComboBox"))
}

impl Drop for ObservationFence {
    fn drop(&mut self) {
        self.state.terminal.store(true, Ordering::Release);
        self.run_loop
            .remove_source(Some(unsafe { self.source.as_ref() }), unsafe {
                kCFRunLoopDefaultMode
            });
        unsafe { CFRelease(self.observer.as_ptr()) };
    }
}

unsafe extern "C" fn callback(
    _observer: *mut c_void,
    _element: *const c_void,
    _notification: *const CFString,
    refcon: *mut c_void,
) {
    let Some(state) = NonNull::new(refcon.cast::<ObserverState>()) else {
        return;
    };
    // SAFETY: refcon points to the boxed state held by ObservationFence while its run-loop source
    // is installed. Drop removes the source before releasing that box.
    unsafe { state.as_ref() }.record_notification();
}

#[link(name = "ApplicationServices", kind = "framework")]
unsafe extern "C" {
    fn AXObserverCreate(
        pid: i32,
        callback: unsafe extern "C" fn(*mut c_void, *const c_void, *const CFString, *mut c_void),
        observer: *mut *mut c_void,
    ) -> i32;
    fn AXObserverGetRunLoopSource(observer: *mut c_void) -> *mut c_void;
    fn AXObserverAddNotification(
        observer: *mut c_void,
        element: *const c_void,
        notification: *const CFString,
        refcon: *mut c_void,
    ) -> i32;
}

#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    fn CFRelease(value: *const c_void);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quiet_window_is_the_accepted_fifty_milliseconds() {
        assert_eq!(QUIET_WINDOW, Duration::from_millis(50));
        assert!(ELEMENT_NOTIFICATIONS.contains(&"AXValueChanged"));
        assert_eq!(observer_create_error(-25211), "permission_required");
        assert_eq!(observer_create_error(-1), "observation_failed");
        assert!(require_observer_status(AX_SUCCESS).is_ok());
        assert_eq!(
            require_observer_status(-25211).unwrap_err().code,
            "permission_required"
        );
        let now = Instant::now();
        assert_eq!(
            quiet_tick(now + Duration::from_secs(1), &AtomicBool::new(true), 3, now)
                .unwrap()
                .unwrap_err()
                .code,
            "cancelled"
        );
        assert_eq!(
            quiet_tick(now, &AtomicBool::new(false), 3, now)
                .unwrap()
                .unwrap_err()
                .code,
            "stabilization_timeout"
        );
        assert_eq!(
            quiet_tick(
                now + Duration::from_secs(1),
                &AtomicBool::new(false),
                3,
                now - QUIET_WINDOW
            )
            .unwrap()
            .unwrap(),
            3
        );
        let state = ObserverState {
            event_count: AtomicU64::new(8),
            terminal: AtomicBool::new(false),
        };
        let mut observed = 4;
        let mut last_change = now;
        update_quiet_cursor(&state, &mut observed, &mut last_change);
        assert_eq!(observed, 8);
        assert_eq!(
            wait_quiet(
                &state,
                now + Duration::from_secs(1),
                &AtomicBool::new(false),
                8,
                now - QUIET_WINDOW
            )
            .unwrap(),
            8
        );
        assert_eq!(
            wait_quiet(&state, now, &AtomicBool::new(false), 8, now)
                .unwrap_err()
                .code,
            "stabilization_timeout"
        );
        assert_eq!(
            wait_quiet(
                &state,
                Instant::now() + Duration::from_millis(15),
                &AtomicBool::new(false),
                8,
                Instant::now()
            )
            .unwrap_err()
            .code,
            "stabilization_timeout"
        );
        assert_eq!(
            wait_quiet(
                &state,
                now + Duration::from_secs(1),
                &AtomicBool::new(true),
                8,
                now
            )
            .unwrap_err()
            .code,
            "cancelled"
        );
        assert_eq!(
            notifications(0, &ax::Element::application(1).unwrap()),
            APPLICATION_NOTIFICATIONS
        );
        assert_eq!(
            notifications(1, &ax::Element::application(1).unwrap()),
            WINDOW_NOTIFICATIONS
        );
        let element = ax::Element::application(std::process::id() as i32).unwrap();
        let names = notifications(2, &element);
        assert!(names.contains(&"AXValueChanged"));
        assert!(notification_registered(AX_SUCCESS));
        assert!(!notification_registered(AX_NOTIFICATION_UNSUPPORTED));
        assert!(!notification_registered(-1));
        assert!(!is_text_role(Some("AXButton")));
        assert!(is_text_role(Some("AXTextField")));
        assert!(
            ObservationFence::install(&crate::discovery::WindowRecord {
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
            })
            .is_err()
        );
    }

    #[test]
    fn late_ax_callback_is_ignored_after_terminal_completion() {
        let state = ObserverState {
            event_count: AtomicU64::new(4),
            terminal: AtomicBool::new(true),
        };
        assert!(!state.record_notification());
        assert_eq!(state.event_count.load(Ordering::SeqCst), 4);
        crate::test_oracles::write(
            "late-ax-callbacks.json",
            &serde_json::json!({
                "schema": "manuvra/cp-07-boundary-oracle@1",
                "case": "late_ax_callback_after_terminal",
                "callback_delivered": false,
                "action_sequence_unchanged": true,
                "event_count_before": 4,
                "event_count_after": state.event_count.load(Ordering::SeqCst),
            }),
        );
    }
}
