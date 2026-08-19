use std::sync::Arc;
use std::time::Instant;
use tokio::sync::{Notify, mpsc};
use windows_sys::Win32::Foundation::{CloseHandle, GetLastError, HANDLE, INVALID_HANDLE_VALUE};
use windows_sys::Win32::Storage::FileSystem::{ReadFile, WriteFile};
use windows_sys::Win32::System::Console::{
    COORD, ClosePseudoConsole, CreatePseudoConsole, HPCON, ResizePseudoConsole,
};
use windows_sys::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
    SetInformationJobObject, TerminateJobObject,
};
use windows_sys::Win32::System::Pipes::CreatePipe;
use windows_sys::Win32::System::Threading::{
    CREATE_SUSPENDED, CreateProcessW, EXTENDED_STARTUPINFO_PRESENT, GetExitCodeProcess,
    InitializeProcThreadAttributeList, LPPROC_THREAD_ATTRIBUTE_LIST,
    PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE, PROCESS_INFORMATION, ResumeThread, STARTUPINFOEXW,
    UpdateProcThreadAttribute, WaitForSingleObject,
};

use crate::{AppState, PTY_CHANNEL_CAPACITY, PtyInput};

#[derive(Clone, Copy)]
pub struct PtyWriteTarget(pub HANDLE);
unsafe impl Send for PtyWriteTarget {}
unsafe impl Sync for PtyWriteTarget {}

pub struct PtyHandle {
    pub(crate) conpty: HPCON,
    pub(crate) process: HANDLE,
    pub(crate) input: HANDLE,
    pub(crate) output: HANDLE,
    /// Job object owning the child and everything it spawns, so a kill
    /// reaches the tree instead of orphaning it.  Windows has no process
    /// group to signal, and `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE` gives the
    /// containment `SIGHUP`-to-the-group gives on Unix: dropping this handle
    /// takes the tree with it.  Null when the job could not be created — the
    /// PTY still works, it just degrades to a leader-only kill.
    pub(crate) job: HANDLE,
}

unsafe impl Send for PtyHandle {}
unsafe impl Sync for PtyHandle {}

pub fn pty_write_all(handle: PtyWriteTarget, mut data: &[u8]) {
    while !data.is_empty() {
        let mut written: u32 = 0;
        let ok = unsafe {
            WriteFile(
                handle.0,
                data.as_ptr(),
                data.len() as u32,
                &mut written,
                std::ptr::null_mut(),
            )
        };
        if ok == 0 || written == 0 {
            break;
        }
        data = &data[written as usize..];
    }
}

pub fn pty_lflag(_handle: &PtyHandle) -> (bool, bool) {
    (false, false)
}

pub fn pty_cwd(_handle: &PtyHandle) -> Option<String> {
    None
}

pub fn resize_pty_os(handle: &PtyHandle, rows: u16, cols: u16) {
    let size = COORD {
        X: cols as i16,
        Y: rows as i16,
    };
    unsafe {
        ResizePseudoConsole(handle.conpty, size);
    }
}

/// Signal a PTY's child.  See the Unix twin for what `group` means; here it
/// selects `TerminateJobObject` over `TerminateProcess`, which is the whole
/// difference between killing the tree and orphaning it.
///
/// `SIGINT` stays a `^C` written into the ConPTY regardless: that is a
/// console-input event the foreground program handles, and there is no
/// job-wide equivalent.
pub fn kill_pty(handle: &PtyHandle, signal: i32, group: bool) {
    match signal {
        2 => pty_write_all(PtyWriteTarget(handle.input), b"\x03"),
        _ => unsafe {
            if group && !handle.job.is_null() {
                TerminateJobObject(handle.job, 1);
            } else {
                windows_sys::Win32::System::Threading::TerminateProcess(handle.process, 1);
            }
        },
    }
}

