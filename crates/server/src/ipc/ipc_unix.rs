use std::os::unix::fs::PermissionsExt;
use std::os::unix::io::RawFd;
use tokio::io::unix::AsyncFd;
use tokio::net::UnixListener;

pub type IpcStream = tokio::net::UnixStream;

pub fn default_ipc_path() -> String {
    if let Ok(dir) = std::env::var("TMPDIR") {
        return format!("{dir}/blit.sock");
    }
    if let Ok(dir) = std::env::var("XDG_RUNTIME_DIR") {
        return format!("{dir}/blit.sock");
    }
    if let Ok(user) = std::env::var("USER") {
        return format!("/tmp/blit-{user}.sock");
    }
    "/tmp/blit.sock".into()
}

pub struct IpcListener {
    inner: UnixListener,
    /// Held for the process lifetime so the flock is released on exit.
    _lock: Option<std::fs::File>,
}

impl IpcListener {
    pub fn bind(path: &str, verbose: bool) -> Self {
        // Acquire an exclusive flock on a companion lockfile so that a
        // previous server instance is terminated before we bind.  The OS
        // releases the lock automatically when the holder exits — even on
        // SIGKILL — so there are no stale-lock issues.
        let lock_path = format!("{path}.lock");
        #[allow(clippy::suspicious_open_options)]
        let lock_file = std::fs::OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .open(&lock_path)
            .unwrap_or_else(|e| {
                eprintln!("blit-server: cannot open {lock_path}: {e}");
                std::process::exit(1);
            });

        use std::os::unix::io::AsRawFd;
        let fd = lock_file.as_raw_fd();
        let mut locked = unsafe { libc::flock(fd, libc::LOCK_EX | libc::LOCK_NB) } == 0;

        if !locked {
            // Lock held by another server — read its PID and terminate it.
            let mut pid_str = String::new();
            if std::io::Read::read_to_string(&mut (&lock_file), &mut pid_str).is_ok()
                && let Ok(old_pid) = pid_str.trim().parse::<i32>()
            {
                eprintln!("blit-server: terminating previous server (pid {old_pid})");
                unsafe { libc::kill(old_pid, libc::SIGTERM) };
            }
            // Wait up to 3 s for the old server to release the lock.
            for _ in 0..30 {
                std::thread::sleep(std::time::Duration::from_millis(100));
                if unsafe { libc::flock(fd, libc::LOCK_EX | libc::LOCK_NB) } == 0 {
                    locked = true;
                    break;
                }
            }
            if !locked {
                eprintln!(
                    "blit-server: cannot acquire lock {lock_path} — is another server running?"
                );
                std::process::exit(1);
            }
        }

        // Record our PID so the next server instance can terminate us.
        {
            use std::io::{Seek, Write};
            let _ = lock_file.set_len(0);
            let _ = (&lock_file).seek(std::io::SeekFrom::Start(0));
            let _ = write!(&lock_file, "{}", std::process::id());
        }

        let _ = std::fs::remove_file(path);
        // Set a restrictive umask before bind so the socket is created with
        // 0700 permissions atomically, closing the race window between bind
        // and the subsequent chmod.
        let old_umask = unsafe { libc::umask(0o077) };
        let listener = UnixListener::bind(path).unwrap_or_else(|e| {
            unsafe { libc::umask(old_umask) };
            eprintln!("blit-server: cannot bind to {path}: {e}");
            std::process::exit(1);
        });
        unsafe { libc::umask(old_umask) };
        if let Err(e) = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700)) {
            eprintln!("blit-server: warning: cannot set socket permissions: {e}");
        }
        if verbose {
            eprintln!("listening on {path}");
        }
        Self {
            inner: listener,
            _lock: Some(lock_file),
        }
    }

    pub fn from_systemd_fd(verbose: bool) -> Option<Self> {
        let fds = std::env::var("LISTEN_FDS").ok()?;
        if fds.trim() != "1" {
            if verbose {
                eprintln!("LISTEN_FDS={fds}, expected 1; falling back to bind");
            }
            return None;
        }
        let pid = std::env::var("LISTEN_PID").ok()?;
        if pid.trim() != std::process::id().to_string() {
            if verbose {
                eprintln!(
                    "LISTEN_PID={pid} does not match our pid {}; falling back to bind",
                    std::process::id()
                );
            }
            return None;
        }
        use std::os::unix::io::FromRawFd;
        let std_listener = unsafe { std::os::unix::net::UnixListener::from_raw_fd(3) };
        std_listener.set_nonblocking(true).unwrap();
        if verbose {
            eprintln!("using socket activation (fd 3)");
        }
        Some(Self {
            inner: UnixListener::from_std(std_listener).unwrap(),
            _lock: None,
        })
    }

    pub async fn accept(&self) -> std::io::Result<IpcStream> {
        let (stream, _) = self.inner.accept().await?;
        Ok(stream)
    }
}

