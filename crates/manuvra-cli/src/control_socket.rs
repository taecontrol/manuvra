use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::fs::{FileTypeExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;
use std::time::{Duration, Instant};

use serde::Serialize;
use serde::de::DeserializeOwned;

const MAX_CONTROL_FRAME_BYTES: usize = 32 * 1024 * 1024;
const CONTROL_READ_TIMEOUT: Duration = Duration::from_millis(250);

pub struct AuthenticatedListener(UnixListener);

impl AuthenticatedListener {
    pub fn bind(path: &Path) -> Result<Self, String> {
        crate::runtime::validate_socket_path(path)?;
        crate::process::ensure_socket_parent(path)?;
        remove_prior_socket(path)?;
        let listener = UnixListener::bind(path).map_err(|error| error.to_string())?;
        if let Err(error) = fs::set_permissions(path, fs::Permissions::from_mode(0o600)) {
            let _ = fs::remove_file(path);
            return Err(error.to_string());
        }
        Ok(Self(listener))
    }

    pub fn accept(&self) -> Result<UnixStream, String> {
        let (stream, _) = self.0.accept().map_err(|error| error.to_string())?;
        crate::socket_auth::authenticate_current_user(&stream)?;
        Ok(stream)
    }

    pub fn set_nonblocking(&self) -> Result<(), String> {
        self.0
            .set_nonblocking(true)
            .map_err(|error| error.to_string())
    }

    pub fn try_accept(&self) -> Result<Option<UnixStream>, String> {
        match self.0.accept() {
            Ok((stream, _)) => {
                crate::socket_auth::authenticate_current_user(&stream).map(|()| Some(stream))
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => Ok(None),
            Err(error) => Err(error.to_string()),
        }
    }
}

pub fn write_frame<T: Serialize>(stream: &mut UnixStream, value: &T) -> Result<(), String> {
    serde_json::to_writer(&mut *stream, value).map_err(|error| error.to_string())?;
    stream.write_all(b"\n").map_err(|error| error.to_string())?;
    stream.flush().map_err(|error| error.to_string())
}

pub fn read_frame<T: DeserializeOwned>(stream: &mut UnixStream) -> Result<T, String> {
    let deadline = Instant::now()
        .checked_add(CONTROL_READ_TIMEOUT)
        .ok_or_else(|| "control frame read deadline is invalid".to_owned())?;
    let reader = DeadlineReader { stream, deadline };
    let mut reader = BufReader::new(reader);
    let bytes = read_bounded_frame(&mut reader, MAX_CONTROL_FRAME_BYTES)?;
    serde_json::from_slice(&bytes).map_err(|error| error.to_string())
}

struct DeadlineReader<'a> {
    stream: &'a mut UnixStream,
    deadline: Instant,
}

impl Read for DeadlineReader<'_> {
    fn read(&mut self, bytes: &mut [u8]) -> std::io::Result<usize> {
        let remaining = self
            .deadline
            .checked_duration_since(Instant::now())
            .ok_or_else(|| {
                std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    "control frame read deadline elapsed",
                )
            })?;
        self.stream.set_read_timeout(Some(remaining))?;
        self.stream.read(bytes).map_err(|error| {
            if matches!(
                error.kind(),
                std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
            ) {
                std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    "control frame read deadline elapsed",
                )
            } else {
                error
            }
        })
    }
}

fn read_bounded_frame(
    reader: &mut BufReader<impl Read>,
    maximum_bytes: usize,
) -> Result<Vec<u8>, String> {
    let mut bytes = Vec::new();
    reader
        .by_ref()
        .take(maximum_bytes as u64)
        .read_until(b'\n', &mut bytes)
        .map_err(|error| error.to_string())?;
    match bytes.last() {
        Some(b'\n') => bytes.pop(),
        _ if bytes.len() >= maximum_bytes => {
            return Err("control frame exceeds the safety bound".into());
        }
        _ => return Err("control frame ended before its newline delimiter".into()),
    };
    if reader
        .buffer()
        .iter()
        .any(|byte| !byte.is_ascii_whitespace())
    {
        return Err("control connection contains data after its first frame".into());
    }
    Ok(bytes)
}

