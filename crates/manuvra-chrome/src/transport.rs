use serde_json::{Value, json};
use std::collections::{HashSet, VecDeque};
use std::io;
use std::net::TcpStream;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender, SyncSender, TryRecvError};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::{Duration, Instant};
use tungstenite::stream::MaybeTlsStream;
use tungstenite::{Message, WebSocket, connect};

const SOCKET_POLL: Duration = Duration::from_millis(10);
const MAX_EVENTS: usize = 10_000;
const MAX_EVENT_BYTES: usize = 8 * 1024 * 1024;

type Socket = WebSocket<MaybeTlsStream<TcpStream>>;
type SharedJournal = Arc<(Mutex<Journal>, Condvar)>;

#[derive(Debug, Clone)]
pub struct JournalEvent {
    pub cursor: u64,
    pub message: Value,
}

#[derive(Debug, Clone)]
pub struct JournalSnapshot {
    pub events: Vec<JournalEvent>,
    /// Events after the requested cursor were evicted, so `events` is incomplete.
    pub overflowed: bool,
    pub last_cursor: u64,
}

/// A bounded sliding window over received CDP events. Old events are evicted
/// once the window is full; a snapshot reports overflow only when its own
/// cursor range lost events.
#[derive(Debug)]
struct Journal {
    next_cursor: u64,
    bytes: usize,
    /// Highest cursor evicted from the window; every later event is retained.
    dropped_through: u64,
    events: VecDeque<RetainedEvent>,
    methods: HashSet<String>,
}

#[derive(Debug)]
struct RetainedEvent {
    event: JournalEvent,
    bytes: usize,
}

impl Default for Journal {
    fn default() -> Self {
        Self {
            next_cursor: 1,
            bytes: 0,
            dropped_through: 0,
            events: VecDeque::new(),
            methods: HashSet::new(),
        }
    }
}

impl Journal {
    fn record(&mut self, message: Value) {
        let bytes = serde_json::to_vec(&message).map_or(MAX_EVENT_BYTES + 1, |value| value.len());
        if let Some(method) = message.get("method").and_then(Value::as_str) {
            self.remember_method(method);
        }
        let event = JournalEvent {
            cursor: self.next_cursor,
            message,
        };
        self.next_cursor += 1;
        self.bytes += bytes;
        self.events.push_back(RetainedEvent { event, bytes });
        self.evict_beyond_bounds();
    }

    fn remember_method(&mut self, method: &str) {
        if !self.methods.contains(method) {
            self.methods.insert(method.to_owned());
        }
    }

    fn evict_beyond_bounds(&mut self) {
        while self.events.len() > MAX_EVENTS || self.bytes > MAX_EVENT_BYTES {
            let Some(evicted) = self.events.pop_front() else {
                return;
            };
            self.bytes -= evicted.bytes;
            self.dropped_through = evicted.event.cursor;
        }
    }

    fn cursor(&self) -> u64 {
        self.next_cursor.saturating_sub(1)
    }

    fn snapshot_since(&self, cursor: u64) -> JournalSnapshot {
        JournalSnapshot {
            events: self
                .events
                .iter()
                .filter(|retained| retained.event.cursor > cursor)
                .map(|retained| retained.event.clone())
                .collect(),
            overflowed: cursor < self.dropped_through,
            last_cursor: self.cursor(),
        }
    }
}

struct Request {
    method: String,
    params: Value,
    deadline: Instant,
    cancellation: Arc<AtomicBool>,
    reply: SyncSender<CommandOutcome>,
}

#[derive(Debug, Clone)]
pub enum CommandOutcome {
    Confirmed(Value),
    Rejected(Value),
    NotSent(String),
    Unknown(String),
}

