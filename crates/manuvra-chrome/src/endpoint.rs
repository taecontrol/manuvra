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
        let value = value.unwrap_or(DEFAULT_ENDPOINT);
        let mut endpoints = Vec::new();
        for item in value.split(',') {
            if item.trim() != item || item.is_empty() {
                return Err(EndpointError::Invalid(item.to_owned()));
            }
            let endpoint = Self::parse(item)?;
            if !endpoints.contains(&endpoint) {
                endpoints.push(endpoint);
            }
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
        let mut stream = TcpStream::connect_timeout(&self.address, timeout)
            .map_err(|error| EndpointError::Io(error.to_string()))?;
        stream
            .set_read_timeout(Some(timeout))
            .and_then(|()| stream.set_write_timeout(Some(timeout)))
            .map_err(|error| EndpointError::Io(error.to_string()))?;
        let host = self.label();
        write!(
            stream,
            "GET {path} HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\nAccept: application/json\r\n\r\n"
        )
        .map_err(|error| EndpointError::Io(error.to_string()))?;
        let response = read_response(&mut stream)?;
        parse_http_json(&response)
    }
}

fn read_response(stream: &mut TcpStream) -> Result<Vec<u8>, EndpointError> {
    let mut response = Vec::new();
    let mut chunk = [0_u8; 8192];
    loop {
        match stream.read(&mut chunk) {
            Ok(0) => break,
            Ok(count) => {
                response.extend_from_slice(&chunk[..count]);
                if response.len() > MAX_DISCOVERY_BYTES {
                    return Err(EndpointError::TooLarge);
                }
                if response_complete(&response) {
                    break;
                }
            }
            Err(error)
                if !response.is_empty()
                    && matches!(
                        error.kind(),
                        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                    ) =>
            {
                break;
            }
            Err(error) => return Err(EndpointError::Io(error.to_string())),
        }
    }
    Ok(response)
}

fn response_complete(response: &[u8]) -> bool {
    let Some(boundary) = response.windows(4).position(|window| window == b"\r\n\r\n") else {
        return false;
    };
    let Ok(head) = std::str::from_utf8(&response[..boundary]) else {
        return false;
    };
    head.lines()
        .filter_map(|line| line.split_once(':'))
        .find(|(name, _)| name.eq_ignore_ascii_case("content-length"))
        .and_then(|(_, value)| value.trim().parse::<usize>().ok())
        .is_some_and(|length| response.len() >= boundary + 4 + length)
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
    }

    #[test]
    fn websocket_url_reuses_only_the_validated_endpoint_authority() {
        let endpoint = Endpoint::parse("127.0.0.1:9222").unwrap();
        assert_eq!(
            endpoint.websocket_url("/devtools/page/abc").unwrap(),
            "ws://127.0.0.1:9222/devtools/page/abc"
        );
        assert!(endpoint.websocket_url("ws://evil.example/page").is_err());
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
}
