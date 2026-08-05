use std::collections::HashMap;
use std::ffi::CString;
use std::sync::{Arc, Mutex, OnceLock};
use tokio::sync::{Notify, mpsc};

use crate::{AppState, PTY_CHANNEL_CAPACITY, PtyInput};

/// Build the environment array for a child process before fork().
/// This avoids calling std::env::set_var/remove_var after fork() in a
/// multi-threaded process (which is UB per POSIX — those functions are
/// not async-signal-safe).
fn build_child_env(
    wayland_display: Option<&str>,
    pulse_server: Option<&str>,
    pipewire_remote: Option<&str>,
    blit_sock: Option<&str>,
    path_dir: Option<&str>,
) -> Vec<CString> {
    let mut env: Vec<(String, String)> = std::env::vars()
        .filter(|(k, _)| {
            k != "COLUMNS"
                && k != "LINES"
                && k != "DISPLAY"
                && k != "PIPEWIRE_REMOTE"
                && k != "DBUS_SESSION_BUS_ADDRESS"
                && k != "DBUS_SYSTEM_BUS_ADDRESS"
                && !(k.starts_with("BLIT_") && k != "BLIT_HUB")
        })
        .collect();
    // Set/override entries.
    let set = |env: &mut Vec<(String, String)>, key: &str, val: &str| {
        if let Some(entry) = env.iter_mut().find(|(k, _)| k == key) {
            entry.1 = val.to_string();
        } else {
            env.push((key.to_string(), val.to_string()));
        }
    };
    set(&mut env, "TERM", "xterm-256color");
    set(&mut env, "COLORTERM", "truecolor");
    // Opt-in (Config::export_sock): point `blit` invocations inside the
    // terminal at this server.  Added after the BLIT_* filter above so the
    // exported value is always the path this server actually listens on.
    if let Some(sock) = blit_sock {
        set(&mut env, "BLIT_SOCK", sock);
    }
    // Opt-in (Config::inject_path): make the server's own binary reachable from
    // spawned terminals, so an exported BLIT_SOCK has something to talk to.
    // Appended rather than prepended because the binary can share a directory
    // with other tools, which must not shadow what is already on PATH.
    if let Some(dir) = path_dir {
        let current = env
            .iter()
            .find(|(k, _)| k == "PATH")
            .map(|(_, v)| v.clone())
            .unwrap_or_default();
        if !current.split(':').any(|entry| entry == dir) {
            let next = if current.is_empty() {
                dir.to_string()
            } else {
                format!("{current}:{dir}")
            };
            set(&mut env, "PATH", &next);
        }
    }
    if let Some(wd) = wayland_display {
        let wd_path = std::path::Path::new(wd);
        if let Some(dir) = wd_path.parent() {
            let xdg = std::env::var_os("XDG_RUNTIME_DIR");
            let needs_update = match &xdg {
                Some(x) => std::path::Path::new(x) != dir,
                None => true,
            };
            if needs_update {
                set(&mut env, "XDG_RUNTIME_DIR", &dir.to_string_lossy());
            }
        }
        // WAYLAND_DISPLAY must be just the socket filename (e.g. "wayland-2"),
        // not a full path.  Clients resolve it under XDG_RUNTIME_DIR.
        let wd_name = wd_path
            .file_name()
            .map(|n| n.to_string_lossy())
            .unwrap_or_else(|| wd.into());
        set(&mut env, "WAYLAND_DISPLAY", &wd_name);
        set(&mut env, "NIXOS_OZONE_WL", "1");
        set(&mut env, "XDG_SESSION_TYPE", "wayland");
        // DISPLAY was already filtered out above.
    }
    if let Some(ps) = pulse_server {
        set(&mut env, "PULSE_SERVER", ps);
    } else {
        // No audio pipeline — point PULSE_SERVER at a path that will make
        // libpulse fail immediately.  Without this, libpulse falls back to
        // autospawn (`pulseaudio --start`) which hangs in headless /
        // container environments.  Setting PULSE_SERVER explicitly also
        // prevents inheriting a host PulseAudio server that would bypass
        // blit's audio pipeline.
        set(&mut env, "PULSE_SERVER", "/dev/null");
    }
    // Set PIPEWIRE_REMOTE so native PipeWire clients (mpv, Firefox, etc.)
    // can connect to our private PipeWire instance.  WirePlumber is running
    // as the session manager and handles linking streams to blit-sink.
    // The path is absolute so it works regardless of the child's
    // XDG_RUNTIME_DIR (which points at the Wayland socket directory).
    if let Some(pr) = pipewire_remote {
        set(&mut env, "PIPEWIRE_REMOTE", pr);
    }
    env.into_iter()
        .filter_map(|(k, v)| CString::new(format!("{k}={v}")).ok())
        .collect()
}

/// Directory holding the running server binary, resolved once.  `None` when the
/// path can't be read or has no usable parent.
fn exe_dir() -> Option<&'static str> {
    static DIR: OnceLock<Option<String>> = OnceLock::new();
    DIR.get_or_init(|| {
        let exe = std::env::current_exe().ok()?;
        Some(exe.parent()?.to_str()?.to_owned())
    })
    .as_deref()
}

