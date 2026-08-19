use std::collections::HashMap;
use std::ffi::CString;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Instant;
use tokio::sync::{Notify, mpsc};

use crate::{AppState, PTY_CHANNEL_CAPACITY, PtyInput};

/// What to run in a terminal, and where.
///
/// `command` and `argv` are mutually exclusive: a command is handed to the
/// login shell, an argv is exec'd directly. Both are absent for a plain shell.
/// The owned counterpart, [`OwnedChildSpec`], is what a `Pty` keeps so a
/// restart can replay the same child rather than degrading to a bare shell.
#[derive(Clone, Copy, Debug, Default)]
pub struct ChildSpec<'a> {
    /// Run through `$SHELL -<flags>c`.
    pub command: Option<&'a str>,
    /// Exec directly, no shell.
    pub argv: Option<&'a [&'a str]>,
    pub dir: Option<&'a str>,
    /// Environment overrides, applied after everything the server derives.
    pub env: &'a [(String, String)],
}

/// [`ChildSpec`] with owned strings, held by a `Pty` for the lifetime of the
/// terminal so `C2S_RESTART` re-runs what was actually started.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct OwnedChildSpec {
    pub command: Option<String>,
    pub argv: Option<Vec<String>>,
    pub dir: Option<String>,
    pub env: Vec<(String, String)>,
}

impl OwnedChildSpec {
    pub fn borrowed<'a>(&'a self, argv: &'a [&'a str]) -> ChildSpec<'a> {
        ChildSpec {
            command: self.command.as_deref(),
            argv: self.argv.is_some().then_some(argv),
            dir: self.dir.as_deref(),
            env: &self.env,
        }
    }

    /// The `&[&str]` backing store `borrowed` needs, since `Vec<String>` and
    /// `&[&str]` have no shared layout.
    pub fn argv_refs(&self) -> Vec<&str> {
        self.argv
            .as_deref()
            .map(|args| args.iter().map(String::as_str).collect())
            .unwrap_or_default()
    }
}

impl ChildSpec<'_> {
    pub fn to_owned_spec(self) -> OwnedChildSpec {
        OwnedChildSpec {
            command: self.command.map(str::to_owned),
            argv: self
                .argv
                .map(|args| args.iter().map(|a| (*a).to_owned()).collect()),
            dir: self.dir.map(str::to_owned),
            env: self.env.to_vec(),
        }
    }
}

/// Build the environment array for a child process before fork().
/// This avoids calling std::env::set_var/remove_var after fork() in a
/// multi-threaded process (which is UB per POSIX — those functions are
/// not async-signal-safe).
///
/// `overrides` are applied **dead last**, after the inherit filter, after the
/// terminal and `BLIT_*` rewrites, and after the session environment — so a
/// client entry always wins, whichever layer would otherwise have set the key.
/// This is the precedence the process family already documents for
/// `PROCESS_SPAWN` (`command_for` in `process.rs`).
#[allow(clippy::too_many_arguments)]
fn build_child_env(
    wayland_display: Option<&str>,
    x_display: Option<&str>,
    desktop_bus: Option<&str>,
    pulse_server: Option<&str>,
    pipewire_remote: Option<&str>,
    blit_sock: Option<&str>,
    path_dir: Option<&str>,
    overrides: &[(String, String)],
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
    // The session half — compositor socket, toolkit steering, desktop bus, and
    // audio sockets — is shared with native `PROCESS_SPAWN_SESSION_ENV` children
    // so both routes reach the same display. Its `remove` list is already
    // covered by the filter above, which drops the same host-session variables
    // before they ever enter `env`.
    let session = crate::app_env::session_env(
        wayland_display,
        x_display,
        desktop_bus,
        pulse_server,
        pipewire_remote,
    );
    for (key, value) in &session.set {
        set(&mut env, key, value);
    }
    // Last word to the client, over every layer above — including the `BLIT_*`
    // filter and the exported socket, which a caller may legitimately want to
    // point somewhere else.
    for (key, value) in overrides {
        set(&mut env, key, value);
    }
    env.into_iter()
        .filter_map(|(k, v)| CString::new(format!("{k}={v}")).ok())
        .collect()
}

