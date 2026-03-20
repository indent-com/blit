use lz4_flex::compress_prepend_size;
use std::collections::{HashMap, VecDeque};
use std::ffi::CString;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::io::{AsRawFd, RawFd};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::io::unix::AsyncFd;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::UnixListener;
use tokio::sync::{mpsc, Mutex, Notify};

type PtyFds = Arc<std::sync::RwLock<HashMap<u16, RawFd>>>;

const C2S_INPUT: u8 = 0x00;
const C2S_RESIZE: u8 = 0x01;
const C2S_SCROLL: u8 = 0x02;
const C2S_ACK: u8 = 0x03;
const C2S_DISPLAY_RATE: u8 = 0x04;
const C2S_CLIENT_METRICS: u8 = 0x05;
const C2S_CREATE: u8 = 0x10;
const C2S_FOCUS: u8 = 0x11;
const C2S_CLOSE: u8 = 0x12;

const S2C_UPDATE: u8 = 0x00;
const S2C_CREATED: u8 = 0x01;
const S2C_CLOSED: u8 = 0x02;
const S2C_LIST: u8 = 0x03;
const S2C_TITLE: u8 = 0x04;

// Cell: 12 bytes
// [0]    flags: fg_type(2) | bg_type(2) | bold(1) | dim(1) | italic(1) | underline(1)
// [1]    flags2: inverse(1) | wide(1) | wide_cont(1) | content_len(3) | reserved(2)
// [2..5] fg (r, g, b) -- idx: r=idx
// [5..8] bg (r, g, b) -- idx: r=idx
// [8..12] content (up to 4 bytes UTF-8)
const CELL_SIZE: usize = 12;

const SCROLLBACK_ROWS_DEFAULT: usize = 100_000;

#[derive(Default)]
struct TitleCallbacks {
    title: String,
    title_dirty: bool,
}

impl vt100::Callbacks for TitleCallbacks {
    fn set_window_title(&mut self, _: &mut vt100::Screen, title: &[u8]) {
        self.title = String::from_utf8_lossy(title).into_owned();
        self.title_dirty = true;
    }
    fn set_window_icon_name(&mut self, _: &mut vt100::Screen, name: &[u8]) {
        self.title = String::from_utf8_lossy(name).into_owned();
        self.title_dirty = true;
    }
}

struct Config {
    shell: String,
    scrollback: usize,
}

struct OwnedFd(RawFd);
impl AsRawFd for OwnedFd {
    fn as_raw_fd(&self) -> RawFd {
        self.0
    }
}

// Keep enough per-client socket queue to fill long-haul links without falling
// back into stop-and-wait. We still rely on dirty-state coalescing upstream,
// but a queue of 4 is too small to sustain 120 fps over high RTT paths.
const OUTBOX_CAPACITY: usize = 32;

async fn read_frame(reader: &mut (impl AsyncRead + Unpin)) -> Option<Vec<u8>> {
    let mut len_buf = [0u8; 4];
    reader.read_exact(&mut len_buf).await.ok()?;
    let len = u32::from_le_bytes(len_buf) as usize;
    if len == 0 {
        return Some(vec![]);
    }
    let mut buf = vec![0u8; len];
    reader.read_exact(&mut buf).await.ok()?;
    Some(buf)
}

async fn write_frame(writer: &mut (impl AsyncWrite + Unpin), payload: &[u8]) -> bool {
    let len = payload.len() as u32;
    let mut buf = Vec::with_capacity(4 + payload.len());
    buf.extend_from_slice(&len.to_le_bytes());
    buf.extend_from_slice(payload);
    writer.write_all(&buf).await.is_ok()
}

fn encode_cell(screen: &vt100::Screen, row: u16, col: u16, buf: &mut [u8; CELL_SIZE]) {
    *buf = [0u8; CELL_SIZE];
    let cell = match screen.cell(row, col) {
        Some(c) => c,
        None => return,
    };

    let mut f0: u8 = 0;
    match cell.fgcolor() {
        vt100::Color::Default => {}
        vt100::Color::Idx(i) => {
            f0 |= 1;
            buf[2] = i;
        }
        vt100::Color::Rgb(r, g, b) => {
            f0 |= 2;
            buf[2] = r;
            buf[3] = g;
            buf[4] = b;
        }
    }
    match cell.bgcolor() {
        vt100::Color::Default => {}
        vt100::Color::Idx(i) => {
            f0 |= 1 << 2;
            buf[5] = i;
        }
        vt100::Color::Rgb(r, g, b) => {
            f0 |= 2 << 2;
            buf[5] = r;
            buf[6] = g;
            buf[7] = b;
        }
    }
    if cell.bold() {
        f0 |= 1 << 4;
    }
    if cell.dim() {
        f0 |= 1 << 5;
    }
    if cell.italic() {
        f0 |= 1 << 6;
    }
    if cell.underline() {
        f0 |= 1 << 7;
    }
    buf[0] = f0;

    let mut f1: u8 = 0;
    if cell.inverse() {
        f1 |= 1;
    }
    if cell.is_wide() {
        f1 |= 2;
    }
    if cell.is_wide_continuation() {
        f1 |= 4;
    }
    let contents = cell.contents();
    let bytes = contents.as_bytes();
    let len = bytes.len().min(4);
    f1 |= (len as u8) << 3;
    buf[1] = f1;
    buf[8..8 + len].copy_from_slice(&bytes[..len]);
}

fn snapshot_screen(screen: &vt100::Screen) -> Vec<u8> {
    let (rows, cols) = screen.size();
    let mut snap = vec![0u8; rows as usize * cols as usize * CELL_SIZE];
    for row in 0..rows {
        for col in 0..cols {
            let off = (row as usize * cols as usize + col as usize) * CELL_SIZE;
            let cell_buf: &mut [u8; CELL_SIZE] =
                (&mut snap[off..off + CELL_SIZE]).try_into().unwrap();
            encode_cell(screen, row, col, cell_buf);
        }
    }
    snap
}

