//! Local origins and the owned Chromium behind the ignored live hosted-run tests, with a provider
//! that plays a careful agent on the page.

use crate::actions;
use crate::run::browser::{HostedBrowser, cleanup_started_browser};
use crate::run::capture::BrowserPage;
use crate::run::tests::support::MemoryJournal;
use manuvra_chrome::{
    BrowserConfig, BrowserError, CapturedPage, Observation, OwnedBrowser, PerformError,
    PerformFact, PreparedInput,
};
use manuvra_contract::Cleanup;
use serde_json::Value;
use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

pub(crate) struct OriginFixture {
    start_port: u16,
    foreign_port: u16,
    stop: Arc<AtomicBool>,
    workers: Vec<thread::JoinHandle<()>>,
}

impl OriginFixture {
    pub(crate) fn start() -> Self {
        let start = TcpListener::bind("127.0.0.1:0").unwrap();
        let foreign = TcpListener::bind("127.0.0.1:0").unwrap();
        start.set_nonblocking(true).unwrap();
        foreign.set_nonblocking(true).unwrap();
        let start_port = start.local_addr().unwrap().port();
        let foreign_port = foreign.local_addr().unwrap().port();
        let stop = Arc::new(AtomicBool::new(false));
        let start_body = format!(
            "<!doctype html><title>Origin guard</title><a href=\"http://127.0.0.1:{foreign_port}/landing\">Leave origin</a>"
        );
        let foreign_body =
            "<!doctype html><title>Foreign origin</title><p>Committed foreign origin</p>"
                .to_owned();
        let workers = vec![
            spawn_page_server(start, start_body, Arc::clone(&stop)),
            spawn_page_server(foreign, foreign_body, Arc::clone(&stop)),
        ];
        Self {
            start_port,
            foreign_port,
            stop,
            workers,
        }
    }

    pub(crate) fn start_url(&self) -> String {
        format!("http://127.0.0.1:{}/", self.start_port)
    }

    pub(crate) fn foreign_origin(&self) -> String {
        format!("http://127.0.0.1:{}", self.foreign_port)
    }
}

impl Drop for OriginFixture {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        let _ = TcpStream::connect(("127.0.0.1", self.start_port));
        let _ = TcpStream::connect(("127.0.0.1", self.foreign_port));
        for worker in self.workers.drain(..) {
            worker.join().unwrap();
        }
    }
}

fn spawn_page_server(
    listener: TcpListener,
    body: String,
    stop: Arc<AtomicBool>,
) -> thread::JoinHandle<()> {
    thread::spawn(move || {
        while !stop.load(Ordering::Relaxed) {
            match listener.accept() {
                Ok((mut stream, _)) => serve_origin_fixture(&mut stream, &body),
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    thread::sleep(Duration::from_millis(5));
                }
                Err(error) => panic!("origin fixture failed: {error}"),
            }
        }
    })
}

/// Serves the synthetic hover-reveal page on a temporary local origin.
pub(crate) struct HoverRevealFixture {
    port: u16,
    stop: Arc<AtomicBool>,
    worker: Option<thread::JoinHandle<()>>,
}

impl HoverRevealFixture {
    pub(crate) fn start() -> Self {
        Self::with_body(include_str!(
            "../../../../../tests/browser/hover-reveal.html"
        ))
    }

    pub(crate) fn with_body(body: &str) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let port = listener.local_addr().unwrap().port();
        let stop = Arc::new(AtomicBool::new(false));
        let body = body.to_owned();
        Self {
            port,
            worker: Some(spawn_page_server(listener, body, Arc::clone(&stop))),
            stop,
        }
    }

    pub(crate) fn url(&self) -> String {
        format!("http://127.0.0.1:{}/plan", self.port)
    }
}

impl Drop for HoverRevealFixture {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        let _ = TcpStream::connect(("127.0.0.1", self.port));
        if let Some(worker) = self.worker.take() {
            worker.join().unwrap();
        }
    }
}

fn serve_origin_fixture(stream: &mut TcpStream, body: &str) {
    let mut request = Vec::new();
    let mut buffer = [0_u8; 1024];
    while !request.windows(4).any(|window| window == b"\r\n\r\n") {
        let read = stream.read(&mut buffer).unwrap();
        if read == 0 {
            break;
        }
        request.extend_from_slice(&buffer[..read]);
    }
    let response = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        body.len(),
        body
    );
    stream.write_all(response.as_bytes()).unwrap();
}

/// The owned Chromium behind the hosted loop, recording the page it shows when the loop
/// closes it.
pub(crate) struct LiveBrowser {
    browser: OwnedBrowser,
    pub(crate) final_page: Option<Observation>,
}