/// Everything the child needs to `execve`, built entirely before `fork()`.
///
/// Nothing here may be deferred to the child: after fork in a multi-threaded
/// process only async-signal-safe calls are legal, and every allocation risks
/// an allocator mutex some dead thread still holds. That includes the
/// `CString`s — a NUL in a client-supplied argument must fail *here*, not as a
/// panic on the wrong side of the fork.
struct ExecPlan {
    /// Kept alive because `ptrs` borrows their interiors.
    _argv: Vec<CString>,
    program: CString,
    ptrs: Vec<*const libc::c_char>,
}

impl ExecPlan {
    /// `program` is what runs; `args` is the child's whole argv, argv[0]
    /// included. The two are allowed to disagree, and both callers make them:
    /// `program` is resolved against the child's own `PATH`, while argv[0]
    /// stays as it was written — the client's word for it, or the shell's
    /// name — so `ps` and busybox-style dispatch see the request rather than
    /// the path it resolved to.
    fn new(program: &std::path::Path, args: &[&str]) -> Option<Self> {
        let program = CString::new(program.as_os_str().as_encoded_bytes()).ok()?;
        let argv: Vec<CString> = args
            .iter()
            .map(|arg| CString::new(*arg).ok())
            .collect::<Option<_>>()?;
        let ptrs = argv
            .iter()
            .map(|arg| arg.as_ptr())
            .chain(std::iter::once(std::ptr::null()))
            .collect();
        Some(Self {
            _argv: argv,
            program,
            ptrs,
        })
    }

    /// Only `execve` and `_exit` run after this; both are async-signal-safe.
    unsafe fn exec(&self, envp: &[*const libc::c_char]) -> ! {
        unsafe {
            libc::execve(self.program.as_ptr(), self.ptrs.as_ptr(), envp.as_ptr());
            libc::_exit(1);
        }
    }
}

/// Resolve and lay out the child's `execve` arguments before forking.
///
/// `env` is the child's own environment, so `PATH` lookup honors an override
/// the caller asked for rather than silently using the server's.
fn plan_exec(
    spec: &ChildSpec<'_>,
    shell: &str,
    shell_flags: &str,
    env: &[CString],
) -> Option<ExecPlan> {
    if let Some(argv) = spec.argv.filter(|argv| !argv.is_empty()) {
        let program = resolve_in_path(argv[0], child_path(env).as_deref())?;
        return ExecPlan::new(&program, argv);
    }
    let program = resolve_in_path(shell, child_path(env).as_deref())
        .unwrap_or_else(|| std::path::PathBuf::from(shell));
    let flag = match (spec.command, shell_flags) {
        (Some(_), "") => Some("-c".to_owned()),
        (Some(_), flags) => Some(format!("-{flags}c")),
        (None, "") => None,
        (None, flags) => Some(format!("-{flags}")),
    };
    let mut args: Vec<&str> = vec![shell];
    if let Some(flag) = &flag {
        args.push(flag);
    }
    if let Some(command) = spec.command {
        args.push(command);
    }
    ExecPlan::new(&program, &args)
}

/// The `PATH` the child will actually run with, read back out of its own
/// prepared environment.
fn child_path(env: &[CString]) -> Option<String> {
    env.iter().find_map(|entry| {
        entry
            .to_str()
            .ok()
            .and_then(|entry| entry.strip_prefix("PATH="))
            .map(str::to_owned)
    })
}

