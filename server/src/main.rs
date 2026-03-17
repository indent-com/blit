use lz4_flex::compress_prepend_size;
use std::collections::HashMap;
use std::ffi::CString;
use std::os::unix::io::{AsRawFd, RawFd};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::unix::AsyncFd;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::UnixListener;
use tokio::sync::{mpsc, Mutex};

type PtyFds = Arc<std::sync::RwLock<HashMap<u16, RawFd>>>;

const C2S_INPUT: u8 = 0x00;
const C2S_RESIZE: u8 = 0x01;
const C2S_SCROLL: u8 = 0x02;
const C2S_ACK: u8 = 0x03;
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

const SCROLLBACK_ROWS: usize = 100_000;

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
}

struct OwnedFd(RawFd);
impl AsRawFd for OwnedFd {
    fn as_raw_fd(&self) -> RawFd {
        self.0
    }
}

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
}

struct ClientState {
    tx: mpsc::Sender<Vec<u8>>,
    focus: Option<u16>,
    size: Option<(u16, u16)>,
    scroll_offset: usize,
    scroll_snap: Vec<u8>,
    last_sent_snap: Vec<u8>,
}

struct Session {
    ptys: HashMap<u16, Pty>,
    next_pty_id: u16,
    next_client_id: u64,
    clients: HashMap<u64, ClientState>,
}

