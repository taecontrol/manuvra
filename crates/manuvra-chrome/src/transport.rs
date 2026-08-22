use serde_json::{Value, json};
use std::collections::VecDeque;
use std::io;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, Sender, SyncSender, TryRecvError};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::{Duration, Instant};
use tungstenite::stream::MaybeTlsStream;
use tungstenite::{Message, WebSocket, connect};

const SOCKET_POLL: Duration = Duration::from_millis(10);
const MAX_EVENTS: usize = 10_000;
const MAX_EVENT_BYTES: usize = 8 * 1024 * 1024;

#[derive(Debug, Clone)]
pub struct JournalEvent {
    pub cursor: u64,
    pub action_sequence: u64,
    pub received_ms: u64,
    pub message: Value,
}

#[derive(Debug, Clone)]
pub struct JournalSnapshot {
    pub events: Vec<JournalEvent>,
    pub overflowed: bool,
    pub last_cursor: u64,
}

#[derive(Debug)]
struct Journal {
    started: Instant,
    next_cursor: u64,
    bytes: usize,
    overflowed: bool,
    events: VecDeque<JournalEvent>,
}

impl Default for Journal {
    fn default() -> Self {
        Self {
            started: Instant::now(),
            next_cursor: 1,
            bytes: 0,
            overflowed: false,
            events: VecDeque::new(),
        }
    }
}

impl Journal {
    fn record(&mut self, message: Value, action_sequence: u64) {
        let bytes = serde_json::to_vec(&message).map_or(MAX_EVENT_BYTES + 1, |value| value.len());
        if self.events.len() >= MAX_EVENTS || self.bytes.saturating_add(bytes) > MAX_EVENT_BYTES {
            self.overflowed = true;
            return;
        }
        let event = JournalEvent {
            cursor: self.next_cursor,
            action_sequence,
            received_ms: self.started.elapsed().as_millis().min(u128::from(u64::MAX)) as u64,
            message,
        };
        self.next_cursor += 1;
        self.bytes += bytes;
        self.events.push_back(event);
    }

    fn cursor(&self) -> u64 {
        self.next_cursor.saturating_sub(1)
    }

