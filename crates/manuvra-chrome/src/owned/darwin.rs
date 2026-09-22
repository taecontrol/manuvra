use super::{BrowserError, safe_error};
use std::env;
use std::ffi::{OsStr, OsString};
use std::fs;
use std::mem::{MaybeUninit, size_of};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

const GRACEFUL_TIMEOUT: Duration = Duration::from_secs(2);
const FORCED_TIMEOUT: Duration = Duration::from_secs(1);

pub(super) struct BrowserOwnership {
    identity: ProcessIdentity,
    monitor_identity: Option<ProcessIdentity>,
    owns_process_group: bool,
    monitor: Option<Child>,
    liveness: Option<ChildStdin>,
}

#[derive(Clone, Copy)]
struct ProcessIdentity {
    pid: u32,
    process_group: u32,
    session_id: u32,
    start_time: u64,
}

pub(super) fn spawn(
    mut command: Command,
    inherit_process_group: bool,
) -> Result<(Child, BrowserOwnership), String> {
    if inherit_process_group {
        return spawn_inherited(command);
    }
    command.process_group(0);
    spawn_owned(command)
}

fn spawn_inherited(mut command: Command) -> Result<(Child, BrowserOwnership), String> {
    let mut child = spawn_identified(&mut command)?;
    let identity = process_identity(child.id()).inspect_err(|_| reap_failed_spawn(&mut child))?;
    Ok((
        child,
        BrowserOwnership {
            identity,
            monitor_identity: None,
            owns_process_group: false,
            monitor: None,
            liveness: None,
        },
    ))
}

fn spawn_owned(mut command: Command) -> Result<(Child, BrowserOwnership), String> {
    let mut child = spawn_identified(&mut command)?;
    let identity = process_identity(child.id()).inspect_err(|_| reap_failed_spawn(&mut child))?;
    if identity.process_group != identity.pid {
        reap_failed_spawn(&mut child);
        return Err("Chromium did not enter its owned process group".into());
    }
    let (monitor, monitor_identity, liveness) = match spawn_liveness_monitor(identity.process_group)
    {
        Ok(monitor) => monitor,
        Err(error) => {
            let _ = signal_leader_group(&identity, libc::SIGKILL);
            let _ = child.wait();
            return Err(error);
        }
    };
    Ok((
        child,
        BrowserOwnership {
            identity,
            monitor_identity: Some(monitor_identity),
            owns_process_group: true,
            monitor: Some(monitor),
            liveness: Some(liveness),
        },
    ))
}

fn spawn_identified(command: &mut Command) -> Result<Child, String> {
    command.spawn().map_err(|error| error.to_string())
}

fn reap_failed_spawn(child: &mut Child) {
    let _ = child.kill();
    let _ = child.wait();
}

fn spawn_liveness_monitor(
    process_group: u32,
) -> Result<(Child, ProcessIdentity, ChildStdin), String> {
    let process_group_i32: i32 = process_group
        .try_into()
        .map_err(|_| "Chromium process group does not fit pid_t".to_owned())?;
    let mut command = Command::new("/bin/sh");
    command
        .args([
            "-c",
            "while IFS= read -r _; do :; done; kill -KILL 0",
            "manuvra-browser-liveness",
        ])
        .process_group(process_group_i32)
        .env_remove("TYPESAFE_API_KEY")
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let mut monitor = command
        .spawn()
        .map_err(|error| format!("cannot start Chromium liveness monitor: {error}"))?;
    let liveness = monitor
        .stdin
        .take()
        .ok_or_else(|| "Chromium liveness monitor has no pipe".to_owned())?;
    let monitor_identity = process_identity(monitor.id())?;
    if monitor_identity.process_group != process_group {
        let _ = monitor.kill();
        let _ = monitor.wait();
        return Err("Chromium liveness monitor escaped its owned process group".into());
    }
    Ok((monitor, monitor_identity, liveness))
}