enum RecvFdResult {
    Fd(RawFd),
    WouldBlock,
    Closed,
}

fn recv_fd(channel: RawFd) -> RecvFdResult {
    unsafe {
        let mut buf = [0u8; 1];
        let mut iov = libc::iovec {
            iov_base: buf.as_mut_ptr() as *mut libc::c_void,
            iov_len: buf.len(),
        };
        let cmsg_space = libc::CMSG_SPACE(std::mem::size_of::<RawFd>() as u32) as usize;
        let mut cmsg_buf = vec![0u8; cmsg_space];
        let mut msg: libc::msghdr = std::mem::zeroed();
        msg.msg_iov = &mut iov;
        msg.msg_iovlen = 1;
        msg.msg_control = cmsg_buf.as_mut_ptr() as *mut libc::c_void;
        msg.msg_controllen = cmsg_space as _;
        let n = libc::recvmsg(channel, &mut msg, libc::MSG_DONTWAIT);
        if n < 0 {
            let err = std::io::Error::last_os_error();
            if err.kind() == std::io::ErrorKind::WouldBlock {
                return RecvFdResult::WouldBlock;
            }
            if err.raw_os_error() == Some(libc::EINTR) {
                return RecvFdResult::WouldBlock;
            }
            return RecvFdResult::Closed;
        }
        if n == 0 {
            return RecvFdResult::Closed;
        }
        let cmsg = libc::CMSG_FIRSTHDR(&msg);
        if cmsg.is_null() {
            return RecvFdResult::Closed;
        }
        if (*cmsg).cmsg_level == libc::SOL_SOCKET && (*cmsg).cmsg_type == libc::SCM_RIGHTS {
            let fd_ptr = libc::CMSG_DATA(cmsg) as *const RawFd;
            RecvFdResult::Fd(std::ptr::read_unaligned(fd_ptr))
        } else {
            RecvFdResult::Closed
        }
    }
}

pub async fn run_fd_channel(channel_fd: RawFd, state: crate::AppState) {
    use std::os::unix::io::FromRawFd;
    if state.config.verbose {
        eprintln!("accepting clients via fd-channel (fd {channel_fd})");
    }
    let channel = unsafe { std::os::unix::net::UnixStream::from_raw_fd(channel_fd) };
    channel.set_nonblocking(true).unwrap();
    let async_channel = AsyncFd::new(channel).unwrap();
    loop {
        let mut guard = match async_channel.readable().await {
            Ok(g) => g,
            Err(e) => {
                eprintln!("fd-channel error: {e}");
                break;
            }
        };
        match recv_fd(channel_fd) {
            RecvFdResult::Fd(client_fd) => {
                let std_stream = unsafe { std::os::unix::net::UnixStream::from_raw_fd(client_fd) };
                std_stream.set_nonblocking(true).unwrap();
                let stream = tokio::net::UnixStream::from_std(std_stream).unwrap();
                let state = state.clone();
                tokio::spawn(crate::handle_client(stream, state));
                guard.retain_ready();
            }
            RecvFdResult::WouldBlock => {
                guard.clear_ready();
            }
            RecvFdResult::Closed => {
                break;
            }
        }
    }
    if state.config.verbose {
        eprintln!("fd-channel closed, shutting down");
    }
}
