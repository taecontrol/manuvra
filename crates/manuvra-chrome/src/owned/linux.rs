use super::process::{
    FORCED_TIMEOUT, GRACEFUL_TIMEOUT, find_on_path, pid_t, signal_group, signal_pid, usable_binary,
    wait_for_process_exit,
};
use super::{BrowserError, safe_error};
use std::ffi::OsString;
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

pub(super) fn terminate(child: &mut Child, ownership: &mut BrowserOwnership) -> Result<(), String> {
    if !ownership.owns_process_group {
        return terminate_process(child);
    }
    // A group id stays reserved while any member lives, even after its leader is reaped.
    let process_group = child.id();
    signal_group(process_group, libc::SIGTERM)?;
    if wait_for_process_group_exit(child, process_group, GRACEFUL_TIMEOUT)? {
        return Ok(());
    }
    signal_group(process_group, libc::SIGKILL)?;
    let _ = child.wait();
    wait_for_process_group_exit(child, process_group, FORCED_TIMEOUT)?
        .then_some(())
        .ok_or_else(|| "Chromium process group remained alive after SIGKILL".into())
}

fn terminate_process(child: &mut Child) -> Result<(), String> {
    // Once reaped, the pid may belong to an unrelated process; until then it stays ours.
    if child
        .try_wait()
        .map_err(|error| safe_error(&error.to_string()))?
        .is_some()
    {
        return Ok(());
    }
    signal_pid(child.id(), libc::SIGTERM)?;
    if wait_for_process_exit(child, GRACEFUL_TIMEOUT)? {
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

fn wait_for_process_group_exit(
    child: &mut Child,
    process_group: u32,
    timeout: Duration,
) -> Result<bool, String> {
    let end = Instant::now() + timeout;
    while Instant::now() < end {
        let _ = child.try_wait();
        if !process_group_exists(process_group)? {
            let _ = child.wait();
            return Ok(true);
        }
        thread::sleep(Duration::from_millis(20));
    }
    Ok(false)
}

fn process_group_exists(process_group: u32) -> Result<bool, String> {
    let result = unsafe { libc::kill(-pid_t(process_group)?, 0) };
    Ok(result == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM))
}

#[cfg(test)]
pub(super) fn ownership_for_test(_child: &Child, owns_process_group: bool) -> BrowserOwnership {
    BrowserOwnership { owns_process_group }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::io::{BufRead, BufReader};
    use std::os::unix::fs::PermissionsExt;
    use std::os::unix::process::ExitStatusExt;
    use std::process::Stdio;

    fn executable(path: &Path, mode: u32) {
        fs::write(path, b"").unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(mode)).unwrap();
    }

    #[test]
    fn discovery_prefers_explicit_then_environment_then_path() {
        let temporary = tempfile::tempdir().unwrap();
        let explicit = temporary.path().join("explicit");
        let environment = temporary.path().join("environment");
        let path_dir = temporary.path().join("bin");
        fs::create_dir(&path_dir).unwrap();
        let path_binary = path_dir.join("chromium");
        for binary in [&explicit, &environment, &path_binary] {
            executable(binary, 0o700);
        }
        let search = || Some(path_dir.clone().into_os_string());
        assert_eq!(
            discover_binary(Some(&explicit), Some(environment.clone()), search()).unwrap(),
            fs::canonicalize(&explicit).unwrap()
        );
        assert_eq!(
            discover_binary(None, Some(environment.clone()), search()).unwrap(),
            fs::canonicalize(&environment).unwrap()
        );
        assert_eq!(
            discover_binary(None, None, search()).unwrap(),
            fs::canonicalize(&path_binary).unwrap()
        );
    }

    #[test]
    fn a_configured_non_executable_file_never_falls_back_to_path() {
        let temporary = tempfile::tempdir().unwrap();
        let configured = temporary.path().join("chromium");
        let path_dir = temporary.path().join("bin");
        fs::create_dir(&path_dir).unwrap();
        executable(&configured, 0o600);
        executable(&path_dir.join("chromium"), 0o700);
        let search = || Some(path_dir.clone().into_os_string());
        assert!(matches!(
            discover_binary(Some(&configured), None, search()),
            Err(BrowserError::Unavailable)
        ));
        assert!(matches!(
            discover_binary(None, Some(configured.clone()), search()),
            Err(BrowserError::Unavailable)
        ));
    }

    fn spawn_reporting(
        script: &str,
        inherit_process_group: bool,
    ) -> (Child, BrowserOwnership, i32) {
        let mut command = Command::new("/bin/sh");
        command.args(["-c", script]).stdout(Stdio::piped());
        let (mut child, ownership) = spawn(command, inherit_process_group).unwrap();
        let mut line = String::new();
        BufReader::new(child.stdout.take().unwrap())
            .read_line(&mut line)
            .unwrap();
        (child, ownership, line.trim().parse().unwrap())
    }

    fn assert_gone(pid: i32) {
        assert_eq!(unsafe { libc::kill(pid, 0) }, -1, "process {pid} survived");
        assert_eq!(
            std::io::Error::last_os_error().raw_os_error(),
            Some(libc::ESRCH)
        );
    }

    #[test]
    fn owned_group_term_resistance_escalates_to_group_sigkill() {
        // The descendant inherits the ignored SIGTERM at fork, so it resists before its PID is read.
        let (mut child, mut ownership, descendant) =
            spawn_reporting("trap '' TERM; sleep 30 & echo $!; wait", false);
        let started = Instant::now();
        terminate(&mut child, &mut ownership).unwrap();
        assert!(started.elapsed() >= GRACEFUL_TIMEOUT);
        assert_eq!(child.wait().unwrap().signal(), Some(libc::SIGKILL));
        assert_gone(descendant);
    }

    #[test]
    fn owned_group_is_cleaned_after_its_leader_was_already_reaped() {
        let (mut child, mut ownership, descendant) =
            spawn_reporting("trap '' TERM; sleep 30 & echo $!; exit 0", false);
        assert!(child.wait().unwrap().success());
        let started = Instant::now();
        terminate(&mut child, &mut ownership).unwrap();
        assert!(started.elapsed() >= GRACEFUL_TIMEOUT);
        assert_gone(descendant);
    }

    #[test]
    fn inherited_group_term_resistance_kills_only_the_exact_child() {
        let (mut child, mut ownership, pid) =
            spawn_reporting("trap '' TERM; echo $$; exec sleep 30", true);
        assert_eq!(u32::try_from(pid).unwrap(), child.id());
        let started = Instant::now();
        terminate(&mut child, &mut ownership).unwrap();
        assert!(started.elapsed() >= GRACEFUL_TIMEOUT);
        assert_eq!(child.wait().unwrap().signal(), Some(libc::SIGKILL));
        assert_gone(pid);
    }

    #[test]
    fn inherited_group_child_that_honours_sigterm_is_reaped_without_escalation() {
        let (mut child, mut ownership, _) = spawn_reporting("echo $$; exec sleep 30", true);
        let started = Instant::now();
        terminate(&mut child, &mut ownership).unwrap();
        assert!(started.elapsed() < GRACEFUL_TIMEOUT);
        assert_eq!(child.wait().unwrap().signal(), Some(libc::SIGTERM));
    }
}
