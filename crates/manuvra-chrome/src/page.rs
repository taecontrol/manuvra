//! Screenshot capture for the owned page.

use crate::transport::{CdpClient, CommandFailure};
use base64::Engine;
use serde_json::{Value, json};
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::Instant;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Screenshot {
    pub bytes: Vec<u8>,
    pub width: u32,
    pub height: u32,
}

#[derive(Debug, thiserror::Error)]
pub enum PageError {
    #[error(transparent)]
    Command(#[from] CommandFailure),
    #[error("Chrome screenshot response omitted image data")]
    MissingScreenshot,
    #[error("Chrome screenshot response contained invalid base64")]
    InvalidBase64,
    #[error("Chrome screenshot response was not a PNG")]
    InvalidPng,
}

pub fn capture_screenshot(
    client: &CdpClient,
    deadline: Instant,
    cancellation: Arc<AtomicBool>,
) -> Result<Screenshot, PageError> {
    let result = client
        .command(
            "Page.captureScreenshot",
            json!({"format": "png", "fromSurface": true}),
            deadline,
            cancellation,
        )
        .result()?;
    let encoded = result
        .get("data")
        .and_then(Value::as_str)
        .ok_or(PageError::MissingScreenshot)?;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(encoded)
        .map_err(|_| PageError::InvalidBase64)?;
    let (width, height) = png_dimensions(&bytes).ok_or(PageError::InvalidPng)?;
    Ok(Screenshot {
        bytes,
        width,
        height,
    })
}

fn png_dimensions(bytes: &[u8]) -> Option<(u32, u32)> {
    (bytes.len() >= 24 && &bytes[..8] == b"\x89PNG\r\n\x1a\n").then(|| {
        (
            u32::from_be_bytes(bytes[16..20].try_into().expect("PNG width")),
            u32::from_be_bytes(bytes[20..24].try_into().expect("PNG height")),
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transport::test_support::ScriptedChrome;
    use serde_json::json;
    use std::sync::atomic::AtomicBool;
    use std::time::Duration;

    fn deadline() -> Instant {
        Instant::now() + Duration::from_secs(1)
    }

    fn cancellation() -> Arc<AtomicBool> {
        Arc::new(AtomicBool::new(false))
    }

    #[test]
    fn screenshot_decodes_png_dimensions() {
        let mut png = b"\x89PNG\r\n\x1a\n".to_vec();
        png.extend_from_slice(&[0; 8]);
        png.extend_from_slice(&640_u32.to_be_bytes());
        png.extend_from_slice(&480_u32.to_be_bytes());
        let chrome = ScriptedChrome::start();
        chrome.reply(
            "Page.captureScreenshot",
            json!({"data": base64::engine::general_purpose::STANDARD.encode(&png)}),
        );
        let client = chrome.connect_raw();

        let screenshot = capture_screenshot(&client, deadline(), cancellation()).unwrap();

        assert_eq!(screenshot.bytes, png);
        assert_eq!((screenshot.width, screenshot.height), (640, 480));
    }

    #[test]
    fn malformed_screenshot_is_rejected() {
        let chrome = ScriptedChrome::start();
        chrome.reply("Page.captureScreenshot", json!({"data": "not base64!"}));
        let client = chrome.connect_raw();

        assert!(matches!(
            capture_screenshot(&client, deadline(), cancellation()),
            Err(PageError::InvalidBase64)
        ));
    }
}