fn remove_prior_socket(path: &Path) -> Result<(), String> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_socket() => {
            fs::remove_file(path).map_err(|error| error.to_string())
        }
        Ok(_) => Err("control socket path is occupied by an unsafe entry".into()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;
    use std::io::{Read, Write};
    use std::net::Shutdown;
    use std::os::unix::fs::symlink;
    #[cfg(target_os = "macos")]
    use std::process::{Command, Stdio};
    use tempfile::TempDir;

    #[cfg(target_os = "macos")]
    const RUNTIME_CLIENT_FIXTURE: &str = "MANUVRA_TEST_RUNTIME_CLIENT";
    #[cfg(target_os = "macos")]
    const FIXED_RUN_ID: &str = "r_1234567890abcdef";

    #[test]
    fn private_listener_accepts_a_live_same_user_before_reading_its_frame() {
        let temporary = tempfile::Builder::new()
            .prefix("mcs")
            .tempdir_in("/tmp")
            .unwrap();
        let socket = temporary.path().join("control.sock");
        let listener = AuthenticatedListener::bind(&socket).unwrap();
        assert_eq!(
            fs::metadata(&socket).unwrap().permissions().mode() & 0o777,
            0o600
        );
        let client_socket = socket.clone();
        let client = std::thread::spawn(move || {
            let mut stream = UnixStream::connect(client_socket).unwrap();
            stream.write_all(b"frame").unwrap();
        });
        let mut stream = listener.accept().unwrap();
        let mut bytes = Vec::new();
        stream.read_to_end(&mut bytes).unwrap();
        client.join().unwrap();
        assert_eq!(bytes, b"frame");
    }

    #[test]
    fn bind_rejects_file_and_symlink_substitution() {
        let temporary = TempDir::new().unwrap();
        let outside = temporary.path().join("outside");
        fs::write(&outside, b"unchanged").unwrap();
        assert!(AuthenticatedListener::bind(&outside).is_err());
        let socket = temporary.path().join("control.sock");
        symlink(&outside, &socket).unwrap();
        assert!(AuthenticatedListener::bind(&socket).is_err());
        assert_eq!(fs::read(outside).unwrap(), b"unchanged");
    }

    #[test]
    fn bounded_frame_accepts_the_exact_limit_and_rejects_one_byte_more() {
        let exact = b"\"12345\"\n";
        let mut reader = BufReader::new(Cursor::new(exact));
        assert_eq!(read_bounded_frame(&mut reader, 8).unwrap(), b"\"12345\"");

        let oversized = b"\"123456\"\n";
        let mut reader = BufReader::new(Cursor::new(oversized));
        assert_eq!(
            read_bounded_frame(&mut reader, 8).unwrap_err(),
            "control frame exceeds the safety bound"
        );
    }

    #[test]
    fn bounded_frame_requires_a_newline_and_rejects_buffered_second_frames() {
        let mut missing = BufReader::new(Cursor::new(b"{}"));
        assert_eq!(
            read_bounded_frame(&mut missing, 8).unwrap_err(),
            "control frame ended before its newline delimiter"
        );

        let mut second = BufReader::new(Cursor::new(b"{}\n{}\n"));
        assert_eq!(
            read_bounded_frame(&mut second, 8).unwrap_err(),
            "control connection contains data after its first frame"
        );

        let mut whitespace = BufReader::new(Cursor::new(b"{}\n \t\r\n"));
        assert_eq!(read_bounded_frame(&mut whitespace, 8).unwrap(), b"{}");
    }

    #[test]
    fn frame_deadline_rejects_a_slow_incomplete_peer_without_waiting_for_eof() {
        let (mut client, mut server) = UnixStream::pair().unwrap();
        client.write_all(b"{").unwrap();
        let trickle = std::thread::spawn(move || {
            for _ in 0..20 {
                std::thread::sleep(Duration::from_millis(40));
                if client.write_all(b" ").is_err() {
                    break;
                }
            }
        });
        let started = Instant::now();
        let error = read_frame::<serde_json::Value>(&mut server).unwrap_err();
        let elapsed = started.elapsed();
        drop(server);
        trickle.join().unwrap();
        assert!(elapsed >= Duration::from_millis(200));
        assert!(elapsed < Duration::from_secs(1));
        assert!(error.contains("control frame read deadline elapsed"));
    }

    #[test]
    fn complete_frame_does_not_require_the_client_to_half_close() {
        let (mut client, mut server) = UnixStream::pair().unwrap();
        client.write_all(b"{}\n").unwrap();
        let value: serde_json::Value = read_frame(&mut server).unwrap();
        assert_eq!(value, serde_json::json!({}));
        client.shutdown(Shutdown::Both).unwrap();
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn subsequent_process_discovers_and_authenticates_xdg_and_temporary_runtime_sockets() {
        if std::env::var_os(RUNTIME_CLIENT_FIXTURE).is_some() {
            let socket = crate::runtime::run_dir(FIXED_RUN_ID)
                .unwrap()
                .join("control.sock");
            let mut stream = UnixStream::connect(socket).unwrap();
            stream.write_all(b"cross-process frame").unwrap();
            return;
        }

        let temporary = tempfile::Builder::new()
            .prefix("mcp")
            .tempdir_in("/tmp")
            .unwrap();
        let xdg = temporary.path().join("xdg");
        let ignored_temporary = temporary.path().join("ignored");
        assert_runtime_client(&xdg, Some(&xdg), Some(&ignored_temporary));

        let fallback = temporary.path().join("temporary");
        assert_runtime_client(&fallback, None, Some(&fallback));
    }

    #[cfg(target_os = "macos")]
    fn assert_runtime_client(root: &Path, xdg: Option<&Path>, temporary: Option<&Path>) {
        let directory = crate::runtime::run_dir_under(root, FIXED_RUN_ID).unwrap();
        let socket = directory.join("control.sock");
        let listener = AuthenticatedListener::bind(&socket).unwrap();
        let mut command = Command::new(std::env::current_exe().unwrap());
        command
            .args([
                "--exact",
                "control_socket::tests::subsequent_process_discovers_and_authenticates_xdg_and_temporary_runtime_sockets",
                "--nocapture",
            ])
            .env(RUNTIME_CLIENT_FIXTURE, "1")
            .env_remove("XDG_RUNTIME_DIR")
            .env_remove("TMPDIR")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        if let Some(xdg) = xdg {
            command.env("XDG_RUNTIME_DIR", xdg);
        }
        if let Some(temporary) = temporary {
            command.env("TMPDIR", temporary);
        }
        let mut child = command.spawn().unwrap();
        let mut stream = listener.accept().unwrap();
        let mut bytes = Vec::new();
        stream.read_to_end(&mut bytes).unwrap();
        assert_eq!(bytes, b"cross-process frame");
        assert!(child.wait().unwrap().success());
        assert_eq!(
            fs::metadata(directory).unwrap().permissions().mode() & 0o777,
            0o700
        );
    }
}
