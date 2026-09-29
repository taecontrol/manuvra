use std::path::{Path, PathBuf};

#[cfg(target_os = "macos")]
#[path = "runtime/darwin.rs"]
mod platform;
#[cfg(target_os = "linux")]
#[path = "runtime/linux.rs"]
mod platform;

pub use platform::runtime_root;

const CONTROL_SOCKET: &str = "control.sock";

pub fn run_dir(run_id: &str) -> Result<PathBuf, String> {
    runtime_root().and_then(|root| run_dir_under(&root, run_id))
}

pub(crate) fn run_dir_under(root: &Path, run_id: &str) -> Result<PathBuf, String> {
    validate_run_id(run_id)?;
    let manuvra = root.join("manuvra");
    let runs = manuvra.join("runs");
    let directory = runs.join(run_id);
    platform::validate_socket_path(&directory.join(CONTROL_SOCKET))?;
    crate::store::create_private_dir(&manuvra)?;
    crate::store::create_private_dir(&runs)?;
    crate::store::create_private_dir(&directory)?;
    Ok(directory)
}

pub fn validate_socket_path(path: &Path) -> Result<(), String> {
    platform::validate_socket_path(path)
}

/// Removes the control socket of a host whose death the run lock has confirmed. Only a socket is
/// removed; any other entry at the recorded path is reported and left in place.
fn remove_dead_socket(path: &Path) -> Result<(), String> {
    use std::os::unix::fs::FileTypeExt;

    match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_socket() => std::fs::remove_file(path)
            .map_err(|error| format!("cannot remove dead host socket: {error}")),
        Ok(_) => Err("dead host socket path is not an owned socket".into()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(format!("cannot inspect dead host socket: {error}")),
    }
}

/// Removes a dead host's control socket and then its private run directory, which holds nothing
/// else in production. A directory that still has other entries is left in place.
pub fn sweep_dead_run(run_dir: &Path) -> Result<(), String> {
    remove_dead_socket(&run_dir.join(CONTROL_SOCKET))?;
    match std::fs::remove_dir(run_dir) {
        Ok(()) => Ok(()),
        Err(error)
            if matches!(
                error.kind(),
                std::io::ErrorKind::NotFound | std::io::ErrorKind::DirectoryNotEmpty
            ) =>
        {
            Ok(())
        }
        Err(error) => Err(format!("cannot remove dead run directory: {error}")),
    }
}

/// Sweeps the run directory named by a durably recorded control socket. A recorded path that
/// is not `<run_id>/control.sock` has only its socket removed.
pub fn sweep_recorded_run(socket: &Path, run_id: &str) -> Result<(), String> {
    let run_dir = socket
        .parent()
        .filter(|directory| directory.file_name() == Some(std::ffi::OsStr::new(run_id)))
        .filter(|_| socket.file_name() == Some(std::ffi::OsStr::new(CONTROL_SOCKET)));
    run_dir.map_or_else(|| remove_dead_socket(socket), sweep_dead_run)
}

fn validate_run_id(run_id: &str) -> Result<(), String> {
    let valid = run_id.len() == 18
        && run_id.starts_with("r_")
        && run_id[2..].bytes().all(|byte| byte.is_ascii_alphanumeric());
    valid
        .then_some(())
        .ok_or_else(|| "runtime Run id must be r_ plus 16 ASCII alphanumerics".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use std::os::unix::net::UnixListener;
    #[cfg(target_os = "macos")]
    use tempfile::TempDir;

    #[test]
    fn fixed_run_id_shape_is_required() {
        assert!(validate_run_id("r_1234567890abcdef").is_ok());
        for invalid in [
            "r_1234567890abcde",
            "r_1234567890abcdefg",
            "x_1234567890abcdef",
            "r_1234567890abcde-",
        ] {
            assert!(validate_run_id(invalid).is_err(), "{invalid}");
        }
    }

    #[test]
    fn private_fixed_id_directory_binds_a_control_socket() {
        let temporary = tempfile::Builder::new()
            .prefix("mrt")
            .tempdir_in("/tmp")
            .unwrap();
        let directory = run_dir_under(temporary.path(), "r_1234567890abcdef").unwrap();
        assert_eq!(
            std::fs::metadata(&directory).unwrap().permissions().mode() & 0o777,
            0o700
        );
        let socket = directory.join(CONTROL_SOCKET);
        let listener = UnixListener::bind(&socket).unwrap();
        drop(listener);
        assert_eq!(
            run_dir_under(temporary.path(), "r_1234567890abcdef").unwrap(),
            directory
        );
    }

    #[test]
    fn dead_socket_removal_is_bounded_to_sockets() {
        let temporary = tempfile::Builder::new()
            .prefix("mds")
            .tempdir_in("/tmp")
            .unwrap();
        let absent = temporary.path().join("absent.sock");
        assert!(remove_dead_socket(&absent).is_ok());

        let socket = temporary.path().join("owned.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        assert!(remove_dead_socket(&socket).is_ok());
        assert!(!socket.exists());
        drop(listener);

        let ordinary = temporary.path().join("ordinary");
        std::fs::write(&ordinary, b"not a socket").unwrap();
        assert!(remove_dead_socket(&ordinary).is_err());
        assert_eq!(std::fs::read(&ordinary).unwrap(), b"not a socket");
    }

    #[test]
    fn dead_run_sweep_removes_only_an_emptied_run_directory() {
        let temporary = tempfile::Builder::new()
            .prefix("msw")
            .tempdir_in("/tmp")
            .unwrap();
        let directory = run_dir_under(temporary.path(), "r_1234567890abcdef").unwrap();
        drop(UnixListener::bind(directory.join(CONTROL_SOCKET)).unwrap());
        sweep_dead_run(&directory).unwrap();
        assert!(!directory.exists());
        assert!(directory.parent().unwrap().is_dir());
        sweep_dead_run(&directory).unwrap();

        let retained = run_dir_under(temporary.path(), "r_abcdef1234567890").unwrap();
        std::fs::write(retained.join("unrelated"), b"kept").unwrap();
        sweep_dead_run(&retained).unwrap();
        assert_eq!(std::fs::read(retained.join("unrelated")).unwrap(), b"kept");
    }

    #[test]
    fn recorded_socket_sweeps_only_its_own_run_directory() {
        let temporary = tempfile::Builder::new()
            .prefix("msr")
            .tempdir_in("/tmp")
            .unwrap();
        let run_id = "r_1234567890abcdef";
        let directory = run_dir_under(temporary.path(), run_id).unwrap();
        let socket = directory.join(CONTROL_SOCKET);
        drop(UnixListener::bind(&socket).unwrap());
        sweep_recorded_run(&socket, "r_other0000000000").unwrap();
        assert!(!socket.exists());
        assert!(
            directory.is_dir(),
            "another run's directory is never removed"
        );

        drop(UnixListener::bind(&socket).unwrap());
        sweep_recorded_run(&socket, run_id).unwrap();
        assert!(!directory.exists());
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn over_bound_socket_is_rejected_before_runtime_creation() {
        let temporary = TempDir::new().unwrap();
        let long_root = temporary.path().join("x".repeat(104));
        assert!(run_dir_under(&long_root, "r_1234567890abcdef").is_err());
        assert!(!long_root.exists());
    }
}
