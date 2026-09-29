use std::path::{Path, PathBuf};

pub fn runtime_root() -> Result<PathBuf, String> {
    std::env::var_os("XDG_RUNTIME_DIR")
        .filter(|path| !path.is_empty())
        .map(PathBuf::from)
        .ok_or_else(|| "XDG_RUNTIME_DIR is not set".into())
}

pub fn validate_socket_path(_: &Path) -> Result<(), String> {
    Ok(())
}