impl CommandOutcome {
    pub fn result(self) -> Result<Value, CommandFailure> {
        match self {
            Self::Confirmed(response) => Ok(response.get("result").cloned().unwrap_or(Value::Null)),
            Self::Rejected(response) => Err(CommandFailure::Rejected(response)),
            Self::NotSent(message) => Err(CommandFailure::NotSent(message)),
            Self::Unknown(message) => Err(CommandFailure::Unknown(message)),
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum CommandFailure {
    #[error("CDP rejected the command")]
    Rejected(Value),
    #[error("CDP command was not sent: {0}")]
    NotSent(String),
    #[error("CDP command may have been sent: {0}")]
    Unknown(String),
}

pub struct CdpClient {
    sender: Sender<Request>,
    journal: SharedJournal,
    disconnected: Arc<AtomicBool>,
}

impl CdpClient {
    pub fn connect(url: String, observe: bool) -> Result<Arc<Self>, String> {
        let (sender, receiver) = mpsc::channel();
        let journal = Arc::new((Mutex::new(Journal::default()), Condvar::new()));
        let disconnected = Arc::new(AtomicBool::new(false));
        let mut socket = connect(&url)
            .map_err(|error| format!("WebSocket connection failed: {error}"))?
            .0;
        configure_timeout(&mut socket, SOCKET_POLL)?;
        let client = Arc::new(Self {
            sender,
            journal: journal.clone(),
            disconnected: disconnected.clone(),
        });
        thread::Builder::new()
            .name("manuvra-cdp".to_owned())
            .spawn(move || worker(&mut socket, receiver, journal, disconnected, observe))
            .map_err(|error| format!("CDP worker spawn failed: {error}"))?;
        Ok(client)
    }

    pub fn command(
        &self,
        method: impl Into<String>,
        params: Value,
        deadline: Instant,
        cancellation: Arc<AtomicBool>,
    ) -> CommandOutcome {
        if let Some(outcome) = self.not_queued_outcome(&cancellation, deadline) {
            return outcome;
        }
        let (reply, receive) = mpsc::sync_channel(1);
        let request = Request {
            method: method.into(),
            params,
            deadline,
            cancellation: cancellation.clone(),
            reply,
        };
        if self.sender.send(request).is_err() {
            return CommandOutcome::NotSent("connection worker stopped".to_owned());
        }
        await_worker_outcome(receive, &cancellation, deadline)
    }

    fn not_queued_outcome(
        &self,
        cancellation: &Arc<AtomicBool>,
        deadline: Instant,
    ) -> Option<CommandOutcome> {
        if self.disconnected.load(Ordering::SeqCst) {
            return Some(CommandOutcome::NotSent(
                "connection is disconnected".to_owned(),
            ));
        }
        if cancellation.load(Ordering::SeqCst) || Instant::now() >= deadline {
            return Some(CommandOutcome::NotSent(
                "cancelled or timed out before queueing".to_owned(),
            ));
        }
        None
    }

    pub fn cursor(&self) -> u64 {
        self.journal.0.lock().expect("CDP journal").cursor()
    }

    pub fn snapshot_since(&self, cursor: u64) -> JournalSnapshot {
        self.journal
            .0
            .lock()
            .expect("CDP journal")
            .snapshot_since(cursor)
    }

    /// Whether events after `cursor` were evicted before they could be read.
    pub fn lost_events_since(&self, cursor: u64) -> bool {
        cursor < self.journal.0.lock().expect("CDP journal").dropped_through
    }

    /// Whether any event with `method` was ever received, even if since evicted.
    pub fn has_received(&self, method: &str) -> bool {
        self.journal
            .0
            .lock()
            .expect("CDP journal")
            .methods
            .contains(method)
    }

    #[cfg(test)]
    pub fn is_disconnected(&self) -> bool {
        self.disconnected.load(Ordering::SeqCst)
    }

    pub fn wait_for_journal_change(&self, cursor: u64, timeout: Duration) {
        let guard = self.journal.0.lock().expect("CDP journal");
        if guard.cursor() != cursor {
            return;
        }
        let _ = self.journal.1.wait_timeout(guard, timeout);
    }
}

fn worker(
    socket: &mut Socket,
    receiver: Receiver<Request>,
    journal: SharedJournal,
    disconnected: Arc<AtomicBool>,
    observe: bool,
) {
    let mut next_id = 1_u64;
    if observe && initialize(socket, &journal, &mut next_id).is_err() {
        disconnected.store(true, Ordering::SeqCst);
        return;
    }
    drive_worker(socket, receiver, journal, disconnected, &mut next_id);
}

fn drive_worker(
    socket: &mut Socket,
    receiver: Receiver<Request>,
    journal: SharedJournal,
    disconnected: Arc<AtomicBool>,
    next_id: &mut u64,
) {
    loop {
        match receiver.try_recv() {
            Ok(request) => fulfill_request(socket, request, &journal, next_id),
            Err(TryRecvError::Empty) => {
                if !poll_incoming(socket, &journal, &disconnected) {
                    break;
                }
            }
            Err(TryRecvError::Disconnected) => break,
        }
    }
}

fn fulfill_request(
    socket: &mut Socket,
    request: Request,
    journal: &SharedJournal,
    next_id: &mut u64,
) {
    let outcome = execute(
        socket,
        Outgoing {
            method: request.method,
            params: request.params,
            deadline: request.deadline,
            cancellation: &request.cancellation,
        },
        journal,
        next_id,
    );
    let _ = request.reply.send(outcome);
}

fn poll_incoming(
    socket: &mut Socket,
    journal: &SharedJournal,
    disconnected: &Arc<AtomicBool>,
) -> bool {
    match read_message(socket) {
        Ok(Some(value)) => {
            record_event(journal, value);
            true
        }
        Ok(None) => true,
        Err(_) => {
            disconnected.store(true, Ordering::SeqCst);
            false
        }
    }
}

fn await_worker_outcome(
    receive: Receiver<CommandOutcome>,
    cancellation: &Arc<AtomicBool>,
    deadline: Instant,
) -> CommandOutcome {
    loop {
        if let Ok(outcome) = receive.recv_timeout(Duration::from_millis(2)) {
            return outcome;
        }
        if cancellation.load(Ordering::SeqCst) {
            return CommandOutcome::Unknown("cancelled while awaiting CDP reply".to_owned());
        }
        if Instant::now() >= deadline {
            return CommandOutcome::Unknown("deadline expired while awaiting CDP reply".to_owned());
        }
    }
}

/// Enables only the event domains the journal's readers consume: page lifecycle
/// and navigation, DOM mutations, and accessibility updates.
fn initialize(
    socket: &mut Socket,
    journal: &SharedJournal,
    next_id: &mut u64,
) -> Result<(), String> {
    let cancellation = Arc::new(AtomicBool::new(false));
    for (method, params) in [
        ("Page.enable", json!({})),
        ("Page.setLifecycleEventsEnabled", json!({"enabled": true})),
        ("DOM.enable", json!({})),
        ("Accessibility.enable", json!({})),
    ] {
        let outcome = execute(
            socket,
            Outgoing {
                method: method.to_owned(),
                params,
                deadline: Instant::now() + Duration::from_secs(2),
                cancellation: &cancellation,
            },
            journal,
            next_id,
        );
        if !matches!(outcome, CommandOutcome::Confirmed(_)) {
            return Err(format!("CDP initialization failed at {method}"));
        }
    }
    Ok(())
}

struct Outgoing<'a> {
    method: String,
    params: Value,
    deadline: Instant,
    cancellation: &'a Arc<AtomicBool>,
}

fn execute(
    socket: &mut Socket,
    outgoing: Outgoing<'_>,
    journal: &SharedJournal,
    next_id: &mut u64,
) -> CommandOutcome {
    if command_should_not_send(outgoing.cancellation, outgoing.deadline) {
        return CommandOutcome::NotSent("cancelled or timed out before send".to_owned());
    }
    let id = take_next_id(next_id);
    let message = json!({"id": id, "method": outgoing.method, "params": outgoing.params});
    if let Err(error) = socket.send(Message::Text(message.to_string().into())) {
        return classify_send_error(error);
    }
    await_command_response(
        socket,
        id,
        outgoing.deadline,
        outgoing.cancellation,
        journal,
    )
}

fn classify_send_error(error: tungstenite::Error) -> CommandOutcome {
    // tungstenite may have written a prefix before reporting an error. Once send is
    // attempted, replay is unsafe unless the transport can prove zero bytes left it.
    CommandOutcome::Unknown(format!("WebSocket send failed: {error}"))
}

fn command_should_not_send(cancellation: &Arc<AtomicBool>, deadline: Instant) -> bool {
    cancellation.load(Ordering::SeqCst) || Instant::now() >= deadline
}

fn take_next_id(next_id: &mut u64) -> u64 {
    let id = *next_id;
    *next_id = next_id.saturating_add(1);
    id
}

fn await_command_response(
    socket: &mut Socket,
    id: u64,
    deadline: Instant,
    cancellation: &Arc<AtomicBool>,
    journal: &SharedJournal,
) -> CommandOutcome {
    loop {
        if cancellation.load(Ordering::SeqCst) {
            return CommandOutcome::Unknown("cancelled after send".to_owned());
        }
        if Instant::now() >= deadline {
            return CommandOutcome::Unknown("deadline expired after send".to_owned());
        }
        match incoming_for_command(read_message(socket), id) {
            CommandIncoming::Done(outcome) => return outcome,
            CommandIncoming::Event(value) => record_event(journal, value),
            CommandIncoming::Ignore => {}
        }
    }
}

enum CommandIncoming {
    Done(CommandOutcome),
    Event(Value),
    Ignore,
}

fn incoming_for_command(read: Result<Option<Value>, String>, id: u64) -> CommandIncoming {
    match read {
        Ok(Some(value)) if value.get("id").and_then(Value::as_u64) == Some(id) => {
            CommandIncoming::Done(outcome_from_response(value))
        }
        Ok(Some(value)) if value.get("method").is_some() => CommandIncoming::Event(value),
        Ok(Some(_)) | Ok(None) => CommandIncoming::Ignore,
        Err(error) => CommandIncoming::Done(CommandOutcome::Unknown(error)),
    }
}

fn outcome_from_response(value: Value) -> CommandOutcome {
    if value.get("error").is_some() {
        CommandOutcome::Rejected(value)
    } else {
        CommandOutcome::Confirmed(value)
    }
}

fn record_event(journal: &SharedJournal, value: Value) {
    journal.0.lock().expect("CDP journal").record(value);
    journal.1.notify_all();
}

fn read_message(socket: &mut Socket) -> Result<Option<Value>, String> {
    match socket.read() {
        Ok(Message::Text(text)) => parse_cdp_json(&text),
        Ok(Message::Ping(payload)) => acknowledge_ping(socket, payload),
        Ok(Message::Close(_)) => Err("CDP connection closed".to_owned()),
        Ok(_) => Ok(None),
        Err(tungstenite::Error::Io(error)) if is_timeout(&error) => Ok(None),
        Err(error) => Err(format!("CDP read failed: {error}")),
    }
}

fn parse_cdp_json(text: &str) -> Result<Option<Value>, String> {
    serde_json::from_str(text)
        .map(Some)
        .map_err(|error| format!("invalid CDP JSON: {error}"))
}

fn acknowledge_ping(
    socket: &mut Socket,
    payload: tungstenite::Bytes,
) -> Result<Option<Value>, String> {
    socket
        .send(Message::Pong(payload))
        .map_err(|error| format!("CDP pong failed: {error}"))?;
    Ok(None)
}

fn configure_timeout(socket: &mut Socket, timeout: Duration) -> Result<(), String> {
    match socket.get_mut() {
        MaybeTlsStream::Plain(stream) => stream
            .set_read_timeout(Some(timeout))
            .map_err(|error| format!("CDP timeout configuration failed: {error}")),
        _ => Err("CDP endpoint unexpectedly negotiated TLS".to_owned()),
    }
}

fn is_timeout(error: &io::Error) -> bool {
    matches!(
        error.kind(),
        io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
    )
}

pub fn event_method(event: &JournalEvent) -> Option<&str> {
    event.message.get("method").and_then(Value::as_str)
}

pub fn event_params(event: &JournalEvent) -> &Value {
    event.message.get("params").unwrap_or(&Value::Null)
}

pub fn is_relevant_event(event: &JournalEvent) -> bool {
    event_method(event).is_some_and(|method| {
        method.starts_with("DOM.")
            || method.starts_with("Accessibility.")
            || matches!(
                method,
                "Page.frameNavigated"
                    | "Page.frameAttached"
                    | "Page.frameDetached"
                    | "Page.domContentEventFired"
                    | "Page.loadEventFired"
            )
            || (method == "Page.lifecycleEvent" && !is_network_lifecycle(event))
    })
}

fn is_network_lifecycle(event: &JournalEvent) -> bool {
    matches!(
        event_params(event).get("name").and_then(Value::as_str),
        Some("networkIdle" | "networkAlmostIdle")
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;
    use tungstenite::accept;

    #[test]
    fn disconnect_after_send_is_unknown_and_never_replayed() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let mut socket = accept(stream).unwrap();
            let first = socket.read().unwrap();
            assert!(matches!(first, Message::Text(_)));
            socket.close(None).unwrap();
        });
        let client =
            CdpClient::connect(format!("ws://{address}/devtools/page/test"), false).unwrap();
        let outcome = client.command(
            "Runtime.evaluate",
            json!({"expression": "40+2"}),
            Instant::now() + Duration::from_secs(1),
            Arc::new(AtomicBool::new(false)),
        );
        assert!(matches!(outcome, CommandOutcome::Unknown(_)));
        server.join().unwrap();
    }

