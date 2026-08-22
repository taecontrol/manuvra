use serde_json::Value;
use std::io::{Read, Write};
use std::net::{IpAddr, SocketAddr, TcpStream};
use std::time::Duration;

const DEFAULT_ENDPOINT: &str = "127.0.0.1:9222";
const MAX_ENDPOINTS: usize = 8;
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

    pub fn configured(value: Option<&str>) -> Result<Vec<Self>, EndpointError> {
        let mut endpoints = Vec::new();
        for item in value.unwrap_or(DEFAULT_ENDPOINT).split(',') {
            push_configured_endpoint(&mut endpoints, item)?;
        }
        if endpoints.is_empty() || endpoints.len() > MAX_ENDPOINTS {
            return Err(EndpointError::Count(endpoints.len()));
        }
        Ok(endpoints)
    }

    pub fn label(&self) -> String {
        self.address.to_string()
    }

    pub fn port(&self) -> u16 {
        self.address.port()
    }

    pub fn ip(&self) -> IpAddr {
        self.address.ip()
    }

    pub fn websocket_url(&self, path: &str) -> Result<String, EndpointError> {
        if !path.starts_with('/') || path.contains('#') || path.contains(char::is_whitespace) {
            return Err(EndpointError::InvalidWebSocketPath(path.to_owned()));
        }
        let host = match self.address.ip() {
            IpAddr::V4(address) => address.to_string(),
            IpAddr::V6(address) => format!("[{address}]"),
        };
        Ok(format!("ws://{host}:{}{path}", self.address.port()))
    }

    pub fn get_json(&self, path: &str, timeout: Duration) -> Result<Value, EndpointError> {
        self.request_json("GET", path, timeout)
            .map_err(RequestFailure::into_endpoint)
    }

    pub(crate) fn get_json_for_probe(
        &self,
        path: &str,
        timeout: Duration,
    ) -> Result<Value, RequestFailure> {
        self.request_json("GET", path, timeout)
    }

    pub(crate) fn put_json(&self, path: &str, timeout: Duration) -> Result<Value, EndpointError> {
        self.request_json("PUT", path, timeout)
            .map_err(RequestFailure::into_endpoint)
    }

    fn request_json(
        &self,
        method: &str,
        path: &str,
        timeout: Duration,
    ) -> Result<Value, RequestFailure> {
        let mut stream =
            TcpStream::connect_timeout(&self.address, timeout).map_err(RequestFailure::io)?;
        stream
            .set_read_timeout(Some(timeout))
            .and_then(|()| stream.set_write_timeout(Some(timeout)))
            .map_err(RequestFailure::io)?;
        let host = self.label();
        write!(
            stream,
            "{method} {path} HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\nAccept: application/json\r\n\r\n"
        )
        .map_err(RequestFailure::io)?;
        let response = read_response(&mut stream)?;
        parse_http_json(&response).map_err(RequestFailure::definitive)
    }
}

#[derive(Debug)]
pub(crate) struct RequestFailure {
    error: EndpointError,
    transient: bool,
}

impl RequestFailure {
    fn io(error: std::io::Error) -> Self {
        let transient = matches!(
            error.kind(),
            std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
        );
        Self {
            error: EndpointError::Io(error.to_string()),
            transient,
        }
    }

    fn definitive(error: EndpointError) -> Self {
        Self {
            error,
            transient: false,
        }
    }

    fn into_endpoint(self) -> EndpointError {
        self.error
    }

    pub(crate) fn is_connection_refused(&self) -> bool {
        self.error.is_connection_refused()
    }

    pub(crate) fn is_transient(&self) -> bool {
        self.transient
    }
}

impl std::fmt::Display for RequestFailure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Display::fmt(&self.error, formatter)
    }
}

fn push_configured_endpoint(
    endpoints: &mut Vec<Endpoint>,
    item: &str,
) -> Result<(), EndpointError> {
    if item.trim() != item || item.is_empty() {
        return Err(EndpointError::Invalid(item.to_owned()));
    }
    let endpoint = Endpoint::parse(item)?;
    if !endpoints.contains(&endpoint) {
        endpoints.push(endpoint);
    }
    Ok(())
}

fn read_response(stream: &mut TcpStream) -> Result<Vec<u8>, RequestFailure> {
    let mut response = Vec::new();
    let mut chunk = [0_u8; 8192];
    while read_discovery_chunk(stream, &mut response, &mut chunk)? {}
    Ok(response)
}