/// Write a diagnostic to the terminal and terminate the child.
///
/// Runs after fork, so it is restricted to `write` and `_exit`. The message
/// reaches the pty, which is the only place a person is looking.
unsafe fn child_fail(what: &[u8], detail: &[u8]) -> ! {
    unsafe {
        for part in [b"blit: " as &[u8], what, detail, b"\r\n"] {
            libc::write(2, part.as_ptr().cast(), part.len());
        }
        libc::_exit(1);
    }
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

/// Resolve a program name to an absolute path by searching `$PATH`.
/// Called before fork() so the child can use execve (which doesn't search PATH).
///
/// `path` is the child's own `PATH` when the caller has one — an override that
/// changes where a program comes from has to change where we look for it, or
/// the terminal runs a different binary than the same command would in a shell.
/// Falls back to the server's.
fn resolve_in_path(program: &str, path: Option<&str>) -> Option<std::path::PathBuf> {
    if program.contains('/') {
        return Some(std::path::PathBuf::from(program));
    }
    let owned;
    let path_var = match path {
        Some(path) => path,
        None => {
            owned = std::env::var("PATH").unwrap_or_default();
            &owned
        }
    };
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

/// Give up on a child's exit status without giving up on reaping it, and
/// schedule the `SIGKILL` that finishes the job if the `SIGHUP` did not.
///
/// `C2S_CLOSE` hangs a live child up and never reports what it exited with,
/// so nothing will ever call `collect_exit_status` for it.  It still has to be
/// waited: the `SIGHUP` only asks it to die, and an unwaited child is a
/// zombie for the life of the server.  Moving the pid here keeps
/// `reap_zombies` sweeping it — discarding the status rather than parking it —
/// until the wait succeeds and the registration goes away for good.
///
/// `kill_group_at` is when [`escalate_abandoned`] stops asking.  Reaping is
/// what cancels it: a pid the kernel has taken back may already name somebody
/// else's process group, and this is the one place that signals a group by
/// number with no handle left to check it against.
pub fn abandon_pty_pid(handle: &PtyHandle, kill_group_at: Instant) {
    let mut reaped = reaped_statuses().lock().unwrap();
    pty_pids().lock().unwrap().remove(&handle.child_pid);
    reaped.remove(&handle.child_pid);
    abandoned_pids()
        .lock()
        .unwrap()
        .insert(handle.child_pid, Some(kill_group_at));
}

/// `SIGKILL` the process groups whose hangup grace has run out.
///
/// The second half of the stop sequence `C2S_CLOSE` starts: `SIGHUP` to the
/// group, wait `TimeoutStopSec`, then this.  A child that ignores `SIGHUP`, or
/// one whose descendants redirected away from the terminal, gets no say in
/// this one.
pub fn escalate_abandoned(now: Instant) {
    let mut pids = abandoned_pids().lock().unwrap();
    for (&pid, kill_at) in pids.iter_mut() {
        if kill_at.is_some_and(|at| now >= at) {
            *kill_at = None;
            unsafe {
                libc::kill(-pid, libc::SIGKILL);
            }
        }
    }
}

/// When [`escalate_abandoned`] next has work, so the supervisor sleeps until
/// exactly then instead of finding out on its backstop sweep.
pub fn next_abandoned_kill() -> Option<Instant> {
    abandoned_pids()
        .lock()
        .unwrap()
        .values()
        .flatten()
        .min()
        .copied()
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
    // Children hung up by `C2S_CLOSE`.  Nobody wants their status, but they
    // still have to be waited or they stay zombies — a server cycling
    // terminals would march to RLIMIT_NPROC.  Drop each registration only
    // once the wait succeeds, so a child still winding down is retried; that
    // is also what disarms a pending group `SIGKILL`, since the pid stops
    // being ours to signal the moment the kernel takes it back.
    let mut abandoned = abandoned_pids().lock().unwrap();
    abandoned
        .retain(|&pid, _| unsafe { libc::waitpid(pid, std::ptr::null_mut(), libc::WNOHANG) } <= 0);
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
        // This drain is indiscriminate, so it can collect an abandoned child
        // out from under its own escalation.  Forget the pid with the status:
        // signalling a group whose number the kernel has already recycled is
        // worse than letting a survivor run.
        abandoned.remove(&pid);
    }
}

/// Statuses reaped by `reap_zombies` before the owning PTY collected them;
/// drained by `collect_exit_status`, so it stays near-empty in the usual path.
fn reaped_statuses() -> &'static Mutex<HashMap<libc::pid_t, i32>> {
    static REAPED: OnceLock<Mutex<HashMap<libc::pid_t, i32>>> = OnceLock::new();
    REAPED.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Children hung up by `C2S_CLOSE`, each mapped to the instant its process
/// group gets `SIGKILL` — `None` once that has gone out, or once the child was
/// reaped before the grace elapsed.  Emptied by `reap_zombies` as each one
/// dies.
fn abandoned_pids() -> &'static Mutex<HashMap<libc::pid_t, Option<Instant>>> {
    static PIDS: OnceLock<Mutex<HashMap<libc::pid_t, Option<Instant>>>> = OnceLock::new();
    PIDS.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Live PTY child pids, so the backstop parks statuses only for
/// children this module owns. A PTY registers on spawn and deregisters
/// when its exit status is collected.
fn pty_pids() -> &'static Mutex<std::collections::HashSet<libc::pid_t>> {
    static PIDS: OnceLock<Mutex<std::collections::HashSet<libc::pid_t>>> = OnceLock::new();
    PIDS.get_or_init(|| Mutex::new(std::collections::HashSet::new()))
}

