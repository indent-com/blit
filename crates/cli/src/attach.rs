//! `blit terminal attach` — drive a remote PTY from this terminal.
//!
//! Every other CLI command sends one frame and reads one answer. This one
//! is a session: it puts the local TTY in raw mode, forwards keystrokes as
//! `C2S_INPUT`, applies the `S2C_UPDATE` stream into a `TerminalState`, and
//! repaints. `TerminalState::get_ansi_text` already renders a grid with
//! colours — the same code `terminal show --ansi` uses — so attaching is
//! mostly lifecycle: raw mode, resize, repaint, and getting the terminal
//! back in one piece afterwards.
//!
//! Repaints are whole-screen. A grid is a few thousand cells and the
//! alternate screen buffer hides the redraw, so tracking damage would buy
//! nothing but a chance to leave the display wrong.

use crate::transport::Transport;
#[cfg(unix)]
use blit_remote::{
    S2C_EXITED, S2C_QUIT, S2C_UPDATE, TerminalState, msg_ack, msg_input, msg_resize, msg_subscribe,
    msg_unsubscribe,
};
#[cfg(unix)]
use std::io::{Read as _, Write as _};
#[cfg(unix)]
use std::sync::Arc;
#[cfg(unix)]
use std::sync::atomic::{AtomicBool, Ordering};

/// Ctrl-] — telnet's escape, and not something a full-screen app expects
/// to receive, so it can be reserved without stealing a useful key.
#[cfg(unix)]
const DETACH: u8 = 0x1d;

/// Terminal has been resized; set from the SIGWINCH handler.
#[cfg(unix)]
static RESIZED: AtomicBool = AtomicBool::new(false);

#[cfg(unix)]
extern "C" fn on_sigwinch(_: libc::c_int) {
    RESIZED.store(true, Ordering::Relaxed);
}

/// The local TTY's saved attributes, restored on drop.
///
/// A guard rather than cleanup at the end of the function: a `?` on any
/// send, a panic, or a detach all have to put the terminal back, and an
/// early return that skipped it would leave the user with no echo.
#[cfg(unix)]
struct RawMode(Option<libc::termios>);

#[cfg(unix)]
impl RawMode {
    fn enter() -> Result<Self, String> {
        // SAFETY: tcgetattr/tcsetattr on fd 0 with a fully initialised
        // termios; both report failure through their return value.
        unsafe {
            if libc::isatty(0) != 1 {
                return Err("stdin is not a terminal (attach needs a tty)".into());
            }
            let mut saved: libc::termios = std::mem::zeroed();
            if libc::tcgetattr(0, &mut saved) != 0 {
                return Err("cannot read terminal attributes".into());
            }
            let mut raw = saved;
            libc::cfmakeraw(&mut raw);
            // Keep reads non-blocking-ish: return as soon as a byte is
            // there, but do not spin when idle.
            raw.c_cc[libc::VMIN] = 1;
            raw.c_cc[libc::VTIME] = 0;
            if libc::tcsetattr(0, libc::TCSANOW, &raw) != 0 {
                return Err("cannot put the terminal in raw mode".into());
            }
            Ok(Self(Some(saved)))
        }
    }
}

#[cfg(unix)]
impl Drop for RawMode {
    fn drop(&mut self) {
        if let Some(saved) = self.0.take() {
            // SAFETY: `saved` came from tcgetattr on this same fd.
            unsafe {
                libc::tcsetattr(0, libc::TCSANOW, &saved);
            }
        }
        // Leave the alternate screen and show the cursor again, in that
        // order, so the shell prompt lands on the original screen.
        let mut out = std::io::stdout();
        let _ = out.write_all(b"\x1b[?25h\x1b[?1049l");
        let _ = out.flush();
    }
}

/// The local terminal's size, or 80x24 when it cannot be determined.
#[cfg(unix)]
fn window_size() -> (u16, u16) {
    // SAFETY: ioctl(TIOCGWINSZ) with an owned, zeroed winsize.
    unsafe {
        let mut ws: libc::winsize = std::mem::zeroed();
        if libc::ioctl(0, libc::TIOCGWINSZ, &mut ws) == 0 && ws.ws_col > 0 && ws.ws_row > 0 {
            (ws.ws_col, ws.ws_row)
        } else {
            (80, 24)
        }
    }
}