impl LiveBrowser {
    pub(crate) fn open(url: &str) -> Self {
        let mut browser = OwnedBrowser::launch(BrowserConfig {
            explicit_binary: None,
            headless: true,
            width: 1120,
            height: 780,
            inherit_process_group: false,
        })
        .unwrap();
        browser.navigate(url).unwrap();
        Self {
            browser,
            final_page: None,
        }
    }
}

impl BrowserPage for LiveBrowser {
    fn capture_redacted_page(&self, sensitive: &[String]) -> Result<CapturedPage, BrowserError> {
        self.browser.capture_redacted_page(sensitive)
    }
    fn observe_page(&self) -> Result<Observation, BrowserError> {
        self.browser.observe_page()
    }
}

impl actions::Performer for LiveBrowser {
    fn dispatch(
        &self,
        input: PreparedInput,
        cancellation: &manuvra_chrome::InputCancellation,
    ) -> Result<PerformFact, PerformError> {
        self.browser.dispatch(input, cancellation)
    }
}

impl HostedBrowser for LiveBrowser {
    fn cleanup_hosted(&mut self) -> Cleanup {
        self.final_page = self.browser.observe().ok();
        cleanup_started_browser(&mut self.browser)
    }
}

/// Plays a careful agent on a live page: for each step goal it clicks the wanted control when
/// it is a visible candidate, otherwise hovers the region that reveals it, and judges a
/// natural done condition from the wanted control's expanded state.
pub(crate) struct RowActionProvider {
    scroll_when_missing: bool,
    wanted: Vec<(&'static str, &'static str)>,
    pub(crate) choices: Mutex<Vec<(String, String, Value)>>,
}

impl RowActionProvider {
    pub(crate) fn new(wanted: Vec<(&'static str, &'static str)>) -> Self {
        Self {
            scroll_when_missing: false,
            wanted,
            choices: Mutex::new(Vec::new()),
        }
    }

    pub(crate) fn scrolling(wanted: Vec<(&'static str, &'static str)>) -> Self {
        let mut provider = Self::new(wanted);
        provider.scroll_when_missing = true;
        provider
    }

    fn wanted_control(&self, request: &Value) -> &'static str {
        let goal = request["state"]["current_step"]["goal"].as_str().unwrap();
        self.wanted
            .iter()
            .find(|(step_goal, _)| *step_goal == goal)
            .map(|(_, control)| *control)
            .expect("scripted goal")
    }

    fn choose(request: &Value, wanted: &str) -> (&'static str, String) {
        request["questions"]["click_target"]["criteria"]
            .as_object()
            .unwrap()
            .iter()
            .find(|(_, criterion)| criterion["name"] == wanted && criterion["disabled"] == false)
            .map_or(("BLOCKED", String::new()), |(key, _)| {
                ("CLICK", key.clone())
            })
    }

    pub(crate) fn calls(&self) -> usize {
        self.choices.lock().unwrap().len()
    }
}

impl manuvra_jev::Evaluator for RowActionProvider {
    fn evaluate(
        &self,
        request: &Value,
        _deadline: Instant,
    ) -> Result<manuvra_jev::Evaluation, manuvra_jev::JevError> {
        let wanted = self.wanted_control(request);
        let (mut operation, click) = Self::choose(request, wanted);
        if self.scroll_when_missing && operation == "BLOCKED" {
            operation = "SCROLL_DOWN";
        }
        let expanded = request["state"]["page"]["elements"]
            .as_array()
            .unwrap()
            .iter()
            .any(|element| element["name"] == wanted && element["expanded"] == true);
        let target_name = &click;
        self.choices.lock().unwrap().push((
            operation.to_owned(),
            target_name.clone(),
            request.clone(),
        ));
        let choice = |selected: &str| manuvra_jev::Answer::Choice {
            choice: selected.into(),
            probabilities: BTreeMap::from([(selected.into(), 0.9)]),
            confidence: 0.9,
        };
        let answers = BTreeMap::from([
            ("operation".into(), choice(operation)),
            ("click_target".into(), choice(&click)),
            ("type_target".into(), choice("NO_TYPE_TEXT_TARGET")),
            ("select_target".into(), choice("NO_SELECT_TARGET")),
            ("type_value".into(), choice("NONE_FITS")),
            ("key".into(), choice("Escape")),
            (
                "step_done".into(),
                manuvra_jev::Answer::Noul {
                    noul: if expanded { 0.95 } else { 0.05 },
                },
            ),
        ]);
        Ok(manuvra_jev::Evaluation {
            answers,
            usage: BTreeMap::new(),
            request_id: Some("row-action-script".into()),
            model: "jev-1.13.0".into(),
        })
    }
}

pub(crate) fn prepared_actions(journal: &MemoryJournal) -> Vec<String> {
    journal
        .prepared()
        .into_iter()
        .map(|entry| {
            let target = entry
                .get("hover_target")
                .unwrap_or(&entry["target"])
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or_default();
            format!("{} {target}", entry["operation"].as_str().unwrap())
        })
        .collect()
}
