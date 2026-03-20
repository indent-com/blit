use blit_remote::{
    msg_ack, msg_close, msg_create, msg_focus, msg_input, msg_resize, TerminalState,
    C2S_DISPLAY_RATE, CELL_SIZE, S2C_CLOSED, S2C_CREATED, S2C_LIST, S2C_TITLE, S2C_UPDATE,
};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::sync::mpsc;

// Browser mode imports
use axum::extract::ws::{Message, WebSocket};
use axum::extract::{FromRequest, WebSocketUpgrade};
use axum::http::header::CONTENT_TYPE;
use axum::response::{Html, IntoResponse, Response};
use axum::routing::get;
use futures_util::{SinkExt, StreamExt};
use std::sync::Arc;

// Embedded web assets
const WEB_INDEX_HTML: &str = include_str!("../../web/index.html");
const WEB_JS: &[u8] = include_bytes!("../../web/blit_browser.js");
const WEB_WASM: &[u8] = include_bytes!("../../web/blit_browser_bg.wasm");
const WEB_DTS: &[u8] = include_bytes!("../../web/blit_browser.d.ts");
const WEB_WASM_DTS: &[u8] = include_bytes!("../../web/blit_browser_bg.wasm.d.ts");
// The wasm-bindgen inline_js snippet — content is stable, hash in the path changes per build.
// Serve for any /snippets/blit-browser-*/inline0.js request.
const WEB_SNIPPET_INLINE0: &[u8] = br#"const glyphTextCache = new Map();

export function blitFillTextCodePoint(ctx, codePoint, x, y) {
  let text = glyphTextCache.get(codePoint);
  if (text === undefined) {
    text = String.fromCodePoint(codePoint);
    glyphTextCache.set(codePoint, text);
  }
  ctx.fillText(text, x, y);
}

export function blitFillText(ctx, text, x, y) {
  ctx.fillText(text, x, y);
}
"#;

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
    fn enter() -> Self {
        unsafe {
            let mut saved: libc::termios = std::mem::zeroed();
            libc::tcgetattr(libc::STDIN_FILENO, &mut saved);
            let mut raw = saved;
            libc::cfmakeraw(&mut raw);
            raw.c_cc[libc::VMIN] = 1;
            raw.c_cc[libc::VTIME] = 0;
            libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, &raw);
            Self { saved }
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
        // Reset attributes, show cursor, disable mouse, leave alternate screen
        const RESET: &[u8] = b"\x1b[?1003l\x1b[?1002l\x1b[?1000l\x1b[?9l\
                               \x1b[?1016l\x1b[?1006l\x1b[?1005l\
                               \x1b[?2004l\x1b>\x1b[?1l\x1b[?25h\x1b[0m\x1b[?1049l\r\n";
        unsafe {
            libc::write(libc::STDOUT_FILENO, RESET.as_ptr().cast(), RESET.len());
        }
    }
}

// ── Framing ───────────────────────────────────────────────────────────────────
async fn read_frame(r: &mut (impl AsyncRead + Unpin)) -> Option<Vec<u8>> {
    let mut hdr = [0u8; 4];
    r.read_exact(&mut hdr).await.ok()?;
    let len = u32::from_le_bytes(hdr) as usize;
    if len == 0 {
        return Some(vec![]);
    }
    let mut buf = vec![0u8; len];
    r.read_exact(&mut buf).await.ok()?;
    Some(buf)
}

fn make_frame(payload: &[u8]) -> Vec<u8> {
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
        let all_dirty = screen.all_dirty();
        let dirty = screen.dirty_flags();

        let cells = screen.cells();
        for i in 0..total {
            if !all_dirty && !dirty[i] {
                continue;
            }
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
                std::mem::forget(child);
                (Box::new(stdout), Box::new(stdin))
            }
        }
    }
}