/// Paint the whole grid, then park the cursor where the PTY has it.
#[cfg(unix)]
fn repaint(state: &TerminalState) {
    let mut out = Vec::with_capacity(16 * 1024);
    // Synchronized update: a terminal that understands 2026 shows the
    // repaint atomically instead of mid-draw.
    out.extend_from_slice(b"\x1b[?2026h");
    out.extend_from_slice(b"\x1b[H\x1b[2J");
    let text = state.get_ansi_text();
    // The grid's own newlines are row breaks, and raw mode does not
    // translate them, so each needs an explicit carriage return.
    for (i, line) in text.lines().enumerate() {
        if i > 0 {
            out.extend_from_slice(b"\r\n");
        }
        out.extend_from_slice(line.as_bytes());
    }
    out.extend_from_slice(
        format!(
            "\x1b[{};{}H",
            state.cursor_row() + 1,
            state.cursor_col() + 1
        )
        .as_bytes(),
    );
    out.extend_from_slice(b"\x1b[?2026l");
    let mut stdout = std::io::stdout();
    let _ = stdout.write_all(&out);
    let _ = stdout.flush();
}

#[cfg(unix)]
pub async fn cmd_attach(transport: Transport, id: u16) -> Result<i32, String> {
    use crate::agent::AgentConn;

    let mut conn = AgentConn::connect(transport).await?;
    if !conn.has_pty(id) {
        return Err(format!("pty {id} not found"));
    }

    // SAFETY: installing a handler that only sets an atomic flag. The cast
    // goes through a fn pointer, not the fn item, so the address is well
    // defined (clippy::fn_to_numeric_cast_any).
    unsafe {
        let handler: extern "C" fn(libc::c_int) = on_sigwinch;
        libc::signal(libc::SIGWINCH, handler as usize as libc::sighandler_t);
    }

    let raw = RawMode::enter()?;
    let mut stdout = std::io::stdout();
    // Alternate screen + hidden cursor; the guard undoes both.
    let _ = stdout.write_all(b"\x1b[?1049h\x1b[?25l");
    let _ = stdout.flush();

    let (cols, rows) = window_size();
    conn.send(&msg_resize(id, rows, cols)).await?;
    conn.send(&msg_subscribe(id)).await?;

    // stdin is blocking, so it reads on its own thread and hands bytes
    // over a channel; the select below stays async.
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
    let stop = Arc::new(AtomicBool::new(false));
    let reader_stop = stop.clone();
    std::thread::spawn(move || {
        let mut buf = [0u8; 4096];
        let mut stdin = std::io::stdin();
        while !reader_stop.load(Ordering::Relaxed) {
            match stdin.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    if tx.send(buf[..n].to_vec()).is_err() {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
    });

    let mut state = TerminalState::new(rows, cols);
    let mut exit = 0i32;
    let mut detached = false;

    while !detached {
        if RESIZED.swap(false, Ordering::Relaxed) {
            let (c, r) = window_size();
            conn.send(&msg_resize(id, r, c)).await?;
        }
        tokio::select! {
            chunk = rx.recv() => {
                let Some(chunk) = chunk else { break };
                if let Some(pos) = chunk.iter().position(|&b| b == DETACH) {
                    // Forward whatever preceded the escape, then leave.
                    if pos > 0 {
                        conn.send(&msg_input(id, &chunk[..pos])).await?;
                    }
                    detached = true;
                } else {
                    conn.send(&msg_input(id, &chunk)).await?;
                }
            }
            msg = conn.recv() => {
                let data = match msg {
                    Ok(d) => d,
                    // The far end going away is a normal end to a session,
                    // not a failure worth a stack of errors.
                    Err(_) => break,
                };
                if data.is_empty() {
                    continue;
                }
                match data[0] {
                    S2C_QUIT => break,
                    S2C_UPDATE if data.len() >= 3 => {
                        if u16::from_le_bytes([data[1], data[2]]) != id {
                            continue;
                        }
                        state.feed_compressed(&data[3..]);
                        conn.send(&msg_ack()).await?;
                        repaint(&state);
                    }
                    S2C_EXITED if data.len() >= 3 => {
                        if u16::from_le_bytes([data[1], data[2]]) != id {
                            continue;
                        }
                        // Show the final frame before tearing the screen
                        // down, then report the child's status.
                        repaint(&state);
                        exit = conn
                            .exited
                            .get(&id)
                            .copied()
                            .map(crate::agent::exit_code_from_status)
                            .unwrap_or(0);
                        break;
                    }
                    _ => {}
                }
            }
        }
    }

    stop.store(true, Ordering::Relaxed);
    let _ = conn.send(&msg_unsubscribe(id)).await;
    drop(raw);
    if detached {
        eprintln!("blit: detached from {id} (still running)");
    }
    Ok(exit)
}

#[cfg(not(unix))]
pub async fn cmd_attach(_transport: Transport, _id: u16) -> Result<i32, String> {
    Err("terminal attach is not supported on this platform".into())
}
