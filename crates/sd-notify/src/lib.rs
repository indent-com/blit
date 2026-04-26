//! Minimal `sd_notify(3)` implementation for signalling readiness to systemd.
//!
//! No `libsystemd` linkage — pure `libc` socket + sendto. Mirrors the
//! documented protocol: a single `SOCK_DGRAM` datagram of `KEY=VALUE` pairs
//! (LF-separated) sent to the path in the `NOTIFY_SOCKET` environment
//! variable. A leading `@` selects the Linux abstract namespace (replaced
//! with a NUL byte). Per the spec, `NOTIFY_SOCKET` may also begin with `/`
//! (filesystem path).
//!
//! All operations are best-effort: if `NOTIFY_SOCKET` is unset, the socket
//! cannot be created, the address is malformed, or the send fails, the
//! function returns silently. systemd treats a missing notification as a
//! `TimeoutStartSec` failure rather than the daemon's responsibility to
//! recover from, so we never want to surface our own error here.
//!
//! Compiled out on non-Unix targets — the entry point is a no-op stub so
//! callers don't need their own `cfg(unix)` gates.

#[cfg(unix)]
pub fn notify_ready(verbose: bool) {
    notify(b"READY=1\n", verbose);
}

#[cfg(not(unix))]
pub fn notify_ready(_verbose: bool) {}

#[cfg(unix)]
fn notify(payload: &[u8], verbose: bool) {
    let path = match std::env::var_os("NOTIFY_SOCKET") {
        Some(p) => p,
        None => return,
    };
    let path_bytes = std::os::unix::ffi::OsStrExt::as_bytes(path.as_os_str());
    if path_bytes.is_empty() {
        return;
    }

    // sun_path is sized at 108 bytes on Linux. Reject anything that won't
    // fit so we don't silently truncate to a different address.
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

    // Spec: leading `@` → abstract socket (the `@` becomes a NUL byte).
    // Otherwise treat as a filesystem path. Both forms are documented and
    // must be supported; systemd itself uses both depending on the version.
    let addr_len = if path_bytes[0] == b'@' {
        addr.sun_path[0] = 0;
        for (i, b) in path_bytes[1..].iter().enumerate() {
            addr.sun_path[i + 1] = *b as libc::c_char;
        }
        // For abstract sockets the length includes every byte of sun_path
        // we wrote — including the leading NUL — but stops *before* any
        // trailing zeros, so the kernel doesn't pad the address.
        std::mem::size_of::<libc::sa_family_t>() + path_bytes.len()
    } else if path_bytes[0] == b'/' {
        for (i, b) in path_bytes.iter().enumerate() {
            addr.sun_path[i] = *b as libc::c_char;
        }
        // Filesystem paths are NUL-terminated, so we count the path bytes
        // plus the implicit NUL byte already zeroed in `addr`.
        std::mem::size_of::<libc::sa_family_t>() + path_bytes.len() + 1
    } else {
        if verbose {
            eprintln!("sd_notify: NOTIFY_SOCKET must start with '/' or '@'");
        }
        return;
    };

    // SOCK_CLOEXEC keeps the fd from leaking into PTY children. SOCK_DGRAM
    // is the format systemd documents; SEQPACKET also works but isn't
    // universally available on macOS, so we stick with DGRAM.
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

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::net::UnixDatagram;

    /// `notify_ready` with no `NOTIFY_SOCKET` set must be a silent no-op
    /// (the common case in dev: nobody started us under systemd).
    #[test]
    fn noop_when_env_unset() {
        // SAFETY: tests are single-threaded by default in this module and
        // none of the others touch NOTIFY_SOCKET.
        unsafe { std::env::remove_var("NOTIFY_SOCKET") };
        notify_ready(true); // must not panic
    }

    /// End-to-end: bind a SOCK_DGRAM, point NOTIFY_SOCKET at it, observe
    /// the exact `READY=1\n` payload that systemd would.
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

    /// Malformed `NOTIFY_SOCKET` must not panic or surface errors — it's a
    /// best-effort hint and the daemon must continue running either way.
    #[test]
    fn malformed_address_is_ignored() {
        unsafe { std::env::set_var("NOTIFY_SOCKET", "not-a-valid-prefix") };
        notify_ready(true); // must not panic
        unsafe { std::env::remove_var("NOTIFY_SOCKET") };
    }

    fn tempdir() -> String {
        let base = std::env::temp_dir();
        // Short, unique, and within sun_path's 108-byte limit even on /tmp.
        let dir = base.join(format!("blit-sd-notify-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir.to_string_lossy().into_owned()
    }
}