struct Pty {
    master_fd: libc::c_int,
    child_pid: libc::pid_t,
    parser: vt100::Parser<TitleCallbacks>,
    dirty: bool,
    reader_handle: tokio::task::JoinHandle<()>,
    /// Cached (echo, icanon) from tcgetattr; refreshed every ~250ms.
    lflag_cache: (bool, bool),
    lflag_last: Instant,
}

struct ClientState {
    tx: mpsc::Sender<Vec<u8>>,
    focus: Option<u16>,
    size: Option<(u16, u16)>,
    scroll_offset: usize,
    scroll_snap: Vec<u8>,
    last_sent_snap: Vec<u8>,
    last_sent_cursor: (u16, u16),
    last_sent_mode: u16,
    /// EWMA RTT estimate in milliseconds.
    rtt_ms: f32,
    /// Minimum-path RTT estimate in milliseconds, excluding queue growth.
    min_rtt_ms: f32,
    /// Client's measured display refresh rate (fps), reported via C2S_DISPLAY_RATE.
    display_fps: f32,
    /// EWMA of delivered payload rate in bytes/sec.
    delivery_bps: f32,
    /// EWMA of actual ACKed goodput in bytes/sec, based on ACK cadence rather than RTT.
    goodput_bps: f32,
    /// EWMA of acknowledged frame payload size in bytes.
    avg_frame_bytes: f32,
    /// Payload bytes currently in flight (sent, not yet ACKed).
    inflight_bytes: usize,
    /// Oldest in-flight frame first; ACKs arrive in order.
    inflight_frames: VecDeque<InFlightFrame>,
    /// Earliest time the next visual update should be sent for smooth pacing.
    next_send_at: Instant,
    /// Temporary additive window growth used to probe for more throughput after
    /// a conservative backoff. Decays when queue delay grows.
    probe_frames: f32,
    /// Diagnostics.
    frames_sent: u32,
    acks_recv: u32,
    acked_bytes_since_log: usize,
    browser_backlog_frames: u16,
    browser_ack_ahead_frames: u16,
    browser_apply_ms: f32,
    last_log: Instant,
    goodput_window_bytes: usize,
    goodput_window_start: Instant,
}

struct InFlightFrame {
    sent_at: Instant,
    bytes: usize,
}

/// Frames to keep in flight: enough to cover one RTT at the client's reported
/// display rate. High-latency links need many frames in flight to avoid
/// devolving into stop-and-wait.
fn frame_window(rtt_ms: f32, display_fps: f32) -> usize {
    let frame_ms = 1_000.0 / display_fps.max(1.0);
    ((rtt_ms / frame_ms).ceil() as usize + 8).max(8)
}

fn path_rtt_ms(client: &ClientState) -> f32 {
    if client.min_rtt_ms > 0.0 {
        client.min_rtt_ms
    } else {
        client.rtt_ms
    }
}

fn effective_rtt_ms(client: &ClientState) -> f32 {
    let path_rtt = path_rtt_ms(client);
    let frame_ms = 1_000.0 / client.display_fps.max(1.0);
    let queue_allowance = frame_ms * 12.0;
    client.rtt_ms.clamp(path_rtt, path_rtt + queue_allowance)
}

fn target_frame_window(client: &ClientState) -> usize {
    frame_window(effective_rtt_ms(client), client.display_fps)
        .saturating_add(client.probe_frames.round().max(0.0) as usize)
}

fn base_queue_ms(client: &ClientState) -> f32 {
    let frame_ms = 1_000.0 / client.display_fps.max(1.0);
    frame_ms * 8.0
}

fn target_queue_ms(client: &ClientState) -> f32 {
    let frame_ms = 1_000.0 / client.display_fps.max(1.0);
    base_queue_ms(client) + client.probe_frames.max(0.0) * frame_ms
}

fn byte_budget_for(client: &ClientState, budget_ms: f32) -> usize {
    let bytes = client.goodput_bps.max(32_768.0) * budget_ms.max(1.0) / 1_000.0;
    bytes
        .ceil()
        .max(client.avg_frame_bytes.max(256.0))
        as usize
}

fn base_byte_window(client: &ClientState) -> usize {
    byte_budget_for(client, path_rtt_ms(client) + base_queue_ms(client))
}

fn target_byte_window(client: &ClientState) -> usize {
    byte_budget_for(client, path_rtt_ms(client) + target_queue_ms(client))
}

fn send_interval(client: &ClientState) -> Duration {
    Duration::from_secs_f64(1.0 / client.display_fps.max(1.0) as f64)
}

fn window_open(client: &ClientState) -> bool {
    client.inflight_frames.len() < target_frame_window(client)
        && client.inflight_bytes < target_byte_window(client)
}

fn can_send_frame(client: &ClientState, now: Instant) -> bool {
    window_open(client) && now >= client.next_send_at
}

fn record_send(client: &mut ClientState, bytes: usize, now: Instant) {
    client.inflight_bytes += bytes;
    client.inflight_frames.push_back(InFlightFrame {
        sent_at: now,
        bytes,
    });
    client.next_send_at = now + send_interval(client);
}

fn ewma_with_direction(old: f32, sample: f32, rise_alpha: f32, fall_alpha: f32) -> f32 {
    let alpha = if sample > old { rise_alpha } else { fall_alpha };
    old * (1.0 - alpha) + sample * alpha
}

