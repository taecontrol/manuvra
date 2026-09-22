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

    #[cfg(target_os = "macos")]
    #[test]
    fn over_bound_socket_is_rejected_before_runtime_creation() {
        let temporary = TempDir::new().unwrap();
        let long_root = temporary.path().join("x".repeat(104));
        assert!(run_dir_under(&long_root, "r_1234567890abcdef").is_err());
        assert!(!long_root.exists());
    }
}
