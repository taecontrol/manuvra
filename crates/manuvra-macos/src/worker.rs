use crate::ax;
use crate::capture;
use crate::discovery::WindowRecord;
use crate::observer::ObservationFence;
use manuvra_runtime::{AdapterContext, AdapterDelivery, AdapterOperation, AdapterReply};
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;
use std::time::Instant;

pub(crate) struct WorkerPool {
    workers: HashMap<String, Worker>,
}

struct Worker {
    generation: u64,
    sender: Sender<Message>,
    sessions: HashSet<String>,
}

#[derive(Clone)]
pub(crate) struct WorkerHandle {
    sender: Sender<Message>,
}

struct Request {
    record: WindowRecord,
    context: AdapterContext,
    operation: AdapterOperation,
    cancellation: Arc<AtomicBool>,
    reply: Sender<AdapterReply>,
}

struct PrepareRequest {
    record: WindowRecord,
    context: AdapterContext,
    operation: AdapterOperation,
    cancellation: Arc<AtomicBool>,
    reply: Sender<Result<AdapterOperation, manuvra_runtime::AdapterError>>,
}

struct PreparedMutation {
    session_id: String,
    action_sequence: u64,
    deadline: Instant,
    native: ax::PreparedAx,
    preflight_ms: u64,
    target_was_frontmost: bool,
    observer: ObservationFence,
}

enum Message {
    Prepare(PrepareRequest),
    Execute(Request),
    CloseSession(String),
    Shutdown,
}

impl WorkerPool {
    pub fn new() -> Self {
        Self {
            workers: HashMap::new(),
        }
    }

    pub fn handle(&mut self, record: &WindowRecord) -> WorkerHandle {
        WorkerHandle {
            sender: self.worker(record).sender.clone(),
        }
    }

    pub fn retain_present(&mut self, targets: &[manuvra_runtime::TargetDescriptor]) {
        let present = targets
            .iter()
            .map(|target| target.target_id.as_str())
            .collect::<HashSet<_>>();
        self.workers.retain(|target_id, worker| {
            let keep = present.contains(target_id.as_str());
            if !keep {
                let _ = worker.sender.send(Message::Shutdown);
            }
            keep
        });
    }

    pub fn session_opened(&mut self, record: &WindowRecord, session_id: &str) {
        self.worker(record).sessions.insert(session_id.to_owned());
    }

    pub fn session_closed(&mut self, target_id: &str, session_id: &str) {
        let should_remove = self.workers.get_mut(target_id).is_some_and(|worker| {
            worker.sessions.remove(session_id);
            let _ = worker
                .sender
                .send(Message::CloseSession(session_id.to_owned()));
            worker.sessions.is_empty()
        });
        if should_remove && let Some(worker) = self.workers.remove(target_id) {
            let _ = worker.sender.send(Message::Shutdown);
        }
    }

    fn worker(&mut self, record: &WindowRecord) -> &mut Worker {
        let target_id = record.descriptor.target_id.clone();
        let generation = record.descriptor.generation;
        let replace = self
            .workers
            .get(&target_id)
            .is_none_or(|worker| worker.generation != generation);
        if replace {
            if let Some(previous) = self.workers.remove(&target_id) {
                let _ = previous.sender.send(Message::Shutdown);
            }
            let (sender, receiver) = mpsc::channel();
            let thread_name = format!(
                "macos-target-{}-{}",
                record.snapshot.pid, record.snapshot.window_id
            );
            thread::Builder::new()
                .name(thread_name)
                .spawn(move || worker_loop(receiver))
                .expect("spawn macOS target worker");
            self.workers.insert(
                target_id.clone(),
                Worker {
                    generation,
                    sender,
                    sessions: HashSet::new(),
                },
            );
        }
        self.workers
            .get_mut(&target_id)
            .expect("worker was inserted")
    }
}

