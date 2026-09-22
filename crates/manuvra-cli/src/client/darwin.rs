use crate::store::RunControl;
use serde_json::{Value, json};
use std::io::{Read, Write};
use std::net::Shutdown;
use std::os::unix::fs::FileTypeExt;
use std::os::unix::net::UnixStream;
use std::time::Duration;

pub fn request_deadline(control: &RunControl, ipc_version: u16) -> Result<(), String> {
    let response = request_value(
        &control.socket,
        &json!({
            "kind":"deadline",
            "ipc_version":ipc_version,
            "run_id":control.run_id,
            "job_digest":control.job_digest,
        }),
    )?;
    let accepted = response.get("accepted").and_then(Value::as_bool) == Some(true);
    let identity_matches = response.get("run_id").and_then(Value::as_str)
        == Some(control.run_id.as_str())
        && response.get("job_digest").and_then(Value::as_str) == Some(control.job_digest.as_str())
        && response.get("ipc_version").and_then(Value::as_u64) == Some(u64::from(ipc_version));
    (accepted && identity_matches)
        .then_some(())
        .ok_or_else(|| "run host rejected or mismatched the deadline request".into())
}

fn request_value(socket: &std::path::Path, payload: &Value) -> Result<Value, String> {
    validate_socket(socket)?;
    let mut stream = UnixStream::connect(socket).map_err(|error| error.to_string())?;
    stream
        .set_read_timeout(Some(Duration::from_millis(250)))
        .and_then(|()| stream.set_write_timeout(Some(Duration::from_millis(250))))
        .map_err(|error| error.to_string())?;
    serde_json::to_writer(&mut stream, payload).map_err(|error| error.to_string())?;
    stream.flush().map_err(|error| error.to_string())?;
    stream
        .shutdown(Shutdown::Write)
        .map_err(|error| error.to_string())?;
    let mut bytes = Vec::new();
    stream
        .read_to_end(&mut bytes)
        .map_err(|error| error.to_string())?;
    serde_json::from_slice(&bytes).map_err(|error| error.to_string())
}

fn validate_socket(socket: &std::path::Path) -> Result<(), String> {
    crate::runtime::validate_socket_path(socket)?;
    let metadata = std::fs::symlink_metadata(socket)
        .map_err(|error| format!("run host socket is unavailable: {error}"))?;
    (!metadata.file_type().is_symlink() && metadata.file_type().is_socket())
        .then_some(())
        .ok_or_else(|| "run host socket is not a private Unix socket".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use manuvra_contract::SchemaVersion;
    use std::os::unix::fs::symlink;
    use std::os::unix::net::UnixListener;
    use tempfile::TempDir;

    #[test]
    fn rejects_regular_file_and_symlink_substitution() {
        let temporary = TempDir::new().unwrap();
        let regular = temporary.path().join("regular");
        std::fs::write(&regular, b"unchanged").unwrap();
        assert!(validate_socket(&regular).is_err());
        let link = temporary.path().join("link");
        symlink(&regular, &link).unwrap();
        assert!(validate_socket(&link).is_err());
        assert_eq!(std::fs::read(regular).unwrap(), b"unchanged");
    }

    #[test]
    fn deadline_request_is_bounded_and_requires_matching_identity() {
        let temporary = tempfile::Builder::new()
            .prefix("mcd")
            .tempdir_in("/tmp")
            .unwrap();
        let socket = temporary.path().join("control.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = Vec::new();
            stream.read_to_end(&mut request).unwrap();
            let request: Value = serde_json::from_slice(&request).unwrap();
            assert_eq!(request["kind"], "deadline");
            serde_json::to_writer(
                &mut stream,
                &json!({
                    "ipc_version":7,
                    "run_id":"r_1234567890abcdef",
                    "job_digest":"digest",
                    "accepted":true
                }),
            )
            .unwrap();
        });
        let control = RunControl {
            schema_version: SchemaVersion,
            ipc_version: 7,
            sequence: 1,
            run_id: "r_1234567890abcdef".into(),
            request_id: "request".into(),
            job_digest: "digest".into(),
            evidence_root: temporary.path().join("evidence"),
            started_unix_ms: 0,
            lifetime_deadline_unix_ms: u64::MAX,
            pause_deadline_unix_ms: None,
            host: None,
            watchdog: None,
            socket,
            result: json!({"state":"running","terminal":false}),
        };
        request_deadline(&control, 7).unwrap();
        server.join().unwrap();
    }
}
