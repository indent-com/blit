//! `sd_notify(3)` over `libc` for signalling readiness to systemd.

#[cfg(target_os = "linux")]
pub fn notify_ready(verbose: bool) {
    notify(b"READY=1\n", verbose);
}

#[cfg(not(target_os = "linux"))]
pub fn notify_ready(_verbose: bool) {}

#[cfg(target_os = "linux")]
fn notify(payload: &[u8], verbose: bool) {
    let path = match std::env::var_os("NOTIFY_SOCKET") {
        Some(p) => p,
        None => return,
    };
    let path_bytes = std::os::unix::ffi::OsStrExt::as_bytes(path.as_os_str());
    if path_bytes.is_empty() {
        return;
    }

    let mut addr: libc::sockaddr_un = unsafe { std::mem::zeroed() };
    if path_bytes.len() > addr.sun_path.len() {
        if verbose {
            eprintln!(
                "sd_notify: NOTIFY_SOCKET path too long ({} bytes)",
                path_bytes.len()
            );
        }
        return;
    }
    addr.sun_family = libc::AF_UNIX as libc::sa_family_t;

    let addr_len = if path_bytes[0] == b'@' {
        addr.sun_path[0] = 0;
        for (i, b) in path_bytes[1..].iter().enumerate() {
            addr.sun_path[i + 1] = *b as libc::c_char;
        }
        std::mem::size_of::<libc::sa_family_t>() + path_bytes.len()
    } else if path_bytes[0] == b'/' {
        for (i, b) in path_bytes.iter().enumerate() {
            addr.sun_path[i] = *b as libc::c_char;
        }
        std::mem::size_of::<libc::sa_family_t>() + path_bytes.len() + 1
    } else {
        if verbose {
            eprintln!("sd_notify: NOTIFY_SOCKET must start with '/' or '@'");
        }
        return;
    };

    let fd = unsafe { libc::socket(libc::AF_UNIX, libc::SOCK_DGRAM | libc::SOCK_CLOEXEC, 0) };
    if fd < 0 {
        if verbose {
            eprintln!(
                "sd_notify: socket() failed: {}",
                std::io::Error::last_os_error()
            );
        }
        return;
    }

    let sent = unsafe {
        libc::sendto(
            fd,
            payload.as_ptr() as *const libc::c_void,
            payload.len(),
            libc::MSG_NOSIGNAL,
            &addr as *const libc::sockaddr_un as *const libc::sockaddr,
            addr_len as libc::socklen_t,
        )
    };
    if sent < 0 && verbose {
        eprintln!(
            "sd_notify: sendto() failed: {}",
            std::io::Error::last_os_error()
        );
    }

    unsafe { libc::close(fd) };
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;
    use std::os::unix::net::UnixDatagram;

    #[test]
    fn noop_when_env_unset() {
        unsafe { std::env::remove_var("NOTIFY_SOCKET") };
        notify_ready(true);
    }

    #[test]
    fn sends_ready_to_filesystem_socket() {
        let dir = tempdir();
        let path = format!("{dir}/notify.sock");
        let listener = UnixDatagram::bind(&path).expect("bind");
        listener
            .set_read_timeout(Some(std::time::Duration::from_secs(2)))
            .unwrap();

        unsafe { std::env::set_var("NOTIFY_SOCKET", &path) };
        notify_ready(true);
        unsafe { std::env::remove_var("NOTIFY_SOCKET") };

        let mut buf = [0u8; 64];
        let n = listener.recv(&mut buf).expect("recv");
        assert_eq!(&buf[..n], b"READY=1\n");
    }

    #[test]
    fn malformed_address_is_ignored() {
        unsafe { std::env::set_var("NOTIFY_SOCKET", "not-a-valid-prefix") };
        notify_ready(true);
        unsafe { std::env::remove_var("NOTIFY_SOCKET") };
    }

    fn tempdir() -> String {
        let base = std::env::temp_dir();
        let dir = base.join(format!("blit-sd-notify-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir.to_string_lossy().into_owned()
    }
}
