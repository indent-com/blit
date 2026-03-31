#[cfg(not(target_os = "linux"))]
fn main() {
    eprintln!("blit-netdash only runs on Linux.");
    std::process::exit(1);
}

#[cfg(target_os = "linux")]
mod linux {
    use std::collections::{HashMap, VecDeque};
    use std::fs;
    use std::hash::{Hash, Hasher};
    use std::io::{self, Write};
    use std::net::IpAddr;
    use std::os::fd::AsRawFd;
    use std::sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    };
    use std::thread;
    use std::time::{Duration, Instant};

    use blit_remote::{CELL_SIZE, CallbackRenderer, CellStyle, Color, Dom, FrameState, Rect};
    use netlink_packet_core::{
        NLM_F_ACK, NLM_F_DUMP, NLM_F_REQUEST, NetlinkHeader, NetlinkMessage, NetlinkPayload,
    };
    use netlink_packet_sock_diag::{
        SockDiagMessage,
        constants::*,
        inet::{
            ExtensionFlags, InetRequest, InetResponse, InetResponseHeader, SocketId, StateFlags,
            nlas::Nla,
        },
    };
    use netlink_sys::{Socket, SocketAddr, protocols::NETLINK_SOCK_DIAG};

    const HISTORY_LEN: usize = 24;
    const MAX_RENDER_PEERS: usize = 128;
    const MAX_RENDER_CONNECTIONS: usize = 256;
    const MAX_EVENTS: usize = 256;
    // From linux/uapi/linux/sock_diag.h sknetlink_groups.
    const SOCK_DIAG_INET_TCP_DESTROY_GROUP: u32 = 1;
    const SOCK_DIAG_INET6_TCP_DESTROY_GROUP: u32 = 3;

    #[derive(Clone, Copy)]
    struct Args {
        fps: u64,
        poll_ms: u64,
    }

    fn parse_args() -> Args {
        let mut fps = 12u64;
        let mut poll_ms = 120u64;
        let mut it = std::env::args().skip(1);
        while let Some(arg) = it.next() {
            match arg.as_str() {
                "--fps" => {
                    if let Some(value) = it.next().and_then(|s| s.parse::<u64>().ok()) {
                        fps = value.clamp(1, 240);
                    }
                }
                "--poll-ms" => {
                    if let Some(value) = it.next().and_then(|s| s.parse::<u64>().ok()) {
                        poll_ms = value.clamp(1, 5_000);
                    }
                }
                "--help" | "-h" => {
                    println!(
                        "blit-netdash\n\n\
                         Linux TCP dashboard demo rendered from a blit surface.\n\n\
                         Usage:\n\
                           cargo run -p blit-demo --bin netdash -- [--fps N] [--poll-ms N]\n\n\
                         Options:\n\
                           --fps N      cap display updates per second (default: 12)\n\
                           --poll-ms N  full sock_diag reconcile cadence in ms (default: 120)\n"
                    );
                    std::process::exit(0);
                }
                _ => {}
            }
        }
        Args { fps, poll_ms }
    }

    fn get_term_size() -> (u16, u16) {
        unsafe {
            let mut ws: libc::winsize = std::mem::zeroed();
            if libc::ioctl(libc::STDOUT_FILENO, libc::TIOCGWINSZ, &mut ws) == 0
                && ws.ws_row > 0
                && ws.ws_col > 0
            {
                return (ws.ws_row, ws.ws_col);
            }
        }
        (30, 120)
    }

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
                raw.c_cc[libc::VMIN] = 0;
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

    struct Cleanup;

    impl Drop for Cleanup {
        fn drop(&mut self) {
            const RESET: &[u8] = b"\x1b[?1000l\x1b[?1006l\x1b[0m\x1b[?25h\x1b[?1049l\r\n";
            unsafe {
                libc::write(libc::STDOUT_FILENO, RESET.as_ptr().cast(), RESET.len());
            }
        }
    }

    struct AnsiRenderer {
        prev_cells: Vec<u8>,
        prev_rows: u16,
        prev_cols: u16,
        cur_row: u16,
        cur_col: u16,
        cur_fg: u64,
        cur_bg: u64,
        cur_attrs: u8,
        attrs_known: bool,
    }

    impl AnsiRenderer {
        fn new() -> Self {
            Self {
                prev_cells: Vec::new(),
                prev_rows: 0,
                prev_cols: 0,
                cur_row: u16::MAX,
                cur_col: u16::MAX,
                cur_fg: u64::MAX,
                cur_bg: u64::MAX,
                cur_attrs: 0,
                attrs_known: false,
            }
        }

        fn render(&mut self, frame: &FrameState, out: &mut Vec<u8>) {
            let resized = frame.rows() != self.prev_rows || frame.cols() != self.prev_cols;
            if resized {
                out.extend_from_slice(b"\x1b[2J");
                self.prev_cells.clear();
                self.prev_rows = frame.rows();
                self.prev_cols = frame.cols();
                self.cur_row = u16::MAX;
                self.cur_col = u16::MAX;
                self.attrs_known = false;
                self.cur_fg = u64::MAX;
                self.cur_bg = u64::MAX;
            }

            let total = frame.rows() as usize * frame.cols() as usize;
            if self.prev_cells.len() != total * CELL_SIZE {
                self.prev_cells = vec![0xff; total * CELL_SIZE];
            }

            for i in 0..total {
                let off = i * CELL_SIZE;
                if frame.cells()[off..off + CELL_SIZE] == self.prev_cells[off..off + CELL_SIZE] {
                    continue;
                }
                let row = (i / frame.cols() as usize) as u16;
                let col = (i % frame.cols() as usize) as u16;
                self.move_to(row, col, out);
                self.emit_cell(&frame.cells()[off..off + CELL_SIZE], out);
                self.prev_cells[off..off + CELL_SIZE]
                    .copy_from_slice(&frame.cells()[off..off + CELL_SIZE]);
            }

            out.extend_from_slice(b"\x1b[?25l");
        }

        fn emit_cell(&mut self, cell: &[u8], out: &mut Vec<u8>) {
            let f0 = cell[0];
            let f1 = cell[1];

            if f1 & 4 != 0 {
                self.reset_sgr(0, pack_color(0, 0, 0, 0), pack_color(0, 0, 0, 0), out);
                out.push(b' ');
                self.cur_col = self.cur_col.saturating_add(1);
                return;
            }

            let fg_type = f0 & 3;
            let bg_type = (f0 >> 2) & 3;
            let bold = (f0 >> 4) & 1;
            let dim = (f0 >> 5) & 1;
            let italic = (f0 >> 6) & 1;
            let underline = (f0 >> 7) & 1;
            let inverse = f1 & 1;
            let wide = (f1 >> 1) & 1;
            let content_len = ((f1 >> 3) & 7) as usize;
            let attrs: u8 = bold | (dim << 1) | (italic << 2) | (underline << 3) | (inverse << 4);

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

            self.reset_sgr(attrs, fg_packed, bg_packed, out);
            if content_len > 0 {
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

        fn reset_sgr(&mut self, attrs: u8, fg: u64, bg: u64, out: &mut Vec<u8>) {
            let need_reset = !self.attrs_known
                || (attrs & !self.cur_attrs) != 0
                || (self.cur_attrs & !attrs) != 0 && attrs < self.cur_attrs;

            if !self.attrs_known
                || self.cur_attrs != attrs
                || self.cur_fg != fg
                || self.cur_bg != bg
            {
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

    #[derive(Clone, Debug)]
    struct DashboardSnapshot {
        title: String,
        summary: String,
        detail: String,
        error: Option<String>,
        peers: Vec<PeerRow>,
        connections: Vec<ConnectionRow>,
        events: Vec<String>,
    }

    impl Default for DashboardSnapshot {
        fn default() -> Self {
            Self {
                title: "blit-netdash | warming up".into(),
                summary: "Watching Linux TCP sockets via sock_diag snapshots + destroy events"
                    .into(),
                detail: "Waiting for the first reconcile batch...".into(),
                error: None,
                peers: Vec::new(),
                connections: Vec::new(),
                events: vec!["launching sock_diag sampler and destroy stream".into()],
            }
        }
    }

    #[derive(Clone, Debug)]
    struct PeerRow {
        ip: IpAddr,
        label: String,
        connections: usize,
        packets_per_sec: f64,
        bytes_per_sec: f64,
        history: Vec<u32>,
    }

    #[derive(Clone, Debug)]
    struct ConnectionRow {
        state: &'static str,
        remote_ip: IpAddr,
        label: String,
        packets_per_sec: f64,
        bytes_per_sec: f64,
        rtt_ms: u32,
        queue_bytes: u32,
        history: Vec<u32>,
    }

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    enum Panel {
        Peers,
        Connections,
        Events,
    }

    impl Panel {
        fn next(self) -> Self {
            match self {
                Self::Peers => Self::Connections,
                Self::Connections => Self::Events,
                Self::Events => Self::Peers,
            }
        }

        fn prev(self) -> Self {
            match self {
                Self::Peers => Self::Events,
                Self::Connections => Self::Peers,
                Self::Events => Self::Connections,
            }
        }
    }

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    enum PeerSort {
        Packets,
        Bytes,
        Connections,
    }

    impl PeerSort {
        fn next(self) -> Self {
            match self {
                Self::Packets => Self::Bytes,
                Self::Bytes => Self::Connections,
                Self::Connections => Self::Packets,
            }
        }

        fn label(self) -> &'static str {
            match self {
                Self::Packets => "pps",
                Self::Bytes => "bytes",
                Self::Connections => "conns",
            }
        }
    }

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    enum ConnectionSort {
        Packets,
        Bytes,
        Rtt,
        Queue,
    }

    impl ConnectionSort {
        fn next(self) -> Self {
            match self {
                Self::Packets => Self::Bytes,
                Self::Bytes => Self::Rtt,
                Self::Rtt => Self::Queue,
                Self::Queue => Self::Packets,
            }
        }

        fn label(self) -> &'static str {
            match self {
                Self::Packets => "pps",
                Self::Bytes => "bytes",
                Self::Rtt => "rtt",
                Self::Queue => "queue",
            }
        }
    }

    #[derive(Clone, Debug)]
    struct UiState {
        focus: Panel,
        peer_selected: usize,
        peer_scroll: usize,
        connection_selected: usize,
        connection_scroll: usize,
        event_offset: usize,
        peer_sort: PeerSort,
        connection_sort: ConnectionSort,
        peer_filter: Option<IpAddr>,
        show_help: bool,
    }

    impl Default for UiState {
        fn default() -> Self {
            Self {
                focus: Panel::Connections,
                peer_selected: 0,
                peer_scroll: 0,
                connection_selected: 0,
                connection_scroll: 0,
                event_offset: 0,
                peer_sort: PeerSort::Packets,
                connection_sort: ConnectionSort::Packets,
                peer_filter: None,
                show_help: false,
            }
        }
    }

    #[derive(Clone, Copy, Debug)]
    struct MouseInput {
        row: u16,
        col: u16,
        kind: MouseKind,
    }

    #[derive(Clone, Copy, Debug)]
    enum MouseKind {
        Left,
        WheelUp,
        WheelDown,
    }

    #[derive(Clone, Copy, Debug)]
    enum InputEvent {
        Quit,
        NextPanel,
        PrevPanel,
        Focus(Panel),
        Move(i32),
        Page(i32),
        Home,
        End,
        Activate,
        CycleSort,
        ClearFilter,
        ToggleHelp,
        Mouse(MouseInput),
    }

    struct InputParser {
        pending: Vec<u8>,
    }

    impl InputParser {
        fn new() -> Self {
            Self {
                pending: Vec::with_capacity(64),
            }
        }

        fn feed(&mut self, bytes: &[u8]) -> Vec<InputEvent> {
            self.pending.extend_from_slice(bytes);
            let mut events = Vec::new();

            loop {
                if self.pending.is_empty() {
                    break;
                }

                let consumed = match self.pending[0] {
                    b'q' | b'Q' | 0x03 | 0x04 => {
                        events.push(InputEvent::Quit);
                        1
                    }
                    b'\t' | b'l' => {
                        events.push(InputEvent::NextPanel);
                        1
                    }
                    b'h' => {
                        events.push(InputEvent::PrevPanel);
                        1
                    }
                    b'1' => {
                        events.push(InputEvent::Focus(Panel::Peers));
                        1
                    }
                    b'2' => {
                        events.push(InputEvent::Focus(Panel::Connections));
                        1
                    }
                    b'3' => {
                        events.push(InputEvent::Focus(Panel::Events));
                        1
                    }
                    b'k' => {
                        events.push(InputEvent::Move(-1));
                        1
                    }
                    b'j' => {
                        events.push(InputEvent::Move(1));
                        1
                    }
                    b' ' | b'\r' | b'\n' => {
                        events.push(InputEvent::Activate);
                        1
                    }
                    b's' | b'S' => {
                        events.push(InputEvent::CycleSort);
                        1
                    }
                    b'c' | b'C' => {
                        events.push(InputEvent::ClearFilter);
                        1
                    }
                    b'?' => {
                        events.push(InputEvent::ToggleHelp);
                        1
                    }
                    0x1b => match self.parse_escape() {
                        EscapeParse::Event(consumed, event) => {
                            events.push(event);
                            consumed
                        }
                        EscapeParse::Ignore(consumed) => consumed,
                        EscapeParse::Incomplete => break,
                    },
                    _ => 1,
                };

                self.pending.drain(..consumed);
            }

            events
        }

        fn parse_escape(&self) -> EscapeParse {
            if self.pending.len() < 2 {
                return EscapeParse::Incomplete;
            }
            let buf = &self.pending;
            match buf[1] {
                b'[' => self.parse_csi(),
                b'O' => {
                    if buf.len() < 3 {
                        return EscapeParse::Incomplete;
                    }
                    match buf[2] {
                        b'H' => EscapeParse::Event(3, InputEvent::Home),
                        b'F' => EscapeParse::Event(3, InputEvent::End),
                        _ => EscapeParse::Ignore(2),
                    }
                }
                _ => EscapeParse::Ignore(1),
            }
        }

        fn parse_csi(&self) -> EscapeParse {
            let buf = &self.pending;
            if buf.len() < 3 {
                return EscapeParse::Incomplete;
            }
            match buf[2] {
                b'A' => EscapeParse::Event(3, InputEvent::Move(-1)),
                b'B' => EscapeParse::Event(3, InputEvent::Move(1)),
                b'C' => EscapeParse::Event(3, InputEvent::NextPanel),
                b'D' => EscapeParse::Event(3, InputEvent::PrevPanel),
                b'H' => EscapeParse::Event(3, InputEvent::Home),
                b'F' => EscapeParse::Event(3, InputEvent::End),
                b'Z' => EscapeParse::Event(3, InputEvent::PrevPanel),
                b'<' => self.parse_mouse(),
                b'1' | b'4' | b'5' | b'6' | b'7' | b'8' => self.parse_tilde_sequence(),
                _ => EscapeParse::Ignore(2),
            }
        }

        fn parse_tilde_sequence(&self) -> EscapeParse {
            let buf = &self.pending;
            let Some(term) = buf.iter().position(|&b| b == b'~') else {
                return EscapeParse::Incomplete;
            };
            let Some(number) = std::str::from_utf8(&buf[2..term])
                .ok()
                .and_then(|s| s.parse::<u16>().ok())
            else {
                return EscapeParse::Ignore(term + 1);
            };
            let event = match number {
                1 | 7 => InputEvent::Home,
                4 | 8 => InputEvent::End,
                5 => InputEvent::Page(-1),
                6 => InputEvent::Page(1),
                _ => return EscapeParse::Ignore(term + 1),
            };
            EscapeParse::Event(term + 1, event)
        }

        fn parse_mouse(&self) -> EscapeParse {
            let buf = &self.pending;
            let Some(term) = buf.iter().position(|&b| b == b'M' || b == b'm') else {
                return EscapeParse::Incomplete;
            };
            let body = match std::str::from_utf8(&buf[3..term]) {
                Ok(body) => body,
                Err(_) => return EscapeParse::Ignore(term + 1),
            };
            let mut parts = body.split(';');
            let button = match parts.next().and_then(|s| s.parse::<u16>().ok()) {
                Some(value) => value,
                None => return EscapeParse::Ignore(term + 1),
            };
            let col = match parts.next().and_then(|s| s.parse::<u16>().ok()) {
                Some(value) if value > 0 => value - 1,
                _ => return EscapeParse::Ignore(term + 1),
            };
            let row = match parts.next().and_then(|s| s.parse::<u16>().ok()) {
                Some(value) if value > 0 => value - 1,
                _ => return EscapeParse::Ignore(term + 1),
            };
            let kind = if button & 64 != 0 {
                if button & 1 == 0 {
                    MouseKind::WheelUp
                } else {
                    MouseKind::WheelDown
                }
            } else if buf[term] == b'M' && button & 3 == 0 {
                MouseKind::Left
            } else {
                return EscapeParse::Ignore(term + 1);
            };
            EscapeParse::Event(term + 1, InputEvent::Mouse(MouseInput { row, col, kind }))
        }
    }

    enum EscapeParse {
        Event(usize, InputEvent),
        Ignore(usize),
        Incomplete,
    }

    struct SharedSnapshot {
        revision: AtomicU64,
        snapshot: Mutex<DashboardSnapshot>,
    }

    impl SharedSnapshot {
        fn new(initial: DashboardSnapshot) -> Self {
            Self {
                revision: AtomicU64::new(1),
                snapshot: Mutex::new(initial),
            }
        }

        fn store(&self, snapshot: DashboardSnapshot) {
            *self.snapshot.lock().unwrap() = snapshot;
            self.revision.fetch_add(1, Ordering::Release);
        }

        fn load(&self, last_seen: &mut u64) -> Option<DashboardSnapshot> {
            let revision = self.revision.load(Ordering::Acquire);
            if revision == *last_seen {
                return None;
            }
            *last_seen = revision;
            Some(self.snapshot.lock().unwrap().clone())
        }
    }

    struct PresentPacer {
        min_interval: Duration,
        next_allowed: Instant,
    }

    impl PresentPacer {
        fn new(max_fps: u64) -> Self {
            let min_interval = Duration::from_nanos(1_000_000_000 / max_fps.max(1));
            Self {
                min_interval,
                next_allowed: Instant::now(),
            }
        }

        fn wait_duration(&self, now: Instant) -> Option<Duration> {
            (now < self.next_allowed).then_some(self.next_allowed - now)
        }

        fn record_present(&mut self, started: Instant, finished: Instant) {
            let target_ready_at = started.checked_add(self.min_interval).unwrap_or(finished);
            self.next_allowed = target_ready_at.max(finished);
        }
    }

    #[derive(Clone)]
    struct ConnKey {
        inode: u32,
        local_ip: IpAddr,
        local_port: u16,
        remote_ip: IpAddr,
        remote_port: u16,
        cookie: [u8; 8],
    }

    impl PartialEq for ConnKey {
        fn eq(&self, other: &Self) -> bool {
            self.inode == other.inode
                && self.local_ip == other.local_ip
                && self.local_port == other.local_port
                && self.remote_ip == other.remote_ip
                && self.remote_port == other.remote_port
                && self.cookie == other.cookie
        }
    }

    impl Eq for ConnKey {}

    impl Hash for ConnKey {
        fn hash<H: Hasher>(&self, state: &mut H) {
            self.inode.hash(state);
            self.local_ip.hash(state);
            self.local_port.hash(state);
            self.remote_ip.hash(state);
            self.remote_port.hash(state);
            self.cookie.hash(state);
        }
    }

    impl ConnKey {
        fn from_header(header: &InetResponseHeader) -> Self {
            Self {
                inode: header.inode,
                local_ip: header.socket_id.source_address,
                local_port: header.socket_id.source_port,
                remote_ip: header.socket_id.destination_address,
                remote_port: header.socket_id.destination_port,
                cookie: header.socket_id.cookie,
            }
        }
    }

    struct Tracker {
        started: Instant,
        last_sample: Option<Instant>,
        seq: u64,
        batches: u64,
        last_batch_ms: u128,
        last_batch_size: usize,
        last_error: Option<String>,
        connections: HashMap<ConnKey, TrackedConnection>,
        peers: HashMap<IpAddr, TrackedPeer>,
        events: VecDeque<String>,
    }

    struct TrackedConnection {
        local_ip: IpAddr,
        local_port: u16,
        remote_ip: IpAddr,
        remote_port: u16,
        state: u8,
        first_seen: Instant,
        last_seen: Instant,
        last_seen_seq: u64,
        seen_batches: u64,
        prev_segs_in: u64,
        prev_segs_out: u64,
        prev_bytes_in: u64,
        prev_bytes_out: u64,
        packets_per_sec: f64,
        bytes_per_sec: f64,
        rtt_ms: u32,
        queue_bytes: u32,
        history: VecDeque<u32>,
        observed_packets: u64,
        observed_bytes: u64,
        hot_mark: u64,
        is_listener: bool,
    }

    struct TrackedPeer {
        connections: usize,
        packets_per_sec: f64,
        bytes_per_sec: f64,
        history: VecDeque<u32>,
        last_seen_seq: u64,
        top_port: u16,
    }

    struct PeerRollup {
        connections: usize,
        packets_per_sec: f64,
        bytes_per_sec: f64,
        top_port: u16,
        top_packets_per_sec: f64,
    }

    impl Tracker {
        fn new() -> Self {
            Self {
                started: Instant::now(),
                last_sample: None,
                seq: 0,
                batches: 0,
                last_batch_ms: 0,
                last_batch_size: 0,
                last_error: None,
                connections: HashMap::new(),
                peers: HashMap::new(),
                events: VecDeque::new(),
            }
        }

        fn push_close_event(&mut self, sampled_at: Instant, conn: TrackedConnection) {
            if !conn.is_listener {
                self.push_event(
                    sampled_at,
                    format!(
                        "- {} after {} ({} packets)",
                        short_connection_label(
                            conn.local_ip,
                            conn.local_port,
                            conn.remote_ip,
                            conn.remote_port
                        ),
                        fmt_duration(sampled_at.saturating_duration_since(conn.first_seen)),
                        fmt_count(conn.observed_packets),
                    ),
                );
            }
        }

        fn remove_connection(&mut self, sampled_at: Instant, key: &ConnKey) -> bool {
            let Some(conn) = self.connections.remove(key) else {
                return false;
            };
            self.push_close_event(sampled_at, conn);
            true
        }

        fn refresh_peers(&mut self, push_history_sample: bool) {
            let mut peers: HashMap<IpAddr, PeerRollup> = HashMap::new();
            for conn in self.connections.values() {
                if conn.is_listener || is_unspecified_ip(&conn.remote_ip) {
                    continue;
                }
                let rollup = peers.entry(conn.remote_ip).or_insert(PeerRollup {
                    connections: 0,
                    packets_per_sec: 0.0,
                    bytes_per_sec: 0.0,
                    top_port: conn.remote_port,
                    top_packets_per_sec: 0.0,
                });
                rollup.connections += 1;
                rollup.packets_per_sec += conn.packets_per_sec;
                rollup.bytes_per_sec += conn.bytes_per_sec;
                if conn.packets_per_sec >= rollup.top_packets_per_sec {
                    rollup.top_packets_per_sec = conn.packets_per_sec;
                    rollup.top_port = conn.remote_port;
                }
            }

            for (ip, rollup) in peers {
                let peer = self.peers.entry(ip).or_insert(TrackedPeer {
                    connections: 0,
                    packets_per_sec: 0.0,
                    bytes_per_sec: 0.0,
                    history: VecDeque::new(),
                    last_seen_seq: self.seq,
                    top_port: rollup.top_port,
                });
                peer.connections = rollup.connections;
                peer.packets_per_sec = rollup.packets_per_sec;
                peer.bytes_per_sec = rollup.bytes_per_sec;
                peer.top_port = rollup.top_port;
                peer.last_seen_seq = self.seq;
                if push_history_sample {
                    push_history(&mut peer.history, peer.packets_per_sec);
                }
            }

            let stale_peers: Vec<_> = self
                .peers
                .iter()
                .filter(|(_, peer)| peer.last_seen_seq != self.seq)
                .map(|(ip, _)| *ip)
                .collect();
            for ip in stale_peers {
                self.peers.remove(&ip);
            }
        }

        fn apply_sample(
            &mut self,
            sampled_at: Instant,
            poll_ms: u64,
            fps: u64,
            batch_ms: u128,
            responses: Vec<InetResponse>,
        ) -> DashboardSnapshot {
            self.seq += 1;
            self.batches += 1;
            self.last_batch_ms = batch_ms;
            self.last_batch_size = responses.len();
            self.last_error = None;

            let dt_secs = self
                .last_sample
                .map(|prev| {
                    sampled_at
                        .saturating_duration_since(prev)
                        .as_secs_f64()
                        .max(0.001)
                })
                .unwrap_or(0.0);
            self.last_sample = Some(sampled_at);

            for response in responses {
                let header = response.header;
                let key = ConnKey::from_header(&header);
                let state = header.state;
                let is_listener =
                    state == TCP_LISTEN || is_unspecified_ip(&header.socket_id.destination_address);

                let mut segs_in = 0u64;
                let mut segs_out = 0u64;
                let mut bytes_in = 0u64;
                let mut bytes_out = 0u64;
                let mut rtt_ms = 0u32;

                for nla in response.nlas {
                    if let Nla::TcpInfo(info) = nla {
                        segs_in = info.segs_in as u64;
                        segs_out = info.segs_out as u64;
                        bytes_in = info.bytes_received;
                        bytes_out = info.bytes_sent;
                        rtt_ms = info.rtt / 1_000;
                    }
                }

                let mut open_event = None;
                let mut hot_event = None;

                {
                    let conn = match self.connections.entry(key) {
                        std::collections::hash_map::Entry::Occupied(entry) => entry.into_mut(),
                        std::collections::hash_map::Entry::Vacant(entry) => {
                            if !is_listener {
                                open_event = Some(format!(
                                    "+ {} {}",
                                    tcp_state_name(state),
                                    short_connection_label(
                                        header.socket_id.source_address,
                                        header.socket_id.source_port,
                                        header.socket_id.destination_address,
                                        header.socket_id.destination_port,
                                    ),
                                ));
                            }
                            entry.insert(TrackedConnection {
                                local_ip: header.socket_id.source_address,
                                local_port: header.socket_id.source_port,
                                remote_ip: header.socket_id.destination_address,
                                remote_port: header.socket_id.destination_port,
                                state,
                                first_seen: sampled_at,
                                last_seen: sampled_at,
                                last_seen_seq: self.seq,
                                seen_batches: 0,
                                prev_segs_in: segs_in,
                                prev_segs_out: segs_out,
                                prev_bytes_in: bytes_in,
                                prev_bytes_out: bytes_out,
                                packets_per_sec: 0.0,
                                bytes_per_sec: 0.0,
                                rtt_ms,
                                queue_bytes: header.recv_queue.saturating_add(header.send_queue),
                                history: VecDeque::new(),
                                observed_packets: 0,
                                observed_bytes: 0,
                                hot_mark: 0,
                                is_listener,
                            })
                        }
                    };

                    let (delta_packets, delta_bytes) = if conn.seen_batches > 0 && dt_secs > 0.0 {
                        let packet_delta = segs_in.saturating_sub(conn.prev_segs_in)
                            + segs_out.saturating_sub(conn.prev_segs_out);
                        let byte_delta = bytes_in.saturating_sub(conn.prev_bytes_in)
                            + bytes_out.saturating_sub(conn.prev_bytes_out);
                        (packet_delta, byte_delta)
                    } else {
                        (0, 0)
                    };

                    conn.state = state;
                    conn.last_seen = sampled_at;
                    conn.last_seen_seq = self.seq;
                    conn.seen_batches += 1;
                    conn.prev_segs_in = segs_in;
                    conn.prev_segs_out = segs_out;
                    conn.prev_bytes_in = bytes_in;
                    conn.prev_bytes_out = bytes_out;
                    conn.packets_per_sec = if dt_secs > 0.0 {
                        delta_packets as f64 / dt_secs
                    } else {
                        0.0
                    };
                    conn.bytes_per_sec = if dt_secs > 0.0 {
                        delta_bytes as f64 / dt_secs
                    } else {
                        0.0
                    };
                    conn.rtt_ms = rtt_ms;
                    conn.queue_bytes = header.recv_queue.saturating_add(header.send_queue);
                    conn.observed_packets = conn.observed_packets.saturating_add(delta_packets);
                    conn.observed_bytes = conn.observed_bytes.saturating_add(delta_bytes);
                    push_history(&mut conn.history, conn.packets_per_sec);

                    if !conn.is_listener
                        && conn.packets_per_sec >= 500.0
                        && self.seq > conn.hot_mark + 20
                    {
                        conn.hot_mark = self.seq;
                        hot_event = Some(format!(
                            "! {} at {}/s",
                            short_connection_label(
                                conn.local_ip,
                                conn.local_port,
                                conn.remote_ip,
                                conn.remote_port
                            ),
                            fmt_packets(conn.packets_per_sec),
                        ));
                    }
                }

                if let Some(event) = open_event {
                    self.push_event(sampled_at, event);
                }
                if let Some(event) = hot_event {
                    self.push_event(sampled_at, event);
                }
            }

            let stale: Vec<_> = self
                .connections
                .iter()
                .filter(|(_, conn)| conn.last_seen_seq != self.seq)
                .map(|(key, _)| key.clone())
                .collect();
            for key in stale {
                self.remove_connection(sampled_at, &key);
            }

            self.refresh_peers(true);

            self.make_snapshot(poll_ms, fps)
        }

        fn apply_destroyed(
            &mut self,
            sampled_at: Instant,
            poll_ms: u64,
            fps: u64,
            destroyed: Vec<ConnKey>,
        ) -> Option<DashboardSnapshot> {
            let mut removed_any = false;
            for key in destroyed {
                removed_any |= self.remove_connection(sampled_at, &key);
            }
            if !removed_any {
                return None;
            }
            self.seq += 1;
            self.last_error = None;
            self.refresh_peers(false);
            Some(self.make_snapshot(poll_ms, fps))
        }

        fn apply_error(
            &mut self,
            sampled_at: Instant,
            poll_ms: u64,
            fps: u64,
            error: String,
        ) -> DashboardSnapshot {
            self.last_error = Some(error.clone());
            self.push_event(sampled_at, format!("x sampler error: {error}"));
            self.make_snapshot(poll_ms, fps)
        }

        fn make_snapshot(&self, poll_ms: u64, fps: u64) -> DashboardSnapshot {
            let mut state_counts: HashMap<&'static str, usize> = HashMap::new();
            let mut total_packets_per_sec = 0.0;
            let mut total_bytes_per_sec = 0.0;
            let mut listener_count = 0usize;
            let mut live_connection_count = 0usize;

            let mut connections: Vec<_> = self
                .connections
                .values()
                .map(|conn| {
                    *state_counts.entry(tcp_state_name(conn.state)).or_insert(0) += 1;
                    if conn.is_listener {
                        listener_count += 1;
                    } else {
                        live_connection_count += 1;
                        total_packets_per_sec += conn.packets_per_sec;
                        total_bytes_per_sec += conn.bytes_per_sec;
                    }
                    ConnectionRow {
                        state: tcp_state_name(conn.state),
                        remote_ip: conn.remote_ip,
                        label: format_connection_label(conn),
                        packets_per_sec: conn.packets_per_sec,
                        bytes_per_sec: conn.bytes_per_sec,
                        rtt_ms: conn.rtt_ms,
                        queue_bytes: conn.queue_bytes,
                        history: conn.history.iter().copied().collect(),
                    }
                })
                .filter(|conn| conn.state != tcp_state_name(TCP_LISTEN))
                .collect();

            connections.sort_by(|a, b| {
                b.packets_per_sec
                    .total_cmp(&a.packets_per_sec)
                    .then_with(|| b.bytes_per_sec.total_cmp(&a.bytes_per_sec))
                    .then_with(|| a.label.cmp(&b.label))
            });
            connections.truncate(MAX_RENDER_CONNECTIONS);

            let mut peers: Vec<_> = self
                .peers
                .iter()
                .map(|(ip, peer)| PeerRow {
                    ip: *ip,
                    label: format!("{ip}:{}", peer.top_port),
                    connections: peer.connections,
                    packets_per_sec: peer.packets_per_sec,
                    bytes_per_sec: peer.bytes_per_sec,
                    history: peer.history.iter().copied().collect(),
                })
                .collect();
            peers.sort_by(|a, b| {
                b.packets_per_sec
                    .total_cmp(&a.packets_per_sec)
                    .then_with(|| b.connections.cmp(&a.connections))
                    .then_with(|| a.label.cmp(&b.label))
            });
            peers.truncate(MAX_RENDER_PEERS);

            let mut states: Vec<_> = state_counts.into_iter().collect();
            states.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(b.0)));
            let state_summary = if states.is_empty() {
                "states idle".to_string()
            } else {
                states
                    .iter()
                    .take(6)
                    .map(|(state, count)| format!("{state} {count}"))
                    .collect::<Vec<_>>()
                    .join("  ")
            };

            let error = self.last_error.clone();
            let title = format!(
                "blit-netdash | {} conns | {} peers | {}/s | {}/s",
                live_connection_count,
                self.peers.len(),
                fmt_packets(total_packets_per_sec),
                fmt_bytes(total_bytes_per_sec),
            );
            let summary = format!(
                "{} live TCP flows, {} listeners, {} remote IPs, {} batch sockets",
                live_connection_count,
                listener_count,
                self.peers.len(),
                self.last_batch_size,
            );
            let mut detail = format!(
                "reconcile {}ms | render <= {}fps | batch {}ms | sample {} | destroy events live | {}",
                poll_ms, fps, self.last_batch_ms, self.batches, state_summary,
            );
            if let Some(err) = &error {
                detail.push_str(" | ");
                detail.push_str(err);
            }

            DashboardSnapshot {
                title,
                summary,
                detail,
                error,
                peers,
                connections,
                events: self.events.iter().cloned().collect(),
            }
        }

        fn push_event(&mut self, sampled_at: Instant, line: String) {
            if self.events.len() == MAX_EVENTS {
                self.events.pop_front();
            }
            let elapsed = sampled_at.saturating_duration_since(self.started);
            self.events
                .push_back(format!("{:>7.1}s  {}", elapsed.as_secs_f64(), line));
        }
    }

    struct Sampler {
        dump_socket: Socket,
        destroy_socket: Socket,
        ipv6_enabled: bool,
    }

    impl Sampler {
        fn new() -> io::Result<Self> {
            let mut dump_socket = Socket::new(NETLINK_SOCK_DIAG)?;
            let _ = dump_socket.bind_auto()?;
            dump_socket.connect(&SocketAddr::new(0, 0))?;
            let mut destroy_socket = Socket::new(NETLINK_SOCK_DIAG)?;
            let _ = destroy_socket.bind_auto()?;
            destroy_socket.add_membership(SOCK_DIAG_INET_TCP_DESTROY_GROUP)?;
            destroy_socket.set_non_blocking(true)?;
            let ipv6_enabled = fs::metadata("/proc/net/if_inet6").is_ok();
            if ipv6_enabled {
                destroy_socket.add_membership(SOCK_DIAG_INET6_TCP_DESTROY_GROUP)?;
            }
            Ok(Self {
                dump_socket,
                destroy_socket,
                ipv6_enabled,
            })
        }

        fn collect(&mut self) -> io::Result<Vec<InetResponse>> {
            let mut responses = self.dump_family(AF_INET)?;
            if self.ipv6_enabled {
                responses.extend(self.dump_family(AF_INET6)?);
            }
            Ok(responses)
        }

        fn wait_for_destroy_events_until(&self, deadline: Instant) -> io::Result<bool> {
            loop {
                let now = Instant::now();
                if now >= deadline {
                    return Ok(false);
                }
                let timeout_ms = deadline
                    .saturating_duration_since(now)
                    .as_millis()
                    .min(i32::MAX as u128) as i32;
                let mut poll_fd = libc::pollfd {
                    fd: self.destroy_socket.as_raw_fd(),
                    events: libc::POLLIN,
                    revents: 0,
                };
                let ready = unsafe { libc::poll(&mut poll_fd, 1, timeout_ms) };
                if ready < 0 {
                    let err = io::Error::last_os_error();
                    if err.kind() == io::ErrorKind::Interrupted {
                        continue;
                    }
                    return Err(err);
                }
                return Ok(ready > 0
                    && (poll_fd.revents & (libc::POLLIN | libc::POLLERR | libc::POLLHUP)) != 0);
            }
        }

        fn drain_destroyed(&mut self) -> io::Result<Vec<ConnKey>> {
            let mut destroyed = Vec::new();
            loop {
                match self.destroy_socket.recv_from_full() {
                    Ok((receive_buffer, _)) => {
                        if receive_buffer.is_empty() {
                            break;
                        }
                        let mut responses = Vec::new();
                        parse_sock_diag_messages(&receive_buffer, &mut responses)?;
                        destroyed.extend(
                            responses
                                .into_iter()
                                .map(|response| ConnKey::from_header(&response.header)),
                        );
                    }
                    Err(err) if err.kind() == io::ErrorKind::WouldBlock => break,
                    Err(err) => return Err(err),
                }
            }
            Ok(destroyed)
        }

        fn dump_family(&mut self, family: u8) -> io::Result<Vec<InetResponse>> {
            let request = InetRequest {
                family,
                protocol: IPPROTO_TCP,
                extensions: ExtensionFlags::INFO,
                states: StateFlags::all(),
                socket_id: if family == AF_INET {
                    SocketId::new_v4()
                } else {
                    SocketId::new_v6()
                },
            };

            let mut header = NetlinkHeader::default();
            header.flags = NLM_F_REQUEST | NLM_F_ACK | NLM_F_DUMP;
            let mut message =
                NetlinkMessage::new(header, SockDiagMessage::InetRequest(request).into());
            message.finalize();

            let mut request_buffer = vec![0; message.buffer_len()];
            message.serialize(&mut request_buffer);
            self.dump_socket.send(&request_buffer, 0)?;

            let mut responses = Vec::new();
            loop {
                let (receive_buffer, _) = self.dump_socket.recv_from_full()?;
                if receive_buffer.is_empty() {
                    break;
                }
                let done = parse_sock_diag_messages(&receive_buffer, &mut responses)?;
                if done {
                    break;
                }
            }
            Ok(responses)
        }
    }

    fn parse_sock_diag_messages(
        buffer: &[u8],
        responses: &mut Vec<InetResponse>,
    ) -> io::Result<bool> {
        let mut offset = 0usize;
        while offset < buffer.len() {
            let remaining = &buffer[offset..];
            let packet = <NetlinkMessage<SockDiagMessage>>::deserialize(remaining)
                .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err.to_string()))?;
            let length = packet.header.length as usize;
            if length == 0 || length > remaining.len() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("invalid netlink packet length {length}"),
                ));
            }

            match packet.payload {
                NetlinkPayload::Noop => {}
                NetlinkPayload::InnerMessage(SockDiagMessage::InetResponse(response)) => {
                    responses.push(*response);
                }
                NetlinkPayload::Done(_) => return Ok(true),
                NetlinkPayload::Error(err) => {
                    if err.code.is_none() {
                        offset += length;
                        continue;
                    }
                    return Err(io::Error::other(format!("sock_diag error: {err:?}")));
                }
                _ => {}
            }

            offset += length;
        }
        Ok(false)
    }

    #[derive(Clone, Copy)]
    struct DashboardLayout {
        banner_rows: u16,
        peers_rect: Rect,
        connections_rect: Rect,
        events_rect: Rect,
    }

    fn compute_layout(rows: u16, cols: u16) -> Option<DashboardLayout> {
        if rows < 14 || cols < 72 {
            return None;
        }

        let banner_rows = 5;
        let gap = 1;
        let log_rows = rows.saturating_sub(banner_rows + 4).clamp(6, 9);
        let body_row = banner_rows + gap;
        let body_rows = rows.saturating_sub(body_row + log_rows + gap);
        let wide = cols >= 110;

        let peers_rect = if wide {
            Rect::new(body_row, 0, body_rows, (cols * 36 / 100).max(32))
        } else {
            Rect::new(body_row, 0, body_rows / 2, cols)
        };
        let connections_rect = if wide {
            Rect::new(
                body_row,
                peers_rect.cols + gap,
                body_rows,
                cols.saturating_sub(peers_rect.cols + gap),
            )
        } else {
            Rect::new(
                peers_rect.row + peers_rect.rows + gap,
                0,
                body_rows.saturating_sub(peers_rect.rows + gap),
                cols,
            )
        };
        let events_rect = Rect::new(rows.saturating_sub(log_rows), 0, log_rows, cols);

        Some(DashboardLayout {
            banner_rows,
            peers_rect,
            connections_rect,
            events_rect,
        })
    }

    fn sorted_peers<'a>(snapshot: &'a DashboardSnapshot, ui: &UiState) -> Vec<&'a PeerRow> {
        let mut peers = snapshot.peers.iter().collect::<Vec<_>>();
        peers.sort_by(|a, b| match ui.peer_sort {
            PeerSort::Packets => b
                .packets_per_sec
                .total_cmp(&a.packets_per_sec)
                .then_with(|| b.bytes_per_sec.total_cmp(&a.bytes_per_sec))
                .then_with(|| a.label.cmp(&b.label)),
            PeerSort::Bytes => b
                .bytes_per_sec
                .total_cmp(&a.bytes_per_sec)
                .then_with(|| b.packets_per_sec.total_cmp(&a.packets_per_sec))
                .then_with(|| a.label.cmp(&b.label)),
            PeerSort::Connections => b
                .connections
                .cmp(&a.connections)
                .then_with(|| b.packets_per_sec.total_cmp(&a.packets_per_sec))
                .then_with(|| a.label.cmp(&b.label)),
        });
        peers
    }

    fn sorted_connections<'a>(
        snapshot: &'a DashboardSnapshot,
        ui: &UiState,
    ) -> Vec<&'a ConnectionRow> {
        let mut connections = snapshot
            .connections
            .iter()
            .filter(|conn| ui.peer_filter.is_none_or(|ip| conn.remote_ip == ip))
            .collect::<Vec<_>>();
        connections.sort_by(|a, b| match ui.connection_sort {
            ConnectionSort::Packets => b
                .packets_per_sec
                .total_cmp(&a.packets_per_sec)
                .then_with(|| b.bytes_per_sec.total_cmp(&a.bytes_per_sec))
                .then_with(|| a.label.cmp(&b.label)),
            ConnectionSort::Bytes => b
                .bytes_per_sec
                .total_cmp(&a.bytes_per_sec)
                .then_with(|| b.packets_per_sec.total_cmp(&a.packets_per_sec))
                .then_with(|| a.label.cmp(&b.label)),
            ConnectionSort::Rtt => b
                .rtt_ms
                .cmp(&a.rtt_ms)
                .then_with(|| b.packets_per_sec.total_cmp(&a.packets_per_sec))
                .then_with(|| a.label.cmp(&b.label)),
            ConnectionSort::Queue => b
                .queue_bytes
                .cmp(&a.queue_bytes)
                .then_with(|| b.packets_per_sec.total_cmp(&a.packets_per_sec))
                .then_with(|| a.label.cmp(&b.label)),
        });
        connections
    }

    fn panel_body_rows(rect: Rect) -> usize {
        rect.rows.saturating_sub(2) as usize
    }

    fn clamp_ui_state(
        ui: &mut UiState,
        snapshot: &DashboardSnapshot,
        layout: Option<DashboardLayout>,
    ) {
        if let Some(filter_ip) = ui.peer_filter
            && !snapshot.peers.iter().any(|peer| peer.ip == filter_ip)
        {
            ui.peer_filter = None;
        }

        let peers = sorted_peers(snapshot, ui);
        let connections = sorted_connections(snapshot, ui);

        if let Some(layout) = layout {
            clamp_selection(
                &mut ui.peer_selected,
                &mut ui.peer_scroll,
                peers.len(),
                panel_body_rows(layout.peers_rect),
            );
            clamp_selection(
                &mut ui.connection_selected,
                &mut ui.connection_scroll,
                connections.len(),
                panel_body_rows(layout.connections_rect),
            );
            let max_event_offset = snapshot
                .events
                .len()
                .saturating_sub(layout.events_rect.rows.saturating_sub(1) as usize);
            ui.event_offset = ui.event_offset.min(max_event_offset);
        } else {
            ui.peer_selected = ui.peer_selected.min(peers.len().saturating_sub(1));
            ui.connection_selected = ui
                .connection_selected
                .min(connections.len().saturating_sub(1));
            ui.peer_scroll = 0;
            ui.connection_scroll = 0;
            ui.event_offset = 0;
        }
    }

    fn clamp_selection(selected: &mut usize, scroll: &mut usize, len: usize, visible: usize) {
        if len == 0 || visible == 0 {
            *selected = 0;
            *scroll = 0;
            return;
        }
        *selected = (*selected).min(len - 1);
        let max_scroll = len.saturating_sub(visible);
        *scroll = (*scroll).min(max_scroll);
        if *selected < *scroll {
            *scroll = *selected;
        }
        if *selected >= *scroll + visible {
            *scroll = *selected + 1 - visible;
        }
    }

    fn apply_input_event(
        ui: &mut UiState,
        snapshot: &DashboardSnapshot,
        rows: u16,
        cols: u16,
        event: InputEvent,
    ) -> bool {
        let layout = compute_layout(rows, cols);
        let mut changed = false;

        match event {
            InputEvent::Quit => {}
            InputEvent::NextPanel => {
                ui.focus = ui.focus.next();
                changed = true;
            }
            InputEvent::PrevPanel => {
                ui.focus = ui.focus.prev();
                changed = true;
            }
            InputEvent::Focus(panel) => {
                if ui.focus != panel {
                    ui.focus = panel;
                    changed = true;
                }
            }
            InputEvent::Move(delta) => {
                changed |= move_active_selection(ui, snapshot, layout, delta);
            }
            InputEvent::Page(delta) => {
                changed |= page_active_selection(ui, snapshot, layout, delta);
            }
            InputEvent::Home => {
                changed |= jump_active_selection(ui, snapshot, layout, true);
            }
            InputEvent::End => {
                changed |= jump_active_selection(ui, snapshot, layout, false);
            }
            InputEvent::Activate => {
                if ui.focus == Panel::Peers {
                    let peers = sorted_peers(snapshot, ui);
                    if let Some(peer) = peers.get(ui.peer_selected) {
                        let next_filter = if ui.peer_filter == Some(peer.ip) {
                            None
                        } else {
                            Some(peer.ip)
                        };
                        if ui.peer_filter != next_filter {
                            ui.peer_filter = next_filter;
                            ui.connection_selected = 0;
                            ui.connection_scroll = 0;
                            ui.focus = Panel::Connections;
                            changed = true;
                        }
                    }
                }
            }
            InputEvent::CycleSort => {
                match ui.focus {
                    Panel::Peers => ui.peer_sort = ui.peer_sort.next(),
                    Panel::Connections => ui.connection_sort = ui.connection_sort.next(),
                    Panel::Events => {}
                }
                changed = ui.focus != Panel::Events;
            }
            InputEvent::ClearFilter => {
                if ui.peer_filter.take().is_some() {
                    ui.connection_selected = 0;
                    ui.connection_scroll = 0;
                    changed = true;
                }
            }
            InputEvent::ToggleHelp => {
                ui.show_help = !ui.show_help;
                changed = true;
            }
            InputEvent::Mouse(mouse) => {
                changed |= handle_mouse(ui, snapshot, layout, mouse);
            }
        }

        if changed {
            clamp_ui_state(ui, snapshot, layout);
        }
        changed
    }

    fn move_active_selection(
        ui: &mut UiState,
        snapshot: &DashboardSnapshot,
        layout: Option<DashboardLayout>,
        delta: i32,
    ) -> bool {
        match ui.focus {
            Panel::Peers => {
                let peers = sorted_peers(snapshot, ui);
                shift_selection(&mut ui.peer_selected, peers.len(), delta)
            }
            Panel::Connections => {
                let connections = sorted_connections(snapshot, ui);
                shift_selection(&mut ui.connection_selected, connections.len(), delta)
            }
            Panel::Events => {
                let max_event_offset = layout
                    .map(|layout| {
                        snapshot
                            .events
                            .len()
                            .saturating_sub(layout.events_rect.rows.saturating_sub(1) as usize)
                    })
                    .unwrap_or(0);
                shift_offset(&mut ui.event_offset, max_event_offset, -delta)
            }
        }
    }

    fn page_active_selection(
        ui: &mut UiState,
        snapshot: &DashboardSnapshot,
        layout: Option<DashboardLayout>,
        delta: i32,
    ) -> bool {
        let step = match (layout, ui.focus) {
            (Some(layout), Panel::Peers) => panel_body_rows(layout.peers_rect).saturating_sub(1),
            (Some(layout), Panel::Connections) => {
                panel_body_rows(layout.connections_rect).saturating_sub(1)
            }
            (Some(layout), Panel::Events) => layout.events_rect.rows.saturating_sub(1) as usize,
            (None, _) => 8,
        }
        .max(1);
        move_active_selection(ui, snapshot, layout, delta.saturating_mul(step as i32))
    }

    fn jump_active_selection(
        ui: &mut UiState,
        snapshot: &DashboardSnapshot,
        layout: Option<DashboardLayout>,
        to_start: bool,
    ) -> bool {
        match ui.focus {
            Panel::Peers => {
                let len = sorted_peers(snapshot, ui).len();
                let next = if to_start { 0 } else { len.saturating_sub(1) };
                set_if_changed(&mut ui.peer_selected, next)
            }
            Panel::Connections => {
                let len = sorted_connections(snapshot, ui).len();
                let next = if to_start { 0 } else { len.saturating_sub(1) };
                set_if_changed(&mut ui.connection_selected, next)
            }
            Panel::Events => {
                let max_event_offset = layout
                    .map(|layout| {
                        snapshot
                            .events
                            .len()
                            .saturating_sub(layout.events_rect.rows.saturating_sub(1) as usize)
                    })
                    .unwrap_or(0);
                let next = if to_start { max_event_offset } else { 0 };
                set_if_changed(&mut ui.event_offset, next)
            }
        }
    }

    fn shift_selection(selected: &mut usize, len: usize, delta: i32) -> bool {
        if len == 0 || delta == 0 {
            return false;
        }
        let next = selected
            .saturating_add_signed(delta as isize)
            .min(len.saturating_sub(1));
        set_if_changed(selected, next)
    }

    fn shift_offset(offset: &mut usize, max: usize, delta: i32) -> bool {
        if delta == 0 {
            return false;
        }
        let next = offset.saturating_add_signed(delta as isize).min(max);
        set_if_changed(offset, next)
    }

    fn set_if_changed(slot: &mut usize, next: usize) -> bool {
        if *slot == next {
            false
        } else {
            *slot = next;
            true
        }
    }

    fn handle_mouse(
        ui: &mut UiState,
        snapshot: &DashboardSnapshot,
        layout: Option<DashboardLayout>,
        mouse: MouseInput,
    ) -> bool {
        let Some(layout) = layout else {
            return false;
        };
        let mut changed = false;

        if rect_contains(layout.peers_rect, mouse.row, mouse.col) {
            let peer_len = sorted_peers(snapshot, ui).len();
            changed |= set_focus(ui, Panel::Peers);
            changed |= apply_mouse_to_list(
                &mut ui.peer_selected,
                &mut ui.peer_scroll,
                peer_len,
                layout.peers_rect,
                mouse,
            );
        } else if rect_contains(layout.connections_rect, mouse.row, mouse.col) {
            let connection_len = sorted_connections(snapshot, ui).len();
            changed |= set_focus(ui, Panel::Connections);
            changed |= apply_mouse_to_list(
                &mut ui.connection_selected,
                &mut ui.connection_scroll,
                connection_len,
                layout.connections_rect,
                mouse,
            );
        } else if rect_contains(layout.events_rect, mouse.row, mouse.col) {
            changed |= set_focus(ui, Panel::Events);
            changed |= apply_mouse_to_events(ui, snapshot, layout.events_rect, mouse);
        }

        changed
    }

    fn set_focus(ui: &mut UiState, panel: Panel) -> bool {
        if ui.focus == panel {
            false
        } else {
            ui.focus = panel;
            true
        }
    }

    fn apply_mouse_to_list(
        selected: &mut usize,
        scroll: &mut usize,
        len: usize,
        rect: Rect,
        mouse: MouseInput,
    ) -> bool {
        let visible = panel_body_rows(rect);
        match mouse.kind {
            MouseKind::WheelUp => shift_selection(selected, len, -3),
            MouseKind::WheelDown => shift_selection(selected, len, 3),
            MouseKind::Left => {
                if len == 0 || visible == 0 {
                    return false;
                }
                if mouse.row < rect.row + 2 {
                    return false;
                }
                let local = (mouse.row - (rect.row + 2)) as usize;
                if local >= visible {
                    return false;
                }
                let index = (*scroll + local).min(len.saturating_sub(1));
                set_if_changed(selected, index)
            }
        }
    }

    fn apply_mouse_to_events(
        ui: &mut UiState,
        snapshot: &DashboardSnapshot,
        rect: Rect,
        mouse: MouseInput,
    ) -> bool {
        let max_offset = snapshot
            .events
            .len()
            .saturating_sub(rect.rows.saturating_sub(1) as usize);
        match mouse.kind {
            MouseKind::WheelUp => shift_offset(&mut ui.event_offset, max_offset, 3),
            MouseKind::WheelDown => shift_offset(&mut ui.event_offset, max_offset, -3),
            MouseKind::Left => false,
        }
    }

    fn rect_contains(rect: Rect, row: u16, col: u16) -> bool {
        row >= rect.row
            && row < rect.row.saturating_add(rect.rows)
            && col >= rect.col
            && col < rect.col.saturating_add(rect.cols)
    }

    fn status_line(ui: &UiState, snapshot: &DashboardSnapshot) -> String {
        match ui.focus {
            Panel::Peers => {
                let peers = sorted_peers(snapshot, ui);
                if let Some(peer) = peers.get(ui.peer_selected) {
                    format!(
                        "peer {} | {} conns | {}/s | {}/s{}",
                        peer.label,
                        peer.connections,
                        fmt_packets(peer.packets_per_sec),
                        fmt_bytes(peer.bytes_per_sec),
                        if ui.peer_filter == Some(peer.ip) {
                            " | filter active"
                        } else {
                            " | enter filters connections"
                        }
                    )
                } else {
                    "peer panel idle".into()
                }
            }
            Panel::Connections => {
                let connections = sorted_connections(snapshot, ui);
                if let Some(conn) = connections.get(ui.connection_selected) {
                    format!(
                        "flow {} | {} | {}/s | {}/s | {}ms rtt | {} queued{}",
                        conn.label,
                        conn.state,
                        fmt_packets(conn.packets_per_sec),
                        fmt_bytes(conn.bytes_per_sec),
                        conn.rtt_ms,
                        fmt_queue(conn.queue_bytes),
                        ui.peer_filter
                            .map(|ip| format!(" | filtered on {ip}"))
                            .unwrap_or_default(),
                    )
                } else if let Some(ip) = ui.peer_filter {
                    format!("no visible connections for peer filter {ip}")
                } else {
                    "connection panel idle".into()
                }
            }
            Panel::Events => format!(
                "events | offset {} from tail | newest at bottom",
                ui.event_offset
            ),
        }
    }

    pub fn main() -> io::Result<()> {
        let args = parse_args();
        let initial = DashboardSnapshot {
            detail: format!(
                "reconcile {}ms | render <= {}fps | destroy events live | tab switches panels | enter filters peer | q quits",
                args.poll_ms, args.fps
            ),
            ..DashboardSnapshot::default()
        };
        let shared = Arc::new(SharedSnapshot::new(initial));
        let stop = Arc::new(AtomicBool::new(false));

        let sampler_shared = Arc::clone(&shared);
        let sampler_stop = Arc::clone(&stop);
        let sampler_args = args;
        let sampler = thread::spawn(move || {
            let mut tracker = Tracker::new();
            let reconcile_interval = Duration::from_millis(sampler_args.poll_ms.max(1));
            let mut sampler = match Sampler::new() {
                Ok(sampler) => sampler,
                Err(err) => {
                    sampler_shared.store(tracker.apply_error(
                        Instant::now(),
                        sampler_args.poll_ms,
                        sampler_args.fps,
                        err.to_string(),
                    ));
                    return;
                }
            };

            let mut next_reconcile_at = Instant::now();
            while !sampler_stop.load(Ordering::Relaxed) {
                let now = Instant::now();
                if now >= next_reconcile_at {
                    let started = now;
                    let snapshot = match sampler.collect() {
                        Ok(responses) => tracker.apply_sample(
                            started,
                            sampler_args.poll_ms,
                            sampler_args.fps,
                            started.elapsed().as_millis(),
                            responses,
                        ),
                        Err(err) => tracker.apply_error(
                            started,
                            sampler_args.poll_ms,
                            sampler_args.fps,
                            err.to_string(),
                        ),
                    };
                    sampler_shared.store(snapshot);

                    let finished = Instant::now();
                    next_reconcile_at = started
                        .checked_add(reconcile_interval)
                        .unwrap_or(finished)
                        .max(finished);
                    continue;
                }

                let wait_deadline = next_reconcile_at.min(now + Duration::from_millis(100));
                match sampler.wait_for_destroy_events_until(wait_deadline) {
                    Ok(true) => match sampler.drain_destroyed() {
                        Ok(destroyed) => {
                            if let Some(snapshot) = tracker.apply_destroyed(
                                Instant::now(),
                                sampler_args.poll_ms,
                                sampler_args.fps,
                                destroyed,
                            ) {
                                sampler_shared.store(snapshot);
                            }
                        }
                        Err(err) => sampler_shared.store(tracker.apply_error(
                            Instant::now(),
                            sampler_args.poll_ms,
                            sampler_args.fps,
                            format!("destroy stream error: {err}"),
                        )),
                    },
                    Ok(false) => {}
                    Err(err) => sampler_shared.store(tracker.apply_error(
                        Instant::now(),
                        sampler_args.poll_ms,
                        sampler_args.fps,
                        format!("destroy stream wait error: {err}"),
                    )),
                }
            }
        });

        let _raw = RawMode::enter();
        let _cleanup = Cleanup;

        let mut stdout = io::stdout();
        stdout.write_all(b"\x1b[?1049h\x1b[?25l\x1b[?1000h\x1b[?1006h\x1b[2J")?;
        stdout.flush()?;

        let (mut rows, mut cols) = get_term_size();
        let mut surface = CallbackRenderer::new(rows, cols);
        let mut ansi = AnsiRenderer::new();
        let mut out = Vec::with_capacity(256 * 1024);
        let mut current_title = String::new();
        let mut last_revision = 0u64;
        let mut snapshot = shared.snapshot.lock().unwrap().clone();
        let mut pacer = PresentPacer::new(args.fps);
        let mut stdin_buf = [0u8; 128];
        let mut parser = InputParser::new();
        let mut ui = UiState::default();
        clamp_ui_state(&mut ui, &snapshot, compute_layout(rows, cols));

        loop {
            let mut ui_dirty = false;
            let n = unsafe {
                libc::read(
                    libc::STDIN_FILENO,
                    stdin_buf.as_mut_ptr().cast(),
                    stdin_buf.len(),
                )
            };
            if n > 0 {
                let events = parser.feed(&stdin_buf[..n as usize]);
                for event in events {
                    if matches!(event, InputEvent::Quit) {
                        stop.store(true, Ordering::Relaxed);
                        let _ = sampler.join();
                        return Ok(());
                    }
                    ui_dirty |= apply_input_event(&mut ui, &snapshot, rows, cols, event);
                }
            }

            let now = Instant::now();
            if let Some(wait) = pacer.wait_duration(now) {
                thread::sleep(wait.min(Duration::from_millis(5)));
                continue;
            }

            let (new_rows, new_cols) = get_term_size();
            let resized = new_rows != rows || new_cols != cols;
            if resized {
                rows = new_rows;
                cols = new_cols;
                surface.resize(rows, cols);
                clamp_ui_state(&mut ui, &snapshot, compute_layout(rows, cols));
            }
            if let Some(updated) = shared.load(&mut last_revision) {
                snapshot = updated;
                clamp_ui_state(&mut ui, &snapshot, compute_layout(rows, cols));
            } else if !resized && !ui_dirty {
                continue;
            }

            let present_started = Instant::now();
            let frame = surface.render(|dom| render_dashboard(dom, &snapshot, &ui, rows, cols));

            out.clear();
            if frame.title() != current_title {
                current_title.clear();
                current_title.push_str(frame.title());
                out.extend_from_slice(b"\x1b]0;");
                out.extend_from_slice(frame.title().as_bytes());
                out.push(b'\x07');
            }
            ansi.render(frame, &mut out);
            stdout.write_all(&out)?;
            stdout.flush()?;
            let present_finished = Instant::now();
            pacer.record_present(present_started, present_finished);
        }
    }

    fn render_dashboard(
        dom: &mut Dom,
        snapshot: &DashboardSnapshot,
        ui: &UiState,
        rows: u16,
        cols: u16,
    ) {
        let page = style(rgb(226, 232, 240), rgb(8, 15, 26));
        dom.set_background(page);
        dom.set_title(snapshot.title.clone());

        if rows < 14 || cols < 72 {
            dom.fill(Rect::new(0, 0, rows, cols), ' ', page);
            dom.text(1, 2, "blit-netdash ░ netflow", accent());
            let body = format!(
                "{}\n{}\n{}\n\nResize larger for the full dashboard.\nq quit | tab panels | enter filters peer",
                snapshot.summary,
                snapshot.detail,
                status_line(ui, snapshot),
            );
            dom.wrapped_text(
                Rect::new(3, 2, rows.saturating_sub(4), cols.saturating_sub(4)),
                body,
                muted(),
            );
            return;
        }

        let layout = compute_layout(rows, cols).expect("layout checked above");
        let peers = sorted_peers(snapshot, ui);
        let connections = sorted_connections(snapshot, ui);

        dom.fill(Rect::new(0, 0, rows, cols), ' ', page);
        dom.fill(Rect::new(0, 0, layout.banner_rows, cols), ' ', banner());
        dom.text(0, 2, "blit-netdash", banner_title());
        dom.text(
            1,
            2,
            truncate_end(&snapshot.summary, cols.saturating_sub(4) as usize),
            banner_text(),
        );
        dom.text(
            2,
            2,
            truncate_end(&snapshot.detail, cols.saturating_sub(4) as usize),
            if snapshot.error.is_some() {
                warning()
            } else {
                banner_text()
            },
        );
        dom.text(
            3,
            2,
            truncate_end(&status_line(ui, snapshot), cols.saturating_sub(4) as usize),
            status_style(ui),
        );
        let controls = format!(
            "{} | sort peer:{} conn:{} | {}",
            "tab/1/2/3 focus  arrows or j/k move  pgup/pgdn scroll  enter filter  c clear  s cycle sort  ? help  q quit",
            ui.peer_sort.label(),
            ui.connection_sort.label(),
            ui.peer_filter
                .map(|ip| format!("filter {ip}"))
                .unwrap_or_else(|| "no filter".into()),
        );
        dom.text(
            4,
            2,
            truncate_end(&controls, cols.saturating_sub(4) as usize),
            muted(),
        );

        render_peer_panel(dom, layout.peers_rect, ui, &peers);
        render_connection_panel(dom, layout.connections_rect, ui, &connections);
        render_event_panel(dom, layout.events_rect, ui, snapshot);

        if ui.show_help {
            render_help_overlay(dom, rows, cols);
        }
    }

    fn render_peer_panel(dom: &mut Dom, rect: Rect, ui: &UiState, peers: &[&PeerRow]) {
        let title = format!("◉ REMOTE IPS [{}]", ui.peer_sort.label());
        render_panel_chrome(dom, rect, &title, ui.focus == Panel::Peers);
        if rect.rows < 3 || rect.cols < 24 {
            return;
        }
        let header_row = rect.row + 1;
        dom.text(header_row, rect.col + 2, "peer", muted());
        let right_header = "conns   pps    bytes  load trend";
        let right_header_col = rect.col
            + rect
                .cols
                .saturating_sub(text_cells(right_header) as u16 + 1);
        dom.text(header_row, right_header_col, right_header, muted());

        let chart_w = rect.cols.saturating_sub(30).clamp(12, 20) as usize;
        let meter_w = chart_w.clamp(4, 6).min(chart_w.saturating_sub(6));
        let trend_w = chart_w.saturating_sub(meter_w + 1).max(6);
        let max_packets = peers
            .iter()
            .map(|peer| peer.packets_per_sec)
            .fold(0.0, f64::max)
            .max(1.0);
        let visible = panel_body_rows(rect);
        for (local_idx, peer) in peers.iter().skip(ui.peer_scroll).take(visible).enumerate() {
            let idx = ui.peer_scroll + local_idx;
            let row = rect.row + 2 + local_idx as u16;
            let selected = idx == ui.peer_selected;
            let style = row_style(ui.focus == Panel::Peers, selected, peer.packets_per_sec);
            if selected {
                dom.fill(
                    Rect::new(row, rect.col + 1, 1, rect.cols.saturating_sub(2)),
                    ' ',
                    row_fill_style(ui.focus == Panel::Peers),
                );
                dom.text(
                    row,
                    rect.col + 1,
                    "▸",
                    selected_marker(ui.focus == Panel::Peers),
                );
            }
            let load = meter_bar(peer.packets_per_sec, max_packets, meter_w);
            let spark = sparkline(&peer.history, trend_w);
            let tail = format!(
                "{:>4} {:>6} {:>7} {} {}",
                peer.connections,
                fmt_packets(peer.packets_per_sec),
                fmt_bytes(peer.bytes_per_sec),
                load,
                spark,
            );
            let tail_col = rect.col + rect.cols.saturating_sub(text_cells(&tail) as u16 + 1);
            let label_col = rect.col + 3;
            let label_w = tail_col.saturating_sub(label_col + 1) as usize;
            let label = truncate_end(&peer.label, label_w);
            dom.text(row, rect.col + 3, &label, style);
            dom.text(row, tail_col, tail, style);
        }
    }

    fn render_connection_panel(
        dom: &mut Dom,
        rect: Rect,
        ui: &UiState,
        connections: &[&ConnectionRow],
    ) {
        let title = if let Some(ip) = ui.peer_filter {
            format!("⇄ HOT CONNECTIONS [{} | {ip}]", ui.connection_sort.label())
        } else {
            format!("⇄ HOT CONNECTIONS [{}]", ui.connection_sort.label())
        };
        render_panel_chrome(dom, rect, &title, ui.focus == Panel::Connections);
        if rect.rows < 3 || rect.cols < 32 {
            return;
        }
        let header_row = rect.row + 1;
        dom.text(header_row, rect.col + 2, "st", muted());
        let right_header = "flow               pps   bytes  rtt    q  load trend";
        let right_header_col = rect.col
            + rect
                .cols
                .saturating_sub(text_cells(right_header) as u16 + 1);
        dom.text(header_row, right_header_col, right_header, muted());

        let chart_w = rect.cols.saturating_sub(42).clamp(12, 20) as usize;
        let meter_w = chart_w.clamp(4, 6).min(chart_w.saturating_sub(6));
        let trend_w = chart_w.saturating_sub(meter_w + 1).max(6);
        let max_packets = connections
            .iter()
            .map(|conn| conn.packets_per_sec)
            .fold(0.0, f64::max)
            .max(1.0);
        let visible = panel_body_rows(rect);
        for (local_idx, conn) in connections
            .iter()
            .skip(ui.connection_scroll)
            .take(visible)
            .enumerate()
        {
            let idx = ui.connection_scroll + local_idx;
            let row = rect.row + 2 + local_idx as u16;
            let selected = idx == ui.connection_selected;
            let style = row_style(
                ui.focus == Panel::Connections,
                selected,
                conn.packets_per_sec,
            );
            if selected {
                dom.fill(
                    Rect::new(row, rect.col + 1, 1, rect.cols.saturating_sub(2)),
                    ' ',
                    row_fill_style(ui.focus == Panel::Connections),
                );
                dom.text(
                    row,
                    rect.col + 1,
                    "▸",
                    selected_marker(ui.focus == Panel::Connections),
                );
            }
            let load = meter_bar(conn.packets_per_sec, max_packets, meter_w);
            let spark = sparkline(&conn.history, trend_w);
            let tail = format!(
                "{:>6} {:>7} {:>4} {:>4} {} {}",
                fmt_packets(conn.packets_per_sec),
                fmt_bytes(conn.bytes_per_sec),
                conn.rtt_ms,
                fmt_queue(conn.queue_bytes),
                load,
                spark,
            );
            let tail_col = rect.col + rect.cols.saturating_sub(text_cells(&tail) as u16 + 1);
            let label_col = rect.col + 8;
            let label_w = tail_col.saturating_sub(label_col + 1) as usize;
            let label = truncate_end(&conn.label, label_w);
            dom.text(row, rect.col + 3, conn.state, style);
            dom.text(row, rect.col + 8, &label, style);
            dom.text(row, tail_col, tail, style);
        }
    }

    fn render_event_panel(dom: &mut Dom, rect: Rect, ui: &UiState, snapshot: &DashboardSnapshot) {
        let title = format!("◆ EVENTS [offset {}]", ui.event_offset);
        render_panel_chrome(dom, rect, &title, ui.focus == Panel::Events);
        if rect.rows < 2 || rect.cols < 16 {
            return;
        }
        let inner = Rect::new(
            rect.row + 1,
            rect.col + 1,
            rect.rows.saturating_sub(1),
            rect.cols.saturating_sub(2),
        );
        let lines = snapshot
            .events
            .iter()
            .map(|line| truncate_end(line, inner.cols as usize))
            .collect::<Vec<_>>();
        dom.scrolling_text(inner, lines, ui.event_offset, muted());
    }

    fn render_panel_chrome(dom: &mut Dom, rect: Rect, title: &str, active: bool) {
        if rect.rows == 0 || rect.cols < 3 {
            return;
        }
        dom.fill(rect, ' ', panel_bg());
        if rect.rows > 1 && rect.cols > 2 {
            dom.fill(
                Rect::new(
                    rect.row + 1,
                    rect.col + 1,
                    rect.rows.saturating_sub(1),
                    rect.cols - 2,
                ),
                ' ',
                panel_bg(),
            );
        }
        dom.fill(
            Rect::new(rect.row, rect.col, 1, rect.cols),
            ' ',
            if active {
                panel_header_active()
            } else {
                panel_header()
            },
        );
        let border_style = if active {
            panel_border_active()
        } else {
            panel_border()
        };
        dom.text(rect.row, rect.col, "╭", border_style);
        if rect.cols > 2 {
            dom.text(
                rect.row,
                rect.col + 1,
                "─".repeat(rect.cols.saturating_sub(2) as usize),
                if active {
                    panel_header_active()
                } else {
                    panel_header()
                },
            );
        }
        dom.text(
            rect.row,
            rect.col + rect.cols.saturating_sub(1),
            "╮",
            border_style,
        );
        for row in rect.row.saturating_add(1)..rect.row.saturating_add(rect.rows) {
            dom.text(row, rect.col, "│", border_style);
            dom.text(
                row,
                rect.col + rect.cols.saturating_sub(1),
                "│",
                border_style,
            );
        }
        dom.text(
            rect.row,
            rect.col + 2,
            truncate_end(title, rect.cols.saturating_sub(4) as usize),
            if active {
                panel_title_active()
            } else {
                panel_title()
            },
        );
    }

    fn render_help_overlay(dom: &mut Dom, rows: u16, cols: u16) {
        let box_rows = rows.min(14).saturating_sub(2);
        let box_cols = cols.min(92).saturating_sub(4);
        let box_row = rows.saturating_sub(box_rows) / 2;
        let box_col = cols.saturating_sub(box_cols) / 2;
        let rect = Rect::new(box_row, box_col, box_rows, box_cols);
        dom.fill(rect, ' ', help_bg());
        dom.fill(
            Rect::new(rect.row, rect.col, 1, rect.cols),
            ' ',
            help_header(),
        );
        dom.text(rect.row, rect.col + 2, "INTERACTION", help_title());
        dom.wrapped_text(
            Rect::new(
                rect.row + 2,
                rect.col + 2,
                rect.rows.saturating_sub(3),
                rect.cols.saturating_sub(4),
            ),
            "Keyboard\n\
             tab or 1/2/3 switches panels\n\
             arrows or j/k move selection\n\
             pgup/pgdn scroll the active panel\n\
             home/end jump to top or tail\n\
             enter toggles the selected peer as a connection filter\n\
             s cycles sort for peers or connections\n\
             c clears the active peer filter\n\
             ? closes this help\n\
             q quits\n\n\
             Mouse\n\
             left click focuses a panel and selects a row\n\
             mouse wheel scrolls the panel under the pointer",
            help_text(),
        );
    }

    fn row_style(active_panel: bool, selected: bool, activity: f64) -> CellStyle {
        if selected {
            return if active_panel {
                selected_row_text()
            } else {
                selected_row_text_dim()
            };
        }
        if activity >= 1_000.0 {
            style(rgb(248, 250, 252), rgb(14, 28, 43))
        } else if activity >= 100.0 {
            style(rgb(191, 219, 254), rgb(14, 28, 43))
        } else {
            style(rgb(203, 213, 225), rgb(14, 28, 43))
        }
    }

    fn row_fill_style(active_panel: bool) -> CellStyle {
        if active_panel {
            style(rgb(248, 250, 252), rgb(24, 73, 109))
        } else {
            style(rgb(226, 232, 240), rgb(28, 47, 65))
        }
    }

    fn selected_marker(active_panel: bool) -> CellStyle {
        let mut style = if active_panel {
            style(rgb(125, 211, 252), rgb(24, 73, 109))
        } else {
            style(rgb(148, 163, 184), rgb(28, 47, 65))
        };
        style.bold = true;
        style
    }

    fn banner() -> CellStyle {
        style(rgb(245, 248, 255), rgb(19, 52, 86))
    }

    fn banner_title() -> CellStyle {
        let mut style = banner();
        style.bold = true;
        style
    }

    fn banner_text() -> CellStyle {
        style(rgb(226, 232, 240), rgb(19, 52, 86))
    }

    fn panel_bg() -> CellStyle {
        style(rgb(226, 232, 240), rgb(14, 28, 43))
    }

    fn panel_header() -> CellStyle {
        style(rgb(248, 250, 252), rgb(20, 44, 66))
    }

    fn panel_header_active() -> CellStyle {
        style(rgb(248, 250, 252), rgb(26, 93, 142))
    }

    fn panel_border() -> CellStyle {
        style(rgb(101, 163, 209), rgb(14, 28, 43))
    }

    fn panel_border_active() -> CellStyle {
        style(rgb(186, 230, 253), rgb(14, 28, 43))
    }

    fn panel_title() -> CellStyle {
        let mut style = panel_header();
        style.bold = true;
        style
    }

    fn panel_title_active() -> CellStyle {
        let mut style = panel_header_active();
        style.bold = true;
        style
    }

    fn accent() -> CellStyle {
        let mut style = style(rgb(125, 211, 252), rgb(8, 15, 26));
        style.bold = true;
        style
    }

    fn muted() -> CellStyle {
        style(rgb(148, 163, 184), rgb(14, 28, 43))
    }

    fn warning() -> CellStyle {
        style(rgb(254, 240, 138), rgb(19, 52, 86))
    }

    fn status_style(ui: &UiState) -> CellStyle {
        let mut style = style(rgb(191, 219, 254), rgb(19, 52, 86));
        if ui.peer_filter.is_some() {
            style.bold = true;
        }
        style
    }

    fn selected_row_text() -> CellStyle {
        let mut style = style(rgb(248, 250, 252), rgb(24, 73, 109));
        style.bold = true;
        style
    }

    fn selected_row_text_dim() -> CellStyle {
        let mut style = style(rgb(226, 232, 240), rgb(28, 47, 65));
        style.bold = true;
        style
    }

    fn help_bg() -> CellStyle {
        style(rgb(226, 232, 240), rgb(11, 26, 39))
    }

    fn help_header() -> CellStyle {
        style(rgb(248, 250, 252), rgb(32, 88, 128))
    }

    fn help_title() -> CellStyle {
        let mut style = help_header();
        style.bold = true;
        style
    }

    fn help_text() -> CellStyle {
        style(rgb(226, 232, 240), rgb(11, 26, 39))
    }

    fn rgb(r: u8, g: u8, b: u8) -> Color {
        Color::Rgb(r, g, b)
    }

    fn style(fg: Color, bg: Color) -> CellStyle {
        CellStyle {
            fg,
            bg,
            ..CellStyle::default()
        }
    }

    fn push_history(history: &mut VecDeque<u32>, value: f64) {
        if history.len() == HISTORY_LEN {
            history.pop_front();
        }
        history.push_back(value.round().clamp(0.0, u32::MAX as f64) as u32);
    }

    fn sparkline(history: &[u32], width: usize) -> String {
        const LEVELS: [char; 8] = ['▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];
        if width == 0 {
            return String::new();
        }
        let mut values = vec![0u32; width];
        let take = history.len().min(width);
        if take > 0 {
            values[width - take..].copy_from_slice(&history[history.len() - take..]);
        }
        let max = values.iter().copied().max().unwrap_or(0);
        if max == 0 {
            return " ".repeat(width);
        }
        values
            .into_iter()
            .map(|value| {
                if value == 0 {
                    ' '
                } else {
                    let idx = ((value as u64 * (LEVELS.len() - 1) as u64) / max as u64) as usize;
                    LEVELS[idx]
                }
            })
            .collect()
    }

    fn meter_bar(value: f64, max: f64, width: usize) -> String {
        const PARTIALS: [char; 8] = [' ', '▏', '▎', '▍', '▌', '▋', '▊', '▉'];
        if width == 0 {
            return String::new();
        }
        if value <= 0.0 || max <= 0.0 {
            return " ".repeat(width);
        }
        let total_eighths = ((value / max).clamp(0.0, 1.0) * (width * 8) as f64).round() as usize;
        let full = (total_eighths / 8).min(width);
        let partial = total_eighths % 8;
        let mut out = String::with_capacity(width);
        for _ in 0..full {
            out.push('█');
        }
        if full < width {
            out.push(PARTIALS[partial]);
        }
        while out.chars().count() < width {
            out.push(' ');
        }
        out
    }

    fn fmt_packets(value: f64) -> String {
        fmt_scaled(value, "")
    }

    fn fmt_bytes(value: f64) -> String {
        fmt_scaled(value, "B")
    }

    fn fmt_scaled(value: f64, suffix: &str) -> String {
        let abs = value.abs();
        if abs >= 1_000_000_000.0 {
            format!("{:.1}G{}", value / 1_000_000_000.0, suffix)
        } else if abs >= 1_000_000.0 {
            format!("{:.1}M{}", value / 1_000_000.0, suffix)
        } else if abs >= 1_000.0 {
            format!("{:.1}k{}", value / 1_000.0, suffix)
        } else {
            format!("{:.0}{}", value, suffix)
        }
    }

    fn fmt_count(value: u64) -> String {
        if value >= 1_000_000_000 {
            format!("{:.1}G", value as f64 / 1_000_000_000.0)
        } else if value >= 1_000_000 {
            format!("{:.1}M", value as f64 / 1_000_000.0)
        } else if value >= 1_000 {
            format!("{:.1}k", value as f64 / 1_000.0)
        } else {
            value.to_string()
        }
    }

    fn fmt_queue(bytes: u32) -> String {
        if bytes >= 1_000_000 {
            format!("{:.1}M", bytes as f64 / 1_000_000.0)
        } else if bytes >= 1_000 {
            format!("{:.0}k", bytes as f64 / 1_000.0)
        } else {
            bytes.to_string()
        }
    }

    fn fmt_duration(duration: Duration) -> String {
        if duration.as_secs() >= 3600 {
            format!("{:.1}h", duration.as_secs_f64() / 3600.0)
        } else if duration.as_secs() >= 60 {
            format!("{:.1}m", duration.as_secs_f64() / 60.0)
        } else {
            format!("{:.1}s", duration.as_secs_f64())
        }
    }

    fn truncate_end(text: &str, width: usize) -> String {
        if width == 0 {
            return String::new();
        }
        let count = text.chars().count();
        if count <= width {
            return format!("{text:<width$}");
        }
        if width <= 1 {
            return ".".repeat(width);
        }
        let mut out = String::with_capacity(width);
        for ch in text.chars().take(width - 1) {
            out.push(ch);
        }
        out.push('~');
        out
    }

    fn text_cells(text: &str) -> usize {
        text.chars().count()
    }

    fn format_connection_label(conn: &TrackedConnection) -> String {
        format!(
            "{}:{} -> {}:{}",
            conn.local_ip, conn.local_port, conn.remote_ip, conn.remote_port
        )
    }

    fn short_connection_label(
        local_ip: IpAddr,
        local_port: u16,
        remote_ip: IpAddr,
        remote_port: u16,
    ) -> String {
        format!("{local_ip}:{local_port} -> {remote_ip}:{remote_port}")
    }

    fn is_unspecified_ip(ip: &IpAddr) -> bool {
        match ip {
            IpAddr::V4(ip) => ip.is_unspecified(),
            IpAddr::V6(ip) => ip.is_unspecified(),
        }
    }

    fn tcp_state_name(state: u8) -> &'static str {
        match state {
            TCP_ESTABLISHED => "ESTAB",
            TCP_SYN_SENT => "SYN-S",
            TCP_SYN_RECV => "SYN-R",
            TCP_FIN_WAIT1 => "FIN1",
            TCP_FIN_WAIT2 => "FIN2",
            TCP_TIME_WAIT => "TIME",
            TCP_CLOSE => "CLOSE",
            TCP_CLOSE_WAIT => "CLSW",
            TCP_LAST_ACK => "LACK",
            TCP_LISTEN => "LISTN",
            TCP_CLOSING => "CLSG",
            _ => "OTHER",
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

    fn push_u16(out: &mut Vec<u8>, value: u16) {
        out.extend_from_slice(value.to_string().as_bytes());
    }
}

#[cfg(target_os = "linux")]
fn main() -> std::io::Result<()> {
    linux::main()
}
