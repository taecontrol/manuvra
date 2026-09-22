use std::os::fd::AsRawFd;
use std::os::unix::net::UnixStream;

#[cfg(target_os = "macos")]
#[path = "socket_auth/darwin.rs"]
mod platform;
#[cfg(target_os = "linux")]
#[path = "socket_auth/linux.rs"]
mod platform;

pub fn authenticate_current_user(stream: &UnixStream) -> Result<(), String> {
    authenticate_with(
        stream,
        platform::peer_effective_user,
        current_effective_user(),
    )
}

fn authenticate_with(
    stream: &UnixStream,
    query: impl FnOnce(i32) -> Result<u32, String>,
    current_user: u32,
) -> Result<(), String> {
    let peer = query(stream.as_raw_fd())?;
    (peer == current_user)
        .then_some(())
        .ok_or_else(|| "control connection belongs to another effective user".into())
}

fn current_effective_user() -> u32 {
    unsafe { libc::geteuid() }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::net::UnixStream;

    #[test]
    fn rejects_foreign_user_and_query_failure_before_caller_can_read() {
        let (server, mut client) = UnixStream::pair().unwrap();
        std::io::Write::write_all(&mut client, b"must remain unread").unwrap();
        let current = current_effective_user();
        assert!(authenticate_with(&server, |_| Ok(current.saturating_add(1)), current).is_err());
        assert!(
            authenticate_with(&server, |_| Err("credential query failed".into()), current).is_err()
        );
        server.set_nonblocking(true).unwrap();
        let mut byte = [0_u8; 1];
        assert_eq!(std::io::Read::read(&mut &server, &mut byte).unwrap(), 1);
    }

    #[test]
    fn live_same_user_connection_is_accepted() {
        let (server, _client) = UnixStream::pair().unwrap();
        authenticate_current_user(&server).unwrap();
    }
}
