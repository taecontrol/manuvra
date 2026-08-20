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
        let state = Box::new(ObserverState {
            event_count: AtomicU64::new(0),
            terminal: AtomicBool::new(false),
        });
        let mut observer = ptr::null_mut();
        let status = unsafe { AXObserverCreate(record.snapshot.pid, callback, &mut observer) };
        if status != AX_SUCCESS {
            return Err(adapter_error(
                if status == -25211 {
                    "permission_required"
                } else {
                    "observation_failed"
                },
                &format!("AXObserverCreate failed with status {status}"),
            ));
        }
        let observer = NonNull::new(observer)
            .ok_or_else(|| adapter_error("observation_failed", "AXObserverCreate returned null"))?;
        let run_loop = CFRunLoop::current()
            .ok_or_else(|| adapter_error("observation_failed", "target worker has no CFRunLoop"))?;
        let source = NonNull::new(unsafe { AXObserverGetRunLoopSource(observer.as_ptr()) }.cast())
            .ok_or_else(|| {
                adapter_error("observation_failed", "AXObserver has no run-loop source")
            })?;
        // SAFETY: the observer owns its source for the lifetime of this fence.
        run_loop.add_source(Some(unsafe { source.as_ref() }), unsafe {
            kCFRunLoopDefaultMode
        });

        let elements = ax::observer_elements(record)?;
        let refcon = (&*state as *const ObserverState).cast_mut().cast();
        let mut registration_count = 0usize;
        for (index, element) in elements.iter().enumerate() {
            for notification in notifications(index, element) {
                let notification = CFString::from_str(notification);
                let status = unsafe {
                    AXObserverAddNotification(
                        observer.as_ptr(),
                        element.as_ptr(),
                        CFRetained::as_ptr(&notification).as_ptr(),
                        refcon,
                    )
                };
                if status == AX_SUCCESS {
                    registration_count += 1;
                } else if status != AX_NOTIFICATION_UNSUPPORTED {
                    // Individual elements legitimately reject many notifications. Other failures
                    // do not authorize a dispatch without at least one working registration.
                }
            }
        }
        if registration_count == 0 {
            run_loop.remove_source(Some(unsafe { source.as_ref() }), unsafe {
                kCFRunLoopDefaultMode
            });
            unsafe { CFRelease(observer.as_ptr()) };
            return Err(adapter_error(
                "observation_failed",
                "AXObserver could not register any relevant notification",
            ));
        }
        Ok(Self {
            observer,
            run_loop,
            source,
            state,
            registration_count,
        })
    }

    pub fn wait_for_quiet(
        &self,
        deadline: Instant,
        cancellation: &AtomicBool,
    ) -> Result<u64, AdapterError> {
        let mut observed = self.state.event_count.load(Ordering::SeqCst);
        let mut last_change = Instant::now();
        loop {
            if cancellation.load(Ordering::SeqCst) {
                return Err(adapter_error(
                    "cancelled",
                    "request was cancelled while awaiting native AX quiet",
                ));
            }
            if last_change.elapsed() >= QUIET_WINDOW {
                return Ok(observed);
            }
            if Instant::now() >= deadline {
                return Err(adapter_error(
                    "stabilization_timeout",
                    "AX notifications did not become quiet before the deadline",
                ));
            }
            let slice = deadline
                .saturating_duration_since(Instant::now())
                .min(Duration::from_millis(5));
            CFRunLoop::run_in_mode(unsafe { kCFRunLoopDefaultMode }, slice.as_secs_f64(), true);
            let current = self.state.event_count.load(Ordering::SeqCst);
            if current != observed {
                observed = current;
                last_change = Instant::now();
            }
        }
    }

    pub fn cursor(&self) -> u64 {
        self.state.event_count.load(Ordering::SeqCst)
    }

    pub fn registration_count(&self) -> usize {
        self.registration_count
    }
}

fn notifications(index: usize, element: &ax::Element) -> Vec<&'static str> {
    if index == 0 {
        return APPLICATION_NOTIFICATIONS.to_vec();
    }
    if index == 1 {
        return WINDOW_NOTIFICATIONS.to_vec();
    }
    let mut notifications = ELEMENT_NOTIFICATIONS.to_vec();
    if matches!(
        element.string("AXRole").as_deref(),
        Some("AXTextArea" | "AXTextField" | "AXComboBox")
    ) {
        notifications.extend(TEXT_NOTIFICATIONS);
    }
    notifications
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
