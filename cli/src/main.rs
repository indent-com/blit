use blit_remote::{
    msg_ack, msg_close, msg_create, msg_focus, msg_input, msg_resize, TerminalState,
    C2S_DISPLAY_RATE, CELL_SIZE, S2C_CLOSED, S2C_CREATED, S2C_CREATED_N, S2C_HELLO, S2C_LIST,
    S2C_TITLE, S2C_UPDATE,
};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::sync::mpsc;

// Browser mode imports
use axum::extract::ws::{Message, WebSocket};
use axum::extract::{FromRequest, WebSocketUpgrade};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::get;
use futures_util::{SinkExt, StreamExt};
use std::sync::Arc;

const WEB_INDEX_HTML: &str = include_str!("../../web-app/dist/index.html");

// ── Terminal size ─────────────────────────────────────────────────────────────
fn term_size() -> (u16, u16) {
    unsafe {
        let mut ws: libc::winsize = std::mem::zeroed();
        if libc::ioctl(libc::STDOUT_FILENO, libc::TIOCGWINSZ, &mut ws) == 0
            && ws.ws_row > 0
            && ws.ws_col > 0
        {
            return (ws.ws_row, ws.ws_col);
        }
    }
    (24, 80)
}

// ── Raw mode ──────────────────────────────────────────────────────────────────
struct RawMode {
    saved: libc::termios,
}

impl RawMode {
    fn enter() -> Option<Self> {
        unsafe {
            let mut saved: libc::termios = std::mem::zeroed();
            if libc::tcgetattr(libc::STDIN_FILENO, &mut saved) != 0 {
                return None;
            }
            let mut raw = saved;
            libc::cfmakeraw(&mut raw);
            raw.c_cc[libc::VMIN] = 1;
            raw.c_cc[libc::VTIME] = 0;
            libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, &raw);
            Some(Self { saved })
        }
    }
}

impl Drop for RawMode {
    fn drop(&mut self) {
        unsafe {
            libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, &self.saved);
        }
    }
}

// ── Cleanup on exit ───────────────────────────────────────────────────────────
struct Cleanup;
impl Drop for Cleanup {
    fn drop(&mut self) {
        // Reset cursor shape, disable mouse, reset modes, show cursor,
        // reset attributes, leave alternate screen, move to bottom, newline.
        const RESET: &[u8] = b"\x1b[ q\
                               \x1b[?1003l\x1b[?1002l\x1b[?1000l\x1b[?9l\
                               \x1b[?1016l\x1b[?1006l\x1b[?1005l\
                               \x1b[?2004l\x1b>\x1b[?1l\x1b[?25h\x1b[0m\x1b[?1049l\
                               \x1b[999;1H\r\n";
        unsafe {
            libc::write(libc::STDOUT_FILENO, RESET.as_ptr().cast(), RESET.len());
        }
    }
}

// ── Framing ───────────────────────────────────────────────────────────────────
const MAX_FRAME_SIZE: usize = 16 * 1024 * 1024;

async fn read_frame(r: &mut (impl AsyncRead + Unpin)) -> Option<Vec<u8>> {
    let mut hdr = [0u8; 4];
    r.read_exact(&mut hdr).await.ok()?;
    let len = u32::from_le_bytes(hdr) as usize;
    if len == 0 {
        return Some(vec![]);
    }
    if len > MAX_FRAME_SIZE {
        return None;
    }
    let mut buf = vec![0u8; len];
    r.read_exact(&mut buf).await.ok()?;
    Some(buf)
}

fn make_frame(payload: &[u8]) -> Vec<u8> {
    debug_assert!(payload.len() <= u32::MAX as usize);
    let mut v = Vec::with_capacity(4 + payload.len());
    v.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    v.extend_from_slice(payload);
    v
}

// ── ANSI renderer ─────────────────────────────────────────────────────────────
struct Renderer {
    prev_rows: u16,
    prev_cols: u16,
    prev_mode: u16,
    cur_row: u16,
    cur_col: u16,
    cur_fg: u64,
    cur_bg: u64,
    cur_attrs: u8,
    attrs_known: bool,
    entered_altscreen: bool,
}

impl Renderer {
    fn new() -> Self {
        Self {
            prev_rows: 0,
            prev_cols: 0,
            prev_mode: 0,
            cur_row: u16::MAX,
            cur_col: u16::MAX,
            cur_fg: u64::MAX,
            cur_bg: u64::MAX,
            cur_attrs: 0,
            attrs_known: false,
            entered_altscreen: false,
        }
    }

    fn render(&mut self, screen: &TerminalState, out: &mut Vec<u8>) {
        let cols = screen.cols();
        let resized = screen.rows() != self.prev_rows || cols != self.prev_cols;

        if !self.entered_altscreen {
            out.extend_from_slice(b"\x1b[?1049h\x1b[?25l");
            self.entered_altscreen = true;
        }

        if resized {
            out.extend_from_slice(b"\x1b[2J");
            self.prev_rows = screen.rows();
            self.prev_cols = cols;
            self.attrs_known = false;
            self.cur_fg = u64::MAX;
            self.cur_bg = u64::MAX;
        }

        // Synchronized output: terminal buffers everything until the end marker,
        // then displays it atomically — prevents tearing.
        out.extend_from_slice(b"\x1b[?2026h");

        self.sync_modes(screen.mode(), out);

        let total = screen.rows() as usize * cols as usize;
        let cols_usize = cols as usize;

        let cells = screen.cells();
        for i in 0..total {
            let off = i * CELL_SIZE;
            let cell = &cells[off..off + CELL_SIZE];
            let row = (i / cols_usize) as u16;
            let col = (i % cols_usize) as u16;
            if cell[1] & 4 != 0 {
                let prev_wide = col > 0 && {
                    let prev_off = off - CELL_SIZE;
                    cells[prev_off + 1] & 2 != 0
                };
                if prev_wide {
                    continue;
                }
                self.move_to(row, col, out);
                self.set_sgr(0, 0, 0, out);
                out.push(b' ');
                self.cur_col = col.saturating_add(1);
                continue;
            }
            self.move_to(row, col, out);
            self.emit_cell(cell, i, screen.frame(), out);
        }

        // Cursor visibility
        let cursor_visible = screen.mode() & 1 != 0;
        let prev_visible = self.prev_mode & 1 != 0;
        if cursor_visible != prev_visible {
            out.extend_from_slice(if cursor_visible {
                b"\x1b[?25h"
            } else {
                b"\x1b[?25l"
            });
        }

        // Cursor shape (DECSCUSR)
        let cursor_style = (screen.mode() >> 12) & 7;
        let prev_cursor_style = (self.prev_mode >> 12) & 7;
        if cursor_style != prev_cursor_style {
            out.extend_from_slice(b"\x1b[");
            push_u16(out, cursor_style);
            out.extend_from_slice(b" q");
        }

        // Place cursor
        self.move_to(screen.cursor_row(), screen.cursor_col(), out);

        // Application cursor keys
        let app_cur = screen.mode() & 2 != 0;
        let prev_app_cur = self.prev_mode & 2 != 0;
        if app_cur != prev_app_cur {
            out.extend_from_slice(if app_cur { b"\x1b[?1h" } else { b"\x1b[?1l" });
        }

        let app_keypad = screen.mode() & 4 != 0;
        let prev_app_keypad = self.prev_mode & 4 != 0;
        if app_keypad != prev_app_keypad {
            out.extend_from_slice(if app_keypad { b"\x1b=" } else { b"\x1b>" });
        }

        self.prev_mode = screen.mode();

        // End synchronized output
        out.extend_from_slice(b"\x1b[?2026l");
    }