pub(super) fn discover_binary(
    explicit: Option<&Path>,
    environment: Option<PathBuf>,
    search_path: Option<OsString>,
) -> Result<PathBuf, BrowserError> {
    discover_binary_from(
        explicit,
        environment,
        search_path,
        env::var_os("HOME").map(PathBuf::from),
        &[PathBuf::from("/Applications")],
    )
}

pub(super) fn discover_binary_from(
    explicit: Option<&Path>,
    environment: Option<PathBuf>,
    search_path: Option<OsString>,
    home: Option<PathBuf>,
    system_application_roots: &[PathBuf],
) -> Result<PathBuf, BrowserError> {
    let configured = explicit.map(Path::to_path_buf).or(environment);
    if let Some(path) = configured {
        return usable_binary(&path).ok_or(BrowserError::Unavailable);
    }
    for root in application_roots(system_application_roots, home.as_deref()) {
        for relative in [
            "Google Chrome.app/Contents/MacOS/Google Chrome",
            "Chromium.app/Contents/MacOS/Chromium",
        ] {
            if let Some(path) = usable_binary(&root.join(relative)) {
                return Ok(path);
            }
        }
    }
    for name in ["google-chrome", "google-chrome-stable", "chromium"] {
        if let Some(path) = find_on_path(name, search_path.as_deref()) {
            return Ok(path);
        }
    }
    Err(BrowserError::Unavailable)
}

fn application_roots(system_roots: &[PathBuf], home: Option<&Path>) -> Vec<PathBuf> {
    let mut roots = system_roots.to_vec();
    if let Some(home) = home {
        roots.push(home.join("Applications"));
    }
    roots
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
    if ownership.owns_process_group {
        terminate_group(child, ownership)
    } else {
        terminate_process(child, &ownership.identity)
    }
}

fn terminate_group(child: &mut Child, ownership: &mut BrowserOwnership) -> Result<(), String> {
    let members = record_owned_group(ownership)?;
    signal_recorded_group(&ownership.identity, &members, libc::SIGTERM)?;
    if !wait_for_owned_group_exit(child, &ownership.identity, GRACEFUL_TIMEOUT)? {
        force_terminate_group(child, &ownership.identity, &members)?;
    }
    reap_group_children(child, ownership);
    Ok(())
}

fn force_terminate_group(
    child: &mut Child,
    identity: &ProcessIdentity,
    members: &[ProcessIdentity],
) -> Result<(), String> {
    signal_recorded_group(identity, members, libc::SIGKILL)?;
    wait_for_owned_group_exit(child, identity, FORCED_TIMEOUT)?
        .then_some(())
        .ok_or_else(|| "Chromium process group remained alive after SIGKILL".into())
}

fn reap_group_children(child: &mut Child, ownership: &mut BrowserOwnership) {
    let _ = child.wait();
    ownership.liveness.take();
    if let Some(monitor) = ownership.monitor.as_mut() {
        let _ = monitor.wait();
    }
}

fn terminate_process(child: &mut Child, identity: &ProcessIdentity) -> Result<(), String> {
    signal_same_process(identity, libc::SIGTERM)?;
    if wait_for_process_exit(child, GRACEFUL_TIMEOUT)? {
        return Ok(());
    }
    signal_same_process(identity, libc::SIGKILL)?;
    child
        .wait()
        .map(|_| ())
        .map_err(|error| safe_error(&error.to_string()))
}

fn signal_same_process(identity: &ProcessIdentity, signal: i32) -> Result<(), String> {
    if !process_is_same(identity) {
        return process_exists(identity.pid).and_then(|exists| {
            if exists {
                Err("Chromium identity changed before signaling".into())
            } else {
                Ok(())
            }
        });
    }
    signal_pid(identity.pid, signal)
}

fn signal_leader_group(identity: &ProcessIdentity, signal: i32) -> Result<(), String> {
    if identity.process_group != identity.pid {
        return Err("Chromium ownership does not name a group leader".into());
    }
    if !process_is_same(identity) {
        return process_exists(identity.pid).and_then(|exists| {
            if exists {
                Err("Chromium group leader identity changed before signaling".into())
            } else {
                Err("Chromium group leader exited before signaling".into())
            }
        });
    }
    signal_group(identity.process_group, signal)
}