    #[test]
    fn cancellation_before_a_queued_send_is_not_sent() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let _socket = accept(stream).unwrap();
            thread::sleep(Duration::from_millis(30));
        });
        let client =
            CdpClient::connect(format!("ws://{address}/devtools/page/test"), false).unwrap();
        let cancellation = Arc::new(AtomicBool::new(true));
        let outcome = client.command(
            "Runtime.evaluate",
            json!({}),
            Instant::now() + Duration::from_secs(1),
            cancellation,
        );
        assert!(matches!(outcome, CommandOutcome::NotSent(_)));
        server.join().unwrap();
    }

    fn record_many(journal: &mut Journal, count: usize) {
        for index in 0..count {
            journal.record(json!({"method": "DOM.childNodeInserted", "params": {"index": index}}));
        }
    }

    #[test]
    fn journal_evicts_old_events_and_reports_overflow_only_for_lost_ranges() {
        let mut journal = Journal::default();
        record_many(&mut journal, MAX_EVENTS + 100);
        let lost = journal.snapshot_since(0);
        assert!(lost.overflowed, "events 1..=100 were evicted");
        assert_eq!(lost.events.len(), MAX_EVENTS);
        assert_eq!(lost.events[0].cursor, 101);
        assert!(journal.snapshot_since(99).overflowed);

        let complete = journal.snapshot_since(100);
        assert!(
            !complete.overflowed,
            "every event after cursor 100 is retained"
        );
        assert_eq!(complete.events.len(), MAX_EVENTS);

        let fence = journal.cursor();
        record_many(&mut journal, MAX_EVENTS / 2);
        let recent = journal.snapshot_since(fence);
        assert!(
            !recent.overflowed,
            "a fence after the eviction is unaffected"
        );
        assert_eq!(recent.events.len(), MAX_EVENTS / 2);
        assert_eq!(recent.last_cursor, fence + (MAX_EVENTS / 2) as u64);
    }

    #[test]
    fn journal_bounds_bytes_and_evicts_an_event_larger_than_the_window() {
        let mut journal = Journal::default();
        record_many(&mut journal, 3);
        let fence = journal.cursor();
        journal.record(
            json!({"method": "DOM.large", "params": {"text": "x".repeat(MAX_EVENT_BYTES)}}),
        );
        assert!(journal.events.is_empty());
        assert_eq!(journal.bytes, 0);
        assert!(journal.snapshot_since(fence).overflowed);
        record_many(&mut journal, 1);
        let after = journal.snapshot_since(fence + 1);
        assert!(!after.overflowed);
        assert_eq!(after.events.len(), 1);
    }

    #[test]
    fn received_methods_are_remembered_after_eviction() {
        let mut journal = Journal::default();
        journal.record(json!({"method": "Page.windowOpen", "params": {}}));
        record_many(&mut journal, MAX_EVENTS + 1);
        assert!(
            !journal
                .snapshot_since(0)
                .events
                .iter()
                .any(|event| event_method(event) == Some("Page.windowOpen"))
        );
        assert!(journal.methods.contains("Page.windowOpen"));
        assert!(!journal.methods.contains("Target.targetCreated"));
    }

    #[test]
    fn relevant_events_are_page_dom_and_ax_not_network() {
        let relevant = [
            "DOM.childNodeInserted",
            "Accessibility.loadComplete",
            "Page.frameNavigated",
            "Page.lifecycleEvent",
            "Page.loadEventFired",
        ];
        for method in relevant {
            assert!(
                is_relevant_event(&journal_event(method, json!({}))),
                "{method} should reset the quiet window"
            );
        }
        assert!(is_relevant_event(&journal_event(
            "Page.lifecycleEvent",
            json!({"name": "DOMContentLoaded"})
        )));
        for event in [
            journal_event("Network.requestWillBeSent", json!({})),
            journal_event("Network.loadingFinished", json!({})),
            journal_event("Network.loadingFailed", json!({})),
            journal_event("Runtime.consoleAPICalled", json!({})),
            journal_event("Page.lifecycleEvent", json!({"name": "networkIdle"})),
            journal_event("Page.lifecycleEvent", json!({"name": "networkAlmostIdle"})),
        ] {
            assert!(
                !is_relevant_event(&event),
                "{} must not reset the quiet window",
                event_method(&event).unwrap_or("event")
            );
        }
    }

    fn journal_event(method: &str, params: Value) -> JournalEvent {
        JournalEvent {
            cursor: 1,
            message: json!({"method": method, "params": params}),
        }
    }

    #[test]
    fn incoming_command_json_is_classified_by_id_error_and_method() {
        assert!(matches!(
            incoming_for_command(Ok(Some(json!({"id": 7, "result": {}}))), 7),
            CommandIncoming::Done(CommandOutcome::Confirmed(_))
        ));
        assert!(matches!(
            incoming_for_command(Ok(Some(json!({"id": 7, "error": {"message": "no"}}))), 7),
            CommandIncoming::Done(CommandOutcome::Rejected(_))
        ));
        assert!(matches!(
            incoming_for_command(Ok(Some(json!({"method": "Page.loadEventFired"}))), 7),
            CommandIncoming::Event(_)
        ));
        assert!(matches!(
            incoming_for_command(Ok(Some(json!({"id": 8}))), 7),
            CommandIncoming::Ignore
        ));
        assert!(matches!(
            incoming_for_command(Ok(None), 7),
            CommandIncoming::Ignore
        ));
        assert!(matches!(
            incoming_for_command(Err("closed".to_owned()), 7),
            CommandIncoming::Done(CommandOutcome::Unknown(_))
        ));
    }

    #[test]
    fn scripted_http_waits_for_complete_request_headers() {
        use std::io::{Read, Write};
        use std::net::TcpStream;

        let chrome = super::test_support::ScriptedChrome::start();
        let mut stream = TcpStream::connect(chrome.address).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_millis(300)))
            .unwrap();
        stream
            .write_all(b"GET /json/list HTTP/1.1\r\nHost: 127.0.0.1")
            .unwrap();
        let mut byte = [0];
        let error = stream.read(&mut byte).unwrap_err();
        assert!(matches!(
            error.kind(),
            std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
        ));

        stream.write_all(b"\r\nConnection: close\r\n\r\n").unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let mut response = String::new();
        stream.read_to_string(&mut response).unwrap();
        let (head, body) = response.split_once("\r\n\r\n").unwrap();
        assert!(head.starts_with("HTTP/1.1 200 OK\r\n"));
        let listed: Value = serde_json::from_str(body).unwrap();
        assert_eq!(listed[0]["id"], "page-1");
    }

    #[test]
    fn scripted_chrome_confirms_rejects_and_records_injected_events() {
        let chrome = super::test_support::ScriptedChrome::start();
        chrome.reject("Runtime.evaluate");
        let client = chrome.connect_raw();
        let deadline = Instant::now() + Duration::from_secs(1);
        let cancellation = Arc::new(AtomicBool::new(false));
        assert!(matches!(
            client.command(
                "Runtime.evaluate",
                json!({}),
                deadline,
                cancellation.clone()
            ),
            CommandOutcome::Rejected(_)
        ));
        chrome.reply("Runtime.evaluate", json!({"value": 42}));
        let confirmed = client.command("Page.enable", json!({}), deadline, cancellation.clone());
        assert!(matches!(confirmed, CommandOutcome::Confirmed(_)));
        chrome.push_event("Page.loadEventFired", json!({}));
        client.wait_for_journal_change(0, Duration::from_millis(200));
        let snapshot = client.snapshot_since(0);
        assert!(
            snapshot
                .events
                .iter()
                .any(|event| event_method(event) == Some("Page.loadEventFired"))
        );
        chrome.ping_once();
        let after_ping = client.command("DOM.enable", json!({}), deadline, cancellation);
        assert!(matches!(after_ping, CommandOutcome::Confirmed(_)));
    }

    #[test]
    fn expired_deadline_is_not_queued_and_initialize_failure_disconnects() {
        let chrome = super::test_support::ScriptedChrome::start();
        chrome.reject("Page.enable");
        let client = chrome.connect_observation();
        let cancellation = Arc::new(AtomicBool::new(false));
        let deadline = Instant::now() + Duration::from_secs(1);
        while !client.is_disconnected() {
            assert!(
                Instant::now() < deadline,
                "observation worker did not disconnect"
            );
            thread::sleep(Duration::from_millis(5));
        }
        let disconnected = client.command(
            "Runtime.evaluate",
            json!({}),
            Instant::now() + Duration::from_secs(1),
            cancellation.clone(),
        );
        assert!(matches!(disconnected, CommandOutcome::NotSent(_)));

        let live = super::test_support::ScriptedChrome::start();
        let client = live.connect_raw();
        let expired = client.command(
            "Runtime.evaluate",
            json!({}),
            Instant::now() - Duration::from_millis(1),
            cancellation,
        );
        assert!(matches!(expired, CommandOutcome::NotSent(_)));
    }

    #[test]
    fn invalid_cdp_json_is_unknown_and_binary_frames_are_ignored() {
        let chrome = super::test_support::ScriptedChrome::start();
        chrome.reply_invalid_json("Runtime.evaluate");
        chrome.send_binary_once();
        let client = chrome.connect_raw();
        let outcome = client.command(
            "Runtime.evaluate",
            json!({}),
            Instant::now() + Duration::from_secs(1),
            Arc::new(AtomicBool::new(false)),
        );
        assert!(matches!(outcome, CommandOutcome::Unknown(_)));
    }
}