fn record_ack(client: &mut ClientState) {
    if let Some(frame) = client.inflight_frames.pop_front() {
        let prev_inflight_frames = client.inflight_frames.len() + 1;
        let prev_inflight_bytes = client.inflight_bytes;
        client.inflight_bytes = client.inflight_bytes.saturating_sub(frame.bytes);
        client.acked_bytes_since_log = client.acked_bytes_since_log.saturating_add(frame.bytes);
        let sample_ms = frame.sent_at.elapsed().as_secs_f32() * 1_000.0;
        client.rtt_ms = ewma_with_direction(client.rtt_ms, sample_ms, 0.125, 0.25);
        client.min_rtt_ms = if client.min_rtt_ms > 0.0 {
            client.min_rtt_ms.min(sample_ms)
        } else {
            sample_ms
        };
        let sample_bps = frame.bytes as f32 / sample_ms.max(1.0e-3) * 1_000.0;
        client.delivery_bps =
            ewma_with_direction(client.delivery_bps, sample_bps, 0.5, 0.125);
        client.avg_frame_bytes = ewma_with_direction(
            client.avg_frame_bytes,
            frame.bytes as f32,
            0.5,
            0.125,
        );
        client.goodput_window_bytes = client
            .goodput_window_bytes
            .saturating_add(frame.bytes);
        let now = Instant::now();
        let goodput_elapsed = now
            .duration_since(client.goodput_window_start)
            .as_secs_f32();
        if goodput_elapsed >= 0.02 {
            let sample_goodput =
                client.goodput_window_bytes as f32 / goodput_elapsed.max(1.0e-3);
            client.goodput_bps = ewma_with_direction(
                client.goodput_bps,
                sample_goodput,
                0.5,
                0.125,
            );
            client.goodput_window_bytes = 0;
            client.goodput_window_start = now;
        }
        let frame_ms = 1_000.0 / client.display_fps.max(1.0);
        let path_rtt = path_rtt_ms(client);
        let queue_delay_ms = (sample_ms - path_rtt).max(0.0);
        let base_frames = frame_window(effective_rtt_ms(client), client.display_fps);
        let base_bytes = base_byte_window(client);
        let likely_window_limited =
            prev_inflight_frames >= base_frames || prev_inflight_bytes >= base_bytes;
        let max_probe_frames = (client.display_fps * 0.125).max(4.0);
        if likely_window_limited && queue_delay_ms <= frame_ms * 8.0 {
            client.probe_frames = (client.probe_frames + 1.0).min(max_probe_frames);
        } else if queue_delay_ms > frame_ms * 12.0 {
            client.probe_frames *= 0.25;
        } else {
            client.probe_frames = (client.probe_frames - 0.5).max(0.0);
        }
    } else {
        client.inflight_bytes = 0;
    }
}

fn reset_inflight(client: &mut ClientState) {
    client.inflight_bytes = 0;
    client.inflight_frames.clear();
}

struct Session {
    ptys: HashMap<u16, Pty>,
    next_pty_id: u16,
    next_client_id: u64,
    /// Diagnostics: how many times tick() was called this second.
    tick_fires: u32,
    /// Diagnostics: how many ticks found the focused PTY dirty (snapshot taken).
    tick_snaps: u32,
    clients: HashMap<u64, ClientState>,
}

struct TickOutcome {
    did_work: bool,
    next_deadline: Option<Instant>,
}

impl Session {
    fn new() -> Self {
        Self {
            ptys: HashMap::new(),
            next_pty_id: 1,
            next_client_id: 1,
            clients: HashMap::new(),
            tick_fires: 0,
            tick_snaps: 0,
        }
    }

    fn send_to_all(&self, msg: &[u8]) {
        for c in self.clients.values() {
            let _ = c.tx.try_send(msg.to_vec());
        }
    }

    fn min_size_for_pty(&self, pty_id: u16) -> Option<(u16, u16)> {
        let mut min_rows: Option<u16> = None;
        let mut min_cols: Option<u16> = None;
        for c in self.clients.values() {
            if c.focus == Some(pty_id) {
                if let Some((r, cols)) = c.size {
                    min_rows = Some(min_rows.map_or(r, |m: u16| m.min(r)));
                    min_cols = Some(min_cols.map_or(cols, |m: u16| m.min(cols)));
                }
            }
        }
        match (min_rows, min_cols) {
            (Some(r), Some(c)) => Some((r.max(1), c.max(1))),
            _ => None,
        }
    }

    fn resize_pty(&mut self, pty_id: u16, rows: u16, cols: u16) {
        let pty = match self.ptys.get_mut(&pty_id) {
            Some(p) => p,
            None => return,
        };
        let (cur_rows, cur_cols) = pty.parser.screen().size();
        if cur_rows == rows && cur_cols == cols {
            return;
        }
        pty.parser.screen_mut().set_size(rows, cols);
        pty.dirty = true;
        for c in self.clients.values_mut() {
            if c.focus == Some(pty_id) {
                c.last_sent_snap.clear();
                c.scroll_snap.clear();
                reset_inflight(c);
            }
        }
        unsafe {
            let ws = libc::winsize {
                ws_row: rows,
                ws_col: cols,
                ws_xpixel: 0,
                ws_ypixel: 0,
            };
            libc::ioctl(pty.master_fd, libc::TIOCSWINSZ, &ws);
            libc::kill(-pty.child_pid, libc::SIGWINCH);
        }
    }

    fn pty_list_msg(&self) -> Vec<u8> {
        let mut msg = vec![S2C_LIST];
        let count = self.ptys.len() as u16;
        msg.extend_from_slice(&count.to_le_bytes());
        let mut ids: Vec<u16> = self.ptys.keys().copied().collect();
        ids.sort();
        for id in ids {
            msg.extend_from_slice(&id.to_le_bytes());
        }
        msg
    }
}

type AppState = Arc<(Config, Mutex<Session>, PtyFds, Arc<Notify>)>;

fn nudge_delivery(state: &AppState) {
    state.3.notify_one();
}

