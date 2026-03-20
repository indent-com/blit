use blit_remote::{
    build_update_msg, FrameState, C2S_ACK, C2S_CLIENT_METRICS, C2S_CLOSE, C2S_CREATE,
    C2S_DISPLAY_RATE, C2S_FOCUS, C2S_INPUT, C2S_RESIZE, C2S_SCROLL, C2S_SEARCH, C2S_SUBSCRIBE,
    C2S_UNSUBSCRIBE, S2C_CLOSED, S2C_CREATED, S2C_LIST, S2C_SEARCH_RESULTS, S2C_TITLE,
};
use blit_wezterm::{SearchResult as WeztermSearchResult, TerminalDriver as WeztermDriver};
use std::collections::{HashMap, HashSet, VecDeque};
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

const SCROLLBACK_ROWS_DEFAULT: usize = 10_000;

struct Config {
    shell: String,
    scrollback: usize,
    socket_path: String,
}

struct OwnedFd(RawFd);
impl AsRawFd for OwnedFd {
    fn as_raw_fd(&self) -> RawFd {
        self.0
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
    fn snapshot(&mut self, echo: bool, icanon: bool) -> FrameState;
    fn scrollback_frame(&mut self, offset: usize) -> FrameState;
}

struct PtySearchResult {
    score: u32,
    primary_source: u8,
    matched_sources: u8,
    context: String,
    scroll_offset: Option<usize>,
}

impl PtyDriver for WeztermDriver {
    fn size(&self) -> (u16, u16) {
        WeztermDriver::size(self)
    }

    fn resize(&mut self, rows: u16, cols: u16) {
        WeztermDriver::resize(self, rows, cols);
    }

    fn process(&mut self, data: &[u8]) {
        WeztermDriver::process(self, data);
    }

    fn title(&self) -> &str {
        WeztermDriver::title(self)
    }

    fn search_result(&self, query: &str) -> Option<PtySearchResult> {
        WeztermDriver::search_result(self, query).map(|result: WeztermSearchResult| PtySearchResult {
            score: result.score,
            primary_source: result.primary_source as u8,
            matched_sources: result.matched_sources,
            context: result.context,
            scroll_offset: result.scroll_offset,
        })
    }

    fn take_title_dirty(&mut self) -> bool {
        WeztermDriver::take_title_dirty(self)
    }

    fn cursor_position(&self) -> (u16, u16) {
        WeztermDriver::cursor_position(self)
    }

    fn snapshot(&mut self, echo: bool, icanon: bool) -> FrameState {
        WeztermDriver::snapshot(self, echo, icanon)
    }