fn read_discovery_chunk(
    stream: &mut TcpStream,
    response: &mut Vec<u8>,
    chunk: &mut [u8],
) -> Result<bool, RequestFailure> {
    match stream.read(chunk) {
        Ok(0) => Ok(false),
        Ok(count) => Ok(!append_discovery_bytes(response, &chunk[..count])?),
        Err(error) if discovery_read_is_complete(response, &error) => Ok(false),
        Err(error) => Err(RequestFailure::io(error)),
    }
}

fn append_discovery_bytes(response: &mut Vec<u8>, chunk: &[u8]) -> Result<bool, RequestFailure> {
    response.extend_from_slice(chunk);
    if response.len() > MAX_DISCOVERY_BYTES {
        return Err(RequestFailure::definitive(EndpointError::TooLarge));
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
    #[error("Chrome endpoint count must be between 1 and {MAX_ENDPOINTS}, got {0}")]
    Count(usize),
    #[error("invalid DevTools WebSocket path: {0}")]
    InvalidWebSocketPath(String),
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

impl EndpointError {
    pub fn is_connection_refused(&self) -> bool {
        matches!(self, Self::Io(message) if connection_refused_text(message))
    }
}

pub fn connection_refused_text(message: &str) -> bool {
    message.contains("Connection refused") || message.contains("connection refused")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn endpoint_configuration_is_loopback_bounded_and_deduplicated() {
        let endpoints =
            Endpoint::configured(Some("127.0.0.1:9222,[::1]:9333,127.0.0.1:9222")).unwrap();
        assert_eq!(endpoints.len(), 2);
        assert!(Endpoint::configured(Some("192.0.2.1:9222")).is_err());
        assert!(Endpoint::configured(Some("http://127.0.0.1:9222")).is_err());
        assert!(Endpoint::configured(Some("127.0.0.1:9222, 127.0.0.1:9333")).is_err());
        assert_eq!(
            Endpoint::parse("127.0.0.1:9222").unwrap().ip(),
            IpAddr::V4(std::net::Ipv4Addr::LOCALHOST)
        );
        assert!(
            EndpointError::Io("Connection refused (os error 61)".to_owned())
                .is_connection_refused()
        );
        assert!(!EndpointError::HttpStatus(404).is_connection_refused());
        assert_eq!(
            Endpoint::configured(None).unwrap()[0].label(),
            "127.0.0.1:9222"
        );
        assert!(Endpoint::configured(Some("")).is_err());
        assert!(Endpoint::configured(Some("127.0.0.1:0")).is_err());
        let too_many = (9222..9232)
            .map(|port| format!("127.0.0.1:{port}"))
            .collect::<Vec<_>>()
            .join(",");
        assert!(Endpoint::configured(Some(&too_many)).is_err());
    }

    #[test]
    fn websocket_url_reuses_only_the_validated_endpoint_authority() {
        let endpoint = Endpoint::parse("127.0.0.1:9222").unwrap();
        assert_eq!(
            endpoint.websocket_url("/devtools/page/abc").unwrap(),
            "ws://127.0.0.1:9222/devtools/page/abc"
        );
        assert!(endpoint.websocket_url("ws://evil.example/page").is_err());
        let v6 = Endpoint::parse("[::1]:9222").unwrap();
        assert_eq!(
            v6.websocket_url("/devtools/page/abc").unwrap(),
            "ws://[::1]:9222/devtools/page/abc"
        );
    }

    #[test]
    fn address_parser_accepts_both_loopback_families() {
        assert!(
            Endpoint::parse(
                &SocketAddr::new(IpAddr::V4(std::net::Ipv4Addr::LOCALHOST), 1).to_string()
            )
            .is_ok()
        );
        assert!(
            Endpoint::parse(
                &SocketAddr::new(IpAddr::V6(std::net::Ipv6Addr::LOCALHOST), 1).to_string()
            )
            .is_ok()
        );
    }

    #[test]
    fn discovery_http_parses_json_and_rejects_status_or_oversize() {
        let chrome = crate::transport::test_support::ScriptedChrome::start();
        let endpoint = chrome.endpoint();
        let listed = endpoint
            .get_json("/json/list", Duration::from_secs(1))
            .unwrap();
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
