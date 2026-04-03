use std::ffi::CString;
use std::sync::Arc;
use tokio::sync::{Notify, mpsc};

use crate::{AppState, PTY_CHANNEL_CAPACITY, PtyInput};

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

pub fn kill_pty(handle: &PtyHandle, signal: i32) {
    unsafe {
        libc::kill(handle.child_pid, signal);
    }
}

pub fn close_pty(handle: &PtyHandle) {
    unsafe {
        libc::kill(handle.child_pid, libc::SIGHUP);
        libc::close(handle.master_fd);
    }
}

pub fn collect_exit_status(handle: &PtyHandle) -> i32 {
    unsafe {
        let mut wstatus: libc::c_int = 0;
        if libc::waitpid(handle.child_pid, &mut wstatus, libc::WNOHANG) > 0 {
            if libc::WIFEXITED(wstatus) {
                return libc::WEXITSTATUS(wstatus);
            } else if libc::WIFSIGNALED(wstatus) {
                return -(libc::WTERMSIG(wstatus) as i32);
            }
        }
        blit_remote::EXIT_STATUS_UNKNOWN
    }
}

pub fn reap_zombies() {
    unsafe { while libc::waitpid(-1, std::ptr::null_mut(), libc::WNOHANG) > 0 {} }
}

pub fn respond_to_queries(handle: &PtyHandle, data: &[u8], size: (u16, u16), cursor: (u16, u16)) {
    for resp in crate::parse_terminal_queries(data, size, cursor) {
        pty_write_all(handle.master_fd, resp.as_bytes());
    }
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
        unsafe {
            std::env::set_var("TERM", "xterm-256color");
            std::env::set_var("COLORTERM", "truecolor");
            std::env::remove_var("COLUMNS");
            std::env::remove_var("LINES");
            for (key, _) in std::env::vars() {
                if key.starts_with("BLIT_") && key != "BLIT_HUB" && key != "BLIT_DISPLAY_FPS" {
                    std::env::remove_var(&key);
                }
            }
        }
        if let Some(wd) = wayland_display {
            unsafe {
                std::env::set_var("WAYLAND_DISPLAY", wd);
                std::env::remove_var("DISPLAY");
            }
        }
        if let Some(command) = command {
            let shell_c = CString::new(shell).unwrap();
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
                libc::execvp(p, [p, f, c, std::ptr::null()].as_ptr());
                libc::_exit(1);
            }
        }
        if let Some(args) = argv
            && !args.is_empty()
        {
            let cargs: Vec<CString> = args.iter().map(|s| CString::new(*s).unwrap()).collect();
            let ptrs: Vec<*const libc::c_char> = cargs
                .iter()
                .map(|c| c.as_ptr())
                .chain(std::iter::once(std::ptr::null()))
                .collect();
            unsafe {
                libc::execvp(ptrs[0], ptrs.as_ptr());
                libc::_exit(1);
            }
        }
        let shell_c = CString::new(shell).unwrap();
        unsafe {
            if shell_flags.is_empty() {
                let p = shell_c.as_ptr();
                libc::execvp(p, [p, std::ptr::null()].as_ptr());
            } else {
                let flag = CString::new(format!("-{}", shell_flags)).unwrap();
                let p = shell_c.as_ptr();
                let f = flag.as_ptr();
                libc::execvp(p, [p, f, std::ptr::null()].as_ptr());
            }
            libc::_exit(1);
        }
    }

    unsafe {
        libc::close(slave);
        let flags = libc::fcntl(master, libc::F_GETFL);
        libc::fcntl(master, libc::F_SETFL, flags | libc::O_NONBLOCK);
    }

    state.2.write().unwrap().insert(id, master);
    let (byte_tx, byte_rx) = mpsc::channel(PTY_CHANNEL_CAPACITY);
    let reader_handle = std::thread::spawn({
        let notify = state.3.clone();
        move || pty_reader(master, byte_tx, notify)
    });
    let handle = PtyHandle {
        master_fd: master,
        child_pid: pid,
    };
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

#[allow(clippy::too_many_arguments)]
pub fn respawn_child(
    shell: &str,
    shell_flags: &str,
    rows: u16,
    cols: u16,
    pty_id: u16,
    command: Option<&str>,
    state: AppState,
    wayland_display: Option<&str>,
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
        }
        set_qos_user_interactive();
        unsafe {
            std::env::set_var("TERM", "xterm-256color");
            std::env::set_var("COLORTERM", "truecolor");
            std::env::remove_var("COLUMNS");
            std::env::remove_var("LINES");
            for (key, _) in std::env::vars() {
                if key.starts_with("BLIT_") && key != "BLIT_HUB" && key != "BLIT_DISPLAY_FPS" {
                    std::env::remove_var(&key);
                }
            }
        }
        if let Some(wd) = wayland_display {
            unsafe {
                std::env::set_var("WAYLAND_DISPLAY", wd);
                std::env::remove_var("DISPLAY");
            }
        }
        if let Some(cmd) = command {
            let shell_c = CString::new(shell).unwrap();
            let flag = CString::new(if shell_flags.is_empty() {
                "-c".to_owned()
            } else {
                format!("-{}c", shell_flags)
            })
            .unwrap();
            let cmd_c = CString::new(cmd).unwrap();
            unsafe {
                libc::execvp(
                    shell_c.as_ptr(),
                    [
                        shell_c.as_ptr(),
                        flag.as_ptr(),
                        cmd_c.as_ptr(),
                        std::ptr::null(),
                    ]
                    .as_ptr(),
                );
                libc::_exit(1);
            }
        }
        let shell_c = CString::new(shell).unwrap();
        unsafe {
            if shell_flags.is_empty() {
                let p = shell_c.as_ptr();
                libc::execvp(p, [p, std::ptr::null()].as_ptr());
            } else {
                let flag = CString::new(format!("-{}", shell_flags)).unwrap();
                let p = shell_c.as_ptr();
                let f = flag.as_ptr();
                libc::execvp(p, [p, f, std::ptr::null()].as_ptr());
            }
            libc::_exit(1);
        }
    }

    unsafe {
        libc::close(slave);
        let flags = libc::fcntl(master, libc::F_GETFL);
        libc::fcntl(master, libc::F_SETFL, flags | libc::O_NONBLOCK);
    }

    state.2.write().unwrap().insert(pty_id, master);
    let (byte_tx, byte_rx) = mpsc::channel(PTY_CHANNEL_CAPACITY);
    let reader_handle = std::thread::spawn({
        let notify = state.3.clone();
        move || pty_reader(master, byte_tx, notify)
    });
    let handle = PtyHandle {
        master_fd: master,
        child_pid: pid,
    };
    Some((handle, reader_handle, byte_rx))
}
