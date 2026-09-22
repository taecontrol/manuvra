use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

const SUN_PATH_CAPACITY: usize = 104;

pub fn runtime_root() -> Result<PathBuf, String> {
    runtime_root_from(
        std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from),
        std::env::var_os("TMPDIR").map(PathBuf::from),
    )
}

fn runtime_root_from(xdg: Option<PathBuf>, temporary: Option<PathBuf>) -> Result<PathBuf, String> {
    xdg.filter(|path| !path.as_os_str().is_empty())
        .or_else(|| temporary.filter(|path| !path.as_os_str().is_empty()))
        .ok_or_else(|| "runtime_directory_unavailable".into())
}

pub fn validate_socket_path(path: &Path) -> Result<(), String> {
    let bytes = path.as_os_str().as_bytes();
    if bytes.contains(&0) {
        return Err("control socket path contains a NUL byte".into());
    }
    let encoded_with_nul = bytes
        .len()
        .checked_add(1)
        .ok_or_else(|| "control socket path length overflowed".to_owned())?;
    (encoded_with_nul <= SUN_PATH_CAPACITY)
        .then_some(())
        .ok_or_else(|| {
            format!(
                "control socket path needs {encoded_with_nul} bytes including NUL; Darwin permits {SUN_PATH_CAPACITY}"
            )
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsStr;

    #[test]
    fn darwin_socket_limit_counts_the_terminating_nul() {
        assert!(validate_socket_path(Path::new(&"a".repeat(103))).is_ok());
        assert!(validate_socket_path(Path::new(&"a".repeat(104))).is_err());
        assert!(validate_socket_path(Path::new(OsStr::from_bytes(b"bad\0path"))).is_err());
    }

    #[test]
    fn xdg_precedes_explicit_temporary_directory_and_absence_refuses() {
        assert_eq!(
            runtime_root_from(Some("/xdg".into()), Some("/temporary".into())).unwrap(),
            PathBuf::from("/xdg")
        );
        assert_eq!(
            runtime_root_from(None, Some("/temporary".into())).unwrap(),
            PathBuf::from("/temporary")
        );
        assert_eq!(
            runtime_root_from(Some(PathBuf::new()), Some("/temporary".into())).unwrap(),
            PathBuf::from("/temporary")
        );
        assert_eq!(
            runtime_root_from(None, None).unwrap_err(),
            "runtime_directory_unavailable"
        );
    }
}
