use crate::store::ProcessIdentity;
use std::fs;

pub fn process_identity(pid: u32) -> Result<ProcessIdentity, String> {
    let fields = proc_identity_fields(pid)?;
    Ok(ProcessIdentity {
        pid,
        process_group: fields.process_group,
        start_marker: fields.start_ticks,
        session_id: fields.session_id,
    })
}

pub fn process_is_same(identity: &ProcessIdentity) -> bool {
    proc_identity_fields(identity.pid).is_ok_and(|fields| fields.matches(identity))
}

/// The recorded process still runs: it has its recorded identity and has not exited into a
/// zombie awaiting its parent.
pub fn process_is_live(identity: &ProcessIdentity) -> bool {
    proc_identity_fields(identity.pid)
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
        // The numeric leader PID exists but no longer has the recorded identity. Its process
        // group may have been reused, so signalling the recorded group would be unsafe.
        return Ok(false);
    }
    if !session_group_has_member(identity.process_group, identity.session_id)? {
        return Ok(false);
    }
    process_exists(identity.pid).map(|exists| !exists)
}

fn session_group_has_member(process_group: u32, session_id: u32) -> Result<bool, String> {
    let entries =
        fs::read_dir("/proc").map_err(|error| format!("cannot inspect /proc: {error}"))?;
    for entry in entries {
        let entry = entry.map_err(|error| format!("cannot inspect /proc entry: {error}"))?;
        let pid = entry
            .file_name()
            .to_str()
            .and_then(|name| name.parse().ok());
        if pid
            .and_then(|pid| proc_identity_fields(pid).ok())
            .is_some_and(|fields| {
                fields.process_group == process_group && fields.session_id == session_id
            })
        {
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

pub fn child_exited_without_reaping(pid: u32) -> Result<bool, String> {
    let pid: i32 = pid
        .try_into()
        .map_err(|_| "host process id does not fit pid_t".to_owned())?;
    let mut info = std::mem::MaybeUninit::<libc::siginfo_t>::zeroed();
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ProcIdentityFields {
    process_group: u32,
    session_id: u32,
    start_ticks: u64,
    exited: bool,
}

impl ProcIdentityFields {
    fn matches(self, identity: &ProcessIdentity) -> bool {
        self.process_group == identity.process_group
            && self.start_ticks == identity.start_marker
            && self.session_id == identity.session_id
    }
}

fn proc_identity_fields(pid: u32) -> Result<ProcIdentityFields, String> {
    let contents = fs::read_to_string(format!("/proc/{pid}/stat"))
        .map_err(|error| format!("cannot inspect process {pid}: {error}"))?;
    let after_name = contents
        .rsplit_once(") ")
        .map(|(_, fields)| fields)
        .ok_or_else(|| format!("invalid /proc stat for process {pid}"))?;
    let fields: Vec<_> = after_name.split_whitespace().collect();
    let parse = |index: usize, name: &str| {
        fields
            .get(index)
            .ok_or_else(|| format!("process {pid} stat has no {name}"))?
            .parse::<u64>()
            .map_err(|error| format!("invalid process {pid} {name}: {error}"))
    };
    Ok(ProcIdentityFields {
        process_group: parse(2, "process group")?
            .try_into()
            .map_err(|_| format!("process {pid} process group does not fit u32"))?,
        session_id: parse(3, "session id")?
            .try_into()
            .map_err(|_| format!("process {pid} session id does not fit u32"))?,
        start_ticks: parse(19, "start identity")?,
        exited: fields
            .first()
            .is_some_and(|state| matches!(*state, "Z" | "X" | "x")),
    })
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
    use std::os::unix::process::CommandExt;
    use std::process::{Command, Stdio};

    #[test]
    fn exited_group_without_members_is_not_owned_or_signalled() {
        let mut child = Command::new("true")
            .process_group(0)
            .stdin(Stdio::null())
            .spawn()
            .unwrap();
        let identity = process_identity(child.id()).unwrap();
        assert_eq!(identity.pid, identity.process_group);
        assert!(child.wait().unwrap().success());
        assert!(!process_is_same(&identity));
        assert!(!owned_group_has_member(&identity).unwrap());
        assert!(!signal_process_group(&identity, libc::SIGKILL).unwrap());
    }

    #[test]
    fn an_unreaped_zombie_keeps_its_identity_but_is_not_live() {
        let mut child = Command::new("sleep")
            .arg("30")
            .process_group(0)
            .stdin(Stdio::null())
            .spawn()
            .unwrap();
        let identity = process_identity(child.id()).unwrap();
        assert!(process_is_live(&identity));
        child.kill().unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while !child_exited_without_reaping(child.id()).unwrap() {
            assert!(std::time::Instant::now() < deadline, "child did not exit");
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert!(process_is_same(&identity));
        assert!(!process_is_live(&identity));
        child.wait().unwrap();
        assert!(!process_is_live(&identity));
    }
}
