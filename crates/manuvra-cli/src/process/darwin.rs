use crate::store::ProcessIdentity;
use std::mem::{MaybeUninit, size_of};

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

/// The recorded process still runs: it has its recorded identity and has not exited into a
/// zombie awaiting its parent.
pub fn process_is_live(identity: &ProcessIdentity) -> bool {
    stable_identity_fields(identity.pid)
        .is_ok_and(|fields| fields.matches(identity) && !fields.exited)
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

#[derive(Clone, Copy)]
struct IdentityFields {
    process_group: u32,
    session_id: u32,
    start_time: u64,
    exited: bool,
}

/// `SZOMB` from `<sys/proc.h>`: the process exited and awaits its parent.
const ZOMBIE_STATUS: u32 = 5;

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
        exited: second.pbi_status == ZOMBIE_STATUS,
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::os::unix::process::CommandExt;
    use std::path::Path;
    use std::process::{Command, Stdio};
    use std::time::{Duration, Instant};
    use tempfile::TempDir;

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
    fn an_unreaped_zombie_is_not_live() {
        let mut child = isolated_group_child();
        let identity = process_identity(child.id()).unwrap();
        assert!(process_is_live(&identity));
        child.kill().unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while !child_exited_without_reaping(child.id()).unwrap() {
            assert!(Instant::now() < deadline, "child did not exit");
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(!process_is_live(&identity));
        child.wait().unwrap();
        assert!(!process_is_live(&identity));
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
}