/// Serialize every raw fork with `std::process::Command` spawning.
///
/// `Command::spawn` waits for EOF on a private exec-error pipe. A concurrent
/// raw fork can inherit that pipe and keep it open indefinitely (PTY children
/// do substantial setup before exec, and test children may never exec), which
/// makes the spawning thread hang. All server fork sites use this lock.
fn child_spawn_lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

/// Fork while excluding `Command::spawn` error-pipe creation.
///
/// The child deliberately leaks its copied guard: unlocking a pthread mutex
/// after fork is not async-signal-safe, and exec/_exit will discard it.
pub(crate) fn fork_child() -> libc::pid_t {
    let guard = child_spawn_lock().lock().unwrap();
    let pid = unsafe { libc::fork() };
    if pid == 0 {
        std::mem::forget(guard);
    }
    pid
}

/// Register a pid as a live PTY child (backstop-parkable).
pub(crate) fn register_pty_pid(pid: libc::pid_t) {
    pty_pids().lock().unwrap().insert(pid);
}

/// Spawn and register a non-PTY child while excluding the server backstop.
///
/// The PID-1 orphan sweep uses `waitpid(-1)`. Holding the registry locks over
/// OS child creation and insertion prevents it from observing a freshly
/// spawned child before that child has an owner and discarding its status.
pub(crate) fn spawn_registered_child<T, E>(
    spawn: impl FnOnce() -> Result<(libc::pid_t, T), E>,
) -> Result<T, E> {
    let _spawn = child_spawn_lock().lock().unwrap();
    let _reaped = reaped_statuses().lock().unwrap();
    let mut owned = pty_pids().lock().unwrap();
    let (pid, child) = spawn()?;
    owned.insert(pid);
    Ok(child)
}

/// Forget a registered child after its owner collected the status itself.
pub(crate) fn deregister_child_pid(pid: libc::pid_t) {
    let mut reaped = reaped_statuses().lock().unwrap();
    pty_pids().lock().unwrap().remove(&pid);
    reaped.remove(&pid);
}

