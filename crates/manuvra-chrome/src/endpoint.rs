use serde_json::Value;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::time::Duration;

const MAX_DISCOVERY_BYTES: usize = 4 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Endpoint {
    address: SocketAddr,
}

impl Endpoint {
    pub fn parse(value: &str) -> Result<Self, EndpointError> {
        let address = value
            .parse::<SocketAddr>()
            .map_err(|_| EndpointError::Invalid(value.to_owned()))?;
        if !address.ip().is_loopback() || address.port() == 0 {
            return Err(EndpointError::NonLoopback(value.to_owned()));
        }
        Ok(Self { address })
    }

    pub fn label(&self) -> String {
        self.address.to_string()
    }

    pub fn get_json(&self, path: &str, timeout: Duration) -> Result<Value, EndpointError> {
        self.request_json("GET", path, timeout)
    }

    fn request_json(
        &self,
        method: &str,
        path: &str,
        timeout: Duration,
    ) -> Result<Value, EndpointError> {
        let mut stream = TcpStream::connect_timeout(&self.address, timeout)
            .map_err(|error| EndpointError::Io(error.to_string()))?;
        stream
            .set_read_timeout(Some(timeout))
            .and_then(|()| stream.set_write_timeout(Some(timeout)))
            .map_err(|error| EndpointError::Io(error.to_string()))?;
        let host = self.label();
        write!(
            stream,
            "{method} {path} HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\nAccept: application/json\r\n\r\n"
        )
        .map_err(|error| EndpointError::Io(error.to_string()))?;
        let response = read_response(&mut stream)?;
        parse_http_json(&response)
    }
}

fn read_response(stream: &mut TcpStream) -> Result<Vec<u8>, EndpointError> {
    let mut response = Vec::new();
    let mut chunk = [0_u8; 8192];
    while read_discovery_chunk(stream, &mut response, &mut chunk)? {}
    Ok(response)
}

fn read_discovery_chunk(
    stream: &mut TcpStream,
    response: &mut Vec<u8>,
    chunk: &mut [u8],
) -> Result<bool, EndpointError> {
    match stream.read(chunk) {
        Ok(0) => Ok(false),
        Ok(count) => Ok(!append_discovery_bytes(response, &chunk[..count])?),
        Err(error) if discovery_read_is_complete(response, &error) => Ok(false),
        Err(error) => Err(EndpointError::Io(error.to_string())),
    }
}

fn append_discovery_bytes(response: &mut Vec<u8>, chunk: &[u8]) -> Result<bool, EndpointError> {
    response.extend_from_slice(chunk);
    if response.len() > MAX_DISCOVERY_BYTES {
        return Err(EndpointError::TooLarge);
    }
    Ok(response_complete(response))
}

fn discovery_read_is_complete(response: &[u8], error: &std::io::Error) -> bool {
    discovery_timeout_kind(error)
        && !response.is_empty()
        && !declared_content_length_unsatisfied(response)
}

fn discovery_timeout_kind(error: &std::io::Error) -> bool {
    matches!(
        error.kind(),
        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
    )
}

fn declared_content_length_unsatisfied(response: &[u8]) -> bool {
    declared_content_length_end(response).is_some_and(|end| response.len() < end)
}

fn response_complete(response: &[u8]) -> bool {
    declared_content_length_end(response).is_some_and(|end| response.len() >= end)
}

fn declared_content_length_end(response: &[u8]) -> Option<usize> {
    let boundary = response
        .windows(4)
        .position(|window| window == b"\r\n\r\n")?;
    let head = std::str::from_utf8(&response[..boundary]).ok()?;
    let length = content_length_value(head)?;
    Some(boundary + 4 + length)
}

fn content_length_value(head: &str) -> Option<usize> {
    head.lines()
        .filter_map(|line| line.split_once(':'))
        .find(|(name, _)| name.eq_ignore_ascii_case("content-length"))
        .and_then(|(_, value)| value.trim().parse().ok())
}

fn parse_http_json(response: &[u8]) -> Result<Value, EndpointError> {
    let boundary = response
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .ok_or(EndpointError::MalformedHttp)?;
    let head =
        std::str::from_utf8(&response[..boundary]).map_err(|_| EndpointError::MalformedHttp)?;
    let status = head
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|value| value.parse::<u16>().ok())
        .ok_or(EndpointError::MalformedHttp)?;
    if status != 200 {
        return Err(EndpointError::HttpStatus(status));
    }
    serde_json::from_slice(&response[boundary + 4..])
        .map_err(|error| EndpointError::Json(error.to_string()))
}

