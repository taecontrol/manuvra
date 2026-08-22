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
        recv_worker_reply(self, record, context, operation, cancellation)
    }

    pub fn prepare(
        &self,
        record: WindowRecord,
        context: &AdapterContext,
        operation: AdapterOperation,
        cancellation: Arc<AtomicBool>,
    ) -> Result<AdapterOperation, manuvra_runtime::AdapterError> {
        prepare_preflight(&cancellation, context.deadline)?;
        recv_prepare_reply(self, record, context, operation, cancellation)
    }
}

impl Drop for WorkerPool {
    fn drop(&mut self) {
        for worker in self.workers.values() {
            let _ = worker.sender.send(Message::Shutdown);
        }
    }
}

fn recv_worker_reply(
    handle: &WorkerHandle,
    record: WindowRecord,
    context: &AdapterContext,
    operation: &AdapterOperation,
    cancellation: Arc<AtomicBool>,
) -> AdapterReply {
    let (reply, receiver) = mpsc::channel();
    let request = Request {
        record,
        context: context.clone(),
        operation: operation.clone(),
        cancellation: cancellation.clone(),
        reply,
    };
    if handle.sender.send(Message::Execute(request)).is_err() {
        return unknown(
            "transport_ambiguous",
            "macOS target worker stopped before dispatch",
        );
    }
    wait_for_worker_reply(context, cancellation, receiver)
}