/// Tear down the console, the pipes and the job.
///
/// Deliberately leaves `process` open: the caller reads the exit code through
/// it immediately afterwards, and `GetExitCodeProcess` on a closed handle
/// returns nothing useful — or, once the handle value is recycled, somebody
/// else's exit code.  `collect_exit_status` closes it, and `abandon_pty_pid`
/// covers the path that never asks for a status.
pub fn close_pty(handle: &PtyHandle) {
    unsafe {
        ClosePseudoConsole(handle.conpty);
        CloseHandle(handle.input);
        CloseHandle(handle.output);
        // Last handle to the job, so KILL_ON_JOB_CLOSE fires here and takes
        // any surviving descendant with it.
        if !handle.job.is_null() {
            CloseHandle(handle.job);
        }
    }
}

/// Read the child's exit code and release the process handle.
///
/// Mirrors the Unix twin's contract: calling this is what says "this child is
/// finished with", so it is also where the last reference goes away.
pub fn collect_exit_status(handle: &PtyHandle) -> i32 {
    const STILL_ACTIVE: u32 = 259;
    unsafe {
        // Bounded, not blocking: this runs under the session mutex, and the
        // supervisor only calls it once `poll_child_exited` has already said
        // the process is done.  The wait is a safety net for the EOF path,
        // which can beat the kernel to the exit code by a hair.
        WaitForSingleObject(handle.process, 50);
        let mut exit_code: u32 = 0;
        let status = if GetExitCodeProcess(handle.process, &mut exit_code) != 0
            && exit_code != STILL_ACTIVE
        {
            exit_code as i32
        } else {
            blit_remote::EXIT_STATUS_UNKNOWN
        };
        CloseHandle(handle.process);
        status
    }
}

/// See the Unix twin.  Windows has no SIGCHLD, so the supervisor polls this
/// on its own cadence instead of being woken.
pub fn poll_child_exited(handle: &PtyHandle) -> bool {
    const WAIT_OBJECT_0: u32 = 0;
    unsafe { WaitForSingleObject(handle.process, 0) == WAIT_OBJECT_0 }
}

/// Give up on a child's exit status.  No zombies here — the process object
/// goes away once the last handle closes — so this is just that close, the
/// counterpart to the Unix wait.
///
/// `kill_group_at` is ignored: `close_pty` already dropped the last handle to
/// the kill-on-close job, so the tree is gone by the time this runs and there
/// is nothing left to escalate against.
pub fn abandon_pty_pid(handle: &PtyHandle, _kill_group_at: Instant) {
    unsafe {
        CloseHandle(handle.process);
    }
}

/// No-op: see [`abandon_pty_pid`].  The job object makes the hangup the kill.
pub fn escalate_abandoned(_now: Instant) {}

/// Always `None`: nothing here ever needs escalating, so the supervisor has no
/// timer to arm.
pub fn next_abandoned_kill() -> Option<Instant> {
    None
}

pub fn reap_zombies() {}

/// Create an anonymous job object that kills its members when the last handle
/// to it closes.  Returns null on failure; callers degrade to a leader-only
/// kill rather than refusing to spawn, since a PTY without tree containment
/// is still a working PTY.
///
/// Nested jobs are fine on Windows 8+, so this works even when the server
/// itself was launched inside a job by a service manager or CI runner.
fn create_kill_on_close_job() -> HANDLE {
    unsafe {
        let job = CreateJobObjectW(std::ptr::null(), std::ptr::null());
        if job.is_null() {
            return std::ptr::null_mut();
        }
        let mut limits: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
        limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        let ok = SetInformationJobObject(
            job,
            JobObjectExtendedLimitInformation,
            (&raw const limits).cast(),
            std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
        );
        if ok == 0 {
            CloseHandle(job);
            return std::ptr::null_mut();
        }
        job
    }
}