#[derive(Debug, thiserror::Error)]
pub enum EndpointError {
    #[error("invalid Chrome endpoint: {0}")]
    Invalid(String),
    #[error("Chrome endpoint must be loopback: {0}")]
    NonLoopback(String),
    #[error("Chrome endpoint I/O failed: {0}")]
    Io(String),
    #[error("Chrome discovery response exceeded {MAX_DISCOVERY_BYTES} bytes")]
    TooLarge,
    #[error("malformed Chrome discovery HTTP response")]
    MalformedHttp,
    #[error("Chrome discovery returned HTTP status {0}")]
    HttpStatus(u16),
    #[error("Chrome discovery returned invalid JSON: {0}")]
    Json(String),
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    #[test]
    fn endpoint_parser_accepts_only_loopback_addresses_with_a_port() {
        assert_eq!(
            Endpoint::parse("127.0.0.1:1").unwrap().label(),
            "127.0.0.1:1"
        );
        assert_eq!(Endpoint::parse("[::1]:1").unwrap().label(), "[::1]:1");
        for rejected in ["192.0.2.1:9222", "127.0.0.1:0"] {
            assert!(matches!(
                Endpoint::parse(rejected),
                Err(EndpointError::NonLoopback(_))
            ));
        }
        assert!(matches!(
            Endpoint::parse("http://127.0.0.1:9222"),
            Err(EndpointError::Invalid(_))
        ));
    }

    fn get_json_when_listening(endpoint: &Endpoint) -> Value {
        let deadline = Instant::now() + Duration::from_secs(1);
        loop {
            match endpoint.get_json("/json/list", Duration::from_millis(200)) {
                Ok(value) => return value,
                Err(error) if Instant::now() < deadline && racy_discovery_io(&error) => {
                    std::thread::sleep(Duration::from_millis(5));
                }
                other => return other.expect("scripted Chrome /json/list"),
            }
        }
    }

    fn racy_discovery_io(error: &EndpointError) -> bool {
        matches!(
            error,
            EndpointError::Io(message)
                if message.contains("reset")
                    || message.contains("Broken pipe")
                    || message.contains("Connection refused")
        )
    }

    #[test]
    fn discovery_http_parses_json_and_rejects_status_or_oversize() {
        let chrome = crate::transport::test_support::ScriptedChrome::start();
        let endpoint = chrome.endpoint();
        let listed = get_json_when_listening(&endpoint);
        assert_eq!(listed[0]["id"], "page-1");

        chrome.http_status(404);
        assert!(matches!(
            endpoint.get_json("/json/list", Duration::from_secs(1)),
            Err(EndpointError::HttpStatus(404))
        ));

        chrome.http_status(200);
        chrome.http_body(b"not-json".to_vec());
        assert!(matches!(
            endpoint.get_json("/json/list", Duration::from_secs(1)),
            Err(EndpointError::Json(_))
        ));

        chrome.raw_http(b"not-http".to_vec());
        assert!(matches!(
            endpoint.get_json("/json/list", Duration::from_millis(200)),
            Err(EndpointError::MalformedHttp)
        ));

        let mut oversize = vec![b'x'; 16];
        assert!(append_discovery_bytes(&mut oversize, &[b'y'; MAX_DISCOVERY_BYTES]).is_err());
        let timeout = std::io::Error::new(std::io::ErrorKind::TimedOut, "timeout");
        assert!(discovery_read_is_complete(b"partial", &timeout));
        assert!(!discovery_read_is_complete(b"", &timeout));
        assert!(!discovery_read_is_complete(
            b"HTTP/1.1 200 OK\r\nContent-Length: 20\r\n\r\nshort",
            &timeout
        ));
    }

    #[test]
    fn partial_discovery_response_without_length_is_accepted_on_timeout() {
        let chrome = crate::transport::test_support::ScriptedChrome::start();
        chrome.omit_content_length();
        chrome.hold_after_headers();
        chrome.http_body(br#"[{"id":"page-1","type":"page","webSocketDebuggerUrl":"ws://127.0.0.1/devtools/page/page-1"}]"#.to_vec());
        let listed = chrome
            .endpoint()
            .get_json("/json/list", Duration::from_millis(40))
            .unwrap();
        assert_eq!(listed[0]["id"], "page-1");
    }
}