    fn scrollback_frame(&mut self, offset: usize) -> FrameState {
        WeztermDriver::scrollback_frame(self, offset)
    }
}

// At 240 fps over a 1 second RTT path we need roughly 240 frames in flight, plus
// jitter slack, before the first ACK comes back. Keep the async outbox large
// enough that the writer task and kernel socket buffers can absorb that window.
//
// Keep small to limit bufferbloat on slow connections.  The soft queue limit
// (OUTBOX_SOFT_QUEUE_LIMIT_FRAMES) prevents the tick from queuing more than
// ~2 frames, so this just needs to be bigger than that with some headroom.
const OUTBOX_CAPACITY: usize = 8;
const OUTBOX_SOFT_QUEUE_LIMIT_FRAMES: usize = 2;
const PTY_READ_DRAIN_MAX_BYTES: usize = 256 * 1024;
const PREVIEW_FPS_CAP: f32 = 30.0;
const PREVIEW_FRAME_RESERVE: usize = 1;
const LOCAL_FAST_PATH_MIN_WINDOW_FRAMES: usize = 8;
/// After the reader processes a batch, wait this long before snapshotting
/// in case the program is mid-frame (e.g. mpv between write() calls where
/// the kernel buffer is momentarily empty).
const SNAPSHOT_WRITE_DEBOUNCE: Duration = Duration::from_micros(500);
/// Maximum time to defer a snapshot after the PTY first becomes dirty.
/// Caps the debounce for continuous output (e.g. base64 /dev/random).
const SNAPSHOT_MAX_DEFER: Duration = Duration::from_millis(1);

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

struct Pty {
    master_fd: libc::c_int,
    child_pid: libc::pid_t,
    driver: Box<dyn PtyDriver>,
    dirty: bool,
    /// When the PTY first became dirty (for capping the defer).
    dirty_since: Option<Instant>,
    /// When the reader last processed data (for write debounce).
    last_write: Instant,
    draining: bool,
    reader_handle: tokio::task::JoinHandle<()>,
    /// Cached (echo, icanon) from tcgetattr; refreshed every ~250ms.
    lflag_cache: (bool, bool),
    lflag_last: Instant,
    /// When we last broadcast a title update for this PTY.
    last_title_send: Instant,
    /// Title changed but not yet sent (debounced).
    title_pending: bool,
}

struct ClientState {
    tx: mpsc::Sender<Vec<u8>>,
    lead: Option<u16>,
    subscriptions: HashSet<u16>,
    size: Option<(u16, u16)>,
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

fn fast_path_bandwidth_ready(client: &ClientState) -> bool {
    let need_bps = display_need_bps(client);
    let observed_bps = client
        .goodput_bps
        .max(client.delivery_bps)
        .max(client.last_goodput_sample_bps);
    observed_bps >= need_bps * 0.9
}

fn local_fast_path(client: &ClientState) -> bool {
    path_rtt_ms(client) <= 2.0
        && client.rtt_ms <= 12.0
        && client.browser_backlog_frames <= 2
        && client.browser_ack_ahead_frames <= 1
        && client.browser_apply_ms <= 1.0
        && fast_path_bandwidth_ready(client)
}

fn cadence_fps(client: &ClientState) -> f32 {
    if local_fast_path(client) {
        client.display_fps.max(1.0)
    } else {
        browser_pacing_fps(client)
    }
}

fn effective_rtt_ms(client: &ClientState) -> f32 {
    let path_rtt = path_rtt_ms(client);
    if local_fast_path(client) {
        return path_rtt;
    }
    let frame_ms = 1_000.0 / cadence_fps(client).max(1.0);
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
    if local_fast_path(client) || !throughput_limited(client) {
        effective
    } else {
        client.rtt_ms.clamp(effective, effective * 2.0)
    }
}

fn window_fps(client: &ClientState) -> f32 {
    if throughput_limited(client) {
        pacing_fps(client)
    } else {
        cadence_fps(client)
    }
}

fn target_frame_window(client: &ClientState) -> usize {
    let frames = frame_window(window_rtt_ms(client), window_fps(client))
        .saturating_add(client.probe_frames.round().max(0.0) as usize);
    if local_fast_path(client) {
        frames.max(LOCAL_FAST_PATH_MIN_WINDOW_FRAMES)
    } else {
        frames
    }
}

fn base_queue_ms(client: &ClientState) -> f32 {
    let frame_ms = 1_000.0 / cadence_fps(client).max(1.0);
    frame_ms * if throughput_limited(client) { 2.0 } else { 8.0 }
}

fn target_queue_ms(client: &ClientState) -> f32 {
    let frame_ms = 1_000.0 / cadence_fps(client).max(1.0);
    let probe_scale = if throughput_limited(client) {
        0.25
    } else {
        1.0
    };
    base_queue_ms(client) + client.probe_frames.max(0.0) * frame_ms * probe_scale
}

fn bandwidth_floor_bps(client: &ClientState) -> f32 {
    let browser_ready = client.browser_ack_ahead_frames <= 1
        && client.browser_apply_ms <= 1.0
        && !outbox_backpressured(client);
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
    let delivery_floor = client.delivery_bps * 0.5;
    let recent_sample_floor = if browser_ready && client.last_goodput_sample_bps > 0.0 {
        client.last_goodput_sample_bps * backlog_scale
    } else {
        0.0
    };
    goodput_floor
        .max(recent_sample_floor)
        .max(delivery_floor)
        .max(16_384.0)
}

fn pacing_fps(client: &ClientState) -> f32 {
    if local_fast_path(client) {
        return client.display_fps.max(1.0);
    }
    let frame_bytes = client.avg_paced_frame_bytes.max(256.0);
    let sustainable = bandwidth_floor_bps(client) / frame_bytes;
    sustainable
        .min(cadence_fps(client))
        .clamp(1.0, client.display_fps.max(1.0))
}

fn throughput_limited(client: &ClientState) -> bool {
    let floor = bandwidth_floor_bps(client);
    // Consider total demand: lead at cadence rate plus previews at their cap.
    // The old check (pacing_fps < cadence * 0.9) only saw lead bandwidth,
    // which is often tiny, so previews could starve the lead undetected.
    let lead_bps = client.avg_paced_frame_bytes.max(256.0) * cadence_fps(client);
    let preview_bps = client.avg_preview_frame_bytes.max(256.0)
        * client.display_fps.min(PREVIEW_FPS_CAP).max(1.0);
    (lead_bps + preview_bps) > floor * 0.9
}

fn browser_pacing_fps(client: &ClientState) -> f32 {
    let mut fps = client.display_fps.max(1.0);

    if client.browser_apply_ms > 0.0 {
        let apply_bound = 1_000.0 / (client.browser_apply_ms * 1.5).max(1.0);
        fps = fps.min(apply_bound);
    }

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
        bandwidth_floor_bps(client).max(32_768.0)
    } else {
        client.goodput_bps.max(32_768.0)
    };
    let bytes = budget_bps * budget_ms.max(1.0) / 1_000.0;
    bytes.ceil().max(client.avg_frame_bytes.max(256.0)) as usize
}

fn target_byte_window(client: &ClientState) -> usize {
    let budget = byte_budget_for(client, path_rtt_ms(client) + target_queue_ms(client));
    // Ensure the byte window can hold the frame window worth of *lead*
    // frames so the pipeline stays full across one RTT.  Use paced
    // (lead) frame size — not avg_frame_bytes which includes large
    // previews and would re-introduce the bufferbloat the budget fixes.
    let lead_floor = (client.avg_paced_frame_bytes.max(256.0)
        * target_frame_window(client).max(2) as f32)
        .ceil() as usize;
    budget.max(lead_floor)
}

fn send_interval(client: &ClientState) -> Duration {
    Duration::from_secs_f64(1.0 / cadence_fps(client).max(1.0) as f64)
}

fn preview_fps(client: &ClientState) -> f32 {
    let mut fps = client.display_fps.min(PREVIEW_FPS_CAP).max(1.0);
    if client.lead.is_some() && !local_fast_path(client) {
        // Always budget preview bandwidth: available minus lead's share.
        // Without this, large preview frames (e.g. 12 KB) at 30 fps consume
        // 360 KB/s, starving the lead even when lead frames are tiny.
        let avail = bandwidth_floor_bps(client);
        let lead_bps = client.avg_paced_frame_bytes.max(256.0) * cadence_fps(client);
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

fn preview_deadline(client: &ClientState, pid: u16, now: Instant) -> Instant {
    client
        .preview_next_send_at
        .get(&pid)
        .copied()
        .unwrap_or(now)
}

fn client_has_due_preview(sess: &Session, client: &ClientState, now: Instant) -> bool {
    if client.lead.is_none() || local_fast_path(client) {
        return false;
    }
    client.subscriptions.iter().copied().any(|pid| {
        Some(pid) != client.lead
            && preview_deadline(client, pid, now) <= now
            && sess.ptys.get(&pid).map(|pty| pty.dirty).unwrap_or(false)
    })
}

fn outbox_queued_frames(client: &ClientState) -> usize {
    OUTBOX_CAPACITY.saturating_sub(client.tx.capacity())
}

fn outbox_backpressured(client: &ClientState) -> bool {
    outbox_queued_frames(client) >= OUTBOX_SOFT_QUEUE_LIMIT_FRAMES
}

fn preview_window_open(client: &ClientState) -> bool {
    window_open(client)
}

fn can_send_preview(client: &ClientState, pid: u16, now: Instant) -> bool {
    preview_window_open(client) && now >= preview_deadline(client, pid, now)
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
    if !reserve_preview_slot || client.lead.is_none() || local_fast_path(client) {
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
            // Slow decay toward smoothed RTT so the floor can recover from
            // stale values or measurement artefacts (e.g. min_rtt=0).
            client.min_rtt_ms = client.min_rtt_ms * 0.999 + client.rtt_ms * 0.001;
            // But if this sample is a genuine new low, lock it in.
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
        let frame_ms = 1_000.0 / cadence_fps(client).max(1.0);
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
                client.goodput_jitter_bps =
                    ewma_with_direction(client.goodput_jitter_bps, jitter_sample, 0.5, 0.125);
                client.max_goodput_jitter_bps =
                    (client.max_goodput_jitter_bps * 0.98).max(jitter_sample);
                client.last_goodput_sample_bps = sample_goodput;
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
        let max_probe_frames = (cadence_fps(client) * 0.125).max(4.0);
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
        } else if queue_delay_ms > frame_ms * high_delay_frames || jitter_ratio > 0.5 {
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

fn subscribe_client_to(client: &mut ClientState, pty_id: u16) {
    if client.subscriptions.insert(pty_id) {
        client.last_sent.remove(&pty_id);
        client.preview_next_send_at.remove(&pty_id);
    }
}

fn unsubscribe_client_from(client: &mut ClientState, pty_id: u16) {
    client.subscriptions.remove(&pty_id);
    client.last_sent.remove(&pty_id);
    client.preview_next_send_at.remove(&pty_id);
    if client.lead == Some(pty_id) {
        client.lead = None;
        client.scroll_offset = 0;
        client.scroll_cache = FrameState::default();
    }
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
    next_pty_id: u16,
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
            if c.lead == Some(pty_id) {
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
        let (cur_rows, cur_cols) = pty.driver.size();
        if cur_rows == rows && cur_cols == cols {
            return;
        }
        pty.driver.resize(rows, cols);
        pty.dirty = true;
        for c in self.clients.values_mut() {
            if c.subscriptions.contains(&pty_id) {
                c.last_sent.remove(&pty_id);
            }
            if c.lead == Some(pty_id) {
                c.scroll_cache = FrameState::default();
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

fn spawn_pty(
    shell: &str,
    rows: u16,
    cols: u16,
    id: u16,
    command: Option<&str>,
    argv: Option<&[&str]>,
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
        std::env::set_var("TERM", "xterm-256color");
        std::env::set_var("COLUMNS", &cols.to_string());
        std::env::set_var("LINES", &rows.to_string());
        if let Some(command) = command {
            let shell_c = CString::new(shell).unwrap();
            let exec_flag = CString::new("-lc").unwrap();
            let command_c = CString::new(command).unwrap();
            unsafe {
                let p = shell_c.as_ptr();
                let f = exec_flag.as_ptr();
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
        driver: Box::new(WeztermDriver::new(rows, cols, scrollback)),
        dirty: true,
        dirty_since: Some(Instant::now()),
        last_write: Instant::now(),
        draining: false,
        reader_handle,
        lflag_cache,
        lflag_last: Instant::now(),
        last_title_send: Instant::now(),
        title_pending: false,
    })
}

fn respond_to_queries(fd: libc::c_int, data: &[u8], size: (u16, u16), cursor: (u16, u16)) {
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

        let mut exit = false;
        let mut drained_any = false;
        let mut drained_bytes = 0usize;
        {
            let mut sess = state.1.lock().await;
            if let Some(pty) = sess.ptys.get_mut(&pty_id) {
                pty.draining = true;
            }
        }
        loop {
            let n = unsafe { libc::read(fd, buf.as_mut_ptr().cast(), buf.len()) };
            if n > 0 {
                let chunk = &buf[..n as usize];
                let mut sess = state.1.lock().await;
                if let Some(pty) = sess.ptys.get_mut(&pty_id) {
                    pty.driver.process(chunk);
                    let now = Instant::now();
                    if !pty.dirty {
                        pty.dirty_since = Some(now);
                    }
                    pty.dirty = true;
                    pty.last_write = now;
                    respond_to_queries(fd, chunk, pty.driver.size(), pty.driver.cursor_position());
                }
                drop(sess);
                drained_any = true;
                drained_bytes = drained_bytes.saturating_add(chunk.len());
                if drained_bytes >= PTY_READ_DRAIN_MAX_BYTES {
                    break;
                }
                tokio::task::yield_now().await;
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

        {
            let mut sess = state.1.lock().await;
            if let Some(pty) = sess.ptys.get_mut(&pty_id) {
                pty.draining = false;
            }
        }
        if drained_any {
            nudge_delivery(&state);
            // Under a firehose workload the fd may still be readable immediately
            // after this drain. Yield here so delivery can snapshot the
            // just-finished drain boundary before we mark the PTY draining again.
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
        for client in sess.clients.values_mut() {
            unsubscribe_client_from(client, pty_id);
        }
        let mut msg = vec![S2C_CLOSED];
        msg.extend_from_slice(&pty_id.to_le_bytes());
        sess.send_to_all(&msg);
    }
}

/// Check if the kernel PTY buffer has data waiting to be read.
fn pty_has_pending_data(fd: libc::c_int) -> bool {
    let mut pfd = libc::pollfd {
        fd,
        events: libc::POLLIN,
        revents: 0,
    };
    let ret = unsafe { libc::poll(&mut pfd, 1, 0) };
    ret > 0 && pfd.revents & libc::POLLIN != 0
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
    current: &FrameState,
    now: Instant,
    paced: bool,
) -> SendOutcome {
    let previous = client.last_sent.get(&pid).cloned().unwrap_or_default();
    let Some(msg) = build_update_msg(pid, current, &previous) else {
        return SendOutcome::NoChange;
    };
    let bytes = msg.len();
    if client.tx.try_send(msg).is_ok() {
        client.last_sent.insert(pid, current.clone());
        record_send(client, bytes, now, paced);
        client.frames_sent += 1;
        SendOutcome::Sent
    } else {
        // Outbox full — the sender can't keep up.  Advance last_sent to
        // the current frame so the NEXT diff is small (only changes since
        // now), effectively dropping this intermediate state.  Without
        // this, backpressure causes the tick to re-dirty the PTY, building
        // ever-larger diffs that make the backlog worse.
        client.last_sent.insert(pid, current.clone());
        SendOutcome::Backpressured
    }
}

fn default_socket_path() -> String {
    if let Ok(dir) = std::env::var("XDG_RUNTIME_DIR") {
        format!("{dir}/blit.sock")
    } else {
        "/tmp/blit.sock".into()
    }
}

fn usage() -> &'static str {
    "usage: blit-server [--socket PATH] [PATH]"
}

fn parse_config() -> Config {
    let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".into());
    let scrollback = std::env::var("BLIT_SCROLLBACK")
        .ok()
        .and_then(|s| s.parse::<usize>().ok())
        .unwrap_or(SCROLLBACK_ROWS_DEFAULT);
    let mut socket_path = std::env::var("BLIT_SOCK").ok();

    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        if arg == "--help" || arg == "-h" {
            println!("{}", usage());
            println!("  --socket PATH            Unix socket path (or set BLIT_SOCK)");
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
        scrollback,
        socket_path: socket_path.unwrap_or_else(default_socket_path),
    }
}

fn bind_socket(sock_path: &str) -> UnixListener {
    let _ = std::fs::remove_file(&sock_path);
    let listener = UnixListener::bind(&sock_path).unwrap();
    std::fs::set_permissions(&sock_path, std::fs::Permissions::from_mode(0o700)).unwrap();
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

    let max_fps = sess
        .clients
        .values()
        .map(|c| cadence_fps(c))
        .fold(1.0_f32, f32::max);
    let title_interval = Duration::from_secs_f64(1.0 / max_fps as f64);
    let ids: Vec<u16> = sess.ptys.keys().copied().collect();
    for &id in &ids {
        let pty = sess.ptys.get_mut(&id).unwrap();
        if pty.driver.take_title_dirty() {
            pty.dirty = true;
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
        let pty = sess.ptys.get_mut(&id).unwrap();
        if !pty.dirty || pty.draining || !needful_ptys.contains(&id) {
            continue;
        }
        // Don't snapshot while the kernel PTY buffer has unread data — the
        // reader hasn't processed it yet, so the terminal is stale.  Also
        // debounce: if the reader just processed data, the program may be
        // between write() calls with the kernel buffer momentarily empty.
        // Both checks are capped by SNAPSHOT_MAX_DEFER for continuous output.
        let capped = pty.dirty_since
            .map(|since| since + SNAPSHOT_MAX_DEFER <= now)
            .unwrap_or(false);
        if !capped {
            let recent = pty.last_write + SNAPSHOT_WRITE_DEBOUNCE > now;
            if recent || pty_has_pending_data(pty.master_fd) {
                let retry = pty.last_write + SNAPSHOT_WRITE_DEBOUNCE;
                next_deadline = Some(match next_deadline {
                    Some(existing) => existing.min(retry),
                    None => retry,
                });
                continue;
            }
        }
        snapshots.insert(id, take_snapshot(pty));
        pty.dirty = false;
        pty.dirty_since = None;
        sess.tick_snaps += 1;
        did_work = true;
    }

    let client_ids: Vec<u64> = sess.clients.keys().copied().collect();
    for cid in client_ids {
        let (
            lead,
            subscriptions,
            scroll_offset,
            can_send_lead,
            lead_has_window,
            any_send_window,
            lead_deadline,
        ) = {
            let c = sess.clients.get(&cid).unwrap();
            let reserve_preview_slot = client_has_due_preview(&sess, c, now);
            (
                c.lead,
                c.subscriptions.iter().copied().collect::<Vec<_>>(),
                c.scroll_offset,
                can_send_frame(c, now, reserve_preview_slot),
                lead_window_open(c, reserve_preview_slot),
                lead_window_open(c, reserve_preview_slot) || preview_window_open(c),
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
                        let c = sess.clients.get(&cid).unwrap();
                        c.scroll_cache.clone()
                    };
                    let outcome = if let Some(pty) = sess.ptys.get_mut(&pid) {
                        if let Some((msg, new_frame)) =
                            build_scrollback_update(pty, pid, scroll_offset, &prev_frame)
                        {
                            let c = sess.clients.get_mut(&cid).unwrap();
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
                                pty.dirty = true;
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
                if let Some(cur) = snapshots.get(&pid) {
                    let c = sess.clients.get_mut(&cid).unwrap();
                    match try_send_update(c, pid, cur, now, true) {
                        SendOutcome::Sent => did_work = true,
                        SendOutcome::Backpressured => {
                            if let Some(pty) = sess.ptys.get_mut(&pid) {
                                pty.dirty = true;
                            }
                        }
                        SendOutcome::NoChange => {}
                    }
                }
            } else {
                let has_pending = sess.ptys.get(&pid).map(|pty| pty.dirty).unwrap_or(false);
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
                        preview_window_open(c),
                    ),
                    None => (false, now, false),
                };
            if !preview_has_window {
                break;
            }
            if !preview_can_send {
                let has_pending = sess.ptys.get(&pid).map(|pty| pty.dirty).unwrap_or(false);
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
                continue;
            };
            let c = sess.clients.get_mut(&cid).unwrap();
            match try_send_update(c, pid, cur, now, false) {
                SendOutcome::Sent => {
                    record_preview_send(c, pid, now);
                    did_work = true;
                }
                SendOutcome::Backpressured => {
                    if let Some(pty) = sess.ptys.get_mut(&pid) {
                        pty.dirty = true;
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
                size: None,
                scroll_offset: 0,
                scroll_cache: FrameState::default(),
                last_sent: HashMap::new(),
                preview_next_send_at: HashMap::new(),
                rtt_ms: 50.0,
                min_rtt_ms: 0.0,
                display_fps: 60.0,
                // Conservative seed — the rise alpha (0.5) converges up to
                // multi-MB/s in a handful of samples on fast paths. Starting
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
                last_log: Instant::now(),
                goodput_window_bytes: 0,
                goodput_window_start: Instant::now(),
            },
        );
        let list = sess.pty_list_msg();
        if let Some(c) = sess.clients.get(&client_id) {
            let _ = c.tx.try_send(list);
            for (&id, pty) in &sess.ptys {
                let title = pty.driver.title();
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
                    if update_client_scroll_state(c, pid, 0) {
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
                        pty.driver.search_result(query).map(|result| SearchResultRow {
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
                let create_payload = data
                    .get(5..)
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
                let id = sess.next_pty_id;
                sess.next_pty_id += 1;
                if let Some(pty) = spawn_pty(
                    &config.shell,
                    rows,
                    cols,
                    id,
                    command,
                    argv.as_deref(),
                    config.scrollback,
                    state.clone(),
                ) {
                    sess.ptys.insert(id, pty);
                    if let Some(c) = sess.clients.get_mut(&client_id) {
                        c.lead = Some(id);
                        subscribe_client_to(c, id);
                        c.scroll_offset = 0;
                        c.scroll_cache = FrameState::default();
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
            C2S_SUBSCRIBE if data.len() >= 3 => {
                let pid = u16::from_le_bytes([data[1], data[2]]);
                if sess.ptys.contains_key(&pid) {
                    if let Some(c) = sess.clients.get_mut(&client_id) {
                        subscribe_client_to(c, pid);
                    }
                    if let Some(pty) = sess.ptys.get_mut(&pid) {
                        pty.dirty = true;
                    }
                    need_nudge = true;
                }
            }
            C2S_UNSUBSCRIBE if data.len() >= 3 => {
                let pid = u16::from_le_bytes([data[1], data[2]]);
                if sess.ptys.contains_key(&pid) {
                    let old_lead = sess.clients.get(&client_id).and_then(|c| c.lead);
                    if let Some(c) = sess.clients.get_mut(&client_id) {
                        unsubscribe_client_from(c, pid);
                        reset_inflight(c);
                    }
                    if old_lead == Some(pid) {
                        if let Some((r, c)) = sess.min_size_for_pty(pid) {
                            sess.resize_pty(pid, r, c);
                            need_nudge = true;
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
        let old_lead = sess.clients.get(&client_id).and_then(|c| c.lead);
        sess.clients.remove(&client_id);
        if let Some(pid) = old_lead {
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

#[cfg(test)]
mod tests {
    use super::*;

    fn test_client_with_capacity(capacity: usize) -> (ClientState, mpsc::Receiver<Vec<u8>>) {
        let (tx, rx) = mpsc::channel(capacity);
        let client = ClientState {
            tx,
            lead: None,
            subscriptions: HashSet::new(),
            size: None,
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
        assert!(preview_window_open(&client));
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
}
