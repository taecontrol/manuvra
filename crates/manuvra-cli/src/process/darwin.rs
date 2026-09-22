use crate::store::ProcessIdentity;
use std::mem::{MaybeUninit, size_of};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::ptr;

pub fn process_identity(pid: u32) -> Result<ProcessIdentity, String> {
    let fields = stable_identity_fields(pid)?;
    Ok(ProcessIdentity {
        pid,
        process_group: fields.process_group,
        start_marker: fields.start_time,
        session_id: fields.session_id,
    })
}

pub fn process_is_same(identity: &ProcessIdentity) -> bool {
    stable_identity_fields(identity.pid).is_ok_and(|fields| fields.matches(identity))
}

pub fn signal_process_group(identity: &ProcessIdentity, signal: i32) -> Result<bool, String> {
    if identity.process_group != identity.pid {
        return Ok(false);
    }
    if !process_is_same(identity) && !owned_group_has_member(identity)? {
        return Ok(false);
    }
    signal_group(identity.process_group, signal)
}

pub fn owned_group_has_member(identity: &ProcessIdentity) -> Result<bool, String> {
    if identity.process_group != identity.pid {
        return Ok(false);
    }
    if process_exists(identity.pid)? {
        // A live process at the recorded leader PID with another identity can own a reused group.
        return Ok(false);
    }
    for pid in process_group_members(identity.process_group)? {
        if stable_identity_fields(pid).is_ok_and(|fields| {
            fields.process_group == identity.process_group
                && fields.session_id == identity.session_id
        }) {
            return process_exists(identity.pid).map(|exists| !exists);
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

pub fn child_exited_without_reaping(pid: u32) -> Result<bool, String> {
    let pid: i32 = pid
        .try_into()
        .map_err(|_| "host process id does not fit pid_t".to_owned())?;
    let mut info = MaybeUninit::<libc::siginfo_t>::zeroed();
    let result = unsafe {
        libc::waitid(
            libc::P_PID,
            pid as libc::id_t,
            info.as_mut_ptr(),
            libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
        )
    };
    if result == -1 {
        return Err(std::io::Error::last_os_error().to_string());
    }
    let info = unsafe { info.assume_init() };
    Ok(unsafe { info.si_pid() } != 0)
}

pub struct ParentLossMonitor {
    queue: OwnedFd,
}

impl ParentLossMonitor {
    pub fn register(parent_pid: u32, liveness_fd: RawFd) -> Result<Self, String> {
        let parent_pid: libc::uintptr_t = parent_pid
            .try_into()
            .map_err(|_| "parent process id does not fit uintptr_t".to_owned())?;
        let liveness_fd: libc::uintptr_t = liveness_fd
            .try_into()
            .map_err(|_| "liveness descriptor is invalid".to_owned())?;
        let queue = unsafe { libc::kqueue() };
        if queue == -1 {
            return Err(format!(
                "cannot create parent-loss queue: {}",
                std::io::Error::last_os_error()
            ));
        }
        let queue = unsafe { OwnedFd::from_raw_fd(queue) };
        let changes = [
            event(parent_pid, libc::EVFILT_PROC, libc::NOTE_EXIT),
            event(liveness_fd, libc::EVFILT_READ, 0),
        ];
        let result = unsafe {
            libc::kevent(
                queue.as_raw_fd(),
                changes.as_ptr(),
                changes.len() as i32,
                ptr::null_mut(),
                0,
                ptr::null(),
            )
        };
        if result == -1 {
            return Err(format!(
                "cannot register parent-loss observation: {}",
                std::io::Error::last_os_error()
            ));
        }
        Ok(Self { queue })
    }

    pub fn wait(self) -> Result<(), String> {
        let mut observed = MaybeUninit::<libc::kevent>::zeroed();
        let result = unsafe {
            libc::kevent(
                self.queue.as_raw_fd(),
                ptr::null(),
                0,
                observed.as_mut_ptr(),
                1,
                ptr::null(),
            )
        };
        if result == -1 {
            return Err(format!(
                "cannot observe parent loss: {}",
                std::io::Error::last_os_error()
            ));
        }
        if result == 0 {
            return Err("parent-loss observation returned without an event".into());
        }
        let observed = unsafe { observed.assume_init() };
        let parent_exit =
            observed.filter == libc::EVFILT_PROC && observed.fflags & libc::NOTE_EXIT != 0;
        let liveness_eof =
            observed.filter == libc::EVFILT_READ && observed.flags & libc::EV_EOF != 0;
        if parent_exit || liveness_eof {
            Ok(())
        } else {
            Err("parent-loss observation returned an unrelated event".into())
        }
    }
}

#[derive(Clone, Copy)]
struct IdentityFields {
    process_group: u32,
    session_id: u32,
    start_time: u64,
}

impl IdentityFields {
    fn matches(self, identity: &ProcessIdentity) -> bool {
        self.process_group == identity.process_group
            && self.session_id == identity.session_id
            && self.start_time == identity.start_marker
    }
}

fn stable_identity_fields(pid: u32) -> Result<IdentityFields, String> {
    let first = bsd_info(pid)?;
    let session_id = process_session(pid)?;
    let second = bsd_info(pid)?;
    let start_time = stable_start_time(pid, &first, &second)?;
    Ok(IdentityFields {
        process_group: first.pbi_pgid,
        session_id,
        start_time,
    })
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

fn stable_start_time(
    pid: u32,
    first: &libc::proc_bsdinfo,
    second: &libc::proc_bsdinfo,
) -> Result<u64, String> {
    if first.pbi_start_tvsec != second.pbi_start_tvsec
        || first.pbi_start_tvusec != second.pbi_start_tvusec
        || first.pbi_pgid != second.pbi_pgid
    {
        return Err(format!("process {pid} identity changed while inspected"));
    }
    first
        .pbi_start_tvsec
        .checked_mul(1_000_000)
        .and_then(|seconds| seconds.checked_add(first.pbi_start_tvusec))
        .ok_or_else(|| format!("process {pid} start identity overflowed"))
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

fn signal_group(process_group: u32, signal: i32) -> Result<bool, String> {
    let process_group: i32 = process_group
        .try_into()
        .map_err(|_| "process group does not fit pid_t".to_owned())?;
    let result = unsafe { libc::kill(-process_group, signal) };
    if result == 0 {
        return Ok(true);
    }
    let error = std::io::Error::last_os_error();
    if error.raw_os_error() == Some(libc::ESRCH) {
        Ok(false)
    } else {
        Err(format!("cannot signal owned process group: {error}"))
    }
}

fn event(ident: libc::uintptr_t, filter: i16, fflags: u32) -> libc::kevent {
    libc::kevent {
        ident,
        filter,
        flags: libc::EV_ADD | libc::EV_ONESHOT,
        fflags,
        data: 0,
        udata: ptr::null_mut(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::os::unix::process::{CommandExt, ExitStatusExt};
    use std::path::Path;
    use std::process::{Command, Stdio};
    use std::time::{Duration, Instant};
    use tempfile::TempDir;

    const PARENT_FIXTURE: &str = "MANUVRA_TEST_PARENT_LOSS_PARENT";
    const WATCHER_FIXTURE: &str = "MANUVRA_TEST_PARENT_LOSS_WATCHER";
    const READY_PATH: &str = "MANUVRA_TEST_PARENT_LOSS_READY";
    const LOST_PATH: &str = "MANUVRA_TEST_PARENT_LOSS_LOST";
    const LIVENESS_FD: &str = "MANUVRA_TEST_PARENT_LOSS_FD";
    const PARENT_PID: &str = "MANUVRA_TEST_PARENT_LOSS_PID";
    const WATCHDOG_LOSS_HOST: &str = "MANUVRA_TEST_WATCHDOG_LOSS_HOST";
    const HELPER_PATH: &str = "MANUVRA_TEST_WATCHDOG_LOSS_HELPER";

    #[test]
    fn live_identity_revalidates_and_stale_start_is_refused() {
        let mut child = isolated_group_child();
        let current = process_identity(child.id()).unwrap();
        assert!(process_is_same(&current));
        let stale = ProcessIdentity {
            start_marker: current.start_marker.saturating_add(1),
            ..current.clone()
        };
        assert!(!process_is_same(&stale));
        assert!(!signal_process_group(&stale, libc::SIGTERM).unwrap());
        std::thread::sleep(Duration::from_millis(50));
        assert!(child.try_wait().unwrap().is_none());
        assert!(signal_process_group(&current, libc::SIGKILL).unwrap());
        child.wait().unwrap();
    }

    #[test]
    fn reaped_leader_does_not_authorize_an_empty_group() {
        let mut command = Command::new("sh");
        command
            .args(["-c", "exit 0"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        unsafe {
            command.pre_exec(|| {
                if libc::setpgid(0, 0) == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let mut child = command.spawn().unwrap();
        let identity = process_identity(child.id()).unwrap();
        child.wait().unwrap();
        assert!(!process_is_same(&identity));
        assert!(!owned_group_has_member(&identity).unwrap());
        assert!(!signal_process_group(&identity, libc::SIGKILL).unwrap());

        let current = process_identity(std::process::id()).unwrap();
        let reused_group = ProcessIdentity {
            process_group: current.process_group,
            session_id: current.session_id,
            ..identity
        };
        assert!(!owned_group_has_member(&reused_group).unwrap());
        assert!(!signal_process_group(&reused_group, 0).unwrap());
    }

    #[test]
    fn leader_dead_group_member_reaches_term_then_kill_cleanup() {
        let temporary = TempDir::new().unwrap();
        let member_path = temporary.path().join("member.pid");
        let mut command = Command::new("sh");
        command
            .args([
                "-c",
                "sh -c 'trap \"\" TERM HUP; printf \"%s\" \"$$\" > \"$MEMBER_PATH\"; while :; do sleep 1; done' & while [ ! -s \"$MEMBER_PATH\" ]; do sleep 0.01; done; exit 0",
            ])
            .env("MEMBER_PATH", &member_path)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        unsafe {
            command.pre_exec(|| {
                if libc::setpgid(0, 0) == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let mut leader = command.spawn().unwrap();
        let identity = process_identity(leader.id()).unwrap();
        wait_for_path(&member_path);
        let member: u32 = fs::read_to_string(&member_path).unwrap().parse().unwrap();
        leader.wait().unwrap();

        assert!(!process_is_same(&identity));
        assert!(owned_group_has_member(&identity).unwrap());
        assert!(signal_process_group(&identity, libc::SIGTERM).unwrap());
        std::thread::sleep(Duration::from_millis(50));
        assert_eq!(unsafe { libc::kill(member as i32, 0) }, 0);
        assert!(signal_process_group(&identity, libc::SIGKILL).unwrap());
        wait_for_group_exit(&identity);
    }

    #[test]
    fn waitid_observes_exit_without_reaping() {
        let mut child = Command::new("sh").args(["-c", "exit 42"]).spawn().unwrap();
        let deadline = Instant::now() + Duration::from_secs(1);
        while !child_exited_without_reaping(child.id()).unwrap() {
            assert!(
                Instant::now() < deadline,
                "waitid did not observe child exit"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(child.try_wait().unwrap().unwrap().code(), Some(42));
        let error = child_exited_without_reaping(child.id()).unwrap_err();
        assert!(error.contains("No child processes"), "{error}");
    }

    #[test]
    fn parent_loss_is_detected_after_registration_handshake() {
        if std::env::var_os(PARENT_FIXTURE).is_some() || std::env::var_os(WATCHER_FIXTURE).is_some()
        {
            return;
        }
        let temporary = TempDir::new().unwrap();
        let ready = temporary.path().join("ready");
        let lost = temporary.path().join("lost");
        let mut parent = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "process::platform::tests::parent_loss_parent_fixture",
                "--nocapture",
            ])
            .env(PARENT_FIXTURE, "1")
            .env(READY_PATH, &ready)
            .env(LOST_PATH, &lost)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        wait_for_path(&ready);
        parent.kill().unwrap();
        parent.wait().unwrap();
        wait_for_path(&lost);
    }

    #[test]
    fn parent_loss_parent_fixture() {
        if std::env::var_os(PARENT_FIXTURE).is_none() {
            return;
        }
        let mut descriptors = [-1_i32; 2];
        assert_eq!(unsafe { libc::pipe(descriptors.as_mut_ptr()) }, 0);
        set_cloexec(descriptors[1]);
        let ready = std::env::var_os(READY_PATH).unwrap();
        let lost = std::env::var_os(LOST_PATH).unwrap();
        let mut watcher = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "process::platform::tests::parent_loss_watcher_fixture",
                "--nocapture",
            ])
            .env(WATCHER_FIXTURE, "1")
            .env(READY_PATH, ready)
            .env(LOST_PATH, lost)
            .env(LIVENESS_FD, descriptors[0].to_string())
            .env(PARENT_PID, std::process::id().to_string())
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        unsafe {
            libc::close(descriptors[0]);
        }
        loop {
            assert!(watcher.try_wait().unwrap().is_none());
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    #[test]
    fn parent_loss_watcher_fixture() {
        if std::env::var_os(WATCHER_FIXTURE).is_none() {
            return;
        }
        let parent_pid = std::env::var(PARENT_PID).unwrap().parse().unwrap();
        let liveness_fd = std::env::var(LIVENESS_FD).unwrap().parse().unwrap();
        let monitor = ParentLossMonitor::register(parent_pid, liveness_fd).unwrap();
        fs::write(std::env::var_os(READY_PATH).unwrap(), b"ready\n").unwrap();
        monitor.wait().unwrap();
        fs::write(std::env::var_os(LOST_PATH).unwrap(), b"lost\n").unwrap();
    }

    #[test]
    fn watchdog_loss_terminates_the_owned_synthetic_group() {
        if std::env::var_os(WATCHDOG_LOSS_HOST).is_some() {
            return;
        }
        let temporary = TempDir::new().unwrap();
        let ready = temporary.path().join("ready");
        let helper = temporary.path().join("helper.pid");
        let mut descriptors = [-1_i32; 2];
        assert_eq!(unsafe { libc::pipe(descriptors.as_mut_ptr()) }, 0);
        set_cloexec(descriptors[1]);
        let mut command = Command::new(std::env::current_exe().unwrap());
        command
            .args([
                "--exact",
                "process::platform::tests::watchdog_loss_host_fixture",
                "--nocapture",
            ])
            .env(WATCHDOG_LOSS_HOST, "1")
            .env(READY_PATH, &ready)
            .env(HELPER_PATH, &helper)
            .env(LIVENESS_FD, descriptors[0].to_string())
            .env(PARENT_PID, std::process::id().to_string())
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        unsafe {
            command.pre_exec(|| {
                if libc::setpgid(0, 0) == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let mut host = command.spawn().unwrap();
        unsafe { libc::close(descriptors[0]) };
        wait_for_path(&ready);
        wait_for_path(&helper);
        let helper_pid: i32 = fs::read_to_string(&helper).unwrap().parse().unwrap();
        unsafe { libc::close(descriptors[1]) };
        let status = host.wait().unwrap();
        assert_eq!(status.signal(), Some(libc::SIGKILL));
        let deadline = Instant::now() + Duration::from_secs(3);
        while unsafe { libc::kill(helper_pid, 0) } == 0 {
            assert!(
                Instant::now() < deadline,
                "synthetic helper escaped cleanup"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    #[allow(clippy::zombie_processes)] // The fixture deliberately SIGKILLs its complete group.
    fn watchdog_loss_host_fixture() {
        if std::env::var_os(WATCHDOG_LOSS_HOST).is_none() {
            return;
        }
        let parent_pid = std::env::var(PARENT_PID).unwrap().parse().unwrap();
        let liveness_fd = std::env::var(LIVENESS_FD).unwrap().parse().unwrap();
        let monitor = ParentLossMonitor::register(parent_pid, liveness_fd).unwrap();
        let helper = Command::new("sh")
            .args(["-c", "trap '' TERM HUP; while :; do sleep 1; done"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        fs::write(
            std::env::var_os(HELPER_PATH).unwrap(),
            helper.id().to_string(),
        )
        .unwrap();
        fs::write(std::env::var_os(READY_PATH).unwrap(), b"ready\n").unwrap();
        monitor.wait().unwrap();
        unsafe { libc::kill(0, libc::SIGKILL) };
        loop {
            std::thread::park();
        }
    }

    fn wait_for_path(path: &Path) {
        let deadline = Instant::now() + Duration::from_secs(3);
        while fs::metadata(path).map_or(true, |metadata| metadata.len() == 0) {
            assert!(
                Instant::now() < deadline,
                "fixture path did not appear: {path:?}"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    fn wait_for_group_exit(identity: &ProcessIdentity) {
        let deadline = Instant::now() + Duration::from_secs(3);
        while owned_group_has_member(identity).unwrap() {
            assert!(Instant::now() < deadline, "owned group survived SIGKILL");
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    fn isolated_group_child() -> std::process::Child {
        let mut command = Command::new("sh");
        command
            .args(["-c", "while :; do sleep 1; done"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        unsafe {
            command.pre_exec(|| {
                if libc::setpgid(0, 0) == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        command.spawn().unwrap()
    }

    fn set_cloexec(fd: RawFd) {
        let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
        assert_ne!(flags, -1);
        assert_ne!(
            unsafe { libc::fcntl(fd, libc::F_SETFD, flags | libc::FD_CLOEXEC) },
            -1
        );
    }
}