// ── Main ──────────────────────────────────────────────────────────────────────
#[tokio::main]
async fn main() {
    let args: Vec<String> = std::env::args().collect();
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

    let flag = args.get(1).map(|s| s.as_str());

    let connector = if flag == Some("--tcp") {
        let addr = args.get(2).unwrap_or_else(|| {
            eprintln!("blit: --tcp requires HOST:PORT");
            std::process::exit(1);
        });
        BrowserConnector::Tcp(addr.clone())
    } else if flag == Some("--socket") || flag == Some("-s") {
        let path = args.get(2).unwrap_or_else(|| {
            eprintln!("blit: --socket requires a path");
            std::process::exit(1);
        });
        BrowserConnector::Unix(path.clone())
    } else if flag == Some("--ssh") || flag == Some("ssh") {
        // Explicit --ssh: remaining args are SSH args
        setup_ssh_forward(&args[2..]).await
    } else if flag.is_some() && !flag.unwrap().starts_with('-') {
        // Bare hostname: treat as SSH target
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

    let injected_html = WEB_INDEX_HTML.replacen(
        "<script type=\"module\">",
        &format!(
            "<style>#auth{{display:none!important}}</style>\n<script>localStorage.setItem('blit.passphrase','{token}');</script>\n<script type=\"module\">"
        ),
        1,
    );
    let injected_html: &'static str = Box::leak(injected_html.into_boxed_str());

    let app = axum::Router::new()
        .fallback(get(move |state, request| browser_root_handler(state, request, injected_html)))
        .with_state(state);

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let url = format!("http://{addr}");
    eprintln!("blit: serving browser UI at {url}");

    open_browser(&url);

    tokio::spawn(async {
        let _ = tokio::signal::ctrl_c().await;
        std::process::exit(0);
    });

    axum::serve(listener, app).await.unwrap();
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

fn serve_embedded_asset(path: &str) -> Option<Response> {
    let trimmed = path.trim_start_matches('/');
    let (bytes, ct): (&[u8], &str) = match trimmed {
        "blit_browser.js" => (WEB_JS, "application/javascript"),
        "blit_browser_bg.wasm" => (WEB_WASM, "application/wasm"),
        "blit_browser.d.ts" => (WEB_DTS, "text/plain; charset=utf-8"),
        "blit_browser_bg.wasm.d.ts" => (WEB_WASM_DTS, "text/plain; charset=utf-8"),
        p if p.starts_with("snippets/blit-browser-") && p.ends_with("/inline0.js") => {
            (WEB_SNIPPET_INLINE0, "application/javascript")
        }
        _ => return None,
    };
    Some(([(CONTENT_TYPE, ct)], bytes.to_vec()).into_response())
}

async fn browser_root_handler(
    axum::extract::State(state): axum::extract::State<Arc<BrowserState>>,
    request: axum::extract::Request,
    index_html: &'static str,
) -> Response {
    let path = request.uri().path().to_owned();

    if let Some(resp) = serve_embedded_asset(&path) {
        return resp;
    }

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
    let transport_to_ws = tokio::spawn(async move {
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
    let ws_to_transport = tokio::spawn(async move {
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
        _ = transport_to_ws => {}
        _ = ws_to_transport => {}
    }

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
                        &title[..max_title]
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
                        handle_stdin(
                            &data, &mut expose, &mut focused_pty, &ptys,
                            &mut renderer, &mut screen, &mut out_buf,
                            &frame_tx, &mut stdout, cur_rows, cur_cols,
                        ).await;
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
                        }
                    }
                    None => break,
                }
            }
        }
    }
}

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
) {
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
                handle_expose_input(
                    &data[pos + 1..], expose, focused_pty, renderer, screen,
                    out_buf, frame_tx, stdout, rows, cols,
                ).await;
            }
            return;
        }
        // No Ctrl-K — forward all to PTY
        if let Some(id) = *focused_pty {
            let _ = frame_tx.send(make_frame(&msg_input(id, data))).await;
        }
    } else {
        handle_expose_input(
            data, expose, focused_pty, renderer, screen,
            out_buf, frame_tx, stdout, rows, cols,
        ).await;
    }
}

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
) {
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
                return;
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
                return;
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
    // Force full repaint of the terminal
    screen.mark_all_dirty();
    out_buf.clear();
    renderer.render(screen, out_buf);
    screen.clear_all_dirty();
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
            for i in 0..count {
                let off = 3 + i * 2;
                if off + 1 < frame.len() {
                    ptys.push(u16::from_le_bytes([frame[off], frame[off + 1]]));
                }
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
                    // Close expose and switch to new PTY
                    expose.open = false;
                    screen.mark_all_dirty();
                    out_buf.clear();
                    renderer.render(screen, out_buf);
                    screen.clear_all_dirty();
                    if !out_buf.is_empty() {
                        let _ = stdout.write_all(out_buf).await;
                    let _ = stdout.flush().await;
                    }
                }
            }
        }

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
                screen.clear_all_dirty();

                if !out_buf.is_empty() {
                    let _ = stdout.write_all(out_buf).await;
                    let _ = stdout.flush().await;
                }
            } else {
                screen.clear_all_dirty();
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
        screen.mark_all_dirty();

        let mut out = Vec::new();
        renderer.render(&screen, &mut out);

        // The first render enters alt screen and hides cursor, then must
        // re-show it because the screen mode says cursor is visible.
        assert!(output_contains(&out, b"\x1b[?25h"),
            "cursor show sequence missing from first render output");
    }

    #[test]
    fn cursor_stays_visible_across_renders() {
        let mut renderer = Renderer::new();
        let mut screen = TerminalState::new(4, 10);
        screen.frame_mut().set_mode(1); // cursor visible
        screen.mark_all_dirty();

        let mut out = Vec::new();
        renderer.render(&screen, &mut out);
        screen.clear_all_dirty();

        // Second render, same mode — should NOT emit hide.
        screen.mark_all_dirty();
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
        screen.mark_all_dirty();

        let mut out = Vec::new();
        renderer.render(&screen, &mut out);
        screen.clear_all_dirty();

        // Now cursor becomes visible.
        screen.frame_mut().set_mode(1);
        screen.mark_all_dirty();
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
        screen.mark_all_dirty();

        let mut out = Vec::new();
        renderer.render(&screen, &mut out);

        // CUP for row 2, col 5 → \x1b[3;6H (1-based)
        assert!(output_contains(&out, b"\x1b[3;6H"),
            "cursor not positioned at expected row=2 col=5");
    }
}