/// Put a `CREATE_SUSPENDED` child into `job` and let it run.  Returns the job
/// actually in force — null when there is none, so the caller stores that and
/// `kill_pty` falls back to terminating the leader.
///
/// The child must be created suspended: assigning it after it has started
/// races against anything it spawns in the meantime, and those grandchildren
/// would land outside the job.
///
/// A failed assignment has to null the handle, not just log: an empty job
/// still looks live to `kill_pty`, which would then route every kill —
/// including the deadline's SIGKILL equivalent — to `TerminateJobObject` on a
/// job containing nothing, leaving a terminal that cannot be killed at all.
fn adopt_into_job(job: HANDLE, pi: &PROCESS_INFORMATION) -> HANDLE {
    unsafe {
        let mut job = job;
        if !job.is_null() && AssignProcessToJobObject(job, pi.hProcess) == 0 {
            let err = GetLastError();
            eprintln!(
                "blit-server: job assignment failed (error {err}); kill will not reach the process tree"
            );
            CloseHandle(job);
            job = std::ptr::null_mut();
        }
        ResumeThread(pi.hThread);
        job
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
        pty_write_all(PtyWriteTarget(handle.input), resp.as_bytes());
    }
    scan
}

pub(crate) struct SendHandle(pub(crate) HANDLE);
unsafe impl Send for SendHandle {}

pub(crate) fn pty_reader(handle: SendHandle, tx: mpsc::Sender<PtyInput>, notify: Arc<Notify>) {
    let handle = handle.0;
    let mut buf = vec![0u8; 64 * 1024];
    let mut sync_scan_tail = Vec::new();

    loop {
        let mut bytes_read: u32 = 0;
        let ok = unsafe {
            ReadFile(
                handle,
                buf.as_mut_ptr(),
                buf.len() as u32,
                &mut bytes_read,
                std::ptr::null_mut(),
            )
        };
        if ok == 0 || bytes_read == 0 {
            let _ = tx.blocking_send(PtyInput::Eof);
            notify.notify_one();
            return;
        }
        let data = buf[..bytes_read as usize].to_vec();
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
    }
}

fn create_pipe_pair() -> Option<(HANDLE, HANDLE)> {
    let mut read_handle: HANDLE = INVALID_HANDLE_VALUE;
    let mut write_handle: HANDLE = INVALID_HANDLE_VALUE;
    let ok = unsafe { CreatePipe(&mut read_handle, &mut write_handle, std::ptr::null(), 0) };
    if ok == 0 {
        return None;
    }
    Some((read_handle, write_handle))
}

fn to_wide(s: &str) -> Vec<u16> {
    use std::os::windows::ffi::OsStrExt;
    std::ffi::OsStr::new(s)
        .encode_wide()
        .chain(std::iter::once(0))
        .collect()
}

fn build_command_line(shell: &str, shell_flags: &str, command: Option<&str>) -> Vec<u16> {
    let shell_lower = shell.to_ascii_lowercase();
    let is_cmd = shell_lower.ends_with("cmd.exe") || shell_lower.ends_with("cmd");
    let cmd = if let Some(command) = command {
        if is_cmd {
            format!("{shell} /c {command}")
        } else if shell_flags.is_empty() {
            format!("{shell} -c {command}")
        } else {
            format!("{shell} -{shell_flags}c {command}")
        }
    } else if shell_flags.is_empty() {
        shell.to_string()
    } else {
        format!("{shell} -{shell_flags}")
    };
    to_wide(&cmd)
}

/// What to run in a terminal, and where.
///
/// The pseudoconsole takes a command *line*, so `argv` and `env` are carried
/// for signature parity with the Unix path and cannot be honored here — which
/// is why `FEATURE_CREATE_EXEC` is not advertised on this platform and the
/// create path refuses a request that sets either.
#[derive(Clone, Copy, Debug, Default)]
pub struct ChildSpec<'a> {
    pub command: Option<&'a str>,
    pub argv: Option<&'a [&'a str]>,
    pub dir: Option<&'a str>,
    pub env: &'a [(String, String)],
}