fn signal_recorded_group(
    identity: &ProcessIdentity,
    members: &[ProcessIdentity],
    signal: i32,
) -> Result<(), String> {
    if !recorded_group_is_signalable(identity, members)? {
        return Ok(());
    }
    signal_group(identity.process_group, signal)
}

fn recorded_group_is_signalable(
    identity: &ProcessIdentity,
    members: &[ProcessIdentity],
) -> Result<bool, String> {
    if identity.process_group != identity.pid {
        return Err("Chromium ownership does not name a group leader".into());
    }
    if members.iter().any(process_is_same) {
        return Ok(true);
    }
    if owned_group_has_member(identity)? {
        return Err("Chromium process group identity changed before signaling".into());
    }
    Ok(false)
}

fn record_owned_group(ownership: &BrowserOwnership) -> Result<Vec<ProcessIdentity>, String> {
    if ownership.identity.process_group != ownership.identity.pid {
        return Err("Chromium ownership does not name a group leader".into());
    }
    let has_owner = process_is_same(&ownership.identity)
        || ownership
            .monitor_identity
            .as_ref()
            .is_some_and(process_is_same);
    if !has_owner {
        if owned_group_has_member(&ownership.identity)? {
            return Err("Chromium owned identity disappeared while its group remained".into());
        }
        return Ok(Vec::new());
    }
    Ok(process_group_members(ownership.identity.process_group)?
        .into_iter()
        .filter_map(|pid| process_identity(pid).ok())
        .filter(|member| {
            member.process_group == ownership.identity.process_group
                && member.session_id == ownership.identity.session_id
        })
        .collect())
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

fn wait_for_owned_group_exit(
    child: &mut Child,
    identity: &ProcessIdentity,
    timeout: Duration,
) -> Result<bool, String> {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        let _ = child.try_wait();
        if !process_is_same(identity) && !owned_group_has_member(identity)? {
            return Ok(true);
        }
        thread::sleep(Duration::from_millis(20));
    }
    Ok(false)
}

fn process_identity(pid: u32) -> Result<ProcessIdentity, String> {
    let fields = stable_identity_fields(pid)?;
    Ok(ProcessIdentity {
        pid,
        process_group: fields.process_group,
        session_id: fields.session_id,
        start_time: fields.start_time,
    })
}

fn process_is_same(identity: &ProcessIdentity) -> bool {
    stable_identity_fields(identity.pid).is_ok_and(|fields| {
        fields.process_group == identity.process_group
            && fields.session_id == identity.session_id
            && fields.start_time == identity.start_time
    })
}

fn owned_group_has_member(identity: &ProcessIdentity) -> Result<bool, String> {
    for pid in process_group_members(identity.process_group)? {
        if stable_identity_fields(pid).is_ok_and(|fields| {
            fields.process_group == identity.process_group
                && fields.session_id == identity.session_id
        }) {
            return Ok(true);
        }
    }
    Ok(false)
}

fn process_exists(pid: u32) -> Result<bool, String> {
    let pid: i32 = pid
        .try_into()
        .map_err(|_| "process id does not fit pid_t".to_owned())?;
    if unsafe { libc::kill(pid, 0) } == 0 {
        return Ok(true);
    }
    let error = std::io::Error::last_os_error();
    match error.raw_os_error() {
        Some(libc::ESRCH) => Ok(false),
        Some(libc::EPERM) => Ok(true),
        _ => Err(format!("cannot test process {pid} existence: {error}")),
    }
}

#[derive(Clone, Copy)]
struct IdentityFields {
    process_group: u32,
    session_id: u32,
    start_time: u64,
}

fn stable_identity_fields(pid: u32) -> Result<IdentityFields, String> {
    let first = bsd_info(pid)?;
    let session_id = process_session(pid)?;
    let second = bsd_info(pid)?;
    if !identity_reads_match(&first, &second) {
        return Err(format!("process {pid} identity changed while inspected"));
    }
    Ok(IdentityFields {
        process_group: first.pbi_pgid,
        session_id,
        start_time: start_time(pid, &first)?,
    })
}

