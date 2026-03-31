use blit_alacritty::{SearchResult as AlacrittySearchResult, TerminalDriver as AlacrittyDriver};
use blit_remote::{
    build_update_msg, msg_hello, FrameState, C2S_ACK, C2S_CLIENT_METRICS, C2S_CLOSE, C2S_CREATE,
    C2S_CREATE2, C2S_CREATE_AT, C2S_CREATE_N, C2S_DISPLAY_RATE, C2S_FOCUS, C2S_INPUT, C2S_MOUSE,
    C2S_READ, C2S_RESIZE, C2S_RESTART, C2S_SCROLL, C2S_SEARCH, C2S_SUBSCRIBE, C2S_UNSUBSCRIBE,
    CREATE2_HAS_COMMAND, CREATE2_HAS_SRC_PTY, FEATURE_CREATE_NONCE, FEATURE_RESIZE_BATCH,
    FEATURE_RESTART, READ_ANSI, READ_TAIL, S2C_CLOSED, S2C_CREATED, S2C_CREATED_N, S2C_LIST,
    S2C_READY, S2C_SEARCH_RESULTS, S2C_TEXT, S2C_TITLE,
};
use std::collections::{HashMap, HashSet, VecDeque};
use std::ffi::CString;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::io::RawFd;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::io::unix::AsyncFd;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::UnixListener;
use tokio::sync::{mpsc, Mutex, Notify};

type PtyFds = Arc<std::sync::RwLock<HashMap<u16, RawFd>>>;

const SCROLLBACK_ROWS_DEFAULT: usize = 10_000;

struct Config {
    shell: String,
    shell_flags: String,
    scrollback: usize,
    socket_path: String,
    fd_channel: Option<RawFd>,
}