/// [`ChildSpec`] with owned strings, held by a `Pty` so a restart can replay
/// the same child.
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
    _wayland_display: Option<&str>,
    _x_display: Option<&str>,
    _desktop_bus: Option<&str>,
    _pulse_server: Option<&str>,
    _pipewire_remote: Option<&str>,
) -> Option<crate::Pty> {
    let command = spec.command;
    let dir = spec.dir;
    let (input_read, input_write) = create_pipe_pair()?;
    let (output_read, output_write) = create_pipe_pair()?;

    let size = COORD {
        X: cols as i16,
        Y: rows as i16,
    };
    let mut conpty: HPCON = 0;
    let hr = unsafe { CreatePseudoConsole(size, input_read, output_write, 0, &mut conpty) };
    if hr != 0 {
        unsafe {
            CloseHandle(input_read);
            CloseHandle(input_write);
            CloseHandle(output_read);
            CloseHandle(output_write);
        }
        eprintln!("CreatePseudoConsole failed for pty {id}: HRESULT 0x{hr:08x}");
        return None;
    }

    let mut attr_list_size: usize = 0;
    unsafe {
        InitializeProcThreadAttributeList(std::ptr::null_mut(), 1, 0, &mut attr_list_size);
    }
    let mut attr_list_buf = vec![0u8; attr_list_size];
    let attr_list = attr_list_buf.as_mut_ptr() as LPPROC_THREAD_ATTRIBUTE_LIST;
    unsafe {
        if InitializeProcThreadAttributeList(attr_list, 1, 0, &mut attr_list_size) == 0 {
            ClosePseudoConsole(conpty);
            CloseHandle(input_read);
            CloseHandle(input_write);
            CloseHandle(output_read);
            CloseHandle(output_write);
            return None;
        }
        if UpdateProcThreadAttribute(
            attr_list,
            0,
            PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE as usize,
            conpty as *mut _,
            std::mem::size_of::<HPCON>(),
            std::ptr::null_mut(),
            std::ptr::null(),
        ) == 0
        {
            ClosePseudoConsole(conpty);
            CloseHandle(input_read);
            CloseHandle(input_write);
            CloseHandle(output_read);
            CloseHandle(output_write);
            return None;
        }
    }

    let mut cmd_line = build_command_line(shell, shell_flags, command);
    let dir_wide = dir.map(|d| to_wide(d));

    let mut si: STARTUPINFOEXW = unsafe { std::mem::zeroed() };
    si.StartupInfo.cb = std::mem::size_of::<STARTUPINFOEXW>() as u32;
    si.lpAttributeList = attr_list;

    let job = create_kill_on_close_job();
    let mut pi: PROCESS_INFORMATION = unsafe { std::mem::zeroed() };
    let ok = unsafe {
        CreateProcessW(
            std::ptr::null(),
            cmd_line.as_mut_ptr(),
            std::ptr::null(),
            std::ptr::null(),
            0,
            EXTENDED_STARTUPINFO_PRESENT | CREATE_SUSPENDED,
            std::ptr::null(),
            dir_wide
                .as_ref()
                .map(|d| d.as_ptr())
                .unwrap_or(std::ptr::null()),
            &si.StartupInfo,
            &mut pi,
        )
    };

    unsafe {
        CloseHandle(input_read);
        CloseHandle(output_write);
    }

    if ok == 0 {
        let err = unsafe { GetLastError() };
        eprintln!("CreateProcessW failed for pty {id}: error {err}");
        unsafe {
            ClosePseudoConsole(conpty);
            CloseHandle(input_write);
            CloseHandle(output_read);
            if !job.is_null() {
                CloseHandle(job);
            }
        }
        return None;
    }

    let job = adopt_into_job(job, &pi);

    unsafe {
        CloseHandle(pi.hThread);
    }

    let handle = PtyHandle {
        conpty,
        process: pi.hProcess,
        input: input_write,
        output: output_read,
        job,
    };

    state
        .pty_fds
        .write()
        .unwrap()
        .insert(id, PtyWriteTarget(handle.input));
    let (byte_tx, byte_rx) = mpsc::channel(PTY_CHANNEL_CAPACITY);
    let reader_output = SendHandle(handle.output);
    let notify = state.delivery_notify.clone();
    let reader_handle = std::thread::Builder::new()
        .name(format!("pty-reader-{id}"))
        .spawn(move || pty_reader(reader_output, byte_tx, notify))
        .expect("failed to spawn pty-reader thread");
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

pub fn respawn_child(
    shell: &str,
    shell_flags: &str,
    rows: u16,
    cols: u16,
    pty_id: u16,
    spec: ChildSpec<'_>,
    state: AppState,
    _wayland_display: Option<&str>,
    _x_display: Option<&str>,
    _desktop_bus: Option<&str>,
    _pulse_server: Option<&str>,
    _pipewire_remote: Option<&str>,
) -> Option<(
    PtyHandle,
    std::thread::JoinHandle<()>,
    mpsc::Receiver<PtyInput>,
)> {
    let command = spec.command;
    let dir = spec.dir;
    let (input_read, input_write) = create_pipe_pair()?;
    let (output_read, output_write) = create_pipe_pair()?;

    let size = COORD {
        X: cols as i16,
        Y: rows as i16,
    };
    let mut conpty: HPCON = 0;
    let hr = unsafe { CreatePseudoConsole(size, input_read, output_write, 0, &mut conpty) };
    if hr != 0 {
        unsafe {
            CloseHandle(input_read);
            CloseHandle(input_write);
            CloseHandle(output_read);
            CloseHandle(output_write);
        }
        return None;
    }

    let mut attr_list_size: usize = 0;
    unsafe {
        InitializeProcThreadAttributeList(std::ptr::null_mut(), 1, 0, &mut attr_list_size);
    }
    let mut attr_list_buf = vec![0u8; attr_list_size];
    let attr_list = attr_list_buf.as_mut_ptr() as LPPROC_THREAD_ATTRIBUTE_LIST;
    unsafe {
        if InitializeProcThreadAttributeList(attr_list, 1, 0, &mut attr_list_size) == 0 {
            ClosePseudoConsole(conpty);
            CloseHandle(input_read);
            CloseHandle(input_write);
            CloseHandle(output_read);
            CloseHandle(output_write);
            return None;
        }
        if UpdateProcThreadAttribute(
            attr_list,
            0,
            PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE as usize,
            conpty as *mut _,
            std::mem::size_of::<HPCON>(),
            std::ptr::null_mut(),
            std::ptr::null(),
        ) == 0
        {
            ClosePseudoConsole(conpty);
            CloseHandle(input_read);
            CloseHandle(input_write);
            CloseHandle(output_read);
            CloseHandle(output_write);
            return None;
        }
    }

    let mut cmd_line = build_command_line(shell, shell_flags, command);
    let mut si: STARTUPINFOEXW = unsafe { std::mem::zeroed() };
    si.StartupInfo.cb = std::mem::size_of::<STARTUPINFOEXW>() as u32;
    si.lpAttributeList = attr_list;

    let job = create_kill_on_close_job();
    let mut pi: PROCESS_INFORMATION = unsafe { std::mem::zeroed() };
    let ok = unsafe {
        CreateProcessW(
            std::ptr::null(),
            cmd_line.as_mut_ptr(),
            std::ptr::null(),
            std::ptr::null(),
            0,
            EXTENDED_STARTUPINFO_PRESENT | CREATE_SUSPENDED,
            std::ptr::null(),
            std::ptr::null(),
            &si.StartupInfo,
            &mut pi,
        )
    };

    unsafe {
        CloseHandle(input_read);
        CloseHandle(output_write);
    }

    if ok == 0 {
        unsafe {
            ClosePseudoConsole(conpty);
            CloseHandle(input_write);
            CloseHandle(output_read);
            if !job.is_null() {
                CloseHandle(job);
            }
        }
        return None;
    }

    let job = adopt_into_job(job, &pi);

    unsafe {
        CloseHandle(pi.hThread);
    }

    let handle = PtyHandle {
        conpty,
        process: pi.hProcess,
        input: input_write,
        output: output_read,
        job,
    };

    state
        .pty_fds
        .write()
        .unwrap()
        .insert(pty_id, PtyWriteTarget(handle.input));
    let (byte_tx, byte_rx) = mpsc::channel(PTY_CHANNEL_CAPACITY);
    let reader_output = SendHandle(handle.output);
    let notify = state.delivery_notify.clone();
    let reader_handle = std::thread::Builder::new()
        .name(format!("pty-reader-{pty_id}"))
        .spawn(move || pty_reader(reader_output, byte_tx, notify))
        .expect("failed to spawn pty-reader thread");
    Some((handle, reader_handle, byte_rx))
}