fn identity_reads_match(first: &libc::proc_bsdinfo, second: &libc::proc_bsdinfo) -> bool {
    first.pbi_start_tvsec == second.pbi_start_tvsec
        && first.pbi_start_tvusec == second.pbi_start_tvusec
        && first.pbi_pgid == second.pbi_pgid
}

fn start_time(pid: u32, fields: &libc::proc_bsdinfo) -> Result<u64, String> {
    fields
        .pbi_start_tvsec
        .checked_mul(1_000_000)
        .and_then(|seconds| seconds.checked_add(fields.pbi_start_tvusec))
        .ok_or_else(|| format!("process {pid} start identity overflowed"))
}

fn process_session(pid: u32) -> Result<u32, String> {
    let pid_t: i32 = pid
        .try_into()
        .map_err(|_| "process id does not fit pid_t".to_owned())?;
    let session = unsafe { libc::getsid(pid_t) };
    if session == -1 {
        return Err(format!(
            "cannot inspect process {pid} session: {}",
            std::io::Error::last_os_error()
        ));
    }
    session
        .try_into()
        .map_err(|_| format!("process {pid} session does not fit u32"))
}

fn bsd_info(pid: u32) -> Result<libc::proc_bsdinfo, String> {
    let pid_t: i32 = pid
        .try_into()
        .map_err(|_| "process id does not fit pid_t".to_owned())?;
    let mut info = MaybeUninit::<libc::proc_bsdinfo>::zeroed();
    let expected: i32 = size_of::<libc::proc_bsdinfo>()
        .try_into()
        .map_err(|_| "proc_bsdinfo size does not fit c_int".to_owned())?;
    let result = unsafe {
        libc::proc_pidinfo(
            pid_t,
            libc::PROC_PIDTBSDINFO,
            0,
            info.as_mut_ptr().cast(),
            expected,
        )
    };
    if result != expected {
        return Err(format!(
            "cannot inspect process {pid}: {}",
            std::io::Error::last_os_error()
        ));
    }
    let info = unsafe { info.assume_init() };
    if info.pbi_pid != pid {
        return Err(format!("process {pid} identity returned another pid"));
    }
    Ok(info)
}

fn process_group_members(process_group: u32) -> Result<Vec<u32>, String> {
    let process_group: i32 = process_group
        .try_into()
        .map_err(|_| "process group does not fit pid_t".to_owned())?;
    let mut capacity = 64_usize;
    loop {
        let pids = query_process_group(process_group, capacity)?;
        if pids.len() < capacity || capacity == 65_536 {
            return Ok(pids);
        }
        capacity = capacity.saturating_mul(2).min(65_536);
    }
}

fn query_process_group(process_group: i32, capacity: usize) -> Result<Vec<u32>, String> {
    let mut pids = vec![0_i32; capacity];
    let bytes: i32 = (pids.len() * size_of::<i32>())
        .try_into()
        .map_err(|_| "process-group query buffer is too large".to_owned())?;
    let result = unsafe { libc::proc_listpgrppids(process_group, pids.as_mut_ptr().cast(), bytes) };
    if result < 0 {
        return Err(format!(
            "cannot inspect process group {process_group}: {}",
            std::io::Error::last_os_error()
        ));
    }
    let used: usize = result
        .try_into()
        .map_err(|_| "process-group query returned an invalid count".to_owned())?;
    if used > pids.len() {
        return Err("process-group query exceeded its buffer".into());
    }
    pids.truncate(used);
    Ok(pids
        .into_iter()
        .filter_map(|pid| u32::try_from(pid).ok())
        .collect())
}