impl WorkerHandle {
    pub fn dispatch(
        &self,
        record: WindowRecord,
        context: &AdapterContext,
        operation: &AdapterOperation,
        cancellation: Arc<AtomicBool>,
    ) -> AdapterReply {
        if Instant::now() >= context.deadline {
            return unknown(
                "timed_out",
                "deadline elapsed before target worker dispatch",
            );
        }
        let (reply, receiver) = mpsc::channel();
        let request = Request {
            record,
            context: context.clone(),
            operation: operation.clone(),
            cancellation: cancellation.clone(),
            reply,
        };
        if self.sender.send(Message::Execute(request)).is_err() {
            return unknown(
                "transport_ambiguous",
                "macOS target worker stopped before dispatch",
            );
        }
        let remaining = context.deadline.saturating_duration_since(Instant::now());
        match receiver.recv_timeout(remaining) {
            Ok(reply) => reply,
            Err(mpsc::RecvTimeoutError::Timeout) => {
                cancellation.store(true, Ordering::SeqCst);
                unknown(
                    "timed_out",
                    "macOS target worker did not finish before the deadline",
                )
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => unknown(
                "transport_ambiguous",
                "macOS target worker stopped while an operation was in flight",
            ),
        }
    }

    pub fn prepare(
        &self,
        record: WindowRecord,
        context: &AdapterContext,
        operation: AdapterOperation,
        cancellation: Arc<AtomicBool>,
    ) -> Result<AdapterOperation, manuvra_runtime::AdapterError> {
        if cancellation.load(Ordering::SeqCst) {
            return Err(ax::adapter_error(
                "cancelled",
                "request was cancelled before native mutation preparation",
            ));
        }
        if Instant::now() >= context.deadline {
            return Err(ax::adapter_error(
                "timed_out",
                "deadline elapsed before native mutation preparation",
            ));
        }
        let (reply, receiver) = mpsc::channel();
        self.sender
            .send(Message::Prepare(PrepareRequest {
                record,
                context: context.clone(),
                operation,
                cancellation: cancellation.clone(),
                reply,
            }))
            .map_err(|_| {
                ax::adapter_error(
                    "transport_ambiguous",
                    "macOS target worker stopped before native preparation",
                )
            })?;
        let remaining = context.deadline.saturating_duration_since(Instant::now());
        match receiver.recv_timeout(remaining) {
            Ok(result) => result,
            Err(mpsc::RecvTimeoutError::Timeout) => {
                cancellation.store(true, Ordering::SeqCst);
                Err(ax::adapter_error(
                    "timed_out",
                    "native mutation preparation exceeded the request deadline",
                ))
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => Err(ax::adapter_error(
                "transport_ambiguous",
                "macOS target worker stopped during native preparation",
            )),
        }
    }
}

impl Drop for WorkerPool {
    fn drop(&mut self) {
        for worker in self.workers.values() {
            let _ = worker.sender.send(Message::Shutdown);
        }
    }
}

fn worker_loop(receiver: Receiver<Message>) {
    // Native observer ownership never leaves this target's serialized worker thread. Every
    // prepared mutation owns a distinct observer fence that is detached at terminal completion.
    let mut state = WorkerState::default();
    while let Ok(message) = receiver.recv() {
        state.prune();
        match message {
            Message::Prepare(request) => handle_prepare(&mut state, request),
            Message::Execute(request) => handle_execute(&mut state, request),
            Message::CloseSession(session_id) => state.close_session(&session_id),
            Message::Shutdown => break,
        }
    }
}

#[derive(Default)]
struct WorkerState {
    prepared: HashMap<String, PreparedMutation>,
    references: ax::ReferenceStore,
    next_token: u64,
}

impl WorkerState {
    fn prune(&mut self) {
        self.prepared
            .retain(|_, mutation| mutation.deadline > Instant::now());
    }

    fn close_session(&mut self, session_id: &str) {
        self.prepared
            .retain(|_, mutation| mutation.session_id != session_id);
        self.references.close_session(session_id);
    }

    fn next_token(&mut self, request: &PrepareRequest) -> String {
        self.next_token = self.next_token.saturating_add(1);
        format!(
            "p_{}_{}_{}",
            request.record.snapshot.pid, request.context.action_sequence, self.next_token
        )
    }
}

fn handle_prepare(state: &mut WorkerState, request: PrepareRequest) {
    let started = Instant::now();
    let result = prepare_request(state, &request, started);
    let _ = request.reply.send(result.map(|(mut operation, token)| {
        let details = operation
            .prepared
            .get_or_insert_with(|| Value::Object(serde_json::Map::new()));
        if let Some(details) = details.as_object_mut() {
            details.insert("macos_token".to_owned(), Value::String(token));
        }
        operation
    }));
}

fn prepare_request(
    state: &mut WorkerState,
    request: &PrepareRequest,
    started: Instant,
) -> Result<(AdapterOperation, String), manuvra_runtime::AdapterError> {
    if state.prepared.len() >= 128 {
        return Err(ax::adapter_error(
            "capability_unavailable",
            "native prepared-mutation table reached its bound",
        ));
    }
    crate::oracle::barrier_for(
        &request.context,
        &request.operation,
        "during_native_resolution",
        &request.cancellation,
    );
    if request.cancellation.load(Ordering::SeqCst) {
        return Err(ax::adapter_error(
            "cancelled",
            "request was cancelled during native resolution",
        ));
    }
    if request.cancellation.load(Ordering::SeqCst) {
        return Err(ax::adapter_error(
            "cancelled",
            "request was cancelled during native mutation preparation",
        ));
    }
    if Instant::now() >= request.context.deadline {
        return Err(ax::adapter_error(
            "timed_out",
            "deadline elapsed during native mutation preparation",
        ));
    }
    let observer = ObservationFence::install(&request.record)?;
    let native = ax::prepare_mutation(
        &request.record,
        &request.context,
        &request.operation,
        &state.references,
    )?;
    let token = state.next_token(request);
    state.prepared.insert(
        token.clone(),
        PreparedMutation {
            session_id: request.context.session_id.clone(),
            action_sequence: request.context.action_sequence,
            deadline: request.context.deadline,
            native,
            preflight_ms: started.elapsed().as_millis() as u64,
            target_was_frontmost: ax::application_is_frontmost(request.record.snapshot.pid)
                .unwrap_or(false),
            observer,
        },
    );
    Ok((request.operation.clone(), token))
}

fn handle_execute(state: &mut WorkerState, request: Request) {
    let reply = crate::oracle::within_request(&request.context, &request.operation, || {
        execute_request(state, &request)
    });
    let _ = request.reply.send(reply);
}

fn execute_request(state: &mut WorkerState, request: &Request) -> AdapterReply {
    if Instant::now() >= request.context.deadline {
        return unknown("timed_out", "deadline elapsed in the target worker queue");
    }
    let mutating = is_mutating(&request.operation.command);
    let prepared = match take_prepared(state, request, mutating) {
        Ok(prepared) => prepared,
        Err(error) => return ax::rejected_error(error),
    };
    if request.cancellation.load(Ordering::SeqCst) {
        return unknown(
            "cancelled",
            "request was cancelled before native mutation dispatch",
        );
    }
    let fence = prepared.as_ref().map(|mutation| &mutation.observer);
    let event_cursor_before = fence.map(ObservationFence::cursor).unwrap_or(0);
    let dispatch_started = Instant::now();
    let mut reply = dispatch_native(request, prepared.as_ref(), &mut state.references);
    reply.timing.preflight_ms = prepared
        .as_ref()
        .map(|mutation| mutation.preflight_ms)
        .unwrap_or(0);
    reply.timing.dispatch_ms = dispatch_started.elapsed().as_millis() as u64;
    detect_background_activation(request, prepared.as_ref(), &mut reply);
    settle_reply(request, fence, event_cursor_before, &mut reply);
    reply
}

fn take_prepared(
    state: &mut WorkerState,
    request: &Request,
    mutating: bool,
) -> Result<Option<PreparedMutation>, manuvra_runtime::AdapterError> {
    if !mutating {
        return Ok(None);
    }
    let token = request
        .operation
        .prepared
        .as_ref()
        .and_then(|details| details.get("macos_token"))
        .and_then(Value::as_str)
        .ok_or_else(|| {
            ax::adapter_error(
                "invalid_request",
                "native mutation is missing its prepared token",
            )
        })?;
    let prepared = state.prepared.remove(token).ok_or_else(|| {
        ax::adapter_error(
            "timed_out",
            "native prepared mutation expired or was already consumed",
        )
    })?;
    if prepared.session_id != request.context.session_id
        || prepared.action_sequence != request.context.action_sequence
    {
        return Err(ax::adapter_error(
            "invalid_request",
            "native prepared mutation authority does not match the admitted request",
        ));
    }
    Ok(Some(prepared))
}

fn dispatch_native(
    request: &Request,
    prepared: Option<&PreparedMutation>,
    references: &mut ax::ReferenceStore,
) -> AdapterReply {
    if let Some(prepared) = prepared {
        ax::invoke_prepared(
            &request.record,
            &request.context,
            &request.operation,
            &prepared.native,
            request.cancellation.clone(),
            references,
        )
    } else {
        ax::invoke(
            &request.record,
            &request.context,
            &request.operation,
            request.cancellation.clone(),
            references,
        )
    }
}

fn detect_background_activation(
    request: &Request,
    prepared: Option<&PreparedMutation>,
    reply: &mut AdapterReply,
) {
    let activated = prepared.is_some_and(|mutation| {
        !mutation.target_was_frontmost
            && ax::application_is_frontmost(request.record.snapshot.pid).unwrap_or(false)
    });
    if reply.delivery == AdapterDelivery::Confirmed
        && request.context.mode.as_str() == "background"
        && activated
    {
        reply.interrupted = true;
        reply.error = Some(ax::adapter_error(
            "interrupted",
            "background AX mutation unexpectedly changed target activation",
        ));
    }
}

fn settle_reply(
    request: &Request,
    fence: Option<&ObservationFence>,
    event_cursor_before: u64,
    reply: &mut AdapterReply,
) {
    if reply.delivery != AdapterDelivery::Confirmed || reply.interrupted {
        return;
    }
    let Some(fence) = fence else { return };
    let started = Instant::now();
    crate::oracle::barrier(
        "during_native_quiet",
        request.context.deadline,
        &request.cancellation,
    );
    match fence.wait_for_quiet(request.context.deadline, &request.cancellation) {
        Ok(events) => {
            reply.already_settled = true;
            reply.timing.stabilize_ms = started.elapsed().as_millis() as u64;
            record_settle_facts(reply, fence, events.saturating_sub(event_cursor_before));
            attach_fenced_capture(request, fence, events, reply);
        }
        Err(error) => {
            apply_wait_error(reply, error);
        }
    }
}

fn record_settle_facts(reply: &mut AdapterReply, fence: &ObservationFence, events: u64) {
    if let Some(object) = reply.response.as_object_mut() {
        object.insert("ax_events".to_owned(), Value::from(events));
        object.insert(
            "observer_registrations".to_owned(),
            Value::from(fence.registration_count() as u64),
        );
    }
}

fn attach_fenced_capture(
    request: &Request,
    fence: &ObservationFence,
    events_before_capture: u64,
    reply: &mut AdapterReply,
) {
    let requires_foreground_ownership = request.context.mode.as_str() == "foreground"
        && request.operation.command.starts_with("action.");
    let Some(first) = capture_with_ownership(
        request,
        reply,
        requires_foreground_ownership,
        "before capture",
        "during capture",
    ) else {
        return;
    };
    let Some(events_after_first) = quiet_after_capture(request, fence, reply) else {
        return;
    };
    if events_after_first == events_before_capture {
        merge_capture(reply, first);
        return;
    }
    retry_fenced_capture(
        request,
        fence,
        events_after_first,
        requires_foreground_ownership,
        reply,
    );
}

fn capture_with_ownership(
    request: &Request,
    reply: &mut AdapterReply,
    requires_ownership: bool,
    before: &str,
    after: &str,
) -> Option<AdapterReply> {
    if ownership_lost(request, requires_ownership) {
        interrupt_capture(reply, before);
        return None;
    }
    let capture = capture::screenshot(&request.record, &request.context, &request.cancellation);
    if capture.delivery != AdapterDelivery::Confirmed {
        reply.interrupted = true;
        reply.error = capture.error.or_else(|| {
            Some(ax::adapter_error(
                "observation_failed",
                "post-dispatch capture did not confirm a complete target frame",
            ))
        });
        return None;
    }
    if ownership_lost(request, requires_ownership) {
        interrupt_capture(reply, after);
        return None;
    }
    Some(capture)
}

fn ownership_lost(request: &Request, required: bool) -> bool {
    required && !crate::foreground::owns_exact(&request.record)
}

fn interrupt_capture(reply: &mut AdapterReply, phase: &str) {
    reply.interrupted = true;
    reply.error = Some(ax::adapter_error(
        "interrupted",
        &format!("the exact target lost foreground ownership {phase}"),
    ));
}

fn quiet_after_capture(
    request: &Request,
    fence: &ObservationFence,
    reply: &mut AdapterReply,
) -> Option<u64> {
    let started = Instant::now();
    let events = match fence.wait_for_quiet(request.context.deadline, &request.cancellation) {
        Ok(events) => events,
        Err(error) => {
            apply_wait_error(reply, error);
            return None;
        }
    };
    reply.timing.stabilize_ms = reply
        .timing
        .stabilize_ms
        .saturating_add(started.elapsed().as_millis() as u64);
    Some(events)
}

fn apply_wait_error(reply: &mut AdapterReply, error: manuvra_runtime::AdapterError) {
    if error.code == "cancelled" {
        reply.interrupted = true;
    } else {
        reply.continuous_events = true;
    }
    reply.error = Some(error);
}

fn retry_fenced_capture(
    request: &Request,
    fence: &ObservationFence,
    events_after_first: u64,
    requires_ownership: bool,
    reply: &mut AdapterReply,
) {
    let Some(second) = capture_with_ownership(
        request,
        reply,
        requires_ownership,
        "before repeated capture",
        "during repeated capture",
    ) else {
        return;
    };
    let Some(events_after_second) = quiet_after_capture(request, fence, reply) else {
        return;
    };
    if events_after_second != events_after_first {
        reply.continuous_events = true;
        return;
    }
    reply.capture_race_once = true;
    if let Some(object) = reply.response.as_object_mut() {
        object.insert("capture_retried".to_owned(), Value::Bool(true));
    }
    merge_capture(reply, second);
}

fn merge_capture(reply: &mut AdapterReply, capture: AdapterReply) {
    reply.screenshot = capture.screenshot;
    reply.screenshot_width = capture.screenshot_width;
    reply.screenshot_height = capture.screenshot_height;
    reply.frame_signature = capture.frame_signature;
    reply.timing.capture_ms = reply
        .timing
        .capture_ms
        .saturating_add(capture.timing.capture_ms);
}

fn is_mutating(command: &str) -> bool {
    command.starts_with("action.") || matches!(command, "raw.ax.set" | "raw.ax.perform")
}

fn unknown(code: &str, message: &str) -> AdapterReply {
    let mut reply = AdapterReply::confirmed(Value::Null, None);
    reply.delivery = AdapterDelivery::Unknown;
    reply.error = Some(ax::adapter_error(code, message));
    reply
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::discovery::{WindowBounds, WindowSnapshot};
    use manuvra_runtime::{ExecutionMode, TargetDescriptor};
    use serde_json::json;
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    #[test]
    fn empty_worker_pool_can_be_pruned() {
        let mut workers = WorkerPool::new();
        workers.retain_present(&[]);
        assert!(workers.workers.is_empty());
    }

    #[test]
    fn blocked_target_wait_does_not_hold_worker_pool_mutex() {
        let (sender_a, receiver_a) = mpsc::channel();
        let (sender_b, _receiver_b) = mpsc::channel();
        let mut workers = WorkerPool::new();
        workers.workers.insert(
            "target-a".to_owned(),
            Worker {
                generation: 1,
                sender: sender_a,
                sessions: HashSet::new(),
            },
        );
        workers.workers.insert(
            "target-b".to_owned(),
            Worker {
                generation: 1,
                sender: sender_b,
                sessions: HashSet::new(),
            },
        );
        let workers = Arc::new(Mutex::new(workers));
        let handle_a = workers.lock().unwrap().handle(&record("target-a"));
        let (waiting, waiting_rx) = mpsc::channel();
        let (release, release_rx) = mpsc::channel();
        let responder = thread::spawn(move || {
            let Message::Execute(request) = receiver_a.recv().unwrap() else {
                panic!("expected execution request");
            };
            waiting.send(()).unwrap();
            release_rx.recv().unwrap();
            request
                .reply
                .send(AdapterReply::confirmed(json!({}), None))
                .unwrap();
        });
        let dispatch = thread::spawn(move || {
            handle_a.dispatch(
                record("target-a"),
                &context("target-a"),
                &AdapterOperation::new("observe.tree".to_owned(), json!({})),
                Arc::new(AtomicBool::new(false)),
            )
        });
        waiting_rx.recv().unwrap();

        let mut pool = workers
            .try_lock()
            .expect("target A's blocking wait must not retain the global pool mutex");
        let _handle_b = pool.handle(&record("target-b"));
        drop(pool);
        release.send(()).unwrap();
        assert_eq!(
            dispatch.join().unwrap().delivery,
            AdapterDelivery::Confirmed
        );
        responder.join().unwrap();
    }

    fn record(target_id: &str) -> WindowRecord {
        WindowRecord {
            descriptor: TargetDescriptor {
                target_id: target_id.to_owned(),
                generation: 1,
                kind: "macos".to_owned(),
                capabilities: vec![],
            },
            snapshot: WindowSnapshot {
                pid: 1,
                window_id: 1,
                owner: "test".to_owned(),
                title: None,
                bounds: WindowBounds {
                    x: 0.0,
                    y: 0.0,
                    width: 1.0,
                    height: 1.0,
                },
                is_on_screen: true,
            },
            present: true,
        }
    }

    fn context(target_id: &str) -> AdapterContext {
        AdapterContext {
            session_id: "session".to_owned(),
            target_id: target_id.to_owned(),
            target_generation: 1,
            action_sequence: 0,
            reference_namespace: "namespace".to_owned(),
            reference_epoch: 1,
            frame_token: None,
            mode: ExecutionMode::Background,
            deadline: Instant::now() + Duration::from_secs(2),
        }
    }
}
