use rand::Rng;
use rand::distr::Alphanumeric;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

pub fn opaque_id(prefix: &str) -> String {
    let suffix: String = rand::rng()
        .sample_iter(Alphanumeric)
        .take(20)
        .map(char::from)
        .collect();
    format!("{prefix}{suffix}")
}

pub fn ensure_private_directory(path: &Path) -> io::Result<()> {
    if path.exists() {
        let metadata = fs::symlink_metadata(path)?;
        if !metadata.file_type().is_dir() || metadata.file_type().is_symlink() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "private root is not a real directory",
            ));
        }
        if metadata.uid() != unsafe { libc::geteuid() } {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "private root belongs to a different user",
            ));
        }
    } else {
        fs::create_dir_all(path)?;
    }
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    Ok(())
}

pub fn atomic_write(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "path has no parent"))?;
    ensure_private_directory(parent)?;
    let temporary = temporary_sibling(path);
    let mut file = OpenOptions::new()
        .create_new(true)
        .write(true)
        .mode(0o600)
        .open(&temporary)?;
    let result = write_sync_rename(&mut file, bytes, &temporary, path, parent);
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

fn write_sync_rename(
    file: &mut File,
    bytes: &[u8],
    temporary: &Path,
    destination: &Path,
    parent: &Path,
) -> io::Result<()> {
    file.write_all(bytes)?;
    file.sync_all()?;
    fs::rename(temporary, destination)?;
    File::open(parent)?.sync_all()?;
    Ok(())
}

fn temporary_sibling(path: &Path) -> PathBuf {
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("state");
    path.with_file_name(format!(".{name}.{}.partial", opaque_id("")))
}

pub fn unix_mode(path: &Path) -> io::Result<u32> {
    Ok(fs::symlink_metadata(path)?.permissions().mode() & 0o777)
}

pub fn rfc3339_after(duration: Duration) -> String {
    let time = SystemTime::now()
        .checked_add(duration)
        .unwrap_or(SystemTime::now());
    rfc3339(time)
}

pub fn rfc3339(time: SystemTime) -> String {
    let seconds = time
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let raw = seconds as libc::time_t;
    let mut output = [0_i8; 32];
    let format = b"%Y-%m-%dT%H:%M:%SZ\0";
    let written = unsafe {
        let mut broken_down = std::mem::zeroed::<libc::tm>();
        if libc::gmtime_r(&raw, &mut broken_down).is_null() {
            return "1970-01-01T00:00:00Z".to_owned();
        }
        libc::strftime(
            output.as_mut_ptr(),
            output.len(),
            format.as_ptr().cast(),
            &broken_down,
        )
    };
    if written == 0 {
        return "1970-01-01T00:00:00Z".to_owned();
    }
    let bytes = output[..written]
        .iter()
        .map(|byte| *byte as u8)
        .collect::<Vec<_>>();
    String::from_utf8(bytes).unwrap_or_else(|_| "1970-01-01T00:00:00Z".to_owned())
}
