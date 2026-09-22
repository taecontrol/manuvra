pub fn peer_effective_user(fd: i32) -> Result<u32, String> {
    let mut user = 0_u32;
    let mut group = 0_u32;
    let result = unsafe { libc::getpeereid(fd, &mut user, &mut group) };
    if result == 0 {
        Ok(user)
    } else {
        Err(format!(
            "cannot authenticate control peer: {}",
            std::io::Error::last_os_error()
        ))
    }
}