    fn emit_cell(&mut self, cell: &[u8], cell_index: usize, frame: &blit_remote::FrameState, out: &mut Vec<u8>) {
        let f0 = cell[0];
        let f1 = cell[1];

        let fg_type = f0 & 3;
        let bg_type = (f0 >> 2) & 3;
        let bold = (f0 >> 4) & 1;
        let dim = (f0 >> 5) & 1;
        let italic = (f0 >> 6) & 1;
        let underline = (f0 >> 7) & 1;
        let inverse = f1 & 1;
        let wide = (f1 >> 1) & 1;
        let content_len = ((f1 >> 3) & 7) as usize;

        // Colors are pre-resolved for inverse — don't include inverse in
        // attrs, otherwise the terminal would double-invert.
        let attrs: u8 = bold | (dim << 1) | (italic << 2) | (underline << 3);

        let (fg_packed, bg_packed) = if inverse != 0 {
            (
                pack_color(bg_type, cell[5], cell[6], cell[7]),
                pack_color(fg_type, cell[2], cell[3], cell[4]),
            )
        } else {
            (
                pack_color(fg_type, cell[2], cell[3], cell[4]),
                pack_color(bg_type, cell[5], cell[6], cell[7]),
            )
        };

        self.set_sgr(attrs, fg_packed, bg_packed, out);

        if content_len == 7 {
            // Overflow: look up the full string from the overflow table.
            if let Some(s) = frame.overflow().get(&cell_index) {
                out.extend_from_slice(s.as_bytes());
            } else {
                out.push(b' ');
            }
        } else if content_len > 0 && content_len <= 4 {
            if let Ok(s) = std::str::from_utf8(&cell[8..8 + content_len]) {
                out.extend_from_slice(s.as_bytes());
            } else {
                out.push(b'?');
            }
        } else {
            out.push(b' ');
        }
        self.cur_col = self.cur_col.saturating_add(if wide != 0 { 2 } else { 1 });
    }

    fn set_sgr(&mut self, attrs: u8, fg: u64, bg: u64, out: &mut Vec<u8>) {
        if self.attrs_known && self.cur_attrs == attrs && self.cur_fg == fg && self.cur_bg == bg {
            return;
        }

        // Only reset when attributes need to be *removed*.  Adding new
        // attributes (bold, italic, …) never requires a full reset.
        let need_reset = !self.attrs_known || (self.cur_attrs & !attrs) != 0;

        if need_reset {
            out.extend_from_slice(b"\x1b[0m");
            self.cur_attrs = 0;
            self.cur_fg = u64::MAX;
            self.cur_bg = u64::MAX;
            self.attrs_known = true;
        }

        if attrs & 1 != 0 && self.cur_attrs & 1 == 0 {
            out.extend_from_slice(b"\x1b[1m");
        }
        if attrs & 2 != 0 && self.cur_attrs & 2 == 0 {
            out.extend_from_slice(b"\x1b[2m");
        }
        if attrs & 4 != 0 && self.cur_attrs & 4 == 0 {
            out.extend_from_slice(b"\x1b[3m");
        }
        if attrs & 8 != 0 && self.cur_attrs & 8 == 0 {
            out.extend_from_slice(b"\x1b[4m");
        }
        if attrs & 16 != 0 && self.cur_attrs & 16 == 0 {
            out.extend_from_slice(b"\x1b[7m");
        }
        self.cur_attrs = attrs;

        if fg != self.cur_fg {
            emit_color(fg, true, out);
            self.cur_fg = fg;
        }
        if bg != self.cur_bg {
            emit_color(bg, false, out);
            self.cur_bg = bg;
        }
    }

    fn sync_modes(&mut self, mode: u16, out: &mut Vec<u8>) {
        let bp = (mode >> 3) & 1;
        let prev_bp = (self.prev_mode >> 3) & 1;
        if bp != prev_bp {
            out.extend_from_slice(if bp != 0 {
                b"\x1b[?2004h"
            } else {
                b"\x1b[?2004l"
            });
        }

        let mouse = (mode >> 4) & 7;
        let prev_mouse = (self.prev_mode >> 4) & 7;
        if mouse != prev_mouse {
            match prev_mouse {
                1 => out.extend_from_slice(b"\x1b[?9l"),
                2 => out.extend_from_slice(b"\x1b[?1000l"),
                3 => out.extend_from_slice(b"\x1b[?1002l"),
                4 => out.extend_from_slice(b"\x1b[?1003l"),
                _ => {}
            }
            match mouse {
                1 => out.extend_from_slice(b"\x1b[?9h"),
                2 => out.extend_from_slice(b"\x1b[?1000h"),
                3 => out.extend_from_slice(b"\x1b[?1002h"),
                4 => out.extend_from_slice(b"\x1b[?1003h"),
                _ => {}
            }
        }

        let enc = (mode >> 7) & 3;
        let prev_enc = (self.prev_mode >> 7) & 3;
        if enc != prev_enc {
            match prev_enc {
                1 => out.extend_from_slice(b"\x1b[?1005l"),
                2 => out.extend_from_slice(b"\x1b[?1006l"),
                3 => out.extend_from_slice(b"\x1b[?1016l"),
                _ => {}
            }
            match enc {
                1 => out.extend_from_slice(b"\x1b[?1005h"),
                2 => out.extend_from_slice(b"\x1b[?1006h"),
                3 => out.extend_from_slice(b"\x1b[?1016h"),
                _ => {}
            }
        }
    }

    fn move_to(&mut self, row: u16, col: u16, out: &mut Vec<u8>) {
        if self.cur_row == row && self.cur_col == col {
            return;
        }
        out.extend_from_slice(b"\x1b[");
        push_u16(out, row + 1);
        out.push(b';');
        push_u16(out, col + 1);
        out.push(b'H');
        self.cur_row = row;
        self.cur_col = col;
    }
}

fn pack_color(color_type: u8, r: u8, g: u8, b: u8) -> u64 {
    ((color_type as u64) << 24) | ((r as u64) << 16) | ((g as u64) << 8) | b as u64
}

fn emit_color(packed: u64, is_fg: bool, out: &mut Vec<u8>) {
    let color_type = ((packed >> 24) & 0xff) as u8;
    let r = ((packed >> 16) & 0xff) as u8;
    let g = ((packed >> 8) & 0xff) as u8;
    let b = (packed & 0xff) as u8;
    let base: u8 = if is_fg { 38 } else { 48 };
    let default_seq: &[u8] = if is_fg { b"\x1b[39m" } else { b"\x1b[49m" };
    match color_type {
        0 => out.extend_from_slice(default_seq),
        1 => {
            out.extend_from_slice(b"\x1b[");
            push_u16(out, base as u16);
            out.extend_from_slice(b";5;");
            push_u16(out, r as u16);
            out.push(b'm');
        }
        2 => {
            out.extend_from_slice(b"\x1b[");
            push_u16(out, base as u16);
            out.extend_from_slice(b";2;");
            push_u16(out, r as u16);
            out.push(b';');
            push_u16(out, g as u16);
            out.push(b';');
            push_u16(out, b as u16);
            out.push(b'm');
        }
        _ => out.extend_from_slice(default_seq),
    }
}

fn push_u16(out: &mut Vec<u8>, n: u16) {
    let mut buf = [0u8; 5];
    let mut pos = buf.len();
    let mut v = n;
    if v == 0 {
        out.push(b'0');
        return;
    }
    while v > 0 {
        pos -= 1;
        buf[pos] = b'0' + (v % 10) as u8;
        v /= 10;
    }
    out.extend_from_slice(&buf[pos..]);
}

