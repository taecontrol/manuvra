use manuvra_runtime::{AdapterContext, AdapterOperation, AdapterReply};
use serde_json::Value;
use std::sync::atomic::AtomicBool;
use std::time::Instant;

#[cfg(debug_assertions)]
mod debug {
    use super::*;
    use manuvra_runtime::AdapterDelivery;
    use serde_json::json;
    use std::cell::RefCell;
    use std::fs;
    use std::fs::OpenOptions;
    use std::io::Write;
    use std::sync::atomic::Ordering;
    use std::sync::{Mutex, OnceLock};
    use std::time::Duration;

    #[derive(Clone)]
    struct RequestContext {
        session_id: String,
        action_sequence: u64,
        command: String,
        mode: String,
    }

    thread_local! {
        static REQUEST: RefCell<Option<RequestContext>> = const { RefCell::new(None) };
    }

    static WRITE_LOCK: Mutex<()> = Mutex::new(());
    static STARTED: OnceLock<Instant> = OnceLock::new();

    pub(crate) fn within_request(
        context: &AdapterContext,
        operation: &AdapterOperation,
        action: impl FnOnce() -> AdapterReply,
    ) -> AdapterReply {
        if std::env::var_os("MANUVRA_CP07_ORACLE_PATH").is_none() {
            return action();
        }
        traced_request(context, operation, action)
    }

    fn traced_request(
        context: &AdapterContext,
        operation: &AdapterOperation,
        action: impl FnOnce() -> AdapterReply,
    ) -> AdapterReply {
        let previous = REQUEST.replace(Some(request_context(context, operation)));
        record("focus_before", crate::ax::focused_window_snapshot());
        record("operation_begin", Value::Null);
        let reply = action();
        record_operation_end(&reply);
        REQUEST.replace(previous);
        reply
    }

    fn request_context(context: &AdapterContext, operation: &AdapterOperation) -> RequestContext {
        RequestContext {
            session_id: context.session_id.clone(),
            action_sequence: context.action_sequence,
            command: operation.command.clone(),
            mode: context.mode.as_str().to_owned(),
        }
    }

    fn record_operation_end(reply: &AdapterReply) {
        record(
            "operation_end",
            json!({
                "delivery": delivery_name(&reply.delivery),
                "interrupted": reply.interrupted,
                "error_code": reply.error.as_ref().map(|error| error.code.as_str()),
            }),
        );
        record("focus_after", crate::ax::focused_window_snapshot());
    }

    pub(crate) fn record(kind: &str, details: Value) {
        let Some(path) = std::env::var_os("MANUVRA_CP07_ORACLE_PATH") else {
            return;
        };
        write_oracle_row(path, kind, details);
    }

    fn write_oracle_row(path: std::ffi::OsString, kind: &str, details: Value) {
        REQUEST.with_borrow(|request| {
            let Some(request) = request else { return };
            append_oracle_row(path, oracle_row(request, kind, details));
        });
    }

    fn oracle_row(request: &RequestContext, kind: &str, details: Value) -> Value {
        json!({
            "monotonic_ms": STARTED.get_or_init(Instant::now).elapsed().as_millis() as u64,
            "pid": std::process::id(),
            "session_id": request.session_id,
            "action_sequence": request.action_sequence,
            "command": request.command,
            "mode": request.mode,
            "kind": kind,
            "details": details,
        })
    }

    fn append_oracle_row(path: std::ffi::OsString, row: Value) {
        let _guard = WRITE_LOCK.lock().expect("CP-07 oracle write lock");
        if let Ok(mut file) = OpenOptions::new().create(true).append(true).open(path) {
            let _ = writeln!(file, "{row}");
        }
    }

    pub(crate) fn barrier(name: &str, deadline: Instant, cancellation: &AtomicBool) {
        let Some(config) = barrier_config() else {
            return;
        };
        if !barrier_matches(name, &config) {
            return;
        }
        wait_for_barrier_release(name, deadline, cancellation, &config);
    }

    fn barrier_config() -> Option<Value> {
        let path = std::env::var_os("MANUVRA_CP07_BARRIER_PATH")?;
        fs::read(&path)
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
    }

    fn barrier_matches(name: &str, config: &Value) -> bool {
        REQUEST.with_borrow(|request| {
            request.as_ref().is_some_and(|request| {
                config["name"] == name
                    && config["session_id"] == request.session_id
                    && config["action_sequence"] == request.action_sequence
            })
        })
    }

    fn wait_for_barrier_release(
        name: &str,
        deadline: Instant,
        cancellation: &AtomicBool,
        config: &Value,
    ) {
        let Some(reached_path) = config["reached_path"].as_str() else {
            return;
        };
        let Some(release_path) = config["release_path"].as_str() else {
            return;
        };
        record("barrier_reached", json!({"name": name}));
        let _ = fs::write(reached_path, b"reached");
        poll_barrier_release(name, deadline, cancellation, release_path);
    }