#[cfg(test)]
pub(crate) mod test_support {
    use super::{CdpClient, Message};
    use serde_json::{Value, json};
    use std::collections::{HashMap, HashSet, VecDeque};
    use std::io::{self, BufRead, BufReader, Write};
    use std::net::{SocketAddr, TcpListener, TcpStream};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex};
    use std::thread::{self, JoinHandle};
    use std::time::{Duration, Instant};
    use tungstenite::accept;

    pub struct ScriptedChrome {
        pub address: SocketAddr,
        script: Arc<Mutex<Script>>,
        stop: Arc<AtomicBool>,
        worker: Option<JoinHandle<()>>,
    }

    #[derive(Default)]
    struct Script {
        received: Vec<Value>,
        received_times: Vec<Instant>,
        replies: HashMap<String, Vec<Value>>,
        evaluation_replies: HashMap<String, Value>,
        reject: HashSet<String>,
        silent: HashSet<String>,
        disconnect_on: HashSet<String>,
        reject_on_call: HashMap<String, usize>,
        pending_events: VecDeque<Value>,
        events_before_reply: HashMap<(String, usize), Vec<Value>>,
        ping_once: bool,
        invalid_json_methods: HashSet<String>,
        invalid_json_on_call: HashMap<String, usize>,
        binary_once: bool,
        http_status: Option<u16>,
        http_body: Option<Vec<u8>>,
        omit_content_length: bool,
        hold_after_headers: bool,
        raw_http: Option<Vec<u8>>,
    }

    impl ScriptedChrome {
        pub fn start() -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            listener.set_nonblocking(true).unwrap();
            let address = listener.local_addr().unwrap();
            let script = Arc::new(Mutex::new(Script::default()));
            let stop = Arc::new(AtomicBool::new(false));
            let worker_script = script.clone();
            let worker_stop = stop.clone();
            let worker = thread::spawn(move || serve(listener, worker_stop, worker_script));
            Self {
                address,
                script,
                stop,
                worker: Some(worker),
            }
        }

        pub fn endpoint(&self) -> crate::endpoint::Endpoint {
            crate::endpoint::Endpoint::parse(&self.address.to_string()).unwrap()
        }

        pub fn ws_url(&self) -> String {
            format!("ws://{}/devtools/page/page-1", self.address)
        }

        pub fn connect_observation(&self) -> Arc<CdpClient> {
            CdpClient::connect(self.ws_url(), true).unwrap()
        }

        pub fn connect_raw(&self) -> Arc<CdpClient> {
            CdpClient::connect(self.ws_url(), false).unwrap()
        }

        pub fn reply(&self, method: &str, result: Value) {
            self.script
                .lock()
                .expect("scripted Chrome")
                .replies
                .entry(method.to_owned())
                .or_default()
                .push(result);
        }

        pub fn received(&self, method: &str) -> Vec<Value> {
            self.script
                .lock()
                .expect("scripted Chrome")
                .received
                .iter()
                .filter(|value| value["method"] == method)
                .cloned()
                .collect()
        }

        pub fn received_times(&self, method: &str) -> Vec<Instant> {
            let script = self.script.lock().expect("scripted Chrome");
            script
                .received
                .iter()
                .zip(&script.received_times)
                .filter(|(value, _)| value["method"] == method)
                .map(|(_, time)| *time)
                .collect()
        }

        pub fn reply_evaluation(&self, expression: &str, result: Value) {
            self.script
                .lock()
                .expect("scripted Chrome")
                .evaluation_replies
                .insert(expression.to_owned(), result);
        }

        pub fn reject(&self, method: &str) {
            self.script
                .lock()
                .expect("scripted Chrome")
                .reject
                .insert(method.to_owned());
        }

        /// Receives `method` without ever answering it.
        pub fn silence(&self, method: &str) {
            self.script
                .lock()
                .expect("scripted Chrome")
                .silent
                .insert(method.to_owned());
        }

        /// Closes the connection after receiving `method`, without answering it.
        pub fn disconnect_on(&self, method: &str) {
            self.script
                .lock()
                .expect("scripted Chrome")
                .disconnect_on
                .insert(method.to_owned());
        }

        /// Every command received so far, in order, as its method and params.
        pub fn commands(&self) -> Vec<(String, Value)> {
            self.script
                .lock()
                .expect("scripted Chrome")
                .received
                .iter()
                .map(|value| {
                    let method = value["method"].as_str().unwrap_or_default().to_owned();
                    (method, value.get("params").cloned().unwrap_or(Value::Null))
                })
                .collect()
        }

        pub fn reject_on_call(&self, method: &str, call: usize) {
            self.script
                .lock()
                .expect("scripted Chrome")
                .reject_on_call
                .insert(method.to_owned(), call);
        }

        /// Sends `events` before answering the `call`-th `method` command, so the
        /// client records them before that command's outcome returns.
        pub fn emit_before_reply(&self, method: &str, call: usize, events: Vec<(&str, Value)>) {
            let events = events
                .into_iter()
                .map(|(event, params)| json!({"method": event, "params": params}))
                .collect();
            self.script
                .lock()
                .expect("scripted Chrome")
                .events_before_reply
                .insert((method.to_owned(), call), events);
        }

        pub fn push_event(&self, method: &str, params: Value) {
            self.script
                .lock()
                .expect("scripted Chrome")
                .pending_events
                .push_back(json!({"method": method, "params": params}));
        }

        pub fn ping_once(&self) {
            self.script.lock().expect("scripted Chrome").ping_once = true;
        }

        pub fn reply_invalid_json(&self, method: &str) {
            self.script
                .lock()
                .expect("scripted Chrome")
                .invalid_json_methods
                .insert(method.to_owned());
        }

        pub fn reply_invalid_json_on_call(&self, method: &str, call: usize) {
            self.script
                .lock()
                .expect("scripted Chrome")
                .invalid_json_on_call
                .insert(method.to_owned(), call);
        }

        pub fn send_binary_once(&self) {
            self.script.lock().expect("scripted Chrome").binary_once = true;
        }

        pub fn http_status(&self, status: u16) {
            self.script.lock().expect("scripted Chrome").http_status = Some(status);
        }

        pub fn http_body(&self, body: Vec<u8>) {
            self.script.lock().expect("scripted Chrome").http_body = Some(body);
        }

        pub fn omit_content_length(&self) {
            self.script
                .lock()
                .expect("scripted Chrome")
                .omit_content_length = true;
        }

        pub fn hold_after_headers(&self) {
            self.script
                .lock()
                .expect("scripted Chrome")
                .hold_after_headers = true;
        }

        pub fn raw_http(&self, bytes: Vec<u8>) {
            self.script.lock().expect("scripted Chrome").raw_http = Some(bytes);
        }
    }

    impl Drop for ScriptedChrome {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::SeqCst);
            if let Some(worker) = self.worker.take() {
                let _ = worker.join();
            }
        }
    }

    fn serve(listener: TcpListener, stop: Arc<AtomicBool>, script: Arc<Mutex<Script>>) {
        while !stop.load(Ordering::SeqCst) {
            match listener.accept() {
                Ok((stream, _)) => {
                    let script = script.clone();
                    thread::spawn(move || handle_client(stream, script));
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    thread::sleep(Duration::from_millis(2));
                }
                Err(_) => break,
            }
        }
    }

    fn handle_client(stream: TcpStream, script: Arc<Mutex<Script>>) {
        let _ = stream.set_nonblocking(false);
        let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
        let _ = stream.set_write_timeout(Some(Duration::from_secs(2)));
        let head = peek_request_head(&stream);
        if head.contains("/devtools/") || head.to_ascii_lowercase().contains("upgrade: websocket") {
            handle_websocket(stream, script);
        } else if !head.is_empty() {
            handle_http(stream, script);
        }
    }

    fn peek_request_head(stream: &TcpStream) -> String {
        let mut peek = [0_u8; 512];
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            match stream.peek(&mut peek) {
                Ok(0) => return String::new(),
                Ok(count) => return String::from_utf8_lossy(&peek[..count]).into_owned(),
                Err(error) if peek_should_retry(&error) && Instant::now() < deadline => {
                    thread::sleep(Duration::from_millis(2));
                }
                Err(_) => return String::new(),
            }
        }
    }

    fn peek_should_retry(error: &std::io::Error) -> bool {
        matches!(
            error.kind(),
            std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
        )
    }

    pub(crate) fn read_http_request_head(stream: &mut TcpStream) -> io::Result<String> {
        let mut reader = BufReader::new(stream);
        let mut head = String::new();
        // Closing with unread request bytes can reset the connection while the
        // discovery client is still sending headers. Consume the complete head.
        while !head.ends_with("\r\n\r\n") {
            if reader.read_line(&mut head)? == 0 {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "incomplete HTTP request headers",
                ));
            }
        }
        Ok(head)
    }

    fn handle_http(mut stream: TcpStream, script: Arc<Mutex<Script>>) {
        if read_http_request_head(&mut stream).is_err() {
            return;
        }
        if let Some(raw) = script.lock().expect("scripted Chrome").raw_http.clone() {
            let _ = stream.write_all(&raw);
            return;
        }
        let (status, body, omit_length, hold) = {
            let script = script.lock().expect("scripted Chrome");
            let status = script.http_status.unwrap_or(200);
            let body = script.http_body.clone().unwrap_or_else(|| {
                serde_json::to_vec(&json!([{
                    "id": "page-1",
                    "type": "page",
                    "title": "Fixture",
                    "webSocketDebuggerUrl": format!(
                        "ws://{}/devtools/page/page-1",
                        stream.local_addr().map(|address| address.to_string()).unwrap_or_default()
                    ),
                }]))
                .expect("scripted /json/list")
            });
            (
                status,
                body,
                script.omit_content_length,
                script.hold_after_headers,
            )
        };
        let length_header = if omit_length {
            String::new()
        } else {
            format!("Content-Length: {}\r\n", body.len())
        };
        let _ = write!(
            stream,
            "HTTP/1.1 {status} OK\r\nContent-Type: application/json\r\n{length_header}Connection: close\r\n\r\n"
        );
        let _ = stream.write_all(&body);
        if hold {
            thread::sleep(Duration::from_millis(80));
        }
    }

    fn handle_websocket(stream: TcpStream, script: Arc<Mutex<Script>>) {
        let mut socket = match accept(stream) {
            Ok(socket) => socket,
            Err(_) => return,
        };
        let _ = socket
            .get_mut()
            .set_read_timeout(Some(Duration::from_millis(20)));
        loop {
            flush_control_frames(&mut socket, &script);
            match socket.read() {
                Ok(Message::Text(text)) => {
                    let Ok(value) = serde_json::from_str::<Value>(&text) else {
                        continue;
                    };
                    if !reply_to_command(&mut socket, &script, value) {
                        break;
                    }
                }
                Ok(Message::Ping(payload)) => {
                    let _ = socket.send(Message::Pong(payload));
                }
                Ok(Message::Close(_)) | Err(tungstenite::Error::ConnectionClosed) => break,
                Err(tungstenite::Error::Io(error))
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                    ) => {}
                Ok(_) => {}
                Err(_) => break,
            }
        }
    }

    fn flush_control_frames(
        socket: &mut tungstenite::WebSocket<TcpStream>,
        script: &Arc<Mutex<Script>>,
    ) {
        let (events, ping, binary) = {
            let mut script = script.lock().expect("scripted Chrome");
            let events = script.pending_events.drain(..).collect::<Vec<_>>();
            let ping = std::mem::take(&mut script.ping_once);
            let binary = std::mem::take(&mut script.binary_once);
            (events, ping, binary)
        };
        if ping {
            let _ = socket.send(Message::Ping(Vec::new().into()));
        }
        if binary {
            let _ = socket.send(Message::Binary(vec![1, 2, 3].into()));
        }
        for event in events {
            let _ = socket.send(Message::Text(event.to_string().into()));
        }
    }

    /// Answers one command as scripted; returns whether the connection stays open.
    fn reply_to_command(
        socket: &mut tungstenite::WebSocket<TcpStream>,
        script: &Arc<Mutex<Script>>,
        value: Value,
    ) -> bool {
        let Some(id) = value.get("id").cloned() else {
            return true;
        };
        let method = value
            .get("method")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();
        let (reply, invalid, events) = {
            let mut script = script.lock().expect("scripted Chrome");
            script.received.push(value.clone());
            script.received_times.push(Instant::now());
            if script.disconnect_on.contains(&method) {
                return false;
            }
            if script.silent.contains(&method) {
                return true;
            }
            let call = script
                .received
                .iter()
                .filter(|item| item["method"] == method)
                .count();
            let invalid = script.invalid_json_methods.contains(&method)
                || script.invalid_json_on_call.get(&method) == Some(&call);
            let events = script
                .events_before_reply
                .remove(&(method.clone(), call))
                .unwrap_or_default();
            let reply = if script.reject.contains(&method)
                || script.reject_on_call.get(&method) == Some(&call)
            {
                json!({"id": id, "error": {"message": "rejected"}})
            } else {
                let evaluation = (method == "Runtime.evaluate")
                    .then(|| value.pointer("/params/expression").and_then(Value::as_str))
                    .flatten()
                    .and_then(|expression| script.evaluation_replies.get(expression).cloned());
                let result = evaluation.unwrap_or_else(|| {
                    script
                        .replies
                        .get_mut(&method)
                        .map(|replies| {
                            if replies.len() > 1 {
                                replies.remove(0)
                            } else {
                                replies[0].clone()
                            }
                        })
                        .unwrap_or(json!({}))
                });
                json!({"id": id, "result": result})
            };
            (reply, invalid, events)
        };
        for event in events {
            let _ = socket.send(Message::Text(event.to_string().into()));
        }
        if invalid {
            let _ = socket.send(Message::Text("not-json".into()));
            return true;
        }
        let _ = socket.send(Message::Text(reply.to_string().into()));
        true
    }
}