fn signal_pid(pid: u32, signal: i32) -> Result<(), String> {
    let pid: i32 = pid
        .try_into()
        .map_err(|_| "process id does not fit pid_t".to_owned())?;
    let result = unsafe { libc::kill(pid, signal) };
    if result == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH) {
        Ok(())
    } else {
        Err(safe_error(&std::io::Error::last_os_error().to_string()))
    }
}

fn signal_group(process_group: u32, signal: i32) -> Result<(), String> {
    let process_group: i32 = process_group
        .try_into()
        .map_err(|_| "process group does not fit pid_t".to_owned())?;
    let result = unsafe { libc::kill(-process_group, signal) };
    if result == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH) {
        Ok(())
    } else {
        Err(safe_error(&std::io::Error::last_os_error().to_string()))
    }
}

#[cfg(test)]
pub(super) fn ownership_for_test(child: &Child, owns_process_group: bool) -> BrowserOwnership {
    BrowserOwnership {
        identity: process_identity(child.id()).expect("test process identity"),
        monitor_identity: None,
        owns_process_group,
        monitor: None,
        liveness: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader};
    use std::os::unix::process::ExitStatusExt;

    fn executable(path: &Path) {
        use std::os::unix::fs::PermissionsExt;
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, "#!/bin/sh\nexit 0\n").unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
    }

    #[test]
    fn discovery_prefers_explicit_environment_system_user_then_path() {
        let temporary = tempfile::tempdir().unwrap();
        let explicit = temporary.path().join("explicit");
        let environment = temporary.path().join("environment");
        let system_root = temporary.path().join("system");
        let home = temporary.path().join("home");
        let system_chrome = system_root.join("Google Chrome.app/Contents/MacOS/Google Chrome");
        let user_chrome = home.join("Applications/Chromium.app/Contents/MacOS/Chromium");
        let path_dir = temporary.path().join("bin");
        let path_binary = path_dir.join("chromium");
        for binary in [
            &explicit,
            &environment,
            &system_chrome,
            &user_chrome,
            &path_binary,
        ] {
            executable(binary);
        }
        let roots = [system_root];
        assert_eq!(
            discover_binary_from(
                Some(&explicit),
                Some(environment.clone()),
                Some(path_dir.clone().into_os_string()),
                Some(home.clone()),
                &roots,
            )
            .unwrap(),
            fs::canonicalize(&explicit).unwrap()
        );
        assert_eq!(
            discover_binary_from(
                None,
                Some(environment.clone()),
                Some(path_dir.clone().into_os_string()),
                Some(home.clone()),
                &roots,
            )
            .unwrap(),
            fs::canonicalize(environment).unwrap()
        );
        assert_eq!(
            discover_binary_from(
                None,
                None,
                Some(path_dir.clone().into_os_string()),
                Some(home.clone()),
                &roots,
            )
            .unwrap(),
            fs::canonicalize(&system_chrome).unwrap()
        );
        fs::remove_file(&system_chrome).unwrap();
        assert_eq!(
            discover_binary_from(
                None,
                None,
                Some(path_dir.into_os_string()),
                Some(home),
                &roots,
            )
            .unwrap(),
            fs::canonicalize(user_chrome).unwrap()
        );
    }

    #[test]
    fn configured_non_executable_never_falls_back_to_an_application_bundle() {
        let temporary = tempfile::tempdir().unwrap();
        let configured = temporary.path().join("configured");
        fs::write(&configured, "not executable").unwrap();
        let system_root = temporary.path().join("system");
        executable(&system_root.join("Google Chrome.app/Contents/MacOS/Google Chrome"));
        assert!(matches!(
            discover_binary_from(
                Some(&configured),
                None,
                None,
                None,
                std::slice::from_ref(&system_root),
            ),
            Err(BrowserError::Unavailable)
        ));
        assert!(matches!(
            discover_binary_from(None, Some(configured), None, None, &[system_root]),
            Err(BrowserError::Unavailable)
        ));
    }

    #[test]
    fn liveness_pipe_eof_kills_the_owned_group() {
        let mut command = Command::new("/bin/sh");
        command.args(["-c", "trap '' TERM; while :; do sleep 1; done"]);
        let (mut child, mut ownership) = spawn(command, false).unwrap();
        let pid = child.id();
        ownership.liveness.take();
        let deadline = Instant::now() + Duration::from_secs(2);
        while child.try_wait().unwrap().is_none() {
            assert!(
                Instant::now() < deadline,
                "liveness monitor did not kill group"
            );
            thread::sleep(Duration::from_millis(10));
        }
        assert!(!process_exists(pid).unwrap());
        if let Some(monitor) = ownership.monitor.as_mut() {
            monitor.wait().unwrap();
        }
    }

    #[test]
    fn leader_dead_descendant_is_found_and_term_resistance_escalates() {
        let mut command = Command::new("/bin/sh");
        command
            .args([
                "-c",
                "trap '' TERM; /bin/sh -c 'trap \"\" TERM; while :; do sleep 1; done' & echo $!; exit 0",
            ])
            .stdout(Stdio::piped());
        let (mut child, mut ownership) = spawn(command, false).unwrap();
        let mut descendant = String::new();
        BufReader::new(child.stdout.take().unwrap())
            .read_line(&mut descendant)
            .unwrap();
        let descendant: u32 = descendant.trim().parse().unwrap();
        let leader_status = child.wait().unwrap();
        assert!(leader_status.success());
        let started = Instant::now();
        terminate(&mut child, &mut ownership).unwrap();
        assert!(started.elapsed() >= GRACEFUL_TIMEOUT);
        let deadline = Instant::now() + Duration::from_secs(1);
        while process_exists(descendant).unwrap() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(10));
        }
        assert!(!process_exists(descendant).unwrap());
    }

    #[test]
    fn direct_child_term_resistance_reaches_sigkill() {
        let mut command = Command::new("/bin/sh");
        command
            .args(["-c", "trap '' TERM; echo ready; while :; do sleep 1; done"])
            .stdout(Stdio::piped());
        let mut child = command.spawn().unwrap();
        let mut ready = String::new();
        BufReader::new(child.stdout.take().unwrap())
            .read_line(&mut ready)
            .unwrap();
        assert_eq!(ready.trim(), "ready");
        let identity = process_identity(child.id()).unwrap();
        let started = Instant::now();
        terminate_process(&mut child, &identity).unwrap();
        assert!(started.elapsed() >= GRACEFUL_TIMEOUT);
        assert_eq!(child.wait().unwrap().signal(), Some(libc::SIGKILL));
    }

    #[test]
    fn inherited_browser_ownership_terminates_only_the_exact_child() {
        let mut command = Command::new("sleep");
        command.arg("30");
        let (mut child, mut ownership) = spawn(command, true).unwrap();
        assert!(!ownership.owns_process_group);
        terminate(&mut child, &mut ownership).unwrap();
        assert!(!process_exists(ownership.identity.pid).unwrap());
    }

    #[test]
    fn stale_group_identity_is_refused_without_signaling() {
        let mut command = Command::new("sleep");
        command.arg("30");
        let (mut child, mut ownership) = spawn(command, false).unwrap();
        let stale = ProcessIdentity {
            start_time: ownership.identity.start_time.saturating_add(1),
            ..ownership.identity
        };
        assert!(signal_leader_group(&stale, libc::SIGKILL).is_err());
        assert!(child.try_wait().unwrap().is_none());
        terminate(&mut child, &mut ownership).unwrap();
    }

    #[test]
    fn stale_recorded_members_do_not_authorize_a_group_signal() {
        let mut command = Command::new("sleep");
        command.arg("30");
        let (mut child, mut ownership) = spawn(command, false).unwrap();
        let mut stale_members = record_owned_group(&ownership).unwrap();
        for member in &mut stale_members {
            member.start_time = member.start_time.saturating_add(1);
        }
        assert!(signal_recorded_group(&ownership.identity, &stale_members, libc::SIGTERM).is_err());
        assert!(child.try_wait().unwrap().is_none());
        terminate(&mut child, &mut ownership).unwrap();
    }
}