/// Recover a status parked by the server backstop, deregistering the child.
///
/// Returns the same convention as PTYs: non-negative exit code, negative
/// signal, or `None` when the backstop did not collect this child.
pub(crate) fn take_reaped_child_status(pid: libc::pid_t) -> Option<i32> {
    let mut reaped = reaped_statuses().lock().unwrap();
    pty_pids().lock().unwrap().remove(&pid);
    reaped.remove(&pid)
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
) -> crate::TerminalScan {
    let mut scan = crate::parse_terminal_queries(data, size, cursor);
    for resp in std::mem::take(&mut scan.responses) {
        pty_write_all(handle.master_fd, resp.as_bytes());
    }
    scan
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

/// Spawn a terminal.
///
/// `list_command` is what `S2C_LIST` will show for this terminal; the caller
/// renders it, because the same string has to clear the catalog's size guard
/// before the id is allocated.
#[allow(clippy::too_many_arguments)]
pub fn spawn_pty(
    shell: &str,
    shell_flags: &str,
    rows: u16,
    cols: u16,
    id: u16,
    tag: &str,
    spec: ChildSpec<'_>,
    list_command: Option<&str>,
    scrollback: usize,
    state: AppState,
    wayland_display: Option<&str>,
    x_display: Option<&str>,
    desktop_bus: Option<&str>,
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
        x_display,
        desktop_bus,
        pulse_server,
        pipewire_remote,
        blit_sock,
        path_dir,
        spec.env,
    );
    let child_envp: Vec<*const libc::c_char> = child_env
        .iter()
        .map(|c| c.as_ptr())
        .chain(std::iter::once(std::ptr::null()))
        .collect();
    // Resolve the program and lay out its argv before fork: execve does not
    // search PATH, and neither the allocation nor a NUL-check may happen on
    // the child's side of the fork.
    let Some(plan) = plan_exec(&spec, shell, shell_flags, &child_env) else {
        eprintln!("cannot resolve a program to run for pty {id}");
        unsafe {
            libc::close(master);
            libc::close(slave);
        }
        return None;
    };
    let dir_c = match spec.dir.map(CString::new) {
        Some(Ok(dir)) => Some(dir),
        None => None,
        Some(Err(_)) => {
            eprintln!("working directory for pty {id} contains a NUL");
            unsafe {
                libc::close(master);
                libc::close(slave);
            }
            return None;
        }
    };

    let pid = fork_child();
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
        // A working directory that cannot be entered used to be ignored, which
        // left the child running somewhere the client never asked for and had
        // no way to notice. Say so on the terminal and stop.
        if let Some(dir_c) = &dir_c
            && unsafe { libc::chdir(dir_c.as_ptr()) } != 0
        {
            unsafe { child_fail(b"cannot enter working directory: ", dir_c.as_bytes()) };
        }
        unsafe { plan.exec(&child_envp) }
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
        snapshot_not_before: None,
        snapshot_by: None,
        ready_frames: std::collections::VecDeque::new(),
        byte_rx,
        reader_handle,
        lflag_cache,
        lflag_last: std::time::Instant::now(),
        last_title_send: std::time::Instant::now(),
        title_pending: false,
        last_used_rows_sent: 0,
        last_scrolled_lines: 0,
        deadline: None,
        stop_deadline: None,
        exit_drain_deadline: None,
        exit_reason: blit_remote::EXIT_REASON_NORMAL,
        exited: false,
        exited_at: None,
        generation: 0,
        exit_status: blit_remote::EXIT_STATUS_UNKNOWN,
        command: list_command.map(str::to_owned),
        spec: spec.to_owned_spec(),
        osc7_cwd: None,
        journal: crate::journal::CommandJournal::default(),
        osc_carry: Vec::new(),
    })
}

