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
        let request = RequestContext {
            session_id: context.session_id.clone(),
            action_sequence: context.action_sequence,
            command: operation.command.clone(),
            mode: context.mode.as_str().to_owned(),
        };
        let previous = REQUEST.replace(Some(request));
        record("focus_before", crate::ax::focused_window_snapshot());
        record("operation_begin", Value::Null);
        let reply = action();
        record(
            "operation_end",
            json!({
                "delivery": delivery_name(&reply.delivery),
                "interrupted": reply.interrupted,
                "error_code": reply.error.as_ref().map(|error| error.code.as_str()),
            }),
        );
        record("focus_after", crate::ax::focused_window_snapshot());
        REQUEST.replace(previous);
        reply
    }

    pub(crate) fn record(kind: &str, details: Value) {
        let Some(path) = std::env::var_os("MANUVRA_CP07_ORACLE_PATH") else {
            return;
        };
        REQUEST.with_borrow(|request| {
            let Some(request) = request else { return };
            let row = json!({
                "monotonic_ms": STARTED.get_or_init(Instant::now).elapsed().as_millis() as u64,
                "pid": std::process::id(),
                "session_id": request.session_id,
                "action_sequence": request.action_sequence,
                "command": request.command,
                "mode": request.mode,
                "kind": kind,
                "details": details,
            });
            let _guard = WRITE_LOCK.lock().expect("CP-07 oracle write lock");
            if let Ok(mut file) = OpenOptions::new().create(true).append(true).open(path) {
                let _ = writeln!(file, "{row}");
            }
        });
    }

    pub(crate) fn barrier(name: &str, deadline: Instant, cancellation: &AtomicBool) {
        let Some(path) = std::env::var_os("MANUVRA_CP07_BARRIER_PATH") else {
            return;
        };
        let Ok(config) = fs::read(&path).and_then(|bytes| {
            serde_json::from_slice::<Value>(&bytes)
                .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))
        }) else {
            return;
        };
        let matches = REQUEST.with_borrow(|request| {
            request.as_ref().is_some_and(|request| {
                config["name"] == name
                    && config["session_id"] == request.session_id
                    && config["action_sequence"] == request.action_sequence
            })
        });
        if !matches {
            return;
        }
        let Some(reached_path) = config["reached_path"].as_str() else {
            return;
        };
        let Some(release_path) = config["release_path"].as_str() else {
            return;
        };
        record("barrier_reached", json!({"name": name}));
        let _ = fs::write(reached_path, b"reached");
        while Instant::now() < deadline && !cancellation.load(Ordering::SeqCst) {
            if std::path::Path::new(release_path).is_file() {
                record("barrier_released", json!({"name": name}));
                return;
            }
            std::thread::park_timeout(Duration::from_millis(5));
        }
        record("barrier_aborted", json!({"name": name}));
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
        let request = RequestContext {
            session_id: context.session_id.clone(),
            action_sequence: context.action_sequence,
            command: operation.command.clone(),
            mode: context.mode.as_str().to_owned(),
        };
        let previous = REQUEST.replace(Some(request));
        barrier(name, context.deadline, cancellation);
        REQUEST.replace(previous);
    }

    fn delivery_name(delivery: &AdapterDelivery) -> &'static str {
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