fn spawn_pty(shell: &str, rows: u16, cols: u16, id: u16, argv: Option<&[&str]>, scrollback: usize, state: AppState) -> Option<Pty> {
    let mut master: libc::c_int = 0;
    let mut slave: libc::c_int = 0;
    unsafe {
        if libc::openpty(
            &mut master,
            &mut slave,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut()
        ) != 0 {
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
        unsafe { libc::close(master); libc::close(slave); }
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
        std::env::set_var("TERM", "xterm-256color");
        std::env::set_var("COLUMNS", &cols.to_string());
        std::env::set_var("LINES", &rows.to_string());
        if let Some(args) = argv {
            if !args.is_empty() {
                let cargs: Vec<CString> = args.iter()
                    .map(|s| CString::new(*s).unwrap())
                    .collect();
                let ptrs: Vec<*const libc::c_char> = cargs.iter()
                    .map(|c| c.as_ptr())
                    .chain(std::iter::once(std::ptr::null()))
                    .collect();
                unsafe {
                    libc::execvp(ptrs[0], ptrs.as_ptr());
                    libc::_exit(1);
                }
            }
        }
        // Default: login shell
        let shell_c = CString::new(shell).unwrap();
        let login_flag = CString::new("-l").unwrap();
        unsafe {
            let p = shell_c.as_ptr();
            let l = login_flag.as_ptr();
            libc::execvp(p, [p, l, std::ptr::null()].as_ptr());
            libc::_exit(1);
        }
    }

    unsafe {
        libc::close(slave);
        let flags = libc::fcntl(master, libc::F_GETFL);
        libc::fcntl(master, libc::F_SETFL, flags | libc::O_NONBLOCK);
    }

    state.2.write().unwrap().insert(id, master);

    let reader_handle = tokio::spawn(pty_reader(master, id, state));

    let lflag_cache = pty_lflag(master);
    Some(Pty {
        master_fd: master,
        child_pid: pid,
        parser: vt100::Parser::new_with_callbacks(
            rows,
            cols,
            scrollback,
            TitleCallbacks::default(),
        ),
        dirty: true,
        reader_handle,
        lflag_cache,
        lflag_last: Instant::now(),
    })
}

fn respond_to_queries(fd: libc::c_int, data: &[u8], screen: &vt100::Screen) {
    const DA1_RESPONSE: &[u8] = b"\x1b[?62;22c";

    let mut i = 0;
    while i < data.len() {
        if data[i] != 0x1b || i + 2 >= data.len() || data[i + 1] != b'[' {
            i += 1;
            continue;
        }
        i += 2;
        let has_q = i < data.len() && data[i] == b'?';
        if has_q {
            i += 1;
        }
        let param_start = i;
        while i < data.len() && (data[i].is_ascii_digit() || data[i] == b';') {
            i += 1;
        }
        if i >= data.len() {
            break;
        }
        let final_byte = data[i];
        let params = &data[param_start..i];
        i += 1;
        if has_q {
            continue;
        }
        let resp: Option<String> = match final_byte {
            b'c' if params.is_empty() || params == b"0" => {
                Some(String::from_utf8_lossy(DA1_RESPONSE).into_owned())
            }
            b'n' if params == b"6" => {
                let cursor = screen.cursor_position();
                Some(format!("\x1b[{};{}R", cursor.0 + 1, cursor.1 + 1))
            }
            b'n' if params == b"5" => Some("\x1b[0n".into()),
            b't' if params == b"18" => {
                let (rows, cols) = screen.size();
                Some(format!("\x1b[8;{rows};{cols}t"))
            }
            b't' if params == b"14" => {
                let (rows, cols) = screen.size();
                Some(format!("\x1b[4;{};{}t", rows * 16, cols * 8))
            }
            _ => None,
        };
        if let Some(r) = resp {
            unsafe {
                libc::write(fd, r.as_ptr().cast(), r.len());
            }
        }
    }
}

async fn pty_reader(fd: libc::c_int, pty_id: u16, state: AppState) {
    let async_fd = match AsyncFd::new(OwnedFd(fd)) {
        Ok(f) => f,
        Err(_) => {
            cleanup_pty(pty_id, &state).await;
            return;
        }
    };

    let mut buf = [0u8; 16384];
    // Accumulate multiple reads before locking the session mutex and nudging
    // delivery. Without batching, high-throughput output (e.g. video) causes
    // many tiny lock/wakeup cycles and extra scheduler overhead.
    let mut batch: Vec<u8> = Vec::with_capacity(64 * 1024);

    loop {
        let mut guard = match async_fd.readable().await {
            Ok(g) => g,
            Err(_) => break,
        };

        // Drain all currently available data into `batch` before processing.
        batch.clear();
        let mut exit = false;
        loop {
            let n = unsafe { libc::read(fd, buf.as_mut_ptr().cast(), buf.len()) };
            if n > 0 {
                batch.extend_from_slice(&buf[..n as usize]);
                // Cap batch size so the mutex isn't held too long in one go.
                if batch.len() >= 64 * 1024 {
                    break;
                }
            } else if n == 0 {
                exit = true;
                break;
            } else {
                let err = std::io::Error::last_os_error();
                if err.kind() == std::io::ErrorKind::WouldBlock {
                    guard.clear_ready();
                    break;
                } else {
                    exit = true;
                    break;
                }
            }
        }

        if !batch.is_empty() {
            let mut sess = state.1.lock().await;
            if let Some(pty) = sess.ptys.get_mut(&pty_id) {
                pty.parser.process(&batch);
                pty.dirty = true;
                respond_to_queries(fd, &batch, pty.parser.screen());
            }
            drop(sess);
            nudge_delivery(&state);
            tokio::task::yield_now().await;
        }

        if exit {
            break;
        }
    }

    tokio::time::sleep(Duration::from_millis(50)).await;
    cleanup_pty(pty_id, &state).await;
}

async fn cleanup_pty(pty_id: u16, state: &AppState) {
    state.2.write().unwrap().remove(&pty_id);
    let mut sess = state.1.lock().await;
    if let Some(pty) = sess.ptys.remove(&pty_id) {
        unsafe {
            libc::kill(pty.child_pid, libc::SIGHUP);
            libc::close(pty.master_fd);
        }
        let mut msg = vec![S2C_CLOSED];
        msg.extend_from_slice(&pty_id.to_le_bytes());
        sess.send_to_all(&msg);
    }
}

fn pty_lflag(fd: libc::c_int) -> (bool, bool) {
    unsafe {
        let mut termios: libc::termios = std::mem::zeroed();
        if libc::tcgetattr(fd, &mut termios) == 0 {
            (
                termios.c_lflag & libc::ECHO != 0,
                termios.c_lflag & libc::ICANON != 0,
            )
        } else {
            (false, false)
        }
    }
}

fn pack_mode(screen: &vt100::Screen, echo: bool, icanon: bool) -> u16 {
    let mut m: u16 = 0;
    if !screen.hide_cursor() {
        m |= 1;
    }
    if screen.application_cursor() {
        m |= 2;
    }
    if screen.application_keypad() {
        m |= 4;
    }
    if screen.bracketed_paste() {
        m |= 8;
    }
    let mouse = match screen.mouse_protocol_mode() {
        vt100::MouseProtocolMode::None => 0u16,
        vt100::MouseProtocolMode::Press => 1,
        vt100::MouseProtocolMode::PressRelease => 2,
        vt100::MouseProtocolMode::ButtonMotion => 3,
        vt100::MouseProtocolMode::AnyMotion => 4,
    };
    m |= mouse << 4;
    let enc = match screen.mouse_protocol_encoding() {
        vt100::MouseProtocolEncoding::Default => 0u16,
        vt100::MouseProtocolEncoding::Utf8 => 1,
        vt100::MouseProtocolEncoding::Sgr => 2,
    };
    m |= enc << 7;
    if echo {
        m |= 1 << 9;
    }
    if icanon {
        m |= 1 << 10;
    }
    m
}

fn build_snapshot_msg(
    id: u16,
    snap: &[u8],
    prev: &[u8],
    rows: u16,
    cols: u16,
    cursor: (u16, u16),
    mode: u16,
    prev_cursor: (u16, u16),
    prev_mode: u16,
) -> Option<Vec<u8>> {
    let total_cells = rows as usize * cols as usize;
    let bitmask_len = (total_cells + 7) / 8;
    let full = prev.len() != snap.len();

    let mut bitmask = vec![0u8; bitmask_len];
    let mut dirty_count = 0usize;
    for i in 0..total_cells {
        let off = i * CELL_SIZE;
        if full || snap[off..off + CELL_SIZE] != prev[off..off + CELL_SIZE] {
            bitmask[i / 8] |= 1 << (i % 8);
            dirty_count += 1;
        }
    }

    // Skip sending when nothing changed (cells, cursor, and mode all identical)
    if dirty_count == 0 && cursor == prev_cursor && mode == prev_mode {
        return None;
    }

    let payload_size = 10 + bitmask_len + dirty_count * CELL_SIZE;
    let mut payload = Vec::with_capacity(payload_size);
    payload.extend_from_slice(&rows.to_le_bytes());
    payload.extend_from_slice(&cols.to_le_bytes());
    payload.extend_from_slice(&cursor.0.to_le_bytes());
    payload.extend_from_slice(&cursor.1.to_le_bytes());
    payload.extend_from_slice(&mode.to_le_bytes());
    payload.extend_from_slice(&bitmask);

    // Struct-of-arrays layout: all f0 bytes, then all f1 bytes, ..., then all c3 bytes.
    // Homogeneous columns (mostly-zero color/flag arrays) compress far better than
    // interleaved AOS because LZ4 can reference long runs of zeros in each column.
    for byte_pos in 0..CELL_SIZE {
        for i in 0..total_cells {
            if bitmask[i / 8] & (1 << (i % 8)) != 0 {
                payload.push(snap[i * CELL_SIZE + byte_pos]);
            }
        }
    }

    let compressed = compress_prepend_size(&payload);
    let mut msg = Vec::with_capacity(3 + compressed.len());
    msg.push(S2C_UPDATE);
    msg.extend_from_slice(&id.to_le_bytes());
    msg.extend_from_slice(&compressed);
    Some(msg)
}

struct PtySnapshot {
    snap: Vec<u8>,
    cursor: (u16, u16),
    mode: u16,
    rows: u16,
    cols: u16,
}

fn take_snapshot(pty: &mut Pty) -> PtySnapshot {
    if pty.lflag_last.elapsed() >= Duration::from_millis(250) {
        pty.lflag_cache = pty_lflag(pty.master_fd);
        pty.lflag_last = Instant::now();
    }
    let (echo, icanon) = pty.lflag_cache;
    let screen = pty.parser.screen();
    let (rows, cols) = screen.size();
    PtySnapshot {
        snap: snapshot_screen(screen),
        cursor: screen.cursor_position(),
        mode: pack_mode(screen, echo, icanon),
        rows,
        cols,
    }
}

fn build_scrollback_update(
    pty: &mut Pty,
    id: u16,
    offset: usize,
    prev_snap: &[u8],
) -> Option<(Vec<u8>, Vec<u8>)> {
    let screen = pty.parser.screen_mut();
    let old_sb = screen.scrollback();
    screen.set_scrollback(offset);

    let snap = snapshot_screen(screen);
    let (rows, cols) = screen.size();
    let mode: u16 = 0;
    let cursor = (0u16, 0u16);

    let msg = build_snapshot_msg(id, &snap, prev_snap, rows, cols, cursor, mode, cursor, mode);

    screen.set_scrollback(old_sb);
    msg.map(|m| (m, snap))
}

fn bind_socket() -> UnixListener {
    let sock_path = std::env::args().nth(1)
        .or_else(|| std::env::var("BLIT_SOCK").ok())
        .unwrap_or_else(|| {
            if let Ok(dir) = std::env::var("XDG_RUNTIME_DIR") {
                format!("{dir}/blit.sock")
            } else {
                "/tmp/blit.sock".into()
            }
        });
    let _ = std::fs::remove_file(&sock_path);
    let listener = UnixListener::bind(&sock_path).unwrap();
    std::fs::set_permissions(&sock_path, std::fs::Permissions::from_mode(0o700)).unwrap();
    eprintln!("listening on {sock_path}");
    listener
}

#[tokio::main]
async fn main() {
    let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".into());
    let scrollback = std::env::var("BLIT_SCROLLBACK")
        .ok()
        .and_then(|s| s.parse::<usize>().ok())
        .unwrap_or(SCROLLBACK_ROWS_DEFAULT);
    let state: AppState = Arc::new((
        Config { shell, scrollback },
        Mutex::new(Session::new()),
        Arc::new(std::sync::RwLock::new(HashMap::new())),
        Arc::new(Notify::new()),
    ));

    let delivery_state = state.clone();
    tokio::spawn(async move {
        let mut next_deadline: Option<Instant> = None;
        loop {
            if let Some(deadline) = next_deadline {
                tokio::select! {
                    _ = delivery_state.3.notified() => {}
                    _ = tokio::time::sleep_until(tokio::time::Instant::from_std(deadline)) => {}
                }
            } else {
                delivery_state.3.notified().await;
            }
            loop {
                let outcome = tick(&delivery_state).await;
                next_deadline = outcome.next_deadline;
                if !outcome.did_work {
                    break;
                }
                tokio::task::yield_now().await;
            }
        }
    });

    // systemd socket activation: if LISTEN_FDS is set, use fd 3.
    // LISTEN_PID is checked but not required to match — some container runtimes
    // and service managers don't set it to the final process PID.
    let listener = if let Ok(fds) = std::env::var("LISTEN_FDS") {
        if fds.trim() == "1" {
            use std::os::unix::io::FromRawFd;
            let std_listener = unsafe { std::os::unix::net::UnixListener::from_raw_fd(3) };
            std_listener.set_nonblocking(true).unwrap();
            eprintln!("using socket activation (fd 3)");
            UnixListener::from_std(std_listener).unwrap()
        } else {
            eprintln!("LISTEN_FDS={fds}, expected 1; falling back to bind");
            bind_socket()
        }
    } else {
        bind_socket()
    };

    loop {
        let (stream, _) = listener.accept().await.unwrap();
        let state = state.clone();
        tokio::spawn(handle_client(stream, state));
    }
}

async fn tick(state: &AppState) -> TickOutcome {
    let mut sess = state.1.lock().await;
    sess.tick_fires += 1;
    let mut did_work = false;
    let mut next_deadline: Option<Instant> = None;
    let now = Instant::now();

    let ids: Vec<u16> = sess.ptys.keys().copied().collect();
    for &id in &ids {
        let pty = sess.ptys.get_mut(&id).unwrap();
        if pty.parser.callbacks().title_dirty {
            let title = pty.parser.callbacks().title.clone();
            pty.parser.callbacks_mut().title_dirty = false;
            let title_bytes = title.as_bytes();
            let mut msg = Vec::with_capacity(3 + title_bytes.len());
            msg.push(S2C_TITLE);
            msg.extend_from_slice(&id.to_le_bytes());
            msg.extend_from_slice(title_bytes);
            sess.send_to_all(&msg);
            did_work = true;
        }
    }

    // Only snapshot PTYs that have at least one client with room in their window.
    // When all windows are full (high-throughput steady state), skip the expensive
    // snapshot+diff+compress work entirely rather than doing it 250×/s for nothing.
    let needful_ptys: std::collections::HashSet<u16> = sess.clients.values()
        .filter(|c| c.scroll_offset == 0)
        .filter(|c| can_send_frame(c, now))
        .filter_map(|c| c.focus)
        .collect();

    let mut snapshots: HashMap<u16, PtySnapshot> = HashMap::new();
    for &id in &ids {
        let pty = sess.ptys.get_mut(&id).unwrap();
        if !pty.dirty {
            continue;
        }
        if !needful_ptys.contains(&id) {
            continue;
        }
        snapshots.insert(id, take_snapshot(pty));
        pty.dirty = false;
        sess.tick_snaps += 1;
        did_work = true;
    }

    let client_ids: Vec<u64> = sess.clients.keys().copied().collect();
    for cid in client_ids {
        let (focus, scroll_offset, can_send) = {
            let c = sess.clients.get(&cid).unwrap();
            (c.focus, c.scroll_offset, can_send_frame(c, now))
        };
        let Some(pid) = focus else { continue };

        if !can_send {
            if let Some(c) = sess.clients.get(&cid) {
                let has_pending = scroll_offset > 0
                    || sess.ptys.get(&pid).map(|pty| pty.dirty).unwrap_or(false);
                if has_pending && window_open(c) {
                    next_deadline = Some(match next_deadline {
                        Some(existing) => existing.min(c.next_send_at),
                        None => c.next_send_at,
                    });
                }
            }
            continue;
        }

        let sent = if scroll_offset == 0 {
            let Some(cur) = snapshots.get(&pid) else {
                continue;
            };
            let (prev_snap, prev_cursor, prev_mode) = {
                let c = sess.clients.get(&cid).unwrap();
                (c.last_sent_snap.clone(), c.last_sent_cursor, c.last_sent_mode)
            };
            let Some(msg) = build_snapshot_msg(
                pid, &cur.snap, &prev_snap, cur.rows, cur.cols, cur.cursor, cur.mode,
                prev_cursor, prev_mode,
            ) else {
                continue;
            };
            let c = sess.clients.get_mut(&cid).unwrap();
            let bytes = msg.len();
            if c.tx.try_send(msg).is_ok() {
                c.last_sent_snap = cur.snap.clone();
                c.last_sent_cursor = cur.cursor;
                c.last_sent_mode = cur.mode;
                record_send(c, bytes, now);
                c.frames_sent += 1;
                did_work = true;
                true
            } else {
                false
            }
        } else {
            let prev_snap = {
                let c = sess.clients.get(&cid).unwrap();
                c.scroll_snap.clone()
            };
            if let Some(pty) = sess.ptys.get_mut(&pid) {
                if let Some((msg, new_snap)) =
                    build_scrollback_update(pty, pid, scroll_offset, &prev_snap)
                {
                    let c = sess.clients.get_mut(&cid).unwrap();
                    let bytes = msg.len();
                    if c.tx.try_send(msg).is_ok() {
                        c.scroll_snap = new_snap;
                        record_send(c, bytes, now);
                        did_work = true;
                        true
                    } else {
                        false
                    }
                } else {
                    continue;
                }
            } else {
                false
            }
        };
        if !sent {
            if let Some(pty) = sess.ptys.get_mut(&pid) {
                pty.dirty = true;
            }
        }
    }

    TickOutcome {
        did_work,
        next_deadline,
    }
}

async fn handle_client(stream: tokio::net::UnixStream, state: AppState) {
    let config = &state.0;
    let (mut reader, mut writer) = stream.into_split();

    let (out_tx, mut out_rx) = mpsc::channel::<Vec<u8>>(OUTBOX_CAPACITY);
    let delivery_notify = state.3.clone();
    let sender = tokio::spawn(async move {
        while let Some(msg) = out_rx.recv().await {
            if !write_frame(&mut writer, &msg).await {
                break;
            }
            delivery_notify.notify_one();
        }
    });
    let client_id;

    {
        let mut sess = state.1.lock().await;
        client_id = sess.next_client_id;
        sess.next_client_id += 1;
        sess.clients.insert(
            client_id,
            ClientState {
                tx: out_tx,
                focus: None,
                size: None,
                scroll_offset: 0,
                scroll_snap: Vec::new(),
                last_sent_snap: Vec::new(),
                last_sent_cursor: (0, 0),
                last_sent_mode: 0,
                rtt_ms: 50.0,
                min_rtt_ms: 0.0,
                display_fps: 60.0,
                delivery_bps: 256_000.0,
                goodput_bps: 256_000.0,
                avg_frame_bytes: 4_096.0,
                inflight_bytes: 0,
                inflight_frames: VecDeque::new(),
                next_send_at: Instant::now(),
                probe_frames: 0.0,
                frames_sent: 0,
                acks_recv: 0,
                acked_bytes_since_log: 0,
                browser_backlog_frames: 0,
                browser_ack_ahead_frames: 0,
                browser_apply_ms: 0.0,
                last_log: Instant::now(),
                goodput_window_bytes: 0,
                goodput_window_start: Instant::now(),
            },
        );
        let list = sess.pty_list_msg();
        if let Some(c) = sess.clients.get(&client_id) {
            let _ = c.tx.try_send(list);
            for (&id, pty) in &sess.ptys {
                let title = &pty.parser.callbacks().title;
                if !title.is_empty() {
                    let title_bytes = title.as_bytes();
                    let mut msg = Vec::with_capacity(3 + title_bytes.len());
                    msg.push(S2C_TITLE);
                    msg.extend_from_slice(&id.to_le_bytes());
                    msg.extend_from_slice(title_bytes);
                    let _ = c.tx.try_send(msg);
                }
            }
        }
    }

    eprintln!("client connected");

    while let Some(data) = read_frame(&mut reader).await {
        if data.is_empty() {
            continue;
        }

        if data[0] == C2S_ACK {
            let mut sess = state.1.lock().await;
            let (do_log, frames_sent, acks_recv, rtt_ms, min_rtt_ms, eff_rtt_ms, inflight_bytes, delivery_bps, goodput_ewma_bps, avg_frame_bytes, display_fps, probe_frames, goodput_bps, window_bytes, browser_backlog_frames, browser_ack_ahead_frames, browser_apply_ms) = {
                let Some(c) = sess.clients.get_mut(&client_id) else {
                    continue;
                };
                c.acks_recv += 1;
                record_ack(c);
                let do_log = c.last_log.elapsed().as_secs_f32() >= 1.0;
                let log_elapsed = c.last_log.elapsed().as_secs_f32().max(1.0e-3);
                let out = (
                    do_log,
                    c.frames_sent,
                    c.acks_recv,
                    c.rtt_ms,
                    path_rtt_ms(c),
                    effective_rtt_ms(c),
                    c.inflight_bytes,
                    c.delivery_bps,
                    c.goodput_bps,
                    c.avg_frame_bytes,
                    c.display_fps,
                    c.probe_frames,
                    c.acked_bytes_since_log as f32 / log_elapsed,
                    target_byte_window(c),
                    c.browser_backlog_frames,
                    c.browser_ack_ahead_frames,
                    c.browser_apply_ms,
                );
                if do_log {
                    c.frames_sent = 0;
                    c.acks_recv = 0;
                    c.acked_bytes_since_log = 0;
                    c.last_log = Instant::now();
                }
                out
            };
            if do_log {
                let frames = frame_window(eff_rtt_ms, display_fps)
                    .saturating_add(probe_frames.round().max(0.0) as usize);
                eprintln!(
                    "client {client_id}: sent={frames_sent} acks={acks_recv} rtt={rtt_ms:.0}ms min_rtt={min_rtt_ms:.0}ms eff_rtt={eff_rtt_ms:.0}ms window={frames}f/{window_bytes}B probe={probe_frames:.0}f inflight={inflight_bytes}B goodput={goodput_bps:.0}B/s goodput_ewma={goodput_ewma_bps:.0}B/s rate={delivery_bps:.0}B/s avg_frame={avg_frame_bytes:.0}B display_fps={display_fps:.0} backlog={browser_backlog_frames} ack_ahead={browser_ack_ahead_frames} apply={browser_apply_ms:.1}ms | tick_fires={} tick_snaps={}",
                    sess.tick_fires, sess.tick_snaps,
                );
                sess.tick_fires = 0;
                sess.tick_snaps = 0;
            }
            nudge_delivery(&state);
            continue;
        }

        if data[0] == C2S_DISPLAY_RATE && data.len() >= 3 {
            let fps = u16::from_le_bytes([data[1], data[2]]) as f32;
            if fps >= 10.0 && fps <= 1000.0 {
                let mut sess = state.1.lock().await;
                if let Some(c) = sess.clients.get_mut(&client_id) {
                    c.display_fps = fps;
                }
            }
            nudge_delivery(&state);
            continue;
        }

        if data[0] == C2S_CLIENT_METRICS && data.len() >= 7 {
            let backlog_frames = u16::from_le_bytes([data[1], data[2]]);
            let ack_ahead_frames = u16::from_le_bytes([data[3], data[4]]);
            let apply_ms = u16::from_le_bytes([data[5], data[6]]) as f32 * 0.1;
            let mut sess = state.1.lock().await;
            if let Some(c) = sess.clients.get_mut(&client_id) {
                c.browser_backlog_frames = backlog_frames;
                c.browser_ack_ahead_frames = ack_ahead_frames;
                c.browser_apply_ms = apply_ms;
            }
            continue;
        }

        if data[0] == C2S_INPUT && data.len() >= 3 {
            let pid = u16::from_le_bytes([data[1], data[2]]);
            let mut need_nudge = false;
            {
                let mut sess = state.1.lock().await;
                if let Some(c) = sess.clients.get_mut(&client_id) {
                    if c.scroll_offset > 0 {
                        c.scroll_offset = 0;
                        c.scroll_snap.clear();
                        c.last_sent_snap.clear();
                        reset_inflight(c);
                        if let Some(pty) = sess.ptys.get_mut(&pid) {
                            pty.dirty = true;
                            need_nudge = true;
                        }
                    }
                }
            }
            if need_nudge {
                nudge_delivery(&state);
            }
            if let Some(&fd) = state.2.read().unwrap().get(&pid) {
                unsafe {
                    libc::write(fd, data[3..].as_ptr().cast(), data.len() - 3);
                }
            }
            continue;
        }

        let mut sess = state.1.lock().await;
        let mut need_nudge = false;
        match data[0] {
            C2S_SCROLL if data.len() >= 7 => {
                let pid = u16::from_le_bytes([data[1], data[2]]);
                let offset =
                    u32::from_le_bytes([data[3], data[4], data[5], data[6]]) as usize;
                if sess.ptys.contains_key(&pid) {
                    if let Some(c) = sess.clients.get_mut(&client_id) {
                        if c.scroll_offset != offset {
                            c.scroll_offset = offset;
                            c.scroll_snap.clear();
                            reset_inflight(c);
                            if offset == 0 {
                                c.last_sent_snap.clear();
                            }
                        }
                    }
                    if let Some(pty) = sess.ptys.get_mut(&pid) {
                        pty.dirty = true;
                        need_nudge = true;
                    }
                }
            }
            C2S_RESIZE if data.len() >= 7 => {
                let pid = u16::from_le_bytes([data[1], data[2]]);
                let rows = u16::from_le_bytes([data[3], data[4]]);
                let cols = u16::from_le_bytes([data[5], data[6]]);
                if let Some(c) = sess.clients.get_mut(&client_id) {
                    c.size = Some((rows, cols));
                }
                if sess.ptys.contains_key(&pid) {
                    if let Some((r, c)) = sess.min_size_for_pty(pid) {
                        sess.resize_pty(pid, r, c);
                        need_nudge = true;
                    }
                }
            }
            C2S_CREATE => {
                let (rows, cols) = if data.len() >= 5 {
                    (
                        u16::from_le_bytes([data[1], data[2]]),
                        u16::from_le_bytes([data[3], data[4]]),
                    )
                } else {
                    (24, 80)
                };
                let argv: Option<Vec<&str>> = if data.len() > 5 {
                    std::str::from_utf8(&data[5..]).ok().map(|s| {
                        s.split('\0').filter(|a| !a.is_empty()).collect::<Vec<_>>()
                    }).filter(|v| !v.is_empty())
                } else {
                    None
                };
                let id = sess.next_pty_id;
                sess.next_pty_id += 1;
                if let Some(pty) = spawn_pty(&config.shell, rows, cols, id, argv.as_deref(), config.scrollback, state.clone()) {
                    sess.ptys.insert(id, pty);
                    if let Some(c) = sess.clients.get_mut(&client_id) {
                        c.focus = Some(id);
                        c.scroll_offset = 0;
                        c.scroll_snap.clear();
                        reset_inflight(c);
                    }
                    let mut msg = vec![S2C_CREATED];
                    msg.extend_from_slice(&id.to_le_bytes());
                    sess.send_to_all(&msg);
                    need_nudge = true;
                }
            }
            C2S_FOCUS if data.len() >= 3 => {
                let pid = u16::from_le_bytes([data[1], data[2]]);
                if sess.ptys.contains_key(&pid) {
                    let old_pid = sess.clients.get(&client_id).and_then(|c| c.focus);
                    if let Some(c) = sess.clients.get_mut(&client_id) {
                        c.focus = Some(pid);
                        c.scroll_offset = 0;
                        c.scroll_snap.clear();
                        c.last_sent_snap.clear();
                        reset_inflight(c);
                    }
                    if let Some(pty) = sess.ptys.get_mut(&pid) {
                        pty.dirty = true;
                        need_nudge = true;
                    }
                    if let Some((r, c)) = sess.min_size_for_pty(pid) {
                        sess.resize_pty(pid, r, c);
                        need_nudge = true;
                    }
                    if let Some(old) = old_pid {
                        if old != pid {
                            if let Some((r, c)) = sess.min_size_for_pty(old) {
                                sess.resize_pty(old, r, c);
                            }
                        }
                    }
                }
            }
            C2S_CLOSE if data.len() >= 3 => {
                let pid = u16::from_le_bytes([data[1], data[2]]);
                if let Some(pty) = sess.ptys.remove(&pid) {
                    state.2.write().unwrap().remove(&pid);
                    pty.reader_handle.abort();
                    unsafe {
                        libc::kill(pty.child_pid, libc::SIGHUP);
                        libc::close(pty.master_fd);
                    }
                    let mut msg = vec![S2C_CLOSED];
                    msg.extend_from_slice(&pid.to_le_bytes());
                    sess.send_to_all(&msg);
                }
            }
            _ => {}
        }
        drop(sess);
        if need_nudge {
            nudge_delivery(&state);
        }
    }

    {
        let mut sess = state.1.lock().await;
        let mut need_nudge = false;
        let old_focus = sess.clients.get(&client_id).and_then(|c| c.focus);
        sess.clients.remove(&client_id);
        if let Some(pid) = old_focus {
            if let Some((r, c)) = sess.min_size_for_pty(pid) {
                sess.resize_pty(pid, r, c);
                need_nudge = true;
            }
        }
        drop(sess);
        if need_nudge {
            nudge_delivery(&state);
        }
    }
    sender.abort();
    eprintln!("client disconnected");
}