impl Session {
    fn new() -> Self {
        Self {
            ptys: HashMap::new(),
            next_pty_id: 1,
            next_client_id: 1,
            clients: HashMap::new(),
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

type AppState = Arc<(Config, Mutex<Session>, PtyFds)>;

fn spawn_pty(shell: &str, rows: u16, cols: u16, id: u16, state: AppState) -> Pty {
    let mut master: libc::c_int = 0;
    let mut slave: libc::c_int = 0;
    unsafe {
        assert!(
            libc::openpty(
                &mut master,
                &mut slave,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut()
            ) == 0,
            "openpty failed"
        );
        let ws = libc::winsize {
            ws_row: rows,
            ws_col: cols,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        libc::ioctl(master, libc::TIOCSWINSZ, &ws);
    }

    let shell_c = CString::new(shell).unwrap();
    let login_flag = CString::new("-l").unwrap();
    let pid = unsafe { libc::fork() };
    assert!(pid >= 0, "fork failed");

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

    Pty {
        master_fd: master,
        child_pid: pid,
        parser: vt100::Parser::new_with_callbacks(
            rows,
            cols,
            SCROLLBACK_ROWS,
            TitleCallbacks::default(),
        ),
        dirty: true,
        reader_handle,
    }
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

    loop {
        let mut guard = match async_fd.readable().await {
            Ok(g) => g,
            Err(_) => break,
        };

        let n = unsafe { libc::read(fd, buf.as_mut_ptr().cast(), buf.len()) };
        if n > 0 {
            let data = &buf[..n as usize];
            let mut sess = state.1.lock().await;
            if let Some(pty) = sess.ptys.get_mut(&pty_id) {
                pty.parser.process(data);
                pty.dirty = true;
                respond_to_queries(fd, data, pty.parser.screen());
            }
            drop(sess);
            tokio::task::yield_now().await;
            continue;
        } else if n == 0 {
            break;
        } else {
            let err = std::io::Error::last_os_error();
            if err.kind() == std::io::ErrorKind::WouldBlock {
                guard.clear_ready();
                continue;
            }
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
) -> Vec<u8> {
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

    let payload_size = 10 + bitmask_len + dirty_count * CELL_SIZE;
    let mut payload = Vec::with_capacity(payload_size);
    payload.extend_from_slice(&rows.to_le_bytes());
    payload.extend_from_slice(&cols.to_le_bytes());
    payload.extend_from_slice(&cursor.0.to_le_bytes());
    payload.extend_from_slice(&cursor.1.to_le_bytes());
    payload.extend_from_slice(&mode.to_le_bytes());
    payload.extend_from_slice(&bitmask);

    for i in 0..total_cells {
        if bitmask[i / 8] & (1 << (i % 8)) != 0 {
            let off = i * CELL_SIZE;
            payload.extend_from_slice(&snap[off..off + CELL_SIZE]);
        }
    }

    let compressed = compress_prepend_size(&payload);
    let mut msg = Vec::with_capacity(3 + compressed.len());
    msg.push(S2C_UPDATE);
    msg.extend_from_slice(&id.to_le_bytes());
    msg.extend_from_slice(&compressed);
    msg
}

struct PtySnapshot {
    snap: Vec<u8>,
    cursor: (u16, u16),
    mode: u16,
    rows: u16,
    cols: u16,
}

fn take_snapshot(pty: &Pty) -> PtySnapshot {
    let (echo, icanon) = pty_lflag(pty.master_fd);
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
) -> (Vec<u8>, Vec<u8>) {
    let screen = pty.parser.screen_mut();
    let old_sb = screen.scrollback();
    screen.set_scrollback(offset);

    let snap = snapshot_screen(screen);
    let (rows, cols) = screen.size();
    let mode: u16 = 0;
    let cursor = (0u16, 0u16);

    let msg = build_snapshot_msg(id, &snap, prev_snap, rows, cols, cursor, mode);

    screen.set_scrollback(old_sb);
    (msg, snap)
}

#[tokio::main]
async fn main() {
    let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".into());
    let sock_path = std::env::var("BLIT_SOCK").unwrap_or_else(|_| "/tmp/blit.sock".into());

    let _ = std::fs::remove_file(&sock_path);

    let state: AppState = Arc::new((
        Config { shell },
        Mutex::new(Session::new()),
        Arc::new(std::sync::RwLock::new(HashMap::new())),
    ));

    let tick_state = state.clone();
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_millis(4));
        loop {
            interval.tick().await;
            tick(&tick_state).await;
        }
    });

    let listener = UnixListener::bind(&sock_path).unwrap();
    eprintln!("listening on {sock_path}");

    loop {
        let (stream, _) = listener.accept().await.unwrap();
        let state = state.clone();
        tokio::spawn(handle_client(stream, state));
    }
}

async fn tick(state: &AppState) {
    let mut sess = state.1.lock().await;

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
        }
    }

    let mut snapshots: HashMap<u16, PtySnapshot> = HashMap::new();
    for &id in &ids {
        let pty = sess.ptys.get_mut(&id).unwrap();
        if !pty.dirty {
            continue;
        }
        snapshots.insert(id, take_snapshot(pty));
        pty.dirty = false;
    }

    let client_ids: Vec<u64> = sess.clients.keys().copied().collect();
    for cid in client_ids {
        let (focus, scroll_offset) = {
            let c = sess.clients.get(&cid).unwrap();
            (c.focus, c.scroll_offset)
        };
        let Some(pid) = focus else { continue };

        let sent = if scroll_offset == 0 {
            let Some(cur) = snapshots.get(&pid) else {
                continue;
            };
            let prev = &sess.clients.get(&cid).unwrap().last_sent_snap;
            let msg = build_snapshot_msg(
                pid, &cur.snap, prev, cur.rows, cur.cols, cur.cursor, cur.mode,
            );
            let c = sess.clients.get_mut(&cid).unwrap();
            if c.tx.try_send(msg).is_ok() {
                c.last_sent_snap = cur.snap.clone();
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
                let (msg, new_snap) =
                    build_scrollback_update(pty, pid, scroll_offset, &prev_snap);
                let c = sess.clients.get_mut(&cid).unwrap();
                if c.tx.try_send(msg).is_ok() {
                    c.scroll_snap = new_snap;
                    true
                } else {
                    false
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
}

async fn handle_client(stream: tokio::net::UnixStream, state: AppState) {
    let config = &state.0;
    let (mut reader, mut writer) = stream.into_split();

    let (out_tx, mut out_rx) = mpsc::channel::<Vec<u8>>(2);
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
            },
        );
        let list = sess.pty_list_msg();
        if let Some(c) = sess.clients.get(&client_id) {
            let _ = c.tx.try_send(list);
        }
    }

    eprintln!("client connected");

    let sender = tokio::spawn(async move {
        while let Some(msg) = out_rx.recv().await {
            if !write_frame(&mut writer, &msg).await {
                break;
            }
        }
    });

    while let Some(data) = read_frame(&mut reader).await {
        if data.is_empty() {
            continue;
        }

        if data[0] == C2S_ACK {
            continue;
        }

        if data[0] == C2S_INPUT && data.len() >= 3 {
            let pid = u16::from_le_bytes([data[1], data[2]]);
            {
                let mut sess = state.1.lock().await;
                if let Some(c) = sess.clients.get_mut(&client_id) {
                    if c.scroll_offset > 0 {
                        c.scroll_offset = 0;
                        c.scroll_snap.clear();
                        c.last_sent_snap.clear();
                        if let Some(pty) = sess.ptys.get_mut(&pid) {
                            pty.dirty = true;
                        }
                    }
                }
            }
            if let Some(&fd) = state.2.read().unwrap().get(&pid) {
                unsafe {
                    libc::write(fd, data[3..].as_ptr().cast(), data.len() - 3);
                }
            }
            continue;
        }

        let mut sess = state.1.lock().await;
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
                            if offset == 0 {
                                c.last_sent_snap.clear();
                            }
                        }
                    }
                    if let Some(pty) = sess.ptys.get_mut(&pid) {
                        pty.dirty = true;
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
                let id = sess.next_pty_id;
                sess.next_pty_id += 1;
                let pty = spawn_pty(&config.shell, rows, cols, id, state.clone());
                sess.ptys.insert(id, pty);
                if let Some(c) = sess.clients.get_mut(&client_id) {
                    c.focus = Some(id);
                    c.scroll_offset = 0;
                    c.scroll_snap.clear();
                }
                let mut msg = vec![S2C_CREATED];
                msg.extend_from_slice(&id.to_le_bytes());
                sess.send_to_all(&msg);
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
                    }
                    if let Some(pty) = sess.ptys.get_mut(&pid) {
                        pty.dirty = true;
                    }
                    if let Some((r, c)) = sess.min_size_for_pty(pid) {
                        sess.resize_pty(pid, r, c);
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
    }

    {
        let mut sess = state.1.lock().await;
        let old_focus = sess.clients.get(&client_id).and_then(|c| c.focus);
        sess.clients.remove(&client_id);
        if let Some(pid) = old_focus {
            if let Some((r, c)) = sess.min_size_for_pty(pid) {
                sess.resize_pty(pid, r, c);
            }
        }
    }
    sender.abort();
    eprintln!("client disconnected");
}
