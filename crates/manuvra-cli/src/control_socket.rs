use std::fs;
use std::os::unix::fs::{FileTypeExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;

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
    use std::io::{Read, Write};
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