/// Resolve a program name to an absolute path by searching $PATH.
/// Called before fork() so the child can use execve (which doesn't search PATH).
fn resolve_in_path(program: &str) -> Option<std::path::PathBuf> {
    if program.contains('/') {
        return Some(std::path::PathBuf::from(program));
    }
    let path_var = std::env::var("PATH").unwrap_or_default();
    for dir in path_var.split(':') {
        let candidate = std::path::Path::new(dir).join(program);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

/// Close all file descriptors >= `from` except those in the `keep` set.
/// Called in the child after fork() to prevent leaking parent fds (IPC
/// listener, other PTY masters, epoll fd, compositor fds, etc.).
///
/// Only uses async-signal-safe libc calls — no heap allocation, no Rust
/// stdlib — because the child inherits locked allocator mutexes from
/// other threads that no longer exist after fork().
unsafe fn close_fds_except(from: libc::c_int, keep: &[libc::c_int]) {
    let max_fd = unsafe { libc::sysconf(libc::_SC_OPEN_MAX) } as libc::c_int;
    let max_fd = if max_fd <= 0 { 4096 } else { max_fd };
    for fd in from..max_fd {
        if !keep.contains(&fd) {
            unsafe { libc::close(fd) };
        }
    }
}

pub type PtyWriteTarget = libc::c_int;

pub struct PtyHandle {
    pub(crate) master_fd: libc::c_int,
    pub(crate) child_pid: libc::pid_t,
}

pub fn pty_write_all(fd: PtyWriteTarget, mut data: &[u8]) {
    while !data.is_empty() {
        let ret = unsafe { libc::write(fd, data.as_ptr().cast(), data.len()) };
        if ret > 0 {
            data = &data[ret as usize..];
        } else if ret < 0 {
            let err = std::io::Error::last_os_error();
            if err.kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            break;
        } else {
            break;
        }
    }
}

pub fn pty_lflag(handle: &PtyHandle) -> (bool, bool) {
    unsafe {
        let mut termios: libc::termios = std::mem::zeroed();
        if libc::tcgetattr(handle.master_fd, &mut termios) == 0 {
            (
                termios.c_lflag & libc::ECHO != 0,
                termios.c_lflag & libc::ICANON != 0,
            )
        } else {
            (false, false)
        }
    }
}

pub fn pty_cwd(handle: &PtyHandle) -> Option<String> {
    let pid = handle.child_pid;
    #[cfg(target_os = "linux")]
    {
        std::fs::read_link(format!("/proc/{pid}/cwd"))
            .ok()
            .and_then(|p| p.into_os_string().into_string().ok())
    }
    #[cfg(target_os = "macos")]
    {
        use std::ffi::CStr;
        let mut buf = vec![0u8; libc::PROC_PIDPATHINFO_MAXSIZE as usize];
        let ret = unsafe {
            libc::proc_pidinfo(
                pid,
                libc::PROC_PIDVNODEPATHINFO,
                0,
                buf.as_mut_ptr() as *mut libc::c_void,
                std::mem::size_of::<libc::proc_vnodepathinfo>() as i32,
            )
        };
        if ret <= 0 {
            return None;
        }
        let info = unsafe { &*(buf.as_ptr() as *const libc::proc_vnodepathinfo) };
        let cstr =
            unsafe { CStr::from_ptr(info.pvi_cdir.vip_path.as_ptr() as *const libc::c_char) };
        cstr.to_str().ok().map(|s| s.to_owned())
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        let _ = pid;
        None
    }
}

fn set_qos_user_interactive() {
    #[cfg(target_os = "macos")]
    {
        const QOS_CLASS_USER_INTERACTIVE: libc::c_uint = 0x21;
        unsafe extern "C" {
            fn pthread_set_qos_class_self_np(
                qos_class: libc::c_uint,
                relative_priority: libc::c_int,
            ) -> libc::c_int;
        }
        unsafe {
            pthread_set_qos_class_self_np(QOS_CLASS_USER_INTERACTIVE, 0);
        }
    }
}

pub fn resize_pty_os(handle: &PtyHandle, rows: u16, cols: u16) {
    unsafe {
        let ws = libc::winsize {
            ws_row: rows,
            ws_col: cols,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        libc::ioctl(handle.master_fd, libc::TIOCSWINSZ, &ws);
        let mut fg_pgid: libc::pid_t = 0;
        libc::ioctl(handle.master_fd, libc::TIOCGPGRP, &mut fg_pgid);
        if fg_pgid > 0 {
            libc::kill(-fg_pgid, libc::SIGWINCH);
        }
        libc::kill(-handle.child_pid, libc::SIGWINCH);
    }
}

/// Signal a PTY's child.
///
/// `group` sends to process groups rather than to the session leader alone.
/// Every blit child is a `setsid()` session leader (see `spawn_pty`), so its
/// pgid equals its pid and `kill(-pid)` is valid with no extra bookkeeping.
/// That reaches the leader's own group; a shell puts each job in a *separate*
/// group, so the foreground job is signalled through `TIOCGPGRP` the same way
/// `resize_pty_os` delivers `SIGWINCH`.  Backgrounded jobs in neither group
/// still survive — bounding those needs a cgroup, not a signal.
///
/// Leader-only remains available because `SIGINT`-to-the-leader is what a
/// caller wants when emulating a keystroke, not a tree-wide interrupt.
pub fn kill_pty(handle: &PtyHandle, signal: i32, group: bool) {
    unsafe {
        if !group {
            libc::kill(handle.child_pid, signal);
            return;
        }
        let mut fg_pgid: libc::pid_t = 0;
        libc::ioctl(handle.master_fd, libc::TIOCGPGRP, &mut fg_pgid);
        if fg_pgid > 0 && fg_pgid != handle.child_pid {
            libc::kill(-fg_pgid, signal);
        }
        libc::kill(-handle.child_pid, signal);
    }
}

/// Hang up a PTY: `SIGHUP` the child's group, then drop the master.
///
/// Closing the master alone makes the kernel hang up the terminal, but only
/// processes still attached to it notice.  A grandchild that redirected away
/// from the tty keeps running and keeps the slave open, which is why the
/// signal goes to the group first.
pub fn close_pty(handle: &PtyHandle) {
    kill_pty(handle, libc::SIGHUP, true);
    unsafe {
        libc::close(handle.master_fd);
    }
}

pub fn collect_exit_status(handle: &PtyHandle) -> i32 {
    // Take reaped_statuses before deregistering, matching reap_zombies'
    // reaped-then-pty_pids order: the backstop locks reaped first, so holding
    // it here excludes the backstop across the deregister and our waitpid.
    // Deregistering first (outside this lock) would let the backstop reap the
    // child — seeing it absent from pty_pids, it drops the status on the floor.
    let mut reaped = reaped_statuses().lock().unwrap();
    pty_pids().lock().unwrap().remove(&handle.child_pid);
    if let Some(status) = reaped.remove(&handle.child_pid) {
        return status;
    }
    unsafe {
        let mut wstatus: libc::c_int = 0;
        if libc::waitpid(handle.child_pid, &mut wstatus, libc::WNOHANG) > 0 {
            return status_from_wstatus(wstatus);
        }
    }
    blit_remote::EXIT_STATUS_UNKNOWN
}

/// Has this child exited?  Non-blocking, and it parks the status so the
/// `cleanup_pty_internal` that follows still reports the real exit code.
///
/// This is what decouples exit detection from EOF on the master fd.  A
/// grandchild that keeps the slave open means the master never reaches EOF,
/// so a child could exit with the terminal stuck in `running` forever; the
/// supervisor polls this instead, woken by SIGCHLD.
pub fn poll_child_exited(handle: &PtyHandle) -> bool {
    let mut reaped = reaped_statuses().lock().unwrap();
    if reaped.contains_key(&handle.child_pid) {
        return true;
    }
    // Same lock order as reap_zombies and collect_exit_status: reaped first,
    // then pty_pids, so a concurrent reaper cannot take the status between
    // the check and the park.
    let owned = pty_pids().lock().unwrap();
    if !owned.contains(&handle.child_pid) {
        // Already collected — the caller has its status.
        return false;
    }
    let mut wstatus: libc::c_int = 0;
    let pid = unsafe { libc::waitpid(handle.child_pid, &mut wstatus, libc::WNOHANG) };
    if pid > 0 {
        reaped.insert(pid, status_from_wstatus(wstatus));
        return true;
    }
    false
}

/// Drop a pid from the owned set without collecting a status, for the close
/// path that kills a live child and never asks what it exited with.  Without
/// this the pid stays owned forever and the backstop parks a status nobody
/// drains.
pub fn forget_pty_pid(handle: &PtyHandle) {
    let mut reaped = reaped_statuses().lock().unwrap();
    pty_pids().lock().unwrap().remove(&handle.child_pid);
    reaped.remove(&handle.child_pid);
}

/// Is reaping unowned children this process's job?
///
/// Only as PID 1.  A PTY grandchild that outlives its parent reparents to
/// init, and if that is us, nobody else will ever wait for it.  Elsewhere an
/// unowned child of this process belongs to a subsystem that reaps it itself,
/// and taking its status is the theft this reaper used to commit.
///
/// A nested `PR_SET_CHILD_SUBREAPER` ancestor would also collect orphans, but
/// blit never sets that on itself and nothing in the tree arranges it, so the
/// PID check is the whole realistic surface.
fn adopts_orphans() -> bool {
    unsafe { libc::getpid() == 1 }
}

pub fn reap_zombies() {
    // Backstop reaper, targeted at pids this module owns.
    //
    // It used to drain `waitpid(-1)` unconditionally and discard anything
    // foreign, which reaped other subsystems' children out from under them:
    // the audio pipeline's own `try_wait` would find the status already
    // taken, and a language server's engine likewise.  The supervisor reaps
    // PTY children promptly off SIGCHLD; this stays as a slow sweep so a
    // missed wakeup cannot leave one a zombie.
    let mut reaped = reaped_statuses().lock().unwrap();
    let owned = pty_pids().lock().unwrap();
    for &pid in owned.iter() {
        let mut wstatus: libc::c_int = 0;
        if unsafe { libc::waitpid(pid, &mut wstatus, libc::WNOHANG) } > 0 {
            reaped.insert(pid, status_from_wstatus(wstatus));
        }
    }
    if !adopts_orphans() {
        return;
    }
    // Running as init (a container entrypoint, say).  Escaped grandchildren
    // reparent here and nothing else will ever collect them, so drain what is
    // left.  This is the old unconditional behaviour, now scoped to the one
    // case where it is correct rather than merely harmful-and-tolerated.
    loop {
        let mut wstatus: libc::c_int = 0;
        let pid = unsafe { libc::waitpid(-1, &mut wstatus, libc::WNOHANG) };
        if pid <= 0 {
            break;
        }
        if owned.contains(&pid) {
            reaped.insert(pid, status_from_wstatus(wstatus));
        }
    }
}

/// Statuses reaped by `reap_zombies` before the owning PTY collected them;
/// drained by `collect_exit_status`, so it stays near-empty in the usual path.
fn reaped_statuses() -> &'static Mutex<HashMap<libc::pid_t, i32>> {
    static REAPED: OnceLock<Mutex<HashMap<libc::pid_t, i32>>> = OnceLock::new();
    REAPED.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Live PTY child pids, so the backstop parks statuses only for
/// children this module owns. A PTY registers on spawn and deregisters
/// when its exit status is collected.
fn pty_pids() -> &'static Mutex<std::collections::HashSet<libc::pid_t>> {
    static PIDS: OnceLock<Mutex<std::collections::HashSet<libc::pid_t>>> = OnceLock::new();
    PIDS.get_or_init(|| Mutex::new(std::collections::HashSet::new()))
}

/// Register a pid as a live PTY child (backstop-parkable).
pub(crate) fn register_pty_pid(pid: libc::pid_t) {
    pty_pids().lock().unwrap().insert(pid);
}

/// WEXITSTATUS on normal exit, negated signal if signalled, else UNKNOWN.
fn status_from_wstatus(wstatus: libc::c_int) -> i32 {
    if libc::WIFEXITED(wstatus) {
        libc::WEXITSTATUS(wstatus)
    } else if libc::WIFSIGNALED(wstatus) {
        -(libc::WTERMSIG(wstatus) as i32)
    } else {
        blit_remote::EXIT_STATUS_UNKNOWN
    }
}

/// Answer terminal queries found in `data`; returns the last OSC 7
/// working-directory report seen in the chunk, if any (docs/protocol.md,
/// "Working directory tracking").
pub fn respond_to_queries(
    handle: &PtyHandle,
    data: &[u8],
    size: (u16, u16),
    cursor: (u16, u16),
) -> Option<String> {
    let scan = crate::parse_terminal_queries(data, size, cursor);
    for resp in scan.responses {
        pty_write_all(handle.master_fd, resp.as_bytes());
    }
    scan.osc7_cwd
}

pub fn pty_reader(fd: PtyWriteTarget, tx: mpsc::Sender<PtyInput>, notify: Arc<Notify>) {
    unsafe {
        let flags = libc::fcntl(fd, libc::F_GETFL);
        libc::fcntl(fd, libc::F_SETFL, flags & !libc::O_NONBLOCK);
    }

    let mut buf = vec![0u8; 64 * 1024];
    let mut sync_scan_tail = Vec::new();

    loop {
        let n = unsafe { libc::read(fd, buf.as_mut_ptr().cast(), buf.len()) };
        if n > 0 {
            let data = buf[..n as usize].to_vec();
            let mut remaining = data;
            loop {
                if remaining.is_empty() {
                    break;
                }
                if let Some(boundary) = crate::find_sync_output_end(&sync_scan_tail, &remaining) {
                    let before = remaining[..boundary].to_vec();
                    let after = remaining[boundary..].to_vec();
                    crate::update_sync_scan_tail(&mut sync_scan_tail, &before);
                    if tx.blocking_send(PtyInput::SyncBoundary { before }).is_err() {
                        return;
                    }
                    notify.notify_one();
                    remaining = after;
                } else {
                    crate::update_sync_scan_tail(&mut sync_scan_tail, &remaining);
                    if tx.blocking_send(PtyInput::Data(remaining)).is_err() {
                        return;
                    }
                    notify.notify_one();
                    break;
                }
            }
        } else {
            let _ = tx.blocking_send(PtyInput::Eof);
            notify.notify_one();
            return;
        }
    }
}

#[allow(clippy::too_many_arguments)]
pub fn spawn_pty(
    shell: &str,
    shell_flags: &str,
    rows: u16,
    cols: u16,
    id: u16,
    tag: &str,
    command: Option<&str>,
    argv: Option<&[&str]>,
    dir: Option<&str>,
    scrollback: usize,
    state: AppState,
    wayland_display: Option<&str>,
    pulse_server: Option<&str>,
    pipewire_remote: Option<&str>,
) -> Option<crate::Pty> {
    let mut master: libc::c_int = 0;
    let mut slave: libc::c_int = 0;
    unsafe {
        if libc::openpty(
            &mut master,
            &mut slave,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        ) != 0
        {
            eprintln!("openpty failed for pty {id}");
            return None;
        }
        let ws = libc::winsize {
            ws_row: rows,
            ws_col: cols,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        libc::ioctl(master, libc::TIOCSWINSZ, &ws);
    }

    // Build the child's environment before fork() to avoid calling
    // set_var/remove_var after fork in a multi-threaded process (UB per POSIX).
    let blit_sock = state
        .config
        .export_sock
        .then(|| state.config.ipc_path.as_str());
    let path_dir = state.config.inject_path.then(exe_dir).flatten();
    let child_env = build_child_env(
        wayland_display,
        pulse_server,
        pipewire_remote,
        blit_sock,
        path_dir,
    );
    let child_envp: Vec<*const libc::c_char> = child_env
        .iter()
        .map(|c| c.as_ptr())
        .chain(std::iter::once(std::ptr::null()))
        .collect();
    // Resolve the shell path before fork (execve doesn't search PATH).
    let shell_path = resolve_in_path(shell);

    let pid = unsafe { libc::fork() };
    if pid < 0 {
        eprintln!("fork failed for pty {id}");
        unsafe {
            libc::close(master);
            libc::close(slave);
        }
        return None;
    }

    if pid == 0 {
        unsafe {
            libc::close(master);
            libc::setsid();
            libc::ioctl(slave, libc::TIOCSCTTY as _, 0);
            libc::dup2(slave, 0);
            libc::dup2(slave, 1);
            libc::dup2(slave, 2);
            if slave > 2 {
                libc::close(slave);
            }
            // Close all inherited parent fds (IPC listener, other PTY masters,
            // epoll fd, compositor fds, etc.) to prevent the child from
            // accessing other sessions or accepting new connections.
            close_fds_except(3, &[]);
            // Reset SIGPIPE to default — the Rust runtime sets it to SIG_IGN,
            // and child programs that rely on SIGPIPE (e.g. piped commands)
            // would get EPIPE errors instead of being killed.
            libc::signal(libc::SIGPIPE, libc::SIG_DFL);
        }
        set_qos_user_interactive();
        let effective_dir = dir.map(String::from);
        if let Some(d) = effective_dir
            && let Ok(dir_c) = CString::new(d)
        {
            unsafe {
                libc::chdir(dir_c.as_ptr());
            }
        }
        if let Some(command) = command {
            let shell_c = match &shell_path {
                Some(p) => CString::new(p.to_string_lossy().as_ref()).unwrap(),
                None => CString::new(shell).unwrap(),
            };
            let command_c = CString::new(command).unwrap();
            let flag = CString::new(if shell_flags.is_empty() {
                "-c".to_owned()
            } else {
                format!("-{}c", shell_flags)
            })
            .unwrap();
            unsafe {
                let p = shell_c.as_ptr();
                let f = flag.as_ptr();
                let c = command_c.as_ptr();
                libc::execve(p, [p, f, c, std::ptr::null()].as_ptr(), child_envp.as_ptr());
                libc::_exit(1);
            }
        }
        if let Some(args) = argv
            && !args.is_empty()
        {
            let cargs: Vec<CString> = args.iter().map(|s| CString::new(*s).unwrap()).collect();
            // Resolve the first arg (program) via PATH.
            let prog = resolve_in_path(args[0])
                .map(|p| CString::new(p.to_string_lossy().as_ref()).unwrap())
                .unwrap_or_else(|| cargs[0].clone());
            let ptrs: Vec<*const libc::c_char> = std::iter::once(prog.as_ptr())
                .chain(cargs[1..].iter().map(|c| c.as_ptr()))
                .chain(std::iter::once(std::ptr::null()))
                .collect();
            unsafe {
                libc::execve(prog.as_ptr(), ptrs.as_ptr(), child_envp.as_ptr());
                libc::_exit(1);
            }
        }
        let shell_c = match &shell_path {
            Some(p) => CString::new(p.to_string_lossy().as_ref()).unwrap(),
            None => CString::new(shell).unwrap(),
        };
        unsafe {
            if shell_flags.is_empty() {
                let p = shell_c.as_ptr();
                libc::execve(p, [p, std::ptr::null()].as_ptr(), child_envp.as_ptr());
            } else {
                let flag = CString::new(format!("-{}", shell_flags)).unwrap();
                let p = shell_c.as_ptr();
                let f = flag.as_ptr();
                libc::execve(p, [p, f, std::ptr::null()].as_ptr(), child_envp.as_ptr());
            }
            libc::_exit(1);
        }
    }

    unsafe {
        libc::close(slave);
        let flags = libc::fcntl(master, libc::F_GETFL);
        libc::fcntl(master, libc::F_SETFL, flags | libc::O_NONBLOCK);
    }

    state.pty_fds.write().unwrap().insert(id, master);
    let (byte_tx, byte_rx) = mpsc::channel(PTY_CHANNEL_CAPACITY);
    let reader_handle = std::thread::Builder::new()
        .name(format!("pty-reader-{id}"))
        .spawn({
            let notify = state.delivery_notify.clone();
            move || pty_reader(master, byte_tx, notify)
        })
        .expect("failed to spawn pty-reader thread");
    let handle = PtyHandle {
        master_fd: master,
        child_pid: pid,
    };
    register_pty_pid(pid);
    let lflag_cache = pty_lflag(&handle);

    Some(crate::Pty {
        handle,
        driver: Box::new(blit_alacritty::TerminalDriver::new(rows, cols, scrollback)),
        tag: tag.to_owned(),
        dirty: true,
        ready_frames: std::collections::VecDeque::new(),
        byte_rx,
        reader_handle,
        lflag_cache,
        lflag_last: std::time::Instant::now(),
        last_title_send: std::time::Instant::now(),
        title_pending: false,
        last_used_rows_sent: 0,
        exited: false,
        exit_status: blit_remote::EXIT_STATUS_UNKNOWN,
        command: command.map(|s| s.to_owned()),
        cwd: dir.map(|s| s.to_owned()),
        osc7_cwd: None,
    })
}

#[allow(clippy::too_many_arguments)]
pub fn respawn_child(
    shell: &str,
    shell_flags: &str,
    rows: u16,
    cols: u16,
    pty_id: u16,
    command: Option<&str>,
    dir: Option<&str>,
    state: AppState,
    wayland_display: Option<&str>,
    pulse_server: Option<&str>,
    pipewire_remote: Option<&str>,
) -> Option<(
    PtyHandle,
    std::thread::JoinHandle<()>,
    mpsc::Receiver<PtyInput>,
)> {
    let mut master: libc::c_int = 0;
    let mut slave: libc::c_int = 0;
    unsafe {
        if libc::openpty(
            &mut master,
            &mut slave,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        ) != 0
        {
            return None;
        }
        let ws = libc::winsize {
            ws_row: rows,
            ws_col: cols,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        libc::ioctl(master, libc::TIOCSWINSZ, &ws);
    }

    // Build the child's environment before fork() (same rationale as spawn_pty).
    let blit_sock = state
        .config
        .export_sock
        .then(|| state.config.ipc_path.as_str());
    let path_dir = state.config.inject_path.then(exe_dir).flatten();
    let child_env = build_child_env(
        wayland_display,
        pulse_server,
        pipewire_remote,
        blit_sock,
        path_dir,
    );
    let child_envp: Vec<*const libc::c_char> = child_env
        .iter()
        .map(|c| c.as_ptr())
        .chain(std::iter::once(std::ptr::null()))
        .collect();
    let shell_path = resolve_in_path(shell);

    let pid = unsafe { libc::fork() };
    if pid < 0 {
        unsafe {
            libc::close(master);
            libc::close(slave);
        }
        return None;
    }
    if pid == 0 {
        unsafe {
            libc::close(master);
            libc::setsid();
            libc::ioctl(slave, libc::TIOCSCTTY as _, 0);
            libc::dup2(slave, 0);
            libc::dup2(slave, 1);
            libc::dup2(slave, 2);
            if slave > 2 {
                libc::close(slave);
            }
            close_fds_except(3, &[]);
            libc::signal(libc::SIGPIPE, libc::SIG_DFL);
        }
        set_qos_user_interactive();
        if let Some(d) = dir
            && let Ok(dir_c) = CString::new(d)
        {
            unsafe {
                libc::chdir(dir_c.as_ptr());
            }
        }
        if let Some(cmd) = command {
            let shell_c = match &shell_path {
                Some(p) => CString::new(p.to_string_lossy().as_ref()).unwrap(),
                None => CString::new(shell).unwrap(),
            };
            let flag = CString::new(if shell_flags.is_empty() {
                "-c".to_owned()
            } else {
                format!("-{}c", shell_flags)
            })
            .unwrap();
            let cmd_c = CString::new(cmd).unwrap();
            unsafe {
                libc::execve(
                    shell_c.as_ptr(),
                    [
                        shell_c.as_ptr(),
                        flag.as_ptr(),
                        cmd_c.as_ptr(),
                        std::ptr::null(),
                    ]
                    .as_ptr(),
                    child_envp.as_ptr(),
                );
                libc::_exit(1);
            }
        }
        let shell_c = match &shell_path {
            Some(p) => CString::new(p.to_string_lossy().as_ref()).unwrap(),
            None => CString::new(shell).unwrap(),
        };
        unsafe {
            if shell_flags.is_empty() {
                let p = shell_c.as_ptr();
                libc::execve(p, [p, std::ptr::null()].as_ptr(), child_envp.as_ptr());
            } else {
                let flag = CString::new(format!("-{}", shell_flags)).unwrap();
                let p = shell_c.as_ptr();
                let f = flag.as_ptr();
                libc::execve(p, [p, f, std::ptr::null()].as_ptr(), child_envp.as_ptr());
            }
            libc::_exit(1);
        }
    }

    unsafe {
        libc::close(slave);
        let flags = libc::fcntl(master, libc::F_GETFL);
        libc::fcntl(master, libc::F_SETFL, flags | libc::O_NONBLOCK);
    }

    state.pty_fds.write().unwrap().insert(pty_id, master);
    let (byte_tx, byte_rx) = mpsc::channel(PTY_CHANNEL_CAPACITY);
    let reader_handle = std::thread::Builder::new()
        .name(format!("pty-reader-{pty_id}"))
        .spawn({
            let notify = state.delivery_notify.clone();
            move || pty_reader(master, byte_tx, notify)
        })
        .expect("failed to spawn pty-reader thread");
    let handle = PtyHandle {
        master_fd: master,
        child_pid: pid,
    };
    register_pty_pid(pid);
    Some((handle, reader_handle, byte_rx))
}

#[cfg(test)]
mod tests {
    use super::{PtyHandle, build_child_env, collect_exit_status, reap_zombies};
    use std::collections::HashMap;

    /// Block until `pid` exits but leave it unreaped (`WNOWAIT`), so the reaper
    /// under test still finds a zombie to consume.
    fn wait_until_zombie(pid: libc::pid_t) {
        let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
        let ret = unsafe {
            libc::waitid(
                libc::P_PID,
                pid as libc::id_t,
                &mut info,
                libc::WEXITED | libc::WNOWAIT,
            )
        };
        assert_eq!(ret, 0, "waitid(WNOWAIT) failed");
    }

    /// The reap_zombies backstop reaps a PTY child before collect_exit_status
    /// runs; collect_exit_status must still report the child's real code (42),
    /// not UNKNOWN (which the client renders as a bogus exit 1).
    #[test]
    fn collect_exit_status_survives_backstop_reap() {
        let pid = unsafe { libc::fork() };
        assert!(pid >= 0, "fork failed");
        if pid == 0 {
            unsafe { libc::_exit(42) };
        }

        // The backstop parks statuses only for registered PTY children.
        super::register_pty_pid(pid);
        wait_until_zombie(pid);
        reap_zombies();

        let handle = PtyHandle {
            master_fd: -1,
            child_pid: pid,
        };
        assert_eq!(collect_exit_status(&handle), 42);
    }

    /// Fork a session leader that forks a child of its own, both parked in
    /// `pause()`.  Returns (leader, grandchild).  Mirrors the shape that
    /// matters in practice: a shell with a running command under it.
    fn fork_leader_with_child() -> (libc::pid_t, libc::pid_t) {
        let mut fds = [0; 2];
        assert_eq!(unsafe { libc::pipe(fds.as_mut_ptr()) }, 0, "pipe failed");
        let leader = unsafe { libc::fork() };
        assert!(leader >= 0, "fork failed");
        if leader == 0 {
            unsafe {
                libc::close(fds[0]);
                libc::setsid();
                let grandchild = libc::fork();
                if grandchild == 0 {
                    loop {
                        libc::pause();
                    }
                }
                // Hand the grandchild's pid back and park.
                let bytes = (grandchild as i32).to_le_bytes();
                libc::write(fds[1], bytes.as_ptr().cast(), 4);
                loop {
                    libc::pause();
                }
            }
        }
        unsafe { libc::close(fds[1]) };
        let mut buf = [0u8; 4];
        let n = unsafe { libc::read(fds[0], buf.as_mut_ptr().cast(), 4) };
        assert_eq!(n, 4, "did not receive grandchild pid");
        unsafe { libc::close(fds[0]) };
        (leader, i32::from_le_bytes(buf))
    }

    fn is_alive(pid: libc::pid_t) -> bool {
        // Signal 0 probes without delivering.  A zombie still answers, so
        // reap first at the call sites that care.
        unsafe { libc::kill(pid, 0) == 0 }
    }

    /// Poll until `pid` is unreachable.  The grandchild is not this process's
    /// child, so `waitid` answers ECHILD for it — once its parent dies it is
    /// reparented and reaped by the subreaper, and probing is the only thing
    /// left that works.
    fn wait_until_gone(pid: libc::pid_t) {
        for _ in 0..500 {
            if !is_alive(pid) {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        panic!("pid {pid} still alive after 5s");
    }

    /// The bug this replaced: `kill(pid, sig)` reached the session leader and
    /// nothing else, so killing a shell left its children running.
    #[test]
    fn leader_only_kill_spares_the_child() {
        let (leader, grandchild) = fork_leader_with_child();
        let handle = PtyHandle {
            master_fd: -1,
            child_pid: leader,
        };

        super::kill_pty(&handle, libc::SIGKILL, false);
        wait_until_zombie(leader);
        assert!(
            is_alive(grandchild),
            "leader-only kill should not reach the child"
        );

        unsafe {
            libc::kill(grandchild, libc::SIGKILL);
            libc::waitpid(leader, std::ptr::null_mut(), 0);
        }
    }

    #[test]
    fn group_kill_reaches_the_child() {
        let (leader, grandchild) = fork_leader_with_child();
        let handle = PtyHandle {
            master_fd: -1,
            child_pid: leader,
        };

        super::kill_pty(&handle, libc::SIGKILL, true);
        wait_until_zombie(leader);
        unsafe {
            libc::waitpid(leader, std::ptr::null_mut(), 0);
        }
        wait_until_gone(grandchild);
    }

    /// Exit detection must not depend on the master fd reaching EOF: a
    /// grandchild holding the slave open keeps a dead terminal marked
    /// running forever.  `poll_child_exited` answers from the child itself.
    #[test]
    fn poll_child_exited_reports_a_dead_child() {
        let pid = unsafe { libc::fork() };
        assert!(pid >= 0, "fork failed");
        if pid == 0 {
            unsafe { libc::_exit(7) };
        }
        super::register_pty_pid(pid);
        let handle = PtyHandle {
            master_fd: -1,
            child_pid: pid,
        };

        wait_until_zombie(pid);
        assert!(super::poll_child_exited(&handle));
        // And the status it parked is still the one the caller gets.
        assert_eq!(collect_exit_status(&handle), 7);
    }

    #[test]
    fn poll_child_exited_is_false_while_the_child_runs() {
        let (leader, grandchild) = fork_leader_with_child();
        super::register_pty_pid(leader);
        let handle = PtyHandle {
            master_fd: -1,
            child_pid: leader,
        };

        assert!(!super::poll_child_exited(&handle));

        super::kill_pty(&handle, libc::SIGKILL, true);
        wait_until_zombie(leader);
        assert!(super::poll_child_exited(&handle));
        unsafe {
            libc::waitpid(leader, std::ptr::null_mut(), 0);
        }
        wait_until_gone(grandchild);
    }

    fn child_env_map(env: Vec<std::ffi::CString>) -> HashMap<String, String> {
        env.into_iter()
            .filter_map(|entry| {
                let entry = entry.into_string().ok()?;
                let (key, value) = entry.split_once('=')?;
                Some((key.to_string(), value.to_string()))
            })
            .collect()
    }

    #[test]
    fn child_env_enables_electron_wayland_when_compositor_is_available() {
        let env = child_env_map(build_child_env(
            Some("/tmp/blit-test/wayland-7"),
            None,
            None,
            None,
            None,
        ));

        assert_eq!(
            env.get("XDG_RUNTIME_DIR").map(String::as_str),
            Some("/tmp/blit-test")
        );
        assert_eq!(
            env.get("WAYLAND_DISPLAY").map(String::as_str),
            Some("wayland-7")
        );
        assert_eq!(env.get("NIXOS_OZONE_WL").map(String::as_str), Some("1"));
        assert_eq!(
            env.get("XDG_SESSION_TYPE").map(String::as_str),
            Some("wayland"),
        );
        assert!(!env.contains_key("DISPLAY"));
    }

    #[test]
    fn child_env_exports_blit_sock_only_when_requested() {
        let env = child_env_map(build_child_env(None, None, None, None, None));
        assert!(!env.contains_key("BLIT_SOCK"));

        let env = child_env_map(build_child_env(
            None,
            None,
            None,
            Some("/tmp/blit-test.sock"),
            None,
        ));
        assert_eq!(
            env.get("BLIT_SOCK").map(String::as_str),
            Some("/tmp/blit-test.sock")
        );
    }

    #[test]
    fn child_env_appends_the_binary_dir_to_path_only_when_requested() {
        let inherited = std::env::var("PATH").unwrap_or_default();

        let env = child_env_map(build_child_env(None, None, None, None, None));
        assert_eq!(
            env.get("PATH").map(String::as_str),
            Some(inherited.as_str())
        );

        let env = child_env_map(build_child_env(
            None,
            None,
            None,
            None,
            Some("/tmp/blit test/bin"),
        ));
        assert_eq!(
            env.get("PATH").map(String::as_str),
            Some(format!("{inherited}:/tmp/blit test/bin").as_str())
        );
    }

    #[test]
    fn child_env_leaves_path_alone_when_the_binary_dir_is_already_on_it() {
        let inherited = std::env::var("PATH").unwrap_or_default();
        let already = inherited.split(':').next_back().unwrap_or_default();

        let env = child_env_map(build_child_env(None, None, None, None, Some(already)));
        assert_eq!(
            env.get("PATH").map(String::as_str),
            Some(inherited.as_str())
        );
    }
}
