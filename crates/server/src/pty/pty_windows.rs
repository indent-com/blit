use std::sync::Arc;
use tokio::sync::{Notify, mpsc};
use windows_sys::Win32::Foundation::{CloseHandle, GetLastError, HANDLE, INVALID_HANDLE_VALUE};
use windows_sys::Win32::Storage::FileSystem::{ReadFile, WriteFile};
use windows_sys::Win32::System::Console::{
    COORD, ClosePseudoConsole, CreatePseudoConsole, HPCON, ResizePseudoConsole,
};
use windows_sys::Win32::System::Pipes::CreatePipe;
use windows_sys::Win32::System::Threading::{
    CreateProcessW, EXTENDED_STARTUPINFO_PRESENT, GetExitCodeProcess,
    InitializeProcThreadAttributeList, LPPROC_THREAD_ATTRIBUTE_LIST,
    PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE, PROCESS_INFORMATION, STARTUPINFOEXW,
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

pub fn kill_pty(handle: &PtyHandle, signal: i32) {
    match signal {
        2 => pty_write_all(PtyWriteTarget(handle.input), b"\x03"),
        _ => unsafe {
            windows_sys::Win32::System::Threading::TerminateProcess(handle.process, 1);
        },
    }
}

pub fn close_pty(handle: &PtyHandle) {
    unsafe {
        ClosePseudoConsole(handle.conpty);
        CloseHandle(handle.input);
        CloseHandle(handle.output);
        CloseHandle(handle.process);
    }
}

pub fn collect_exit_status(handle: &PtyHandle) -> i32 {
    const STILL_ACTIVE: u32 = 259;
    unsafe {
        WaitForSingleObject(handle.process, 1000);
        let mut exit_code: u32 = 0;
        if GetExitCodeProcess(handle.process, &mut exit_code) != 0 && exit_code != STILL_ACTIVE {
            exit_code as i32
        } else {
            blit_remote::EXIT_STATUS_UNKNOWN
        }
    }
}

pub fn reap_zombies() {}

pub fn respond_to_queries(handle: &PtyHandle, data: &[u8], size: (u16, u16), cursor: (u16, u16)) {
    for resp in crate::parse_terminal_queries(data, size, cursor) {
        pty_write_all(PtyWriteTarget(handle.input), resp.as_bytes());
    }
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
                if tx
                    .blocking_send(PtyInput::SyncBoundary {
                        before,
                        after: after.clone(),
                    })
                    .is_err()
                {
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

#[allow(clippy::too_many_arguments)]
pub fn spawn_pty(
    shell: &str,
    shell_flags: &str,
    rows: u16,
    cols: u16,
    id: u16,
    tag: &str,
    command: Option<&str>,
    _argv: Option<&[&str]>,
    dir: Option<&str>,
    scrollback: usize,
    state: AppState,
    _wayland_display: Option<&str>,
) -> Option<crate::Pty> {
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

    let mut pi: PROCESS_INFORMATION = unsafe { std::mem::zeroed() };
    let ok = unsafe {
        CreateProcessW(
            std::ptr::null(),
            cmd_line.as_mut_ptr(),
            std::ptr::null(),
            std::ptr::null(),
            0,
            EXTENDED_STARTUPINFO_PRESENT,
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
        }
        return None;
    }

    unsafe {
        CloseHandle(pi.hThread);
    }

    let handle = PtyHandle {
        conpty,
        process: pi.hProcess,
        input: input_write,
        output: output_read,
    };

    state
        .2
        .write()
        .unwrap()
        .insert(id, PtyWriteTarget(handle.input));
    let (byte_tx, byte_rx) = mpsc::channel(PTY_CHANNEL_CAPACITY);
    let reader_output = SendHandle(handle.output);
    let notify = state.3.clone();
    let reader_handle = std::thread::spawn(move || pty_reader(reader_output, byte_tx, notify));
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
        exited: false,
        exit_status: blit_remote::EXIT_STATUS_UNKNOWN,
        command: command.map(|s| s.to_owned()),
    })
}

pub fn respawn_child(
    shell: &str,
    shell_flags: &str,
    rows: u16,
    cols: u16,
    pty_id: u16,
    command: Option<&str>,
    state: AppState,
    _wayland_display: Option<&str>,
) -> Option<(
    PtyHandle,
    std::thread::JoinHandle<()>,
    mpsc::Receiver<PtyInput>,
)> {
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

    let mut pi: PROCESS_INFORMATION = unsafe { std::mem::zeroed() };
    let ok = unsafe {
        CreateProcessW(
            std::ptr::null(),
            cmd_line.as_mut_ptr(),
            std::ptr::null(),
            std::ptr::null(),
            0,
            EXTENDED_STARTUPINFO_PRESENT,
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
        }
        return None;
    }

    unsafe {
        CloseHandle(pi.hThread);
    }

    let handle = PtyHandle {
        conpty,
        process: pi.hProcess,
        input: input_write,
        output: output_read,
    };

    state
        .2
        .write()
        .unwrap()
        .insert(pty_id, PtyWriteTarget(handle.input));
    let (byte_tx, byte_rx) = mpsc::channel(PTY_CHANNEL_CAPACITY);
    let reader_output = SendHandle(handle.output);
    let notify = state.3.clone();
    let reader_handle = std::thread::spawn(move || pty_reader(reader_output, byte_tx, notify));
    Some((handle, reader_handle, byte_rx))
}
