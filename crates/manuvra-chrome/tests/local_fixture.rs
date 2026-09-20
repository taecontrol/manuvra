#![cfg(target_os = "linux")]

use manuvra_chrome::{BrowserConfig, OwnedBrowser};
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;

const FIXTURE: &str = include_str!("../../../tests/fixtures/browser-adversarial.html");

struct FixtureServer {
    address: std::net::SocketAddr,
    stop: Arc<AtomicBool>,
    worker: Option<thread::JoinHandle<()>>,
}

impl FixtureServer {
    fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let address = listener.local_addr().unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let worker_stop = Arc::clone(&stop);
        let worker = thread::spawn(move || {
            while !worker_stop.load(Ordering::Relaxed) {
                match listener.accept() {
                    Ok((mut stream, _)) => serve_fixture(&mut stream),
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(std::time::Duration::from_millis(5));
                    }
                    Err(error) => panic!("fixture server failed: {error}"),
                }
            }
        });
        Self {
            address,
            stop,
            worker: Some(worker),
        }
    }

    fn url(&self) -> String {
        format!("http://{}/", self.address)
    }
}

impl Drop for FixtureServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Ok(mut stream) = TcpStream::connect(self.address) {
            let _ = stream.write_all(b"GET /shutdown HTTP/1.1\r\n\r\n");
        }
        self.worker.take().unwrap().join().unwrap();
    }
}

fn serve_fixture(stream: &mut TcpStream) {
    let mut request = [0_u8; 2048];
    let _ = stream.read(&mut request);
    let response = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        FIXTURE.len(),
        FIXTURE
    );
    stream.write_all(response.as_bytes()).unwrap();
}

#[test]
#[ignore = "requires the local Chromium executable"]
fn production_snapshot_and_masking_cover_truncation_split_nodes_and_zero_masks() {
    let server = FixtureServer::start();
    let mut browser = OwnedBrowser::launch(BrowserConfig {
        explicit_binary: None,
        headless: true,
        width: 1120,
        height: 780,
    })
    .unwrap();
    browser.navigate(&server.url()).unwrap();

    let observed = browser.observe().unwrap();
    assert!(!observed.coverage.viewport_complete);
    for gap in [
        "visible_text_truncated",
        "covered_text_truncated",
        "dialog_text_truncated",
    ] {
        assert!(observed.coverage.gaps.iter().any(|actual| actual == gap));
    }
    assert!(observed.visible_text.len() <= 8000);
    assert!(observed.covered_text.len() <= 8000);
    assert!(observed.dialog_texts["Long dialog"].len() <= 8000);

    let plain = browser.capture().unwrap();
    let masked = browser.capture_redacted(&["split-secret".into()]).unwrap();
    assert!(masked.redaction.verifies(1));
    assert_eq!(masked.redaction.matched_values, 1);
    assert!(masked.redaction.mask_count >= 1);
    assert_ne!(plain.screenshot.bytes, masked.screenshot.bytes);

    let absent = browser
        .capture_redacted(&["not-rendered-anywhere".into()])
        .unwrap();
    assert!(absent.redaction.verifies(1));
    assert_eq!(absent.redaction.matched_values, 0);
    assert_eq!(absent.redaction.mask_count, 0);
    browser.close().unwrap();
}