fn write_all_stdout(buf: &[u8]) {
    let mut off = 0;
    while off < buf.len() {
        let n = unsafe {
            libc::write(
                libc::STDOUT_FILENO,
                buf[off..].as_ptr().cast(),
                buf.len() - off,
            )
        };
        if n > 0 {
            off += n as usize;
        } else if n < 0 {
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

// ── Transport ─────────────────────────────────────────────────────────────────
enum Transport {
    Unix(tokio::net::UnixStream),
    Tcp(tokio::net::TcpStream),
    Ssh(tokio::process::Child),
}

fn default_local_socket() -> String {
    if let Ok(p) = std::env::var("BLIT_SOCK") {
        return p;
    }
    // System-wide socket activation: /run/blit/<user>.sock
    if let Ok(user) = std::env::var("USER") {
        let sys = format!("/run/blit/{user}.sock");
        if std::path::Path::new(&sys).exists() {
            return sys;
        }
    }
    // User-level fallback
    if let Ok(dir) = std::env::var("XDG_RUNTIME_DIR") {
        return format!("{dir}/blit.sock");
    }
    "/tmp/blit.sock".into()
}

impl Transport {
    fn split(
        self,
    ) -> (
        Box<dyn AsyncRead + Unpin + Send>,
        Box<dyn AsyncWrite + Unpin + Send>,
    ) {
        match self {
            Transport::Unix(s) => {
                let (r, w) = tokio::io::split(s);
                (Box::new(r), Box::new(w))
            }
            Transport::Tcp(s) => {
                let (r, w) = tokio::io::split(s);
                (Box::new(r), Box::new(w))
            }
            Transport::Ssh(mut child) => {
                let stdout = child.stdout.take().expect("ssh stdout");
                let stdin = child.stdin.take().expect("ssh stdin");
                tokio::spawn(async move {
                    let _ = child.wait().await;
                });
                (Box::new(stdout), Box::new(stdin))
            }
        }
    }
}

// ── Main ──────────────────────────────────────────────────────────────────────
#[tokio::main]
async fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.iter().any(|a| a == "--version" || a == "-V") {
        println!("blit {}", env!("CARGO_PKG_VERSION"));
        return;
    }
    if args.iter().any(|a| a == "--help" || a == "-h") {
        println!("blit {} — terminal streaming client", env!("CARGO_PKG_VERSION"));
        println!();
        println!("USAGE: blit [OPTIONS] [HOST]");
        println!();
        println!("OPTIONS:");
        println!("  --console           Render to terminal instead of opening browser");
        println!("  --port PORT         Bind browser UI to a specific port (default: random)");
        println!("  --socket PATH       Connect to a specific Unix socket");
        println!("  --tcp HOST:PORT     Connect via raw TCP");
        println!("  --help, -h          Print this help");
        println!("  --version, -V       Print version");
        return;
    }
    let console_mode = args.iter().any(|a| a == "--console");
    if console_mode {
        let filtered_args: Vec<String> =
            args.iter().filter(|a| a.as_str() != "--console").cloned().collect();
        let transport = match connect(&filtered_args).await {
            Ok(t) => t,
            Err(e) => {
                eprintln!("blit: {e}");
                std::process::exit(1);
            }
        };
        run(transport).await;
    } else {
        let filtered_args: Vec<String> =
            args.iter().filter(|a| a.as_str() != "--browser").cloned().collect();
        run_browser(filtered_args).await;
    }
}

// ── Browser mode ──────────────────────────────────────────────────────────────

/// How each WS client connects to the blit-server.
enum BrowserConnector {
    /// Connect to a Unix socket directly (local server).
    Unix(String),
    /// An SSH -L forward maps a local Unix socket to the remote one.
    /// Each WS client connects to the local socket; SSH multiplexes
    /// all connections over one TCP connection.
    SshForward {
        local_sock: String,
        // Keep the SSH process alive for the lifetime of the server.
        _ssh_child: tokio::process::Child,
    },
    /// TCP passthrough.
    Tcp(String),
}

impl BrowserConnector {
    async fn connect(&self) -> Result<Transport, String> {
        let path = match self {
            Self::Unix(p) | Self::SshForward { local_sock: p, .. } => p,
            Self::Tcp(addr) => {
                let s = tokio::net::TcpStream::connect(addr.as_str())
                    .await
                    .map_err(|e| format!("cannot connect to {addr}: {e}"))?;
                let _ = s.set_nodelay(true);
                return Ok(Transport::Tcp(s));
            }
        };
        Ok(Transport::Unix(
            tokio::net::UnixStream::connect(path)
                .await
                .map_err(|e| format!("cannot connect to {path}: {e}"))?,
        ))
    }
}

struct BrowserState {
    token: String,
    connector: BrowserConnector,
}

async fn run_browser(args: Vec<String>) {
    let token: String = {
        use rand::Rng;
        let mut rng = rand::thread_rng();
        (0..32).map(|_| rng.sample(rand::distributions::Alphanumeric) as char).collect()
    };

    // Extract --port before connector parsing.
    let mut bind_port: u16 = 0;
    let args: Vec<String> = {
        let mut filtered = Vec::new();
        let mut skip_next = false;
        for (i, arg) in args.iter().enumerate() {
            if skip_next { skip_next = false; continue; }
            if arg == "--port" {
                if let Some(p) = args.get(i + 1) {
                    bind_port = p.parse().unwrap_or_else(|_| {
                        eprintln!("blit: --port requires a valid port number");
                        std::process::exit(1);
                    });
                    skip_next = true;
                } else {
                    eprintln!("blit: --port requires a port number");
                    std::process::exit(1);
                }
            } else {
                filtered.push(arg.clone());
            }
        }
        filtered
    };

    let flag = args.get(1).map(|s| s.as_str());

    let mut remote_host: Option<String> = None;

    let connector = if flag == Some("--tcp") {
        let addr = args.get(2).unwrap_or_else(|| {
            eprintln!("blit: --tcp requires HOST:PORT");
            std::process::exit(1);
        });
        remote_host = Some(addr.split(':').next().unwrap_or(addr).to_string());
        BrowserConnector::Tcp(addr.clone())
    } else if flag == Some("--socket") || flag == Some("-s") {
        let path = args.get(2).unwrap_or_else(|| {
            eprintln!("blit: --socket requires a path");
            std::process::exit(1);
        });
        BrowserConnector::Unix(path.clone())
    } else if flag == Some("--ssh") || flag == Some("ssh") {
        // Explicit --ssh: remaining args are SSH args
        if let Some(target) = args.get(2) {
            remote_host = Some(target.to_string());
        }
        setup_ssh_forward(&args[2..]).await
    } else if flag.is_some() && !flag.unwrap().starts_with('-') {
        // Bare hostname: treat as SSH target
        remote_host = Some(flag.unwrap().to_string());
        setup_ssh_forward(&args[1..]).await
    } else {
        // No args: local Unix socket
        let path = default_local_socket();
        BrowserConnector::Unix(path)
    };

    // Verify the connection works before opening the browser.
    // For SSH forwards, retry briefly while sshd sets up the socket.
    let mut attempts = 0;
    loop {
        match connector.connect().await {
            Ok(_transport) => break, // drop the test connection
            Err(e) if attempts < 30 => {
                attempts += 1;
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                if attempts == 30 {
                    eprintln!("blit: {e}");
                    std::process::exit(1);
                }
            }
            Err(e) => {
                eprintln!("blit: {e}");
                std::process::exit(1);
            }
        }
    }

    let state = Arc::new(BrowserState {
        token: token.clone(),
        connector,
    });

    fn js_escape(s: &str) -> String {
        s.replace('\\', "\\\\")
            .replace('\'', "\\'")
            .replace('<', "\\x3c")
            .replace('>', "\\x3e")
    }
    let host_injection = match &remote_host {
        Some(h) => format!("localStorage.setItem('blit.host','{}');", js_escape(h)),
        None => String::new(),
    };
    let injected_html = WEB_INDEX_HTML.replacen(
        "<script",
        &format!(
            "<script>localStorage.setItem('blit.passphrase','{}');{host_injection}</script>\n<script",
            js_escape(&token)
        ),
        1,
    );
    let injected_html: &'static str = Box::leak(injected_html.into_boxed_str());

    let app = axum::Router::new()
        .fallback(get(move |state, request| browser_root_handler(state, request, injected_html)))
        .with_state(state);

    let listener = tokio::net::TcpListener::bind(format!("127.0.0.1:{bind_port}")).await.unwrap_or_else(|e| {
        eprintln!("blit: cannot bind to port {bind_port}: {e}");
        std::process::exit(1);
    });
    let addr = listener.local_addr().unwrap();
    let url = format!("http://{addr}");
    eprintln!("blit: serving browser UI at {url}");

    open_browser(&url);

    tokio::select! {
        r = axum::serve(listener, app) => { if let Err(e) = r { eprintln!("blit: serve error: {e}"); } }
        _ = tokio::signal::ctrl_c() => {}
    }
}

async fn setup_ssh_forward(ssh_args: &[String]) -> BrowserConnector {
    if ssh_args.is_empty() {
        eprintln!("blit: ssh requires a host argument");
        std::process::exit(1);
    }

    // Resolve the remote socket path via a quick SSH command.
    // SSH -L doesn't expand shell variables, so we need the absolute path.
    let resolve = tokio::process::Command::new("ssh")
        .arg("-T")
        .arg("-o").arg("ControlMaster=auto")
        .arg("-o").arg("ControlPath=/tmp/blit-ssh-%r@%h:%p")
        .arg("-o").arg("ControlPersist=300")
        .args(ssh_args)
        .arg("--")
        .arg(r#"sh -c 'echo "${BLIT_SOCK:-/run/blit/$(id -un).sock}"'"#)
        .output()
        .await
        .unwrap_or_else(|e| {
            eprintln!("blit: ssh: {e}");
            std::process::exit(1);
        });

    if !resolve.status.success() {
        let stderr = String::from_utf8_lossy(&resolve.stderr);
        eprintln!("blit: ssh failed to resolve remote socket path: {}", stderr.trim());
        std::process::exit(1);
    }

    let remote_sock = String::from_utf8_lossy(&resolve.stdout).trim().to_owned();
    if remote_sock.is_empty() {
        eprintln!("blit: could not determine remote blit socket path");
        std::process::exit(1);
    }

    let local_sock = format!("/tmp/blit-browser-{}.sock", std::process::id());
    let _ = std::fs::remove_file(&local_sock);

    // SSH -L forwards a local Unix socket to the remote blit.sock.
    // Each browser tab connects to the local socket; SSH multiplexes
    // all of them over one TCP connection as separate channels.
    // The ControlMaster from the resolve step is reused — no extra handshake.
    let child = tokio::process::Command::new("ssh")
        .arg("-N") // no remote command — just forward
        .arg("-T")
        .arg("-o").arg("ControlMaster=auto")
        .arg("-o").arg("ControlPath=/tmp/blit-ssh-%r@%h:%p")
        .arg("-o").arg("ControlPersist=300")
        .arg("-o").arg("ExitOnForwardFailure=yes")
        .arg("-o").arg("StreamLocalBindUnlink=yes")
        .arg("-L").arg(format!("{local_sock}:{remote_sock}"))
        .args(ssh_args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::inherit())
        .spawn()
        .unwrap_or_else(|e| {
            eprintln!("blit: ssh: {e}");
            std::process::exit(1);
        });

    BrowserConnector::SshForward {
        local_sock,
        _ssh_child: child,
    }
}

fn open_browser(url: &str) {
    #[cfg(target_os = "macos")]
    let _ = std::process::Command::new("open").arg(url).spawn();
    #[cfg(target_os = "linux")]
    let _ = std::process::Command::new("xdg-open").arg(url).spawn();
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    eprintln!("blit: open {url} in your browser");
}

async fn browser_root_handler(
    axum::extract::State(state): axum::extract::State<Arc<BrowserState>>,
    request: axum::extract::Request,
    index_html: &'static str,
) -> Response {
    let is_ws = request
        .headers()
        .get("upgrade")
        .and_then(|v| v.to_str().ok())
        .map(|v| v.eq_ignore_ascii_case("websocket"))
        .unwrap_or(false);

    if is_ws {
        match WebSocketUpgrade::from_request(request, &state).await {
            Ok(ws) => ws.on_upgrade(move |socket| browser_handle_ws(socket, state)),
            Err(e) => e.into_response(),
        }
    } else {
        Html(index_html).into_response()
    }
}

async fn browser_handle_ws(mut ws: WebSocket, state: Arc<BrowserState>) {
    // Auth: expect the token as the first text message.
    let authed = loop {
        match ws.recv().await {
            Some(Ok(Message::Text(pass))) => {
                if pass.trim() == state.token {
                    let _ = ws.send(Message::Text("ok".into())).await;
                    break true;
                } else {
                    let _ = ws.close().await;
                    break false;
                }
            }
            Some(Ok(Message::Ping(d))) => {
                let _ = ws.send(Message::Pong(d)).await;
            }
            _ => break false,
        }
    };
    if !authed {
        return;
    }

    // Each WS client gets its own transport to the blit-server — separate
    // session, separate congestion control, separate PTY focus.
    let transport = match state.connector.connect().await {
        Ok(t) => t,
        Err(e) => {
            eprintln!("blit: transport connect failed: {e}");
            let _ = ws.send(Message::Text(format!("error:{e}").into())).await;
            let _ = ws.close().await;
            return;
        }
    };
    eprintln!("blit: browser client connected");

    let (mut transport_reader, mut transport_writer) = transport.split();
    let (mut ws_tx, mut ws_rx) = ws.split();

    // Transport → WS
    let mut transport_to_ws = tokio::spawn(async move {
        let mut frames = 0u64;
        while let Some(data) = read_frame(&mut transport_reader).await {
            frames += 1;
            if ws_tx.send(Message::Binary(data.into())).await.is_err() {
                break;
            }
        }
        if frames == 0 {
            // The server closed before sending any data — likely not running.
            let _ = ws_tx
                .send(Message::Text(
                    "error:blit-server not reachable (is it running on the remote host?)".into(),
                ))
                .await;
        }
    });

    // WS → Transport
    let mut ws_to_transport = tokio::spawn(async move {
        while let Some(msg_result) = ws_rx.next().await {
            match msg_result {
                Ok(Message::Binary(d)) => {
                    let frame = make_frame(&d);
                    if transport_writer.write_all(&frame).await.is_err() {
                        eprintln!("blit: ws→transport: write failed");
                        break;
                    }
                }
                Ok(Message::Close(_)) => break,
                Err(e) => {
                    eprintln!("blit: ws→transport: ws error: {e}");
                    break;
                }
                _ => continue,
            }
        }
    });

    tokio::select! {
        _ = &mut transport_to_ws => {}
        _ = &mut ws_to_transport => {}
    }
    transport_to_ws.abort();
    ws_to_transport.abort();

    eprintln!("blit: browser client disconnected");
}

async fn connect(args: &[String]) -> Result<Transport, String> {
    // blit                          → local Unix socket
    // blit --socket PATH            → explicit Unix socket
    // blit --tcp HOST:PORT          → TCP
    // blit --ssh [SSH_ARGS...] HOST → SSH tunnel
    // blit ssh [SSH_ARGS...] HOST   → same
    let flag = args.get(1).map(|s| s.as_str());

    if flag == Some("--socket") || flag == Some("-s") {
        let path = args.get(2).ok_or("--socket requires a path")?;
        return Ok(Transport::Unix(
            tokio::net::UnixStream::connect(path)
                .await
                .map_err(|e| format!("cannot connect to {path}: {e}"))?,
        ));
    }

    if flag == Some("--tcp") {
        let addr = args.get(2).ok_or("--tcp requires HOST:PORT")?;
        let s = tokio::net::TcpStream::connect(addr.as_str())
            .await
            .map_err(|e| format!("cannot connect to {addr}: {e}"))?;
        let _ = s.set_nodelay(true);
        return Ok(Transport::Tcp(s));
    }

    if flag == Some("--ssh") || flag == Some("ssh") {
        let ssh_args = &args[2..];
        if ssh_args.is_empty() {
            return Err("ssh requires a host argument".into());
        }
        // Bridge: use sh -c to ensure POSIX shell (remote login shell may be fish).
        let bridge = r#"sh -c 'S="${BLIT_SOCK:-/run/blit/$(id -un).sock}"; exec nc -U "$S" 2>/dev/null || socat - "UNIX-CONNECT:$S"'"#;
        let child = tokio::process::Command::new("ssh")
            .arg("-T") // no PTY — raw byte tunnel
            .arg("-o").arg("ControlMaster=auto")
            .arg("-o").arg("ControlPath=/tmp/blit-ssh-%r@%h:%p")
            .arg("-o").arg("ControlPersist=300")
            .args(ssh_args)
            .arg("--")
            .arg(bridge)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .spawn()
            .map_err(|e| format!("ssh: {e}"))?;
        return Ok(Transport::Ssh(child));
    }

    // Default: local socket
    let path = std::env::var("BLIT_SOCK").unwrap_or_else(|_| {
        std::env::var("XDG_RUNTIME_DIR")
            .map(|d| format!("{d}/blit.sock"))
            .unwrap_or_else(|_| "/tmp/blit.sock".into())
    });
    Ok(Transport::Unix(
        tokio::net::UnixStream::connect(&path)
            .await
            .map_err(|e| format!("cannot connect to {path}: {e}"))?,
    ))
}

// ── Events from background tasks ──────────────────────────────────────────────
enum Event {
    Stdin(Vec<u8>),
    Resize(u16, u16),
}

// ── Expose (PTY switcher) ────────────────────────────────────────────────────
struct Expose {
    open: bool,
    selected: usize,
    lru: Vec<u16>,
    titles: std::collections::HashMap<u16, String>,
}

impl Expose {
    fn new() -> Self {
        Self {
            open: false,
            selected: 0,
            lru: Vec::new(),
            titles: std::collections::HashMap::new(),
        }
    }

    fn touch(&mut self, id: u16) {
        if let Some(pos) = self.lru.iter().position(|&x| x == id) {
            self.lru.remove(pos);
        }
        self.lru.insert(0, id);
    }

    fn remove(&mut self, id: u16) {
        self.lru.retain(|&x| x != id);
        self.titles.remove(&id);
    }

    fn sync(&mut self, ptys: &[u16]) {
        let active: std::collections::HashSet<u16> = ptys.iter().copied().collect();
        self.lru.retain(|id| active.contains(id));
        for &id in ptys {
            if !self.lru.contains(&id) {
                self.lru.push(id);
            }
        }
    }

    fn visible_ids(&self) -> &[u16] {
        &self.lru
    }

    fn clamp_selection(&mut self) {
        let len = self.lru.len();
        if len == 0 {
            self.selected = 0;
        } else if self.selected >= len {
            self.selected = len - 1;
        }
    }

    fn render(&self, rows: u16, cols: u16, focused: Option<u16>, out: &mut Vec<u8>) {
        // Clear screen with dark background
        out.extend_from_slice(b"\x1b[0m\x1b[2J\x1b[H");
        let ids = self.visible_ids();
        if ids.is_empty() {
            let msg = b"  No PTYs.  Press Ctrl-N to create one.";
            let r = rows / 2;
            out.extend_from_slice(b"\x1b[");
            push_u16(out, r + 1);
            out.extend_from_slice(b";1H\x1b[90m");
            out.extend_from_slice(msg);
            out.extend_from_slice(b"\x1b[0m");
            return;
        }

        // Header
        out.extend_from_slice(b"\x1b[1;1H\x1b[1;97m  PTY Switcher\x1b[0m\x1b[90m  (");
        push_u16(out, ids.len() as u16);
        out.extend_from_slice(b" PTYs)  \xe2\x86\x91\xe2\x86\x93 navigate  Enter switch  ^N new  ^W kill  Esc close\x1b[0m");

        let start_row = 3u16;
        let max_visible = (rows.saturating_sub(start_row + 1)) as usize;

        // Scroll the list if selection is beyond visible range
        let scroll_offset = if self.selected >= max_visible {
            self.selected - max_visible + 1
        } else {
            0
        };

        for (vi, &id) in ids.iter().skip(scroll_offset).take(max_visible).enumerate() {
            let row = start_row + vi as u16;
            let is_selected = scroll_offset + vi == self.selected;
            let is_focused = Some(id) == focused;

            out.extend_from_slice(b"\x1b[");
            push_u16(out, row + 1);
            out.extend_from_slice(b";1H");

            if is_selected {
                out.extend_from_slice(b"\x1b[7m"); // inverse
            }

            // Indicator
            out.extend_from_slice(if is_focused { b"  \xe2\x96\xb6 " } else { b"    " });

            // PTY id
            out.extend_from_slice(b"#");
            push_u16(out, id);

            // Title
            if let Some(title) = self.titles.get(&id) {
                if !title.is_empty() {
                    out.extend_from_slice(b": ");
                    let max_title = (cols as usize).saturating_sub(12);
                    let display = if title.len() > max_title {
                        let mut end = max_title;
                        while end > 0 && !title.is_char_boundary(end) {
                            end -= 1;
                        }
                        &title[..end]
                    } else {
                        title.as_str()
                    };
                    out.extend_from_slice(display.as_bytes());
                }
            }

            // Pad to full width for inverse highlight
            if is_selected {
                let written = 4 + 1 + digits(id) + self.titles.get(&id).map(|t| t.len() + 2).unwrap_or(0);
                let pad = (cols as usize).saturating_sub(written);
                for _ in 0..pad {
                    out.push(b' ');
                }
                out.extend_from_slice(b"\x1b[0m");
            }
        }
    }
}

fn digits(mut n: u16) -> usize {
    if n == 0 { return 1; }
    let mut d = 0;
    while n > 0 { d += 1; n /= 10; }
    d
}

/// Parse a single keypress from raw stdin bytes.  Returns the action and
/// how many bytes were consumed.
enum ExposeAction {
    None,
    Close,
    Up,
    Down,
    Select,
    Create,
    Kill,
    Quit,
}

fn parse_expose_key(buf: &[u8]) -> (ExposeAction, usize) {
    if buf.is_empty() {
        return (ExposeAction::None, 0);
    }
    match buf[0] {
        0x0B => (ExposeAction::Close, 1),             // Ctrl-K
        0x1B if buf.len() == 1 => (ExposeAction::Close, 1), // bare Escape
        0x1B if buf.len() >= 3 && buf[1] == b'[' => {
            match buf[2] {
                b'A' => (ExposeAction::Up, 3),
                b'B' => (ExposeAction::Down, 3),
                _ => (ExposeAction::None, 3),
            }
        }
        0x0D | 0x0A => (ExposeAction::Select, 1),     // Enter
        0x0E => (ExposeAction::Create, 1),             // Ctrl-N
        0x17 => (ExposeAction::Kill, 1),               // Ctrl-W
        b'q' | b'Q' => (ExposeAction::Quit, 1),
        _ => (ExposeAction::None, 1),
    }
}

async fn run(transport: Transport) {
    let (mut server_reader, server_writer) = transport.split();

    let _raw = RawMode::enter();
    let _cleanup = Cleanup;

    let (rows, cols) = term_size();

    // Channel: main loop → writer task (fully-formed frames)
    let (frame_tx, mut frame_rx) = mpsc::channel::<Vec<u8>>(128);

    // Channel: background tasks → main loop (stdin bytes and resize events)
    let (ev_tx, mut ev_rx) = mpsc::channel::<Event>(64);

    // Writer task: drains frame_rx and sends to server
    let mut writer = server_writer;
    tokio::spawn(async move {
        while let Some(frame) = frame_rx.recv().await {
            if writer.write_all(&frame).await.is_err() {
                break;
            }
        }
    });

    // Stdin task
    let ev_tx_stdin = ev_tx.clone();
    tokio::spawn(async move {
        let mut stdin = tokio::io::stdin();
        let mut buf = [0u8; 4096];
        loop {
            match stdin.read(&mut buf).await {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    let _ = ev_tx_stdin.send(Event::Stdin(buf[..n].to_vec())).await;
                }
            }
        }
    });

    // SIGWINCH task
    let ev_tx_sig = ev_tx;
    tokio::spawn(async move {
        use tokio::signal::unix::{signal, SignalKind};
        let mut sig = signal(SignalKind::window_change()).expect("SIGWINCH");
        loop {
            sig.recv().await;
            let (r, c) = term_size();
            let _ = ev_tx_sig.send(Event::Resize(r, c)).await;
        }
    });

    // Advertise the client's target display rate. The actual send pace is still
    // ACK-limited, but a higher default avoids baking in a 60 fps assumption.
    let fps: u16 = std::env::var("BLIT_DISPLAY_FPS")
        .ok()
        .and_then(|s| s.parse::<u16>().ok())
        .filter(|fps| (10..=1000).contains(fps))
        .unwrap_or(240);
    let _ = frame_tx
        .send(make_frame(&[C2S_DISPLAY_RATE, fps as u8, (fps >> 8) as u8]))
        .await;

    let mut screen = TerminalState::new(rows, cols);
    let mut renderer = Renderer::new();
    let mut out_buf: Vec<u8> = Vec::with_capacity(256 * 1024);
    let mut focused_pty: Option<u16> = None;
    let mut current_title = String::new();
    let mut ptys: Vec<u16> = Vec::new();
    let mut expose = Expose::new();
    let mut cur_rows = rows;
    let mut cur_cols = cols;
    let mut stdout = tokio::io::stdout();

    loop {
        tokio::select! {
            // Server → us
            frame = read_frame(&mut server_reader) => {
                let frame = match frame { Some(f) => f, None => break };
                if frame.is_empty() { continue; }
                handle_server_msg(
                    &frame, &mut screen, &mut renderer, &mut out_buf,
                    &mut focused_pty, &mut ptys, &mut current_title, &frame_tx,
                    &mut stdout, cur_rows, cur_cols, &mut expose,
                ).await;
            }
            // Background tasks → us
            ev = ev_rx.recv() => {
                match ev {
                    Some(Event::Stdin(data)) => {
                        if handle_stdin(
                            &data, &mut expose, &mut focused_pty, &ptys,
                            &mut renderer, &mut screen, &mut out_buf,
                            &frame_tx, &mut stdout, cur_rows, cur_cols,
                        ).await { break; }
                    }
                    Some(Event::Resize(r, c)) => {
                        cur_rows = r;
                        cur_cols = c;
                        if expose.open {
                            out_buf.clear();
                            expose.render(r, c, focused_pty, &mut out_buf);
                            if !out_buf.is_empty() {
                                let _ = stdout.write_all(&out_buf).await;
                                let _ = stdout.flush().await;
                            }
                        } else if let Some(id) = focused_pty {
                            let _ = frame_tx.send(make_frame(&msg_resize(id, r, c))).await;
                            out_buf.clear();
                            renderer.render(&screen, &mut out_buf);
                            if !out_buf.is_empty() {
                                let _ = stdout.write_all(&out_buf).await;
                                let _ = stdout.flush().await;
                            }
                        }
                    }
                    None => break,
                }
            }
        }
    }
}

/// Returns true if the CLI should quit.
async fn handle_stdin(
    data: &[u8],
    expose: &mut Expose,
    focused_pty: &mut Option<u16>,
    ptys: &[u16],
    renderer: &mut Renderer,
    screen: &mut TerminalState,
    out_buf: &mut Vec<u8>,
    frame_tx: &mpsc::Sender<Vec<u8>>,
    stdout: &mut tokio::io::Stdout,
    rows: u16,
    cols: u16,
) -> bool {
    // Check for Ctrl-K (0x0B) to toggle expose
    if !expose.open {
        // Scan for Ctrl-K in the input
        if let Some(pos) = data.iter().position(|&b| b == 0x0B) {
            // Send bytes before Ctrl-K to the PTY
            if pos > 0 {
                if let Some(id) = *focused_pty {
                    let _ = frame_tx.send(make_frame(&msg_input(id, &data[..pos]))).await;
                }
            }
            // Open expose
            expose.sync(ptys);
            expose.open = true;
            // Pre-select the focused PTY
            if let Some(id) = *focused_pty {
                expose.selected = expose.lru.iter().position(|&x| x == id).unwrap_or(0);
            }
            out_buf.clear();
            out_buf.extend_from_slice(b"\x1b[?25l"); // hide cursor
            expose.render(rows, cols, *focused_pty, out_buf);
            let _ = stdout.write_all(out_buf).await;
                    let _ = stdout.flush().await;
            // Process remaining bytes after Ctrl-K as expose input
            if pos + 1 < data.len() {
                if handle_expose_input(
                    &data[pos + 1..], expose, focused_pty, renderer, screen,
                    out_buf, frame_tx, stdout, rows, cols,
                ).await { return true; }
            }
            return false;
        }
        // No Ctrl-K — forward all to PTY
        if let Some(id) = *focused_pty {
            let _ = frame_tx.send(make_frame(&msg_input(id, data))).await;
        }
    } else {
        if handle_expose_input(
            data, expose, focused_pty, renderer, screen,
            out_buf, frame_tx, stdout, rows, cols,
        ).await { return true; }
    }
    false
}

/// Returns true if the CLI should quit.
async fn handle_expose_input(
    data: &[u8],
    expose: &mut Expose,
    focused_pty: &mut Option<u16>,
    renderer: &mut Renderer,
    screen: &mut TerminalState,
    out_buf: &mut Vec<u8>,
    frame_tx: &mpsc::Sender<Vec<u8>>,
    stdout: &mut tokio::io::Stdout,
    rows: u16,
    cols: u16,
) -> bool {
    let mut off = 0;
    while off < data.len() {
        let (action, consumed) = parse_expose_key(&data[off..]);
        if consumed == 0 { break; }
        off += consumed;

        match action {
            ExposeAction::None => {}
            ExposeAction::Close => {
                close_expose(expose, focused_pty, renderer, screen, out_buf, frame_tx, stdout, rows, cols).await;
                // Remaining bytes after close go to PTY
                if off < data.len() {
                    if let Some(id) = *focused_pty {
                        let _ = frame_tx.send(make_frame(&msg_input(id, &data[off..]))).await;
                    }
                }
                return false;
            }
            ExposeAction::Up => {
                if expose.selected > 0 {
                    expose.selected -= 1;
                }
            }
            ExposeAction::Down => {
                let len = expose.visible_ids().len();
                if len > 0 && expose.selected < len - 1 {
                    expose.selected += 1;
                }
            }
            ExposeAction::Select => {
                let ids = expose.visible_ids();
                if let Some(&id) = ids.get(expose.selected) {
                    *focused_pty = Some(id);
                    expose.touch(id);
                    let _ = frame_tx.send(make_frame(&msg_focus(id))).await;
                    let _ = frame_tx.send(make_frame(&msg_resize(id, rows, cols))).await;
                }
                close_expose(expose, focused_pty, renderer, screen, out_buf, frame_tx, stdout, rows, cols).await;
                return false;
            }
            ExposeAction::Create => {
                let _ = frame_tx.send(make_frame(&msg_create(rows, cols))).await;
                // Stay in expose — the new PTY will appear when S2C_CREATED arrives
            }
            ExposeAction::Kill => {
                let ids = expose.visible_ids().to_vec();
                if let Some(&id) = ids.get(expose.selected) {
                    let _ = frame_tx
                        .send(make_frame(&msg_close(id)))
                        .await;
                }
            }
            ExposeAction::Quit => {
                return true;
            }
        }
    }
    // Re-render expose after processing all keys
    if expose.open {
        expose.clamp_selection();
        out_buf.clear();
        expose.render(rows, cols, *focused_pty, out_buf);
        if !out_buf.is_empty() {
            let _ = stdout.write_all(out_buf).await;
                    let _ = stdout.flush().await;
        }
    }
    false
}

async fn close_expose(
    expose: &mut Expose,
    focused_pty: &mut Option<u16>,
    renderer: &mut Renderer,
    screen: &mut TerminalState,
    out_buf: &mut Vec<u8>,
    frame_tx: &mpsc::Sender<Vec<u8>>,
    stdout: &mut tokio::io::Stdout,
    rows: u16,
    cols: u16,
) {
    expose.open = false;
    out_buf.clear();
    renderer.render(screen, out_buf);
    if !out_buf.is_empty() {
        let _ = stdout.write_all(out_buf).await;
                    let _ = stdout.flush().await;
    }
    // Re-send resize in case the terminal size changed while expose was open
    if let Some(id) = *focused_pty {
        let _ = frame_tx.send(make_frame(&msg_resize(id, rows, cols))).await;
    }
}

async fn handle_server_msg(
    frame: &[u8],
    screen: &mut TerminalState,
    renderer: &mut Renderer,
    out_buf: &mut Vec<u8>,
    focused_pty: &mut Option<u16>,
    ptys: &mut Vec<u16>,
    current_title: &mut String,
    frame_tx: &mpsc::Sender<Vec<u8>>,
    stdout: &mut tokio::io::Stdout,
    rows: u16,
    cols: u16,
    expose: &mut Expose,
) {
    match frame[0] {
        S2C_LIST if frame.len() >= 3 => {
            let count = u16::from_le_bytes([frame[1], frame[2]]) as usize;
            ptys.clear();
            let mut off = 3;
            for _ in 0..count {
                if off + 4 > frame.len() { break; }
                let id = u16::from_le_bytes([frame[off], frame[off + 1]]);
                let tag_len = u16::from_le_bytes([frame[off + 2], frame[off + 3]]) as usize;
                off += 4 + tag_len;
                ptys.push(id);
            }
            expose.sync(ptys);
            if let Some(&id) = ptys.first() {
                *focused_pty = Some(id);
                expose.touch(id);
                let _ = frame_tx.send(make_frame(&msg_focus(id))).await;
                let _ = frame_tx.send(make_frame(&msg_resize(id, rows, cols))).await;
            } else {
                let _ = frame_tx.send(make_frame(&msg_create(rows, cols))).await;
            }
        }

        S2C_CREATED if frame.len() >= 3 => {
            let id = u16::from_le_bytes([frame[1], frame[2]]);
            if !ptys.contains(&id) {
                ptys.push(id);
            }
            expose.sync(ptys);
            if focused_pty.is_none() || expose.open {
                *focused_pty = Some(id);
                expose.touch(id);
                let _ = frame_tx.send(make_frame(&msg_focus(id))).await;
                let _ = frame_tx.send(make_frame(&msg_resize(id, rows, cols))).await;
                if expose.open {
                    expose.open = false;
                    out_buf.clear();
                    renderer.render(screen, out_buf);
                    if !out_buf.is_empty() {
                        let _ = stdout.write_all(out_buf).await;
                    let _ = stdout.flush().await;
                    }
                }
            }
        }

        S2C_CREATED_N if frame.len() >= 5 => {
            let id = u16::from_le_bytes([frame[3], frame[4]]);
            if !ptys.contains(&id) {
                ptys.push(id);
            }
            expose.sync(ptys);
            if focused_pty.is_none() || expose.open {
                *focused_pty = Some(id);
                expose.touch(id);
                let _ = frame_tx.send(make_frame(&msg_focus(id))).await;
                let _ = frame_tx.send(make_frame(&msg_resize(id, rows, cols))).await;
                if expose.open {
                    expose.open = false;
                    out_buf.clear();
                    renderer.render(screen, out_buf);
                    if !out_buf.is_empty() {
                        let _ = stdout.write_all(out_buf).await;
                    let _ = stdout.flush().await;
                    }
                }
            }
        }

        S2C_HELLO => {}

        S2C_CLOSED if frame.len() >= 3 => {
            let id = u16::from_le_bytes([frame[1], frame[2]]);
            ptys.retain(|&x| x != id);
            expose.remove(id);
            expose.sync(ptys);
            if expose.open {
                expose.clamp_selection();
                out_buf.clear();
                expose.render(rows, cols, *focused_pty, &mut *out_buf);
                if !out_buf.is_empty() {
                    let _ = stdout.write_all(out_buf).await;
                    let _ = stdout.flush().await;
                }
            }
            if *focused_pty == Some(id) {
                *focused_pty = expose.lru.first().copied().or_else(|| ptys.first().copied());
                if let Some(new_id) = *focused_pty {
                    let _ = frame_tx.send(make_frame(&msg_focus(new_id))).await;
                    let _ = frame_tx
                        .send(make_frame(&msg_resize(new_id, rows, cols)))
                        .await;
                }
            }
        }

        S2C_TITLE if frame.len() >= 3 => {
            let id = u16::from_le_bytes([frame[1], frame[2]]);
            if let Ok(title) = std::str::from_utf8(&frame[3..]) {
                expose.titles.insert(id, title.to_owned());
                if expose.open {
                    out_buf.clear();
                    expose.render(rows, cols, *focused_pty, &mut *out_buf);
                    if !out_buf.is_empty() {
                        let _ = stdout.write_all(out_buf).await;
                    let _ = stdout.flush().await;
                    }
                }
            }
            if *focused_pty == Some(id) {
                if let Ok(title) = std::str::from_utf8(&frame[3..]) {
                    current_title.clear();
                    current_title.push_str(title);
                    if !expose.open {
                        out_buf.clear();
                        out_buf.extend_from_slice(b"\x1b]0;");
                        out_buf.extend_from_slice(title.as_bytes());
                        out_buf.push(b'\x07');
                        write_all_stdout(out_buf);
                    }
                }
            }
        }

        S2C_UPDATE if frame.len() >= 3 => {
            let id = u16::from_le_bytes([frame[1], frame[2]]);
            if *focused_pty != Some(id) {
                // Still ACK even if not the focused PTY
                let _ = frame_tx.send(make_frame(&msg_ack())).await;
                return;
            }

            screen.feed_compressed(&frame[3..]);

            if !expose.open {
                out_buf.clear();

                if screen.title() != current_title {
                    current_title.clear();
                    current_title.push_str(screen.title());
                    out_buf.extend_from_slice(b"\x1b]0;");
                    out_buf.extend_from_slice(screen.title().as_bytes());
                    out_buf.push(b'\x07');
                }

                renderer.render(screen, out_buf);

                if !out_buf.is_empty() {
                    let _ = stdout.write_all(out_buf).await;
                    let _ = stdout.flush().await;
                }
            }

            let _ = frame_tx.send(make_frame(&msg_ack())).await;
        }

        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn output_contains(out: &[u8], needle: &[u8]) -> bool {
        out.windows(needle.len()).any(|w| w == needle)
    }

    #[test]
    fn first_render_shows_cursor_when_visible() {
        let mut renderer = Renderer::new();
        let mut screen = TerminalState::new(4, 10);
        screen.frame_mut().set_mode(1); // cursor visible

        let mut out = Vec::new();
        renderer.render(&screen, &mut out);

        assert!(output_contains(&out, b"\x1b[?25h"),
            "cursor show sequence missing from first render output");
    }

    #[test]
    fn cursor_stays_visible_across_renders() {
        let mut renderer = Renderer::new();
        let mut screen = TerminalState::new(4, 10);
        screen.frame_mut().set_mode(1); // cursor visible

        let mut out = Vec::new();
        renderer.render(&screen, &mut out);

        out.clear();
        renderer.render(&screen, &mut out);

        assert!(!output_contains(&out, b"\x1b[?25l"),
            "cursor hide emitted when cursor should stay visible");
    }

    #[test]
    fn cursor_hidden_then_shown() {
        let mut renderer = Renderer::new();
        let mut screen = TerminalState::new(4, 10);
        screen.frame_mut().set_mode(0); // cursor hidden

        let mut out = Vec::new();
        renderer.render(&screen, &mut out);

        screen.frame_mut().set_mode(1);
        out.clear();
        renderer.render(&screen, &mut out);

        assert!(output_contains(&out, b"\x1b[?25h"),
            "cursor show missing after mode change");
    }

    #[test]
    fn cursor_positioned_at_screen_cursor() {
        let mut renderer = Renderer::new();
        let mut screen = TerminalState::new(4, 10);
        screen.frame_mut().set_mode(1);
        screen.frame_mut().set_cursor(2, 5);

        let mut out = Vec::new();
        renderer.render(&screen, &mut out);

        // CUP for row 2, col 5 → \x1b[3;6H (1-based)
        assert!(output_contains(&out, b"\x1b[3;6H"),
            "cursor not positioned at expected row=2 col=5");
    }

    // ── make_frame ──────────────────────────────────────────────────────

    #[test]
    fn make_frame_empty_payload() {
        let f = make_frame(&[]);
        assert_eq!(f, vec![0, 0, 0, 0]);
    }

    #[test]
    fn make_frame_small_payload() {
        let f = make_frame(&[0xAB, 0xCD]);
        assert_eq!(f.len(), 6);
        assert_eq!(&f[..4], &2u32.to_le_bytes());
        assert_eq!(&f[4..], &[0xAB, 0xCD]);
    }

    #[test]
    fn make_frame_256_byte_payload() {
        let payload = vec![0x42; 256];
        let f = make_frame(&payload);
        assert_eq!(f.len(), 260);
        assert_eq!(u32::from_le_bytes([f[0], f[1], f[2], f[3]]), 256);
        assert!(f[4..].iter().all(|&b| b == 0x42));
    }

    // ── pack_color / emit_color ─────────────────────────────────────────

    #[test]
    fn pack_color_default() {
        let p = pack_color(0, 0, 0, 0);
        assert_eq!(p, 0);
    }

    #[test]
    fn pack_color_indexed() {
        let p = pack_color(1, 42, 0, 0);
        // color_type=1 at bits 24, r=42 at bits 16
        assert_eq!(p, (1u64 << 24) | (42u64 << 16));
    }

    #[test]
    fn pack_color_rgb() {
        let p = pack_color(2, 0xFF, 0x80, 0x40);
        assert_eq!(p, (2u64 << 24) | (0xFFu64 << 16) | (0x80u64 << 8) | 0x40u64);
    }

    #[test]
    fn emit_color_default_fg() {
        let packed = pack_color(0, 0, 0, 0);
        let mut out = Vec::new();
        emit_color(packed, true, &mut out);
        assert_eq!(out, b"\x1b[39m");
    }

    #[test]
    fn emit_color_default_bg() {
        let packed = pack_color(0, 0, 0, 0);
        let mut out = Vec::new();
        emit_color(packed, false, &mut out);
        assert_eq!(out, b"\x1b[49m");
    }

    #[test]
    fn emit_color_indexed_fg() {
        let packed = pack_color(1, 196, 0, 0);
        let mut out = Vec::new();
        emit_color(packed, true, &mut out);
        // \x1b[38;5;196m
        let expected = b"\x1b[38;5;196m";
        assert_eq!(out, expected);
    }

    #[test]
    fn emit_color_indexed_bg() {
        let packed = pack_color(1, 42, 0, 0);
        let mut out = Vec::new();
        emit_color(packed, false, &mut out);
        assert_eq!(out, b"\x1b[48;5;42m");
    }

    #[test]
    fn emit_color_rgb_fg() {
        let packed = pack_color(2, 10, 20, 30);
        let mut out = Vec::new();
        emit_color(packed, true, &mut out);
        assert_eq!(out, b"\x1b[38;2;10;20;30m");
    }

    #[test]
    fn emit_color_rgb_bg() {
        let packed = pack_color(2, 255, 128, 0);
        let mut out = Vec::new();
        emit_color(packed, false, &mut out);
        assert_eq!(out, b"\x1b[48;2;255;128;0m");
    }

    #[test]
    fn emit_color_unknown_type_falls_back_to_default() {
        let packed = pack_color(3, 1, 2, 3);
        let mut out = Vec::new();
        emit_color(packed, true, &mut out);
        assert_eq!(out, b"\x1b[39m");
    }

    // ── push_u16 ────────────────────────────────────────────────────────

    #[test]
    fn push_u16_zero() {
        let mut out = Vec::new();
        push_u16(&mut out, 0);
        assert_eq!(out, b"0");
    }

    #[test]
    fn push_u16_one() {
        let mut out = Vec::new();
        push_u16(&mut out, 1);
        assert_eq!(out, b"1");
    }

    #[test]
    fn push_u16_255() {
        let mut out = Vec::new();
        push_u16(&mut out, 255);
        assert_eq!(out, b"255");
    }

    #[test]
    fn push_u16_max() {
        let mut out = Vec::new();
        push_u16(&mut out, 65535);
        assert_eq!(out, b"65535");
    }

    #[test]
    fn push_u16_power_of_ten() {
        let mut out = Vec::new();
        push_u16(&mut out, 1000);
        assert_eq!(out, b"1000");
    }

    // ── digits ──────────────────────────────────────────────────────────

    #[test]
    fn digits_zero() {
        assert_eq!(digits(0), 1);
    }

    #[test]
    fn digits_single() {
        assert_eq!(digits(1), 1);
        assert_eq!(digits(9), 1);
    }

    #[test]
    fn digits_double() {
        assert_eq!(digits(10), 2);
        assert_eq!(digits(99), 2);
    }

    #[test]
    fn digits_triple() {
        assert_eq!(digits(100), 3);
        assert_eq!(digits(999), 3);
    }

    #[test]
    fn digits_large() {
        assert_eq!(digits(10000), 5);
        assert_eq!(digits(65535), 5);
    }

    // ── parse_expose_key ────────────────────────────────────────────────

    #[test]
    fn parse_expose_key_empty() {
        let (action, consumed) = parse_expose_key(&[]);
        assert!(matches!(action, ExposeAction::None));
        assert_eq!(consumed, 0);
    }

    #[test]
    fn parse_expose_key_ctrl_k() {
        let (action, consumed) = parse_expose_key(&[0x0B]);
        assert!(matches!(action, ExposeAction::Close));
        assert_eq!(consumed, 1);
    }

    #[test]
    fn parse_expose_key_bare_escape() {
        let (action, consumed) = parse_expose_key(&[0x1B]);
        assert!(matches!(action, ExposeAction::Close));
        assert_eq!(consumed, 1);
    }

    #[test]
    fn parse_expose_key_arrow_up() {
        let (action, consumed) = parse_expose_key(&[0x1B, b'[', b'A']);
        assert!(matches!(action, ExposeAction::Up));
        assert_eq!(consumed, 3);
    }

    #[test]
    fn parse_expose_key_arrow_down() {
        let (action, consumed) = parse_expose_key(&[0x1B, b'[', b'B']);
        assert!(matches!(action, ExposeAction::Down));
        assert_eq!(consumed, 3);
    }

    #[test]
    fn parse_expose_key_unknown_csi() {
        // e.g. arrow right \x1b[C — not handled, returns None
        let (action, consumed) = parse_expose_key(&[0x1B, b'[', b'C']);
        assert!(matches!(action, ExposeAction::None));
        assert_eq!(consumed, 3);
    }

    #[test]
    fn parse_expose_key_enter_cr() {
        let (action, consumed) = parse_expose_key(&[0x0D]);
        assert!(matches!(action, ExposeAction::Select));
        assert_eq!(consumed, 1);
    }

    #[test]
    fn parse_expose_key_enter_lf() {
        let (action, consumed) = parse_expose_key(&[0x0A]);
        assert!(matches!(action, ExposeAction::Select));
        assert_eq!(consumed, 1);
    }

    #[test]
    fn parse_expose_key_ctrl_n() {
        let (action, consumed) = parse_expose_key(&[0x0E]);
        assert!(matches!(action, ExposeAction::Create));
        assert_eq!(consumed, 1);
    }

    #[test]
    fn parse_expose_key_ctrl_w() {
        let (action, consumed) = parse_expose_key(&[0x17]);
        assert!(matches!(action, ExposeAction::Kill));
        assert_eq!(consumed, 1);
    }

    #[test]
    fn parse_expose_key_tab_is_none() {
        // Tab (0x09) is not a recognized expose key
        let (action, consumed) = parse_expose_key(&[0x09]);
        assert!(matches!(action, ExposeAction::None));
        assert_eq!(consumed, 1);
    }

    #[test]
    fn parse_expose_key_backspace_is_none() {
        // Backspace (0x7F) is not a recognized expose key
        let (action, consumed) = parse_expose_key(&[0x7F]);
        assert!(matches!(action, ExposeAction::None));
        assert_eq!(consumed, 1);
    }

    #[test]
    fn parse_expose_key_plain_char_is_none() {
        let (action, consumed) = parse_expose_key(b"a");
        assert!(matches!(action, ExposeAction::None));
        assert_eq!(consumed, 1);
    }

    // ── Expose struct ───────────────────────────────────────────────────

    #[test]
    fn expose_new_defaults() {
        let e = Expose::new();
        assert!(!e.open);
        assert_eq!(e.selected, 0);
        assert!(e.lru.is_empty());
        assert!(e.titles.is_empty());
    }

    #[test]
    fn expose_touch_adds_to_front() {
        let mut e = Expose::new();
        e.touch(1);
        e.touch(2);
        assert_eq!(e.lru, vec![2, 1]);
    }

    #[test]
    fn expose_touch_moves_existing_to_front() {
        let mut e = Expose::new();
        e.touch(1);
        e.touch(2);
        e.touch(3);
        e.touch(1); // move 1 to front
        assert_eq!(e.lru, vec![1, 3, 2]);
    }

    #[test]
    fn expose_remove_removes_from_lru_and_titles() {
        let mut e = Expose::new();
        e.touch(1);
        e.touch(2);
        e.titles.insert(1, "hello".into());
        e.titles.insert(2, "world".into());
        e.remove(1);
        assert_eq!(e.lru, vec![2]);
        assert!(!e.titles.contains_key(&1));
        assert!(e.titles.contains_key(&2));
    }

    #[test]
    fn expose_remove_nonexistent_is_noop() {
        let mut e = Expose::new();
        e.touch(1);
        e.remove(99);
        assert_eq!(e.lru, vec![1]);
    }

    #[test]
    fn expose_sync_adds_missing_and_removes_stale() {
        let mut e = Expose::new();
        e.touch(1);
        e.touch(2);
        e.touch(3);
        // Sync with [2, 4]: removes 1 and 3, adds 4 at end
        e.sync(&[2, 4]);
        assert_eq!(e.lru.len(), 2);
        assert!(e.lru.contains(&2));
        assert!(e.lru.contains(&4));
        assert!(!e.lru.contains(&1));
        assert!(!e.lru.contains(&3));
    }

    #[test]
    fn expose_sync_preserves_lru_order_for_existing() {
        let mut e = Expose::new();
        e.touch(1);
        e.touch(2);
        e.touch(3);
        // lru is [3, 2, 1]
        e.sync(&[1, 2, 3]);
        // Existing items keep their LRU order: [3, 2, 1]
        assert_eq!(e.lru, vec![3, 2, 1]);
    }

    #[test]
    fn expose_sync_empty_clears_lru() {
        let mut e = Expose::new();
        e.touch(1);
        e.sync(&[]);
        assert!(e.lru.is_empty());
    }

    #[test]
    fn expose_visible_ids_returns_lru() {
        let mut e = Expose::new();
        e.touch(5);
        e.touch(10);
        assert_eq!(e.visible_ids(), &[10, 5]);
    }

    #[test]
    fn expose_clamp_selection_empty() {
        let mut e = Expose::new();
        e.selected = 5;
        e.clamp_selection();
        assert_eq!(e.selected, 0);
    }

    #[test]
    fn expose_clamp_selection_within_bounds() {
        let mut e = Expose::new();
        e.touch(1);
        e.touch(2);
        e.touch(3);
        e.selected = 1;
        e.clamp_selection();
        assert_eq!(e.selected, 1);
    }

    #[test]
    fn expose_clamp_selection_over_bounds() {
        let mut e = Expose::new();
        e.touch(1);
        e.touch(2);
        e.selected = 10;
        e.clamp_selection();
        assert_eq!(e.selected, 1); // len - 1
    }
}