#[allow(clippy::too_many_arguments)]
pub fn respawn_child(
    shell: &str,
    shell_flags: &str,
    rows: u16,
    cols: u16,
    pty_id: u16,
    spec: ChildSpec<'_>,
    state: AppState,
    wayland_display: Option<&str>,
    x_display: Option<&str>,
    desktop_bus: Option<&str>,
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
        x_display,
        desktop_bus,
        pulse_server,
        pipewire_remote,
        blit_sock,
        path_dir,
        spec.env,
    );
    let child_envp: Vec<*const libc::c_char> = child_env
        .iter()
        .map(|c| c.as_ptr())
        .chain(std::iter::once(std::ptr::null()))
        .collect();
    let plan = plan_exec(&spec, shell, shell_flags, &child_env)?;
    let dir_c = match spec.dir.map(CString::new) {
        Some(Ok(dir)) => Some(dir),
        None => None,
        Some(Err(_)) => return None,
    };

    let pid = fork_child();
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
        if let Some(dir_c) = &dir_c
            && unsafe { libc::chdir(dir_c.as_ptr()) } != 0
        {
            unsafe { child_fail(b"cannot enter working directory: ", dir_c.as_bytes()) };
        }
        unsafe { plan.exec(&child_envp) }
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
    use super::{
        ChildSpec, PtyHandle, build_child_env, child_path, collect_exit_status, plan_exec,
        reap_zombies, resolve_in_path,
    };
    use std::collections::HashMap;
    use std::ffi::CString;
    use std::time::{Duration, Instant};

    /// `build_child_env` with no client overrides — the shape every test that
    /// predates them expects.
    #[allow(clippy::too_many_arguments)]
    fn session_child_env(
        wayland_display: Option<&str>,
        x_display: Option<&str>,
        desktop_bus: Option<&str>,
        pulse_server: Option<&str>,
        pipewire_remote: Option<&str>,
        blit_sock: Option<&str>,
        path_dir: Option<&str>,
    ) -> Vec<CString> {
        build_child_env(
            wayland_display,
            x_display,
            desktop_bus,
            pulse_server,
            pipewire_remote,
            blit_sock,
            path_dir,
            &[],
        )
    }

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
        let pid = super::fork_child();
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
        fork_leader_with_child_ignoring(&[])
    }

    /// As [`fork_leader_with_child`], with `signals` set to `SIG_IGN` before
    /// the inner fork so the grandchild inherits the disposition too — the
    /// shape `trap "" HUP` leaves behind, and the reason a hangup needs
    /// escalating.
    fn fork_leader_with_child_ignoring(signals: &[libc::c_int]) -> (libc::pid_t, libc::pid_t) {
        let mut fds = [0; 2];
        assert_eq!(unsafe { libc::pipe(fds.as_mut_ptr()) }, 0, "pipe failed");
        let leader = super::fork_child();
        assert!(leader >= 0, "fork failed");
        if leader == 0 {
            unsafe {
                libc::close(fds[0]);
                libc::setsid();
                for &sig in signals {
                    libc::signal(sig, libc::SIG_IGN);
                }
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

    /// SIGKILL a forked group when the test ends, pass or fail.
    ///
    /// A failed assertion leaves the paused children holding the harness's
    /// captured stdout open, so without this a regression hangs the whole run
    /// instead of failing one test.
    struct KillGroupOnDrop(libc::pid_t);

    impl Drop for KillGroupOnDrop {
        fn drop(&mut self) {
            unsafe {
                libc::kill(-self.0, libc::SIGKILL);
                libc::waitpid(self.0, std::ptr::null_mut(), 0);
            }
        }
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
        let pid = super::fork_child();
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

    /// `C2S_CLOSE` hangs a live child up and never asks its status.  It still
    /// has to be waited, or every close leaves a zombie for the life of the
    /// server and a terminal-cycling client marches to RLIMIT_NPROC.
    #[test]
    fn abandoned_children_are_still_reaped() {
        let pid = super::fork_child();
        assert!(pid >= 0, "fork failed");
        if pid == 0 {
            unsafe { libc::_exit(0) };
        }
        super::register_pty_pid(pid);
        let handle = PtyHandle {
            master_fd: -1,
            child_pid: pid,
        };

        // The close path: no status will ever be collected for this child.
        // The grace is far off, so this is the plain hangup-worked case.
        super::abandon_pty_pid(&handle, Instant::now() + Duration::from_secs(3600));
        wait_until_zombie(pid);
        reap_zombies();

        // Reaped, so no longer waitable — a still-zombie child would return
        // its pid here instead of -1.
        let ret = unsafe { libc::waitpid(pid, std::ptr::null_mut(), libc::WNOHANG) };
        assert_eq!(ret, -1, "child was left unreaped");

        // And the pending escalation went with it: the pid is the kernel's to
        // hand out again, so nothing may signal that group afterwards.
        assert!(
            !super::abandoned_pids().lock().unwrap().contains_key(&pid),
            "reaping left an armed escalation on a released pid"
        );
    }

    /// The half `C2S_CLOSE` was missing: SIGHUP is a request, and a child that
    /// ignores it — with a grandchild that inherited the disposition — has to
    /// be gone anyway once `TimeoutStopSec` elapses.
    #[test]
    fn hangup_escalates_to_a_group_sigkill() {
        let (leader, grandchild) = fork_leader_with_child_ignoring(&[libc::SIGHUP]);
        let _cleanup = KillGroupOnDrop(leader);
        super::register_pty_pid(leader);
        let handle = PtyHandle {
            master_fd: -1,
            child_pid: leader,
        };

        // What the close path does, with a grace that has already run out.
        let due = Instant::now();
        super::kill_pty(&handle, libc::SIGHUP, true);
        super::abandon_pty_pid(&handle, due);
        assert!(
            is_alive(leader),
            "a child ignoring SIGHUP should survive the hangup"
        );
        assert_eq!(
            super::abandoned_pids().lock().unwrap().get(&leader),
            Some(&Some(due)),
            "the escalation was not armed"
        );

        super::escalate_abandoned(due);
        // The grandchild inherited the ignored SIGHUP and is only reachable
        // through the group, so its death is what says the escalation reached
        // past the leader.  Asserting on the leader instead would race the
        // reaper: any concurrent `reap_zombies` may collect it first.
        wait_until_gone(grandchild);
        reap_zombies();
    }

    /// The grace is a grace: nothing may die before it elapses.
    #[test]
    fn an_undue_escalation_does_not_fire() {
        let (leader, grandchild) = fork_leader_with_child_ignoring(&[libc::SIGHUP]);
        let _cleanup = KillGroupOnDrop(leader);
        let handle = PtyHandle {
            master_fd: -1,
            child_pid: leader,
        };

        let due = Instant::now() + Duration::from_secs(3600);
        super::abandon_pty_pid(&handle, due);
        super::escalate_abandoned(Instant::now());
        assert!(is_alive(leader), "killed before the grace elapsed");
        assert_eq!(
            super::abandoned_pids().lock().unwrap().get(&leader),
            Some(&Some(due)),
            "an undue escalation should stay armed"
        );

        super::escalate_abandoned(due);
        wait_until_gone(grandchild);
        reap_zombies();
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

    fn overrides(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect()
    }

    /// The client's entries are applied after every layer the server derives —
    /// the inherit filter, the terminal rewrites, the exported socket, and the
    /// session environment — so "explicit beats inherited" holds no matter
    /// which layer would otherwise have owned the key.
    #[test]
    fn child_env_overrides_beat_every_layer_the_server_derives() {
        let env = child_env_map(build_child_env(
            Some("/tmp/blit-test/wayland-7"),
            None,
            None,
            None,
            None,
            Some("/tmp/blit-test/ipc.sock"),
            None,
            &overrides(&[
                // A plain addition.
                ("BLIT_PROBE", "hello"),
                // Beats the unconditional terminal rewrite.
                ("TERM", "dumb"),
                // Beats the exported socket.
                ("BLIT_SOCK", "/somewhere/else.sock"),
                // Beats `session_env`'s compositor socket.
                ("WAYLAND_DISPLAY", "wayland-99"),
            ]),
        ));
        assert_eq!(env.get("BLIT_PROBE").map(String::as_str), Some("hello"));
        assert_eq!(env.get("TERM").map(String::as_str), Some("dumb"));
        assert_eq!(
            env.get("BLIT_SOCK").map(String::as_str),
            Some("/somewhere/else.sock")
        );
        assert_eq!(
            env.get("WAYLAND_DISPLAY").map(String::as_str),
            Some("wayland-99")
        );
    }

    /// An override that changes where programs come from has to change where
    /// we look for them, or the terminal runs a different binary than the same
    /// command would in a shell.
    #[test]
    fn path_lookup_follows_the_child_environment() {
        let dir = std::env::temp_dir().join(format!("blit-path-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let program = dir.join("blit-test-probe");
        std::fs::write(&program, b"#!/bin/sh\n").unwrap();

        let env = build_child_env(
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            &overrides(&[("PATH", dir.to_str().unwrap())]),
        );
        assert_eq!(child_path(&env).as_deref(), dir.to_str());
        assert_eq!(
            resolve_in_path("blit-test-probe", child_path(&env).as_deref()),
            Some(program.clone())
        );
        // Without it the server's own PATH answers, and this is not on it.
        assert_eq!(resolve_in_path("blit-test-probe", None), None);

        std::fs::remove_dir_all(&dir).ok();
    }

    /// A NUL cannot survive `execve`. It used to reach a `CString::new().unwrap()`
    /// on the child's side of a `fork` in a multi-threaded process, where a
    /// panic is neither async-signal-safe nor recoverable.
    #[test]
    fn a_nul_in_an_argument_fails_before_the_fork() {
        let argv = ["echo", "a\0b"];
        assert!(
            plan_exec(
                &ChildSpec {
                    argv: Some(&argv),
                    ..Default::default()
                },
                "/bin/sh",
                "",
                &[],
            )
            .is_none()
        );
    }

    /// A program that does not exist is a failure to plan, not a fork that
    /// exits 1 with nothing said.
    #[test]
    fn an_unresolvable_program_fails_before_the_fork() {
        let argv = ["blit-definitely-not-a-program"];
        assert!(
            plan_exec(
                &ChildSpec {
                    argv: Some(&argv),
                    ..Default::default()
                },
                "/bin/sh",
                "",
                &[],
            )
            .is_none()
        );
    }

    #[test]
    fn child_env_enables_electron_wayland_when_compositor_is_available() {
        let env = child_env_map(session_child_env(
            Some("/tmp/blit-test/wayland-7"),
            None,
            None,
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
        // No XWayland stands behind the DISPLAY we drop, so every toolkit a
        // shell might launch has to be aimed at Wayland explicitly.
        assert_eq!(
            env.get("QT_QPA_PLATFORM").map(String::as_str),
            Some("wayland"),
        );
        assert_eq!(env.get("GDK_BACKEND").map(String::as_str), Some("wayland"),);
        assert_eq!(env.get("MOZ_ENABLE_WAYLAND").map(String::as_str), Some("1"));
        assert_eq!(
            env.get("ELECTRON_OZONE_PLATFORM_HINT").map(String::as_str),
            Some("wayland"),
        );
        assert_eq!(
            env.get("SDL_VIDEODRIVER").map(String::as_str),
            Some("wayland"),
        );
        assert!(!env.contains_key("DISPLAY"));
    }

    /// The host's DISPLAY is filtered out unconditionally; only a bridge in
    /// this session puts one back, and an X11 app started from a shell needs
    /// it to run at all.
    #[test]
    fn child_env_exports_display_only_for_a_bridged_session() {
        let env = child_env_map(session_child_env(
            Some("/tmp/blit-test/wayland-7"),
            Some(":20"),
            None,
            None,
            None,
            None,
            None,
        ));
        assert_eq!(env.get("DISPLAY").map(String::as_str), Some(":20"));
        assert_eq!(
            env.get("GDK_BACKEND").map(String::as_str),
            Some("wayland,x11")
        );

        // No compositor, no session to point at: DISPLAY stays gone even
        // when the host had one.
        let env = child_env_map(session_child_env(
            None,
            Some(":20"),
            None,
            None,
            None,
            None,
            None,
        ));
        assert!(!env.contains_key("DISPLAY"));
    }

    #[test]
    fn child_env_uses_the_compositor_scoped_session_bus() {
        let env = child_env_map(session_child_env(
            Some("/tmp/blit-test/wayland-7"),
            None,
            None,
            None,
            None,
            None,
            None,
        ));
        assert!(!env.contains_key("DBUS_SESSION_BUS_ADDRESS"));

        let env = child_env_map(session_child_env(
            Some("/tmp/blit-test/wayland-7"),
            None,
            Some("unix:path=/tmp/blit-test/desktop-bus"),
            None,
            None,
            None,
            None,
        ));
        assert_eq!(
            env.get("DBUS_SESSION_BUS_ADDRESS").map(String::as_str),
            Some("unix:path=/tmp/blit-test/desktop-bus")
        );
    }

    #[test]
    fn child_env_exports_blit_sock_only_when_requested() {
        let env = child_env_map(session_child_env(None, None, None, None, None, None, None));
        assert!(!env.contains_key("BLIT_SOCK"));

        let env = child_env_map(session_child_env(
            None,
            None,
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

        let env = child_env_map(session_child_env(None, None, None, None, None, None, None));
        assert_eq!(
            env.get("PATH").map(String::as_str),
            Some(inherited.as_str())
        );

        let env = child_env_map(session_child_env(
            None,
            None,
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

        let env = child_env_map(session_child_env(
            None,
            None,
            None,
            None,
            None,
            None,
            Some(already),
        ));
        assert_eq!(
            env.get("PATH").map(String::as_str),
            Some(inherited.as_str())
        );
    }
}