    fn poll_barrier_release(
        name: &str,
        deadline: Instant,
        cancellation: &AtomicBool,
        release_path: &str,
    ) {
        if !poll_until_released(name, deadline, cancellation, release_path) {
            record("barrier_aborted", json!({"name": name}));
        }
    }

    fn poll_until_released(
        name: &str,
        deadline: Instant,
        cancellation: &AtomicBool,
        release_path: &str,
    ) -> bool {
        wait_while_open(deadline, cancellation, || {
            barrier_released(name, release_path)
        })
    }

    pub(super) fn wait_while_open(
        deadline: Instant,
        cancellation: &AtomicBool,
        mut released: impl FnMut() -> bool,
    ) -> bool {
        loop {
            if !barrier_should_wait(deadline, cancellation) {
                return false;
            }
            if released() {
                return true;
            }
            std::thread::park_timeout(Duration::from_millis(5));
        }
    }

    fn barrier_should_wait(deadline: Instant, cancellation: &AtomicBool) -> bool {
        Instant::now() < deadline && !cancellation.load(Ordering::SeqCst)
    }

    fn barrier_released(name: &str, release_path: &str) -> bool {
        std::path::Path::new(release_path)
            .is_file()
            .then(|| {
                record("barrier_released", json!({"name": name}));
            })
            .is_some()
    }

    pub(crate) fn barrier_for(
        context: &AdapterContext,
        operation: &AdapterOperation,
        name: &str,
        cancellation: &AtomicBool,
    ) {
        if std::env::var_os("MANUVRA_CP07_BARRIER_PATH").is_none() {
            return;
        }
        let previous = REQUEST.replace(Some(request_context(context, operation)));
        barrier(name, context.deadline, cancellation);
        REQUEST.replace(previous);
    }

    pub(super) fn delivery_name(delivery: &AdapterDelivery) -> &'static str {
        match delivery {
            AdapterDelivery::Rejected => "rejected",
            AdapterDelivery::Confirmed => "confirmed",
            AdapterDelivery::Unknown => "unknown",
        }
    }
}

#[cfg(debug_assertions)]
pub(crate) use debug::{barrier, barrier_for, record, within_request};

#[cfg(not(debug_assertions))]
pub(crate) fn within_request(
    _context: &AdapterContext,
    _operation: &AdapterOperation,
    action: impl FnOnce() -> AdapterReply,
) -> AdapterReply {
    action()
}

#[cfg(not(debug_assertions))]
pub(crate) fn record(_kind: &str, _details: Value) {}

#[cfg(not(debug_assertions))]
pub(crate) fn barrier(_name: &str, _deadline: Instant, _cancellation: &AtomicBool) {}

#[cfg(not(debug_assertions))]
pub(crate) fn barrier_for(
    _context: &AdapterContext,
    _operation: &AdapterOperation,
    _name: &str,
    _cancellation: &AtomicBool,
) {
}

#[cfg(test)]
mod tests {
    use super::*;
    use manuvra_runtime::{AdapterOperation, AdapterReply, ExecutionMode};
    use serde_json::json;
    use std::sync::atomic::AtomicBool;
    use std::time::{Duration, Instant};

    #[test]
    fn oracle_helpers_are_noops_without_cp07_paths() {
        let context = AdapterContext {
            session_id: "s".to_owned(),
            target_id: "macos_test".to_owned(),
            target_generation: 1,
            action_sequence: 1,
            reference_namespace: "n".to_owned(),
            reference_epoch: 1,
            frame_token: None,
            mode: ExecutionMode::Background,
            deadline: Instant::now() + Duration::from_secs(1),
        };
        let operation = AdapterOperation::new("observe.tree".to_owned(), json!({}));
        let reply = within_request(&context, &operation, || {
            AdapterReply::confirmed(json!({"ok": true}), None)
        });
        assert_eq!(reply.response["ok"], true);
        record("unused", json!({}));
        barrier("during_capture", context.deadline, &AtomicBool::new(false));
        barrier_for(
            &context,
            &operation,
            "during_native_resolution",
            &AtomicBool::new(false),
        );
        assert_eq!(
            super::debug::delivery_name(&manuvra_runtime::AdapterDelivery::Rejected),
            "rejected"
        );
        assert_eq!(
            super::debug::delivery_name(&manuvra_runtime::AdapterDelivery::Confirmed),
            "confirmed"
        );
        assert_eq!(
            super::debug::delivery_name(&manuvra_runtime::AdapterDelivery::Unknown),
            "unknown"
        );
        assert!(super::debug::wait_while_open(
            Instant::now() + Duration::from_secs(1),
            &AtomicBool::new(false),
            || true
        ));
        assert!(!super::debug::wait_while_open(
            Instant::now(),
            &AtomicBool::new(false),
            || false
        ));
        assert!(!super::debug::wait_while_open(
            Instant::now() + Duration::from_secs(1),
            &AtomicBool::new(true),
            || false
        ));
        let mut polls = 0;
        assert!(super::debug::wait_while_open(
            Instant::now() + Duration::from_secs(1),
            &AtomicBool::new(false),
            || {
                polls += 1;
                polls > 1
            }
        ));
    }
}