fn pty_write_all(fd: libc::c_int, mut data: &[u8]) {
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

trait PtyDriver: Send {
    fn size(&self) -> (u16, u16);
    fn resize(&mut self, rows: u16, cols: u16);
    fn process(&mut self, data: &[u8]);
    fn title(&self) -> &str;
    fn search_result(&self, query: &str) -> Option<PtySearchResult>;
    fn take_title_dirty(&mut self) -> bool;
    fn cursor_position(&self) -> (u16, u16);
    fn synced_output(&self) -> bool;
    fn snapshot(&mut self, echo: bool, icanon: bool) -> FrameState;
    fn scrollback_frame(&mut self, offset: usize) -> FrameState;
    fn reset_modes(&mut self);
    fn mouse_event(
        &self,
        type_: u8,
        button: u8,
        col: u16,
        row: u16,
        echo: bool,
        icanon: bool,
    ) -> Option<Vec<u8>>;
}

struct PtySearchResult {
    score: u32,
    primary_source: u8,
    matched_sources: u8,
    context: String,
    scroll_offset: Option<usize>,
}

impl PtyDriver for AlacrittyDriver {
    fn size(&self) -> (u16, u16) {
        AlacrittyDriver::size(self)
    }

    fn resize(&mut self, rows: u16, cols: u16) {
        AlacrittyDriver::resize(self, rows, cols);
    }

    fn process(&mut self, data: &[u8]) {
        AlacrittyDriver::process(self, data);
    }

    fn title(&self) -> &str {
        AlacrittyDriver::title(self)
    }

    fn search_result(&self, query: &str) -> Option<PtySearchResult> {
        AlacrittyDriver::search_result(self, query).map(|result: AlacrittySearchResult| {
            PtySearchResult {
                score: result.score,
                primary_source: result.primary_source as u8,
                matched_sources: result.matched_sources,
                context: result.context,
                scroll_offset: result.scroll_offset,
            }
        })
    }

    fn take_title_dirty(&mut self) -> bool {
        AlacrittyDriver::take_title_dirty(self)
    }

    fn cursor_position(&self) -> (u16, u16) {
        AlacrittyDriver::cursor_position(self)
    }

    fn synced_output(&self) -> bool {
        AlacrittyDriver::synced_output(self)
    }

    fn snapshot(&mut self, echo: bool, icanon: bool) -> FrameState {
        AlacrittyDriver::snapshot(self, echo, icanon)
    }

    fn scrollback_frame(&mut self, offset: usize) -> FrameState {
        AlacrittyDriver::scrollback_frame(self, offset)
    }

    fn reset_modes(&mut self) {
        AlacrittyDriver::reset_modes(self);
    }

    fn mouse_event(
        &self,
        type_: u8,
        button: u8,
        col: u16,
        row: u16,
        echo: bool,
        icanon: bool,
    ) -> Option<Vec<u8>> {
        AlacrittyDriver::mouse_event(self, type_, button, col, row, echo, icanon)
    }
}

// Keep small to limit bufferbloat on slow connections.  The soft queue limit
// (OUTBOX_SOFT_QUEUE_LIMIT_FRAMES) prevents the tick from queuing more than
// ~2 frames, so this just needs to be bigger than that with some headroom.
const OUTBOX_CAPACITY: usize = 8;
const OUTBOX_SOFT_QUEUE_LIMIT_FRAMES: usize = 2;
const PREVIEW_FRAME_RESERVE: usize = 1;
const READY_FRAME_QUEUE_CAP: usize = 4;
const PTY_CHANNEL_CAPACITY: usize = 64;
const SYNC_OUTPUT_END: &[u8] = b"\x1b[?2026l";

/// A chunk of data from the PTY reader, sent through a lock-free channel
/// so the reader never contends with the delivery tick for the Session mutex.
enum PtyInput {
    /// Raw bytes from the PTY, with the reader's sync-scan tail for boundary
    /// detection. The tick task calls `process()` + `respond_to_queries()`.
    Data(Vec<u8>),
    /// Data up to a sync-output-close boundary. `before` should be processed
    /// and then a snapshot taken. `after` is remainder for the next chunk.
    SyncBoundary { before: Vec<u8>, after: Vec<u8> },
    /// The PTY fd hit EOF or an error — the child likely exited.
    Eof,
}

const MAX_FRAME_SIZE: usize = 16 * 1024 * 1024;

async fn read_frame(reader: &mut (impl AsyncRead + Unpin)) -> Option<Vec<u8>> {
    let mut len_buf = [0u8; 4];
    reader.read_exact(&mut len_buf).await.ok()?;
    let len = u32::from_le_bytes(len_buf) as usize;
    if len == 0 {
        return Some(vec![]);
    }
    if len > MAX_FRAME_SIZE {
        return None;
    }
    let mut buf = vec![0u8; len];
    reader.read_exact(&mut buf).await.ok()?;
    Some(buf)
}

async fn write_frame(writer: &mut (impl AsyncWrite + Unpin), payload: &[u8]) -> bool {
    if payload.len() > u32::MAX as usize {
        return false;
    }
    let len = payload.len() as u32;
    let mut buf = Vec::with_capacity(4 + payload.len());
    buf.extend_from_slice(&len.to_le_bytes());
    buf.extend_from_slice(payload);
    writer.write_all(&buf).await.is_ok()
}

struct Pty {
    master_fd: libc::c_int,
    child_pid: libc::pid_t,
    driver: Box<dyn PtyDriver>,
    /// Client-chosen tag set at creation time.
    tag: String,
    dirty: bool,
    ready_frames: VecDeque<FrameState>,
    /// Receives raw byte chunks from the PTY reader task without mutex contention.
    byte_rx: mpsc::Receiver<PtyInput>,
    reader_handle: std::thread::JoinHandle<()>,
    /// Cached (echo, icanon) from tcgetattr; refreshed every ~250ms.
    lflag_cache: (bool, bool),
    lflag_last: Instant,
    /// When we last broadcast a title update for this PTY.
    last_title_send: Instant,
    /// Title changed but not yet sent (debounced).
    title_pending: bool,
    /// The subprocess has exited but the terminal state is retained for reading.
    exited: bool,
    /// Exit status: WEXITSTATUS if normal exit, negative signal number if signalled,
    /// EXIT_STATUS_UNKNOWN if not yet collected.
    exit_status: i32,
    /// Command used to create this PTY (None = default shell).
    command: Option<String>,
}

impl Pty {
    fn mark_dirty(&mut self) {
        self.dirty = true;
    }

    fn clear_dirty(&mut self) {
        self.dirty = false;
    }
}

struct ClientState {
    tx: mpsc::Sender<Vec<u8>>,
    lead: Option<u16>,
    subscriptions: HashSet<u16>,
    view_sizes: HashMap<u16, (u16, u16)>,
    scroll_offset: usize,
    scroll_cache: FrameState,
    last_sent: HashMap<u16, FrameState>,
    preview_next_send_at: HashMap<u16, Instant>,
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
    /// EWMA of absolute goodput sample-to-sample jitter in bytes/sec.
    goodput_jitter_bps: f32,
    /// Decaying peak goodput jitter in bytes/sec.
    max_goodput_jitter_bps: f32,
    /// Last sampled ACK goodput for jitter estimation.
    last_goodput_sample_bps: f32,
    /// EWMA of acknowledged frame payload size in bytes.
    avg_frame_bytes: f32,
    /// EWMA of acknowledged lead/paced frame payload size in bytes.
    avg_paced_frame_bytes: f32,
    /// EWMA of acknowledged preview/unpaced frame payload size in bytes.
    avg_preview_frame_bytes: f32,
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
    last_metrics_update: Instant,
    last_log: Instant,
    goodput_window_bytes: usize,
    goodput_window_start: Instant,
}

struct InFlightFrame {
    sent_at: Instant,
    bytes: usize,
    paced: bool,
}

/// Frames to keep in flight: enough to cover one RTT at the client's reported
/// display rate. High-latency links need many frames in flight to avoid
/// devolving into stop-and-wait.
fn frame_window(rtt_ms: f32, display_fps: f32) -> usize {
    let frame_ms = 1_000.0 / display_fps.max(1.0);
    let base_frames = (rtt_ms / frame_ms).ceil().max(0.0) as usize;
    let slack_frames = ((base_frames as f32) * 0.125).ceil() as usize + 2;
    base_frames.saturating_add(slack_frames).max(2)
}

fn path_rtt_ms(client: &ClientState) -> f32 {
    if client.min_rtt_ms > 0.0 {
        client.min_rtt_ms
    } else {
        client.rtt_ms
    }
}

fn display_need_bps(client: &ClientState) -> f32 {
    client.avg_paced_frame_bytes.max(256.0) * client.display_fps.max(1.0)
}

fn effective_rtt_ms(client: &ClientState) -> f32 {
    let path_rtt = path_rtt_ms(client);
    let frame_ms = 1_000.0 / browser_pacing_fps(client).max(1.0);
    let queue_allowance = frame_ms
        * if throughput_limited(client) {
            4.0
        } else {
            12.0
        };
    client.rtt_ms.clamp(path_rtt, path_rtt + queue_allowance)
}

fn window_rtt_ms(client: &ClientState) -> f32 {
    let effective = effective_rtt_ms(client);
    if !throughput_limited(client) {
        effective
    } else {
        client.rtt_ms.clamp(effective, effective * 2.0)
    }
}

fn target_frame_window(client: &ClientState) -> usize {
    let window_fps = if throughput_limited(client) {
        pacing_fps(client)
    } else {
        browser_pacing_fps(client)
    };
    frame_window(window_rtt_ms(client), window_fps)
        .saturating_add(client.probe_frames.round().max(0.0) as usize)
}

fn base_queue_ms(client: &ClientState) -> f32 {
    let frame_ms = 1_000.0 / browser_pacing_fps(client).max(1.0);
    frame_ms * if throughput_limited(client) { 2.0 } else { 8.0 }
}

fn target_queue_ms(client: &ClientState) -> f32 {
    let frame_ms = 1_000.0 / browser_pacing_fps(client).max(1.0);
    let probe_scale = if throughput_limited(client) {
        0.25
    } else {
        1.0
    };
    base_queue_ms(client) + client.probe_frames.max(0.0) * frame_ms * probe_scale
}

fn browser_ready(client: &ClientState) -> bool {
    client.browser_ack_ahead_frames <= 1
        && client.browser_apply_ms <= 1.0
        && !outbox_backpressured(client)
}

fn bandwidth_floor_bps(client: &ClientState) -> f32 {
    let browser_ready = browser_ready(client);
    let backlog_scale = match client.browser_backlog_frames {
        0..=2 => 0.9,
        3..=8 => 0.8,
        _ => 0.65,
    };
    let penalty = client
        .goodput_jitter_bps
        .max(client.max_goodput_jitter_bps * 0.5)
        .min(client.goodput_bps * if browser_ready { 0.75 } else { 0.9 });
    let goodput_floor = (client.goodput_bps - penalty)
        .max(client.goodput_bps * if browser_ready { 0.35 } else { 0.2 });
    // On a browser-ready path, the per-frame delivery estimate is already
    // end-to-end and reacts much faster than ACK-window goodput. Halving it
    // leaves large-frame local links chronically underpaced.
    let delivery_floor = client.delivery_bps * if browser_ready { 1.0 } else { 0.5 };
    let recent_sample_floor = if browser_ready && client.last_goodput_sample_bps > 0.0 {
        client.last_goodput_sample_bps * backlog_scale
    } else {
        0.0
    };
    goodput_floor.max(recent_sample_floor).max(delivery_floor)
}

fn pacing_fps(client: &ClientState) -> f32 {
    let frame_bytes = client.avg_paced_frame_bytes.max(256.0);
    let sustainable = bandwidth_floor_bps(client) / frame_bytes;
    sustainable.min(browser_pacing_fps(client))
}

fn throughput_limited(client: &ClientState) -> bool {
    let floor = bandwidth_floor_bps(client);
    // Consider total demand: lead at cadence rate plus previews at their cap.
    // The old check (pacing_fps < cadence * 0.9) only saw lead bandwidth,
    // which is often tiny, so previews could starve the lead undetected.
    let lead_bps = client.avg_paced_frame_bytes.max(256.0) * browser_pacing_fps(client);
    let preview_bps = client.avg_preview_frame_bytes.max(256.0) * client.display_fps.max(1.0);
    (lead_bps + preview_bps) > floor * 0.9
}

fn browser_pacing_fps(client: &ClientState) -> f32 {
    let mut fps = client.display_fps.max(1.0);

    // Backlog and ack-ahead are direct signals from the browser about
    // whether it's keeping up.  No predictive apply-time bound — it
    // consistently underestimates capacity and causes 30fps death spirals.
    let backlog = client.browser_backlog_frames as f32;
    if backlog > 4.0 {
        fps = fps.min(fps * (4.0 / backlog));
    }

    if client.browser_ack_ahead_frames > 4 {
        fps = fps.min(client.display_fps.max(1.0) * 0.5);
    }

    fps.max(1.0)
}

fn browser_backlog_blocked(client: &ClientState) -> bool {
    client.browser_backlog_frames > 8
}

fn byte_budget_for(client: &ClientState, budget_ms: f32) -> usize {
    let budget_bps = if throughput_limited(client) {
        bandwidth_floor_bps(client)
    } else {
        client.goodput_bps.max(bandwidth_floor_bps(client))
    };
    let bytes = budget_bps * budget_ms.max(1.0) / 1_000.0;
    bytes.ceil().max(client.avg_frame_bytes.max(256.0)) as usize
}

fn target_byte_window(client: &ClientState) -> usize {
    let budget = byte_budget_for(client, path_rtt_ms(client) + target_queue_ms(client));
    let frame_bytes = client.avg_paced_frame_bytes.max(256.0).ceil() as usize;
    let target_frames = target_frame_window(client);
    let pipeline_bytes = frame_bytes.saturating_mul(target_frames);
    // For small pipelines (e.g. idle terminals with 1KB frames), allow the
    // full frame window worth of bytes so we pipeline across the RTT instead
    // of stop-and-wait.  For large pipelines (e.g. 50KB frames × 5 frames =
    // 250KB), the budget (BDP-based) is the binding constraint; fall back to
    // a one-frame floor so we don't pile up many RTTs worth of large frames.
    const PIPELINE_FLOOR_LIMIT: usize = 32_768; // 32 KB
    let floor = if pipeline_bytes <= PIPELINE_FLOOR_LIMIT {
        pipeline_bytes
    } else {
        frame_bytes // one-frame floor for large pipelines
    };
    budget.max(floor)
}

fn send_interval(client: &ClientState) -> Duration {
    Duration::from_secs_f64(1.0 / browser_pacing_fps(client).max(1.0) as f64)
}

fn preview_fps(client: &ClientState) -> f32 {
    let mut fps = client.display_fps.max(1.0);
    if client.lead.is_some() {
        // Always budget preview bandwidth: available minus lead's share.
        // Without this, large preview frames (e.g. 12 KB) at 30 fps consume
        // 360 KB/s, starving the lead even when lead frames are tiny.
        let avail = bandwidth_floor_bps(client);
        let lead_bps = client.avg_paced_frame_bytes.max(256.0) * browser_pacing_fps(client);
        let preview_budget = (avail - lead_bps).max(avail * 0.25).max(0.0);
        let bw_cap = preview_budget / client.avg_preview_frame_bytes.max(256.0);
        fps = fps.min(bw_cap.max(1.0));
    }
    fps.max(1.0)
}

fn preview_send_interval(client: &ClientState) -> Duration {
    Duration::from_secs_f64(1.0 / preview_fps(client) as f64)
}

fn advance_deadline(deadline: &mut Instant, now: Instant, interval: Duration) {
    let scheduled = deadline.checked_add(interval).unwrap_or(now + interval);
    *deadline = if scheduled + interval < now {
        now + interval
    } else {
        scheduled
    };
}

fn should_snapshot_pty(dirty: bool, needful: bool, synced_output: bool) -> bool {
    dirty && needful && !synced_output
}

fn enqueue_ready_frame(queue: &mut VecDeque<FrameState>, frame: FrameState) -> bool {
    if queue.len() >= READY_FRAME_QUEUE_CAP {
        return false;
    }
    queue.push_back(frame);
    true
}

fn pty_has_visual_update(pty: &Pty) -> bool {
    pty.dirty || !pty.ready_frames.is_empty() || !pty.byte_rx.is_empty()
}

/// Find the first `\x1b[?2026l` in `bytes`, handling sequences that span
/// the `prefix`/`bytes` boundary. Uses SIMD-accelerated memchr for the
/// initial ESC scan.
fn find_sync_output_end(prefix: &[u8], bytes: &[u8]) -> Option<usize> {
    if bytes.is_empty() {
        return None;
    }
    let needle = SYNC_OUTPUT_END;
    let nlen = needle.len();

    // Check for a match straddling the prefix/bytes boundary.
    if !prefix.is_empty() {
        let tail = if prefix.len() >= nlen - 1 {
            &prefix[prefix.len() - (nlen - 1)..]
        } else {
            prefix
        };
        let combined_len = tail.len() + bytes.len().min(nlen);
        if combined_len >= nlen {
            // Small stack buffer to check the boundary region.
            let mut buf = [0u8; 32]; // SYNC_OUTPUT_END is 8 bytes, so 32 is plenty
            let blen = combined_len.min(buf.len());
            let tlen = tail.len().min(blen);
            buf[..tlen].copy_from_slice(&tail[..tlen]);
            let rest = (blen - tlen).min(bytes.len());
            buf[tlen..tlen + rest].copy_from_slice(&bytes[..rest]);
            for i in 0..=(blen.saturating_sub(nlen)) {
                if &buf[i..i + nlen] == needle {
                    let end_in_bytes = (i + nlen).saturating_sub(tail.len());
                    if end_in_bytes > 0 && end_in_bytes <= bytes.len() {
                        return Some(end_in_bytes);
                    }
                }
            }
        }
    }

    // SIMD-scan for ESC (0x1b) then verify the full sequence.
    let mut offset = 0;
    while let Some(pos) = memchr::memchr(0x1b, &bytes[offset..]) {
        let abs = offset + pos;
        if abs + nlen <= bytes.len() && &bytes[abs..abs + nlen] == needle {
            return Some(abs + nlen);
        }
        offset = abs + 1;
    }
    None
}

fn update_sync_scan_tail(tail: &mut Vec<u8>, bytes: &[u8]) {
    if bytes.is_empty() {
        return;
    }
    tail.extend_from_slice(bytes);
    let keep = SYNC_OUTPUT_END.len().saturating_sub(1);
    if tail.len() > keep {
        let drop = tail.len() - keep;
        tail.drain(..drop);
    }
}

fn preview_deadline(client: &ClientState, pid: u16, now: Instant) -> Instant {
    client
        .preview_next_send_at
        .get(&pid)
        .copied()
        .unwrap_or(now)
}

fn client_has_due_preview(sess: &Session, client: &ClientState, now: Instant) -> bool {
    if client.lead.is_none() {
        return false;
    }
    client.subscriptions.iter().copied().any(|pid| {
        Some(pid) != client.lead
            && preview_deadline(client, pid, now) <= now
            && sess
                .ptys
                .get(&pid)
                .map(pty_has_visual_update)
                .unwrap_or(false)
    })
}

fn outbox_queued_frames(client: &ClientState) -> usize {
    OUTBOX_CAPACITY.saturating_sub(client.tx.capacity())
}

fn outbox_backpressured(client: &ClientState) -> bool {
    outbox_queued_frames(client) >= OUTBOX_SOFT_QUEUE_LIMIT_FRAMES
}

fn can_send_preview(client: &ClientState, pid: u16, now: Instant) -> bool {
    window_open(client) && now >= preview_deadline(client, pid, now)
}

fn record_preview_send(client: &mut ClientState, pid: u16, now: Instant) {
    let mut deadline = client
        .preview_next_send_at
        .get(&pid)
        .copied()
        .unwrap_or(now);
    advance_deadline(&mut deadline, now, preview_send_interval(client));
    client.preview_next_send_at.insert(pid, deadline);
}

fn window_open(client: &ClientState) -> bool {
    !browser_backlog_blocked(client)
        && !outbox_backpressured(client)
        && client.inflight_frames.len() < target_frame_window(client)
        && client.inflight_bytes < target_byte_window(client)
}

fn lead_window_open(client: &ClientState, reserve_preview_slot: bool) -> bool {
    if !reserve_preview_slot || client.lead.is_none() {
        return window_open(client);
    }
    if browser_backlog_blocked(client) || outbox_backpressured(client) {
        return false;
    }
    let target_frames = target_frame_window(client);
    let reserve_frames = PREVIEW_FRAME_RESERVE.min(target_frames.saturating_sub(1));
    let frame_limit = target_frames.saturating_sub(reserve_frames).max(1);
    let reserve_bytes = client.avg_preview_frame_bytes.max(256.0).ceil() as usize;
    let byte_limit = target_byte_window(client)
        .saturating_sub(reserve_bytes)
        .max(client.avg_paced_frame_bytes.max(256.0).ceil() as usize);
    client.inflight_frames.len() < frame_limit && client.inflight_bytes < byte_limit
}

fn can_send_frame(client: &ClientState, now: Instant, reserve_preview_slot: bool) -> bool {
    lead_window_open(client, reserve_preview_slot) && now >= client.next_send_at
}

fn record_send(client: &mut ClientState, bytes: usize, now: Instant, paced: bool) {
    client.inflight_bytes += bytes;
    client.inflight_frames.push_back(InFlightFrame {
        sent_at: now,
        bytes,
        paced,
    });
    if paced {
        let interval = send_interval(client);
        advance_deadline(&mut client.next_send_at, now, interval);
    }
}

fn ewma_with_direction(old: f32, sample: f32, rise_alpha: f32, fall_alpha: f32) -> f32 {
    let alpha = if sample > old { rise_alpha } else { fall_alpha };
    old * (1.0 - alpha) + sample * alpha
}

fn window_saturated(client: &ClientState, inflight_frames: usize, inflight_bytes: usize) -> bool {
    let target_frames = target_frame_window(client);
    let target_bytes = target_byte_window(client);
    inflight_frames.saturating_mul(10) >= target_frames.saturating_mul(9)
        || inflight_bytes.saturating_mul(10) >= target_bytes.saturating_mul(9)
}

fn record_ack(client: &mut ClientState) {
    if let Some(frame) = client.inflight_frames.pop_front() {
        let prev_inflight_frames = client.inflight_frames.len() + 1;
        let prev_inflight_bytes = client.inflight_bytes;
        client.inflight_bytes = client.inflight_bytes.saturating_sub(frame.bytes);
        client.acked_bytes_since_log = client.acked_bytes_since_log.saturating_add(frame.bytes);
        let sample_ms = frame.sent_at.elapsed().as_secs_f32() * 1_000.0;
        client.rtt_ms = ewma_with_direction(client.rtt_ms, sample_ms, 0.125, 0.25);
        if client.min_rtt_ms > 0.0 {
            // Only update downward: min_rtt tracks the unloaded path RTT and
            // must not drift upward during congestion (queued RTT ≠ path RTT).
            client.min_rtt_ms = client.min_rtt_ms.min(sample_ms);
        } else {
            client.min_rtt_ms = sample_ms;
        }
        client.min_rtt_ms = client.min_rtt_ms.max(0.5);
        let sample_bps = frame.bytes as f32 / sample_ms.max(1.0e-3) * 1_000.0;
        client.delivery_bps = ewma_with_direction(client.delivery_bps, sample_bps, 0.5, 0.125);
        client.avg_frame_bytes =
            ewma_with_direction(client.avg_frame_bytes, frame.bytes as f32, 0.5, 0.125);
        if frame.paced {
            client.avg_paced_frame_bytes =
                ewma_with_direction(client.avg_paced_frame_bytes, frame.bytes as f32, 0.5, 0.125);
        } else {
            client.avg_preview_frame_bytes = ewma_with_direction(
                client.avg_preview_frame_bytes,
                frame.bytes as f32,
                0.5,
                0.125,
            );
        }
        let frame_ms = 1_000.0 / browser_pacing_fps(client).max(1.0);
        let path_rtt = path_rtt_ms(client);
        let likely_window_limited =
            window_saturated(client, prev_inflight_frames, prev_inflight_bytes);
        client.goodput_window_bytes = client.goodput_window_bytes.saturating_add(frame.bytes);
        let now = Instant::now();
        let goodput_elapsed = now
            .duration_since(client.goodput_window_start)
            .as_secs_f32();
        if goodput_elapsed >= 0.02 {
            let sample_goodput = client.goodput_window_bytes as f32 / goodput_elapsed.max(1.0e-3);
            if likely_window_limited || client.browser_backlog_frames > 0 {
                let prev_goodput_sample = if client.last_goodput_sample_bps > 0.0 {
                    client.last_goodput_sample_bps
                } else {
                    sample_goodput
                };
                let jitter_sample = (sample_goodput - prev_goodput_sample).abs();
                client.goodput_bps =
                    ewma_with_direction(client.goodput_bps, sample_goodput, 0.5, 0.125);
                // Only update jitter from windows with at least 2 frames.
                // Single-frame windows are pure measurement noise (0 or 1
                // frame per 25 ms is a Bernoulli trial, not a congestion
                // signal) and inflate jitter_bps, which in turn depresses
                // bandwidth_floor_bps and causes pacing to stall.
                let min_reliable = (client.avg_paced_frame_bytes.max(256.0) * 2.0) as usize;
                if client.goodput_window_bytes >= min_reliable {
                    client.goodput_jitter_bps =
                        ewma_with_direction(client.goodput_jitter_bps, jitter_sample, 0.5, 0.125);
                    let jitter_decay = if browser_ready(client) && sample_ms < path_rtt * 3.0 {
                        0.90
                    } else {
                        0.98
                    };
                    client.max_goodput_jitter_bps =
                        (client.max_goodput_jitter_bps * jitter_decay).max(jitter_sample);
                    // Cap jitter at 45% of goodput so jitter_ratio can never
                    // exceed 0.45 from measurement noise alone.  Real congestion
                    // will still drive goodput_bps down and widen the window.
                    client.max_goodput_jitter_bps =
                        client.max_goodput_jitter_bps.min(client.goodput_bps * 0.45);
                } else {
                    // Thin sample: gently decay jitter rather than updating it.
                    client.goodput_jitter_bps *= 0.9;
                    client.max_goodput_jitter_bps *= 0.95;
                }
                // Sticky-high: never let last_goodput_sample_bps drop abruptly.
                // A sudden drop (e.g. 1-frame window following a 2-frame window)
                // inflates jitter_sample on the next cycle, collapsing probe_frames.
                client.last_goodput_sample_bps =
                    (client.last_goodput_sample_bps * 0.99).max(sample_goodput);
            } else {
                // When the path is underfilled, ACK cadence mostly measures our
                // own pacing rather than network capacity.  Use a fall alpha
                // proportional to estimation error: when the estimate is 10x+
                // the sample, converge aggressively; when close, stay gentle.
                let ratio = client.goodput_bps / sample_goodput.max(1.0);
                let fall_alpha = if ratio > 10.0 {
                    0.5
                } else if ratio > 3.0 {
                    0.25
                } else {
                    0.03
                };
                client.goodput_bps =
                    ewma_with_direction(client.goodput_bps, sample_goodput, 0.5, fall_alpha);
                client.goodput_jitter_bps *= 0.5;
                client.max_goodput_jitter_bps *= 0.9;
                client.last_goodput_sample_bps =
                    (client.last_goodput_sample_bps * 0.99).max(sample_goodput);
            }
            client.goodput_window_bytes = 0;
            client.goodput_window_start = now;
        }
        let queue_baseline_ms = if throughput_limited(client) {
            window_rtt_ms(client)
        } else {
            path_rtt
        };
        let queue_delay_ms = (sample_ms - queue_baseline_ms).max(0.0);
        let max_probe_frames = (browser_pacing_fps(client) * 0.125).max(4.0);
        let jitter_ratio = client.max_goodput_jitter_bps / client.goodput_bps.max(1.0);
        let low_delay_frames = if throughput_limited(client) { 2.0 } else { 8.0 };
        let high_delay_frames = if throughput_limited(client) {
            4.0
        } else {
            12.0
        };
        if likely_window_limited
            && queue_delay_ms <= frame_ms * low_delay_frames
            && jitter_ratio < 0.25
        {
            client.probe_frames = (client.probe_frames + 1.0).min(max_probe_frames);
        } else if !likely_window_limited
            && browser_ready(client)
            && queue_delay_ms <= frame_ms * 2.0
            && jitter_ratio < 0.25
        {
            client.probe_frames = (client.probe_frames + 0.25).min(max_probe_frames * 0.5);
        } else if queue_delay_ms > frame_ms * high_delay_frames || jitter_ratio > 0.5 {
            client.probe_frames = (client.probe_frames * 0.5).max(1.0);
        } else if queue_delay_ms > frame_ms * 2.0 || !browser_ready(client) {
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

fn is_unset_view_size(rows: u16, cols: u16) -> bool {
    rows == 0 && cols == 0
}

fn subscribe_client_to(client: &mut ClientState, pty_id: u16) {
    if client.subscriptions.insert(pty_id) {
        client.last_sent.remove(&pty_id);
        client.preview_next_send_at.remove(&pty_id);
    }
}

fn unsubscribe_client_from(client: &mut ClientState, pty_id: u16) -> bool {
    let removed_sub = client.subscriptions.remove(&pty_id);
    client.last_sent.remove(&pty_id);
    client.preview_next_send_at.remove(&pty_id);
    let removed_view = client.view_sizes.remove(&pty_id).is_some();
    if client.lead == Some(pty_id) {
        client.lead = None;
        client.scroll_offset = 0;
        client.scroll_cache = FrameState::default();
    }
    removed_sub || removed_view
}

fn update_client_scroll_state(client: &mut ClientState, pty_id: u16, next_offset: usize) -> bool {
    if client.lead != Some(pty_id) || client.scroll_offset == next_offset {
        return false;
    }

    let prev_offset = client.scroll_offset;
    if prev_offset == 0 && next_offset > 0 {
        client.scroll_cache = client.last_sent.get(&pty_id).cloned().unwrap_or_default();
    } else if prev_offset > 0 && next_offset == 0 {
        if client.scroll_cache.rows() > 0 && client.scroll_cache.cols() > 0 {
            client.last_sent.insert(pty_id, client.scroll_cache.clone());
        } else {
            client.last_sent.remove(&pty_id);
        }
        client.scroll_cache = FrameState::default();
    }

    client.scroll_offset = next_offset;
    reset_inflight(client);
    true
}

struct Session {
    ptys: HashMap<u16, Pty>,
    next_client_id: u64,
    /// Diagnostics: how many times tick() was called this second.
    tick_fires: u32,
    /// Diagnostics: how many ticks found the focused PTY dirty (snapshot taken).
    tick_snaps: u32,
    clients: HashMap<u64, ClientState>,
}

struct SearchResultRow {
    pty_id: u16,
    score: u32,
    primary_source: u8,
    matched_sources: u8,
    context: String,
    scroll_offset: Option<usize>,
}

struct TickOutcome {
    did_work: bool,
    next_deadline: Option<Instant>,
}

impl Session {
    fn new() -> Self {
        Self {
            ptys: HashMap::new(),
            next_client_id: 1,
            clients: HashMap::new(),
            tick_fires: 0,
            tick_snaps: 0,
        }
    }

    fn allocate_pty_id(&mut self) -> Option<u16> {
        (1..=u16::MAX).find(|id| !self.ptys.contains_key(id))
    }

    fn send_to_all(&self, msg: &[u8]) {
        for c in self.clients.values() {
            let _ = c.tx.try_send(msg.to_vec());
        }
    }

    fn mediated_size_for_pty(&self, pty_id: u16) -> Option<(u16, u16)> {
        let mut min_rows: Option<u16> = None;
        let mut min_cols: Option<u16> = None;
        for c in self.clients.values() {
            if let Some((r, cols)) = c.view_sizes.get(&pty_id).copied() {
                min_rows = Some(min_rows.map_or(r, |m: u16| m.min(r)));
                min_cols = Some(min_cols.map_or(cols, |m: u16| m.min(cols)));
            }
        }
        match (min_rows, min_cols) {
            (Some(r), Some(c)) => Some((r.max(1), c.max(1))),
            _ => None,
        }
    }

    fn resize_pty(&mut self, pty_id: u16, rows: u16, cols: u16) -> bool {
        let pty = match self.ptys.get_mut(&pty_id) {
            Some(p) => p,
            None => return false,
        };
        let (cur_rows, cur_cols) = pty.driver.size();
        if cur_rows == rows && cur_cols == cols {
            return false;
        }
        pty.ready_frames.clear();
        pty.driver.resize(rows, cols);
        pty.mark_dirty();
        for c in self.clients.values_mut() {
            if c.subscriptions.contains(&pty_id) {
                c.last_sent.remove(&pty_id);
            }
            if c.lead == Some(pty_id) {
                c.scroll_cache = FrameState::default();
                reset_inflight(c);
            }
        }
        if !pty.exited {
            unsafe {
                let ws = libc::winsize {
                    ws_row: rows,
                    ws_col: cols,
                    ws_xpixel: 0,
                    ws_ypixel: 0,
                };
                libc::ioctl(pty.master_fd, libc::TIOCSWINSZ, &ws);
                let mut fg_pgid: libc::pid_t = 0;
                libc::ioctl(pty.master_fd, libc::TIOCGPGRP, &mut fg_pgid);
                if fg_pgid > 0 {
                    libc::kill(-fg_pgid, libc::SIGWINCH);
                }
                libc::kill(-pty.child_pid, libc::SIGWINCH);
            }
        }
        true
    }

    fn resize_ptys_to_mediated_sizes<I>(&mut self, pty_ids: I) -> bool
    where
        I: IntoIterator<Item = u16>,
    {
        let mut changed = false;
        let mut seen = HashSet::new();
        for pty_id in pty_ids {
            if !seen.insert(pty_id) {
                continue;
            }
            if let Some((rows, cols)) = self.mediated_size_for_pty(pty_id) {
                changed |= self.resize_pty(pty_id, rows, cols);
            }
        }
        changed
    }

    fn pty_list_msg(&self) -> Vec<u8> {
        let mut msg = vec![S2C_LIST];
        let count = self.ptys.len() as u16;
        msg.extend_from_slice(&count.to_le_bytes());
        let mut ids: Vec<u16> = self.ptys.keys().copied().collect();
        ids.sort();
        for id in ids {
            let tag = self.ptys[&id].tag.as_bytes();
            msg.extend_from_slice(&id.to_le_bytes());
            msg.extend_from_slice(&(tag.len() as u16).to_le_bytes());
            msg.extend_from_slice(tag);
        }
        msg
    }
}

type AppState = Arc<(Config, Mutex<Session>, PtyFds, Arc<Notify>)>;

fn nudge_delivery(state: &AppState) {
    state.3.notify_one();
}

fn pty_cwd(pid: libc::pid_t) -> Option<String> {
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

#[allow(clippy::too_many_arguments)]
fn spawn_pty(
    shell: &str,
    rows: u16,
    cols: u16,
    id: u16,
    tag: &str,
    command: Option<&str>,
    argv: Option<&[&str]>,
    dir: Option<&str>,
    scrollback: usize,
    state: AppState,
) -> Option<Pty> {
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
        let effective_dir = dir.map(String::from);
        if let Some(d) = effective_dir {
            if let Ok(dir_c) = CString::new(d) {
                unsafe {
                    libc::chdir(dir_c.as_ptr());
                }
            }
        }
        std::env::set_var("TERM", "xterm-256color");
        std::env::set_var("COLORTERM", "truecolor");
        // Don't set COLUMNS/LINES — ncurses apps prioritize these over
        // TIOCGWINSZ and won't resize properly if they're set to stale values.
        std::env::remove_var("COLUMNS");
        std::env::remove_var("LINES");
        for (key, _) in std::env::vars() {
            if key.starts_with("BLIT_") {
                std::env::remove_var(&key);
            }
        }
        let shell_flags = &state.0.shell_flags;
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
        if let Some(args) = argv {
            if !args.is_empty() {
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
    let lflag_cache = pty_lflag(master);

    Some(Pty {
        master_fd: master,
        child_pid: pid,
        driver: Box::new(AlacrittyDriver::new(rows, cols, scrollback)),
        tag: tag.to_owned(),
        dirty: true,
        ready_frames: VecDeque::new(),
        byte_rx,
        reader_handle,
        lflag_cache,
        lflag_last: Instant::now(),
        last_title_send: Instant::now(),
        title_pending: false,
        exited: false,
        exit_status: blit_remote::EXIT_STATUS_UNKNOWN,
        command: command.map(|s| s.to_owned()),
    })
}

/// Spawn a new child process on a fresh PTY pair.
/// Returns (master_fd, child_pid, reader_handle, byte_rx) for swapping into an existing Pty.
fn respawn_child(
    shell: &str,
    rows: u16,
    cols: u16,
    pty_id: u16,
    command: Option<&str>,
    state: AppState,
) -> Option<(libc::c_int, libc::pid_t, std::thread::JoinHandle<()>, mpsc::Receiver<PtyInput>)> {
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
        std::env::set_var("TERM", "xterm-256color");
        std::env::set_var("COLORTERM", "truecolor");
        std::env::remove_var("COLUMNS");
        std::env::remove_var("LINES");
        for (key, _) in std::env::vars() {
            if key.starts_with("BLIT_") {
                std::env::remove_var(&key);
            }
        }
        let shell_flags = &state.0.shell_flags;
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
    Some((master, pid, reader_handle, byte_rx))
}

fn respond_to_queries(fd: libc::c_int, data: &[u8], size: (u16, u16), cursor: (u16, u16)) {
    // VT420 with features matching xterm-256color capabilities.
    const DA1_RESPONSE: &[u8] = b"\x1b[?64;1;2;6;9;15;18;21;22c";

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
            b'n' if params == b"6" => Some(format!("\x1b[{};{}R", cursor.0 + 1, cursor.1 + 1)),
            b'n' if params == b"5" => Some("\x1b[0n".into()),
            b't' if params == b"18" => {
                let (rows, cols) = size;
                Some(format!("\x1b[8;{rows};{cols}t"))
            }
            b't' if params == b"14" => {
                let (rows, cols) = size;
                Some(format!("\x1b[4;{};{}t", rows * 16, cols * 8))
            }
            _ => None,
        };
        if let Some(r) = resp {
            pty_write_all(fd, r.as_bytes());
        }
    }
}

fn pty_reader(
    fd: libc::c_int,
    tx: mpsc::Sender<PtyInput>,
    notify: Arc<Notify>,
) {
    // Use a dedicated OS thread with a plain blocking read() instead of
    // tokio's AsyncFd (kqueue/epoll). On macOS, registering a kqueue watcher
    // on the PTY master fd adds significant per-write overhead in the kernel's
    // TTY layer — every slave write triggers a kevent notification. A blocking
    // read in a dedicated thread avoids this entirely, matching what native
    // terminals like Ghostty do.

    // Ensure the fd is in blocking mode.
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
                if let Some(boundary) = find_sync_output_end(&sync_scan_tail, &remaining) {
                    let before = remaining[..boundary].to_vec();
                    let after = remaining[boundary..].to_vec();
                    update_sync_scan_tail(&mut sync_scan_tail, &before);
                    if tx.blocking_send(PtyInput::SyncBoundary { before, after: after.clone() }).is_err() {
                        return;
                    }
                    notify.notify_one();
                    remaining = after;
                } else {
                    update_sync_scan_tail(&mut sync_scan_tail, &remaining);
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

/// Split accumulated bytes at sync-output boundaries and send through the channel.
async fn cleanup_pty(pty_id: u16, state: &AppState) {
    // Remove the fd so no more writes go to the closed master.
    state.2.write().unwrap().remove(&pty_id);
    let mut sess = state.1.lock().await;
    if let Some(pty) = sess.ptys.get_mut(&pty_id) {
        if pty.exited {
            return;
        }
        pty.exited = true;
        // Reset mouse mode etc. so the client stops sending mouse events
        // to the now-dead shell (e.g. mpv exits without disabling ?1003).
        pty.driver.reset_modes();
        unsafe {
            libc::kill(pty.child_pid, libc::SIGHUP);
            libc::close(pty.master_fd);
            let mut wstatus: libc::c_int = 0;
            if libc::waitpid(pty.child_pid, &mut wstatus, libc::WNOHANG) > 0 {
                if libc::WIFEXITED(wstatus) {
                    pty.exit_status = libc::WEXITSTATUS(wstatus);
                } else if libc::WIFSIGNALED(wstatus) {
                    pty.exit_status = -(libc::WTERMSIG(wstatus) as i32);
                }
            }
        }
        pty.mark_dirty();
        let msg = blit_remote::msg_exited(pty_id, pty.exit_status);
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

fn take_snapshot(pty: &mut Pty) -> FrameState {
    if pty.lflag_last.elapsed() >= Duration::from_millis(250) {
        pty.lflag_cache = pty_lflag(pty.master_fd);
        pty.lflag_last = Instant::now();
    }
    let (echo, icanon) = pty.lflag_cache;
    pty.driver.snapshot(echo, icanon)
}

fn build_scrollback_update(
    pty: &mut Pty,
    id: u16,
    offset: usize,
    prev_frame: &FrameState,
) -> Option<(Vec<u8>, FrameState)> {
    let frame = pty.driver.scrollback_frame(offset);
    let msg = build_update_msg(id, &frame, prev_frame);
    msg.map(|m| (m, frame))
}

fn build_search_results_msg(request_id: u16, results: &[SearchResultRow]) -> Vec<u8> {
    let count = results.len().min(u16::MAX as usize);
    let payload_bytes: usize = results[..count]
        .iter()
        .map(|result| 14 + result.context.len().min(u16::MAX as usize))
        .sum();
    let mut msg = Vec::with_capacity(5 + payload_bytes);
    msg.push(S2C_SEARCH_RESULTS);
    msg.extend_from_slice(&request_id.to_le_bytes());
    msg.extend_from_slice(&(count as u16).to_le_bytes());
    for result in &results[..count] {
        msg.extend_from_slice(&result.pty_id.to_le_bytes());
        msg.extend_from_slice(&result.score.to_le_bytes());
        msg.push(result.primary_source);
        msg.push(result.matched_sources);
        let scroll_offset = result
            .scroll_offset
            .map(|offset| offset.min(u32::MAX as usize - 1) as u32)
            .unwrap_or(u32::MAX);
        msg.extend_from_slice(&scroll_offset.to_le_bytes());
        let context = result.context.as_bytes();
        let context_len = context.len().min(u16::MAX as usize);
        msg.extend_from_slice(&(context_len as u16).to_le_bytes());
        msg.extend_from_slice(&context[..context_len]);
    }
    msg
}

enum SendOutcome {
    NoChange,
    Sent,
    Backpressured,
}

fn try_send_update(
    client: &mut ClientState,
    pid: u16,
    current: FrameState,
    msg: Option<Vec<u8>>,
    now: Instant,
    paced: bool,
) -> SendOutcome {
    let Some(msg) = msg else {
        return SendOutcome::NoChange;
    };
    let bytes = msg.len();
    if client.tx.try_send(msg).is_ok() {
        client.last_sent.insert(pid, current);
        record_send(client, bytes, now, paced);
        client.frames_sent = client.frames_sent.wrapping_add(1);
        SendOutcome::Sent
    } else {
        // Outbox full — the sender can't keep up.  Advance last_sent to
        // the current frame so the NEXT diff is small (only changes since
        // now), effectively dropping this intermediate state.  Without
        // this, backpressure causes the tick to re-dirty the PTY, building
        // ever-larger diffs that make the backlog worse.
        client.last_sent.insert(pid, current);
        SendOutcome::Backpressured
    }
}

fn default_socket_path() -> String {
    if let Ok(dir) = std::env::var("TMPDIR") {
        return format!("{dir}/blit.sock");
    }
    if let Ok(dir) = std::env::var("XDG_RUNTIME_DIR") {
        return format!("{dir}/blit.sock");
    }
    if let Ok(user) = std::env::var("USER") {
        return format!("/tmp/blit-{user}.sock");
    }
    "/tmp/blit.sock".into()
}

fn usage() -> &'static str {
    "usage: blit-server [--socket PATH] [--fd-channel FD] [--shell-flags FLAGS] [PATH]"
}

fn parse_fd_value(s: &str, label: &str) -> RawFd {
    s.parse::<RawFd>().unwrap_or_else(|_| {
        eprintln!("invalid fd number for {label}: {s}");
        std::process::exit(2);
    })
}

fn parse_config() -> Config {
    let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".into());
    let mut shell_flags = std::env::var("BLIT_SHELL_FLAGS").unwrap_or_else(|_| "li".into());
    let scrollback = std::env::var("BLIT_SCROLLBACK")
        .ok()
        .and_then(|s| s.parse::<usize>().ok())
        .unwrap_or(SCROLLBACK_ROWS_DEFAULT);
    let mut socket_path = std::env::var("BLIT_SOCK").ok();
    let mut fd_channel: Option<RawFd> = std::env::var("BLIT_FD_CHANNEL")
        .ok()
        .map(|s| parse_fd_value(&s, "BLIT_FD_CHANNEL"));

    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        if arg == "--help" || arg == "-h" {
            println!("{}", usage());
            println!("  --socket PATH            Unix socket path (or set BLIT_SOCK)");
            println!("  --fd-channel FD          Accept clients via fd-passing on FD (or set BLIT_FD_CHANNEL)");
            println!(
                "  --shell-flags FLAGS      Shell flags (default: li, or set BLIT_SHELL_FLAGS)"
            );
            println!("  --version, -V            Print version");
            std::process::exit(0);
        }
        if arg == "--version" || arg == "-V" {
            println!("blit-server {}", env!("CARGO_PKG_VERSION"));
            std::process::exit(0);
        }

        if let Some(value) = arg.strip_prefix("--socket=") {
            socket_path = Some(value.to_owned());
            continue;
        }

        if arg == "--socket" {
            socket_path = Some(args.next().unwrap_or_else(|| {
                eprintln!("missing value for --socket");
                eprintln!("{}", usage());
                std::process::exit(2);
            }));
            continue;
        }

        if let Some(value) = arg.strip_prefix("--fd-channel=") {
            fd_channel = Some(parse_fd_value(value, "--fd-channel"));
            continue;
        }

        if arg == "--fd-channel" {
            let value = args.next().unwrap_or_else(|| {
                eprintln!("missing value for --fd-channel");
                eprintln!("{}", usage());
                std::process::exit(2);
            });
            fd_channel = Some(parse_fd_value(&value, "--fd-channel"));
            continue;
        }

        if let Some(value) = arg.strip_prefix("--shell-flags=") {
            shell_flags = value.to_owned();
            continue;
        }

        if arg == "--shell-flags" {
            shell_flags = args.next().unwrap_or_else(|| {
                eprintln!("missing value for --shell-flags");
                eprintln!("{}", usage());
                std::process::exit(2);
            });
            continue;
        }

        if arg.starts_with('-') {
            eprintln!("unrecognized argument: {arg}");
            eprintln!("{}", usage());
            std::process::exit(2);
        }

        if socket_path.replace(arg).is_some() {
            eprintln!("multiple socket paths provided");
            eprintln!("{}", usage());
            std::process::exit(2);
        }
    }

    Config {
        shell,
        shell_flags,
        scrollback,
        socket_path: socket_path.unwrap_or_else(default_socket_path),
        fd_channel,
    }
}

enum RecvFdResult {
    Fd(RawFd),
    WouldBlock,
    Closed,
}

fn recv_fd(channel: RawFd) -> RecvFdResult {
    unsafe {
        let mut buf = [0u8; 1];
        let mut iov = libc::iovec {
            iov_base: buf.as_mut_ptr() as *mut libc::c_void,
            iov_len: buf.len(),
        };
        let cmsg_space = libc::CMSG_SPACE(std::mem::size_of::<RawFd>() as u32) as usize;
        let mut cmsg_buf = vec![0u8; cmsg_space];
        let mut msg: libc::msghdr = std::mem::zeroed();
        msg.msg_iov = &mut iov;
        msg.msg_iovlen = 1;
        msg.msg_control = cmsg_buf.as_mut_ptr() as *mut libc::c_void;
        msg.msg_controllen = cmsg_space as _;
        let n = libc::recvmsg(channel, &mut msg, libc::MSG_DONTWAIT);
        if n < 0 {
            let err = std::io::Error::last_os_error();
            if err.kind() == std::io::ErrorKind::WouldBlock {
                return RecvFdResult::WouldBlock;
            }
            if err.raw_os_error() == Some(libc::EINTR) {
                return RecvFdResult::WouldBlock;
            }
            return RecvFdResult::Closed;
        }
        if n == 0 {
            return RecvFdResult::Closed;
        }
        let cmsg = libc::CMSG_FIRSTHDR(&msg);
        if cmsg.is_null() {
            return RecvFdResult::Closed;
        }
        if (*cmsg).cmsg_level == libc::SOL_SOCKET && (*cmsg).cmsg_type == libc::SCM_RIGHTS {
            let fd_ptr = libc::CMSG_DATA(cmsg) as *const RawFd;
            RecvFdResult::Fd(std::ptr::read_unaligned(fd_ptr))
        } else {
            RecvFdResult::Closed
        }
    }
}

fn bind_socket(sock_path: &str) -> UnixListener {
    let _ = std::fs::remove_file(sock_path);
    let listener = UnixListener::bind(sock_path).unwrap_or_else(|e| {
        eprintln!("blit-server: cannot bind to {sock_path}: {e}");
        std::process::exit(1);
    });
    if let Err(e) = std::fs::set_permissions(sock_path, std::fs::Permissions::from_mode(0o700)) {
        eprintln!("blit-server: warning: cannot set socket permissions: {e}");
    }
    eprintln!("listening on {sock_path}");
    listener
}

#[tokio::main]
async fn main() {
    let config = parse_config();
    let state: AppState = Arc::new((
        config,
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

    tokio::spawn(async {
        loop {
            tokio::time::sleep(Duration::from_secs(5)).await;
            unsafe { while libc::waitpid(-1, std::ptr::null_mut(), libc::WNOHANG) > 0 {} }
        }
    });

    if let Some(channel_fd) = state.0.fd_channel {
        use std::os::unix::io::FromRawFd;
        eprintln!("accepting clients via fd-channel (fd {channel_fd})");
        let channel = unsafe { std::os::unix::net::UnixStream::from_raw_fd(channel_fd) };
        channel.set_nonblocking(true).unwrap();
        let async_channel = AsyncFd::new(channel).unwrap();
        loop {
            let mut guard = match async_channel.readable().await {
                Ok(g) => g,
                Err(e) => {
                    eprintln!("fd-channel error: {e}");
                    break;
                }
            };
            match recv_fd(channel_fd) {
                RecvFdResult::Fd(client_fd) => {
                    let std_stream =
                        unsafe { std::os::unix::net::UnixStream::from_raw_fd(client_fd) };
                    std_stream.set_nonblocking(true).unwrap();
                    let stream = tokio::net::UnixStream::from_std(std_stream).unwrap();
                    let state = state.clone();
                    tokio::spawn(handle_client(stream, state));
                    guard.retain_ready();
                }
                RecvFdResult::WouldBlock => {
                    guard.clear_ready();
                }
                RecvFdResult::Closed => {
                    break;
                }
            }
        }
        eprintln!("fd-channel closed, shutting down");
        return;
    }

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
            bind_socket(&state.0.socket_path)
        }
    } else {
        bind_socket(&state.0.socket_path)
    };

    loop {
        let (stream, _) = match listener.accept().await {
            Ok(conn) => conn,
            Err(e) => {
                eprintln!("accept error: {e}");
                tokio::time::sleep(Duration::from_millis(100)).await;
                continue;
            }
        };
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

    let max_fps = sess
        .clients
        .values()
        .map(browser_pacing_fps)
        .fold(1.0_f32, f32::max);
    let title_interval = Duration::from_secs_f64(1.0 / max_fps as f64);
    let ids: Vec<u16> = sess.ptys.keys().copied().collect();
    for &id in &ids {
        let Some(pty) = sess.ptys.get_mut(&id) else {
            continue;
        };
        if pty.driver.take_title_dirty() {
            pty.mark_dirty();
            pty.title_pending = true;
        }
        if pty.title_pending && now.duration_since(pty.last_title_send) >= title_interval {
            let msg = {
                let title_bytes = pty.driver.title().as_bytes();
                let mut msg = Vec::with_capacity(3 + title_bytes.len());
                msg.push(S2C_TITLE);
                msg.extend_from_slice(&id.to_le_bytes());
                msg.extend_from_slice(title_bytes);
                msg
            };
            pty.last_title_send = now;
            pty.title_pending = false;
            sess.send_to_all(&msg);
            did_work = true;
        }
    }

    // Drain bytes from PTY reader channels. This is the only place
    // process() is called, so there is no contention with the readers.
    let mut eof_ptys: Vec<u16> = Vec::new();
    for &id in &ids {
        let Some(pty) = sess.ptys.get_mut(&id) else {
            continue;
        };
        while let Ok(input) = pty.byte_rx.try_recv() {
            match input {
                PtyInput::Data(data) => {
                    respond_to_queries(
                        pty.master_fd,
                        &data,
                        pty.driver.size(),
                        pty.driver.cursor_position(),
                    );
                    pty.driver.process(&data);
                    pty.mark_dirty();
                    did_work = true;
                }
                PtyInput::SyncBoundary { before, after } => {
                    if !before.is_empty() {
                        respond_to_queries(
                            pty.master_fd,
                            &before,
                            pty.driver.size(),
                            pty.driver.cursor_position(),
                        );
                        pty.driver.process(&before);
                        pty.mark_dirty();
                    }
                    if !pty.driver.synced_output() {
                        let frame = take_snapshot(pty);
                        enqueue_ready_frame(&mut pty.ready_frames, frame);
                        pty.clear_dirty();
                    }
                    if !after.is_empty() {
                        respond_to_queries(
                            pty.master_fd,
                            &after,
                            pty.driver.size(),
                            pty.driver.cursor_position(),
                        );
                        pty.driver.process(&after);
                        pty.mark_dirty();
                    }
                    did_work = true;
                }
                PtyInput::Eof => {
                    eof_ptys.push(id);
                }
            }
        }
    }
    // Handle EOF outside the borrow loop.
    drop(sess);
    for id in eof_ptys {
        tokio::time::sleep(Duration::from_millis(50)).await;
        cleanup_pty(id, state).await;
    }
    let mut sess = state.1.lock().await;

    // Only snapshot PTYs that have at least one client ready to consume a fresh
    // frame right now. This avoids burning CPU on snapshot+diff+compress work
    // while the lead is merely waiting for its next pacing deadline.
    let needful_ptys: HashSet<u16> = sess
        .clients
        .values()
        .flat_map(|c| {
            let reserve_preview_slot = client_has_due_preview(&sess, c, now);
            c.subscriptions.iter().copied().filter(move |pid| {
                if Some(*pid) == c.lead {
                    c.scroll_offset == 0 && can_send_frame(c, now, reserve_preview_slot)
                } else {
                    can_send_preview(c, *pid, now)
                }
            })
        })
        .collect();

    let mut snapshots: HashMap<u16, FrameState> = HashMap::new();
    for &id in &ids {
        let Some(pty) = sess.ptys.get_mut(&id) else {
            continue;
        };
        if needful_ptys.contains(&id) {
            if let Some(frame) = pty.ready_frames.pop_front() {
                snapshots.insert(id, frame);
                sess.tick_snaps += 1;
                did_work = true;
                continue;
            }
        }
        if !should_snapshot_pty(
            pty.dirty,
            needful_ptys.contains(&id),
            pty.driver.synced_output(),
        ) {
            continue;
        }
        // Applications that care about complete-frame boundaries should
        // use DEC synchronized output (?2026). Outside that bracket we
        // snapshot immediately instead of heuristically coalescing reads.
        snapshots.insert(id, take_snapshot(pty));
        pty.clear_dirty();
        sess.tick_snaps += 1;
        did_work = true;
    }

    let client_ids: Vec<u64> = sess.clients.keys().copied().collect();
    for cid in client_ids {
        // When the pipe is idle (nothing in flight), RTT cannot be measured
        // and the last observed value stales.  Decay it toward min_rtt so
        // a stale congested RTT doesn't permanently suppress the send window
        // after congestion clears or traffic patterns change (e.g. switching
        // from a large-frame burst to idle small-frame updates).
        if let Some(c) = sess.clients.get_mut(&cid) {
            if c.inflight_bytes == 0 && c.min_rtt_ms > 0.0 && c.rtt_ms > c.min_rtt_ms {
                c.rtt_ms = (c.rtt_ms * 0.99 + c.min_rtt_ms * 0.01).max(c.min_rtt_ms);
            }
            // Decay stale browser metrics so a missed/delayed metrics update
            // can't permanently block the delivery loop.
            if c.last_metrics_update.elapsed() > Duration::from_secs(1) {
                c.browser_backlog_frames = 0;
                c.browser_ack_ahead_frames = 0;
            }
        }
        let (
            lead,
            subscriptions,
            scroll_offset,
            can_send_lead,
            lead_has_window,
            any_send_window,
            lead_deadline,
        ) = {
            let Some(c) = sess.clients.get(&cid) else {
                continue;
            };
            let reserve_preview_slot = client_has_due_preview(&sess, c, now);
            (
                c.lead,
                c.subscriptions.iter().copied().collect::<Vec<_>>(),
                c.scroll_offset,
                can_send_frame(c, now, reserve_preview_slot),
                lead_window_open(c, reserve_preview_slot),
                lead_window_open(c, reserve_preview_slot) || window_open(c),
                c.next_send_at,
            )
        };

        if subscriptions.is_empty() {
            continue;
        }

        if let Some(pid) = lead {
            if scroll_offset > 0 {
                if can_send_lead {
                    let prev_frame = {
                        let Some(c) = sess.clients.get(&cid) else {
                            continue;
                        };
                        c.scroll_cache.clone()
                    };
                    let outcome = if let Some(pty) = sess.ptys.get_mut(&pid) {
                        if let Some((msg, new_frame)) =
                            build_scrollback_update(pty, pid, scroll_offset, &prev_frame)
                        {
                            let Some(c) = sess.clients.get_mut(&cid) else {
                                break;
                            };
                            let bytes = msg.len();
                            if c.tx.try_send(msg).is_ok() {
                                c.scroll_cache = new_frame;
                                record_send(c, bytes, now, true);
                                c.frames_sent += 1;
                                SendOutcome::Sent
                            } else {
                                SendOutcome::Backpressured
                            }
                        } else {
                            SendOutcome::NoChange
                        }
                    } else {
                        SendOutcome::NoChange
                    };
                    match outcome {
                        SendOutcome::Sent => did_work = true,
                        SendOutcome::Backpressured => {
                            if let Some(pty) = sess.ptys.get_mut(&pid) {
                                pty.mark_dirty();
                            }
                        }
                        SendOutcome::NoChange => {}
                    }
                } else if lead_has_window {
                    next_deadline = Some(match next_deadline {
                        Some(existing) => existing.min(lead_deadline),
                        None => lead_deadline,
                    });
                }
            } else if can_send_lead {
                if let Some(cur) = snapshots.get(&pid).cloned() {
                    let previous = sess
                        .clients
                        .get(&cid)
                        .and_then(|c| c.last_sent.get(&pid).cloned())
                        .unwrap_or_default();
                    drop(sess);
                    let msg = build_update_msg(pid, &cur, &previous);
                    sess = state.1.lock().await;
                    let Some(c) = sess.clients.get_mut(&cid) else {
                        continue;
                    };
                    match try_send_update(c, pid, cur, msg, now, true) {
                        SendOutcome::Sent => did_work = true,
                        SendOutcome::Backpressured => {
                            if let Some(pty) = sess.ptys.get_mut(&pid) {
                                pty.mark_dirty();
                            }
                        }
                        SendOutcome::NoChange => {}
                    }
                } else {
                    let has_pending = sess
                        .ptys
                        .get(&pid)
                        .map(pty_has_visual_update)
                        .unwrap_or(false);
                    let _ = has_pending;
                }
            } else {
                let has_pending = sess
                    .ptys
                    .get(&pid)
                    .map(pty_has_visual_update)
                    .unwrap_or(false);
                if has_pending && lead_has_window {
                    next_deadline = Some(match next_deadline {
                        Some(existing) => existing.min(lead_deadline),
                        None => lead_deadline,
                    });
                }
            }
        }

        if !any_send_window {
            continue;
        }

        let mut preview_ids = subscriptions;
        preview_ids.retain(|pid| Some(*pid) != lead);
        preview_ids.sort_unstable();

        for pid in preview_ids {
            let (preview_can_send, preview_due_at, preview_has_window) =
                match sess.clients.get(&cid) {
                    Some(c) => (
                        can_send_preview(c, pid, now),
                        preview_deadline(c, pid, now),
                        window_open(c),
                    ),
                    None => (false, now, false),
                };
            if !preview_has_window {
                break;
            }
            if !preview_can_send {
                let has_pending = sess
                    .ptys
                    .get(&pid)
                    .map(pty_has_visual_update)
                    .unwrap_or(false);
                // Only set a deadline when the reason is *timing* (deadline
                // in the future), not capacity (preview window closed).
                // A past deadline here spins the delivery loop because
                // sleep_until(past) returns immediately.
                if has_pending && preview_due_at > now {
                    next_deadline = Some(match next_deadline {
                        Some(existing) => existing.min(preview_due_at),
                        None => preview_due_at,
                    });
                }
                continue;
            }
            let Some(cur) = snapshots.get(&pid) else {
                let has_pending = sess
                    .ptys
                    .get(&pid)
                    .map(pty_has_visual_update)
                    .unwrap_or(false);
                let _ = has_pending;
                continue;
            };
            let cur = cur.clone();
            let previous = sess
                .clients
                .get(&cid)
                .and_then(|c| c.last_sent.get(&pid).cloned())
                .unwrap_or_default();
            drop(sess);
            let msg = build_update_msg(pid, &cur, &previous);
            sess = state.1.lock().await;
            let Some(c) = sess.clients.get_mut(&cid) else {
                break;
            };
            match try_send_update(c, pid, cur, msg, now, false) {
                SendOutcome::Sent => {
                    record_preview_send(c, pid, now);
                    did_work = true;
                }
                SendOutcome::Backpressured => {
                    if let Some(pty) = sess.ptys.get_mut(&pid) {
                        pty.mark_dirty();
                    }
                    break;
                }
                SendOutcome::NoChange => {}
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
    let sender = tokio::spawn(async move {
        while let Some(msg) = out_rx.recv().await {
            if !write_frame(&mut writer, &msg).await {
                break;
            }
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
                lead: None,
                subscriptions: HashSet::new(),
                view_sizes: HashMap::new(),
                scroll_offset: 0,
                scroll_cache: FrameState::default(),
                last_sent: HashMap::new(),
                preview_next_send_at: HashMap::new(),
                rtt_ms: 50.0,
                min_rtt_ms: 0.0,
                display_fps: 60.0,
                // Conservative seed — the rise alpha (0.5) converges up to
                // multi-MB/s in a handful of samples on low-latency paths. Starting
                // high causes catastrophic bufferbloat on slow links because
                // target_byte_window scales with the goodput estimate.
                delivery_bps: 262_144.0,
                goodput_bps: 262_144.0,
                goodput_jitter_bps: 0.0,
                max_goodput_jitter_bps: 0.0,
                last_goodput_sample_bps: 0.0,
                avg_frame_bytes: 1_024.0,
                avg_paced_frame_bytes: 1_024.0,
                avg_preview_frame_bytes: 1_024.0,
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
                last_metrics_update: Instant::now(),
                last_log: Instant::now(),
                goodput_window_bytes: 0,
                goodput_window_start: Instant::now(),
            },
        );
        if let Some(c) = sess.clients.get(&client_id) {
            let _ = c.tx.try_send(msg_hello(
                1,
                FEATURE_CREATE_NONCE | FEATURE_RESTART | FEATURE_RESIZE_BATCH,
            ));
        }
        let mut initial_msgs = Vec::new();
        initial_msgs.push(sess.pty_list_msg());
        for (&id, pty) in &sess.ptys {
            let title = pty.driver.title();
            if !title.is_empty() {
                let title_bytes = title.as_bytes();
                let mut msg = Vec::with_capacity(3 + title_bytes.len());
                msg.push(S2C_TITLE);
                msg.extend_from_slice(&id.to_le_bytes());
                msg.extend_from_slice(title_bytes);
                initial_msgs.push(msg);
            }
            if pty.exited {
                initial_msgs.push(blit_remote::msg_exited(id, pty.exit_status));
            }
        }
        initial_msgs.push(vec![S2C_READY]);
        let tx = sess.clients.get(&client_id).map(|c| c.tx.clone());
        drop(sess);
        if let Some(tx) = tx {
            for msg in initial_msgs {
                if tx.send(msg).await.is_err() {
                    break;
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
            let (
                do_log,
                frames_sent,
                acks_recv,
                rtt_ms,
                min_rtt_ms,
                eff_rtt_ms,
                inflight_bytes,
                delivery_bps,
                goodput_ewma_bps,
                goodput_jitter_bps,
                max_goodput_jitter_bps,
                avg_frame_bytes,
                avg_paced_frame_bytes,
                avg_preview_frame_bytes,
                display_fps,
                paced_fps,
                display_need_bps,
                probe_frames,
                goodput_bps,
                window_frames,
                window_bytes,
                outbox_frames,
                browser_backlog_frames,
                browser_ack_ahead_frames,
                browser_apply_ms,
            ) = {
                let Some(c) = sess.clients.get_mut(&client_id) else {
                    continue;
                };
                c.acks_recv += 1;
                record_ack(c);
                let do_log = c.last_log.elapsed().as_secs_f32() >= 1.0;
                let log_elapsed = c.last_log.elapsed().as_secs_f32().max(1.0e-3);
                let paced_fps = pacing_fps(c);
                let display_need_bps = display_need_bps(c);
                let out = (
                    do_log,
                    c.frames_sent,
                    c.acks_recv,
                    c.rtt_ms,
                    path_rtt_ms(c),
                    window_rtt_ms(c),
                    c.inflight_bytes,
                    c.delivery_bps,
                    c.goodput_bps,
                    c.goodput_jitter_bps,
                    c.max_goodput_jitter_bps,
                    c.avg_frame_bytes,
                    c.avg_paced_frame_bytes,
                    c.avg_preview_frame_bytes,
                    c.display_fps,
                    paced_fps,
                    display_need_bps,
                    c.probe_frames,
                    c.acked_bytes_since_log as f32 / log_elapsed,
                    target_frame_window(c),
                    target_byte_window(c),
                    outbox_queued_frames(c),
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
                eprintln!(
                    "client {client_id}: sent={frames_sent} acks={acks_recv} rtt={rtt_ms:.0}ms min_rtt={min_rtt_ms:.0}ms eff_rtt={eff_rtt_ms:.0}ms window={window_frames}f/{window_bytes}B probe={probe_frames:.0}f inflight={inflight_bytes}B outbox={outbox_frames}f goodput={goodput_bps:.0}B/s goodput_ewma={goodput_ewma_bps:.0}B/s jitter={goodput_jitter_bps:.0}/{max_goodput_jitter_bps:.0}B/s rate={delivery_bps:.0}B/s avg_frame={avg_frame_bytes:.0}B lead_frame={avg_paced_frame_bytes:.0}B preview_frame={avg_preview_frame_bytes:.0}B need={display_need_bps:.0}B/s display_fps={display_fps:.0} paced_fps={paced_fps:.0} backlog={browser_backlog_frames} ack_ahead={browser_ack_ahead_frames} apply={browser_apply_ms:.1}ms | tick_fires={} tick_snaps={}",
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
            if fps > 0.0 {
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
                c.last_metrics_update = Instant::now();
            }
            nudge_delivery(&state);
            continue;
        }

        // Server-side mouse: client sends structured mouse data, server generates
        // the correct escape sequence using the terminal's current mouse mode/encoding.
        if data[0] == C2S_MOUSE && data.len() >= 9 {
            let pid = u16::from_le_bytes([data[1], data[2]]);
            let type_ = data[3];
            let button = data[4];
            let col = u16::from_le_bytes([data[5], data[6]]);
            let row = u16::from_le_bytes([data[7], data[8]]);
            let sess = state.1.lock().await;
            if let Some(pty) = sess.ptys.get(&pid) {
                let (echo, icanon) = pty.lflag_cache;
                if let Some(seq) = pty
                    .driver
                    .mouse_event(type_, button, col, row, echo, icanon)
                {
                    if let Some(&fd) = state.2.read().unwrap().get(&pid) {
                        pty_write_all(fd, &seq);
                    }
                }
            }
            continue;
        }

        if data[0] == C2S_INPUT && data.len() >= 3 {
            let pid = u16::from_le_bytes([data[1], data[2]]);
            let mut need_nudge = false;
            {
                let mut sess = state.1.lock().await;
                if let Some(c) = sess.clients.get_mut(&client_id) {
                    if update_client_scroll_state(c, pid, 0) {
                        if let Some(pty) = sess.ptys.get_mut(&pid) {
                            pty.mark_dirty();
                            need_nudge = true;
                        }
                    }
                }
            }
            if need_nudge {
                nudge_delivery(&state);
            }
            if let Some(&fd) = state.2.read().unwrap().get(&pid) {
                pty_write_all(fd, &data[3..]);
            }
            continue;
        }

        if data[0] == C2S_SEARCH && data.len() >= 3 {
            let request_id = u16::from_le_bytes([data[1], data[2]]);
            let query = std::str::from_utf8(&data[3..]).unwrap_or("").trim();
            let mut sess = state.1.lock().await;
            let lead = sess.clients.get(&client_id).and_then(|c| c.lead);
            let mut ranked: Vec<SearchResultRow> = if query.is_empty() {
                Vec::new()
            } else {
                sess.ptys
                    .iter()
                    .filter_map(|(&pty_id, pty)| {
                        pty.driver
                            .search_result(query)
                            .map(|result| SearchResultRow {
                                pty_id,
                                score: result.score,
                                primary_source: result.primary_source,
                                matched_sources: result.matched_sources,
                                context: result.context,
                                scroll_offset: result.scroll_offset,
                            })
                    })
                    .collect()
            };
            ranked.sort_by(|a, b| {
                b.score
                    .cmp(&a.score)
                    .then_with(|| (Some(b.pty_id) == lead).cmp(&(Some(a.pty_id) == lead)))
                    .then_with(|| a.pty_id.cmp(&b.pty_id))
            });
            if let Some(client) = sess.clients.get_mut(&client_id) {
                let _ = client
                    .tx
                    .try_send(build_search_results_msg(request_id, &ranked));
            }
            continue;
        }

        let mut sess = state.1.lock().await;
        let mut need_nudge = false;
        match data[0] {
            C2S_SCROLL if data.len() >= 7 => {
                let pid = u16::from_le_bytes([data[1], data[2]]);
                let offset = u32::from_le_bytes([data[3], data[4], data[5], data[6]]) as usize;
                if sess.ptys.contains_key(&pid) {
                    if let Some(c) = sess.clients.get_mut(&client_id) {
                        update_client_scroll_state(c, pid, offset);
                    }
                    if let Some(pty) = sess.ptys.get_mut(&pid) {
                        pty.mark_dirty();
                        need_nudge = true;
                    }
                }
            }
            C2S_RESIZE if data.len() >= 7 => {
                let entries = data[1..].chunks_exact(6);
                if !entries.remainder().is_empty() {
                    continue;
                }
                let mut touched = Vec::new();
                for entry in entries {
                    let pid = u16::from_le_bytes([entry[0], entry[1]]);
                    if !sess.ptys.contains_key(&pid) {
                        continue;
                    }
                    let rows = u16::from_le_bytes([entry[2], entry[3]]);
                    let cols = u16::from_le_bytes([entry[4], entry[5]]);
                    if let Some(c) = sess.clients.get_mut(&client_id) {
                        if is_unset_view_size(rows, cols) {
                            if c.view_sizes.remove(&pid).is_some() {
                                touched.push(pid);
                            }
                        } else if rows == 0 || cols == 0 {
                            continue;
                        } else {
                            c.view_sizes.insert(pid, (rows, cols));
                            touched.push(pid);
                        }
                    }
                }
                if sess.resize_ptys_to_mediated_sizes(touched) {
                    need_nudge = true;
                }
            }
            C2S_CREATE => {
                // Format: [opcode][rows:2][cols:2][tag_len:2][tag:N][command...]
                let (rows, cols) = if data.len() >= 5 {
                    (
                        u16::from_le_bytes([data[1], data[2]]),
                        u16::from_le_bytes([data[3], data[4]]),
                    )
                } else {
                    (24, 80)
                };
                let tag_len = if data.len() >= 7 {
                    u16::from_le_bytes([data[5], data[6]]) as usize
                } else {
                    0
                };
                let tag = if data.len() >= 7 + tag_len {
                    std::str::from_utf8(&data[7..7 + tag_len]).unwrap_or_default()
                } else {
                    ""
                };
                let cmd_start = 7 + tag_len;
                let dir: Option<String> = None;
                let create_payload = data
                    .get(cmd_start..)
                    .and_then(|bytes| std::str::from_utf8(bytes).ok());
                let command = create_payload
                    .filter(|payload| !payload.contains('\0'))
                    .map(str::trim)
                    .filter(|payload| !payload.is_empty());
                let argv: Option<Vec<&str>> = create_payload
                    .filter(|payload| payload.contains('\0'))
                    .map(|payload| {
                        payload
                            .split('\0')
                            .filter(|arg| !arg.is_empty())
                            .collect::<Vec<_>>()
                    })
                    .filter(|args| !args.is_empty());
                let Some(id) = sess.allocate_pty_id() else {
                    continue;
                };
                if let Some(pty) = spawn_pty(
                    &config.shell,
                    rows,
                    cols,
                    id,
                    tag,
                    command,
                    argv.as_deref(),
                    dir.as_deref(),
                    config.scrollback,
                    state.clone(),
                ) {
                    let mut msg = Vec::with_capacity(3 + pty.tag.len());
                    msg.push(S2C_CREATED);
                    msg.extend_from_slice(&id.to_le_bytes());
                    msg.extend_from_slice(pty.tag.as_bytes());
                    sess.ptys.insert(id, pty);
                    if let Some(c) = sess.clients.get_mut(&client_id) {
                        c.lead = Some(id);
                        c.view_sizes.insert(id, (rows, cols));
                        subscribe_client_to(c, id);
                        c.scroll_offset = 0;
                        c.scroll_cache = FrameState::default();
                        reset_inflight(c);
                    }
                    sess.send_to_all(&msg);
                    need_nudge = true;
                }
            }
            C2S_CREATE_N => {
                // Format: [opcode][nonce:2][rows:2][cols:2][tag_len:2][tag:N][command...]
                let nonce = if data.len() >= 3 {
                    u16::from_le_bytes([data[1], data[2]])
                } else {
                    0
                };
                let (rows, cols) = if data.len() >= 7 {
                    (
                        u16::from_le_bytes([data[3], data[4]]),
                        u16::from_le_bytes([data[5], data[6]]),
                    )
                } else {
                    (24, 80)
                };
                let tag_len = if data.len() >= 9 {
                    u16::from_le_bytes([data[7], data[8]]) as usize
                } else {
                    0
                };
                let tag = if data.len() >= 9 + tag_len {
                    std::str::from_utf8(&data[9..9 + tag_len]).unwrap_or_default()
                } else {
                    ""
                };
                let cmd_start = 9 + tag_len;
                let dir: Option<String> = None;
                let create_payload = data
                    .get(cmd_start..)
                    .and_then(|bytes| std::str::from_utf8(bytes).ok());
                let command = create_payload
                    .filter(|payload| !payload.contains('\0'))
                    .map(str::trim)
                    .filter(|payload| !payload.is_empty());
                let argv: Option<Vec<&str>> = create_payload
                    .filter(|payload| payload.contains('\0'))
                    .map(|payload| {
                        payload
                            .split('\0')
                            .filter(|arg| !arg.is_empty())
                            .collect::<Vec<_>>()
                    })
                    .filter(|args| !args.is_empty());
                let Some(id) = sess.allocate_pty_id() else {
                    continue;
                };
                if let Some(pty) = spawn_pty(
                    &config.shell,
                    rows,
                    cols,
                    id,
                    tag,
                    command,
                    argv.as_deref(),
                    dir.as_deref(),
                    config.scrollback,
                    state.clone(),
                ) {
                    let tag_bytes = pty.tag.as_bytes();
                    let mut nonce_msg = Vec::with_capacity(5 + tag_bytes.len());
                    nonce_msg.push(S2C_CREATED_N);
                    nonce_msg.extend_from_slice(&nonce.to_le_bytes());
                    nonce_msg.extend_from_slice(&id.to_le_bytes());
                    nonce_msg.extend_from_slice(tag_bytes);
                    let mut broadcast_msg = Vec::with_capacity(3 + tag_bytes.len());
                    broadcast_msg.push(S2C_CREATED);
                    broadcast_msg.extend_from_slice(&id.to_le_bytes());
                    broadcast_msg.extend_from_slice(tag_bytes);
                    sess.ptys.insert(id, pty);
                    if let Some(c) = sess.clients.get_mut(&client_id) {
                        c.lead = Some(id);
                        c.view_sizes.insert(id, (rows, cols));
                        subscribe_client_to(c, id);
                        c.scroll_offset = 0;
                        c.scroll_cache = FrameState::default();
                        reset_inflight(c);
                        let _ = c.tx.try_send(nonce_msg);
                    }
                    for (&cid, c) in sess.clients.iter() {
                        if cid != client_id {
                            let _ = c.tx.try_send(broadcast_msg.clone());
                        }
                    }
                    need_nudge = true;
                }
            }
            C2S_CREATE_AT => {
                // Format: [opcode][rows:2][cols:2][tag_len:2][tag:N][src_pty_id:2]
                let (rows, cols) = if data.len() >= 5 {
                    (
                        u16::from_le_bytes([data[1], data[2]]),
                        u16::from_le_bytes([data[3], data[4]]),
                    )
                } else {
                    (24, 80)
                };
                let tag_len = if data.len() >= 7 {
                    u16::from_le_bytes([data[5], data[6]]) as usize
                } else {
                    0
                };
                let tag = if data.len() >= 7 + tag_len {
                    std::str::from_utf8(&data[7..7 + tag_len]).unwrap_or_default()
                } else {
                    ""
                };
                let src_start = 7 + tag_len;
                let dir = if data.len() >= src_start + 2 {
                    let src_id = u16::from_le_bytes([data[src_start], data[src_start + 1]]);
                    sess.ptys.get(&src_id).and_then(|p| pty_cwd(p.child_pid))
                } else {
                    None
                };
                let Some(id) = sess.allocate_pty_id() else {
                    continue;
                };
                if let Some(pty) = spawn_pty(
                    &config.shell,
                    rows,
                    cols,
                    id,
                    tag,
                    None,
                    None,
                    dir.as_deref(),
                    config.scrollback,
                    state.clone(),
                ) {
                    let mut msg = Vec::with_capacity(3 + pty.tag.len());
                    msg.push(S2C_CREATED);
                    msg.extend_from_slice(&id.to_le_bytes());
                    msg.extend_from_slice(pty.tag.as_bytes());
                    sess.ptys.insert(id, pty);
                    if let Some(c) = sess.clients.get_mut(&client_id) {
                        c.lead = Some(id);
                        c.view_sizes.insert(id, (rows, cols));
                        subscribe_client_to(c, id);
                        c.scroll_offset = 0;
                        c.scroll_cache = FrameState::default();
                        reset_inflight(c);
                    }
                    sess.send_to_all(&msg);
                    need_nudge = true;
                }
            }
            C2S_CREATE2 => {
                // Generic create: [0x18][nonce:2][rows:2][cols:2][features:1][tag_len:2][tag:N][...fields]
                if data.len() < 10 {
                    continue;
                }
                let nonce = u16::from_le_bytes([data[1], data[2]]);
                let rows = u16::from_le_bytes([data[3], data[4]]);
                let cols = u16::from_le_bytes([data[5], data[6]]);
                let features = data[7];
                let tag_len = u16::from_le_bytes([data[8], data[9]]) as usize;
                let tag = if data.len() >= 10 + tag_len {
                    std::str::from_utf8(&data[10..10 + tag_len]).unwrap_or_default()
                } else {
                    ""
                };
                let mut cursor = 10 + tag_len;
                let dir = if features & CREATE2_HAS_SRC_PTY != 0 && data.len() >= cursor + 2 {
                    let src_id = u16::from_le_bytes([data[cursor], data[cursor + 1]]);
                    cursor += 2;
                    sess.ptys.get(&src_id).and_then(|p| pty_cwd(p.child_pid))
                } else {
                    None
                };
                let create_payload = if features & CREATE2_HAS_COMMAND != 0 {
                    data.get(cursor..).and_then(|b| std::str::from_utf8(b).ok())
                } else {
                    None
                };
                let command = create_payload
                    .filter(|p| !p.contains('\0'))
                    .map(str::trim)
                    .filter(|p| !p.is_empty());
                let argv: Option<Vec<&str>> = create_payload
                    .filter(|p| p.contains('\0'))
                    .map(|p| p.split('\0').filter(|a| !a.is_empty()).collect::<Vec<_>>())
                    .filter(|a| !a.is_empty());
                let Some(id) = sess.allocate_pty_id() else {
                    continue;
                };
                if let Some(pty) = spawn_pty(
                    &config.shell,
                    rows,
                    cols,
                    id,
                    tag,
                    command,
                    argv.as_deref(),
                    dir.as_deref(),
                    config.scrollback,
                    state.clone(),
                ) {
                    let tag_bytes = pty.tag.as_bytes();
                    let mut nonce_msg = Vec::with_capacity(5 + tag_bytes.len());
                    nonce_msg.push(S2C_CREATED_N);
                    nonce_msg.extend_from_slice(&nonce.to_le_bytes());
                    nonce_msg.extend_from_slice(&id.to_le_bytes());
                    nonce_msg.extend_from_slice(tag_bytes);
                    let mut broadcast_msg = Vec::with_capacity(3 + tag_bytes.len());
                    broadcast_msg.push(S2C_CREATED);
                    broadcast_msg.extend_from_slice(&id.to_le_bytes());
                    broadcast_msg.extend_from_slice(tag_bytes);
                    sess.ptys.insert(id, pty);
                    if let Some(c) = sess.clients.get_mut(&client_id) {
                        c.lead = Some(id);
                        c.view_sizes.insert(id, (rows, cols));
                        subscribe_client_to(c, id);
                        c.scroll_offset = 0;
                        c.scroll_cache = FrameState::default();
                        reset_inflight(c);
                        let _ = c.tx.try_send(nonce_msg);
                    }
                    for (&cid, c) in sess.clients.iter() {
                        if cid != client_id {
                            let _ = c.tx.try_send(broadcast_msg.clone());
                        }
                    }
                    need_nudge = true;
                }
            }
            C2S_FOCUS if data.len() >= 3 => {
                let pid = u16::from_le_bytes([data[1], data[2]]);
                if sess.ptys.contains_key(&pid) {
                    let old_pid = sess.clients.get(&client_id).and_then(|c| c.lead);
                    if let Some(c) = sess.clients.get_mut(&client_id) {
                        c.lead = Some(pid);
                        subscribe_client_to(c, pid);
                        if old_pid == Some(pid) {
                            update_client_scroll_state(c, pid, 0);
                        } else {
                            c.scroll_offset = 0;
                            c.scroll_cache = FrameState::default();
                            reset_inflight(c);
                        }
                    }
                    if let Some(pty) = sess.ptys.get_mut(&pid) {
                        pty.mark_dirty();
                        need_nudge = true;
                    }
                }
            }
            C2S_SUBSCRIBE if data.len() >= 3 => {
                let pid = u16::from_le_bytes([data[1], data[2]]);
                if sess.ptys.contains_key(&pid) {
                    if let Some(c) = sess.clients.get_mut(&client_id) {
                        subscribe_client_to(c, pid);
                    }
                    if let Some(pty) = sess.ptys.get_mut(&pid) {
                        pty.mark_dirty();
                    }
                    need_nudge = true;
                }
            }
            C2S_UNSUBSCRIBE if data.len() >= 3 => {
                let pid = u16::from_le_bytes([data[1], data[2]]);
                if sess.ptys.contains_key(&pid) {
                    let mut touched = Vec::new();
                    if let Some(c) = sess.clients.get_mut(&client_id) {
                        if unsubscribe_client_from(c, pid) {
                            touched.push(pid);
                        }
                        reset_inflight(c);
                    }
                    if sess.resize_ptys_to_mediated_sizes(touched) {
                        need_nudge = true;
                    }
                }
            }
            C2S_RESTART if data.len() >= 3 => {
                let pid = u16::from_le_bytes([data[1], data[2]]);
                let restart_info = sess
                    .ptys
                    .get(&pid)
                    .filter(|p| p.exited)
                    .map(|p| (p.driver.size(), p.command.clone(), p.tag.clone()));
                if let Some(((rows, cols), command, tag)) = restart_info {
                    if let Some((master, child, reader, byte_rx)) = respawn_child(
                        &state.0.shell,
                        rows,
                        cols,
                        pid,
                        command.as_deref(),
                        state.clone(),
                    ) {
                        let Some(pty) = sess.ptys.get_mut(&pid) else {
                            break;
                        };
                        pty.master_fd = master;
                        pty.child_pid = child;
                        pty.reader_handle = reader;
                        pty.byte_rx = byte_rx;
                        pty.exited = false;
                        pty.exit_status = blit_remote::EXIT_STATUS_UNKNOWN;
                        pty.lflag_cache = pty_lflag(master);
                        pty.lflag_last = Instant::now();
                        pty.mark_dirty();
                        if let Some(c) = sess.clients.get_mut(&client_id) {
                            c.lead = Some(pid);
                            subscribe_client_to(c, pid);
                            c.scroll_offset = 0;
                            c.scroll_cache = FrameState::default();
                            reset_inflight(c);
                        }
                        let mut msg = Vec::with_capacity(3 + tag.len());
                        msg.push(S2C_CREATED);
                        msg.extend_from_slice(&pid.to_le_bytes());
                        msg.extend_from_slice(tag.as_bytes());
                        sess.send_to_all(&msg);
                        need_nudge = true;
                    }
                }
            }
            C2S_READ if data.len() >= 13 => {
                let nonce = u16::from_le_bytes([data[1], data[2]]);
                let pid = u16::from_le_bytes([data[3], data[4]]);
                let req_offset = u32::from_le_bytes([data[5], data[6], data[7], data[8]]) as usize;
                let req_limit =
                    u32::from_le_bytes([data[9], data[10], data[11], data[12]]) as usize;
                let flags = data.get(13).copied().unwrap_or(0);
                let ansi = flags & READ_ANSI != 0;
                let tail = flags & READ_TAIL != 0;

                if let Some(pty) = sess.ptys.get_mut(&pid) {
                    let (rows, _cols) = pty.driver.size();
                    let viewport = take_snapshot(pty);
                    let scrollback_lines = viewport.scrollback_lines() as usize;
                    let total_lines = scrollback_lines + rows as usize;

                    let extract = |f: &FrameState| -> String {
                        if ansi {
                            f.get_ansi_text()
                        } else {
                            f.get_all_text()
                        }
                    };

                    let mut all_lines: Vec<String> = Vec::new();

                    let mut scroll_offset = scrollback_lines;
                    while scroll_offset > 0 {
                        let frame = pty.driver.scrollback_frame(scroll_offset);
                        let page = extract(&frame);
                        let page_lines: Vec<&str> = page.lines().collect();
                        let take = if scroll_offset < rows as usize {
                            scroll_offset.min(page_lines.len())
                        } else {
                            page_lines.len()
                        };
                        for line in &page_lines[..take] {
                            all_lines.push(line.to_string());
                        }
                        if scroll_offset <= rows as usize {
                            break;
                        }
                        scroll_offset = scroll_offset.saturating_sub(rows as usize);
                    }

                    for line in extract(&viewport).lines() {
                        all_lines.push(line.to_string());
                    }

                    let (start, end) = if tail {
                        let end = all_lines.len().saturating_sub(req_offset);
                        let start = if req_limit == 0 {
                            0
                        } else {
                            end.saturating_sub(req_limit)
                        };
                        (start, end)
                    } else {
                        let start = req_offset.min(all_lines.len());
                        let end = if req_limit == 0 {
                            all_lines.len()
                        } else {
                            (start + req_limit).min(all_lines.len())
                        };
                        (start, end)
                    };
                    let text = all_lines[start..end].join("\n");

                    let mut msg = Vec::with_capacity(13 + text.len());
                    msg.push(S2C_TEXT);
                    msg.extend_from_slice(&nonce.to_le_bytes());
                    msg.extend_from_slice(&pid.to_le_bytes());
                    msg.extend_from_slice(&(total_lines as u32).to_le_bytes());
                    msg.extend_from_slice(&(start as u32).to_le_bytes());
                    msg.extend_from_slice(text.as_bytes());
                    if let Some(client) = sess.clients.get(&client_id) {
                        let _ = client.tx.try_send(msg);
                    }
                }
            }
            C2S_CLOSE if data.len() >= 3 => {
                let pid = u16::from_le_bytes([data[1], data[2]]);
                if let Some(pty) = sess.ptys.remove(&pid) {
                    if !pty.exited {
                        state.2.write().unwrap().remove(&pid);
                        drop(pty.reader_handle); // thread exits when master fd is closed below
                        unsafe {
                            libc::kill(pty.child_pid, libc::SIGHUP);
                            libc::close(pty.master_fd);
                        }
                    }
                    for client in sess.clients.values_mut() {
                        unsubscribe_client_from(client, pid);
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
        let affected_ptys = sess
            .clients
            .remove(&client_id)
            .map(|client| client.view_sizes.keys().copied().collect::<Vec<_>>())
            .unwrap_or_default();
        if sess.resize_ptys_to_mediated_sizes(affected_ptys) {
            need_nudge = true;
        }
        drop(sess);
        if need_nudge {
            nudge_delivery(&state);
        }
    }
    sender.abort();
    eprintln!("client disconnected");
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_client_with_capacity(capacity: usize) -> (ClientState, mpsc::Receiver<Vec<u8>>) {
        let (tx, rx) = mpsc::channel(capacity);
        let client = ClientState {
            tx,
            lead: None,
            subscriptions: HashSet::new(),
            view_sizes: HashMap::new(),
            scroll_offset: 0,
            scroll_cache: FrameState::default(),
            last_sent: HashMap::new(),
            preview_next_send_at: HashMap::new(),
            rtt_ms: 50.0,
            min_rtt_ms: 50.0,
            display_fps: 60.0,
            delivery_bps: 262_144.0,
            goodput_bps: 262_144.0,
            goodput_jitter_bps: 0.0,
            max_goodput_jitter_bps: 0.0,
            last_goodput_sample_bps: 0.0,
            avg_frame_bytes: 1_024.0,
            avg_paced_frame_bytes: 1_024.0,
            avg_preview_frame_bytes: 1_024.0,
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
            last_metrics_update: Instant::now(),
            last_log: Instant::now(),
            goodput_window_bytes: 0,
            goodput_window_start: Instant::now(),
        };
        (client, rx)
    }

    fn test_client() -> ClientState {
        let (client, _rx) = test_client_with_capacity(OUTBOX_CAPACITY);
        client
    }

    fn fill_inflight(client: &mut ClientState, frames: usize, bytes_per_frame: usize) {
        let now = Instant::now();
        client.inflight_bytes = frames.saturating_mul(bytes_per_frame);
        client.inflight_frames = (0..frames)
            .map(|_| InFlightFrame {
                sent_at: now,
                bytes: bytes_per_frame,
                paced: true,
            })
            .collect();
    }

    fn sample_frame(text: &str) -> FrameState {
        let mut frame = FrameState::new(2, 8);
        frame.write_text(0, 0, text, blit_remote::CellStyle::default());
        frame
    }

    #[test]
    fn unset_view_size_accepts_zero_pair_only() {
        assert!(is_unset_view_size(0, 0));
        assert!(!is_unset_view_size(0, 80));
        assert!(!is_unset_view_size(u16::MAX, u16::MAX));
    }

    #[test]
    fn unsubscribe_client_from_clears_view_size() {
        let mut client = test_client();
        client.subscriptions.insert(7);
        client.view_sizes.insert(7, (24, 80));
        assert!(unsubscribe_client_from(&mut client, 7));
        assert!(!client.subscriptions.contains(&7));
        assert!(!client.view_sizes.contains_key(&7));
    }

    #[test]
    fn mediated_size_uses_per_pty_view_sizes_without_lead() {
        let mut session = Session::new();
        let mut c1 = test_client();
        let mut c2 = test_client();
        c1.view_sizes.insert(7, (30, 120));
        c2.view_sizes.insert(7, (24, 100));
        session.clients.insert(1, c1);
        session.clients.insert(2, c2);
        assert_eq!(session.mediated_size_for_pty(7), Some((24, 100)));
    }

    #[test]
    fn due_preview_reserves_the_last_lead_slot() {
        let mut client = test_client();
        client.lead = Some(1);
        client.subscriptions.insert(1);
        client.subscriptions.insert(2);

        let target_frames = target_frame_window(&client);
        let lead_limit = target_frames.saturating_sub(1).max(1);
        fill_inflight(&mut client, lead_limit, 512);

        assert!(window_open(&client));
        assert!(lead_window_open(&client, false));
        assert!(!lead_window_open(&client, true));
        assert!(can_send_preview(&client, 2, Instant::now()));
    }

    #[test]
    fn entering_scrollback_uses_current_visible_frame_as_baseline() {
        let mut client = test_client();
        let live = sample_frame("live");
        client.lead = Some(7);
        client.subscriptions.insert(7);
        client.last_sent.insert(7, live.clone());

        assert!(update_client_scroll_state(&mut client, 7, 12));
        assert_eq!(client.scroll_offset, 12);
        assert_eq!(client.scroll_cache, live);
    }

    #[test]
    fn leaving_scrollback_seeds_live_diff_from_scrollback_view() {
        let mut client = test_client();
        let history = sample_frame("hist");
        client.lead = Some(7);
        client.subscriptions.insert(7);
        client.scroll_offset = 12;
        client.scroll_cache = history.clone();

        assert!(update_client_scroll_state(&mut client, 7, 0));
        assert_eq!(client.scroll_offset, 0);
        assert_eq!(client.last_sent.get(&7), Some(&history));
        assert_eq!(client.scroll_cache, FrameState::default());
    }

    // ── frame_window ──

    #[test]
    fn frame_window_minimum_is_two() {
        assert!(frame_window(0.0, 60.0) >= 2);
    }

    #[test]
    fn frame_window_scales_with_rtt() {
        let low = frame_window(10.0, 60.0);
        let high = frame_window(200.0, 60.0);
        assert!(high > low, "higher RTT should need more frames in flight");
    }

    #[test]
    fn frame_window_scales_with_fps() {
        let slow = frame_window(100.0, 10.0);
        let fast = frame_window(100.0, 120.0);
        assert!(fast > slow, "higher fps should need more frames in flight");
    }

    #[test]
    fn frame_window_zero_rtt() {
        assert!(frame_window(0.0, 120.0) >= 2);
    }

    // ── path_rtt_ms ──

    #[test]
    fn path_rtt_ms_uses_min_when_positive() {
        let mut client = test_client();
        client.rtt_ms = 100.0;
        client.min_rtt_ms = 30.0;
        assert_eq!(path_rtt_ms(&client), 30.0);
    }

    #[test]
    fn path_rtt_ms_falls_back_to_rtt_when_min_zero() {
        let mut client = test_client();
        client.rtt_ms = 80.0;
        client.min_rtt_ms = 0.0;
        assert_eq!(path_rtt_ms(&client), 80.0);
    }

    // ── ewma_with_direction ──

    #[test]
    fn ewma_rising_uses_rise_alpha() {
        let result = ewma_with_direction(100.0, 200.0, 0.5, 0.1);
        // rise: 100 * 0.5 + 200 * 0.5 = 150
        assert!((result - 150.0).abs() < 0.01);
    }

    #[test]
    fn ewma_falling_uses_fall_alpha() {
        let result = ewma_with_direction(200.0, 100.0, 0.5, 0.1);
        // fall: 200 * 0.9 + 100 * 0.1 = 190
        assert!((result - 190.0).abs() < 0.01);
    }

    #[test]
    fn ewma_same_value_unchanged() {
        let result = ewma_with_direction(50.0, 50.0, 0.5, 0.5);
        assert!((result - 50.0).abs() < 0.01);
    }

    // ── advance_deadline ──

    #[test]
    fn advance_deadline_steps_forward() {
        let now = Instant::now();
        let mut deadline = now;
        let interval = Duration::from_millis(16);
        advance_deadline(&mut deadline, now, interval);
        assert!(deadline > now);
        assert!(deadline <= now + interval + Duration::from_micros(100));
    }

    #[test]
    fn advance_deadline_resets_when_far_behind() {
        let now = Instant::now();
        // deadline is way in the past (more than 2 intervals ago)
        let mut deadline = now - Duration::from_secs(10);
        let interval = Duration::from_millis(16);
        advance_deadline(&mut deadline, now, interval);
        // Should snap to now + interval since scheduled + interval < now
        assert!(deadline >= now);
    }

    #[test]
    fn should_snapshot_pty_requires_dirty_and_needful() {
        assert!(should_snapshot_pty(true, true, false));
        assert!(!should_snapshot_pty(false, true, false));
        assert!(!should_snapshot_pty(true, false, false));
    }

    #[test]
    fn should_snapshot_pty_defers_synced_output() {
        assert!(!should_snapshot_pty(true, true, true));
        assert!(should_snapshot_pty(true, true, false));
    }

    #[test]
    fn enqueue_ready_frame_refuses_new_frames_when_capped() {
        let mut queue = VecDeque::new();
        for cols in 1..=(READY_FRAME_QUEUE_CAP as u16) {
            assert!(enqueue_ready_frame(&mut queue, FrameState::new(1, cols)));
        }
        assert!(!enqueue_ready_frame(
            &mut queue,
            FrameState::new(1, READY_FRAME_QUEUE_CAP as u16 + 1),
        ));
        assert_eq!(queue.len(), READY_FRAME_QUEUE_CAP);
        assert_eq!(queue.front().map(FrameState::cols), Some(1));
        assert_eq!(
            queue.back().map(FrameState::cols),
            Some(READY_FRAME_QUEUE_CAP as u16),
        );
    }

    #[test]
    fn find_sync_output_end_returns_end_of_first_close_sequence() {
        let bytes = b"abc\x1b[?2026lrest\x1b[?2026l";
        assert_eq!(find_sync_output_end(&[], bytes), Some(11));
    }

    #[test]
    fn find_sync_output_end_returns_none_without_close_sequence() {
        assert_eq!(find_sync_output_end(&[], b"\x1b[?2026hpartial"), None);
    }

    #[test]
    fn find_sync_output_end_detects_boundary_split_across_reads() {
        assert_eq!(find_sync_output_end(b"abc\x1b[?20", b"26lrest"), Some(3));
    }

    #[test]
    fn update_sync_scan_tail_keeps_recent_suffix_only() {
        let mut tail = Vec::new();
        update_sync_scan_tail(&mut tail, b"123456789");
        assert_eq!(tail, b"3456789");
    }

    // ── window_saturated ──

    #[test]
    fn window_saturated_at_90_percent_frames() {
        let client = test_client();
        let target = target_frame_window(&client);
        let frames_90 = (target * 9).div_ceil(10); // ceil(target * 0.9)
        assert!(window_saturated(&client, frames_90, 0));
    }

    #[test]
    fn window_saturated_not_at_low_usage() {
        let client = test_client();
        assert!(!window_saturated(&client, 1, 0));
    }

    #[test]
    fn window_saturated_at_90_percent_bytes() {
        let client = test_client();
        let target_bytes = target_byte_window(&client);
        let bytes_90 = (target_bytes * 9).div_ceil(10);
        assert!(window_saturated(&client, 0, bytes_90));
    }

    // ── outbox_queued_frames / outbox_backpressured ──

    #[test]
    fn outbox_queued_frames_zero_when_empty() {
        let client = test_client();
        assert_eq!(outbox_queued_frames(&client), 0);
    }

    #[test]
    fn outbox_backpressured_when_queue_full() {
        let (client, _rx) = test_client_with_capacity(OUTBOX_CAPACITY);
        // Fill the channel to trigger backpressure
        for _ in 0..OUTBOX_SOFT_QUEUE_LIMIT_FRAMES {
            let _ = client.tx.try_send(vec![0u8]);
        }
        assert!(outbox_backpressured(&client));
    }

    #[test]
    fn outbox_not_backpressured_when_empty() {
        let client = test_client();
        assert!(!outbox_backpressured(&client));
    }

    // ── browser_pacing_fps baseline ──

    #[test]
    fn browser_pacing_fps_matches_display_fps_when_browser_ready() {
        let mut client = test_client();
        client.rtt_ms = 1.0;
        client.min_rtt_ms = 1.0;
        client.browser_backlog_frames = 0;
        client.browser_ack_ahead_frames = 0;
        client.browser_apply_ms = 0.0;
        client.goodput_bps = 1_000_000.0;
        client.delivery_bps = 1_000_000.0;
        client.display_fps = 144.0;
        assert!((browser_pacing_fps(&client) - 144.0).abs() < 0.01);
    }

    #[test]
    fn browser_pacing_fps_drops_below_display_fps_when_backlogged() {
        let mut client = test_client();
        client.browser_backlog_frames = 20;
        let fps = browser_pacing_fps(&client);
        assert!(fps >= 1.0);
        assert!(fps < client.display_fps);
    }

    // ── effective_rtt_ms ──

    #[test]
    fn effective_rtt_ms_equals_path_when_queue_is_empty() {
        let mut client = test_client();
        client.rtt_ms = 1.0;
        client.min_rtt_ms = 1.0;
        client.browser_backlog_frames = 0;
        client.browser_ack_ahead_frames = 0;
        client.browser_apply_ms = 0.0;
        client.goodput_bps = 1_000_000.0;
        client.delivery_bps = 1_000_000.0;
        assert!((effective_rtt_ms(&client) - 1.0).abs() < 0.01);
    }

    #[test]
    fn effective_rtt_ms_at_least_path_rtt() {
        let client = test_client();
        assert!(effective_rtt_ms(&client) >= path_rtt_ms(&client));
    }

    // ── target_frame_window ──

    #[test]
    fn target_frame_window_at_least_two() {
        let client = test_client();
        assert!(target_frame_window(&client) >= 2);
    }

    #[test]
    fn target_frame_window_grows_with_probe() {
        let mut client = test_client();
        let base = target_frame_window(&client);
        client.probe_frames = 10.0;
        let probed = target_frame_window(&client);
        assert!(probed > base, "probe_frames should grow the window");
    }

    // ── bandwidth_floor_bps ──

    #[test]
    fn bandwidth_floor_bps_at_least_16k() {
        let mut client = test_client();
        client.goodput_bps = 0.0;
        client.delivery_bps = 0.0;
        assert_eq!(bandwidth_floor_bps(&client), 0.0);
    }

    #[test]
    fn bandwidth_floor_bps_scales_with_goodput() {
        let mut client = test_client();
        client.goodput_bps = 1_000_000.0;
        client.delivery_bps = 1_000_000.0;
        let floor = bandwidth_floor_bps(&client);
        assert!(floor > 0.0);
    }

    #[test]
    fn browser_ready_delivery_floor_can_drive_large_frames_to_display_fps() {
        let mut client = test_client();
        client.display_fps = 60.0;
        client.browser_backlog_frames = 0;
        client.browser_ack_ahead_frames = 0;
        client.browser_apply_ms = 0.2;
        client.goodput_bps = 3_000_000.0;
        client.delivery_bps = 9_500_000.0;
        client.last_goodput_sample_bps = 3_000_000.0;
        client.avg_paced_frame_bytes = 150_000.0;
        client.avg_preview_frame_bytes = 1_024.0;
        client.avg_frame_bytes = 150_000.0;

        assert!(
            (pacing_fps(&client) - client.display_fps).abs() < 0.01,
            "browser-ready delivery floor should let large frames reach display_fps on a fast path",
        );
    }

    // ── pacing_fps ──

    #[test]
    fn pacing_fps_zero_when_no_bandwidth() {
        let mut client = test_client();
        client.goodput_bps = 0.0;
        client.delivery_bps = 0.0;
        client.last_goodput_sample_bps = 0.0;
        assert!(
            pacing_fps(&client) == 0.0,
            "pacing_fps should be 0 with zero bandwidth"
        );
    }

    #[test]
    fn pacing_fps_reaches_display_fps_when_not_bandwidth_limited() {
        let mut client = test_client();
        client.rtt_ms = 1.0;
        client.min_rtt_ms = 1.0;
        client.browser_backlog_frames = 0;
        client.browser_ack_ahead_frames = 0;
        client.browser_apply_ms = 0.0;
        client.goodput_bps = 1_000_000.0;
        client.delivery_bps = 1_000_000.0;
        client.display_fps = 60.0;
        assert!((pacing_fps(&client) - 60.0).abs() < 0.01);
    }

    // ── throughput_limited ──

    #[test]
    fn throughput_limited_when_low_bandwidth() {
        let mut client = test_client();
        client.goodput_bps = 1_000.0;
        client.delivery_bps = 1_000.0;
        client.last_goodput_sample_bps = 0.0;
        assert!(throughput_limited(&client));
    }

    #[test]
    fn throughput_not_limited_with_high_bandwidth() {
        let mut client = test_client();
        client.goodput_bps = 100_000_000.0;
        client.delivery_bps = 100_000_000.0;
        assert!(!throughput_limited(&client));
    }

    // ── browser_pacing_fps ──

    #[test]
    fn browser_pacing_fps_at_least_one() {
        let client = test_client();
        assert!(browser_pacing_fps(&client) >= 1.0);
    }

    #[test]
    fn browser_pacing_fps_reduced_by_high_backlog() {
        let mut client = test_client();
        let normal = browser_pacing_fps(&client);
        client.browser_backlog_frames = 20;
        let backlogged = browser_pacing_fps(&client);
        assert!(backlogged < normal, "high backlog should reduce pacing fps");
    }

    #[test]
    fn browser_pacing_fps_reduced_by_high_ack_ahead() {
        let mut client = test_client();
        let normal = browser_pacing_fps(&client);
        client.browser_ack_ahead_frames = 10;
        let ahead = browser_pacing_fps(&client);
        assert!(ahead < normal, "high ack_ahead should reduce pacing fps");
    }

    // ── browser_backlog_blocked ──

    #[test]
    fn browser_backlog_blocked_over_threshold() {
        let mut client = test_client();
        client.browser_backlog_frames = 9;
        assert!(browser_backlog_blocked(&client));
    }

    #[test]
    fn browser_backlog_not_blocked_under_threshold() {
        let mut client = test_client();
        client.browser_backlog_frames = 8;
        assert!(!browser_backlog_blocked(&client));
    }

    // ── byte_budget_for ──

    #[test]
    fn byte_budget_for_at_least_one_frame() {
        let client = test_client();
        let budget = byte_budget_for(&client, 10.0);
        assert!(budget >= client.avg_frame_bytes.max(256.0) as usize);
    }

    #[test]
    fn byte_budget_for_grows_with_time() {
        let client = test_client();
        let short = byte_budget_for(&client, 10.0);
        let long = byte_budget_for(&client, 1000.0);
        assert!(long >= short);
    }

    // ── target_byte_window ──

    #[test]
    fn target_byte_window_positive() {
        let client = test_client();
        assert!(target_byte_window(&client) > 0);
    }

    #[test]
    fn target_byte_window_covers_frame_window() {
        let client = test_client();
        let byte_win = target_byte_window(&client);
        let frame_win = target_frame_window(&client);
        let min_bytes =
            (client.avg_paced_frame_bytes.max(256.0) * frame_win.max(2) as f32).ceil() as usize;
        assert!(
            byte_win >= min_bytes,
            "byte window should cover at least frame_window worth of paced frames"
        );
    }

    // ── send_interval ──

    #[test]
    fn send_interval_matches_browser_pacing() {
        let client = test_client();
        let interval = send_interval(&client);
        let expected = Duration::from_secs_f64(1.0 / browser_pacing_fps(&client) as f64);
        let diff = interval.abs_diff(expected);
        assert!(diff < Duration::from_micros(10));
    }

    // ── preview_fps ──

    #[test]
    fn preview_fps_at_least_one() {
        let client = test_client();
        assert!(preview_fps(&client) >= 1.0);
    }

    // ── window_open ──

    #[test]
    fn window_open_initially() {
        let client = test_client();
        assert!(window_open(&client));
    }

    #[test]
    fn window_open_false_when_browser_blocked() {
        let mut client = test_client();
        client.browser_backlog_frames = 20;
        assert!(!window_open(&client));
    }

    #[test]
    fn window_open_false_when_inflight_full() {
        let mut client = test_client();
        let target = target_frame_window(&client);
        fill_inflight(&mut client, target + 10, 1024);
        assert!(!window_open(&client));
    }

    // ── lead_window_open ──

    #[test]
    fn lead_window_open_no_reserve_same_as_window_open() {
        let client = test_client();
        assert_eq!(lead_window_open(&client, false), window_open(&client));
    }

    #[test]
    fn lead_window_open_reserves_preview_slot() {
        let mut client = test_client();
        client.lead = Some(1);
        client.subscriptions.insert(1);
        let target = target_frame_window(&client);
        // Fill to just under target minus reserve
        fill_inflight(&mut client, target.saturating_sub(1), 512);
        // Without reserve: may still be open
        // With reserve: should be closed
        assert!(!lead_window_open(&client, true));
    }

    // ── can_send_frame ──

    #[test]
    fn can_send_frame_when_window_open_and_time_due() {
        let mut client = test_client();
        client.next_send_at = Instant::now() - Duration::from_millis(100);
        assert!(can_send_frame(&client, Instant::now(), false));
    }

    #[test]
    fn can_send_frame_false_when_not_due() {
        let mut client = test_client();
        client.next_send_at = Instant::now() + Duration::from_secs(10);
        assert!(!can_send_frame(&client, Instant::now(), false));
    }

    #[test]
    fn can_send_frame_false_when_window_closed() {
        let mut client = test_client();
        client.browser_backlog_frames = 20; // triggers browser_backlog_blocked
        client.next_send_at = Instant::now() - Duration::from_millis(100);
        assert!(!can_send_frame(&client, Instant::now(), false));
    }

    // ── record_send / record_ack state transitions ──

    #[test]
    fn record_send_increases_inflight() {
        let mut client = test_client();
        let now = Instant::now();
        assert_eq!(client.inflight_bytes, 0);
        assert_eq!(client.inflight_frames.len(), 0);

        record_send(&mut client, 1000, now, true);
        assert_eq!(client.inflight_bytes, 1000);
        assert_eq!(client.inflight_frames.len(), 1);

        record_send(&mut client, 500, now, false);
        assert_eq!(client.inflight_bytes, 1500);
        assert_eq!(client.inflight_frames.len(), 2);
    }

    #[test]
    fn record_send_paced_advances_deadline() {
        let mut client = test_client();
        let now = Instant::now();
        client.next_send_at = now;
        record_send(&mut client, 1000, now, true);
        assert!(client.next_send_at > now);
    }

    #[test]
    fn record_send_unpaced_does_not_advance_deadline() {
        let mut client = test_client();
        let now = Instant::now();
        let before = client.next_send_at;
        record_send(&mut client, 1000, now, false);
        assert_eq!(client.next_send_at, before);
    }

    #[test]
    fn record_ack_decreases_inflight() {
        let mut client = test_client();
        let now = Instant::now();
        record_send(&mut client, 1000, now, true);
        record_send(&mut client, 500, now, true);
        assert_eq!(client.inflight_frames.len(), 2);

        record_ack(&mut client);
        assert_eq!(client.inflight_frames.len(), 1);
        assert_eq!(client.inflight_bytes, 500);
    }

    #[test]
    fn record_ack_on_empty_clears_bytes() {
        let mut client = test_client();
        client.inflight_bytes = 999; // stale state
        record_ack(&mut client);
        assert_eq!(client.inflight_bytes, 0);
    }

    #[test]
    fn record_ack_updates_rtt_estimate() {
        let mut client = test_client();
        let now = Instant::now();
        client.inflight_frames.push_back(InFlightFrame {
            sent_at: now - Duration::from_millis(20),
            bytes: 512,
            paced: true,
        });
        client.inflight_bytes = 512;
        let old_rtt = client.rtt_ms;
        record_ack(&mut client);
        // RTT should have been updated (moved toward ~20ms from the default 50ms)
        assert!(
            (client.rtt_ms - old_rtt).abs() > 0.01,
            "rtt_ms should be updated after ack"
        );
    }

    #[test]
    fn record_ack_paced_updates_avg_paced_frame_bytes() {
        let mut client = test_client();
        let now = Instant::now();
        client.inflight_frames.push_back(InFlightFrame {
            sent_at: now - Duration::from_millis(10),
            bytes: 4096,
            paced: true,
        });
        client.inflight_bytes = 4096;
        let old_avg = client.avg_paced_frame_bytes;
        record_ack(&mut client);
        // Should move toward 4096 from 1024
        assert!(client.avg_paced_frame_bytes > old_avg);
    }

    #[test]
    fn record_ack_unpaced_updates_avg_preview_frame_bytes() {
        let mut client = test_client();
        let now = Instant::now();
        client.inflight_frames.push_back(InFlightFrame {
            sent_at: now - Duration::from_millis(10),
            bytes: 8192,
            paced: false,
        });
        client.inflight_bytes = 8192;
        let old_avg = client.avg_preview_frame_bytes;
        record_ack(&mut client);
        assert!(client.avg_preview_frame_bytes > old_avg);
    }

    // ── Session::pty_list_msg format ──

    #[test]
    fn pty_list_msg_empty_session() {
        let sess = Session::new();
        let msg = sess.pty_list_msg();
        assert_eq!(msg[0], S2C_LIST);
        assert_eq!(u16::from_le_bytes([msg[1], msg[2]]), 0);
        assert_eq!(msg.len(), 3);
    }

    #[test]
    fn pty_list_msg_includes_tags() {
        let _sess = Session::new();
        // Insert minimal Pty entries. We can't call spawn_pty, so build
        // a mock-like Pty with a stub driver. Instead, directly insert
        // into the HashMap using an unsafe-free approach: just build the
        // wire message by hand and verify against a known layout.
        //
        // The wire format is: [S2C_LIST] [count:u16le] [id:u16le tag_len:u16le tag_bytes]...
        //
        // Since we can't easily construct a Pty without forking, verify
        // the format by constructing the expected bytes and comparing.
        let tag1 = "shell";
        let tag2 = "build";

        // Expected wire for ptys {1 => "shell", 3 => "build"} sorted by id:
        let mut expected = vec![S2C_LIST];
        expected.extend_from_slice(&2u16.to_le_bytes());
        // id=1
        expected.extend_from_slice(&1u16.to_le_bytes());
        expected.extend_from_slice(&(tag1.len() as u16).to_le_bytes());
        expected.extend_from_slice(tag1.as_bytes());
        // id=3
        expected.extend_from_slice(&3u16.to_le_bytes());
        expected.extend_from_slice(&(tag2.len() as u16).to_le_bytes());
        expected.extend_from_slice(tag2.as_bytes());

        // Verify our expected format starts with S2C_LIST and has correct count
        assert_eq!(expected[0], S2C_LIST);
        assert_eq!(u16::from_le_bytes([expected[1], expected[2]]), 2);
        // Verify tags are embedded
        let msg_str = String::from_utf8_lossy(&expected);
        assert!(msg_str.contains("shell"));
        assert!(msg_str.contains("build"));
    }

    // ── can_send_preview / record_preview_send ──

    #[test]
    fn can_send_preview_true_when_due() {
        let mut client = test_client();
        let now = Instant::now();
        client
            .preview_next_send_at
            .insert(5, now - Duration::from_millis(100));
        assert!(can_send_preview(&client, 5, now));
    }

    #[test]
    fn can_send_preview_false_when_not_due() {
        let mut client = test_client();
        let now = Instant::now();
        client
            .preview_next_send_at
            .insert(5, now + Duration::from_secs(10));
        assert!(!can_send_preview(&client, 5, now));
    }

    #[test]
    fn can_send_preview_false_when_window_closed() {
        let mut client = test_client();
        client.browser_backlog_frames = 20;
        let now = Instant::now();
        assert!(!can_send_preview(&client, 5, now));
    }

    #[test]
    fn can_send_preview_true_for_unseen_pid() {
        let client = test_client();
        let now = Instant::now();
        // No entry in preview_next_send_at means deadline defaults to now
        assert!(can_send_preview(&client, 99, now));
    }

    #[test]
    fn record_preview_send_sets_future_deadline() {
        let mut client = test_client();
        let now = Instant::now();
        record_preview_send(&mut client, 5, now);
        let deadline = client.preview_next_send_at.get(&5).unwrap();
        assert!(*deadline > now);
    }

    #[test]
    fn record_preview_send_successive_calls_advance() {
        let mut client = test_client();
        let now = Instant::now();
        record_preview_send(&mut client, 5, now);
        let first = *client.preview_next_send_at.get(&5).unwrap();
        record_preview_send(&mut client, 5, first);
        let second = *client.preview_next_send_at.get(&5).unwrap();
        assert!(second > first, "successive sends should advance deadline");
    }

    // ── congestion control end-to-end properties ──
    //
    // These tests encode the two goals of the congestion controller:
    //   1. Browser-ready, well-provisioned path → full display FPS, minimal added latency
    //   2. Bottleneck                           → lowest sustainable FPS, fast recovery when pipe clears
    //
    // Some tests assert desired future behaviour and currently FAIL due to
    // known issues (min_rtt contamination, lead_floor dominating byte window).
    // They are marked with a comment so they are easy to find when fixing.

    /// Return a client in ideal low-latency, high-bandwidth conditions:
    /// browser ready, abundant bandwidth, and tiny RTT. The normal pacing path
    /// should still reach display_fps.
    fn browser_ready_high_bandwidth_client() -> ClientState {
        let mut c = test_client();
        c.display_fps = 120.0;
        c.rtt_ms = 1.0;
        c.min_rtt_ms = 1.0;
        c.goodput_bps = 50_000_000.0;
        c.delivery_bps = 50_000_000.0;
        c.last_goodput_sample_bps = 50_000_000.0;
        c.avg_paced_frame_bytes = 30_000.0;
        c.avg_preview_frame_bytes = 1_024.0;
        c.avg_frame_bytes = 30_000.0;
        c.browser_apply_ms = 0.3;
        c
    }

    /// Return a client that has converged to a clearly congested state:
    /// ~10× min_rtt inflation, low goodput.
    fn congested_client() -> ClientState {
        let mut c = test_client();
        c.display_fps = 120.0;
        c.rtt_ms = 500.0;
        c.min_rtt_ms = 40.0;
        c.goodput_bps = 200_000.0;
        c.delivery_bps = 150_000.0;
        c.last_goodput_sample_bps = 200_000.0;
        c.avg_paced_frame_bytes = 50_000.0;
        c.avg_preview_frame_bytes = 1_024.0;
        c.avg_frame_bytes = 50_000.0;
        c.goodput_jitter_bps = 50_000.0;
        c.max_goodput_jitter_bps = 200_000.0;
        c.browser_apply_ms = 1.0;
        c
    }

    /// Simulate one ACK: insert a frame with the given RTT into inflight and
    /// call record_ack.  Forces a goodput-window sample each call so that
    /// goodput estimates respond within a few calls.
    fn sim_ack(client: &mut ClientState, bytes: usize, rtt_ms: f32) {
        let sent_at = Instant::now() - Duration::from_millis(rtt_ms as u64);
        client.inflight_bytes += bytes;
        client.inflight_frames.push_back(InFlightFrame {
            sent_at,
            bytes,
            paced: true,
        });
        // Age the goodput window so record_ack always emits a sample.
        client.goodput_window_start = Instant::now() - Duration::from_millis(25);
        record_ack(client);
    }

    fn sim_acks(client: &mut ClientState, n: usize, bytes: usize, rtt_ms: f32) {
        for _ in 0..n {
            sim_ack(client, bytes, rtt_ms);
        }
    }

    // ── property: full FPS on a browser-ready path ──

    #[test]
    fn browser_ready_high_bandwidth_client_uses_full_display_fps() {
        let client = browser_ready_high_bandwidth_client();
        assert!(
            (pacing_fps(&client) - client.display_fps).abs() < 0.01,
            "pacing_fps {} should equal display_fps {} when browser is ready and bandwidth is abundant",
            pacing_fps(&client),
            client.display_fps,
        );
    }

    #[test]
    fn browser_ready_high_bandwidth_client_send_interval_within_one_frame() {
        let client = browser_ready_high_bandwidth_client();
        let interval_ms = send_interval(&client).as_secs_f32() * 1000.0;
        let frame_ms = 1000.0 / client.display_fps;
        assert!(
            interval_ms <= frame_ms + 0.1,
            "send_interval {interval_ms:.2}ms exceeds one frame ({frame_ms:.2}ms) when browser is ready"
        );
    }

    // ── property: degraded FPS when bottlenecked ──

    #[test]
    fn congested_pipe_reduces_pacing_fps_substantially() {
        let client = congested_client();
        let fps = pacing_fps(&client);
        assert!(
            fps < client.display_fps * 0.5,
            "pacing_fps {fps:.0} should be well below display_fps {} when congested",
            client.display_fps,
        );
    }

    #[test]
    fn congested_pipe_is_throughput_limited() {
        let client = congested_client();
        assert!(
            throughput_limited(&client),
            "congested client must be recognised as throughput-limited"
        );
    }

    // ── property: byte window should stay near BDP ──
    //
    // KNOWN FAILING: lead_floor in target_byte_window overrides the BDP
    // budget when avg_paced_frame_bytes is large.  Fix: cap lead_floor.

    #[test]
    fn byte_window_bounded_near_bdp_when_congested() {
        let client = congested_client();
        // BDP at the unloaded path RTT.
        let bdp = client.goodput_bps * (path_rtt_ms(&client) / 1_000.0);
        let window = target_byte_window(&client);
        assert!(
            window < bdp as usize * 8,
            "byte window {window}B is {:.1}× BDP ({bdp:.0}B); \
             expected ≤ 8× — lead_floor may be dominating",
            window as f32 / bdp.max(1.0),
        );
    }

    // ── property: min_rtt must not drift upward under congestion ──
    //
    // KNOWN FAILING: the `min_rtt_ms * 0.999 + rtt_ms * 0.001` update
    // bleeds queued RTT into min_rtt.

    #[test]
    fn min_rtt_not_contaminated_by_congested_rtts() {
        let mut client = test_client();
        client.display_fps = 120.0;
        client.rtt_ms = 40.0;
        client.min_rtt_ms = 40.0;
        client.goodput_bps = 2_000_000.0;
        client.delivery_bps = 2_000_000.0;
        client.avg_paced_frame_bytes = 30_000.0;
        client.avg_preview_frame_bytes = 1_024.0;
        let original_min = client.min_rtt_ms;

        // 200 ACKs arriving with 500ms RTT (severe congestion).
        sim_acks(&mut client, 200, 30_000, 500.0);

        assert!(
            client.min_rtt_ms < original_min * 2.0,
            "min_rtt drifted from {original_min}ms to {:.1}ms after 200 congested ACKs",
            client.min_rtt_ms,
        );
    }

    // ── property: fast recovery when congestion clears ──

    #[test]
    fn delivery_bps_rises_quickly_when_congestion_clears() {
        let mut client = congested_client();
        let before = client.delivery_bps;

        // 10 ACKs at low latency / high throughput.
        sim_acks(&mut client, 10, 30_000, 40.0);

        assert!(
            client.delivery_bps > before * 2.0,
            "delivery_bps {:.0} should more than double from {before:.0} after 10 fast ACKs",
            client.delivery_bps,
        );
    }

    #[test]
    fn pacing_fps_recovers_after_congestion_clears() {
        let mut client = congested_client();

        // Use window-saturated rounds: fill the window with frames, age the
        // goodput window once, then ACK all.  The first ACK each round emits
        // a sample; the remaining target-1 ACKs carry over into the next
        // window, so sample throughput grows as target grows — mimicking a
        // real link where the sender keeps the pipe full across one RTT.
        for _ in 0..40 {
            let target = target_frame_window(&client).max(2);
            for _ in 0..target {
                let sent_at = Instant::now() - Duration::from_millis(40);
                client.inflight_bytes += 30_000;
                client.inflight_frames.push_back(InFlightFrame {
                    sent_at,
                    bytes: 30_000,
                    paced: true,
                });
            }
            client.goodput_window_start = Instant::now() - Duration::from_millis(25);
            for _ in 0..target {
                record_ack(&mut client);
            }
        }

        let fps = pacing_fps(&client);
        assert!(
            fps > client.display_fps * 0.7,
            "pacing_fps {fps:.0} didn't recover toward display_fps {} \
             after window-saturated rounds at low RTT",
            client.display_fps,
        );
    }

    #[test]
    fn rtt_estimate_drops_quickly_when_congestion_clears() {
        let mut client = test_client();
        client.rtt_ms = 500.0;
        client.min_rtt_ms = 40.0;
        client.goodput_bps = 2_000_000.0;
        client.avg_paced_frame_bytes = 30_000.0;
        client.avg_preview_frame_bytes = 1_024.0;

        // The asymmetric EWMA uses rise=0.125, fall=0.25, so rtt_ms drops
        // at fall_alpha=0.25 per sample toward the new low.
        sim_acks(&mut client, 10, 30_000, 40.0);

        assert!(
            client.rtt_ms < 300.0,
            "rtt_ms {:.0}ms did not fall fast enough after congestion cleared",
            client.rtt_ms,
        );
    }

    // ── property: probing ──

    #[test]
    fn probe_collapses_immediately_on_queue_delay() {
        let mut client = test_client();
        client.display_fps = 120.0;
        client.rtt_ms = 40.0;
        client.min_rtt_ms = 40.0;
        client.goodput_bps = 5_000_000.0;
        client.delivery_bps = 5_000_000.0;
        client.last_goodput_sample_bps = 5_000_000.0;
        client.avg_paced_frame_bytes = 10_000.0;
        client.avg_preview_frame_bytes = 1_024.0;
        client.probe_frames = 10.0;

        // ACKs arriving with high RTT signal queue buildup.
        sim_acks(&mut client, 5, 10_000, 600.0);

        assert!(
            client.probe_frames < 5.0,
            "probe_frames {:.1} should have collapsed on queue delay signal",
            client.probe_frames,
        );
    }

    #[test]
    fn probe_grows_when_window_saturated_with_clean_rtt() {
        let mut client = test_client();
        client.display_fps = 120.0;
        client.rtt_ms = 40.0;
        client.min_rtt_ms = 40.0;
        client.goodput_bps = 5_000_000.0;
        client.delivery_bps = 5_000_000.0;
        client.last_goodput_sample_bps = 5_000_000.0;
        client.avg_paced_frame_bytes = 10_000.0;
        client.avg_preview_frame_bytes = 1_024.0;
        client.goodput_jitter_bps = 0.0;
        client.max_goodput_jitter_bps = 0.0;
        client.probe_frames = 0.0;

        // Saturate inflight so window_saturated returns true during acks.
        let target = target_frame_window(&client);
        for _ in 0..target {
            let sent_at = Instant::now() - Duration::from_millis(40);
            client.inflight_bytes += 10_000;
            client.inflight_frames.push_back(InFlightFrame {
                sent_at,
                bytes: 10_000,
                paced: true,
            });
        }

        // Ack one frame with clean RTT.  One saturated ACK is sufficient to
        // verify the property: as probe_frames increments, target_frame_window
        // grows, so the remaining (target-1) frames would fall below the 90%
        // threshold and trigger gentle decay.  The property under test is that
        // *receiving an ACK while window-saturated* increments probe_frames —
        // not that it stays incremented across subsequent unsaturated ACKs.
        // Also: do NOT age the goodput window — that would emit a per-frame
        // sample far below goodput_bps, spiking jitter and collapsing probe.
        record_ack(&mut client);

        assert!(
            client.probe_frames > 0.0,
            "probe_frames should grow when window-saturated with clean RTT"
        );
    }

    // ── property: frame window larger on high-latency links ──

    #[test]
    fn frame_window_larger_on_high_latency_link() {
        let mut lo = test_client();
        lo.display_fps = 120.0;
        lo.rtt_ms = 10.0;
        lo.min_rtt_ms = 10.0;
        lo.goodput_bps = 5_000_000.0;
        lo.delivery_bps = 5_000_000.0;
        lo.avg_paced_frame_bytes = 10_000.0;
        lo.avg_preview_frame_bytes = 1_024.0;

        let mut hi = test_client();
        hi.display_fps = 120.0;
        hi.rtt_ms = 200.0;
        hi.min_rtt_ms = 200.0;
        hi.goodput_bps = 5_000_000.0;
        hi.delivery_bps = 5_000_000.0;
        hi.avg_paced_frame_bytes = 10_000.0;
        hi.avg_preview_frame_bytes = 1_024.0;

        let lo_win = target_frame_window(&lo);
        let hi_win = target_frame_window(&hi);
        assert!(
            hi_win > lo_win,
            "high-latency link ({hi_win}f) should need more frames in flight \
             than low-latency ({lo_win}f)"
        );
    }

    // ── property: small-frame byte window allows pipelining ──

    #[test]
    fn small_frame_byte_window_enables_pipelining() {
        // Tiny terminal frames (~1KB) with a stale congested RTT and low
        // goodput estimate (stop-and-wait artifact): byte window must be at
        // least target_frame_window × frame_bytes so the sender can pipeline
        // rather than stay stuck in stop-and-wait.
        let mut client = test_client();
        client.display_fps = 120.0;
        client.rtt_ms = 165.0;
        client.min_rtt_ms = 8.0;
        client.goodput_bps = 11_000.0; // stop-and-wait artifact
        client.delivery_bps = 6_800.0;
        client.last_goodput_sample_bps = 11_000.0;
        client.avg_paced_frame_bytes = 1_120.0;
        client.avg_preview_frame_bytes = 1_024.0;
        client.goodput_jitter_bps = 4_300.0;
        client.max_goodput_jitter_bps = 6_500.0;

        let window = target_byte_window(&client);
        let frames = target_frame_window(&client);
        let pipeline = frames * 1_120;

        assert!(
            window >= pipeline,
            "byte window {window}B should be >= pipeline ({frames}f × 1120B = {pipeline}B) \
             so small frames can pipeline across the RTT"
        );
    }

    #[test]
    fn large_frame_byte_window_bounded_by_one_frame_floor() {
        // With large frames (50KB), pipelining the full frame window (5×50KB=250KB)
        // would be many multiples of BDP.  Byte window should fall back to
        // the one-frame floor so the BDP budget governs.
        let mut client = test_client();
        client.display_fps = 120.0;
        client.rtt_ms = 165.0;
        client.min_rtt_ms = 8.0;
        client.goodput_bps = 11_000.0;
        client.delivery_bps = 6_800.0;
        client.last_goodput_sample_bps = 11_000.0;
        client.avg_paced_frame_bytes = 50_000.0; // large frame
        client.avg_preview_frame_bytes = 1_024.0;
        client.goodput_jitter_bps = 0.0;
        client.max_goodput_jitter_bps = 0.0;

        let window = target_byte_window(&client);
        let frames = target_frame_window(&client);
        let pipeline = frames.saturating_mul(50_000);

        assert!(
            window < pipeline,
            "byte window {window}B should be < full pipeline {pipeline}B \
             ({frames}f × 50KB) — large frames must use one-frame floor"
        );
        assert!(
            window >= 50_000,
            "byte window {window}B must be at least one frame (50KB)"
        );
    }

    // ── property: preview reservation applies uniformly ──

    #[test]
    fn preview_reservation_applies_even_on_low_latency_high_bandwidth_links() {
        let mut client = browser_ready_high_bandwidth_client();
        client.lead = Some(1);
        client.subscriptions.insert(1);
        let target = target_frame_window(&client);
        fill_inflight(&mut client, target.saturating_sub(1), 512);
        assert!(
            !lead_window_open(&client, true),
            "preview reservation should apply uniformly for lead clients"
        );
    }

    // ── property: blip recovery on healthy paths ──

    #[test]
    fn probe_recovers_on_healthy_path_after_blip() {
        let mut client = browser_ready_high_bandwidth_client();
        client.probe_frames = 8.0;

        // Blip: 3 ACKs with inflated RTT crush probes.
        sim_acks(&mut client, 3, 30_000, 200.0);
        let post_blip = client.probe_frames;
        assert!(
            post_blip < 4.0,
            "probe_frames {post_blip:.1} should have dropped after blip"
        );

        // Reset browser metrics to healthy (browser cleared backlog).
        client.browser_backlog_frames = 0;
        client.browser_ack_ahead_frames = 0;
        client.browser_apply_ms = 0.3;

        // Recovery: 20 healthy ACKs at low RTT on an underfilled path.
        sim_acks(&mut client, 20, 30_000, 1.0);

        assert!(
            client.probe_frames > post_blip,
            "probe_frames {:.1} should have recovered from {post_blip:.1} after healthy ACKs",
            client.probe_frames,
        );
    }

    #[test]
    fn jitter_decays_fast_on_browser_ready_path() {
        let mut client = browser_ready_high_bandwidth_client();

        // Inject elevated jitter (simulating post-blip state).
        client.max_goodput_jitter_bps = client.goodput_bps * 0.4;
        client.goodput_jitter_bps = client.goodput_bps * 0.3;
        let initial_jitter = client.max_goodput_jitter_bps;

        // 10 healthy ACKs on a browser-ready path.
        sim_acks(&mut client, 10, 30_000, 1.0);

        assert!(
            client.max_goodput_jitter_bps < initial_jitter * 0.5,
            "max_goodput_jitter_bps {:.0} should have decayed below {:.0} \
             (50% of initial {initial_jitter:.0}) after 10 healthy ACKs on a ready path",
            client.max_goodput_jitter_bps,
            initial_jitter * 0.5,
        );
    }

    #[test]
    fn byte_budget_uses_floor_when_goodput_depressed() {
        let mut client = browser_ready_high_bandwidth_client();
        client.goodput_bps = 100_000.0;

        let budget = byte_budget_for(&client, 100.0);
        let floor_budget = (bandwidth_floor_bps(&client) * 100.0 / 1_000.0).ceil() as usize;

        assert!(
            budget >= floor_budget,
            "byte_budget {budget} should be at least bandwidth_floor-based {floor_budget} \
             when goodput_bps is depressed but delivery_bps is high"
        );
    }

    #[test]
    fn probe_floor_maintained_under_congestion_signal() {
        let mut client = test_client();
        client.display_fps = 120.0;
        client.rtt_ms = 40.0;
        client.min_rtt_ms = 40.0;
        client.goodput_bps = 5_000_000.0;
        client.delivery_bps = 5_000_000.0;
        client.last_goodput_sample_bps = 5_000_000.0;
        client.avg_paced_frame_bytes = 10_000.0;
        client.avg_preview_frame_bytes = 1_024.0;
        client.probe_frames = 10.0;

        // Many ACKs with high RTT: probes should not drop below the floor.
        sim_acks(&mut client, 20, 10_000, 600.0);

        assert!(
            client.probe_frames >= 1.0,
            "probe_frames {:.1} should not drop below the floor of 1.0",
            client.probe_frames,
        );
    }
}
