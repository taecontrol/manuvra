use super::{BrowserError, safe_error};
use std::env;
use std::ffi::{OsStr, OsString};
use std::fs;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::thread;
use std::time::{Duration, Instant};

pub(super) struct BrowserOwnership {
    owns_process_group: bool,
}

pub(super) fn spawn(
    mut command: Command,
    inherit_process_group: bool,
) -> Result<(Child, BrowserOwnership), String> {
    if !inherit_process_group {
        command.process_group(0);
    }
    unsafe {
        command.pre_exec(|| {
            if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL) == -1 {
                return Err(std::io::Error::last_os_error());
            }
            if libc::getppid() == 1 {
                return Err(std::io::Error::other("parent exited before Chromium spawn"));
            }
            Ok(())
        });
    }
    let child = command.spawn().map_err(|error| error.to_string())?;
    Ok((
        child,
        BrowserOwnership {
            owns_process_group: !inherit_process_group,
        },
    ))
}

pub(super) fn discover_binary(
    explicit: Option<&Path>,
    environment: Option<PathBuf>,
    search_path: Option<OsString>,
) -> Result<PathBuf, BrowserError> {
    discover_binary_from(explicit, environment, search_path)
}

pub(super) fn discover_binary_from(
    explicit: Option<&Path>,
    environment: Option<PathBuf>,
    search_path: Option<OsString>,
) -> Result<PathBuf, BrowserError> {
    let configured = explicit.map(Path::to_path_buf).or(environment);
    if let Some(path) = configured {
        return usable_binary(&path).ok_or(BrowserError::Unavailable);
    }
    for name in [
        "chromium",
        "chromium-browser",
        "google-chrome",
        "google-chrome-stable",
    ] {
        if let Some(path) = find_on_path(name, search_path.as_deref()) {
            return Ok(path);
        }
    }
    for path in [
        Path::new("/usr/bin/chromium"),
        Path::new("/usr/bin/google-chrome"),
    ] {
        if let Some(path) = usable_binary(path) {
            return Ok(path);
        }
    }
    Err(BrowserError::Unavailable)
}

fn find_on_path(name: &str, search_path: Option<&OsStr>) -> Option<PathBuf> {
    env::split_paths(search_path?).find_map(|directory| usable_binary(&directory.join(name)))
}

fn usable_binary(path: &Path) -> Option<PathBuf> {
    let metadata = path.metadata().ok()?;
    if !metadata.is_file() {
        return None;
    }
    use std::os::unix::fs::PermissionsExt;
    (metadata.permissions().mode() & 0o111 != 0)
        .then(|| fs::canonicalize(path).ok())
        .flatten()
}

pub(super) fn terminate(child: &mut Child, ownership: &mut BrowserOwnership) -> Result<(), String> {
    if !ownership.owns_process_group {
        return terminate_process(child);
    }
    let process_group = child_pid(child)?;
    signal_process_group(process_group, libc::SIGTERM)?;
    if wait_for_process_group_exit(child, process_group, Duration::from_secs(2)) {
        return Ok(());
    }
    signal_process_group(process_group, libc::SIGKILL)?;
    let _ = child.wait();
    wait_for_process_group_exit(child, process_group, Duration::from_secs(1))
        .then_some(())
        .ok_or_else(|| "Chromium process group remained alive after SIGKILL".into())
}

fn terminate_process(child: &mut Child) -> Result<(), String> {
    signal_process(child_pid(child)?, libc::SIGTERM)?;
    if wait_for_process_exit(child, Duration::from_secs(2))? {
        return Ok(());
    }
    child
        .kill()
        .map_err(|error| safe_error(&error.to_string()))?;
    child
        .wait()
        .map(|_| ())
        .map_err(|error| safe_error(&error.to_string()))
}

fn child_pid(child: &Child) -> Result<i32, String> {
    child
        .id()
        .try_into()
        .map_err(|_| "Chromium process id does not fit pid_t".to_owned())
}

fn signal_process(pid: i32, signal: i32) -> Result<(), String> {
    let result = unsafe { libc::kill(pid, signal) };
    if result == -1 && std::io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH) {
        Err(safe_error(&std::io::Error::last_os_error().to_string()))
    } else {
        Ok(())
    }
}

fn wait_for_process_exit(child: &mut Child, timeout: Duration) -> Result<bool, String> {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if child
            .try_wait()
            .map_err(|error| safe_error(&error.to_string()))?
            .is_some()
        {
            return Ok(true);
        }
        thread::sleep(Duration::from_millis(20));
    }
    Ok(false)
}

fn wait_for_process_group_exit(child: &mut Child, process_group: i32, timeout: Duration) -> bool {
    let end = Instant::now() + timeout;
    while Instant::now() < end {
        let _ = child.try_wait();
        if !process_group_exists(process_group) {
            let _ = child.wait();
            return true;
        }
        thread::sleep(Duration::from_millis(20));
    }
    false
}

fn signal_process_group(process_group: i32, signal: i32) -> Result<(), String> {
    let result = unsafe { libc::kill(-process_group, signal) };
    if result == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH) {
        Ok(())
    } else {
        Err(safe_error(&std::io::Error::last_os_error().to_string()))
    }
}

fn process_group_exists(process_group: i32) -> bool {
    let result = unsafe { libc::kill(-process_group, 0) };
    result == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

#[cfg(test)]
pub(super) fn ownership_for_test(_child: &Child, owns_process_group: bool) -> BrowserOwnership {
    BrowserOwnership { owns_process_group }
}