fn wait_for_worker_reply(
    context: &AdapterContext,
    cancellation: Arc<AtomicBool>,
    receiver: Receiver<AdapterReply>,
) -> AdapterReply {
    match receiver.recv_timeout(context.deadline.saturating_duration_since(Instant::now())) {
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

fn prepare_preflight(
    cancellation: &AtomicBool,
    deadline: Instant,
) -> Result<(), manuvra_runtime::AdapterError> {
    if cancellation.load(Ordering::SeqCst) {
        return Err(ax::adapter_error(
            "cancelled",
            "request was cancelled before native mutation preparation",
        ));
    }
    (Instant::now() < deadline).then_some(()).ok_or_else(|| {
        ax::adapter_error(
            "timed_out",
            "deadline elapsed before native mutation preparation",
        )
    })
}

fn recv_prepare_reply(
    handle: &WorkerHandle,
    record: WindowRecord,
    context: &AdapterContext,
    operation: AdapterOperation,
    cancellation: Arc<AtomicBool>,
) -> Result<AdapterOperation, manuvra_runtime::AdapterError> {
    let (reply, receiver) = mpsc::channel();
    handle
        .sender
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
    wait_for_prepare_reply(context, cancellation, receiver)
}

fn wait_for_prepare_reply(
    context: &AdapterContext,
    cancellation: Arc<AtomicBool>,
    receiver: Receiver<Result<AdapterOperation, manuvra_runtime::AdapterError>>,
) -> Result<AdapterOperation, manuvra_runtime::AdapterError> {
    match receiver.recv_timeout(context.deadline.saturating_duration_since(Instant::now())) {
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

fn worker_loop(receiver: Receiver<Message>) {
    // Native observer ownership never leaves this target's serialized worker thread. Every
    // prepared mutation owns a distinct observer fence that is detached at terminal completion.
    let mut state = WorkerState::default();
    while let Ok(message) = receiver.recv() {
        state.prune();
        if !handle_worker_message(&mut state, message) {
            break;
        }
    }
}

fn handle_worker_message(state: &mut WorkerState, message: Message) -> bool {
    match message {
        Message::Shutdown => false,
        other => {
            apply_worker_work(state, other);
            true
        }
    }
}

fn apply_worker_work(state: &mut WorkerState, message: Message) {
    if let Message::Prepare(request) = message {
        handle_prepare(state, request);
    } else {
        apply_execute_or_close(state, message);
    }
}

fn apply_execute_or_close(state: &mut WorkerState, message: Message) {
    if let Message::Execute(request) = message {
        handle_execute(state, request);
    } else {
        close_if_session(state, message);
    }
}

fn close_if_session(state: &mut WorkerState, message: Message) {
    if let Message::CloseSession(session_id) = message {
        state.close_session(&session_id);
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
    require_prepare_capacity(state)?;
    prepare_after_capacity(state, request, started)
}

fn prepare_after_capacity(
    state: &mut WorkerState,
    request: &PrepareRequest,
    started: Instant,
) -> Result<(AdapterOperation, String), manuvra_runtime::AdapterError> {
    crate::oracle::barrier_for(
        &request.context,
        &request.operation,
        "during_native_resolution",
        &request.cancellation,
    );
    require_prepare_still_live(request)?;
    store_prepared_mutation(state, request, started)
}

fn require_prepare_capacity(state: &WorkerState) -> Result<(), manuvra_runtime::AdapterError> {
    (state.prepared.len() < 128).then_some(()).ok_or_else(|| {
        ax::adapter_error(
            "capability_unavailable",
            "native prepared-mutation table reached its bound",
        )
    })
}

fn require_prepare_still_live(
    request: &PrepareRequest,
) -> Result<(), manuvra_runtime::AdapterError> {
    require_not_cancelled(request)?;
    (Instant::now() < request.context.deadline)
        .then_some(())
        .ok_or_else(|| {
            ax::adapter_error(
                "timed_out",
                "deadline elapsed during native mutation preparation",
            )
        })
}

fn require_not_cancelled(request: &PrepareRequest) -> Result<(), manuvra_runtime::AdapterError> {
    if request.cancellation.load(Ordering::SeqCst) {
        Err(ax::adapter_error(
            "cancelled",
            "request was cancelled during native resolution",
        ))
    } else {
        Ok(())
    }
}

fn store_prepared_mutation(
    state: &mut WorkerState,
    request: &PrepareRequest,
    started: Instant,
) -> Result<(AdapterOperation, String), manuvra_runtime::AdapterError> {
    remember_prepared(
        state,
        request,
        started,
        ObservationFence::install(&request.record)?,
    )
}

fn remember_prepared(
    state: &mut WorkerState,
    request: &PrepareRequest,
    started: Instant,
    observer: ObservationFence,
) -> Result<(AdapterOperation, String), manuvra_runtime::AdapterError> {
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
    dispatch_prepared_request(state, request)
}

fn dispatch_prepared_request(state: &mut WorkerState, request: &Request) -> AdapterReply {
    let prepared = match take_prepared(state, request, is_mutating(&request.operation.command)) {
        Ok(prepared) => prepared,
        Err(error) => return ax::rejected_error(error),
    };
    if request.cancellation.load(Ordering::SeqCst) {
        return unknown(
            "cancelled",
            "request was cancelled before native mutation dispatch",
        );
    }
    settle_native_reply(state, request, prepared)
}

fn settle_native_reply(
    state: &mut WorkerState,
    request: &Request,
    prepared: Option<PreparedMutation>,
) -> AdapterReply {
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
    consume_prepared(state, request)
}

fn consume_prepared(
    state: &mut WorkerState,
    request: &Request,
) -> Result<Option<PreparedMutation>, manuvra_runtime::AdapterError> {
    let token = prepared_token(&request.operation)?;
    let prepared = state.prepared.remove(token).ok_or_else(|| {
        ax::adapter_error(
            "timed_out",
            "native prepared mutation expired or was already consumed",
        )
    })?;
    require_prepared_authority(&prepared, request).map(|()| Some(prepared))
}

fn prepared_token(operation: &AdapterOperation) -> Result<&str, manuvra_runtime::AdapterError> {
    operation
        .prepared
        .as_ref()
        .and_then(|details| details.get("macos_token"))
        .and_then(Value::as_str)
        .ok_or_else(|| {
            ax::adapter_error(
                "invalid_request",
                "native mutation is missing its prepared token",
            )
        })
}

fn require_prepared_authority(
    prepared: &PreparedMutation,
    request: &Request,
) -> Result<(), manuvra_runtime::AdapterError> {
    (prepared.session_id == request.context.session_id
        && prepared.action_sequence == request.context.action_sequence)
        .then_some(())
        .ok_or_else(|| {
            ax::adapter_error(
                "invalid_request",
                "native prepared mutation authority does not match the admitted request",
            )
        })
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
    if background_activation_interrupted(request, prepared, reply.delivery.clone()) {
        reply.interrupted = true;
        reply.error = Some(ax::adapter_error(
            "interrupted",
            "background AX mutation unexpectedly changed target activation",
        ));
    }
}

fn background_activation_interrupted(
    request: &Request,
    prepared: Option<&PreparedMutation>,
    delivery: AdapterDelivery,
) -> bool {
    delivery == AdapterDelivery::Confirmed
        && request.context.mode.as_str() == "background"
        && prepared_became_frontmost(request, prepared)
}

fn prepared_became_frontmost(request: &Request, prepared: Option<&PreparedMutation>) -> bool {
    prepared.is_some_and(|mutation| {
        !mutation.target_was_frontmost
            && ax::application_is_frontmost(request.record.snapshot.pid).unwrap_or(false)
    })
}

fn settle_reply(
    request: &Request,
    fence: Option<&ObservationFence>,
    event_cursor_before: u64,
    reply: &mut AdapterReply,
) {
    if !confirmed_uninterrupted(reply) {
        return;
    }
    if let Some(fence) = fence {
        settle_confirmed(request, fence, event_cursor_before, reply);
    }
}

fn confirmed_uninterrupted(reply: &AdapterReply) -> bool {
    reply.delivery == AdapterDelivery::Confirmed && !reply.interrupted
}

fn settle_confirmed(
    request: &Request,
    fence: &ObservationFence,
    event_cursor_before: u64,
    reply: &mut AdapterReply,
) {
    let started = Instant::now();
    crate::oracle::barrier(
        "during_native_quiet",
        request.context.deadline,
        &request.cancellation,
    );
    if let Some(events) = take_quiet_events(
        fence.wait_for_quiet(request.context.deadline, &request.cancellation),
        reply,
    ) {
        record_quiet_settle(request, fence, event_cursor_before, reply, started, events);
    }
}

fn record_quiet_settle(
    request: &Request,
    fence: &ObservationFence,
    event_cursor_before: u64,
    reply: &mut AdapterReply,
    started: Instant,
    events: u64,
) {
    reply.already_settled = true;
    reply.timing.stabilize_ms = started.elapsed().as_millis() as u64;
    record_settle_facts(reply, fence, events.saturating_sub(event_cursor_before));
    attach_fenced_capture(request, fence, events, reply);
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
    capture_if_owned(
        request,
        fence,
        events_before_capture,
        reply,
        requires_foreground_capture(request),
    )
}

fn requires_foreground_capture(request: &Request) -> bool {
    request.context.mode.as_str() == "foreground"
        && request.operation.command.starts_with("action.")
}

fn capture_if_owned(
    request: &Request,
    fence: &ObservationFence,
    events_before_capture: u64,
    reply: &mut AdapterReply,
    requires_foreground_ownership: bool,
) {
    if let Some(first) = capture_with_ownership(
        request,
        reply,
        requires_foreground_ownership,
        "before capture",
        "during capture",
    ) {
        settle_first_capture(
            request,
            fence,
            events_before_capture,
            requires_foreground_ownership,
            reply,
            first,
        );
    }
}

fn settle_first_capture(
    request: &Request,
    fence: &ObservationFence,
    events_before_capture: u64,
    requires_foreground_ownership: bool,
    reply: &mut AdapterReply,
    first: AdapterReply,
) {
    if let Some(events_after_first) = quiet_after_capture(request, fence, reply) {
        keep_or_retry_capture(
            request,
            fence,
            events_before_capture,
            requires_foreground_ownership,
            reply,
            first,
            events_after_first,
        );
    }
}

fn keep_or_retry_capture(
    request: &Request,
    fence: &ObservationFence,
    events_before_capture: u64,
    requires_foreground_ownership: bool,
    reply: &mut AdapterReply,
    first: AdapterReply,
    events_after_first: u64,
) {
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
    confirm_owned_capture(request, reply, requires_ownership, after)
}

fn confirm_owned_capture(
    request: &Request,
    reply: &mut AdapterReply,
    requires_ownership: bool,
    after: &str,
) -> Option<AdapterReply> {
    let capture = capture::screenshot(&request.record, &request.context, &request.cancellation);
    if capture.delivery != AdapterDelivery::Confirmed {
        fail_unconfirmed_capture(reply, capture.error);
        return None;
    }
    keep_owned_capture(request, reply, requires_ownership, after, capture)
}

fn keep_owned_capture(
    request: &Request,
    reply: &mut AdapterReply,
    requires_ownership: bool,
    after: &str,
    capture: AdapterReply,
) -> Option<AdapterReply> {
    if ownership_lost(request, requires_ownership) {
        interrupt_capture(reply, after);
        return None;
    }
    Some(capture)
}

fn fail_unconfirmed_capture(
    reply: &mut AdapterReply,
    error: Option<manuvra_runtime::AdapterError>,
) {
    reply.interrupted = true;
    reply.error = error.or_else(|| {
        Some(ax::adapter_error(
            "observation_failed",
            "post-dispatch capture did not confirm a complete target frame",
        ))
    });
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
    take_quiet_events(
        fence.wait_for_quiet(request.context.deadline, &request.cancellation),
        reply,
    )
    .map(|events| add_quiet_time(reply, started, events))
}

fn take_quiet_events(
    result: Result<u64, manuvra_runtime::AdapterError>,
    reply: &mut AdapterReply,
) -> Option<u64> {
    match result {
        Ok(events) => Some(events),
        Err(error) => {
            apply_wait_error(reply, error);
            None
        }
    }
}

fn add_quiet_time(reply: &mut AdapterReply, started: Instant, events: u64) -> u64 {
    reply.timing.stabilize_ms = reply
        .timing
        .stabilize_ms
        .saturating_add(started.elapsed().as_millis() as u64);
    events
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
    apply_retry_capture(request, fence, events_after_first, reply, second);
}

fn apply_retry_capture(
    request: &Request,
    fence: &ObservationFence,
    events_after_first: u64,
    reply: &mut AdapterReply,
    second: AdapterReply,
) {
    let Some(events_after_second) = quiet_after_capture(request, fence, reply) else {
        return;
    };
    if events_after_second != events_after_first {
        reply.continuous_events = true;
        return;
    }
    record_capture_retry(reply);
    merge_capture(reply, second);
}

fn record_capture_retry(reply: &mut AdapterReply) {
    reply.capture_race_once = true;
    if let Some(object) = reply.response.as_object_mut() {
        object.insert("capture_retried".to_owned(), Value::Bool(true));
    }
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
        assert!(!is_mutating("observe.tree"));
        assert!(is_mutating("action.click"));
        assert!(is_mutating("raw.ax.set"));
        let mut state = WorkerState::default();
        let request = Request {
            record: record("target-a"),
            context: context("target-a"),
            operation: AdapterOperation::new("action.click".to_owned(), json!({})),
            cancellation: Arc::new(AtomicBool::new(false)),
            reply: mpsc::channel().0,
        };
        assert_eq!(
            take_prepared(&mut state, &request, true)
                .err()
                .unwrap()
                .code,
            "invalid_request"
        );
        assert!(
            take_prepared(&mut state, &request, false)
                .unwrap()
                .is_none()
        );
        assert!(
            prepare_preflight(
                &AtomicBool::new(true),
                Instant::now() + Duration::from_secs(1)
            )
            .is_err()
        );
        assert!(prepare_preflight(&AtomicBool::new(false), Instant::now()).is_err());
        let unknown = unknown("timed_out", "deadline elapsed");
        assert_eq!(unknown.delivery, AdapterDelivery::Unknown);
        let mut reply = AdapterReply::confirmed(json!({}), None);
        fail_unconfirmed_capture(&mut reply, None);
        assert!(reply.interrupted);
        apply_wait_error(&mut reply, ax::adapter_error("cancelled", "cancelled"));
        assert!(reply.interrupted);
        apply_wait_error(
            &mut reply,
            ax::adapter_error("stabilization_timeout", "quiet"),
        );
        assert!(reply.continuous_events);
        let mut quiet = AdapterReply::confirmed(json!({}), None);
        assert_eq!(take_quiet_events(Ok(4), &mut quiet), Some(4));
        assert!(
            take_quiet_events(Err(ax::adapter_error("cancelled", "cancelled")), &mut quiet)
                .is_none()
        );
        assert!(quiet.interrupted);
        assert!(
            take_quiet_events(
                Err(ax::adapter_error("stabilization_timeout", "quiet")),
                &mut quiet
            )
            .is_none()
        );
        assert!(quiet.continuous_events);
        record_capture_retry(&mut reply);
        assert!(reply.capture_race_once);
        assert_eq!(reply.response["capture_retried"], true);
        let mut live = WorkerPool::new();
        let handle = live.handle(&record("spawned"));
        let observed = handle.dispatch(
            record("spawned"),
            &context("spawned"),
            &AdapterOperation::new("observe.tree".to_owned(), json!({})),
            Arc::new(AtomicBool::new(false)),
        );
        assert!(observed.error.is_some());
        live.session_opened(&record("spawned"), "session");
        live.session_closed("spawned", "session");
        let cancelled_prepare = handle.prepare(
            record("spawned"),
            &context("spawned"),
            AdapterOperation::new("action.click".to_owned(), json!({})),
            Arc::new(AtomicBool::new(true)),
        );
        assert_eq!(cancelled_prepare.unwrap_err().code, "cancelled");
        drop(live);
        let disconnected = wait_for_prepare_reply(
            &context("spawned"),
            Arc::new(AtomicBool::new(false)),
            mpsc::channel().1,
        )
        .unwrap_err()
        .code;
        assert!(
            matches!(disconnected.as_str(), "transport_ambiguous" | "timed_out"),
            "{disconnected}"
        );
        assert!(!background_activation_interrupted(
            &request,
            None,
            AdapterDelivery::Confirmed
        ));
        settle_reply(
            &request,
            None,
            0,
            &mut AdapterReply::confirmed(json!({}), None),
        );
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
                owner: "test".to_owned(),
                title: None,
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