    fn snapshot_since(&self, cursor: u64) -> JournalSnapshot {
        JournalSnapshot {
            events: self
                .events
                .iter()
                .filter(|event| event.cursor > cursor)
                .cloned()
                .collect(),
            overflowed: self.overflowed,
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
    journal: Arc<(Mutex<Journal>, Condvar)>,
    action_sequence: Arc<AtomicU64>,
    disconnected: Arc<AtomicBool>,
}

impl CdpClient {
    pub fn connect(url: String, observe: bool) -> Result<Arc<Self>, String> {
        let (sender, receiver) = mpsc::channel();
        let journal = Arc::new((Mutex::new(Journal::default()), Condvar::new()));
        let action_sequence = Arc::new(AtomicU64::new(0));
        let disconnected = Arc::new(AtomicBool::new(false));
        let mut socket = connect(&url)
            .map_err(|error| format!("WebSocket connection failed: {error}"))?
            .0;
        configure_timeout(&mut socket, SOCKET_POLL)?;
        let client = Arc::new(Self {
            sender,
            journal: journal.clone(),
            action_sequence: action_sequence.clone(),
            disconnected: disconnected.clone(),
        });
        thread::Builder::new()
            .name("manuvra-cdp".to_owned())
            .spawn(move || {
                worker(
                    &mut socket,
                    receiver,
                    journal,
                    action_sequence,
                    disconnected,
                    observe,
                )
            })
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

    pub fn set_action_sequence(&self, sequence: u64) {
        self.action_sequence.store(sequence, Ordering::SeqCst);
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

    pub fn is_disconnected(&self) -> bool {
        self.disconnected.load(Ordering::SeqCst)
    }

    pub fn wait_for_journal_change(&self, cursor: u64, timeout: Duration) {
        let guard = self.journal.0.lock().expect("CDP journal");
        if guard.cursor() != cursor || guard.overflowed {
            return;
        }
        let _ = self.journal.1.wait_timeout(guard, timeout);
    }
}

fn worker(
    socket: &mut WebSocket<MaybeTlsStream<std::net::TcpStream>>,
    receiver: Receiver<Request>,
    journal: Arc<(Mutex<Journal>, Condvar)>,
    action_sequence: Arc<AtomicU64>,
    disconnected: Arc<AtomicBool>,
    observe: bool,
) {
    let mut next_id = 1_u64;
    if observe && initialize(socket, &journal, &action_sequence, &mut next_id).is_err() {
        disconnected.store(true, Ordering::SeqCst);
        return;
    }
    drive_worker(
        socket,
        receiver,
        journal,
        action_sequence,
        disconnected,
        &mut next_id,
    );
}

fn drive_worker(
    socket: &mut WebSocket<MaybeTlsStream<std::net::TcpStream>>,
    receiver: Receiver<Request>,
    journal: Arc<(Mutex<Journal>, Condvar)>,
    action_sequence: Arc<AtomicU64>,
    disconnected: Arc<AtomicBool>,
    next_id: &mut u64,
) {
    loop {
        match receiver.try_recv() {
            Ok(request) => fulfill_request(socket, request, &journal, &action_sequence, next_id),
            Err(TryRecvError::Empty) => {
                if !poll_incoming(socket, &journal, &action_sequence, &disconnected) {
                    break;
                }
            }
            Err(TryRecvError::Disconnected) => break,
        }
    }
}

fn fulfill_request(
    socket: &mut WebSocket<MaybeTlsStream<std::net::TcpStream>>,
    request: Request,
    journal: &Arc<(Mutex<Journal>, Condvar)>,
    action_sequence: &Arc<AtomicU64>,
    next_id: &mut u64,
) {
    let outcome = execute(
        socket,
        request.method,
        request.params,
        request.deadline,
        &request.cancellation,
        journal,
        action_sequence,
        next_id,
    );
    let _ = request.reply.send(outcome);
}

fn poll_incoming(
    socket: &mut WebSocket<MaybeTlsStream<std::net::TcpStream>>,
    journal: &Arc<(Mutex<Journal>, Condvar)>,
    action_sequence: &Arc<AtomicU64>,
    disconnected: &Arc<AtomicBool>,
) -> bool {
    match read_message(socket) {
        Ok(Some(value)) => {
            record_event(journal, action_sequence, value);
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

fn initialize(
    socket: &mut WebSocket<MaybeTlsStream<std::net::TcpStream>>,
    journal: &Arc<(Mutex<Journal>, Condvar)>,
    action_sequence: &Arc<AtomicU64>,
    next_id: &mut u64,
) -> Result<(), String> {
    let cancellation = Arc::new(AtomicBool::new(false));
    for (method, params) in [
        ("Page.enable", json!({})),
        ("Page.setLifecycleEventsEnabled", json!({"enabled": true})),
        ("DOM.enable", json!({})),
        ("Accessibility.enable", json!({})),
        ("Network.enable", json!({})),
        ("Runtime.enable", json!({})),
        ("Log.enable", json!({})),
    ] {
        let outcome = execute(
            socket,
            method.to_owned(),
            params,
            Instant::now() + Duration::from_secs(2),
            &cancellation,
            journal,
            action_sequence,
            next_id,
        );
        if !matches!(outcome, CommandOutcome::Confirmed(_)) {
            return Err(format!("CDP initialization failed at {method}"));
        }
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn execute(
    socket: &mut WebSocket<MaybeTlsStream<std::net::TcpStream>>,
    method: String,
    params: Value,
    deadline: Instant,
    cancellation: &Arc<AtomicBool>,
    journal: &Arc<(Mutex<Journal>, Condvar)>,
    action_sequence: &Arc<AtomicU64>,
    next_id: &mut u64,
) -> CommandOutcome {
    if command_should_not_send(cancellation, deadline) {
        return CommandOutcome::NotSent("cancelled or timed out before send".to_owned());
    }
    let id = take_next_id(next_id);
    let message = json!({"id": id, "method": method, "params": params});
    if let Err(error) = socket.send(Message::Text(message.to_string().into())) {
        return CommandOutcome::NotSent(format!("WebSocket send failed: {error}"));
    }
    await_command_response(socket, id, deadline, cancellation, journal, action_sequence)
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
    socket: &mut WebSocket<MaybeTlsStream<std::net::TcpStream>>,
    id: u64,
    deadline: Instant,
    cancellation: &Arc<AtomicBool>,
    journal: &Arc<(Mutex<Journal>, Condvar)>,
    action_sequence: &Arc<AtomicU64>,
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
            CommandIncoming::Event(value) => record_event(journal, action_sequence, value),
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

fn record_event(
    journal: &Arc<(Mutex<Journal>, Condvar)>,
    action_sequence: &Arc<AtomicU64>,
    value: Value,
) {
    journal
        .0
        .lock()
        .expect("CDP journal")
        .record(value, action_sequence.load(Ordering::SeqCst));
    journal.1.notify_all();
}

fn read_message(
    socket: &mut WebSocket<MaybeTlsStream<std::net::TcpStream>>,
) -> Result<Option<Value>, String> {
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
    socket: &mut WebSocket<MaybeTlsStream<std::net::TcpStream>>,
    payload: tungstenite::Bytes,
) -> Result<Option<Value>, String> {
    socket
        .send(Message::Pong(payload))
        .map_err(|error| format!("CDP pong failed: {error}"))?;
    Ok(None)
}

fn configure_timeout(
    socket: &mut WebSocket<MaybeTlsStream<std::net::TcpStream>>,
    timeout: Duration,
) -> Result<(), String> {
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

pub fn is_log_event(event: &JournalEvent) -> bool {
    matches!(
        event_method(event),
        Some("Runtime.consoleAPICalled" | "Runtime.exceptionThrown" | "Log.entryAdded")
    )
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

    #[test]
    fn journal_overflow_is_explicit_and_never_looks_complete() {
        let mut journal = Journal::default();
        for index in 0..=MAX_EVENTS {
            journal.record(
                json!({"method": "DOM.childNodeInserted", "index": index}),
                7,
            );
        }
        let snapshot = journal.snapshot_since(0);
        assert!(snapshot.overflowed);
        assert_eq!(snapshot.events.len(), MAX_EVENTS);
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
            action_sequence: 0,
            received_ms: 0,
            message: json!({"method": method, "params": params}),
        }
    }

    #[test]
    fn command_outcome_result_maps_confirmed_rejected_and_unsent() {
        assert_eq!(
            CommandOutcome::Confirmed(json!({"result": {"ok": true}}))
                .result()
                .unwrap(),
            json!({"ok": true})
        );
        assert_eq!(
            CommandOutcome::Confirmed(json!({"id": 1}))
                .result()
                .unwrap(),
            Value::Null
        );
        assert!(matches!(
            CommandOutcome::Rejected(json!({"error": {"message": "no"}})).result(),
            Err(CommandFailure::Rejected(_))
        ));
        assert!(matches!(
            CommandOutcome::NotSent("queued".to_owned()).result(),
            Err(CommandFailure::NotSent(_))
        ));
        assert!(matches!(
            CommandOutcome::Unknown("maybe".to_owned()).result(),
            Err(CommandFailure::Unknown(_))
        ));
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
    use std::io::{Read, Write};
    use std::net::{SocketAddr, TcpListener, TcpStream};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex};
    use std::thread::{self, JoinHandle};
    use std::time::Duration;
    use tungstenite::accept;

    pub struct ScriptedChrome {
        pub address: SocketAddr,
        script: Arc<Mutex<Script>>,
        stop: Arc<AtomicBool>,
        worker: Option<JoinHandle<()>>,
    }

    #[derive(Default)]
    struct Script {
        replies: HashMap<String, Value>,
        reject: HashSet<String>,
        events_after: HashMap<String, Vec<Value>>,
        events_after_once: HashMap<String, Vec<Value>>,
        pending_events: VecDeque<Value>,
        ping_once: bool,
        invalid_json_methods: HashSet<String>,
        binary_once: bool,
        http_status: Option<u16>,
        http_body: Option<Vec<u8>>,
        omit_content_length: bool,
        hold_after_headers: bool,
        raw_http: Option<Vec<u8>>,
        shutdown: bool,
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

        pub fn endpoint(&self) -> crate::Endpoint {
            crate::Endpoint::parse(&self.address.to_string()).unwrap()
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
                .insert(method.to_owned(), result);
        }

        pub fn reject(&self, method: &str) {
            self.script
                .lock()
                .expect("scripted Chrome")
                .reject
                .insert(method.to_owned());
        }

        pub fn events_after(&self, method: &str, events: Vec<Value>) {
            self.script
                .lock()
                .expect("scripted Chrome")
                .events_after
                .insert(method.to_owned(), events);
        }

        pub fn events_after_once(&self, method: &str, events: Vec<Value>) {
            self.script
                .lock()
                .expect("scripted Chrome")
                .events_after_once
                .insert(method.to_owned(), events);
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

        pub fn disconnect(&self) {
            self.script.lock().expect("scripted Chrome").shutdown = true;
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
        let mut peek = [0_u8; 512];
        let count = stream.peek(&mut peek).unwrap_or(0);
        let head = String::from_utf8_lossy(&peek[..count]);
        if head.contains("/devtools/") || head.to_ascii_lowercase().contains("upgrade: websocket") {
            handle_websocket(stream, script);
        } else {
            handle_http(stream, script);
        }
    }

    fn handle_http(mut stream: TcpStream, script: Arc<Mutex<Script>>) {
        let mut request = [0_u8; 2048];
        let _ = stream.read(&mut request);
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
            if script.lock().expect("scripted Chrome").shutdown {
                let _ = socket.close(None);
                break;
            }
            flush_control_frames(&mut socket, &script);
            match socket.read() {
                Ok(Message::Text(text)) => {
                    let Ok(value) = serde_json::from_str::<Value>(&text) else {
                        continue;
                    };
                    reply_to_command(&mut socket, &script, value);
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

    fn reply_to_command(
        socket: &mut tungstenite::WebSocket<TcpStream>,
        script: &Arc<Mutex<Script>>,
        value: Value,
    ) {
        let Some(id) = value.get("id").cloned() else {
            return;
        };
        let method = value
            .get("method")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();
        let (reply, events, invalid) = {
            let mut script = script.lock().expect("scripted Chrome");
            let invalid = script.invalid_json_methods.contains(&method);
            let reply = if script.reject.contains(&method) {
                json!({"id": id, "error": {"message": "rejected"}})
            } else {
                let result = script.replies.get(&method).cloned().unwrap_or(json!({}));
                json!({"id": id, "result": result})
            };
            let mut events = script
                .events_after
                .get(&method)
                .cloned()
                .unwrap_or_default();
            if let Some(once) = script.events_after_once.remove(&method) {
                events.extend(once);
            }
            (reply, events, invalid)
        };
        if invalid {
            let _ = socket.send(Message::Text("not-json".into()));
            return;
        }
        let _ = socket.send(Message::Text(reply.to_string().into()));
        for event in events {
            let _ = socket.send(Message::Text(event.to_string().into()));
        }
    }
}
