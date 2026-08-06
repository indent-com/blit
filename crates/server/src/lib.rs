use blit_alacritty::{SearchResult as AlacrittySearchResult, TerminalDriver as AlacrittyDriver};
use blit_compositor::{CompositorCommand, CompositorEvent, CompositorHandle};
use blit_remote::{
    C2S_ACK, C2S_CLIENT_FEATURES, C2S_CLIENT_METRICS, C2S_CLIPBOARD_GET, C2S_CLIPBOARD_LIST,
    C2S_CLIPBOARD_SET, C2S_CLOSE, C2S_COPY_RANGE, C2S_CREATE, C2S_CREATE_AT, C2S_CREATE_N,
    C2S_CREATE2, C2S_DEADLINE, C2S_DISPLAY_RATE, C2S_FOCUS, C2S_INPUT, C2S_KILL, C2S_MOUSE,
    C2S_PING, C2S_QUIT, C2S_READ, C2S_RESIZE, C2S_RESTART, C2S_SCROLL, C2S_SEARCH, C2S_SUBSCRIBE,
    C2S_SURFACE_ACK, C2S_SURFACE_CAPTURE, C2S_SURFACE_CLOSE, C2S_SURFACE_FOCUS, C2S_SURFACE_INPUT,
    C2S_SURFACE_LIST, C2S_SURFACE_POINTER, C2S_SURFACE_POINTER_AXIS, C2S_SURFACE_POINTER_AXIS2,
    C2S_SURFACE_RESIZE, C2S_SURFACE_SUBSCRIBE, C2S_SURFACE_TEXT, C2S_SURFACE_UNSUBSCRIBE,
    C2S_TERM_CWD, C2S_UNSUBSCRIBE, CAPTURE_FORMAT_AVIF, CAPTURE_FORMAT_PNG, CREATE2_HAS_COMMAND,
    CREATE2_HAS_CWD, CREATE2_HAS_DEADLINE, CREATE2_HAS_SRC_PTY, CREATE2_WANT_STATUS,
    FEATURE_COMPOSITOR, FEATURE_COPY_RANGE, FEATURE_CREATE_NONCE, FEATURE_CREATE_STATUS,
    FEATURE_KILL_MODE, FEATURE_PTY_DEADLINE, FEATURE_RESIZE_BATCH, FEATURE_RESTART, FrameState,
    KILL_LEADER_ONLY, READ_ANSI, READ_TAIL, S2C_CLOSED, S2C_CREATED, S2C_CREATED_N, S2C_LIST,
    S2C_PING, S2C_QUIT, S2C_READY, S2C_SEARCH_RESULTS, S2C_SURFACE_CAPTURE, S2C_SURFACE_LIST,
    S2C_TEXT, S2C_TITLE, STATUS_BUDGET, STATUS_INVALID, STATUS_OTHER, STATUS_TOO_LARGE,
    SURFACE_FRAME_CODEC_H264, SURFACE_FRAME_FLAG_KEYFRAME, SURFACE_POINTER_AXIS2_LEN,
    build_update_msg, msg_hello, msg_s2c_clipboard_content, msg_s2c_clipboard_list,
    msg_s2c_used_rows, msg_surface_app_id, msg_surface_created, msg_surface_destroyed,
    msg_surface_encoder, msg_surface_frame, msg_surface_resized, msg_surface_title,
    msg_term_cwd_reply, parse_surface_pointer_axis2,
};
#[cfg(target_os = "linux")]
use blit_remote::{C2S_AUDIO_SUBSCRIBE, C2S_AUDIO_UNSUBSCRIBE, FEATURE_AUDIO};
use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::time::{Duration, Instant};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::sync::{Mutex, Notify, mpsc};

#[cfg(target_os = "linux")]
mod audio;
#[cfg(target_os = "linux")]
mod audio_pw;
mod gpu_libs;
mod ipc;
mod kv;
mod net;
mod nvenc_encode;
mod pty;
mod surface_encoder;
#[cfg(target_os = "linux")]
mod vaapi_encode;

pub use ipc::{IpcListener, default_ipc_path};
use pty::{PtyHandle, PtyWriteTarget};
pub use surface_encoder::ChromaSubsampling;
use surface_encoder::SurfaceEncoder;
pub use surface_encoder::SurfaceEncoderPreference;
pub use surface_encoder::SurfaceH264EncoderPreference;
pub use surface_encoder::{SurfaceBandwidth, SurfaceEncoding, SurfaceSpeed};

type PtyFds = Arc<std::sync::RwLock<HashMap<u16, PtyWriteTarget>>>;

/// How many exited-but-retained terminals to keep, oldest evicted first.
/// `BLIT_MAX_EXITED` overrides; 0 disables the bound.
///
/// A terminal's output stays readable after its command exits, and consumers
/// legitimately read it back long afterwards, so this is generous — it exists
/// to stop an orchestrator that never sends `C2S_CLOSE` from growing the map
/// without limit, not to reclaim memory promptly.
pub const DEFAULT_MAX_EXITED: usize = 1024;

/// Evict exited terminals this long after they exit.  `BLIT_EXITED_LINGER`
/// overrides, in seconds.
///
/// Off by default, deliberately: a time bound throws away output someone may
/// still want, and "how long is a result interesting" is a policy question
/// the server has no way to answer. The count bound alone keeps the map
/// bounded without ever discarding anything an active consumer is likely to
/// come back for.
pub const DEFAULT_EXITED_LINGER: Duration = Duration::ZERO;

fn max_exited() -> usize {
    static V: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
    *V.get_or_init(|| {
        std::env::var("BLIT_MAX_EXITED")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(DEFAULT_MAX_EXITED)
    })
}

fn exited_linger() -> Duration {
    static V: std::sync::OnceLock<Duration> = std::sync::OnceLock::new();
    *V.get_or_init(|| {
        std::env::var("BLIT_EXITED_LINGER")
            .ok()
            .and_then(|v| v.parse().ok())
            .map(Duration::from_secs)
            .unwrap_or(DEFAULT_EXITED_LINGER)
    })
}

pub struct Config {
    pub shell: String,
    pub shell_flags: String,
    pub scrollback: usize,
    pub ipc_path: String,
    pub surface_encoders: Vec<SurfaceEncoderPreference>,
    pub surface_encoding: SurfaceEncoding,
    pub chroma: ChromaSubsampling,
    pub vaapi_device: String,
    #[cfg(unix)]
    pub fd_channel: Option<std::os::unix::io::RawFd>,
    pub verbose: bool,
    /// Maximum number of concurrent client connections (0 = unlimited).
    pub max_connections: usize,
    /// Maximum number of PTYs across all clients (0 = unlimited).  Counts
    /// exited-but-retained terminals too, since those still hold an id and a
    /// scrollback.
    pub max_ptys: usize,
    /// Application-level ping interval.  The server sends S2C_PING to every
    /// client at this cadence so that transports without native keepalive
    /// (WebRTC data channels) can detect dead connections.  0 = disabled.
    pub ping_interval: Duration,
    /// Skip compositor initialization (e.g. for share-only mode).
    pub skip_compositor: bool,
    /// Export the server's IPC path as `BLIT_SOCK` in spawned terminals so
    /// `blit` invocations inside them target this server.  Off by default:
    /// `BLIT_*` is otherwise stripped from child environments.
    pub export_sock: bool,
    /// Append the directory holding the running server binary to `PATH` in
    /// spawned terminals, so `blit` is callable inside them (Unix only; the
    /// Windows PTY inherits the server's environment wholesale).  Off by
    /// default, and worth leaving off when the server is embedded in a host
    /// binary whose directory holds no `blit`.
    pub inject_path: bool,
    /// Permit relayed streams to skip TLS certificate verification
    /// (`NET_OPEN_INSECURE`). Right for a self-signed dev server on loopback,
    /// wrong for anything reached across a network.
    /// `--allow-forward` egress patterns (docs/design/net.md § Target
    /// policy). Empty = unrestricted, the default.
    pub allow_forward: Vec<String>,
    pub allow_forward_insecure: bool,
}

trait PtyDriver: Send {
    fn size(&self) -> (u16, u16);
    fn resize(&mut self, rows: u16, cols: u16);
    fn process(&mut self, data: &[u8]);
    fn title(&self) -> &str;
    fn search_result(&self, query: &str) -> Option<PtySearchResult>;
    fn take_title_dirty(&mut self) -> bool;
    fn take_clipboard_stores(&mut self) -> Vec<String>;
    fn used_rows(&self) -> u16;
    fn take_used_rows_dirty(&mut self) -> bool;
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
    fn get_text_range(
        &self,
        start_tail: u32,
        start_col: u16,
        end_tail: u32,
        end_col: u16,
    ) -> String;
    fn total_lines(&self) -> u32;
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

    fn take_clipboard_stores(&mut self) -> Vec<String> {
        AlacrittyDriver::take_clipboard_stores(self)
    }

    fn used_rows(&self) -> u16 {
        AlacrittyDriver::used_rows(self)
    }

    fn take_used_rows_dirty(&mut self) -> bool {
        AlacrittyDriver::take_used_rows_dirty(self)
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

    fn get_text_range(
        &self,
        start_tail: u32,
        start_col: u16,
        end_tail: u32,
        end_col: u16,
    ) -> String {
        AlacrittyDriver::get_text_range(self, start_tail, start_col, end_tail, end_col)
    }

    fn total_lines(&self) -> u32 {
        AlacrittyDriver::total_lines(self)
    }
}

// Soft backpressure thresholds.  The outbox channel is unbounded so messages
// are never dropped, but production is throttled (via `window_open` /
// `surface_window_open`) once either counter exceeds these limits.
const OUTBOX_SOFT_QUEUE_LIMIT_FRAMES: usize = 4;
// Must comfortably hold one keyframe at 1920x1080 from a software encoder
// (200-400 KB is typical).  Setting this too low deadlocks the outbox gate
// when a single frame exceeds the cap — surface_window_open returns false
// even at outbox=1, and no new frames can ever be produced.
const OUTBOX_SOFT_QUEUE_LIMIT_BYTES: usize = 1024 * 1024;
const PREVIEW_FRAME_RESERVE: usize = 1;
const READY_FRAME_QUEUE_CAP: usize = 4;
const PTY_CHANNEL_CAPACITY: usize = 64;
/// Max bytes of PTY output parsed per PTY per tick.  Parsing happens inside
/// the tick task while it holds the session mutex, so an unbudgeted drain of
/// a flooding PTY (`dd if=/dev/random`) starves every input handler, every
/// other PTY, and new connections — the whole server wedges at 100% CPU.
/// When the budget runs out the tick finishes its round (snapshots, input,
/// frame delivery) and resumes immediately; the bounded byte channel then
/// backs up, the reader thread blocks, the kernel PTY buffer fills, and the
/// flooding process's write(2) blocks.  That is ordinary terminal flow
/// control: the producer runs at the speed we can actually parse and render,
/// instead of the server disappearing under it.
const PTY_PARSE_BUDGET_PER_TICK: usize = 256 * 1024;
const SYNC_OUTPUT_END: &[u8] = b"\x1b[?2026l";

/// Number of surface frames to send at wire speed after a keyframe request
/// (subscribe, resubscribe, or error recovery).  During this burst window
/// only outbox backpressure gates delivery — the time-based pacing interval
/// is skipped.  This lets bandwidth estimates ramp up quickly on high-latency
/// links instead of starving the pipeline with conservative initial rates.
const SURFACE_BURST_FRAMES: u8 = 4;

/// A chunk of data from the PTY reader, sent through a lock-free channel
/// so the reader never contends with the delivery tick for the Session mutex.
enum PtyInput {
    /// Raw bytes from the PTY, with the reader's sync-scan tail for boundary
    /// detection. The tick task calls `process()` + `respond_to_queries()`.
    Data(Vec<u8>),
    /// Data up to and including a sync-output-close (`\x1b[?2026l`).
    /// Process `before` and then take a snapshot.  Any bytes following the
    /// boundary are sent in a subsequent `Data` or `SyncBoundary` event —
    /// the reader's loop re-scans them, so this event must not try to
    /// process them itself.
    SyncBoundary { before: Vec<u8> },
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

/// Largest bulk-frame payload we'll write in a single length-prefixed
/// message.  Payloads above this are split into `S2C_FRAGMENT` messages
/// so audio frames can be drained between chunks, bounding the time
/// audio sits blocked behind a bulk write to (roughly) `CHUNK_BYTES /
/// network_bandwidth`.  Too small and per-message overhead dominates
/// (each fragment adds an 8-byte length prefix + 2-byte fragment header);
/// too large and audio suffers head-of-line blocking again.  4 KiB keeps
/// per-chunk write time under ~4 ms even on a 1 MB/s link — well below
/// the 20 ms audio frame cadence with headroom for a handful of chunks
/// in flight.
const BULK_CHUNK_BYTES: usize = 4 * 1024;

/// Write a bulk message, draining pending audio frames between chunks.
///
/// Payloads that fit within `BULK_CHUNK_BYTES` are written as a single
/// length-prefixed frame after a pre-drain of pending audio (same as
/// before).  Larger payloads are split into `S2C_FRAGMENT` messages so
/// audio frames written between fragments remain valid, complete,
/// length-prefixed messages on the wire — never interleaved inside a
/// single `read_exact`-delimited payload, which would desynchronise
/// the reader's framing.
async fn write_frame_interleaved(
    writer: &mut (impl AsyncWrite + Unpin),
    payload: &[u8],
    audio_rx: &mut mpsc::UnboundedReceiver<Vec<u8>>,
) -> bool {
    // Small message: drain any queued audio, then write as-is.
    if payload.len() <= BULK_CHUNK_BYTES {
        while let Ok(audio_msg) = audio_rx.try_recv() {
            if !write_frame(writer, &audio_msg).await {
                return false;
            }
        }
        return write_frame(writer, payload).await;
    }

    // Large message: split into S2C_FRAGMENT messages, draining audio
    // between each chunk.  The chunks carry the original payload bytes
    // verbatim (including its type byte in the first chunk); the
    // receiver concatenates them and dispatches the reassembled buffer.
    let mut offset = 0;
    while offset < payload.len() {
        while let Ok(audio_msg) = audio_rx.try_recv() {
            if !write_frame(writer, &audio_msg).await {
                return false;
            }
        }
        let end = (offset + BULK_CHUNK_BYTES).min(payload.len());
        let is_last = end == payload.len();
        let mut frag = Vec::with_capacity(2 + (end - offset));
        frag.push(blit_remote::S2C_FRAGMENT);
        frag.push(if is_last {
            blit_remote::FRAGMENT_FLAG_LAST
        } else {
            0
        });
        frag.extend_from_slice(&payload[offset..end]);
        if !write_frame(writer, &frag).await {
            return false;
        }
        offset = end;
    }
    true
}

struct Pty {
    handle: PtyHandle,
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
    /// Last used visible rows value broadcast for this PTY.
    last_used_rows_sent: u16,
    /// When the server should stop this terminal, if a client armed a
    /// deadline.  Absent means unbounded, which stays the default — a
    /// multiplexer whose sessions expire on their own would be useless.
    deadline: Option<Instant>,
    /// Set once the deadline has fired and SIGTERM has gone out; when it
    /// passes, the group gets SIGKILL.
    stop_deadline: Option<Instant>,
    /// Attributed cause, moved onto `S2C_EXITED` by `cleanup_pty_internal`.
    exit_reason: u8,
    /// The subprocess has exited but the terminal state is retained for reading.
    exited: bool,
    /// When it exited, for the retention bound.  `None` while live.
    exited_at: Option<Instant>,
    /// Bumped every time this slot gets a fresh child, so work queued against
    /// one generation cannot land on the next.  `C2S_RESTART` reuses the id
    /// and the driver in place, which makes the id alone useless as identity.
    generation: u64,
    /// Exit status: WEXITSTATUS if normal exit, negative signal number if signalled,
    /// EXIT_STATUS_UNKNOWN if not yet collected.
    exit_status: i32,
    /// Command used to create this PTY (None = default shell).
    command: Option<String>,
    /// Explicit working directory used to create this PTY.
    cwd: Option<String>,
    /// Working directory last reported by the shell via OSC 7, already
    /// validated by `parse_osc7_url` (docs/protocol.md, "Working directory
    /// tracking").  Last write wins; None until shell integration first
    /// reports (then `C2S_TERM_CWD` falls back to the kernel's view).
    osc7_cwd: Option<String>,
}

impl Pty {
    fn mark_dirty(&mut self) {
        self.dirty = true;
    }

    fn clear_dirty(&mut self) {
        self.dirty = false;
    }
}

struct CachedSurfaceInfo {
    surface_id: u16,
    parent_id: u16,
    width: u16,
    height: u16,
    title: String,
    app_id: String,
}

/// Last committed pixel buffer for a surface, kept so we can re-encode a
/// keyframe for late-joining clients without going back to the compositor.
struct LastPixels {
    width: u32,
    height: u32,
    pixels: blit_compositor::PixelData,
    /// Monotonically increasing counter bumped on every SurfaceCommit.
    /// Used to skip re-encoding when the pixel data hasn't changed.
    generation: u64,
    /// CLOCK_MONOTONIC milliseconds captured at compositor commit time.
    /// Used as the surface frame timestamp so the client sees the source's
    /// presentation timing rather than the (jittery) encode-delivery clock.
    timestamp_ms: u32,
}

/// The most recent bitstream a compositor-resident encoder produced for
/// one `(surface, client)` pair.
///
/// Kept apart from `last_pixels` because Vulkan Video owns one encoder per
/// subscribing client: the bytes belong to exactly one client and must
/// never be handed to another, which is what sharing them by target size
/// used to do.
struct LastEncoded {
    width: u32,
    height: u32,
    data: Arc<Vec<u8>>,
    is_keyframe: bool,
    codec_flag: u8,
    generation: u64,
    timestamp_ms: u32,
}

/// Drop every `last_pixels` entry belonging to `sid`, regardless of
/// per-target size.  Used when the surface is destroyed/resized/created
/// to avoid serving stale frames to encoders that were sized against
/// the prior composite.
fn last_pixels_remove_for_sid(last_pixels: &mut HashMap<(u16, u32, u32), LastPixels>, sid: u16) {
    let keys: Vec<(u16, u32, u32)> = last_pixels.keys().filter(|k| k.0 == sid).copied().collect();
    for k in keys {
        last_pixels.remove(&k);
    }
}

/// Drop every compositor-encoded frame belonging to `sid`, for every
/// client.  Paired with `last_pixels_remove_for_sid`: a surface that was
/// destroyed or resized invalidates both.
fn last_encoded_remove_for_sid(last_encoded: &mut HashMap<(u16, u64), LastEncoded>, sid: u16) {
    last_encoded.retain(|k, _| k.0 != sid);
}

/// Authoritative compositor native dims for `sid`, preferring the value
/// stored from the most recent `SurfaceResized` event.  Falls back to the
/// largest entry in the per-target pixel snapshot when the resized event
/// hasn't been received yet (very first render after `SurfaceCreated`).
///
/// Native MUST NOT be derived from `pixel_snapshot.max_by_key(area)` once
/// the `SurfaceResized` value exists: the renderer can blit into stale
/// `external_outputs` / `downscale_outputs` entries (registered for prior
/// per-client targets) and those produce extra pixel snapshots at the
/// old, possibly-larger sizes.  The largest-area pick then yields a
/// stale value, mis-clamping `per_client_encode_target` and triggering
/// avoidable encoder rebuilds — and on aspect-ratio mismatches, freezing
/// visible frames at the stale target until the entry is cleared.
fn compositor_native_for_sid(
    native_sizes: &HashMap<u16, (u32, u32)>,
    pixel_snapshot: &[(u16, u32, u32, u64, u32)],
    sid: u16,
) -> Option<(u32, u32)> {
    if let Some(&dims) = native_sizes.get(&sid) {
        return Some(dims);
    }
    pixel_snapshot
        .iter()
        .filter(|&&(s, _, _, _, _)| s == sid)
        .max_by_key(|&&(_, w, h, _, _)| (w as u64) * (h as u64))
        .map(|&(_, w, h, _, _)| (w, h))
}

struct SharedCompositor {
    handle: CompositorHandle,
    surfaces: HashMap<u16, CachedSurfaceInfo>,
    /// Latest pixel snapshot per `(surface_id, width, height)`.  The
    /// compositor renders one surface into multiple per-target buffers
    /// (one per registered per-client encoder size) plus a native BGRA
    /// staging readback, so the same surface produces several entries
    /// here — one per distinct size.  The encode loop picks the entry
    /// matching its client's per-client encode target; CPU encoders
    /// without a registered external fall back to the largest entry
    /// (the native composite) and downscale themselves.
    last_pixels: HashMap<(u16, u32, u32), LastPixels>,
    /// Latest compositor-encoded bitstream per `(surface_id, client_id)`.
    last_encoded: HashMap<(u16, u64), LastEncoded>,
    /// Per-surface timestamp of the last RequestFrame sent.  Used to
    /// throttle requests to at most one per 1 ms so frame callbacks
    /// carry distinct `elapsed_ms` timestamps — video players (mpv)
    /// use these to pace their presentation clock.  Supports up to 1 kHz.
    last_frame_request: HashMap<u16, Instant>,
    #[cfg(target_os = "linux")]
    created_at: Instant,
    /// Monotonically increasing counter for pixel generations.
    pixel_generation: u64,
    /// Last time we sent blanket RequestFrame for all surfaces (including
    /// those without subscribers).  Throttled to prevent hot-looping when
    /// apps commit at high rates without any client consuming frames.
    last_blanket_frame_request: Instant,
    /// Last dimensions sent to the compositor via `CompositorCommand::SurfaceResize`.
    /// Used to dedup resize commands — the composited output size
    /// (`info.width`/`info.height`) may differ from the requested size
    /// when the Wayland client sets `xdg_geometry` (e.g. excluding a
    /// title bar), so we compare against the actually-requested values.
    last_configured_size: HashMap<u16, (u16, u16, u16)>,
    /// Instant of the last resize actually handed to the compositor, per
    /// surface.  Opens that surface's settle window; see
    /// `SURFACE_RESIZE_SETTLE`.
    last_resize_at: HashMap<u16, Instant>,
    /// The most recent size requested for a surface while its settle window
    /// was still open.  Dispatched by `flush_due_resizes` once the window
    /// closes; overwritten (not queued) by every further request, so a drag
    /// costs one configure per window rather than one per frame.
    pending_resize: HashMap<u16, (u16, u16, u16)>,
    /// Authoritative compositor native (physical) size per surface, set from
    /// `CompositorEvent::SurfaceResized`.  Used by the per-client encode
    /// target computation as the `(native_w, native_h)` clamp.
    ///
    /// Why not derive native from `last_pixels.max_by_key((w, h))`?  The
    /// renderer can blit into stale `external_outputs` / `downscale_outputs`
    /// entries (registered for prior per-client targets that no longer match
    /// the current native).  Those produce extra `last_pixels` entries at
    /// the old, possibly-larger sizes.  Picking the largest entry as
    /// "native" then yields a stale value, which mis-clamps
    /// `per_client_encode_target` and triggers an avoidable encoder
    /// rebuild — and on aspect-ratio mismatches between old downscale
    /// targets and new compositor native, the encoder ends up sized for
    /// the stale target, freezing visible frames at the wrong size until
    /// the stale entry is cleared.
    native_sizes: HashMap<u16, (u32, u32)>,
    /// Audio capture pipeline (PipeWire daemon → in-process libpipewire capture → Opus encode).
    /// `None` when PipeWire is not available or `BLIT_AUDIO=0`.
    #[cfg(target_os = "linux")]
    audio_pipeline: Option<audio::AudioPipeline>,
    /// Shared fan-out state for audio — subscribers, catch-up ring,
    /// listener flag.  Persistent across pipeline restarts so clients
    /// stay subscribed even when the pipeline is restarted.  Always present on Linux;
    /// subscribe/unsubscribe succeeds even when the pipeline itself is
    /// absent (frames just don't flow until it's back).
    #[cfg(target_os = "linux")]
    audio_broadcast: Arc<audio::AudioBroadcast>,
    /// Compositor instance ID passed to `AudioPipeline::spawn()` so restarts
    /// reuse the same audio runtime directory.
    #[cfg(target_os = "linux")]
    audio_session_id: u16,
    /// When the last audio pipeline restart was attempted.  Used to enforce a
    /// cooldown so we don't spin on persistent failures.
    #[cfg(target_os = "linux")]
    last_audio_restart: Option<Instant>,
}

/// How long a surface's resize settle window stays open.  The first resize
/// of a window is dispatched immediately — a lone resize, and the start of a
/// drag, react at RTT rather than waiting out a timer — and everything that
/// arrives while the window is open is coalesced into a single configure at
/// the end of it.
///
/// This bounds compositor configure cycles (and the encoder recreation, hence
/// keyframe, that a size change forces) to one per surface per window, no
/// matter how fast sizes arrive.  It has to live here rather than only in the
/// client: the mediated size is a minimum across *all* subscribers, so a
/// second viewer resizing can churn a surface no single client is dragging,
/// and non-browser clients reach `C2S_SURFACE_RESIZE` with no debounce at all.
const SURFACE_RESIZE_SETTLE: Duration = Duration::from_millis(100);

/// What to do with a requested surface size.  Split out from
/// `Session::resize_surface` so the policy is testable without a live
/// compositor.
#[derive(Debug, PartialEq, Eq)]
enum ResizeAction {
    /// Already the size we last asked for — nothing to send.
    Ignore,
    /// Inside the settle window: keep it and let `tick` send it later.
    Hold,
    /// Send it now and open a new settle window.
    Dispatch,
}

fn resize_action(
    last_configured: Option<(u16, u16, u16)>,
    last_resize_at: Option<Instant>,
    now: Instant,
    requested: (u16, u16, u16),
) -> ResizeAction {
    // Compare against the last *requested* dimensions, not the composited
    // output dimensions (`info.width`/`info.height`).  The composited output
    // may be smaller when the Wayland client sets xdg_geometry (e.g. Chromium
    // excludes the title bar), so comparing against it would make every
    // resize look like a change, flooding the compositor with redundant
    // configures and re-creating the encoder (keyframe) on every tick during
    // a drag-resize.
    if last_configured == Some(requested) {
        return ResizeAction::Ignore;
    }
    match last_resize_at {
        Some(t) if now.duration_since(t) < SURFACE_RESIZE_SETTLE => ResizeAction::Hold,
        _ => ResizeAction::Dispatch,
    }
}

impl SharedCompositor {
    /// Hand a resize to the compositor and open a fresh settle window.
    fn dispatch_resize(
        &mut self,
        surface_id: u16,
        width: u16,
        height: u16,
        scale_120: u16,
        now: Instant,
    ) {
        self.pending_resize.remove(&surface_id);
        self.last_configured_size
            .insert(surface_id, (width, height, scale_120));
        self.last_resize_at.insert(surface_id, now);
        let _ = self
            .handle
            .command_tx
            .send(CompositorCommand::SurfaceResize {
                surface_id,
                width,
                height,
                scale_120,
            });
        // Commands are only drained at the top of the compositor's event
        // loop, which is otherwise parked in `dispatch()` for up to a
        // second.  Every other command site wakes it; this one did not, so
        // a configure sat in the queue until something unrelated ran the
        // loop — a Wayland event, the next blanket `RequestFrame`, or a
        // pointer/key event from the very surface being resized.  That last
        // one is why a resize looked like it only took effect once you
        // interacted with the window.
        self.handle.wake();
    }

    /// Dispatch every held-back resize whose settle window has closed.
    /// Returns the earliest instant at which a still-held resize comes due,
    /// so the delivery loop can park until exactly then.
    fn flush_due_resizes(&mut self, now: Instant) -> Option<Instant> {
        if self.pending_resize.is_empty() {
            return None;
        }
        let mut next: Option<Instant> = None;
        let sids: Vec<u16> = self.pending_resize.keys().copied().collect();
        for sid in sids {
            let due_at = self
                .last_resize_at
                .get(&sid)
                .map(|&t| t + SURFACE_RESIZE_SETTLE);
            match due_at {
                Some(due) if due > now => {
                    next = Some(next.map_or(due, |n: Instant| n.min(due)));
                }
                _ => {
                    if let Some(&(w, h, s)) = self.pending_resize.get(&sid) {
                        self.dispatch_resize(sid, w, h, s, now);
                    }
                }
            }
        }
        next
    }
}

fn encode_rgba_to_png(pixels: &[u8], width: u32, height: u32) -> Vec<u8> {
    let mut buf = Vec::new();
    {
        let expected = (width as usize) * (height as usize) * 4;
        let actual = pixels.len();
        if actual != expected {
            // Size mismatch — return a 1×1 red pixel PNG rather than panicking.
            let mut encoder = png::Encoder::new(&mut buf, 1, 1);
            encoder.set_color(png::ColorType::Rgba);
            encoder.set_depth(png::BitDepth::Eight);
            let mut writer = encoder.write_header().unwrap();
            writer.write_image_data(&[255, 0, 0, 255]).unwrap();
            eprintln!(
                "[capture] pixel buffer size mismatch: {width}x{height} expected {expected} got {actual}"
            );
        } else {
            let mut encoder = png::Encoder::new(&mut buf, width, height);
            encoder.set_color(png::ColorType::Rgba);
            encoder.set_depth(png::BitDepth::Eight);
            let mut writer = encoder.write_header().unwrap();
            writer.write_image_data(pixels).unwrap();
        }
    }
    buf
}

/// Encode RGBA pixels to AVIF.  `quality` 0 = lossless, 1–100 = lossy.
fn encode_rgba_to_avif(pixels: &[u8], width: u32, height: u32, quality: u8) -> Vec<u8> {
    let rgba: Vec<rgb::RGBA8> = pixels
        .chunks_exact(4)
        .map(|c| rgb::RGBA8::new(c[0], c[1], c[2], c[3]))
        .collect();
    let img = ravif::Img::new(&rgba[..], width as usize, height as usize);
    let q = if quality == 0 { 100.0 } else { quality as f32 };
    let encoder = ravif::Encoder::new()
        .with_quality(q)
        .with_alpha_quality(q)
        .with_speed(6)
        .with_alpha_color_mode(ravif::AlphaColorMode::UnassociatedClean)
        .with_num_threads(None);
    let result = encoder.encode_rgba(img).expect("AVIF encoding failed");
    result.avif_file
}

/// Encode RGBA pixels to the requested capture format.
fn encode_capture(pixels: &[u8], width: u32, height: u32, format: u8, quality: u8) -> Vec<u8> {
    match format {
        CAPTURE_FORMAT_AVIF => encode_rgba_to_avif(pixels, width, height, quality),
        _ => encode_rgba_to_png(pixels, width, height),
    }
}

/// Whether a target may be published as a GPU-only NV12 `OPAQUE_FD`
/// buffer.
///
/// The compositor publishes one representation per `(surface, w, h)`, so
/// this is a property of every subscriber at that size rather than of the
/// one being registered. Only NVENC can import an `OPAQUE_FD` handle; a
/// software or VA-API encoder sharing the size would be handed memory it
/// cannot map and would encode black. One dissenter puts everyone at that
/// size back on BGRA.
///
/// `others` yields each *other* subscriber's `(target, can_take_nv12)`.
/// A subscriber at a different size is irrelevant — it reads its own key.
fn nv12_opaque_safe_for_target(
    this_wants: bool,
    target: (u32, u32),
    others: impl Iterator<Item = (Option<(u32, u32)>, bool)>,
) -> bool {
    this_wants
        && others
            .into_iter()
            .all(|(their_target, they_want)| their_target != Some(target) || they_want)
}

async fn request_surface_capture_with_timeout(
    command_tx: std::sync::mpsc::Sender<CompositorCommand>,
    surface_id: u16,
    scale_120: u16,
    timeout: Duration,
) -> Option<(u32, u32, Vec<u8>)> {
    let (tx, rx) = std::sync::mpsc::sync_channel(1);
    command_tx
        .send(CompositorCommand::Capture {
            surface_id,
            scale_120,
            reply: tx,
        })
        .ok()?;

    // The compositor replies through a blocking std::sync::mpsc channel.
    // Wait for it off the async runtime so this request never stalls the
    // tokio worker thread or holds the Session mutex while blocked.
    tokio::task::spawn_blocking(move || rx.recv_timeout(timeout))
        .await
        .ok()?
        .ok()
        .flatten()
}

/// Per-surface bookkeeping for an active subscription.  Every field
/// defaults to "no-op" so a fresh `entry(sid).or_default()` is safe
/// even before any other state has been recorded.
#[derive(Default)]
struct SurfaceSubState {
    /// Active encoder for this surface.  `None` between encode jobs
    /// while the encoder is temporarily owned by the spawn_blocking
    /// task (see `encode_in_flight`) or before the first encode.
    encoder: Option<SurfaceEncoder>,
    /// Whether this subscriber's encoder can read a GPU-only NV12
    /// `OPAQUE_FD` buffer (i.e. is NVENC).
    ///
    /// Recorded rather than asked of `encoder` on demand, because that
    /// field is `None` while an encode task owns it — and a subscriber
    /// missed during that window would be read as "can take NV12" and
    /// handed a buffer it cannot map.
    wants_nv12_opaque: bool,
    /// Next tick this surface may send a frame (pacing deadline).
    next_send_at: Option<Instant>,
    /// Frames remaining in the post-subscribe burst window that
    /// bypass time-based pacing so bandwidth estimates ramp up fast
    /// on high-latency links.
    burst_remaining: u8,
    /// True while an encoder-creation spawn_blocking task is running
    /// for this surface.  Prevents dispatching a second creation in
    /// parallel and (via the `needs_new_encoder` path) skips encode
    /// dispatch until the creation task lands its result.
    creation_in_flight: bool,
    /// True while this surface's encoder is in an encode spawn_blocking
    /// task.  Prevents dispatching a parallel encode for the same
    /// surface (the encoder has been moved into the task).
    encode_in_flight: bool,
    /// Set if the in-flight encoder was invalidated by a codec /
    /// bandwidth / speed change (resubscribe) while encoding — the completion
    /// handler must drop the stale encoder instead of reinserting it.
    encoder_invalidated: bool,
    /// This client holds a decodable keyframe for this surface, so a delta
    /// frame is safe to send.  Cleared whenever the reference chain breaks
    /// or becomes unknown: encoder rebuilt or lost, surface resized,
    /// resubscribe with changed preferences, a send that failed, a Vulkan
    /// session withdrawn.  `false` — the default — means the next frame
    /// this surface sends must be a keyframe, which is right for a
    /// subscription that has never been sent one: it cannot decode a delta.
    ///
    /// Per surface, not per client.  A client watching several surfaces has
    /// an independent reference chain for each, and one surface's keyframe
    /// says nothing about another's.
    has_keyframe: bool,
    /// Pixel generation that was last encoded; used to skip re-
    /// encoding identical pixel data on subsequent ticks.
    last_encoded_gen: Option<u64>,
    /// Consecutive `nal_data=None` encodes.  After too many, the
    /// encoder is dropped so a fresh one is created on the next tick
    /// (bounds runaway encoder-recreation loops).
    nal_none_streak: u32,
    /// When the streak last latched (hit the drop threshold).  Auto-
    /// clears after a backoff so a freshly-created encoder can retry
    /// without needing a user-driven resize/resubscribe.
    nal_none_latched_at: Option<Instant>,
    /// Consecutive encoder creations that came back with nothing, cleared
    /// by the first that succeeds.
    ///
    /// A failure at a size some backend *could* have carried is retried at
    /// that size rather than shrinking the surface, since the usual cause
    /// is momentary.  This counts how long "momentary" has gone on: past
    /// [`CREATE_FAILURES_BEFORE_DEGRADE`] the surface comes down to what
    /// the whole chain clears, because a smaller picture beats none.
    create_failures: u32,
    /// Per-surface codec support override from C2S_SURFACE_SUBSCRIBE
    /// (bitmask of CODEC_SUPPORT_*).  0 = defer to client-wide
    /// `surface_codec_support`.
    codec_override: u8,
    /// Per-surface bandwidth override.  `None` = use server default.
    bandwidth_override: Option<SurfaceBandwidth>,
    /// Per-surface speed override.  `None` = use server default.
    speed_override: Option<SurfaceSpeed>,
    /// Fixed encode size this client asked for on C2S_SURFACE_SUBSCRIBE.
    ///
    /// `Some` opts the subscription out of surface-size mediation entirely:
    /// the compositor surface keeps whatever size the *mediated* viewers
    /// want, and this client is served a server-side downscale of it.  That
    /// is the whole point — a side-panel thumbnail can ask for a card-sized
    /// stream without dragging the Wayland window down to a card for
    /// everyone watching it full size.
    ///
    /// `None` — the default — means the client participates in mediation via
    /// C2S_SURFACE_RESIZE like any other viewer.
    scaled_target: Option<(u16, u16)>,
    /// EWMA of this surface's encoded frame size in bytes.  Per surface
    /// (unlike `avg_surface_frame_bytes`) so a client watching two
    /// surfaces can split its bandwidth budget between them.  0 = no
    /// frame measured yet.
    frame_bytes: f32,
    /// Quantizer the adaptive controller is currently asking for.  `None`
    /// = run at the ceiling (`bandwidth_override` / server default).
    adaptive_quantizer: Option<u8>,
    /// When the controller last moved `adaptive_quantizer`, for hysteresis.
    rate_stepped_at: Option<Instant>,
    /// Bit per Vulkan Video encoder the compositor has refused this client on
    /// this surface (see [`SurfaceEncoderPreference::vulkan_refusal_bit`]).
    /// Latched so selection stops offering that one — otherwise the next tick
    /// re-selects it, is refused again, and the surface never reaches a
    /// server-side encoder.  Cleared when the surface is invalidated
    /// (resize / destroy), which is a fair time to retry.
    ///
    /// Per encoder rather than one flag for the tier: with `av1-vulkan` ahead
    /// of `h264-vulkan` in the default list, a single flag let an AV1 refusal
    /// disqualify H.264 too, losing a path that works.
    vulkan_refused: u8,
    /// Last per-client downscale target dims registered with the
    /// compositor.  Used to send `ClearDownscaleTarget` for the old
    /// dims when the encoder is recreated at a new size, so stale
    /// downscale outputs don't accumulate in the compositor.  `None`
    /// = no target registered yet (or the encoder was an external
    /// GBM path that uses `external_outputs` instead).
    last_registered_target: Option<(u32, u32)>,
    /// The compositor native size `last_registered_target` was inscribed
    /// into, as the compositor has it stamped.  The compositor refuses to
    /// fill a target whose stamp no longer matches what it is compositing,
    /// so this has to be refreshed whenever the native moves — including
    /// when the target itself lands on the same numbers as before and the
    /// encoder is therefore not rebuilt.  Without that, a surface nudged a
    /// pixel by *another* viewer's resize would leave this one's target
    /// stamped for a size that will never come back, and it would stop
    /// receiving frames entirely.
    last_registered_native: Option<(u32, u32)>,
    /// Which preference won the fallback chain for this surface, once one
    /// has.  Sizing prefers it over guessing: before an encoder exists we
    /// size for the most capable backend the client could decode, and this
    /// replaces that guess with the answer.  `None` = no encoder built yet.
    selected_encoder: Option<SurfaceEncoderPreference>,
    /// Latched when a creation attempt was refused for being too large.
    /// Sizes the next attempt to the ceiling *every* backend in the chain
    /// clears, so a surface no wide-format encoder can carry still gets a
    /// picture instead of retrying the same oversized request forever.
    ///
    /// Cleared on a prefs-changed resubscribe, deliberately *not* on the
    /// smaller creation that follows the refusal: that creation can be won by
    /// a backend whose own ceiling is wider than the size just refused, and
    /// clearing here would size the surface straight back up into it.  See
    /// the creation completion handler.
    encoder_cap_degraded: bool,
}

/// The codec bitmask in force for one (client, surface) pair: the
/// per-surface override from `C2S_SURFACE_SUBSCRIBE` when set, else the
/// client-wide value.  0 means "accept anything".
fn surface_codec_support(client: &ClientState, surface_id: u16) -> u8 {
    client
        .surface_subs
        .get(&surface_id)
        .map(|s| s.codec_override)
        .filter(|&c| c != 0)
        .unwrap_or(client.surface_codec_support)
}

/// How large a frame this client may be served for `surface_id`.
///
/// The encoder ceiling is not a property of the chain as a whole — H.264
/// stops at 3840x2160 and hardware AV1 goes to 8192x4352 — so taking the
/// tightest cap across every configured preference would hold an AV1 viewer
/// to H.264's limit purely because H.264 is in the list as a fallback.
/// Instead:
///
///   - Once the chain has resolved, the winner's own ceiling is the truth.
///   - Before that, size for the most capable backend the client can decode
///     and let `SurfaceEncoder::new` skip the ones that can't carry it.
///   - If that request was refused for size, fall back to the ceiling every
///     eligible backend clears.  This is the one case that costs a round
///     trip, and it converges after exactly one.
///
/// The result is then intersected with what the client said its decoder can
/// handle, because a ceiling the encoder clears is worthless if the browser
/// refuses the bitstream.
///
/// `None` (empty or fully-ineligible preference list) means no cap.
fn surface_encode_cap(
    prefs: &[SurfaceEncoderPreference],
    client: &ClientState,
    surface_id: u16,
) -> Option<(u16, u16)> {
    let codec_support = surface_codec_support(client, surface_id);
    let sub = client.surface_subs.get(&surface_id);
    let eligible: Vec<_> = prefs
        .iter()
        .copied()
        .filter(|p| p.supported_by_client(codec_support))
        .collect();
    let (cw, ch) = if sub.is_some_and(|s| s.encoder_cap_degraded) {
        SurfaceEncoderPreference::tightest_for_list(&eligible)
    } else if let Some(pref) = sub.and_then(|s| s.selected_encoder) {
        Some(pref.max_dimensions())
    } else {
        SurfaceEncoderPreference::widest_for_list(&eligible)
    }?;
    let (dw, dh) = match client.surface_max_decode {
        // Undeclared: hold at the H.264 ceiling.  Every client predating the
        // field lands here, and none of them was being served more than that
        // before, so this is the status quo rather than a new restriction.
        (0, 0) => SurfaceEncoderPreference::H264Software.max_dimensions(),
        declared => declared,
    };
    Some((cw.min(dw), ch.min(dh)))
}

/// How many creations in a row may come back empty at a size some backend
/// could have carried before the surface is brought down anyway.
///
/// Failures there are usually momentary — an allocation, a busy engine, a
/// compositor buffer not imported yet — and retrying at the same size keeps
/// the viewer's resolution.  But a backend can also fail only at scale (VRAM
/// for a 5K frame, a per-resolution driver limit the reported maximum does
/// not admit to) and go on doing it, and then holding out for the large size
/// means holding out forever.  Retries are spaced by
/// `NAL_NONE_RETRY_BACKOFF`, so this is a few seconds of black at worst.
const CREATE_FAILURES_BEFORE_DEGRADE: u32 = 3;

/// Whether a failed encoder creation should narrow this surface's ceiling
/// rather than simply be tried again.
///
/// True only when nothing is left that could have carried the frame: every
/// backend the client can decode and this host can run is too small for it.
/// Then a smaller surface is the only way to a picture, and the caller
/// latches `encoder_cap_degraded`.
///
/// The distinction matters because that latch does not clear until the
/// client resubscribes.  If a backend fits the frame and works on this host,
/// its failure was a momentary one — an allocation, a busy engine — and
/// another attempt at the same size is the right answer; treating it as a
/// size problem would pin the viewer to 2160p for the rest of the session.
/// A backend that goes on failing anyway is caught by
/// [`CREATE_FAILURES_BEFORE_DEGRADE`] instead, so "momentary" cannot mean
/// "forever".
///
/// `available` reports whether a backend has ever built an encoder here; it
/// is a parameter so this stays a decision about the arguments rather than
/// about process-global state.
fn refused_for_size(
    prefs: &[SurfaceEncoderPreference],
    codec_support: u8,
    width: u32,
    height: u32,
    available: impl Fn(SurfaceEncoderPreference) -> bool,
) -> bool {
    !prefs
        .iter()
        .copied()
        .filter(|p| p.supported_by_client(codec_support))
        .filter(|p| available(*p))
        .any(|p| p.fits(width, height))
}

struct ClientState {
    tx: mpsc::UnboundedSender<Vec<u8>>,
    outbox_queued_frames: Arc<AtomicUsize>,
    outbox_queued_bytes: Arc<AtomicUsize>,
    /// Microseconds the writer task has spent blocked inside a socket
    /// write, accumulated.  A blocked write is the earliest and cheapest
    /// congestion signal available; the bandwidth controller samples the
    /// delta between its steps rather than the absolute value.
    write_blocked_us: Arc<AtomicU64>,
    /// `write_blocked_us` as of the controller's last step, so it can read a
    /// delta out of a monotonically growing counter.
    write_blocked_us_seen: u64,
    /// Dedicated channel for audio frames.  The writer task selects on this
    /// with higher priority than the main outbox so audio is never starved
    /// by large video/terminal messages.
    #[cfg(target_os = "linux")]
    audio_tx: mpsc::UnboundedSender<Vec<u8>>,
    lead: Option<u16>,
    subscriptions: HashSet<u16>,
    /// Active surface subscriptions for this client.
    surface_subscriptions: HashSet<u16>,
    /// Whether this client is subscribed to audio frames.
    #[cfg(target_os = "linux")]
    audio_subscribed: bool,
    /// Per-client audio bitrate preference in kbps from C2S_AUDIO_SUBSCRIBE.
    /// 0 means use the server/env default.
    #[cfg(target_os = "linux")]
    audio_bitrate_kbps: u16,
    view_sizes: HashMap<u16, (u16, u16)>,
    scroll_offsets: HashMap<u16, usize>,
    scroll_caches: HashMap<u16, FrameState>,
    last_sent: HashMap<u16, FrameState>,
    last_used_rows_sent: HashMap<u16, u16>,
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
    /// EWMA of surface (video) frame payload size in bytes.  Tracked
    /// separately from terminal frame sizes so surface pacing uses
    /// `goodput_bps / avg_surface_frame_bytes` without polluting
    /// terminal congestion control estimates.
    avg_surface_frame_bytes: f32,
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
    /// Throttle timestamp for "[surface-gate] blocked" diagnostic logs.
    last_window_blocked_log: Instant,
    /// Throttle timestamp for "[encode-skip]" diagnostic logs.
    last_skip_log: Instant,
    /// Counters for silent encode-skip paths, reset each pacing log tick.
    skip_same_gen_count: u32,
    skip_in_flight_count: u32,
    skip_pacing_count: u32,
    skip_vulkan_await_count: u32,
    /// Client had no subscriptions when encode pass ran.
    skip_no_subs_count: u32,
    /// Client not subscribed to a given sid in pixel_snapshot.
    skip_not_subbed_count: u32,
    /// last_pixels entry missing / dimensions mismatched pixel_snapshot.
    skip_last_pixels_mismatch_count: u32,
    /// Iterations through pixel_snapshot for this client (sanity check).
    encode_loop_iters: u32,
    goodput_window_bytes: usize,
    goodput_window_start: Instant,
    /// Per-surface encode/pacing/override state.  Holds every piece of
    /// bookkeeping the encode loop maintains between frames for a
    /// subscribed surface.  Entries are created lazily via
    /// `entry(sid).or_default()` on first touch and dropped wholesale
    /// on UNSUBSCRIBE / SurfaceDestroyed.
    surface_subs: HashMap<u16, SurfaceSubState>,
    /// Surfaces that use Vulkan Video encoding in the compositor rather than
    /// a local SurfaceEncoder.  Maps surface_id → (encoder_name, codec_flag).
    vulkan_video_surfaces: HashMap<u16, (&'static str, u8)>,
    /// Surface frames in flight — separate from terminal inflight so surface
    /// ACKs feed shared RTT / goodput without corrupting terminal frame-size
    /// averages or probe_frames.
    surface_inflight_frames: VecDeque<SurfaceInFlightFrame>,
    /// Per-client desired surface sizes (surface_id → (width, height, scale_120, codec_support)).
    /// Mirrors `view_sizes` for PTYs: the server mediates across all clients
    /// and picks min(width), min(height), max(scale).
    /// `scale_120` is the DPR in 1/120th units (Wayland convention): 240 = 2×.
    surface_view_sizes: HashMap<u16, (u16, u16, u16)>,
    /// Intersection of codec support across all surfaces for this client.
    /// Used to pick an encoder the client can decode.  0 = accept anything.
    surface_codec_support: u8,
    /// Largest frame this client's video decoder reported it can handle,
    /// from `C2S_CLIENT_FEATURES`.  `(0, 0)` = not declared, which covers
    /// every client predating the field; those are held to the H.264
    /// ceiling, the most they could have been served anyway.
    ///
    /// Separate from `surface_codec_support` because the two answer
    /// different questions: the bitmask says *which* codecs decode, this
    /// says *how large* they decode.  A browser that reports AV1 support
    /// from a 1080p probe has said nothing about 5K.
    surface_max_decode: (u16, u16),
    /// Evdev keycodes currently held down by this client on compositor
    /// surfaces.  On disconnect we send synthetic key-up events for each
    /// so modifiers don't stay stuck and keys don't auto-repeat forever.
    pressed_surface_keys: HashSet<u32>,
}

struct InFlightFrame {
    sent_at: Instant,
    bytes: usize,
    paced: bool,
}

/// A surface frame handed to the writer, awaiting its C2S_SURFACE_ACK.
/// Carries the surface id so an ack is matched to the frame it actually
/// acknowledges: with two surfaces subscribed the acks interleave, and
/// popping blindly would credit one surface's bytes with the other's
/// delivery time.
struct SurfaceInFlightFrame {
    sent_at: Instant,
    bytes: usize,
    surface_id: u16,
}

/// Floor on the unacked-surface-frame cap.  Also the whole cap on any
/// ordinary link: at 20 ms RTT and 60 Hz the window is about four frames,
/// so 64 is already enormous headroom.
const SURFACE_INFLIGHT_MIN: usize = 64;

/// Ceiling regardless of bandwidth-delay product, so a client reporting a
/// nonsense display rate or a wildly inflated RTT cannot grow the queue
/// without bound.  Each entry is a few dozen bytes, so even this is
/// kilobytes, not megabytes.
const SURFACE_INFLIGHT_HARD_MAX: usize = 512;

/// Cap on unacked surface frames tracked per client.  A frame can go
/// unacked forever (client teardown mid-flight, a transport that drops
/// it), and every orphan permanently offsets the queue — so the oldest
/// entries are evicted rather than trusted.
///
/// Derived from the bandwidth-delay product rather than fixed, because a
/// constant is two different things at two different latencies.  At 1 s RTT
/// and 60 Hz the link legitimately holds 60 frames, so a flat cap of 64 sat
/// right on top of the steady state: `surface_frame_window` returned 71,
/// above the cap, which made `inflight > window` unreachable and silenced
/// the rate controller entirely — and at 90 Hz the deque evicted live
/// entries continuously, so `record_surface_ack` matched each ACK to a
/// newer frame than the one it belonged to and understated delivery time.
/// Twice the window keeps the backoff comparison reachable while still
/// bounding orphans.
fn surface_inflight_cap(client: &ClientState) -> usize {
    surface_frame_window(client)
        .saturating_mul(2)
        .clamp(SURFACE_INFLIGHT_MIN, SURFACE_INFLIGHT_HARD_MAX)
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
    //
    // The backoff is steep: at the block threshold (backlog>8) we've
    // already dropped to display_fps/4.  A gentler schedule (4/backlog)
    // held 48fps at backlog=10 for software-encoded 1080p, which is
    // faster than the browser can decode → backlog never drains, the
    // hard block stays latched, and encoding stalls entirely.
    //
    // Trigger threshold (backlog > 4) gives a few frames of transient
    // headroom before backoff engages — at 120 Hz, a 30 fps source naturally
    // queues 1-2 frames during decoder hiccups, and triggering backoff there
    // chops the rate just to absorb normal jitter.
    let backlog = client.browser_backlog_frames as f32;
    if backlog > 4.0 {
        fps = fps.min(fps * (2.0 / backlog));
    }

    if client.browser_ack_ahead_frames > 4 {
        fps = fps.min(client.display_fps.max(1.0) * 0.5);
    }
    if client.browser_ack_ahead_frames > 8 {
        fps = fps.min(client.display_fps.max(1.0) * 0.25);
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
    if client.lead.is_some() && throughput_limited(client) {
        // Only budget preview bandwidth when the link is actually saturated.
        // Without this, large preview frames (e.g. 12 KB) at 30 fps consume
        // 360 KB/s, starving the lead even when lead frames are tiny.
        // On fast links (localhost, LAN), previews run at display_fps.
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

/// Unacked frames one surface may hold before pacing backs off: what a
/// healthy link should have in flight at the client's display rate, RTT
/// included.  Deriving it from RTT rather than a constant keeps a distant
/// but perfectly healthy link from being mistaken for a struggling one —
/// at 100 ms RTT and 60 Hz, six frames in flight is correct, not congested.
///
/// `surface_inflight_cap` is derived from this, at twice the window, so the
/// `inflight > window` comparison stays reachable at any bandwidth-delay
/// product — a flat cap silenced the backoff entirely once the window grew
/// past it (1 s RTT at 60 Hz wants 71 against a cap of 64).  The hard
/// ceiling can still bind for an absurd RTT/rate pair, and there
/// `surface_window_open`'s outbox backpressure remains the backstop.
fn surface_frame_window(client: &ClientState) -> usize {
    frame_window(effective_rtt_ms(client), client.display_fps.max(1.0))
}

fn surface_inflight_for(client: &ClientState, surface_id: u16) -> usize {
    client
        .surface_inflight_frames
        .iter()
        .filter(|f| f.surface_id == surface_id)
        .count()
}

/// Surface frame rate.
///
/// Deliberately *not* `browser_pacing_fps`.  That function's inputs are
/// terminal metrics: `browser_backlog_frames` carries the client's
/// `pendingAppliedFrames`, which counts applied-but-unpainted *terminal*
/// frames and is cleared when a terminal paints
/// (`TerminalStore.noteFrameRendered`).  Pacing video off it meant a burst
/// of shell output throttled an unrelated video surface — steeply, since
/// the terminal schedule quarters the rate by a backlog of 8 — and because
/// the client only reports every 250 ms, the cut outlived the burst that
/// caused it.
///
/// Surfaces carry their own signal instead: frames sent but not yet acked
/// for *this* surface.  The browser acks a surface frame as soon as it
/// hands the chunk to its decoder, so a queue deeper than the link should
/// hold is real congestion — the network or the decoder input — rather
/// than a busy paint loop somewhere else in the page.
fn surface_pacing_fps(client: &ClientState, surface_id: u16) -> f32 {
    let fps = client.display_fps.max(1.0);
    let window = surface_frame_window(client);
    let inflight = surface_inflight_for(client, surface_id);
    if inflight > window {
        // Proportional, not stepped: the terminal path's steep schedule
        // exists to break apply-time death spirals, which do not apply
        // here — an overdeep surface queue drains on its own once the rate
        // eases, and `surface_window_open`'s outbox backpressure is still
        // the hard backstop for a client that has genuinely stopped.
        (fps * window as f32 / inflight as f32).max(1.0)
    } else {
        fps
    }
}

fn surface_send_interval(client: &ClientState, surface_id: u16) -> Duration {
    Duration::from_secs_f64(1.0 / surface_pacing_fps(client, surface_id).max(1.0) as f64)
}

/// Slowest surface pacing across this client, for the metrics line.
fn slowest_surface_pacing_fps(client: &ClientState) -> f32 {
    client
        .surface_subs
        .keys()
        .map(|&sid| surface_pacing_fps(client, sid))
        .fold(f32::INFINITY, f32::min)
        .min(client.display_fps.max(1.0))
}

/// Whether the next frame sent to `client` for `sid` must be a keyframe.
///
/// Per surface: a client watching several surfaces keeps an independent
/// decoder reference chain for each, so one surface's keyframe says nothing
/// about another's.  A surface with no sub state yet has been sent nothing
/// and cannot decode a delta, so it owes one.
fn owes_keyframe(client: &ClientState, sid: u16) -> bool {
    !client
        .surface_subs
        .get(&sid)
        .is_some_and(|s| s.has_keyframe)
}

/// What an encode result leaves in the sub's `last_encoded_gen`.
///
/// That field is the "already shown to this client" mark the encode loop's
/// `unchanged` gate reads, so only a generation that actually produced a
/// bitstream may advance it.  `encode_pixels` returns `None` as ordinary
/// control flow — rav1e asking for more data before it emits anything, a
/// DMA-BUF that could not be mapped, a zero-size x264 output — and marking
/// one of those as encoded makes the gate skip that generation forever.
/// While the surface keeps painting, the next generation covers for it.
/// When the surface goes still on exactly that frame — a video reaching its
/// last frame, an app settling after its final repaint — nothing covers for
/// it, and the client is left holding the frame before it.
fn encoded_generation(
    previous: Option<u64>,
    generation: u64,
    produced_output: bool,
) -> Option<u64> {
    if produced_output {
        Some(generation)
    } else {
        previous
    }
}

// ---------------------------------------------------------------------------
// Adaptive bandwidth
//
// The configured bandwidth is a CEILING, not an operating point: a surface
// never spends more than it was granted, but the server spends less when the
// link cannot carry it.  The controller compares what frames actually cost
// against what the measured goodput affords at the current pacing rate, and
// walks the AV1 quantizer between the ceiling and a floor.  It is a delay-
// free loop on purpose — every input is already measured per client (goodput
// from surface ACKs, blocked writes from the writer task) so no new wire
// messages or client cooperation are needed.
// ---------------------------------------------------------------------------

/// Worst quantizer the controller will fall back to.  Past this the picture
/// is not worth sending; dropping frame rate is the better trade and pacing
/// already does that.
const ADAPTIVE_MAX_QUANTIZER: u8 = 200;
/// Fraction of measured goodput a surface may budget for.  The remainder is
/// headroom: aiming at 100% of an estimate that is itself derived from what
/// was sent guarantees a standing queue.
const ADAPTIVE_GOODPUT_SHARE: f32 = 0.8;
/// Minimum gap between steps, so the loop settles instead of oscillating.
const ADAPTIVE_STEP_INTERVAL: Duration = Duration::from_millis(250);
/// Quantizer step when merely off-budget.
const ADAPTIVE_STEP: u8 = 6;
/// A backend that cannot retarget in place has to be rebuilt, which costs a
/// keyframe — only worth it past this much accumulated drift.
const ADAPTIVE_REBUILD_STEP: u8 = 24;
/// Blocked-write time within one step interval that counts as congestion.
/// A write that blocks for a tenth of the interval means the socket, not the
/// encoder, is setting the pace.
const WRITE_BLOCKED_CONGESTED_US: u64 = 25_000;
/// Gap between refinement steps on a surface that has stopped changing.
/// Longer than `ADAPTIVE_STEP_INTERVAL` because each step costs a keyframe
/// and there is no deadline to meet — nothing is moving.
const STILL_REFRESH_INTERVAL: Duration = Duration::from_millis(400);
/// Smallest quantizer improvement worth spending a keyframe on.
const STILL_REFINE_MIN_STEP: u8 = 16;

/// Next quantizer when refining a frozen picture back toward the ceiling.
///
/// Halves the remaining distance, with a floor on the step size so a wide
/// gap does not cost a long tail of barely-better keyframes.  The last step
/// always lands exactly on the ceiling.
fn refine_toward_ceiling(current: u8, ceiling: u8) -> u8 {
    let gap = current.saturating_sub(ceiling);
    if gap == 0 {
        return ceiling;
    }
    let step = gap.div_ceil(2).max(STILL_REFINE_MIN_STEP);
    current.saturating_sub(step).max(ceiling)
}

/// One surface's view of the link, as the controller sees it.
#[derive(Clone, Copy, Debug)]
struct RateSample {
    /// Best (lowest) quantizer allowed: the configured ceiling.
    ceiling: u8,
    /// Quantizer currently in effect.
    current: u8,
    /// Bytes per frame the link affords this surface.
    budget_bytes: f32,
    /// Bytes per frame this surface is actually producing.
    observed_bytes: f32,
    /// The transport told us it could not keep up (blocked write or a
    /// backed-up outbox) since the last step.
    congested: bool,
    /// Nothing on the path is straining: writes aren't blocking, the
    /// browser isn't backlogged, acks aren't piling up.  Goodput measured
    /// in this state describes our own send rate, not link capacity, so a
    /// budget derived from it is not evidence of anything.
    app_limited: bool,
}

/// Next quantizer for a surface, clamped to `[ceiling, ADAPTIVE_MAX_QUANTIZER]`.
///
/// Multiplicative decrease on congestion, additive otherwise, and additive
/// increase back toward the ceiling only when frames are comfortably inside
/// budget — a surface that is exactly on budget is left alone.
fn next_quantizer(sample: RateSample) -> u8 {
    let ceiling = sample.ceiling.min(ADAPTIVE_MAX_QUANTIZER);
    let clamp = |q: i32| q.clamp(ceiling as i32, ADAPTIVE_MAX_QUANTIZER as i32) as u8;
    if sample.congested {
        // Back off hard: the queue is already forming, and the frames that
        // caused it are still in flight.
        return clamp(sample.current as i32 + (sample.current as i32 / 8).max(12));
    }
    // An unstrained link never justifies getting worse, whatever the budget
    // comparison below would say: on an app-limited link goodput converges
    // to whatever we are currently sending, so "over budget" is
    // self-fulfilling — smaller frames drag the measurement down, which
    // shrinks the budget, which asks for smaller frames again, all the way
    // to the floor (a lone spinner animation used to ride this spiral to
    // quantizer 200).  Spend the idle link on walking back to the
    // configured quality instead; if that turns out to be more than the
    // path can carry, the pressure signals return and the backoff above
    // answers them.
    if sample.app_limited {
        return clamp(sample.current as i32 - ADAPTIVE_STEP as i32);
    }
    // No usable budget yet (no goodput estimate, or no frame measured):
    // hold position rather than guess.
    if sample.budget_bytes <= 0.0 || sample.observed_bytes <= 0.0 {
        return clamp(sample.current as i32);
    }
    if sample.observed_bytes > sample.budget_bytes * 1.25 {
        clamp(sample.current as i32 + ADAPTIVE_STEP as i32)
    } else if sample.observed_bytes < sample.budget_bytes * 0.75 {
        clamp(sample.current as i32 - ADAPTIVE_STEP as i32)
    } else {
        clamp(sample.current as i32)
    }
}

/// Per-frame byte budget for one surface: its share of the client's measured
/// goodput at the current pacing rate.  A client watching two surfaces
/// splits by how many bytes each is actually producing, so a big active
/// window is not starved by a small idle one.
fn surface_budget_bytes(client: &ClientState, surface_id: u16) -> f32 {
    let fps = surface_pacing_fps(client, surface_id).max(1.0);
    let total: f32 = client.surface_subs.values().map(|s| s.frame_bytes).sum();
    let own = client
        .surface_subs
        .get(&surface_id)
        .map_or(0.0, |s| s.frame_bytes);
    let share = if total > 0.0 && own > 0.0 {
        own / total
    } else {
        let subs = client.surface_subs.len().max(1) as f32;
        1.0 / subs
    };
    client.goodput_bps * ADAPTIVE_GOODPUT_SHARE * share / fps
}

/// Bandwidth a surface should encode at right now: the configured ceiling,
/// lowered by whatever the controller has decided the link can carry.
fn resolve_bandwidth(
    client: &ClientState,
    default: SurfaceBandwidth,
    surface_id: u16,
) -> SurfaceBandwidth {
    let sub = client.surface_subs.get(&surface_id);
    let ceiling = sub.and_then(|s| s.bandwidth_override).unwrap_or(default);
    match sub.and_then(|s| s.adaptive_quantizer) {
        Some(q) if q > ceiling.av1_quantizer() as u8 => SurfaceBandwidth::Custom { quantizer: q },
        _ => ceiling,
    }
}

/// Run one step of the controller for a surface and report whether the live
/// encoder now needs rebuilding (the backend could not retarget in place and
/// the drift is large enough to be worth a keyframe).
/// Outcome of one adaptive step for a surface.
struct AdaptiveStep {
    /// The quantizer moved, and the encoder in hand could not take the new
    /// rate in place, so it has to be rebuilt (paying a keyframe).
    rebuild: bool,
    /// The quantizer the surface should now encode at, when it moved.
    /// A compositor-resident encoder is retargeted with this; a local one
    /// has already been retargeted in place.
    quantizer: Option<u8>,
}

///
/// `unchanged` says the surface is showing a frame the client already has.
/// In that mode the controller stops rate-controlling — the budget it would
/// judge against describes motion that has stopped — and instead walks the
/// quantizer back toward the ceiling so a picture that is going to sit on
/// screen ends up as good as the configuration allows.
fn step_adaptive_bandwidth(
    client: &mut ClientState,
    default: SurfaceBandwidth,
    surface_id: u16,
    now: Instant,
    unchanged: bool,
) -> AdaptiveStep {
    let blocked_us = client.write_blocked_us.load(Ordering::Relaxed);
    let congested = outbox_backpressured(client)
        || blocked_us.saturating_sub(client.write_blocked_us_seen) > WRITE_BLOCKED_CONGESTED_US;
    // Pressure evidence, from strongest to weakest: the writer blocking on
    // the socket, the browser reporting an apply backlog (>4 is where
    // pacing starts backing off too), and this surface's unacked frames
    // piling up well past what send-rate × RTT parks in flight.  With none
    // of these present the link is app-limited and the budget below says
    // nothing about capacity.
    let surface_inflight = client
        .surface_inflight_frames
        .iter()
        .filter(|f| f.surface_id == surface_id)
        .count();
    // Deliberately SURFACE_INFLIGHT_MIN, not the derived cap.  This is the
    // quality controller, not the pacer: it asks "is the link app-limited,
    // so the measured budget says nothing about capacity".  Wiring it to
    // `surface_inflight_cap(client) / 2` — which equals surface_frame_window
    // whenever the cap is unclamped — makes the boundary move with RTT, so
    // at 1 s / 60 Hz it lands at 71 against a steady-state inflight of ~60
    // and flips app_limited on a link that is simply deep, not idle.  The
    // threshold here must stay a constant; it happens to be the same 32 the
    // flat cap used to give.
    let app_limited = !congested
        && client.browser_backlog_frames <= 4
        && surface_inflight < SURFACE_INFLIGHT_MIN / 2;
    let budget_bytes = surface_budget_bytes(client, surface_id);
    let ceiling = client
        .surface_subs
        .get(&surface_id)
        .and_then(|s| s.bandwidth_override)
        .unwrap_or(default);
    let ceiling_q = ceiling.av1_quantizer().min(255) as u8;

    let held = AdaptiveStep {
        rebuild: false,
        quantizer: None,
    };
    let Some(sub) = client.surface_subs.get_mut(&surface_id) else {
        return held;
    };
    let interval = if unchanged {
        STILL_REFRESH_INTERVAL
    } else {
        ADAPTIVE_STEP_INTERVAL
    };
    if sub
        .rate_stepped_at
        .is_some_and(|at| now.duration_since(at) < interval)
    {
        return held;
    }
    let current = sub.adaptive_quantizer.unwrap_or(ceiling_q).max(ceiling_q);
    let next = if unchanged {
        // A frozen picture is exactly when the link is idle and the bits
        // are affordable — unless the backlog says otherwise, in which
        // case leave it alone rather than pile a keyframe onto a queue.
        if congested {
            current
        } else {
            refine_toward_ceiling(current, ceiling_q)
        }
    } else {
        next_quantizer(RateSample {
            ceiling: ceiling_q,
            current,
            budget_bytes,
            observed_bytes: sub.frame_bytes,
            congested,
            app_limited,
        })
    };
    sub.rate_stepped_at = Some(now);
    client.write_blocked_us_seen = blocked_us;
    if next == current {
        // Nothing moved.  Reporting a step anyway would be harmless for a
        // live surface (a redundant set to the rate already in effect) but
        // a still one reads it as "the picture improved" and spends a
        // keyframe on it, every interval, forever.
        return held;
    }
    sub.adaptive_quantizer = if next > ceiling_q { Some(next) } else { None };

    // Retarget the live encoder in place if it can be; otherwise ask for a
    // rebuild, but only once the drift is big enough to pay for a keyframe.
    let target = SurfaceBandwidth::Custom { quantizer: next };
    let rebuild = match sub.encoder.as_mut() {
        Some(enc) => {
            if enc.set_bandwidth(target) {
                false
            } else {
                let running = enc.encoding().bandwidth.av1_quantizer() as i32;
                (next as i32 - running).abs() >= ADAPTIVE_REBUILD_STEP as i32
            }
        }
        // No encoder in hand (between jobs, in flight, or owned by the
        // compositor): the next creation picks the new bandwidth up from
        // `resolve_bandwidth`, and the caller retargets a Vulkan session.
        None => false,
    };
    AdaptiveStep {
        rebuild,
        quantizer: Some(next),
    }
}

/// Emit a pacing-metrics line for this client if 10s have elapsed since
/// the last one.  Called both from the ACK handler and from `tick()` so
/// an idle client (no ACK traffic) still gets periodic metrics.
fn maybe_log_pacing_metrics(sess: &mut Session, client_id: u64, verbose: bool) {
    let Some(c) = sess.clients.get_mut(&client_id) else {
        return;
    };
    if c.last_log.elapsed().as_secs_f32() < 10.0 {
        return;
    }
    let log_elapsed = c.last_log.elapsed().as_secs_f32().max(1.0e-3);
    let paced_fps = pacing_fps(c);
    let display_need_bps_v = display_need_bps(c);
    let surface_fps = slowest_surface_pacing_fps(c);
    let frames_sent = c.frames_sent;
    let acks_recv = c.acks_recv;
    let rtt_ms = c.rtt_ms;
    let min_rtt_ms = path_rtt_ms(c);
    let eff_rtt_ms = window_rtt_ms(c);
    let inflight_bytes = c.inflight_bytes;
    let delivery_bps = c.delivery_bps;
    let goodput_ewma_bps = c.goodput_bps;
    let goodput_jitter_bps = c.goodput_jitter_bps;
    let max_goodput_jitter_bps = c.max_goodput_jitter_bps;
    let avg_frame_bytes = c.avg_frame_bytes;
    let avg_paced_frame_bytes = c.avg_paced_frame_bytes;
    let avg_preview_frame_bytes = c.avg_preview_frame_bytes;
    let display_fps = c.display_fps;
    let probe_frames = c.probe_frames;
    let goodput_bps = c.acked_bytes_since_log as f32 / log_elapsed;
    let window_frames = target_frame_window(c);
    let window_bytes = target_byte_window(c);
    let outbox_frames = outbox_queued_frames(c);
    let browser_backlog_frames = c.browser_backlog_frames;
    let browser_ack_ahead_frames = c.browser_ack_ahead_frames;
    let browser_apply_ms = c.browser_apply_ms;
    let avg_surface_frame_bytes = c.avg_surface_frame_bytes;
    let skip_same_gen = c.skip_same_gen_count;
    let skip_in_flight = c.skip_in_flight_count;
    let skip_pacing = c.skip_pacing_count;
    let skip_vk_await = c.skip_vulkan_await_count;
    let skip_no_subs = c.skip_no_subs_count;
    let skip_not_subbed = c.skip_not_subbed_count;
    let skip_mismatch = c.skip_last_pixels_mismatch_count;
    let loop_iters = c.encode_loop_iters;
    let own_subs: usize = c.surface_subscriptions.len();
    let vk_surfs = c.vulkan_video_surfaces.len();
    let in_flight_set_len = c
        .surface_subs
        .values()
        .filter(|s| s.encode_in_flight)
        .count();
    let surface_burst: u8 = c
        .surface_subs
        .values()
        .map(|s| s.burst_remaining)
        .max()
        .unwrap_or(0);
    // Worst (highest) quantizer the adaptive controller has fallen back to
    // across this client's surfaces; absent = every surface is at its
    // configured ceiling.
    let adaptive_q = c
        .surface_subs
        .values()
        .filter_map(|s| s.adaptive_quantizer)
        .max();
    let adaptive_q_log = adaptive_q.map_or(-1i32, |q| q as i32);

    c.frames_sent = 0;
    c.acks_recv = 0;
    c.acked_bytes_since_log = 0;
    c.skip_same_gen_count = 0;
    c.skip_in_flight_count = 0;
    c.skip_pacing_count = 0;
    c.skip_vulkan_await_count = 0;
    c.skip_no_subs_count = 0;
    c.skip_not_subbed_count = 0;
    c.skip_last_pixels_mismatch_count = 0;
    c.encode_loop_iters = 0;
    c.last_log = Instant::now();

    if verbose {
        let surf_info = sess.compositor.as_ref().map(|cs| {
            let surfaces = cs.surfaces.len();
            let pending = 0usize;
            let subs: usize = sess
                .clients
                .values()
                .map(|c| c.surface_subscriptions.len())
                .sum();
            (surfaces, pending, subs)
        });
        let (surf_count, surf_pending, surf_subs) = surf_info.unwrap_or((0, 0, 0));
        eprintln!(
            "client {client_id}: sent={frames_sent} acks={acks_recv} rtt={rtt_ms:.0}ms min_rtt={min_rtt_ms:.0}ms eff_rtt={eff_rtt_ms:.0}ms window={window_frames}f/{window_bytes}B probe={probe_frames:.0}f inflight={inflight_bytes}B outbox={outbox_frames}f goodput={goodput_bps:.0}B/s goodput_ewma={goodput_ewma_bps:.0}B/s jitter={goodput_jitter_bps:.0}/{max_goodput_jitter_bps:.0}B/s rate={delivery_bps:.0}B/s avg_frame={avg_frame_bytes:.0}B lead_frame={avg_paced_frame_bytes:.0}B preview_frame={avg_preview_frame_bytes:.0}B need={display_need_bps_v:.0}B/s display_fps={display_fps:.0} paced_fps={paced_fps:.0} surface_fps={surface_fps:.0} surface_frame={avg_surface_frame_bytes:.0}B backlog={browser_backlog_frames} ack_ahead={browser_ack_ahead_frames} apply={browser_apply_ms:.1}ms | tick_fires={} tick_snaps={} | surfaces={surf_count} subs={surf_subs} own_subs={own_subs} pending_req={surf_pending} commits={} encodes={} enc_bytes={} surf_sent={} px_empty_ticks={} px_snap_len={} loop_iters={loop_iters} skip_same_gen={skip_same_gen} skip_in_flight={skip_in_flight} skip_pacing={skip_pacing} skip_vk_await={skip_vk_await} skip_no_subs={skip_no_subs} skip_not_subbed={skip_not_subbed} skip_mismatch={skip_mismatch} vk_surfs={vk_surfs} enc_in_flight_set={in_flight_set_len} burst={surface_burst} adaptive_q={adaptive_q_log}",
            sess.tick_fires,
            sess.tick_snaps,
            sess.surface_commits,
            sess.surface_encodes,
            sess.surface_encode_bytes,
            sess.surface_frames_sent,
            sess.ticks_pixel_snapshot_empty,
            sess.pixel_snapshot_len,
        );
    }
    sess.tick_fires = 0;
    sess.tick_snaps = 0;
    sess.surface_commits = 0;
    sess.surface_encodes = 0;
    sess.surface_encode_bytes = 0;
    sess.surface_frames_sent = 0;
    sess.ticks_pixel_snapshot_empty = 0;
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
    client.outbox_queued_frames.load(Ordering::Relaxed)
}

fn outbox_queued_bytes(client: &ClientState) -> usize {
    client.outbox_queued_bytes.load(Ordering::Relaxed)
}

fn outbox_backpressured(client: &ClientState) -> bool {
    // Always allow at least one frame queued, even if it exceeds the byte
    // soft limit.  Large keyframes from software encoders can be larger than
    // OUTBOX_SOFT_QUEUE_LIMIT_BYTES; treating the first queued frame as
    // backpressure would permanently close surface_window_open and deadlock
    // encoding (the one queued frame cannot drain until the sender task
    // flushes it, but the sender was waiting for a new frame that we
    // refuse to produce — deadlock).
    let frames = outbox_queued_frames(client);
    if frames >= OUTBOX_SOFT_QUEUE_LIMIT_FRAMES {
        return true;
    }
    frames >= 2 && outbox_queued_bytes(client) >= OUTBOX_SOFT_QUEUE_LIMIT_BYTES
}

fn mark_outbox_drained(
    queued_frames: &Arc<AtomicUsize>,
    queued_bytes: &Arc<AtomicUsize>,
    bytes: usize,
) {
    let _ = queued_frames.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |value| {
        Some(value.saturating_sub(1))
    });
    let _ = queued_bytes.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |value| {
        Some(value.saturating_sub(bytes))
    });
}

fn send_outbox_tracked(
    tx: &mpsc::UnboundedSender<Vec<u8>>,
    queued_frames: &Arc<AtomicUsize>,
    queued_bytes: &Arc<AtomicUsize>,
    msg: Vec<u8>,
) -> Result<(), mpsc::error::SendError<Vec<u8>>> {
    let bytes = msg.len();
    tx.send(msg)?;
    queued_frames.fetch_add(1, Ordering::Relaxed);
    queued_bytes.fetch_add(bytes, Ordering::Relaxed);
    Ok(())
}

fn send_outbox(client: &ClientState, msg: Vec<u8>) -> Result<(), mpsc::error::SendError<Vec<u8>>> {
    send_outbox_tracked(
        &client.tx,
        &client.outbox_queued_frames,
        &client.outbox_queued_bytes,
        msg,
    )
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

/// Surface send gate: outbox backpressure only.  Rate is governed by
/// `surface_send_interval`; per-surface encode concurrency by the
/// `encode_in_flight` flag on `SurfaceSubState`.
fn surface_window_open(client: &ClientState) -> bool {
    !outbox_backpressured(client)
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

/// Process a surface ACK.  Feeds delivery_bps and goodput estimates (same
/// pipe) from the surface inflight queue.  Does NOT update rtt_ms / min_rtt_ms
/// — surface frames are large and their wall-clock delivery time is dominated
/// by serialization and wire transfer, not network latency.  Feeding those
/// samples into the shared RTT inflates it by orders of magnitude and
/// destabilises terminal pacing and congestion control.
/// Record a surface frame handed to the writer: queue it for ack matching
/// (evicting the oldest orphans past the cap) and fold its size into the
/// per-surface EWMA the bandwidth controller budgets against.
fn record_surface_frame_sent(
    client: &mut ClientState,
    surface_id: u16,
    bytes: usize,
    is_keyframe: bool,
    now: Instant,
) {
    // Computed before the mutable borrow below; the cap tracks the link's
    // bandwidth-delay product, so it is not a constant.
    let cap = surface_inflight_cap(client);
    while client.surface_inflight_frames.len() >= cap {
        client.surface_inflight_frames.pop_front();
    }
    client
        .surface_inflight_frames
        .push_back(SurfaceInFlightFrame {
            sent_at: now,
            bytes,
            surface_id,
        });
    if let Some(sub) = client.surface_subs.get_mut(&surface_id) {
        // Keyframes are 5-10× a P-frame; budgeting against them would
        // starve the steady stream.  Seed from one anyway (÷4) so an
        // all-intra encoder doesn't leave the estimate at zero forever.
        sub.frame_bytes = if sub.frame_bytes <= 0.0 {
            if is_keyframe {
                (bytes as f32 / 4.0).max(4_096.0)
            } else {
                bytes as f32
            }
        } else if is_keyframe {
            ewma_with_direction(sub.frame_bytes, bytes as f32, 0.05, 0.05)
        } else {
            ewma_with_direction(sub.frame_bytes, bytes as f32, 0.5, 0.125)
        };
    }
}

fn record_surface_ack(client: &mut ClientState, surface_id: u16) {
    let matched = client
        .surface_inflight_frames
        .iter()
        .position(|f| f.surface_id == surface_id);
    if let Some(frame) = matched.and_then(|i| client.surface_inflight_frames.remove(i)) {
        client.acked_bytes_since_log = client.acked_bytes_since_log.saturating_add(frame.bytes);

        let sample_ms = frame.sent_at.elapsed().as_secs_f32() * 1_000.0;

        // Shared delivery rate (bandwidth, not latency — safe to update).
        let sample_bps = frame.bytes as f32 / sample_ms.max(1.0e-3) * 1_000.0;
        client.delivery_bps = ewma_with_direction(client.delivery_bps, sample_bps, 0.5, 0.125);

        // Shared goodput window — accumulate bytes, flush periodically.
        // Surface traffic at display_fps is sustained, so always use the
        // window-limited EWMA parameters (rise 0.5, fall 0.125).  No
        // jitter tracking — jitter is a terminal congestion-control signal
        // and large keyframe/P-frame variance would poison it.
        client.goodput_window_bytes = client.goodput_window_bytes.saturating_add(frame.bytes);
        let now = Instant::now();
        let goodput_elapsed = now
            .duration_since(client.goodput_window_start)
            .as_secs_f32();
        if goodput_elapsed >= 0.02 {
            let sample_goodput = client.goodput_window_bytes as f32 / goodput_elapsed.max(1.0e-3);
            client.goodput_bps =
                ewma_with_direction(client.goodput_bps, sample_goodput, 0.5, 0.125);
            client.last_goodput_sample_bps =
                (client.last_goodput_sample_bps * 0.99).max(sample_goodput);
            client.goodput_window_bytes = 0;
            client.goodput_window_start = now;
        }
    }
}

/// Forget every unacked frame for `surface_id`.
///
/// A surface that has gone away (unsubscribed, destroyed, resized) will
/// never be acked, and Wayland reuses surface ids: a later frame on the
/// recycled id would match a minutes-old entry, report an absurd RTT, and
/// drag the goodput estimate — and so the adaptive controller — down.
fn forget_surface_inflight(client: &mut ClientState, surface_id: u16) {
    client
        .surface_inflight_frames
        .retain(|f| f.surface_id != surface_id);
}

fn reset_inflight(client: &mut ClientState) {
    client.inflight_bytes = 0;
    client.inflight_frames.clear();
    // Surface frames sent before the reset will never be acked either;
    // leaving them queued permanently offsets every later ack.
    client.surface_inflight_frames.clear();
    client.next_send_at = Instant::now();
    client.browser_backlog_frames = 0;
    client.browser_ack_ahead_frames = 0;
}

fn is_unset_view_size(rows: u16, cols: u16) -> bool {
    rows == 0 && cols == 0
}

/// Highest display refresh rate a client may declare, in Hz.
///
/// Past any shipping panel, and the pacing maths only ever wants an upper
/// bound here — a client that lies high makes the server work harder for
/// itself and raises the compositor's advertised rate for everyone.
const MAX_DISPLAY_FPS: u16 = 480;

/// Longest `C2S_SEARCH` query accepted, in bytes.
///
/// The query is a regex, compiled once per PTY on every search while the
/// session lock is held, so its cost is multiplied by the terminal count.
/// The regex engines bound their own compiled size — alacritty sets an NFA
/// size limit and `regex` defaults to 10 MB — but nothing bounded the input,
/// and a frame can carry 16 MiB of it.
const MAX_SEARCH_QUERY: usize = 1024;

/// Largest view dimension a client may ask for, per axis.
///
/// An 8K display at a 4px font is ~540 rows and ~3840 columns, so this is
/// past any real viewport. It exists because `C2S_RESIZE` carries two raw
/// `u16`s and only rejected zero: a single client asking for 65535x65535
/// became the mediated size — the minimum across clients, which is its own
/// when it is the only one — and the terminal grid was allocated at that.
const MAX_VIEW_DIM: u16 = 4096;

/// Clamp a client-supplied view size to something a frame can describe.
///
/// Both bounds matter: the per-axis cap keeps a single absurd dimension out,
/// and the cell budget is the wire's own limit — a grid past
/// [`blit_remote::MAX_CELL_COUNT`] produces frames every receiver rejects, so
/// sizing one is strictly worse than clamping.
fn clamp_view_size(rows: u16, cols: u16) -> (u16, u16) {
    let rows = rows.min(MAX_VIEW_DIM);
    let mut cols = cols.min(MAX_VIEW_DIM);
    let budget = blit_remote::MAX_CELL_COUNT / (rows as usize).max(1);
    if (cols as usize) > budget {
        cols = budget.max(1) as u16;
    }
    (rows, cols)
}

fn subscribe_client_to(client: &mut ClientState, pty_id: u16) {
    if client.subscriptions.insert(pty_id) {
        client.last_sent.remove(&pty_id);
        client.last_used_rows_sent.remove(&pty_id);
        client.preview_next_send_at.remove(&pty_id);
    }
}

fn unsubscribe_client_from(client: &mut ClientState, pty_id: u16) -> bool {
    let removed_sub = client.subscriptions.remove(&pty_id);
    client.last_sent.remove(&pty_id);
    client.last_used_rows_sent.remove(&pty_id);
    client.preview_next_send_at.remove(&pty_id);
    client.scroll_offsets.remove(&pty_id);
    client.scroll_caches.remove(&pty_id);
    let removed_view = client.view_sizes.remove(&pty_id).is_some();
    if client.lead == Some(pty_id) {
        client.lead = None;
    }
    removed_sub || removed_view
}

fn update_client_scroll_state(client: &mut ClientState, pty_id: u16, next_offset: usize) -> bool {
    let prev_offset = client.scroll_offsets.get(&pty_id).copied().unwrap_or(0);
    if prev_offset == next_offset {
        return false;
    }

    if prev_offset == 0 && next_offset > 0 {
        client.scroll_caches.insert(
            pty_id,
            client.last_sent.get(&pty_id).cloned().unwrap_or_default(),
        );
    } else if prev_offset > 0
        && next_offset == 0
        && let Some(cache) = client.scroll_caches.remove(&pty_id)
    {
        if cache.rows() > 0 && cache.cols() > 0 {
            client.last_sent.insert(pty_id, cache);
        } else {
            client.last_sent.remove(&pty_id);
        }
    }

    if next_offset > 0 {
        client.scroll_offsets.insert(pty_id, next_offset);
    } else {
        client.scroll_offsets.remove(&pty_id);
    }
    reset_inflight(client);
    true
}

struct Session {
    ptys: HashMap<u16, Pty>,
    compositor: Option<SharedCompositor>,
    next_client_id: u64,
    next_compositor_id: u16,
    next_pty_id: u16,
    tick_fires: u32,
    tick_snaps: u32,
    surface_commits: u32,
    surface_encodes: u32,
    surface_encode_bytes: u64,
    surface_frames_sent: u32,
    /// Ticks where pixel_snapshot was empty → entire encode loop skipped.
    ticks_pixel_snapshot_empty: u32,
    /// Number of (sid,w,h) tuples in the most recent non-empty pixel_snapshot.
    pixel_snapshot_len: usize,
    last_ping: Instant,
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
    next_deadline: Option<Instant>,
}

impl Session {
    /// Re-decide a downscale target whose subscriber set just changed.
    ///
    /// Re-registers it for whoever is left — which re-evaluates whether the
    /// NV12 `OPAQUE_FD` shape is safe, so an NVENC reader gets the zero-copy
    /// path back once a subscriber that needed CPU pixels has gone — or
    /// clears it when nobody is left.
    ///
    /// Clearing unconditionally would be wrong on both counts: it pulls the
    /// buffer out from under clients still registered at that size, and it
    /// leaves survivors on BGRA until something unrelated re-registers them.
    fn resettle_downscale_target(&mut self, surface_id: u16, tw: u32, th: u32) {
        let survivors: Vec<(bool, (u32, u32))> = self
            .clients
            .values()
            .filter_map(|c| {
                let s = c.surface_subs.get(&surface_id)?;
                (s.last_registered_target == Some((tw, th))).then(|| {
                    (
                        s.wants_nv12_opaque,
                        s.last_registered_native.unwrap_or((tw, th)),
                    )
                })
            })
            .collect();
        let Some(cs) = self.compositor.as_mut() else {
            return;
        };
        if let Some(&(_, (native_w, native_h))) = survivors.first() {
            let _ = cs.handle.command_tx.send(
                blit_compositor::CompositorCommand::RegisterDownscaleTarget {
                    surface_id: surface_id as u32,
                    target_w: tw,
                    target_h: th,
                    native_w,
                    native_h,
                    want_nv12_opaque: survivors.iter().all(|(w, _)| *w),
                },
            );
        } else {
            let _ = cs.handle.command_tx.send(
                blit_compositor::CompositorCommand::ClearDownscaleTarget {
                    surface_id: surface_id as u32,
                    target_w: tw,
                    target_h: th,
                },
            );
            cs.last_pixels.remove(&(surface_id, tw, th));
        }
        cs.handle.wake();
    }

    fn new() -> Self {
        Self {
            ptys: HashMap::new(),
            compositor: None,
            next_client_id: 1,
            next_compositor_id: 1,
            next_pty_id: 1,
            clients: HashMap::new(),
            tick_fires: 0,
            tick_snaps: 0,
            surface_commits: 0,
            surface_encodes: 0,
            surface_encode_bytes: 0,
            ticks_pixel_snapshot_empty: 0,
            pixel_snapshot_len: 0,
            last_ping: Instant::now(),
            surface_frames_sent: 0,
        }
    }

    fn ensure_compositor(
        &mut self,
        verbose: bool,
        event_notify: Arc<dyn Fn() + Send + Sync>,
        gpu_device: &str,
    ) -> &str {
        if self.compositor.is_none() {
            #[cfg(target_os = "linux")]
            let session_id = self.next_compositor_id;
            self.next_compositor_id = self.next_compositor_id.wrapping_add(1);
            // Create the epoch before spawning anything so audio and video
            // share the same time origin for A/V sync.
            #[cfg(target_os = "linux")]
            let created_at = Instant::now();
            let handle = blit_compositor::spawn_compositor(verbose, event_notify, gpu_device);
            #[cfg(target_os = "linux")]
            let audio_broadcast = audio::AudioBroadcast::new();
            #[cfg(target_os = "linux")]
            let audio_pipeline = {
                let audio_disabled = std::env::var("BLIT_AUDIO")
                    .map(|v| v == "0")
                    .unwrap_or(false);
                if !audio_disabled && audio::pipewire_available() {
                    let runtime_dir = std::path::Path::new(&handle.socket_name)
                        .parent()
                        .unwrap_or(std::path::Path::new("/tmp"));
                    let bitrate = std::env::var("BLIT_AUDIO_BITRATE")
                        .ok()
                        .and_then(|v| v.parse::<i32>().ok())
                        .unwrap_or(0);
                    // Wrap in block_in_place so the thread::sleep calls
                    // inside spawn() don't stall the tokio runtime.
                    let broadcast = audio_broadcast.clone();
                    tokio::task::block_in_place(|| {
                        match audio::AudioPipeline::spawn(
                            runtime_dir,
                            session_id,
                            bitrate,
                            verbose,
                            created_at,
                            broadcast,
                        ) {
                            Ok(pipeline) => {
                                if verbose {
                                    eprintln!(
                                        "[audio] pipeline started, PULSE_SERVER={}",
                                        pipeline.pulse_server_path(),
                                    );
                                }
                                Some(pipeline)
                            }
                            Err(e) => {
                                eprintln!("[audio] failed to start pipeline: {e}");
                                None
                            }
                        }
                    })
                } else {
                    if verbose && !audio_disabled {
                        let missing = audio::missing_pipewire_binaries();
                        let load_err = audio_pw::load_error();
                        if !missing.is_empty() {
                            eprintln!(
                                "[audio] audio disabled: missing binaries on $PATH: {}",
                                missing.join(", ")
                            );
                        }
                        if !load_err.is_empty() {
                            eprintln!("[audio] audio disabled: {load_err}");
                        }
                        if missing.is_empty() && load_err.is_empty() {
                            eprintln!(
                                "[audio] audio disabled (reason not recorded; call pipewire_available() logged above)"
                            );
                        }
                    }
                    None
                }
            };

            self.compositor = Some(SharedCompositor {
                handle,
                surfaces: HashMap::new(),
                last_pixels: HashMap::new(),
                last_encoded: HashMap::new(),
                last_frame_request: HashMap::new(),
                #[cfg(target_os = "linux")]
                created_at,
                pixel_generation: 0,
                last_blanket_frame_request: Instant::now(),
                last_configured_size: HashMap::new(),
                last_resize_at: HashMap::new(),
                pending_resize: HashMap::new(),
                native_sizes: HashMap::new(),
                #[cfg(target_os = "linux")]
                audio_pipeline,
                #[cfg(target_os = "linux")]
                audio_broadcast,
                #[cfg(target_os = "linux")]
                audio_session_id: session_id,
                #[cfg(target_os = "linux")]
                last_audio_restart: None,
            });
        }
        &self.compositor.as_ref().unwrap().handle.socket_name
    }

    /// Returns the `PULSE_SERVER` path if the audio pipeline is active.
    #[cfg(target_os = "linux")]
    fn pulse_server_path(&self) -> Option<String> {
        self.compositor
            .as_ref()
            .and_then(|cs| cs.audio_pipeline.as_ref())
            .map(|ap| ap.pulse_server_path())
    }

    /// Returns the `PIPEWIRE_REMOTE` path if the audio pipeline is active.
    #[cfg(target_os = "linux")]
    fn pipewire_remote_path(&self) -> Option<String> {
        self.compositor
            .as_ref()
            .and_then(|cs| cs.audio_pipeline.as_ref())
            .map(|ap| ap.pipewire_remote_path())
    }

    fn live_ptys(&self) -> usize {
        self.ptys.values().filter(|pty| !pty.exited).count()
    }

    fn allocate_pty_id(&mut self, max_ptys: usize) -> Option<u16> {
        // Live terminals only.  Counting exited-but-retained ones would let a
        // client that runs 256 short commands hit a cap of 256 with nothing
        // actually running; those are bounded separately, by retention.
        if max_ptys > 0 && self.live_ptys() >= max_ptys {
            // A `CREATE2(WANT_STATUS)` caller now gets `S2C_CREATE_FAILED`
            // with `BUDGET`, but the older create opcodes still drop the
            // request with no reply, so keep saying it here — for those the
            // cap otherwise still looks like a hang.
            eprintln!("blit-server: refusing CREATE, BLIT_MAX_PTYS ({max_ptys}) reached");
            return None;
        }
        let start = self.next_pty_id;
        let mut id = start;
        loop {
            if !self.ptys.contains_key(&id) {
                self.next_pty_id = if id == u16::MAX { 1 } else { id + 1 };
                return Some(id);
            }
            id = if id == u16::MAX { 1 } else { id + 1 };
            if id == start {
                return None;
            }
        }
    }

    fn send_to_all(&self, msg: &[u8]) {
        for c in self.clients.values() {
            let _ = send_outbox(c, msg.to_vec());
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
        pty.last_used_rows_sent = pty.last_used_rows_sent.min(rows);
        for c in self.clients.values_mut() {
            if c.subscriptions.contains(&pty_id) {
                c.last_sent.remove(&pty_id);
                c.last_used_rows_sent.remove(&pty_id);
            }
            if c.scroll_caches.remove(&pty_id).is_some() {
                reset_inflight(c);
            }
        }
        if !pty.exited {
            pty::resize_pty_os(&pty.handle, rows, cols);
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

    // ------------------------------------------------------------------
    // Surface sizing — same consumer-tracking model as PTY sizing.
    // Each client reports how large it can display a surface; the server
    // picks min(width), min(height) across all clients and configures the
    // compositor accordingly.
    // ------------------------------------------------------------------

    /// Returns the compositor's mediated (width, height, scale_120) for
    /// `surface_id`, mediated across every client subscribed to it.
    ///
    /// Mediation rule (mirrors PTY sizing): the compositor surface must
    /// fit every viewer at the highest density any viewer has.
    ///
    /// - **Smallest logical size wins** so the surface fits on every
    ///   client's screen.  Each client reports its viewport in *physical*
    ///   pixels along with its DPR (`scale_120`), so we convert each
    ///   client's report to logical pixels (`physical * 120 / scale`)
    ///   before taking the min.  Otherwise a 1× client and a 2× client
    ///   reporting the same logical size would mediate at half the
    ///   intended logical area.
    /// - **Highest scale wins** so the densest client gets native pixels.
    ///   Lower-DPR clients get the same logical size at higher density;
    ///   the per-client encoder then downscales to their physical
    ///   viewport.
    ///
    /// The returned `(width, height)` is in *physical* pixels at the
    /// returned `scale_120` (i.e. `min_logical * max_scale_120 / 120`),
    /// so the existing compositor handler — which converts physical →
    /// logical with the same scale — sees the correct logical surface
    /// size.  `max` clamps the physical size to the encoder's limits.
    /// Pick the per-client encoder source dimensions for one
    /// (client, surface) pair.  This is the size each viewer's bitstream
    /// is encoded at — the encode pipeline downscales from
    /// `(native_w, native_h)` (the compositor's mediated size) into
    /// these dimensions before handing pixels to the encoder.
    ///
    /// Clamping rules (in order):
    ///   1. Preserve native aspect ratio.  The viewport gives us a max
    ///      box; we inscribe a `native_w × native_h`-shaped box inside
    ///      it.  Stretching to fill the viewport would distort the
    ///      frame because the JS canvas blits the encoded image at its
    ///      intrinsic aspect (object-fit: contain) — any aspect
    ///      mismatch we encode is locked into the bitstream and
    ///      letterboxed by the browser.
    ///   2. Cap at `(native_w, native_h)` so we never upscale —
    ///      asking for a larger encoder just wastes bandwidth.
    ///   3. Cap at `max` — this viewer's encoder ceiling, from
    ///      `surface_encode_cap` — preserving aspect across the cap.
    ///      This is per-viewer, not per-surface: on a 5K surface an AV1
    ///      viewer encodes at 5120×2880 while an H.264 viewer watching
    ///      the same surface gets a 3840×2160 downscale of it.
    ///   4. Floor at 2×2 (and even) so the encoder doesn't reject the
    ///      dimensions and chroma subsampling has a valid grid.
    ///
    /// `view_size` is `Some((physical_w, physical_h, scale_120))` when
    /// the client has sent at least one `C2S_SURFACE_RESIZE`; the
    /// fallback (`None` or zero dimensions) is the compositor's native
    /// size, matching how the surface looked to the very first
    /// subscriber.
    fn per_client_encode_target(
        view_size: Option<(u16, u16, u16)>,
        native_w: u32,
        native_h: u32,
        max: Option<(u16, u16)>,
    ) -> (u32, u32) {
        // Largest box no larger than `(box_w, box_h)` that has the
        // same aspect ratio as `(native_w, native_h)`.
        let inscribe = |box_w: u32, box_h: u32| -> (u32, u32) {
            if native_w == 0 || native_h == 0 || box_w == 0 || box_h == 0 {
                return (box_w, box_h);
            }
            // Use u64 to avoid overflow on the cross-multiply.
            let nw = native_w as u64;
            let nh = native_h as u64;
            let bw = box_w as u64;
            let bh = box_h as u64;
            // Two candidates: width-bound (w=box_w, h=box_w*nh/nw) and
            // height-bound (h=box_h, w=box_h*nw/nh).  Pick whichever
            // fits inside the box.
            let h_for_full_w = (bw * nh) / nw;
            if h_for_full_w <= bh {
                (box_w, h_for_full_w as u32)
            } else {
                let w_for_full_h = (bh * nw) / nh;
                (w_for_full_h as u32, box_h)
            }
        };

        let (w, h) = view_size
            .map(|(w, h, _)| (w as u32, h as u32))
            .filter(|&(w, h)| w > 0 && h > 0)
            // Cap viewport box to native (no upscale) before inscribing.
            .map(|(w, h)| (w.min(native_w), h.min(native_h)))
            .map(|(w, h)| inscribe(w, h))
            .unwrap_or((native_w, native_h));
        // Encoder-family cap, also aspect-preserving.
        let (w, h) = match max {
            Some((mw, mh)) if w > mw as u32 || h > mh as u32 => inscribe(mw as u32, mh as u32),
            _ => (w, h),
        };
        // Round to even and floor at 2 — H.264/H.265/AV1 NV12 sampling
        // grids and most encoder APIs (NVENC, VAAPI) require even
        // dimensions.
        let w = (w & !1).max(2);
        let h = (h & !1).max(2);
        (w, h)
    }

    /// The size the compositor should render this surface at, given every
    /// client watching it.
    ///
    /// `prefs` is the configured encoder chain; the ceiling applied here is
    /// the *loosest* one any subscriber could actually be served, because a
    /// codec limit is a transport constraint rather than a rendering one.
    /// Composite for the viewer with the most capable decoder and let the
    /// rest take a downscale (`per_client_encode_target`) — the alternative,
    /// clamping to the tightest, would drag a 5K AV1 viewer down to 4K
    /// because some other tab in the session only speaks H.264.  When every
    /// subscriber is H.264-only the result is the old 3840×2160, so nothing
    /// composites larger than it can be sent.
    fn mediated_size_for_surface(
        &self,
        surface_id: u16,
        prefs: &[SurfaceEncoderPreference],
    ) -> Option<(u16, u16, u16)> {
        // Per axis: the smallest logical extent asked for, plus the exact
        // physical extent and scale of the client that asked for it.
        let mut min_w: Option<(u32, u32, u16)> = None;
        let mut min_h: Option<(u32, u32, u16)> = None;
        let mut max_scale: u16 = 0;
        let mut max: Option<(u16, u16)> = None;
        for c in self.clients.values() {
            // Only count clients that are actually subscribed.  A
            // stale view_size left behind by a client that
            // unsubscribed but didn't clear the size (or that resized
            // before its first subscribe) shouldn't shrink everyone
            // else's surface.
            if !c.surface_subscriptions.contains(&surface_id) {
                continue;
            }
            // A scaled subscriber asked to be served a downscale of whatever
            // the surface happens to be, so it gets no say in how big that
            // is.  Counting it would defeat the point: a card-sized
            // thumbnail would win the minimum and shrink the Wayland window
            // for the viewers watching it full size.
            if c.surface_subs
                .get(&surface_id)
                .is_some_and(|s| s.scaled_target.is_some())
            {
                continue;
            }
            let Some(&(pw, ph, s)) = c.surface_view_sizes.get(&surface_id) else {
                continue;
            };
            let s_eff = (s as u32).max(120);
            // Round-half-up so a 1× client and a 2× client both reporting
            // the same logical size land on the same logical integer.
            let lw = ((pw as u32) * 120 + s_eff / 2) / s_eff;
            let lh = ((ph as u32) * 120 + s_eff / 2) / s_eff;
            if min_w.is_none_or(|(m, _, _)| lw < m) {
                min_w = Some((lw, pw as u32, s));
            }
            if min_h.is_none_or(|(m, _, _)| lh < m) {
                min_h = Some((lh, ph as u32, s));
            }
            max_scale = max_scale.max(s);
            // Widen the ceiling to whatever this viewer can be served.  Read
            // from the same clients that get a say in the size — a scaled
            // subscriber already skipped above, and letting a thumbnail's
            // ceiling raise the composite would be as wrong as letting its
            // size lower it.
            if let Some((cw, ch)) = surface_encode_cap(prefs, c, surface_id) {
                max = Some(match max {
                    Some((mw, mh)) => (mw.max(cw), mh.max(ch)),
                    None => (cw, ch),
                });
            }
        }
        let (min_w, min_h) = match (min_w, min_h) {
            (Some(w), Some(h)) => (w, h),
            _ => return None,
        };
        let s = max_scale.max(120) as u32;
        // Back to physical at the chosen (highest) scale — but take the
        // constraining client's own physical extent verbatim when it is
        // already at that scale, because the logical round trip does not
        // return what it was given: at 2x an odd physical extent comes back
        // one pixel *larger* (1001 → 501 → 1002). The surface is then a pixel
        // bigger than the pane that asked for it, `per_client_encode_target`
        // inscribes the native aspect into the smaller viewport, and the
        // difference shows up as a letterbox bar on an otherwise exact fit.
        // Fractional CSS pane widths — what a tiled split produces — make odd
        // physical extents the common case, not the corner one.
        let exact = |(lw, pw, cs): (u32, u32, u16)| -> u32 {
            if (cs as u32).max(120) == s {
                pw
            } else {
                (lw.max(1) * s) / 120
            }
        };
        let pw = exact(min_w).clamp(1, u16::MAX as u32) as u16;
        let ph = exact(min_h).clamp(1, u16::MAX as u32) as u16;
        let (pw, ph) = if let Some((mw, mh)) = max {
            (pw.min(mw), ph.min(mh))
        } else {
            (pw, ph)
        };
        Some((pw.max(1), ph.max(1), s as u16))
    }

    /// Ask the compositor for a new surface size, subject to the settle
    /// window in `SURFACE_RESIZE_SETTLE`.  Returns true if the compositor was
    /// told right away; a false return may still mean the size was recorded
    /// and will be dispatched by `flush_due_resizes`.
    fn resize_surface(&mut self, surface_id: u16, width: u16, height: u16, scale_120: u16) -> bool {
        let now = Instant::now();
        let cs = match self.compositor.as_mut() {
            Some(cs) => cs,
            None => return false,
        };
        match resize_action(
            cs.last_configured_size.get(&surface_id).copied(),
            cs.last_resize_at.get(&surface_id).copied(),
            now,
            (width, height, scale_120),
        ) {
            ResizeAction::Ignore => {
                // A drag that ends back where it started leaves nothing to
                // do.  Drop the held size rather than replaying it later,
                // which would configure the surface to a stale intermediate.
                cs.pending_resize.remove(&surface_id);
                false
            }
            ResizeAction::Hold => {
                // Keep only the latest size; the delivery loop dispatches it
                // when the window closes.
                cs.pending_resize
                    .insert(surface_id, (width, height, scale_120));
                false
            }
            ResizeAction::Dispatch => {
                cs.dispatch_resize(surface_id, width, height, scale_120, now);
                true
            }
        }
    }

    /// Returns true if any surface is left holding a resize for its settle
    /// window.  Those are dispatched only by `tick`, so a caller outside the
    /// delivery loop must nudge it — mirrors `resize_ptys_to_mediated_sizes`.
    fn resize_surfaces_to_mediated_sizes<I>(
        &mut self,
        surface_ids: I,
        encoder_preferences: &[SurfaceEncoderPreference],
        verbose: bool,
    ) -> bool
    where
        I: IntoIterator<Item = u16>,
    {
        let mut seen = HashSet::new();
        for sid in surface_ids {
            if !seen.insert(sid) {
                continue;
            }
            if let Some((w, h, scale_120)) =
                self.mediated_size_for_surface(sid, encoder_preferences)
            {
                let dispatched = self.resize_surface(sid, w, h, scale_120);
                if verbose {
                    // The subscribers' own view sizes are the inputs to the
                    // mediation and exist only at runtime, so when a surface
                    // comes out an unexpected size in a shared session this is
                    // the line that says which viewer pinned it there.
                    //
                    // Report which of the three outcomes it was, not just
                    // whether a configure went out: `resize_surface` returns
                    // false for a settle-window hold as well as for a no-op,
                    // and during a drag those mean opposite things — one is
                    // parked for `tick` to send, the other is nothing at all.
                    let outcome = if dispatched {
                        "dispatched"
                    } else if self
                        .compositor
                        .as_ref()
                        .is_some_and(|cs| cs.pending_resize.contains_key(&sid))
                    {
                        "held"
                    } else {
                        "unchanged"
                    };
                    let views = self
                        .clients
                        .values()
                        .filter(|c| c.surface_subscriptions.contains(&sid))
                        .filter_map(|c| c.surface_view_sizes.get(&sid))
                        .map(|&(w, h, s)| format!("{w}x{h}@{s}"))
                        .collect::<Vec<_>>()
                        .join(", ");
                    eprintln!(
                        "mediate-resize: sid={sid} -> {w}x{h} scale={scale_120} {outcome} (views: {views})"
                    );
                }
            }
        }
        self.compositor
            .as_ref()
            .is_some_and(|cs| !cs.pending_resize.is_empty())
    }

    fn pty_list_msg(&self) -> Vec<u8> {
        let mut msg = vec![S2C_LIST];
        let count = self.ptys.len() as u16;
        msg.extend_from_slice(&count.to_le_bytes());
        let mut ids: Vec<u16> = self.ptys.keys().copied().collect();
        ids.sort();
        for id in ids {
            let pty = &self.ptys[&id];
            let tag = pty.tag.as_bytes();
            msg.extend_from_slice(&id.to_le_bytes());
            msg.extend_from_slice(&(tag.len() as u16).to_le_bytes());
            msg.extend_from_slice(tag);
            let cmd = pty.command.as_deref().unwrap_or("").as_bytes();
            msg.extend_from_slice(&(cmd.len() as u16).to_le_bytes());
            msg.extend_from_slice(cmd);
        }
        msg
    }

    fn surface_list_msg(&self) -> Vec<u8> {
        let cs = match self.compositor.as_ref() {
            Some(cs) => cs,
            None => {
                let mut msg = vec![S2C_SURFACE_LIST];
                msg.extend_from_slice(&0u16.to_le_bytes());
                return msg;
            }
        };
        let mut msg = vec![S2C_SURFACE_LIST];
        let count = cs.surfaces.len() as u16;
        msg.extend_from_slice(&count.to_le_bytes());
        let mut ids: Vec<u16> = cs.surfaces.keys().copied().collect();
        ids.sort();
        for id in ids {
            let info = &cs.surfaces[&id];
            let title = info.title.as_bytes();
            let app_id = info.app_id.as_bytes();
            msg.extend_from_slice(&info.surface_id.to_le_bytes());
            msg.extend_from_slice(&info.parent_id.to_le_bytes());
            msg.extend_from_slice(&info.width.to_le_bytes());
            msg.extend_from_slice(&info.height.to_le_bytes());
            msg.extend_from_slice(&(title.len() as u16).to_le_bytes());
            msg.extend_from_slice(title);
            msg.extend_from_slice(&(app_id.len() as u16).to_le_bytes());
            msg.extend_from_slice(app_id);
        }
        msg
    }
}

struct AppStateInner {
    config: Config,
    /// Opaque identifier shared by every connection to this server process.
    boot_generation: u64,
    session: Mutex<Session>,
    pty_fds: PtyFds,
    delivery_notify: Arc<Notify>,
    /// Signalled when a client sends C2S_QUIT to initiate server shutdown.
    shutdown_notify: Arc<Notify>,
    /// Wakes the supervisor loop.  Separate from `delivery_notify` because
    /// the two have opposite duty cycles: delivery only runs while a client
    /// is attached, and lifecycle work is exactly what has to keep running
    /// when none is.
    supervisor_notify: Arc<Notify>,
    /// Tracks the number of currently connected clients for enforcing
    /// `config.max_connections`.
    active_connections: std::sync::atomic::AtomicUsize,
}

type AppState = Arc<AppStateInner>;

fn new_boot_generation() -> u64 {
    let mut bytes = [0; 8];
    getrandom::fill(&mut bytes).expect("failed to generate boot generation");
    u64::from_le_bytes(bytes)
}

fn nudge_delivery(state: &AppState) {
    state.delivery_notify.notify_one();
}

#[cfg(unix)]
#[allow(dead_code)]
fn spawn_compositor_child(
    command: &str,
    argv: Option<&[&str]>,
    wayland_socket: &str,
    dir: Option<&str>,
) -> libc::pid_t {
    use std::ffi::CString;
    let pid = unsafe { libc::fork() };
    if pid == 0 {
        if let Some(d) = dir {
            let c_dir = CString::new(d).unwrap();
            unsafe {
                libc::chdir(c_dir.as_ptr());
            }
        }
        unsafe {
            let wd_path = std::path::Path::new(wayland_socket);
            if let Some(dir) = wd_path.parent() {
                let xdg = std::env::var_os("XDG_RUNTIME_DIR");
                let needs_update = match &xdg {
                    Some(x) => std::path::Path::new(x) != dir,
                    None => true,
                };
                if needs_update {
                    std::env::set_var("XDG_RUNTIME_DIR", dir);
                }
            }
            std::env::set_var("WAYLAND_DISPLAY", wayland_socket);
            // blit is a Wayland-only compositor (no XWayland), and DISPLAY is
            // removed just below — so steer GUI toolkits to their Wayland
            // backends. Without these, Electron/Chromium (Cursor), Firefox,
            // GTK and Qt default to X11 and come up with no window. Only set
            // when unset so an explicit caller/environment override still wins.
            for (k, v) in [
                ("NIXOS_OZONE_WL", "1"),
                ("ELECTRON_OZONE_PLATFORM_HINT", "wayland"),
                ("MOZ_ENABLE_WAYLAND", "1"),
                ("GDK_BACKEND", "wayland"),
                ("QT_QPA_PLATFORM", "wayland"),
                ("SDL_VIDEODRIVER", "wayland"),
            ] {
                if std::env::var_os(k).is_none() {
                    std::env::set_var(k, v);
                }
            }
            std::env::remove_var("DISPLAY");
            std::env::remove_var("DBUS_SESSION_BUS_ADDRESS");
            std::env::remove_var("DBUS_SYSTEM_BUS_ADDRESS");
        }
        if let Some(args) = argv {
            let prog = CString::new(args[0]).unwrap();
            let c_args: Vec<CString> = args.iter().map(|a| CString::new(*a).unwrap()).collect();
            let c_ptrs: Vec<*const libc::c_char> = c_args
                .iter()
                .map(|a| a.as_ptr())
                .chain(std::iter::once(std::ptr::null()))
                .collect();
            unsafe {
                libc::execvp(prog.as_ptr(), c_ptrs.as_ptr());
            }
        } else {
            let prog = CString::new(command).unwrap();
            let c_ptrs = [prog.as_ptr(), std::ptr::null()];
            unsafe {
                libc::execvp(prog.as_ptr(), c_ptrs.as_ptr());
                libc::_exit(1);
            }
        }
    }
    pid
}

/// Map xterm-256 color index to (r, g, b) in 16-bit per channel.
fn xterm256_color(idx: u8) -> (u16, u16, u16) {
    // Standard 16 colors (0-15)
    const BASE16: [(u8, u8, u8); 16] = [
        (0, 0, 0),
        (128, 0, 0),
        (0, 128, 0),
        (128, 128, 0),
        (0, 0, 128),
        (128, 0, 128),
        (0, 128, 128),
        (192, 192, 192),
        (128, 128, 128),
        (255, 0, 0),
        (0, 255, 0),
        (255, 255, 0),
        (0, 0, 255),
        (255, 0, 255),
        (0, 255, 255),
        (255, 255, 255),
    ];
    let (r8, g8, b8) = if idx < 16 {
        BASE16[idx as usize]
    } else if idx < 232 {
        // 6x6x6 color cube (indices 16-231)
        let n = idx - 16;
        let ri = n / 36;
        let gi = (n % 36) / 6;
        let bi = n % 6;
        let to_val = |v: u8| if v == 0 { 0u8 } else { 55 + 40 * v };
        (to_val(ri), to_val(gi), to_val(bi))
    } else {
        // Grayscale ramp (indices 232-255)
        let v = 8 + 10 * (idx - 232);
        (v, v, v)
    };
    // Scale 8-bit to 16-bit (0xFF -> 0xFFFF)
    let scale = |v: u8| (v as u16) << 8 | v as u16;
    (scale(r8), scale(g8), scale(b8))
}
/// Result of scanning a PTY output chunk in `parse_terminal_queries`.
struct TerminalScan {
    /// Query responses to write back into the PTY (DA1, DSR, OSC color
    /// queries, ...).
    responses: Vec<String>,
    /// Last valid OSC 7 working-directory report in the chunk
    /// (docs/protocol.md, "Working directory tracking"): a percent-decoded
    /// absolute local path of at most `blit_remote::TERM_CWD_MAX` bytes.
    osc7_cwd: Option<String>,
}

/// This machine's hostname, for filtering OSC 7 host components.  Cached
/// because the scan runs on every PTY output chunk.
fn local_hostname() -> &'static str {
    static HOST: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    HOST.get_or_init(|| {
        #[cfg(unix)]
        {
            // Reserve the last byte: gethostname need not NUL-terminate on
            // truncation.
            let mut buf = [0u8; 256];
            if unsafe { libc::gethostname(buf.as_mut_ptr().cast(), buf.len() - 1) } == 0 {
                let end = buf.iter().position(|&b| b == 0).unwrap_or(0);
                return String::from_utf8_lossy(&buf[..end]).into_owned();
            }
            String::new()
        }
        #[cfg(not(unix))]
        {
            // No gethostname off unix; COMPUTERNAME covers Windows.  Empty
            // just narrows accepted OSC 7 hosts to ""/"localhost".
            std::env::var("COMPUTERNAME").unwrap_or_default()
        }
    })
}

/// Parse an OSC 7 URL (`file://<host><path>`) into a local absolute cwd.
/// Rejects rather than guesses:
/// - non-`file://` payloads;
/// - hosts other than this machine (empty, "localhost", or `local_host`,
///   ASCII-case-insensitively) — a remote-ssh shell's OSC 7 names the
///   remote host, and its path is not a local path;
/// - non-absolute paths (nothing after the host, or no literal `/`);
/// - malformed percent-escapes, embedded NUL, or invalid UTF-8 after
///   decoding;
/// - decoded paths longer than `blit_remote::TERM_CWD_MAX` (longer than
///   any kernel-accepted cwd; keeps the pushed message bounded).
fn parse_osc7_url(url: &[u8], local_host: &str) -> Option<String> {
    let rest = url.strip_prefix(b"file://")?;
    // The path starts at the first literal '/'; a percent-encoded slash
    // does not make a path absolute.
    let slash = rest.iter().position(|&b| b == b'/')?;
    let (host, raw_path) = rest.split_at(slash);
    let host_ok = host.is_empty()
        || host.eq_ignore_ascii_case(b"localhost")
        || (!local_host.is_empty() && host.eq_ignore_ascii_case(local_host.as_bytes()));
    if !host_ok {
        return None;
    }
    // Percent-decode: shell integrations encode non-ASCII and reserved
    // bytes as %XX (two hex digits).
    let mut decoded = Vec::with_capacity(raw_path.len());
    let mut i = 0;
    while i < raw_path.len() {
        if raw_path[i] == b'%' {
            let hex = raw_path.get(i + 1..i + 3)?;
            let hi = (hex[0] as char).to_digit(16)?;
            let lo = (hex[1] as char).to_digit(16)?;
            decoded.push((hi * 16 + lo) as u8);
            i += 3;
        } else {
            decoded.push(raw_path[i]);
            i += 1;
        }
    }
    if decoded.len() > blit_remote::TERM_CWD_MAX || decoded.contains(&0) {
        return None;
    }
    String::from_utf8(decoded).ok()
}

fn parse_terminal_queries(data: &[u8], size: (u16, u16), cursor: (u16, u16)) -> TerminalScan {
    const DA1_RESPONSE: &[u8] = b"\x1b[?64;1;2;6;9;15;18;21;22c";

    let mut results = Vec::new();
    let mut osc7_cwd = None;
    let mut i = 0;
    while i < data.len() {
        if data[i] != 0x1b || i + 1 >= data.len() {
            i += 1;
            continue;
        }

        // Handle OSC sequences: \x1b] ... (ST or BEL)
        if data[i + 1] == b']' {
            let osc_start = i + 2;
            // Find the terminator: BEL (\x07) or ST (\x1b\\)
            let mut end = osc_start;
            while end < data.len() {
                if data[end] == 0x07 {
                    break;
                }
                if data[end] == 0x1b && end + 1 < data.len() && data[end + 1] == b'\\' {
                    break;
                }
                end += 1;
            }
            if end < data.len() {
                let payload = &data[osc_start..end];
                // OSC 11 ; ? — query background color
                if payload == b"11;?" {
                    // Respond with dark background (rgb:0000/0000/0000)
                    results.push("\x1b]11;rgb:0000/0000/0000\x1b\\".into());
                }
                // OSC 10 ; ? — query foreground color
                else if payload == b"10;?" {
                    results.push("\x1b]10;rgb:ffff/ffff/ffff\x1b\\".into());
                }
                // OSC 4 ; N ; ? — query palette color N
                else if payload.starts_with(b"4;") && payload.ends_with(b";?") {
                    let idx_bytes = &payload[2..payload.len() - 2];
                    if let Ok(idx_str) = std::str::from_utf8(idx_bytes)
                        && let Ok(idx) = idx_str.parse::<u8>()
                    {
                        let (r, g, b) = xterm256_color(idx);
                        results.push(format!("\x1b]4;{idx};rgb:{r:04x}/{g:04x}/{b:04x}\x1b\\"));
                    }
                }
                // OSC 7 — shell integration reports its working directory
                // as a file:// URL at every prompt (docs/protocol.md,
                // "Working directory tracking").  Last valid report in the
                // chunk wins.
                else if let Some(url) = payload.strip_prefix(b"7;")
                    && let Some(cwd) = parse_osc7_url(url, local_hostname())
                {
                    osc7_cwd = Some(cwd);
                }
                i = end + if data[end] == 0x07 { 1 } else { 2 };
                continue;
            }
            i = end;
            continue;
        }

        // Handle CSI sequences: \x1b[ ...
        if i + 2 >= data.len() || data[i + 1] != b'[' {
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
                // Widen to u32 so the cell-size multiply cannot overflow for any
                // u16 terminal dimension (max 65535*16 = 1_048_560, fits in u32).
                // Previously `rows * 16` / `cols * 8` were u16*u16 and panicked
                // (debug) or wrapped (release) for large terminals.
                Some(format!("\x1b[4;{};{}t", rows as u32 * 16, cols as u32 * 8))
            }
            _ => None,
        };
        if let Some(r) = resp {
            results.push(r);
        }
    }
    TerminalScan {
        responses: results,
        osc7_cwd,
    }
}

/// Record an OSC 7 report against a PTY's stored cwd; returns the
/// `S2C_TERM_CWD_EVENT` to broadcast only when the value changed.  Shells
/// re-emit OSC 7 at every prompt, so identical repeats must produce no
/// traffic (docs/protocol.md, "Working directory tracking").
fn note_osc7_cwd(stored: &mut Option<String>, pty_id: u16, cwd: Option<String>) -> Option<Vec<u8>> {
    let cwd = cwd?;
    if stored.as_deref() == Some(cwd.as_str()) {
        return None;
    }
    let msg = blit_remote::msg_term_cwd_event(pty_id, &cwd);
    *stored = Some(cwd);
    Some(msg)
}

/// Working-directory precedence for `C2S_TERM_CWD` (docs/protocol.md,
/// "Working directory tracking"): prefer the cwd the shell itself reported
/// via OSC 7 — it is fresher (re-emitted at every prompt by the interactive
/// shell, not whatever the kernel tracks for the immediate PTY child) and
/// costs nothing, while the kernel fallback (`pty::pty_cwd`: /proc readlink
/// on Linux, proc_pidinfo on macOS) is a per-request syscall that only sees
/// the direct child.  Shells without OSC 7 integration never populate the
/// report, so the kernel path remains the fallback.
fn resolve_term_cwd(osc7: Option<&str>, kernel: impl FnOnce() -> Option<String>) -> Option<String> {
    match osc7 {
        Some(cwd) => Some(cwd.to_owned()),
        None => kernel(),
    }
}

/// Answer a refused `C2S_CREATE2` that asked for a correlated outcome.
///
/// `CREATE`, `CREATE_AT`, `CREATE_N`, and `CREATE2` without
/// [`CREATE2_WANT_STATUS`] keep their success-only contract, so this is a
/// no-op for them — a server must not send `S2C_CREATE_FAILED` to a client
/// that did not ask for it (docs/protocol.md, "Common status registry").
fn refuse_create(
    sess: &Session,
    client_id: u64,
    want_status: bool,
    nonce: u16,
    status: u8,
    detail: &str,
) {
    if !want_status {
        return;
    }
    if let Some(c) = sess.clients.get(&client_id) {
        let _ = send_outbox(c, blit_remote::msg_create_failed(nonce, status, detail));
    }
}

/// Read a `CREATE2` tag out of `data`, or name why it is unusable.
///
/// `data` is the whole message; the tag is `[tag_len:2]` at offset 8 followed
/// by that many bytes.  Both failures used to fall back to an empty tag and
/// let the create proceed, which breaks the one-outcome contract in two
/// different ways.  A client correlating terminals by tag gets one it can
/// never match.  Worse, an overrunning `tag_len` leaves the read cursor past
/// the end of the message, so a `CREATE2` carrying a command but no cwd or
/// deadline — nothing else left to bounds-check it — finds no command bytes
/// and spawns the default shell instead of what was asked for.
fn create2_tag(data: &[u8]) -> Result<&str, &'static str> {
    let tag_len = u16::from_le_bytes([data[8], data[9]]) as usize;
    let bytes = data
        .get(10..10 + tag_len)
        .ok_or("tag length past end of message")?;
    std::str::from_utf8(bytes).map_err(|_| "tag is not valid UTF-8")
}

/// Name the field that would not survive `S2C_LIST`'s `u16` length prefixes,
/// or `None` when the record is representable.  `pty_list_msg` casts both
/// lengths with `as u16`, so an oversize value does not fail loudly — it
/// silently truncates and desynchronizes the frame for every client.
fn oversize_list_field(tag: &str, command: Option<&str>) -> Option<&'static str> {
    if tag.len() > u16::MAX as usize {
        return Some("tag");
    }
    if command.is_some_and(|c| c.len() > u16::MAX as usize) {
        return Some("command");
    }
    None
}

/// Diagnostic for a `STATUS_BUDGET` creation refusal.  `allocate_pty_id`
/// returns `None` for two different exhaustions and the operator fix differs,
/// so name which one was hit.
/// The timer state a `C2S_DEADLINE` puts a terminal into: `(deadline,
/// stop_deadline, exit_reason)`.
///
/// Split out from the handler so the stand-down rule is pinned by a test
/// rather than by three assignments that are easy to get half-right: the
/// pending SIGKILL has to be cancelled whether the message re-arms or clears,
/// or a refresh that lands inside the grace kills the terminal it was sent to
/// save.
fn armed_deadline(now: Instant, ms: u32) -> (Option<Instant>, Option<Instant>, u8) {
    let deadline = (ms > 0).then(|| now + Duration::from_millis(ms as u64));
    (deadline, None, blit_remote::EXIT_REASON_NORMAL)
}

fn pty_budget_detail(live: usize, max_ptys: usize) -> String {
    if max_ptys > 0 && live >= max_ptys {
        format!("terminal cap reached ({max_ptys}); raise --max-ptys or close a terminal")
    } else {
        "terminal id space exhausted".to_string()
    }
}

/// How often the supervisor sweeps when nothing has woken it.
///
/// On Unix this is a pure backstop — SIGCHLD wakes it the moment a child
/// dies, and the sweep only covers a missed signal (they coalesce, so two
/// children dying together deliver one).  Windows has no SIGCHLD and this is
/// the actual detection latency.
const SUPERVISOR_SWEEP_INTERVAL: Duration = Duration::from_secs(5);

/// Reactive lifecycle loop, deliberately not part of the delivery tick.
///
/// The tick only schedules itself while a client is attached —
/// `blanket_frame_interval` returns `None` on an empty client map and every
/// other deadline it computes is client-gated — so a server with nobody
/// watching parks on `delivery_notify` indefinitely.  That is precisely when
/// a runaway command needs supervising, so lifecycle work gets its own loop.
async fn supervisor_loop(state: AppState) {
    loop {
        // Wake at whichever comes first: something asked us to recompute, an
        // armed deadline is due, or the backstop sweep comes round.
        let next = {
            let sess = state.session.lock().await;
            earliest_armed_deadline(&sess)
        };
        let sweep = Instant::now() + SUPERVISOR_SWEEP_INTERVAL;
        let wake = next.map_or(sweep, |d| d.min(sweep));
        tokio::select! {
            _ = state.supervisor_notify.notified() => {}
            _ = tokio::time::sleep_until(tokio::time::Instant::from_std(wake)) => {}
        }
        supervise(&state).await;
    }
}

/// The soonest instant the supervisor has work to do, or `None` when nothing
/// is armed.
fn earliest_armed_deadline(sess: &Session) -> Option<Instant> {
    sess.ptys
        .values()
        .filter(|pty| !pty.exited)
        .filter_map(|pty| pty.deadline.into_iter().chain(pty.stop_deadline).min())
        .min()
}

/// Terminals to evict to stay inside the retention bounds, oldest first.
///
/// Pure so the policy is testable without a real PTY: it only needs when
/// each exited terminal exited.
fn slots_to_evict(
    mut exited: Vec<(u16, Instant)>,
    now: Instant,
    max_exited: usize,
    linger: Duration,
) -> Vec<u16> {
    exited.sort_by_key(|&(_, at)| at);
    let mut doomed: Vec<u16> = Vec::new();
    if !linger.is_zero() {
        let expired = exited
            .iter()
            .filter(|&&(_, at)| now.duration_since(at) >= linger);
        doomed.extend(expired.map(|&(id, _)| id));
    }
    if max_exited > 0 && exited.len() > max_exited {
        let over = exited.len() - max_exited;
        doomed.extend(exited.iter().take(over).map(|&(id, _)| id));
    }
    doomed.sort_unstable();
    doomed.dedup();
    doomed
}

/// Drop exited terminals that have fallen outside the retention bounds.
///
/// `cleanup_pty_internal` marks a terminal exited and keeps its entry so the
/// output stays readable; nothing but an explicit `C2S_CLOSE` ever removed
/// one, so a client that creates a terminal per task and never closes it grew
/// the map until the id space ran out. Eviction takes the same path a
/// `C2S_CLOSE` would and broadcasts the same `S2C_CLOSED`, so clients need no
/// new message to understand it.
///
/// Only ever touches terminals whose command has already exited.
async fn evict_exited(state: &AppState) {
    let now = Instant::now();
    let mut sess = state.session.lock().await;
    let exited: Vec<(u16, Instant)> = sess
        .ptys
        .iter()
        .filter_map(|(&id, pty)| pty.exited_at.map(|at| (id, at)))
        .collect();
    let doomed = slots_to_evict(exited, now, max_exited(), exited_linger());
    for id in doomed {
        let Some(pty) = sess.ptys.remove(&id) else {
            continue;
        };
        // Already exited by construction, so the fd and the child are gone;
        // this is only dropping the retained terminal state.
        drop(pty);
        state.pty_fds.write().unwrap().remove(&id);
        for client in sess.clients.values_mut() {
            unsubscribe_client_from(client, id);
        }
        let mut msg = vec![S2C_CLOSED];
        msg.extend_from_slice(&id.to_le_bytes());
        sess.send_to_all(&msg);
    }
}

/// Signal numbers for the stop sequence.  Spelled out rather than taken from
/// `libc` because this code is shared with Windows, where `kill_pty` treats
/// the number as an opaque "not SIGINT" and terminates the job.
const SIGKILL: i32 = 9;
const SIGTERM: i32 = 15;

/// Act on terminals whose deadline has come due.
///
/// Expiry is a two-step stop: SIGTERM to the group, then SIGKILL once the
/// grace elapses, so a command that handles SIGTERM gets to unwind. The
/// attribution is recorded now and travels to `S2C_EXITED` later, because the
/// terminal does not finish exiting until the child actually dies.
async fn enforce_deadlines(state: &AppState) {
    let now = Instant::now();
    let mut sess = state.session.lock().await;
    for pty in sess.ptys.values_mut() {
        if pty.exited {
            continue;
        }
        if pty.stop_deadline.is_some_and(|d| now >= d) {
            pty.stop_deadline = None;
            pty::kill_pty(&pty.handle, SIGKILL, true);
        } else if pty.deadline.is_some_and(|d| now >= d) {
            pty.deadline = None;
            pty.exit_reason = blit_remote::EXIT_REASON_DEADLINE;
            pty.stop_deadline =
                Some(now + Duration::from_millis(blit_remote::DEADLINE_STOP_GRACE_MS as u64));
            pty::kill_pty(&pty.handle, SIGTERM, true);
        }
    }
}

/// One supervisor pass: notice children that have exited and run the exit
/// path for them.
///
/// Exit used to be detected only by EOF on the master fd, which reports "the
/// last fd on the slave closed", not "the child exited".  A grandchild
/// holding the slave open kept a dead terminal marked `running` forever, and
/// `blit terminal wait` blocked until its own client-side timeout.
async fn supervise(state: &AppState) {
    let exited: Vec<(u16, u64)> = {
        let sess = state.session.lock().await;
        sess.ptys
            .iter()
            .filter(|(_, pty)| !pty.exited && pty::poll_child_exited(&pty.handle))
            .map(|(&id, pty)| (id, pty.generation))
            .collect()
    };
    for (id, generation) in exited {
        cleanup_pty_internal(id, Some(generation), state).await;
    }
    // After the exit scan, never before it: `reap_zombies` waits a child
    // without marking its terminal exited, so between that wait and the next
    // scan the pty is `!exited` with its pid already freed.  Signalling first
    // would aim the stop sequence's `kill(-pid)` at a released process group.
    enforce_deadlines(state).await;
    // The backstop still runs, now targeted at owned pids only, so a child
    // whose SIGCHLD we missed cannot linger as a zombie.
    pty::reap_zombies();
    // The audio pipeline's children are nobody else's to collect on this
    // cadence: the health check that reaps them as a side effect lives in
    // the delivery tick, which is asleep whenever no client is attached.
    #[cfg(target_os = "linux")]
    {
        let mut sess = state.session.lock().await;
        if let Some(cs) = sess.compositor.as_mut()
            && let Some(ap) = cs.audio_pipeline.as_mut()
        {
            ap.reap_children();
        }
    }
    evict_exited(state).await;
}

/// Run a terminal's exit path.
///
/// `generation` is the child this cleanup was decided for.  The EOF path in
/// the delivery tick defers by 50ms, and the supervisor now reaches the same
/// terminal within a millisecond of SIGCHLD, so a client that sees
/// `S2C_EXITED` and immediately restarts can have a fresh child running by the
/// time the deferred call lands.  Without the check that call would drop the
/// new child's fd, hang it up, and broadcast a second `S2C_EXITED` with an
/// unknown status — the one place the exactly-once contract actually breaks.
/// `None` means "whatever is there now", for callers that just looked.
async fn cleanup_pty_internal(pty_id: u16, generation: Option<u64>, state: &AppState) {
    let mut sess = state.session.lock().await;
    if let Some(pty) = sess.ptys.get_mut(&pty_id) {
        if generation.is_some_and(|g| g != pty.generation) {
            return;
        }
        if pty.exited {
            return;
        }
        state.pty_fds.write().unwrap().remove(&pty_id);
        pty.exited = true;
        pty.exited_at = Some(Instant::now());
        pty.deadline = None;
        pty.stop_deadline = None;
        pty::close_pty(&pty.handle);
        pty.exit_status = pty::collect_exit_status(&pty.handle);
        pty.mark_dirty();
        let msg = blit_remote::msg_exited_reason(pty_id, pty.exit_status, pty.exit_reason);
        sess.send_to_all(&msg);
    }
}

fn take_snapshot(pty: &mut Pty) -> FrameState {
    if pty.lflag_last.elapsed() >= Duration::from_millis(250) {
        pty.lflag_cache = pty::pty_lflag(&pty.handle);
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
    if send_outbox(client, msg).is_ok() {
        client.last_sent.insert(pid, current);
        record_send(client, bytes, now, paced);
        client.frames_sent = client.frames_sent.wrapping_add(1);
        SendOutcome::Sent
    } else {
        // Receiver dropped — client disconnected.  Advance last_sent so
        // the next diff (if any) is small rather than accumulating stale
        // changes.
        client.last_sent.insert(pid, current);
        SendOutcome::Backpressured
    }
}

pub async fn run(config: Config) {
    let state: AppState = Arc::new(AppStateInner {
        config,
        boot_generation: new_boot_generation(),
        session: Mutex::new(Session::new()),
        pty_fds: Arc::new(std::sync::RwLock::new(HashMap::new())),
        delivery_notify: Arc::new(Notify::new()),
        shutdown_notify: Arc::new(Notify::new()),
        supervisor_notify: Arc::new(Notify::new()),
        active_connections: std::sync::atomic::AtomicUsize::new(0),
    });

    // Start the compositor eagerly so it is ready before any client
    // connects or any terminal is created.
    if !state.config.skip_compositor {
        let notify = state.delivery_notify.clone();
        let event_notify = Arc::new(move || notify.notify_one()) as Arc<dyn Fn() + Send + Sync>;
        let mut sess = state.session.lock().await;
        sess.ensure_compositor(
            state.config.verbose,
            event_notify,
            &state.config.vaapi_device,
        );
    }

    let delivery_state = state.clone();
    tokio::spawn(async move {
        let mut next_deadline: Option<Instant> = None;
        loop {
            if let Some(deadline) = next_deadline {
                tokio::select! {
                    _ = delivery_state.delivery_notify.notified() => {}
                    _ = tokio::time::sleep_until(tokio::time::Instant::from_std(deadline)) => {}
                }
            } else {
                delivery_state.delivery_notify.notified().await;
            }
            let outcome = tick(&delivery_state).await;
            next_deadline = outcome.next_deadline;
        }
    });

    let supervisor_state = state.clone();
    tokio::spawn(async move {
        supervisor_loop(supervisor_state).await;
    });

    // SIGCHLD is what makes exit detection prompt without polling.  The
    // handler does nothing but wake the supervisor: reaping from a signal
    // context would race the session mutex, and the supervisor already knows
    // which pids it owns.
    #[cfg(unix)]
    {
        let sigchld_state = state.clone();
        tokio::spawn(async move {
            let Ok(mut sigchld) =
                tokio::signal::unix::signal(tokio::signal::unix::SignalKind::child())
            else {
                eprintln!("[supervisor] SIGCHLD unavailable; falling back to the poll");
                return;
            };
            loop {
                sigchld.recv().await;
                sigchld_state.supervisor_notify.notify_one();
            }
        });
    }

    // Warm the KV store off the serving paths (docs/design/kv.md
    // § Storage): the load+hash of the whole database happens now, in the
    // background, instead of inline in the first connection's first KV
    // message. BLIT_KV=0 disables the family, so nothing to warm.
    if !std::env::var("BLIT_KV").is_ok_and(|v| v == "0") {
        kv::warm();
    }

    #[cfg(unix)]
    if let Some(channel_fd) = state.config.fd_channel {
        blit_sd_notify::notify_ready(state.config.verbose);
        ipc::run_fd_channel(channel_fd, state).await;
        return;
    }

    #[cfg(unix)]
    let listener = {
        if let Some(l) = IpcListener::from_systemd_fd(state.config.verbose) {
            l
        } else {
            IpcListener::bind(&state.config.ipc_path, state.config.verbose)
        }
    };
    #[cfg(not(unix))]
    let mut listener = IpcListener::bind(&state.config.ipc_path, state.config.verbose);

    blit_sd_notify::notify_ready(state.config.verbose);

    // Broadcast S2C_QUIT on SIGTERM / SIGINT so clients can reconnect promptly
    // instead of waiting for a transport-level timeout.
    {
        let state = state.clone();
        tokio::spawn(async move {
            #[cfg(unix)]
            {
                use tokio::signal::unix::{SignalKind, signal};
                let mut sigterm = signal(SignalKind::terminate()).expect("signal handler");
                let mut sigint = signal(SignalKind::interrupt()).expect("signal handler");
                tokio::select! {
                    _ = sigterm.recv() => {}
                    _ = sigint.recv() => {}
                }
            }
            #[cfg(not(unix))]
            {
                let _ = tokio::signal::ctrl_c().await;
            }
            let sess = state.session.lock().await;
            sess.send_to_all(&[S2C_QUIT]);
            drop(sess);
            state.shutdown_notify.notify_one();
        });
    }

    let shutdown = state.shutdown_notify.clone();
    loop {
        let stream = tokio::select! {
            result = listener.accept() => match result {
                Ok(s) => s,
                Err(e) => {
                    eprintln!("accept error: {e}");
                    tokio::time::sleep(Duration::from_millis(100)).await;
                    continue;
                }
            },
            _ = shutdown.notified() => break,
        };
        let max = state.config.max_connections;
        if max > 0 {
            let current = state
                .active_connections
                .load(std::sync::atomic::Ordering::Relaxed);
            if current >= max {
                eprintln!("max connections ({max}) reached, rejecting");
                drop(stream);
                continue;
            }
        }
        state
            .active_connections
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let state = state.clone();
        tokio::spawn(async move {
            handle_client(stream, state.clone()).await;
            state
                .active_connections
                .fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
        });
    }
    // Brief grace period for S2C_QUIT to reach clients before the process exits.
    tokio::time::sleep(Duration::from_millis(100)).await;
}

/// Minimum interval between blanket RequestFrame rounds.  Keeps video
/// players (mpv) and browsers ticking even when no client is consuming
/// frames.  Also used as the maximum tick-loop sleep so the loop never
/// blocks longer than this.
///
/// When any client has an active surface subscription, use 62.5 ms (16 Hz)
/// so video players keep getting frame callbacks.  Without active surfaces,
/// 250 ms (4 Hz) is enough to keep apps from stalling entirely.
///
/// This is a floor on liveness, not the frame rate: a subscribed surface is
/// paced by `frame_window` and the adaptive controller, which run far above
/// 16 Hz.  The blanket round only exists so an app nobody is watching still
/// makes progress.
const BLANKET_FRAME_INTERVAL_IDLE: Duration = Duration::from_millis(250);
const BLANKET_FRAME_INTERVAL_SURFACE: Duration = Duration::from_micros(62_500);

/// Returns the interval at which the tick loop must send blanket
/// `RequestFrame` events to keep Wayland apps (mpv, browsers, etc.)
/// making progress. Returns `None` when no clients are connected — in
/// that state the loop can sleep purely on event notifications, and
/// apps pause until a viewer reconnects (resuming within SURFACE).
fn blanket_frame_interval(sess: &Session) -> Option<Duration> {
    if sess.clients.is_empty() {
        return None;
    }
    let has_surface_subs = sess
        .clients
        .values()
        .any(|c| !c.surface_subscriptions.is_empty());
    if has_surface_subs {
        Some(BLANKET_FRAME_INTERVAL_SURFACE)
    } else {
        Some(BLANKET_FRAME_INTERVAL_IDLE)
    }
}

async fn tick(state: &AppState) -> TickOutcome {
    let mut sess = state.session.lock().await;
    sess.tick_fires += 1;
    let mut next_deadline: Option<Instant> = None;
    let now = Instant::now();

    // Emit pacing metrics every 10s for each client, even when no ACKs
    // are flowing (idle session): the ACK handler also calls this so the
    // first client with traffic still owns the tick-counter reset.
    let log_client_ids: Vec<u64> = sess.clients.keys().copied().collect();
    for cid in log_client_ids {
        maybe_log_pacing_metrics(&mut sess, cid, state.config.verbose);
    }

    // Application-level keepalive. Only scheduled when a client is
    // connected — otherwise there's no one to ping and the timer would
    // be pure polling cost.
    let ping_interval = state.config.ping_interval;
    if !ping_interval.is_zero() && !sess.clients.is_empty() {
        if now.duration_since(sess.last_ping) >= ping_interval {
            sess.send_to_all(&[S2C_PING]);
            sess.last_ping = now;
        }
        let next_ping = sess.last_ping + ping_interval;
        next_deadline = Some(next_deadline.map_or(next_ping, |d: Instant| d.min(next_ping)));
    }

    // Surface IDs whose per-client encoders need to be invalidated.
    let mut invalidate_client_encoders: Vec<u16> = Vec::new();
    let mut vulkan_unavailable: Vec<(u16, u64)> = Vec::new();
    // Surface IDs resized by the compositor this tick.  After the
    // compositor borrow is released we wake pacing for every client
    // subscribed to each sid so the first post-resize frame bypasses
    // the per-surface time gate.
    let mut resized_surface_ids: Vec<u16> = Vec::new();

    let mut surface_commit_count = 0u32;
    if let Some(cs) = sess.compositor.as_mut() {
        let mut events = Vec::new();
        while let Ok(event) = cs.handle.event_rx.try_recv() {
            events.push(event);
        }
        let mut broadcast: Vec<Vec<u8>> = Vec::new();
        for event in events {
            match event {
                CompositorEvent::SurfaceCreated {
                    surface_id,
                    title,
                    app_id,
                    parent_id,
                    width,
                    height,
                } => {
                    broadcast.push(msg_surface_created(
                        surface_id, parent_id, width, height, &title, &app_id,
                    ));
                    cs.surfaces.insert(
                        surface_id,
                        CachedSurfaceInfo {
                            surface_id,
                            parent_id,
                            width,
                            height,
                            title,
                            app_id,
                        },
                    );
                    last_pixels_remove_for_sid(&mut cs.last_pixels, surface_id);
                    last_encoded_remove_for_sid(&mut cs.last_encoded, surface_id);
                    invalidate_client_encoders.push(surface_id);
                }
                CompositorEvent::SurfaceDestroyed { surface_id } => {
                    cs.surfaces.remove(&surface_id);
                    last_pixels_remove_for_sid(&mut cs.last_pixels, surface_id);
                    last_encoded_remove_for_sid(&mut cs.last_encoded, surface_id);
                    cs.last_configured_size.remove(&surface_id);
                    cs.last_resize_at.remove(&surface_id);
                    cs.pending_resize.remove(&surface_id);
                    cs.native_sizes.remove(&surface_id);
                    invalidate_client_encoders.push(surface_id);
                    broadcast.push(msg_surface_destroyed(surface_id));
                }
                CompositorEvent::SurfaceCommit {
                    surface_id,
                    width,
                    height,
                    pixels,
                    timestamp_ms,
                } => {
                    surface_commit_count += 1;
                    // The compositor emits one SurfaceCommit per
                    // (surface, target size).  The largest entry is
                    // the native composite (also used by the pointer
                    // path), but `info.width/height` always reflect
                    // the most-recent emission — that's fine because
                    // info dims are only read for fallback display in
                    // the surface-created event when nothing else is
                    // known.
                    if let Some(info) = cs.surfaces.get_mut(&surface_id) {
                        info.width = width as u16;
                        info.height = height as u16;
                    }
                    cs.pixel_generation += 1;
                    cs.last_pixels.insert(
                        (surface_id, width, height),
                        LastPixels {
                            width,
                            height,
                            pixels,
                            generation: cs.pixel_generation,
                            timestamp_ms,
                        },
                    );
                }
                CompositorEvent::SurfaceEncoded {
                    frame,
                    timestamp_ms,
                } => {
                    surface_commit_count += 1;
                    cs.pixel_generation += 1;
                    cs.last_encoded.insert(
                        (frame.surface_id, frame.client_id),
                        LastEncoded {
                            width: frame.width,
                            height: frame.height,
                            data: frame.data,
                            is_keyframe: frame.is_keyframe,
                            codec_flag: frame.codec_flag,
                            generation: cs.pixel_generation,
                            timestamp_ms,
                        },
                    );
                }
                CompositorEvent::VulkanEncoderUnavailable {
                    surface_id,
                    client_id,
                } => {
                    // The compositor could not give this client a session
                    // (driver refusal, or we are at the session cap).  Drop
                    // the tracking entry so the next tick routes it through
                    // a server-side encoder instead of waiting forever.
                    vulkan_unavailable.push((surface_id, client_id));
                }
                CompositorEvent::SurfaceTitle { surface_id, title } => {
                    if let Some(info) = cs.surfaces.get_mut(&surface_id) {
                        info.title = title.clone();
                    }
                    broadcast.push(msg_surface_title(surface_id, &title));
                }
                CompositorEvent::SurfaceAppId { surface_id, app_id } => {
                    if let Some(info) = cs.surfaces.get_mut(&surface_id) {
                        info.app_id = app_id.clone();
                    }
                    broadcast.push(msg_surface_app_id(surface_id, &app_id));
                }
                CompositorEvent::SurfaceResized {
                    surface_id,
                    width,
                    height,
                } => {
                    if let Some(info) = cs.surfaces.get_mut(&surface_id) {
                        info.width = width;
                        info.height = height;
                    }
                    cs.native_sizes
                        .insert(surface_id, (width as u32, height as u32));
                    last_pixels_remove_for_sid(&mut cs.last_pixels, surface_id);
                    last_encoded_remove_for_sid(&mut cs.last_encoded, surface_id);
                    // Don't eagerly invalidate client encoders here.  The
                    // encode path already checks for dimension mismatches
                    // (source_dimensions != pixel size) and recreates the
                    // encoder on demand.  Eagerly destroying encoders on
                    // every intermediate size during a drag-resize causes
                    // expensive encoder teardown+creation cycles for sizes
                    // that may never actually be encoded (because a newer
                    // SurfaceCommit arrives before the next encode tick).
                    broadcast.push(msg_surface_resized(surface_id, width, height));
                    resized_surface_ids.push(surface_id);
                }
                CompositorEvent::ClipboardContent {
                    mime_type, data, ..
                } => {
                    broadcast.push(msg_s2c_clipboard_content(&mime_type, &data));
                }
                CompositorEvent::SurfaceCursor { surface_id, cursor } => {
                    // Format: [0x29][surface_id:2][type:1][payload...]
                    // type 0 = named: [name_len:1][name:N]
                    // type 1 = hidden (no payload)
                    // type 2 = custom: [hotx:2][hoty:2][w:2][h:2][png:N]
                    let mut msg = Vec::new();
                    msg.push(blit_remote::S2C_SURFACE_CURSOR);
                    msg.extend_from_slice(&surface_id.to_le_bytes());
                    match &cursor {
                        blit_compositor::CursorImage::Named(name) => {
                            msg.push(0); // type = named
                            msg.push(name.len() as u8);
                            msg.extend_from_slice(name.as_bytes());
                        }
                        blit_compositor::CursorImage::Hidden => {
                            msg.push(1); // type = hidden
                        }
                        blit_compositor::CursorImage::Custom {
                            hotspot_x,
                            hotspot_y,
                            width,
                            height,
                            rgba,
                        } => {
                            // Encode as PNG to keep message small.
                            let mut png_buf = Vec::new();
                            {
                                let mut encoder =
                                    png::Encoder::new(&mut png_buf, *width as u32, *height as u32);
                                encoder.set_color(png::ColorType::Rgba);
                                encoder.set_depth(png::BitDepth::Eight);
                                if let Ok(mut writer) = encoder.write_header() {
                                    let _ = writer.write_image_data(rgba);
                                }
                            }
                            msg.push(2); // type = custom
                            msg.extend_from_slice(&hotspot_x.to_le_bytes());
                            msg.extend_from_slice(&hotspot_y.to_le_bytes());
                            msg.extend_from_slice(&width.to_le_bytes());
                            msg.extend_from_slice(&height.to_le_bytes());
                            msg.extend_from_slice(&png_buf);
                        }
                    }
                    broadcast.push(msg);
                }
            }
        }
        for msg in &broadcast {
            sess.send_to_all(msg);
        }
    }
    sess.surface_commits += surface_commit_count;

    // Apply deferred per-client encoder invalidation (couldn't mutate
    // sess.clients while sess.compositor was borrowed above).  Any
    // surface event (resize, destroy, reconfigure) invalidates every
    // encoder bound to that sid's pixel stream.
    for sid in invalidate_client_encoders {
        let mut had_vulkan = false;
        for c in sess.clients.values_mut() {
            // Everything about the encode is rebuilt against the new
            // composite, so the entry goes.  A scaled subscriber's requested
            // size is not part of that: it describes the client's own
            // viewport, which this event says nothing about.  Dropping it
            // here would silently revert a thumbnail to full-size encoding
            // for as long as it took to resubscribe — and a surface it never
            // interacts with may never give it a reason to.
            let still_subscribed = c.surface_subscriptions.contains(&sid);
            let previous = c.surface_subs.remove(&sid);
            if still_subscribed && let Some(target) = previous.and_then(|s| s.scaled_target) {
                c.surface_subs.entry(sid).or_default().scaled_target = Some(target);
            }
            had_vulkan |= c.vulkan_video_surfaces.remove(&sid).is_some();
            forget_surface_inflight(c, sid);
        }
        // The compositor's sessions are sized against the old composite,
        // so drop every client's encoder for this surface.  Selection will
        // rebuild them at the new size.
        if had_vulkan && let Some(cs) = sess.compositor.as_ref() {
            let _ = cs.handle.command_tx.send(
                blit_compositor::CompositorCommand::DestroyVulkanEncoder {
                    surface_id: sid as u32,
                    client_id: None,
                },
            );
            cs.handle.wake();
        }
    }

    // A client the compositor could not give a session to falls back to a
    // server-side encoder.  The refusal has to be latched: selection only
    // asks whether the client's target matches native, which it still
    // does, so without the latch the next tick re-selects Vulkan, is
    // refused again, and the surface never reaches an encoder at all.
    for (sid, cid) in vulkan_unavailable {
        if let Some(c) = sess.clients.get_mut(&cid)
            && let Some((_, codec_flag)) = c.vulkan_video_surfaces.remove(&sid)
        {
            // Latch only the encoder that was actually refused.  The entry we
            // just removed says which one was in flight; anything else in the
            // Vulkan tier is still worth trying on the next tick.
            let refused = if codec_flag == SurfaceEncoderPreference::VulkanVideoAV1.codec_flag() {
                SurfaceEncoderPreference::VulkanVideoAV1
            } else {
                SurfaceEncoderPreference::VulkanVideoH264
            };
            // Keep the rest of the subscription: it carries this client's
            // bandwidth/speed/codec overrides, which a refusal is no
            // reason to reset.  Clearing the encoder is enough to make the
            // next tick build a server-side one.
            let sub = c.surface_subs.entry(sid).or_default();
            sub.vulkan_refused |= refused.vulkan_refusal_bit();
            sub.encoder = None;
            sub.has_keyframe = false;
            if sub.encode_in_flight || sub.creation_in_flight {
                sub.encoder_invalidated = true;
            }
            forget_surface_inflight(c, sid);
            eprintln!(
                "[vulkan-video] cid={cid} sid={sid}: compositor declined a session, \
                 falling back to a server-side encoder",
            );
        }
        if let Some(cs) = sess.compositor.as_mut() {
            cs.last_encoded.remove(&(sid, cid));
        }
    }

    // Wake pacing for every subscriber of a compositor-resized surface.
    // Reset the burst window and clear next_send_at so the first frame
    // at the new dimensions flows at wire speed instead of waiting for
    // the per-surface time gate (up to ~1/fps), and force a keyframe
    // so decoders recover cleanly after the dimension change.
    for sid in resized_surface_ids {
        for c in sess.clients.values_mut() {
            if !c.surface_subscriptions.contains(&sid) {
                continue;
            }
            let s = c.surface_subs.entry(sid).or_default();
            s.burst_remaining = SURFACE_BURST_FRAMES;
            s.next_send_at = None;
            s.nal_none_streak = 0;
            s.nal_none_latched_at = None;
            s.has_keyframe = false;
        }
    }

    // Per-client surface encode + deliver.
    // Each client has its own encoder per surface.  We encode from
    // shared last_pixels into each client's encoder and deliver.
    //
    // Snapshot pixel metadata from the compositor first to avoid
    // holding an immutable borrow on sess.compositor while mutating
    // sess.clients.
    // Snapshot every surface entry so each client's per-surface encoder
    // can draw from the latest pixels without holding the compositor
    // borrow through the (lengthy) encoder-dispatch loop below.
    // (sid, width, height, generation, timestamp_ms) per per-target
    // entry.  One sid can appear several times — once for each
    // distinct (width, height) the renderer produced (per-encoder
    // target plus the native composite).
    let pixel_snapshot: Vec<(u16, u32, u32, u64, u32)> = sess
        .compositor
        .as_ref()
        .map(|cs| {
            cs.last_pixels
                .iter()
                .map(|(&(sid, _, _), lp)| {
                    (sid, lp.width, lp.height, lp.generation, lp.timestamp_ms)
                })
                .collect()
        })
        .unwrap_or_default();
    // Compositor-encoded bitstreams live on their own generation stream,
    // one per `(surface, client)`.  Snapshotted here so the encode loop can
    // ask "does this client already have this frame?" without reaching back
    // into `sess.compositor` while it holds a client borrow.
    let encoded_snapshot: HashMap<(u16, u64), u64> = sess
        .compositor
        .as_ref()
        .map(|cs| {
            cs.last_encoded
                .iter()
                .map(|(&key, e)| (key, e.generation))
                .collect()
        })
        .unwrap_or_default();
    if pixel_snapshot.is_empty() {
        sess.ticks_pixel_snapshot_empty = sess.ticks_pixel_snapshot_empty.saturating_add(1);
    } else {
        sess.pixel_snapshot_len = pixel_snapshot.len();
    }

    // ---- Surface encode (off main thread) + deliver ----
    //
    // Collect encode jobs, drop the session lock, run encodes in
    // spawn_blocking, re-acquire the lock, and deliver.

    struct EncodeJob {
        cid: u64,
        sid: u16,
        /// The encoder's source dimensions, equal to this client's
        /// physical viewport.  Pixels arrive at this size from the
        /// compositor — either zero-copy via NV12/VA-Surface DMA-BUFs
        /// (VAAPI GBM-backed externals) or a server-allocated BGRA
        /// staging buffer that the compositor GPU-blit into at this
        /// size (NVENC, software encoders).  These dims go on the
        /// wire as the frame `width`/`height` so each viewer sizes
        /// its `<canvas>` to its own bitstream.
        target_w: u32,
        target_h: u32,
        /// Pixel data to encode (already at target size).
        pixels: blit_compositor::PixelData,
        needs_keyframe: bool,
        encoder: SurfaceEncoder,
        generation: u64,
        /// CLOCK_MONOTONIC ms captured at compositor commit time.
        timestamp_ms: u32,
    }
    struct EncoderCreateParams {
        preferences: Vec<SurfaceEncoderPreference>,
        vaapi_device: String,
        encoding: SurfaceEncoding,
        verbose: bool,
        codec_support: u8,
        chroma: ChromaSubsampling,
    }
    /// A creation task runs `SurfaceEncoder::new` + GBM-buffer
    /// allocation on a blocking thread, then hands back the encoder
    /// and its external buffers to the main loop to register with the
    /// compositor.  No encoding happens here — the first encode runs
    /// on a subsequent tick after the compositor has committed into
    /// the new buffers.
    struct CreateJob {
        cid: u64,
        sid: u16,
        /// Encoder source dimensions = this client's physical viewport.
        /// The compositor may render larger; the encode pipeline
        /// downscales per-client into these dimensions.
        target_w: u32,
        target_h: u32,
        /// The compositor native size `(target_w, target_h)` was inscribed
        /// into.  Handed back to the compositor with the target so it can
        /// tell, without re-deriving our arithmetic, whether the composite
        /// has since moved and the target can no longer be filled without
        /// squashing the picture.
        native_w: u32,
        native_h: u32,
        params: EncoderCreateParams,
    }
    struct CreateResult {
        cid: u64,
        sid: u16,
        /// The compositor native size the target was inscribed into, carried
        /// through so the registration below can stamp it on the target.
        native_w: u32,
        native_h: u32,
        /// None when `SurfaceEncoder::new` failed; the completion
        /// handler logs and latches a backoff so the tick loop doesn't
        /// spin on retries.
        encoder: Option<SurfaceEncoder>,
        fresh: Option<FreshEncoder>,
        /// Creation failed with at least one eligible backend skipped for
        /// being unable to carry a frame this large.  Asking for less will
        /// reach those backends, so the completion handler degrades the cap
        /// and lets the next tick retry immediately instead of spending the
        /// failure backoff on a request that is merely too big.
        oversized: bool,
    }
    /// Metadata shipped with an encode result when the encoder was
    /// created this tick (deferred to spawn_blocking).  `Some` = the
    /// main loop should send S2C_SURFACE_ENCODER, register external
    /// GBM buffers with the compositor, and accept the encoder back.
    struct FreshEncoder {
        name: &'static str,
        codec_string: String,
        #[cfg(target_os = "linux")]
        external_bufs: Vec<blit_compositor::ExternalOutputBuffer>,
    }
    struct EncodeResult {
        cid: u64,
        sid: u16,
        /// Encoded frame dimensions (what goes on the wire).  Equal to
        /// the encoder's source dimensions, i.e. this client's physical
        /// viewport — not the compositor's native size.
        target_w: u32,
        target_h: u32,
        generation: u64,
        encoder: SurfaceEncoder,
        nal_data: Option<(Vec<u8>, bool)>, // (data, is_keyframe)
        codec_flag: u8,
        /// CLOCK_MONOTONIC ms from compositor commit time.
        timestamp_ms: u32,
    }

    let mut encode_jobs: Vec<EncodeJob> = Vec::new();
    let mut create_jobs: Vec<CreateJob> = Vec::new();
    // Surfaces that had encode jobs dispatched this tick.  Used below to
    // eagerly pre-request the next frame so the compositor renders in
    // parallel with the in-flight encode (pipeline overlap).
    let mut encode_dispatched_surfaces: HashSet<u16> = HashSet::new();

    // Collect (cid, subs) for clients that are due, then build encode jobs
    // in a second pass to avoid overlapping borrows.  `subs` is the set of
    // surface ids this client subscribes to.  Whether a keyframe is owed is
    // read per surface inside that second pass, from the sub's own
    // `has_keyframe` — it is not a property of the client.
    struct ClientWork {
        cid: u64,
        subs: HashSet<u16>,
    }
    let mut client_work: Vec<ClientWork> = Vec::new();

    if !pixel_snapshot.is_empty() {
        for (&cid, client) in sess.clients.iter_mut() {
            if !surface_window_open(client) {
                // Log persistent blockage so hangs are visible.
                let now_inst = Instant::now();
                if now_inst
                    .duration_since(client.last_window_blocked_log)
                    .as_secs_f32()
                    > 5.0
                {
                    client.last_window_blocked_log = now_inst;
                    let max_burst: u8 = client
                        .surface_subs
                        .values()
                        .map(|s| s.burst_remaining)
                        .max()
                        .unwrap_or(0);
                    eprintln!(
                        "[surface-gate] cid={cid} surface_window_open=false outbox={}f/{}B (limits {}f/{}B) burst={max_burst}",
                        outbox_queued_frames(client),
                        outbox_queued_bytes(client),
                        OUTBOX_SOFT_QUEUE_LIMIT_FRAMES,
                        OUTBOX_SOFT_QUEUE_LIMIT_BYTES,
                    );
                }
                continue;
            }
            // Per-surface pacing is checked in the inner loop below so
            // that each surface can run at full frame rate independently.
            if client.surface_subscriptions.is_empty() {
                client.skip_no_subs_count = client.skip_no_subs_count.saturating_add(1);
                continue;
            }
            let subs: HashSet<u16> = client.surface_subscriptions.iter().copied().collect();
            client_work.push(ClientWork { cid, subs });
            // Don't advance the deadline here — wait until we know an
            // encode job was actually collected (see below).  Advancing
            // eagerly wastes time slots when the encode is skipped due
            // to in-flight limits or unchanged pixel data.
        }

        // Track which (client, surface) pairs actually had encode jobs
        // collected so we can advance per-surface deadlines afterwards.
        let mut encoded_client_surfaces: HashSet<(u64, u16)> = HashSet::new();

        // Pre-extract compositor Vulkan Video capabilities so we don't
        // need to borrow sess.compositor inside the client-mutation loop.
        let vk_encode_available = sess
            .compositor
            .as_ref()
            .is_some_and(|cs| cs.handle.vulkan_video_encode);
        let vk_encode_av1_available = sess
            .compositor
            .as_ref()
            .is_some_and(|cs| cs.handle.vulkan_video_encode_av1);

        // `(surface, client)` pairs whose Vulkan Video encoder should be
        // torn down after the client loop, because that client now wants a
        // per-client target smaller than the compositor's native size.
        // Deferred so we can mutate the client map and the compositor
        // without holding the per-client mutable borrow used inside the
        // loop.  Only the affected client is torn down; ownership is per
        // pair, so a smaller viewport no longer costs everyone else their
        // hardware encoder.
        let mut vulkan_teardown: Vec<(u16, u64)> = Vec::new();

        // Vulkan Video encoder setup commands to send after the client loop.
        struct VulkanEncoderSetup {
            surface_id: u32,
            client_id: u64,
            codec: u8,
            qp: u8,
            width: u32,
            height: u32,
            is_444: bool,
        }
        let mut pending_vulkan_encoder_setups: Vec<VulkanEncoderSetup> = Vec::new();
        let mut pending_vulkan_keyframe_requests: Vec<(u32, u64)> = Vec::new();
        let mut pending_vulkan_qp_updates: Vec<(u32, u64, u8)> = Vec::new();

        for work in &client_work {
            for &sid in &work.subs {
                // Native dims come from the authoritative `native_sizes`
                // map (see `compositor_native_for_sid` for why the
                // historical "largest pixel snapshot" pick is wrong
                // after a resize).
                let Some((native_w, native_h)) = sess.compositor.as_ref().and_then(|cs| {
                    compositor_native_for_sid(&cs.native_sizes, &pixel_snapshot, sid)
                }) else {
                    let client = sess.clients.get_mut(&work.cid).unwrap();
                    client.skip_last_pixels_mismatch_count =
                        client.skip_last_pixels_mismatch_count.saturating_add(1);
                    continue;
                };
                // Generation / timestamp for the same-gen skip and the
                // Vulkan-Video fast-path fallback come from the
                // matching native pixel entry when present, else from
                // the largest entry for this surface (best effort).
                // These are only consulted when no exact-target snapshot
                // exists, in which case the dispatch loop skips with
                // `(px_w, px_h) != (target_w, target_h)` anyway, so the
                // values are not safety-critical.
                let (native_gen, native_ts) = pixel_snapshot
                    .iter()
                    .find(|&&(s, w, h, _, _)| s == sid && (w, h) == (native_w, native_h))
                    .or_else(|| {
                        pixel_snapshot
                            .iter()
                            .filter(|&&(s, _, _, _, _)| s == sid)
                            .max_by_key(|&&(_, w, h, _, _)| (w as u64) * (h as u64))
                    })
                    .map(|&(_, _, _, g, t)| (g, t))
                    .unwrap_or((0, 0));
                {
                    let client = sess.clients.get_mut(&work.cid).unwrap();
                    client.encode_loop_iters = client.encode_loop_iters.saturating_add(1);
                }
                let client = sess.clients.get_mut(&work.cid).unwrap();

                // Per-surface pacing gate: during burst-start, skip the
                // time-based check so frames flow at wire speed; otherwise
                // each surface independently waits for its own deadline.
                {
                    let (burst, deadline) = client.surface_subs.get(&sid).map_or((0, now), |s| {
                        (s.burst_remaining, s.next_send_at.unwrap_or(now))
                    });
                    if burst == 0 && deadline > now {
                        // Safety clamp: the deadline should never be more
                        // than 2× the send interval ahead.  If it is, snap
                        // back to now so encoding doesn't stall permanently.
                        let interval = surface_send_interval(client, sid);
                        if deadline > now + interval + interval {
                            client.surface_subs.entry(sid).or_default().next_send_at = Some(now);
                        } else {
                            next_deadline = Some(match next_deadline {
                                Some(existing) => existing.min(deadline),
                                None => deadline,
                            });
                            client.skip_pacing_count = client.skip_pacing_count.saturating_add(1);
                            continue;
                        }
                    }
                }

                // A scaled subscription names its own encode box and ignores
                // the mediated view size.  Scale 120 because the size is
                // already in the pixels the client wants out of the encoder;
                // per_client_encode_target only reads the scale to nothing.
                let view = client
                    .surface_subs
                    .get(&sid)
                    .and_then(|s| s.scaled_target)
                    .map(|(w, h)| (w, h, 120))
                    .or_else(|| client.surface_view_sizes.get(&sid).copied());
                let (target_w, target_h) = Session::per_client_encode_target(
                    view,
                    native_w,
                    native_h,
                    surface_encode_cap(&state.config.surface_encoders, client, sid),
                );
                let (enc_w, enc_h) = (target_w, target_h);

                // The target the compositor holds is stamped with the native
                // it was inscribed into, and it refuses to fill one whose
                // stamp has gone stale — otherwise the composite moves
                // first and the frame comes out squashed into the previous
                // aspect.  When the native moves the target usually moves
                // with it and the rebuild below re-stamps; but the
                // inscription can land on the same numbers as before (a
                // one-pixel native change, say, from another viewer nudging
                // the mediated size), and then nothing would ever refresh
                // the stamp and this client would stop receiving frames.
                // The buffers are still the right ones — only the record of
                // what they were sized against is behind.
                let restamp = client.surface_subs.get(&sid).and_then(|s| {
                    let registered = s.last_registered_target?;
                    (registered == (target_w, target_h)
                        && s.last_registered_native != Some((native_w, native_h)))
                    .then_some(registered)
                });
                if let Some((tw, th)) = restamp {
                    client
                        .surface_subs
                        .entry(sid)
                        .or_default()
                        .last_registered_native = Some((native_w, native_h));
                    if let Some(cs) = sess.compositor.as_ref() {
                        let _ = cs.handle.command_tx.send(
                            blit_compositor::CompositorCommand::RestampTarget {
                                surface_id: sid as u32,
                                target_w: tw,
                                target_h: th,
                                native_w,
                                native_h,
                            },
                        );
                        cs.handle.wake();
                    }
                }
                let client = sess.clients.get_mut(&work.cid).unwrap();

                if state.config.verbose {
                    static EDB: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
                    let n = EDB.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    if n < 30 || n.is_multiple_of(500) {
                        eprintln!(
                            "[encode-target #{n}] cid={} sid={sid} view={view:?} native={native_w}x{native_h} target={target_w}x{target_h}",
                            work.cid,
                        );
                    }
                }

                // The compositor produces one snapshot per (sid,
                // target) once the per-client encoder has registered
                // either an external buffer (VAAPI GBM) or a downscale
                // target (NVENC, software).  Find it.  On the very
                // first tick after encoder install the snapshot may not
                // exist yet; we use native as the source for the
                // Vulkan-Video / generation gate below, but the pixels
                // lookup further down requires an exact (sid, w, h)
                // match — feeding mis-sized pixels to a target-sized
                // encoder garbles content (the encoder reads at
                // `source_dimensions` stride into a different-sized
                // buffer, which wraps rows).
                let target_snapshot = pixel_snapshot
                    .iter()
                    .find(|&&(s, w, h, _, _)| s == sid && (w, h) == (target_w, target_h))
                    .copied();
                let (px_w, px_h, px_gen, px_timestamp_ms) = target_snapshot
                    .map(|(_, w, h, g, t)| (w, h, g, t))
                    .unwrap_or((native_w, native_h, native_gen, native_ts));

                // Has anything changed since the frame this client already
                // has?  Answered before the controller runs, because a still
                // surface must not be judged on `frame_bytes` — that EWMA
                // describes motion that has already stopped.
                //
                // A client on a compositor-resident encoder is served from
                // the bitstream stream, not the pixel snapshot, and the two
                // carry independent generations; ask the one it is actually
                // fed from.
                let has_vulkan_enc = client.vulkan_video_surfaces.contains_key(&sid);
                let latest_gen = if has_vulkan_enc {
                    match encoded_snapshot.get(&(sid, work.cid)) {
                        Some(&g) => g,
                        // The session exists but has not produced anything
                        // yet; there is nothing to hold still.
                        None => u64::MAX,
                    }
                } else {
                    px_gen
                };
                let owes_keyframe = owes_keyframe(client, sid);
                let unchanged = !owes_keyframe
                    && client
                        .surface_subs
                        .get(&sid)
                        .and_then(|s| s.last_encoded_gen)
                        == Some(latest_gen);

                // Adaptive bandwidth: one step per surface per tick, after
                // the pacing gate so an idle surface neither steps nor is
                // judged on a stale frame size.  A `true` return means the
                // backend cannot retarget in place and the drift now
                // justifies paying for a rebuild + keyframe.
                let step = step_adaptive_bandwidth(
                    client,
                    state.config.surface_encoding.bandwidth,
                    sid,
                    now,
                    unchanged,
                );
                if step.rebuild {
                    let sub = client.surface_subs.entry(sid).or_default();
                    sub.encoder = None;
                    if sub.encode_in_flight || sub.creation_in_flight {
                        sub.encoder_invalidated = true;
                    }
                }
                // A compositor-resident encoder takes the new rate from the
                // next frame on — no rebuild, no keyframe.  This is only
                // meaningful because sessions are owned per `(surface,
                // client)`: one viewer's backoff no longer degrades
                // everyone else's stream.
                if step.quantizer.is_some() && has_vulkan_enc {
                    // Through `resolve_bandwidth`, not the raw step: the
                    // controller floors at `ADAPTIVE_MAX_QUANTIZER`, so a
                    // ceiling set cheaper than that would otherwise be
                    // overshot into spending more bits than allowed.
                    // Mapped to the session's own QP scale — an H.264
                    // session takes 0–51, and feeding it the controller's
                    // 0–255 walk would pin it at its worst quality.
                    let bw =
                        resolve_bandwidth(client, state.config.surface_encoding.bandwidth, sid);
                    let q = match client.vulkan_video_surfaces.get(&sid) {
                        Some(&(_, flag)) if flag == SURFACE_FRAME_CODEC_H264 => bw.h264_qp(),
                        _ => bw.av1_qp_for_vulkan(),
                    };
                    pending_vulkan_qp_updates.push((sid as u32, work.cid, q));
                }

                // The picture has not changed.  Normally that means there is
                // nothing to send — but the frame the client is looking at
                // was encoded at whatever quantizer the controller had
                // backed off to, and it is about to stay on screen.  If the
                // step above bought an improvement, spend it; otherwise
                // there is nothing to gain.
                let still_refresh = unchanged && step.quantizer.is_some();
                if unchanged {
                    if !still_refresh {
                        client.skip_same_gen_count = client.skip_same_gen_count.saturating_add(1);
                        continue;
                    }
                    if has_vulkan_enc {
                        // Nothing to re-send here: the bitstream in hand is
                        // the one the client already has.  The qp update
                        // above is staged, and the keyframe request forces
                        // the recomposite that makes the compositor encode
                        // at it.  Delivery happens next tick, on the new
                        // generation.
                        pending_vulkan_keyframe_requests.push((sid as u32, work.cid));
                        continue;
                    }
                }

                // Fast path: this client owns a compositor-resident
                // encoder for this surface, so its bitstream is waiting in
                // `last_encoded` under its own client id.  Nothing here is
                // shared with any other subscriber — a second viewer has
                // its own session, its own GOP and its own quantizer.
                if client.vulkan_video_surfaces.contains_key(&sid) {
                    let encoded = sess
                        .compositor
                        .as_ref()
                        .and_then(|cs| cs.last_encoded.get(&(sid, work.cid)))
                        .map(|e| {
                            (
                                e.width,
                                e.height,
                                e.data.clone(),
                                e.is_keyframe,
                                e.codec_flag,
                                e.generation,
                                e.timestamp_ms,
                            )
                        });
                    let client = sess.clients.get_mut(&work.cid).unwrap();
                    if let Some((ew, eh, data, is_keyframe, codec_flag, frame_gen, ts)) = encoded {
                        // `last_encoded` holds only the newest frame per
                        // (surface, client), so the session's opening IDR
                        // survives there for one frame period — 16.6ms at
                        // 60fps.  A tick that arrives after it has been
                        // overwritten used to forward the P frame sitting
                        // there to a subscriber that had never received a
                        // keyframe, and never asked for another: the client
                        // then had no SPS/PPS and no recovery point, so the
                        // whole stream was undecodable until something else
                        // happened to force an IDR.  Ask for one and wait.
                        if owes_keyframe && !is_keyframe {
                            pending_vulkan_keyframe_requests.push((sid as u32, work.cid));
                            client.skip_vulkan_await_count =
                                client.skip_vulkan_await_count.saturating_add(1);
                            continue;
                        }
                        if !owes_keyframe
                            && client
                                .surface_subs
                                .get(&sid)
                                .and_then(|s| s.last_encoded_gen)
                                == Some(frame_gen)
                        {
                            client.skip_same_gen_count =
                                client.skip_same_gen_count.saturating_add(1);
                            continue;
                        }
                        if (target_w, target_h) != (ew, eh) {
                            // This client now wants a different size than
                            // the compositor encodes at.  Vulkan Video only
                            // emits at native, so drop this client's
                            // session and let its server-side encoder take
                            // over.  Every other subscriber keeps theirs.
                            if !vulkan_teardown.contains(&(sid, work.cid)) {
                                vulkan_teardown.push((sid, work.cid));
                            }
                            continue;
                        }
                        let flags = codec_flag
                            | if is_keyframe {
                                SURFACE_FRAME_FLAG_KEYFRAME
                            } else {
                                0
                            };
                        let msg = msg_surface_frame(sid, ts, flags, ew as u16, eh as u16, &data);
                        let bytes = msg.len();
                        match send_outbox(client, msg) {
                            Err(_e) => {
                                client.surface_subs.entry(sid).or_default().has_keyframe = false;
                            }
                            Ok(()) => {
                                record_surface_frame_sent(client, sid, bytes, is_keyframe, now);
                                if !is_keyframe {
                                    client.avg_surface_frame_bytes = ewma_with_direction(
                                        client.avg_surface_frame_bytes,
                                        bytes as f32,
                                        0.5,
                                        0.125,
                                    );
                                }
                                client.frames_sent = client.frames_sent.wrapping_add(1);
                                let s = client.surface_subs.entry(sid).or_default();
                                if is_keyframe {
                                    s.has_keyframe = true;
                                }
                                s.burst_remaining = s.burst_remaining.saturating_sub(1);
                            }
                        }
                        encoded_client_surfaces.insert((work.cid, sid));
                        encode_dispatched_surfaces.insert(sid);
                        client.surface_subs.entry(sid).or_default().last_encoded_gen =
                            Some(frame_gen);
                        continue;
                    }
                    // The session exists but has not produced a frame yet.
                    if owes_keyframe {
                        pending_vulkan_keyframe_requests.push((sid as u32, work.cid));
                    }
                    client.skip_vulkan_await_count =
                        client.skip_vulkan_await_count.saturating_add(1);
                    let now_inst = Instant::now();
                    if now_inst.duration_since(client.last_skip_log).as_secs_f32() > 5.0 {
                        client.last_skip_log = now_inst;
                        eprintln!(
                            "[encode-skip] cid={} sid={sid} reason=vulkan_await \
                             (compositor has not produced a bitstream yet) count={}",
                            work.cid, client.skip_vulkan_await_count,
                        );
                    }
                    continue;
                }

                let pixels: blit_compositor::PixelData = {
                    let cs = sess.compositor.as_ref().unwrap();
                    match cs.last_pixels.get(&(sid, px_w, px_h)) {
                        // A GPU-only commit carries no pixels — the
                        // compositor skipped the readback because a Vulkan
                        // Video encoder owned the surface and nothing had
                        // registered a target for CPU pixels. This client
                        // wants a server-side encoder, so its registration
                        // is what makes the compositor resume publishing
                        // BGRA; skip until that lands rather than encode an
                        // empty frame.
                        Some(lp) if matches!(lp.pixels, blit_compositor::PixelData::GpuOnly) => {
                            let client = sess.clients.get_mut(&work.cid).unwrap();
                            client.skip_last_pixels_mismatch_count =
                                client.skip_last_pixels_mismatch_count.saturating_add(1);
                            continue;
                        }
                        Some(lp) => lp.pixels.clone(),
                        None => {
                            let client = sess.clients.get_mut(&work.cid).unwrap();
                            client.skip_last_pixels_mismatch_count =
                                client.skip_last_pixels_mismatch_count.saturating_add(1);
                            continue;
                        }
                    }
                };
                let client = sess.clients.get_mut(&work.cid).unwrap();

                // Skip if an encode or creation job is already in
                // flight for this surface.  Creations also block encode
                // dispatch: the encoder is None while creation runs,
                // and we don't want to re-queue another creation until
                // the first one completes.
                if client
                    .surface_subs
                    .get(&sid)
                    .is_some_and(|s| s.encode_in_flight || s.creation_in_flight)
                {
                    client.skip_in_flight_count = client.skip_in_flight_count.saturating_add(1);
                    let now_inst = Instant::now();
                    if now_inst.duration_since(client.last_skip_log).as_secs_f32() > 5.0 {
                        client.last_skip_log = now_inst;
                        let burst = client
                            .surface_subs
                            .get(&sid)
                            .map_or(0, |s| s.burst_remaining);
                        eprintln!(
                            "[encode-skip] cid={} sid={sid} reason=in_flight same_gen={} in_flight={} burst={burst}",
                            work.cid, client.skip_same_gen_count, client.skip_in_flight_count,
                        );
                    }
                    continue;
                }

                let needs_new_encoder = if has_vulkan_enc {
                    false
                } else {
                    client
                        .surface_subs
                        .get(&sid)
                        .and_then(|s| s.encoder.as_ref())
                        .is_none_or(|e| e.source_dimensions() != (enc_w, enc_h))
                };

                // If the encoder was dropped due to persistent nal_data=None,
                // back off for a short window before retrying.  Each retry
                // allocates GBM fds, so we don't want a genuinely broken
                // encoder (GPU lost) to recreate at tick rate and exhaust
                // the process fd limit — but a warm-up burst (compositor
                // hasn't imported the freshly-allocated external output
                // buffers yet) should recover within seconds without
                // requiring a user-driven resize/resubscribe.
                const NAL_NONE_RETRY_BACKOFF: Duration = Duration::from_secs(2);
                if needs_new_encoder
                    && client
                        .surface_subs
                        .get(&sid)
                        .is_some_and(|s| s.nal_none_streak >= 10)
                {
                    let ready_to_retry = client
                        .surface_subs
                        .get(&sid)
                        .and_then(|s| s.nal_none_latched_at)
                        .is_some_and(|t| now.duration_since(t) >= NAL_NONE_RETRY_BACKOFF);
                    if ready_to_retry {
                        if let Some(s) = client.surface_subs.get_mut(&sid) {
                            s.nal_none_streak = 0;
                            s.nal_none_latched_at = None;
                        }
                    } else {
                        continue;
                    }
                }

                // --- Try Vulkan Video first ---
                if needs_new_encoder {
                    let codec_support = surface_codec_support(client, sid);
                    let encoding = SurfaceEncoding {
                        bandwidth: resolve_bandwidth(
                            client,
                            state.config.surface_encoding.bandwidth,
                            sid,
                        ),
                        speed: client
                            .surface_subs
                            .get(&sid)
                            .and_then(|s| s.speed_override)
                            .unwrap_or(state.config.surface_encoding.speed),
                    };

                    // Vulkan Video encodes at the compositor's native size
                    // — only valid when this client's per-client target
                    // matches native.  If we selected Vulkan Video here for
                    // a smaller-target client, the bitstream would be at
                    // the wrong resolution and we'd have no way to scale
                    // it.  Other subscribers are unaffected either way:
                    // each owns its own session.
                    let vulkan_eligible = (target_w, target_h) == (px_w, px_h);
                    let refused_bits = client
                        .surface_subs
                        .get(&sid)
                        .map_or(0, |s| s.vulkan_refused);

                    let mut vulkan_selected = false;
                    for &pref in &state.config.surface_encoders {
                        if !pref.is_vulkan_video() {
                            continue;
                        }
                        if !vulkan_eligible {
                            continue;
                        }
                        // Refusals are per encoder: one the compositor has
                        // already turned down is skipped, but the rest of the
                        // tier still gets its turn.
                        if refused_bits & pref.vulkan_refusal_bit() != 0 {
                            continue;
                        }
                        if !pref.supported_by_client(codec_support) {
                            continue;
                        }
                        // Check compositor capability (pre-extracted above).
                        let available = match pref {
                            SurfaceEncoderPreference::VulkanVideoH264 => vk_encode_available,
                            SurfaceEncoderPreference::VulkanVideoAV1 => vk_encode_av1_available,
                            _ => false,
                        };
                        if !available {
                            continue;
                        }
                        // Would this client actually be served 4:4:4?  Both
                        // the server's configuration and the client's own
                        // announcement have to say so.
                        let want_444 = state.config.chroma.is_444()
                            && pref.supports_444_by_client(codec_support);

                        // H.264 carries 4:4:4 as High 4:4:4 Predictive, which
                        // the compositor asks the driver for and which the
                        // 4090 supports (the Raphael iGPU does not — its caps
                        // query refuses, the session is declined, and the
                        // fallback chain takes over).  AV1 through Vulkan is
                        // still 4:2:0-only here, so it steps aside rather than
                        // silently downgrading a client that asked for 4:4:4.
                        if want_444 && pref == SurfaceEncoderPreference::VulkanVideoAV1 {
                            continue;
                        }
                        let qp = match pref {
                            SurfaceEncoderPreference::VulkanVideoAV1 => {
                                encoding.bandwidth.av1_qp_for_vulkan()
                            }
                            _ => encoding.bandwidth.h264_qp(),
                        };
                        // The name and codec string are what the client
                        // configures its decoder from, so they have to state
                        // the chroma actually being encoded — promising High
                        // 4:4:4 Predictive for a High 4:2:0 stream (or the
                        // reverse) misconfigures it.
                        let enc_name: &'static str = match (pref, want_444) {
                            (SurfaceEncoderPreference::VulkanVideoH264, true) => {
                                "h264-vulkan 4:4:4"
                            }
                            (SurfaceEncoderPreference::VulkanVideoH264, false) => "h264-vulkan",
                            (SurfaceEncoderPreference::VulkanVideoAV1, _) => "av1-vulkan",
                            _ => "vulkan",
                        };
                        // Queue commands to send after the client loop.
                        pending_vulkan_encoder_setups.push(VulkanEncoderSetup {
                            surface_id: sid as u32,
                            client_id: work.cid,
                            codec: pref.vulkan_codec(),
                            qp,
                            width: px_w,
                            height: px_h,
                            is_444: want_444,
                        });
                        pending_vulkan_keyframe_requests.push((sid as u32, work.cid));
                        if let Some(s) = client.surface_subs.get_mut(&sid) {
                            s.encoder = None;
                        }
                        client
                            .vulkan_video_surfaces
                            .insert(sid, (enc_name, pref.codec_flag()));
                        let codec_str = match pref {
                            // High 4:4:4 Predictive, else High 4:2:0.
                            SurfaceEncoderPreference::VulkanVideoH264 => if want_444 {
                                "avc1.F4001f"
                            } else {
                                "avc1.640034"
                            }
                            .to_string(),
                            SurfaceEncoderPreference::VulkanVideoAV1 => {
                                // 4:2:0 only on this path — see the skip above.
                                let profile =
                                    surface_encoder::av1_profile_digit(ChromaSubsampling::Cs420);
                                let level = surface_encoder::av1_level_for(px_w, px_h);
                                format!("av01.{profile}.{level}M.08")
                            }
                            _ => String::new(),
                        };
                        let enc_msg = msg_surface_encoder(sid, enc_name, &codec_str);
                        let _ = send_outbox(client, enc_msg);
                        if state.config.verbose {
                            eprintln!(
                                "[surface-encoder] cid={} sid={sid} {px_w}x{px_h}: using {enc_name}",
                                work.cid,
                            );
                        }
                        vulkan_selected = true;
                        break;
                    }

                    // The compositor owns this subscription's encoder now.
                    // Falling through would queue a server-side one for the
                    // same (client, surface): a second encoder that never
                    // encodes a frame, because the delivery path takes the
                    // Vulkan bitstream and skips on `skip_vulkan_await`
                    // until it arrives.  It is not a fallback either — a
                    // refusal comes back asynchronously as
                    // `VulkanEncoderUnavailable`, which latches
                    // `vulkan_refused` so a later tick retries the tier
                    // below with this encoder skipped.
                    if vulkan_selected {
                        continue;
                    }

                    // Defer encoder creation to spawn_blocking so the
                    // tick loop isn't blocked by slow VA-API init.
                    // The creation task allocates GBM buffers and
                    // returns the encoder; the first encode runs on a
                    // subsequent tick, after the main loop forwards
                    // the buffers to the compositor and the compositor
                    // commits a new frame through them.
                    {
                        let state = client.surface_subs.entry(sid).or_default();
                        state.encoder = None;
                        state.creation_in_flight = true;
                    }
                    create_jobs.push(CreateJob {
                        cid: work.cid,
                        sid,
                        target_w: enc_w,
                        target_h: enc_h,
                        native_w,
                        native_h,
                        params: EncoderCreateParams {
                            preferences: state.config.surface_encoders.clone(),
                            vaapi_device: state.config.vaapi_device.clone(),
                            encoding,
                            verbose: state.config.verbose,
                            codec_support,
                            chroma: state.config.chroma,
                        },
                    });
                    continue;
                }

                // The per-client encoder reads pixels at its
                // `source_dimensions` stride.  If the only available
                // snapshot is at native dims (e.g. the compositor
                // hasn't blitted to the freshly-registered downscale
                // target yet), feeding it would read at the wrong
                // stride and garble content (rows wrap horizontally,
                // looking like the encoded frame is letterboxed AND
                // stretched).  Skip — the next tick after the
                // compositor commits a target-sized frame will
                // pick it up.
                if (px_w, px_h) != (target_w, target_h) {
                    client.skip_last_pixels_mismatch_count =
                        client.skip_last_pixels_mismatch_count.saturating_add(1);
                    continue;
                }

                let encoder = client
                    .surface_subs
                    .get_mut(&sid)
                    .and_then(|s| s.encoder.take())
                    .unwrap();
                client.surface_subs.entry(sid).or_default().encode_in_flight = true;
                // A refresh has to be an IDR: a P-frame against an identical
                // reference codes as skip blocks and refines nothing, however
                // much finer the quantizer is.
                let needs_kf = owes_keyframe || needs_new_encoder || still_refresh;
                encoded_client_surfaces.insert((work.cid, sid));
                encode_dispatched_surfaces.insert(sid);
                encode_jobs.push(EncodeJob {
                    cid: work.cid,
                    sid,
                    target_w: enc_w,
                    target_h: enc_h,
                    pixels,
                    needs_keyframe: needs_kf,
                    encoder,
                    generation: px_gen,
                    timestamp_ms: px_timestamp_ms,
                });
            }
        }

        // Tear down Vulkan Video for surfaces where at least one client
        // wants a per-client target smaller than the compositor's native
        // size.  After this, the compositor produces raw NV12/BGRA on
        // the next frame and every subscriber's per-client encoder takes
        // over.
        for &(sid, cid) in &vulkan_teardown {
            if let Some(c) = sess.clients.get_mut(&cid)
                && c.vulkan_video_surfaces.remove(&sid).is_some()
            {
                c.surface_subs.entry(sid).or_default().has_keyframe = false;
            }
            if let Some(cs) = sess.compositor.as_mut() {
                cs.last_encoded.remove(&(sid, cid));
                let _ = cs.handle.command_tx.send(
                    blit_compositor::CompositorCommand::DestroyVulkanEncoder {
                        surface_id: sid as u32,
                        client_id: Some(cid),
                    },
                );
                cs.handle.wake();
                eprintln!(
                    "[vulkan-video] teardown sid={sid} cid={cid}: target ≠ native size; \
                     switching that client to a server-side encoder",
                );
            }
        }

        // Send Vulkan Video encoder commands to compositor.
        if (!pending_vulkan_encoder_setups.is_empty()
            || !pending_vulkan_keyframe_requests.is_empty()
            || !pending_vulkan_qp_updates.is_empty())
            && let Some(cs) = sess.compositor.as_ref()
        {
            for setup in pending_vulkan_encoder_setups {
                eprintln!(
                    "[vulkan-video] sending SetVulkanEncoder sid={} cid={} codec={} {}x{} qp={}",
                    setup.surface_id,
                    setup.client_id,
                    setup.codec,
                    setup.width,
                    setup.height,
                    setup.qp,
                );
                let _ = cs.handle.command_tx.send(
                    blit_compositor::CompositorCommand::SetVulkanEncoder {
                        surface_id: setup.surface_id,
                        client_id: setup.client_id,
                        codec: setup.codec,
                        qp: setup.qp,
                        width: setup.width,
                        height: setup.height,
                        is_444: setup.is_444,
                    },
                );
            }
            for (surface_id, client_id, qp) in pending_vulkan_qp_updates {
                let _ = cs.handle.command_tx.send(
                    blit_compositor::CompositorCommand::SetVulkanEncoderQp {
                        surface_id,
                        client_id,
                        qp,
                    },
                );
            }
            for (surface_id, client_id) in pending_vulkan_keyframe_requests {
                let _ = cs.handle.command_tx.send(
                    blit_compositor::CompositorCommand::RequestVulkanKeyframe {
                        surface_id,
                        client_id,
                    },
                );
            }
            cs.handle.wake();
        }

        // Advance per-surface pacing deadlines only for surfaces that
        // actually had an encode job collected.  Surfaces skipped due to
        // in-flight limits or unchanged pixels keep their current
        // deadline so the next tick retries without burning a time slot.
        for work in &client_work {
            if let Some(client) = sess.clients.get_mut(&work.cid) {
                for &sid in &work.subs {
                    if encoded_client_surfaces.contains(&(work.cid, sid)) {
                        // Per surface: pacing now reads that surface's own
                        // inflight depth, so one congested surface no longer
                        // sets the cadence for its neighbours.
                        let interval = surface_send_interval(client, sid);
                        let deadline = client
                            .surface_subs
                            .entry(sid)
                            .or_default()
                            .next_send_at
                            .get_or_insert(now);
                        advance_deadline(deadline, now, interval);
                    }
                }
            }
        }
    }

    if !encode_jobs.is_empty() {
        // Fire-and-forget: spawn the encode and deliver asynchronously
        // so the tick loop is never blocked by slow encoders.
        let state2 = state.clone();
        tokio::spawn(async move {
            // Track (cid, sid) for each job so we can clear the sub's
            // `encode_in_flight` flag if the blocking task panics or
            // times out (otherwise that surface is permanently blocked).
            let job_ids: Vec<(u64, u16)> = encode_jobs.iter().map(|j| (j.cid, j.sid)).collect();

            let handles: Vec<_> = encode_jobs
                .into_iter()
                .map(|job| {
                    tokio::task::spawn_blocking(move || {
                        let mut encoder = job.encoder;
                        if job.needs_keyframe {
                            encoder.request_keyframe();
                        }
                        // The compositor produces a target-sized
                        // PixelData per registered (sid, target) — either
                        // a zero-copy NV12/VA-Surface DMA-BUF (VAAPI
                        // GBM-backed) or a server-allocated BGRA blit
                        // staging buffer (NVENC, software).  Both arrive
                        // at the encoder's source dimensions, so the
                        // encoder consumes them directly with no CPU
                        // resize step.
                        let nal_data = encoder.encode_pixels(&job.pixels);
                        let codec_flag = encoder.codec_flag();
                        EncodeResult {
                            cid: job.cid,
                            sid: job.sid,
                            target_w: job.target_w,
                            target_h: job.target_h,
                            generation: job.generation,
                            encoder,
                            nal_data,
                            codec_flag,
                            timestamp_ms: job.timestamp_ms,
                        }
                    })
                })
                .collect();

            // Timeout: if a hardware encoder hangs (e.g. vaSyncSurface on
            // AMD), don't block delivery of other surfaces' results forever.
            const ENCODE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

            let mut results = Vec::with_capacity(handles.len());
            let mut failed: Vec<(u64, u16)> = Vec::new();
            for (i, h) in handles.into_iter().enumerate() {
                // Wrap the timeout in a nested tokio::spawn so that
                // panics from tokio::time::timeout during runtime
                // shutdown ("A Tokio 1.x context was found, but it is
                // being shutdown") are caught as JoinErrors instead of
                // crashing the outer task.
                let wrapper =
                    tokio::spawn(async move { tokio::time::timeout(ENCODE_TIMEOUT, h).await });
                match wrapper.await {
                    Ok(Ok(Ok(r))) => results.push(r),
                    Ok(Ok(Err(_join_err))) => {
                        // spawn_blocking panicked — encoder is lost.
                        let (cid, sid) = job_ids[i];
                        eprintln!("[surface-encoder] encode task panicked: cid={cid} sid={sid}",);
                        failed.push(job_ids[i]);
                    }
                    Ok(Err(_timeout)) => {
                        // Encoder hung (e.g. GPU hang in vaSyncSurface).
                        // The blocking thread is leaked but we must not
                        // let it stall all other surfaces forever.
                        let (cid, sid) = job_ids[i];
                        eprintln!(
                            "[surface-encoder] encode timed out ({}s): cid={cid} sid={sid}",
                            ENCODE_TIMEOUT.as_secs(),
                        );
                        failed.push(job_ids[i]);
                    }
                    Err(_join_err) => {
                        // Runtime shutting down — abandon remaining work.
                        eprintln!("[surface-encoder] runtime shutting down, aborting delivery");
                        return;
                    }
                }
            }

            // Deliver encoded frames.
            let mut sess = state2.session.lock().await;
            let now = Instant::now();
            let mut local_encodes = 0u32;
            let mut local_encode_bytes = 0u64;
            let mut local_frames_sent = 0u32;

            // Clean up in-flight tracking for panicked/timed-out encodes.
            // Without this, the surface is permanently blocked from
            // future encode jobs and frame delivery stops for it.
            for (cid, sid) in failed {
                if let Some(client) = sess.clients.get_mut(&cid) {
                    // The encoder was moved into the spawn_blocking closure
                    // and is now lost.  A fresh encoder will be created on
                    // the next tick when the sub's encoder is None.  Force
                    // a keyframe so the new encoder starts with a clean
                    // reference chain.
                    let s = client.surface_subs.entry(sid).or_default();
                    s.encode_in_flight = false;
                    s.has_keyframe = false;
                }
            }

            for result in results {
                // Return the encoder unless a resubscribe invalidated
                // it mid-encode.  Don't compare against `last_pixels`
                // here — it races with concurrent ticks.  The next
                // tick's `needs_new_encoder` check rebuilds the
                // encoder before any encode at the new size.

                if let Some(client) = sess.clients.get_mut(&result.cid) {
                    let state = client.surface_subs.entry(result.sid).or_default();
                    state.encode_in_flight = false;
                    let invalidated = std::mem::replace(&mut state.encoder_invalidated, false);
                    if !invalidated {
                        state.encoder = Some(result.encoder);
                    }
                    // Record the generation we just encoded so we don't
                    // re-encode identical pixel data on subsequent ticks.
                    state.last_encoded_gen = encoded_generation(
                        state.last_encoded_gen,
                        result.generation,
                        result.nal_data.is_some(),
                    );
                }

                let Some((nal_data, is_keyframe)) = result.nal_data else {
                    if let Some(client) = sess.clients.get_mut(&result.cid) {
                        let state = client.surface_subs.entry(result.sid).or_default();
                        state.nal_none_streak += 1;
                        let streak = state.nal_none_streak;
                        if streak == 10 {
                            state.encoder = None;
                            state.nal_none_latched_at = Some(now);
                            state.has_keyframe = false;
                            eprintln!(
                                "[encode] nal_data=None x{streak} sid={} cid={} {}x{} — dropping encoder, backing off retry",
                                result.sid, result.cid, result.target_w, result.target_h,
                            );
                        } else if streak < 10 {
                            eprintln!(
                                "[encode] nal_data=None sid={} cid={} {}x{}",
                                result.sid, result.cid, result.target_w, result.target_h,
                            );
                        }
                        // streak >= 10: suppress the log spam
                    }
                    continue;
                };
                // Encoder produced output — reset the None streak.
                if let Some(client) = sess.clients.get_mut(&result.cid)
                    && let Some(s) = client.surface_subs.get_mut(&result.sid)
                {
                    s.nal_none_streak = 0;
                }

                {
                    static EC: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
                    let n = EC.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    if n < 5 || n.is_multiple_of(1000) {
                        eprintln!(
                            "[encode #{n}] sid={} {}x{} kf={is_keyframe} bytes={}",
                            result.sid,
                            result.target_w,
                            result.target_h,
                            nal_data.len(),
                        );
                    }
                }

                local_encodes += 1;
                local_encode_bytes += nal_data.len() as u64;

                let flags = result.codec_flag
                    | if is_keyframe {
                        SURFACE_FRAME_FLAG_KEYFRAME
                    } else {
                        0
                    };
                let msg = msg_surface_frame(
                    result.sid,
                    result.timestamp_ms,
                    flags,
                    result.target_w as u16,
                    result.target_h as u16,
                    &nal_data,
                );
                let bytes = msg.len();

                let Some(client) = sess.clients.get_mut(&result.cid) else {
                    continue;
                };

                // Don't check window_open here — we already checked before
                // starting the encode job.  Dropping an encoded P-frame
                // breaks the decoder's reference chain and causes glitches.
                // With the per-sub `encode_in_flight` flag limiting to 1
                // concurrent encode per surface, at most 1 frame arrives
                // after the window closes, which is acceptable.
                match send_outbox(client, msg) {
                    Err(_e) => {
                        // Receiver dropped (client disconnected during encode).
                        // Request keyframe so the next encoder starts clean.
                        client
                            .surface_subs
                            .entry(result.sid)
                            .or_default()
                            .has_keyframe = false;
                    }
                    Ok(()) => {
                        // Track surface frames in their own inflight queue
                        // so surface ACKs feed shared goodput / RTT without
                        // polluting terminal frame-size averages or probing.
                        record_surface_frame_sent(client, result.sid, bytes, is_keyframe, now);
                        // Prefer updating avg_surface_frame_bytes from delta
                        // (non-keyframe) frames — keyframes are 5-10× larger
                        // than P-frames and would inflate the average, dragging
                        // surface_pacing_fps below the sustainable rate.
                        //
                        // However, we must still update from keyframes with a
                        // very slow alpha: all-intra encoders (e.g. AV1 VAAPI
                        // before P-frame support) only produce keyframes, so
                        // skipping them entirely leaves the average stuck at
                        // the 8 KB initial value, causing the pacer to wildly
                        // overshoot the send rate and saturate the transport.
                        if !is_keyframe {
                            client.avg_surface_frame_bytes = ewma_with_direction(
                                client.avg_surface_frame_bytes,
                                bytes as f32,
                                0.5,
                                0.125,
                            );
                        } else if client.avg_surface_frame_bytes <= 16_384.0 {
                            // First keyframe while the estimate is still at or
                            // near the initial 8 KB seed.  No P-frame data has
                            // been seen yet, so the seed is pure fiction.  Use a
                            // realistic P-frame estimate: keyframes are typically
                            // 3-8× larger than P-frames, so divide by 4.  This
                            // prevents surface_pacing_fps from being wildly
                            // optimistic (8 KB → 32 fps at 256 KB/s) when the
                            // actual frames are 50-200 KB keyframes.
                            client.avg_surface_frame_bytes = (bytes as f32 / 4.0).max(4_096.0);
                        } else {
                            // Slow convergence so one keyframe doesn't wreck
                            // the estimate for dozens of subsequent P-frames.
                            client.avg_surface_frame_bytes = ewma_with_direction(
                                client.avg_surface_frame_bytes,
                                bytes as f32,
                                0.05,
                                0.05,
                            );
                        }
                        client.frames_sent = client.frames_sent.wrapping_add(1);
                        local_frames_sent += 1;
                        let s = client.surface_subs.entry(result.sid).or_default();
                        if is_keyframe {
                            s.has_keyframe = true;
                        }
                        s.burst_remaining = s.burst_remaining.saturating_sub(1);
                    }
                }
            }
            sess.surface_encodes += local_encodes;
            sess.surface_encode_bytes += local_encode_bytes;
            sess.surface_frames_sent += local_frames_sent;
            drop(sess);
            // Wake the tick loop so it can request the next frame.
            state2.delivery_notify.notify_one();
        });
    }

    if !create_jobs.is_empty() {
        // Encoder creation runs on spawn_blocking so VA-API device open
        // and context allocation don't stall the tick loop.  When the
        // task lands, the main loop installs the encoder into the sub's
        // `encoder` slot, forwards the GBM buffers to the compositor
        // (`SetExternalOutputBuffers`), and sends S2C_SURFACE_ENCODER
        // to the client.  Encoding starts on the NEXT tick — once the
        // compositor has committed a frame through the new buffers.
        let state2 = state.clone();
        tokio::spawn(async move {
            // Track (cid, sid) for each job so we can clear
            // `creation_in_flight` if a task panics or times out.
            let job_ids: Vec<(u64, u16)> = create_jobs.iter().map(|j| (j.cid, j.sid)).collect();

            let handles: Vec<_> = create_jobs
                .into_iter()
                .map(|job| {
                    tokio::task::spawn_blocking(move || {
                        let params = job.params;
                        #[allow(unused_mut)]
                        let mut encoder = match SurfaceEncoder::new(
                            &params.preferences,
                            job.target_w,
                            job.target_h,
                            &params.vaapi_device,
                            params.encoding,
                            params.verbose,
                            params.codec_support,
                            params.chroma,
                        ) {
                            Ok(enc) => enc,
                            Err(err) => {
                                if params.verbose {
                                    eprintln!(
                                        "[surface-encoder] cid={} sid={} {}x{}: {err}",
                                        job.cid, job.sid, job.target_w, job.target_h,
                                    );
                                }
                                // Families are eliminated at 4:2:0, the chroma
                                // every attempt falls back to, so one missing
                                // there is missing outright.
                                let oversized = refused_for_size(
                                    &params.preferences,
                                    params.codec_support,
                                    job.target_w,
                                    job.target_h,
                                    |p| {
                                        !surface_encoder::known_unavailable(
                                            p,
                                            surface_encoder::ChromaSubsampling::Cs420,
                                        )
                                    },
                                );
                                return CreateResult {
                                    cid: job.cid,
                                    sid: job.sid,
                                    native_w: job.native_w,
                                    native_h: job.native_h,
                                    encoder: None,
                                    fresh: None,
                                    oversized,
                                };
                            }
                        };

                        #[cfg(target_os = "linux")]
                        let external_bufs = {
                            {
                                let drm_fd = encoder.drm_fd_raw();
                                let count = encoder.gbm_buffers().len();
                                if count > 0 {
                                    encoder.allocate_nv12_buffers(drm_fd, count);
                                }
                            }
                            let gbm_bufs = encoder.gbm_buffers();
                            if gbm_bufs.is_empty() {
                                Vec::new()
                            } else {
                                let nv12_bufs = encoder.gbm_nv12_buffers();
                                let (enc_w, enc_h) = encoder.encoder_dimensions();
                                let bufs: Result<Vec<_>, std::io::Error> = gbm_bufs
                                    .iter()
                                    .enumerate()
                                    .map(|(i, b)| {
                                        let nv12 = nv12_bufs.get(i);
                                        Ok(blit_compositor::ExternalOutputBuffer {
                                            fd: std::sync::Arc::new(b.fd.try_clone()?),
                                            fourcc: 0x34325241,
                                            modifier: 0,
                                            stride: b.stride,
                                            offset: 0,
                                            width: b.width,
                                            height: b.height,
                                            va_surface_id: 0,
                                            va_display: 0,
                                            planes: vec![blit_compositor::ExternalOutputPlane {
                                                offset: 0,
                                                pitch: b.stride,
                                            }],
                                            nv12_fd: nv12.map(|n| n.fd.clone()),
                                            nv12_stride: nv12.map_or(0, |n| n.stride),
                                            nv12_uv_offset: nv12.map_or(0, |n| n.uv_offset),
                                            nv12_modifier: nv12.map_or(0, |n| n.modifier),
                                            nv12_width: enc_w,
                                            nv12_height: enc_h,
                                        })
                                    })
                                    .collect();
                                match bufs {
                                    Ok(b) => b,
                                    Err(e) => {
                                        eprintln!("[encode] dup gbm fd failed: {e}");
                                        Vec::new()
                                    }
                                }
                            }
                        };
                        let fresh = FreshEncoder {
                            name: encoder.encoder_name(),
                            codec_string: encoder.webcodecs_codec_string(),
                            #[cfg(target_os = "linux")]
                            external_bufs,
                        };
                        CreateResult {
                            cid: job.cid,
                            sid: job.sid,
                            native_w: job.native_w,
                            native_h: job.native_h,
                            encoder: Some(encoder),
                            fresh: Some(fresh),
                            oversized: false,
                        }
                    })
                })
                .collect();

            const CREATE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);
            let mut results: Vec<CreateResult> = Vec::with_capacity(handles.len());
            let mut failed: Vec<(u64, u16)> = Vec::new();
            for (i, h) in handles.into_iter().enumerate() {
                let wrapper =
                    tokio::spawn(async move { tokio::time::timeout(CREATE_TIMEOUT, h).await });
                match wrapper.await {
                    Ok(Ok(Ok(r))) => results.push(r),
                    Ok(Ok(Err(_))) | Ok(Err(_)) => {
                        let (cid, sid) = job_ids[i];
                        eprintln!("[surface-encoder] create task failed: cid={cid} sid={sid}",);
                        failed.push(job_ids[i]);
                    }
                    Err(_) => return,
                }
            }

            let mut sess = state2.session.lock().await;
            let now = Instant::now();

            // Clear creation_in_flight for failed tasks; latch a brief
            // backoff so the next tick doesn't immediately retry.
            for (cid, sid) in failed {
                if let Some(client) = sess.clients.get_mut(&cid)
                    && let Some(s) = client.surface_subs.get_mut(&sid)
                {
                    s.creation_in_flight = false;
                    s.nal_none_streak = 10;
                    s.nal_none_latched_at = Some(now);
                }
            }

            // Surfaces whose ceiling moved as a result of these creations —
            // either a backend resolved to something other than what sizing
            // assumed, or a request was refused for size.  Re-mediated after
            // the loop, because the composite was sized against a guess: left
            // alone it renders every frame at a resolution no subscriber can
            // actually be sent.
            let mut receilinged_surfaces: Vec<u16> = Vec::new();

            for result in results {
                let Some(encoder) = result.encoder else {
                    if let Some(client) = sess.clients.get_mut(&result.cid)
                        && let Some(s) = client.surface_subs.get_mut(&result.sid)
                    {
                        s.creation_in_flight = false;
                        s.create_failures = s.create_failures.saturating_add(1);
                        // Bring the surface down to what the whole chain
                        // clears when the size is what stands in the way.
                        // Either it plainly is — nothing eligible could have
                        // carried the frame — or the backends that could have
                        // keep failing, and after enough tries a smaller
                        // picture beats none.  The counter is what separates
                        // the two from a momentary failure, which must not
                        // cost the viewer its resolution: this only clears on
                        // a resubscribe.
                        let narrow =
                            result.oversized || s.create_failures >= CREATE_FAILURES_BEFORE_DEGRADE;
                        if narrow && !s.encoder_cap_degraded {
                            // Retry at once rather than serving the backoff:
                            // the smaller size may simply work, and waiting
                            // stalls the first picture by seconds on every
                            // AV1-less host with a >4K display.
                            s.encoder_cap_degraded = true;
                            receilinged_surfaces.push(result.sid);
                        } else {
                            s.nal_none_streak = 10;
                            s.nal_none_latched_at = Some(now);
                        }
                    }
                    continue;
                };

                // Move the external buffers (and register them with the
                // compositor) BEFORE stashing the encoder, so subsequent
                // ticks see the encoder only once its buffers are live.
                let fresh = result.fresh;
                #[cfg(target_os = "linux")]
                {
                    if let Some(f) = &fresh
                        && !f.external_bufs.is_empty()
                        && let Some(cs) = sess.compositor.as_mut()
                    {
                        // Drop every cached snapshot for this surface so
                        // the next compositor frame re-fills with the
                        // newly-registered NV12 DMA-BUF target.  Stale
                        // entries (e.g. native BGRA from a previous
                        // tick) will be re-added by SurfaceCommit.
                        last_pixels_remove_for_sid(&mut cs.last_pixels, result.sid);
                    }
                }
                #[cfg(target_os = "linux")]
                let (fresh_meta, external_bufs) = match fresh {
                    Some(f) => (Some((f.name, f.codec_string)), Some(f.external_bufs)),
                    None => (None, None),
                };
                #[cfg(not(target_os = "linux"))]
                let fresh_meta = fresh.map(|f| (f.name, f.codec_string));

                #[cfg(target_os = "linux")]
                {
                    let (tw, th) = encoder.source_dimensions();
                    // Clear the previously-registered downscale target
                    // for this client/surface (if any) so stale entries
                    // don't accumulate when the per-client target dims
                    // change.  Externals replace by key in the renderer
                    // (`set_external_output_buffers`) so they don't
                    // need an explicit clear, but downscale targets do.
                    let prev_target = sess
                        .clients
                        .get(&result.cid)
                        .and_then(|c| c.surface_subs.get(&result.sid))
                        .and_then(|s| s.last_registered_target);
                    if let Some((pw, ph)) = prev_target
                        && (pw, ph) != (tw, th)
                        && let Some(cs) = sess.compositor.as_mut()
                    {
                        let _ = cs.handle.command_tx.send(
                            blit_compositor::CompositorCommand::ClearDownscaleTarget {
                                surface_id: result.sid as u32,
                                target_w: pw,
                                target_h: ph,
                            },
                        );
                        // Drop any cached snapshot at the old size so
                        // the encode loop can't pick it up after we've
                        // moved on.
                        cs.last_pixels.remove(&(result.sid, pw, ph));
                    }
                    // Whether the NV12 OPAQUE_FD shape is safe for this
                    // target, which is a property of *every* subscriber at
                    // it, not just this one. The buffer is GPU-only memory
                    // published under a single (sid, w, h) key, so one
                    // software or VA-API encoder sharing the size is enough
                    // to rule it out for all of them — it would be handed a
                    // handle it cannot map and would show black.
                    //
                    // Computed before the compositor borrow below, which
                    // takes `sess` mutably.
                    let encoder_wants_nv12_opaque = encoder.wants_nv12_opaque_fd();
                    let want_nv12_opaque = nv12_opaque_safe_for_target(
                        encoder_wants_nv12_opaque,
                        (tw, th),
                        sess.clients
                            .iter()
                            .filter(|(cid, _)| **cid != result.cid)
                            .map(|(_, c)| {
                                c.surface_subs
                                    .get(&result.sid)
                                    .map(|s| (s.last_registered_target, s.wants_nv12_opaque))
                                    .unwrap_or((None, true))
                            }),
                    );
                    if let Some(bufs) = external_bufs
                        && !bufs.is_empty()
                        && let Some(cs) = sess.compositor.as_mut()
                    {
                        let _ = cs.handle.command_tx.send(
                            blit_compositor::CompositorCommand::SetExternalOutputBuffers {
                                surface_id: result.sid as u32,
                                target_w: tw,
                                target_h: th,
                                native_w: result.native_w,
                                native_h: result.native_h,
                                buffers: bufs,
                            },
                        );
                        cs.handle.wake();
                    } else if let Some(cs) = sess.compositor.as_mut() {
                        // No GBM externals — register a server-allocated
                        // downscale target so the compositor can GPU-blit
                        // the native composite into target-sized pixels for
                        // this encoder.  Idempotent in the renderer.
                        //
                        // NVENC additionally asks for the NV12 OPAQUE_FD
                        // shape, which converts on the GPU and hands over a
                        // handle CUDA can import — skipping the readback
                        // into staging and the Vec that used to carry it.
                        // Every other backend needs pixels on the CPU and
                        // takes the BGRA path. The renderer falls back to
                        // BGRA on its own if the export fails, so this
                        // stays a request rather than a commitment, and it
                        // reconciles a `false` here by dropping an NV12
                        // target it had already built.
                        let _ = cs.handle.command_tx.send(
                            blit_compositor::CompositorCommand::RegisterDownscaleTarget {
                                surface_id: result.sid as u32,
                                target_w: tw,
                                target_h: th,
                                native_w: result.native_w,
                                native_h: result.native_h,
                                want_nv12_opaque,
                            },
                        );
                        cs.handle.wake();
                    }
                    if let Some(client) = sess.clients.get_mut(&result.cid) {
                        let s = client.surface_subs.entry(result.sid).or_default();
                        s.last_registered_target = Some((tw, th));
                        s.last_registered_native = Some((result.native_w, result.native_h));
                        // This encoder's own capability, not the resolved
                        // decision above: a later subscriber asks whether
                        // *we* could take NV12, and must not inherit a
                        // "no" we only arrived at because of a third party
                        // that has since gone away.
                        s.wants_nv12_opaque = encoder_wants_nv12_opaque;
                    }
                }
                #[cfg(not(target_os = "linux"))]
                let _ = &encoder;

                if let Some(client) = sess.clients.get_mut(&result.cid) {
                    let state = client.surface_subs.entry(result.sid).or_default();
                    state.creation_in_flight = false;
                    let invalidated = std::mem::replace(&mut state.encoder_invalidated, false);
                    if invalidated {
                        // Preferences changed mid-creation (codec /
                        // bandwidth / speed resubscribe).  Drop the encoder we just built;
                        // the next tick will dispatch a fresh creation
                        // with the new prefs.
                        continue;
                    }
                    // Sizing has been guessing which backend would win; now it
                    // knows.  A surface that came up on AV1 can grow past the
                    // H.264 ceiling, and one that came up on H.264 stops
                    // being composited as if it might not — but only after a
                    // re-mediation, so note it when the answer is new.
                    //
                    // `encoder_cap_degraded` is deliberately *not* cleared
                    // here.  It latches only when a request was refused for
                    // size, and clearing it on the smaller creation that
                    // followed would let the next winner's wider ceiling
                    // raise the surface straight back into the size that was
                    // just refused.  A resubscribe clears it; that is the
                    // point at which retrying is a fresh question.
                    let winner = Some(encoder.preference());
                    if state.selected_encoder != winner {
                        state.selected_encoder = winner;
                        receilinged_surfaces.push(result.sid);
                    }
                    state.encoder = Some(encoder);
                    state.nal_none_streak = 0;
                    state.nal_none_latched_at = None;
                    state.create_failures = 0;
                    if let Some((name, codec_string)) = fresh_meta {
                        let enc_msg = msg_surface_encoder(result.sid, name, &codec_string);
                        let _ = send_outbox(client, enc_msg);
                    }
                }
            }
            if !receilinged_surfaces.is_empty() {
                sess.resize_surfaces_to_mediated_sizes(
                    receilinged_surfaces,
                    &state2.config.surface_encoders,
                    state2.config.verbose,
                );
            }
            drop(sess);
            state2.delivery_notify.notify_one();
        });
    }

    // Request frames from the compositor for surfaces that have at least
    // one subscriber whose pacing says it can accept a new frame.  This
    // fires the surface's pending wl_surface.frame callback so the
    // Wayland client will paint and commit its next frame.
    //
    // Demand-driven with pipeline overlap:
    //   When an encode job is dispatched, we eagerly pre-request the next
    //   frame so the Wayland client paints in parallel with the encode.
    //   Fresh pixels are ready when the encode completes, turning the
    //   serial   encode + round_trip   into   max(encode, round_trip).
    {
        // Only request frames for surfaces where at least one client is
        // ready to consume the result.  Without this check, apps that are
        // always ready to paint (video players like mpv) cause a hot loop:
        // RequestFrame → commit → SurfaceCommit wakes tick → no client
        // ready → RequestFrame again → 100% CPU.
        let mut wanted: HashSet<u16> = HashSet::new();

        // Pre-request: surfaces with an encode just dispatched.  The
        // compositor will render the next frame while the encode runs,
        // so pixels are ready when the next pacing window opens.
        for &sid in &encode_dispatched_surfaces {
            wanted.insert(sid);
        }
        let mut blanket_requested = false;
        // Request frames for all known surfaces so Wayland apps can make
        // rendering progress.  Video players (mpv) need frequent callbacks
        // to advance their presentation clock; browsers need them for
        // page loads and animations.
        if let Some(cs) = sess.compositor.as_ref()
            && let Some(interval) = blanket_frame_interval(&sess)
            && now.duration_since(cs.last_blanket_frame_request) >= interval
        {
            for &sid in cs.surfaces.keys() {
                wanted.insert(sid);
            }
            blanket_requested = true;
        }
        for client in sess.clients.values() {
            // Don't gate frame requests on surface_window_open — the
            // compositor should keep producing pixels even when the
            // inflight window is closed.  Otherwise, recovery after a
            // wifi stall has to wait for the full render pipeline to
            // flush (request → paint → commit → encode) before the
            // first frame can be sent, causing a visible hang.
            if client.surface_subscriptions.is_empty() {
                continue;
            }
            for &sid in &client.surface_subscriptions {
                let (burst, deadline) = client.surface_subs.get(&sid).map_or((0, now), |s| {
                    (s.burst_remaining, s.next_send_at.unwrap_or(now))
                });
                if deadline <= now || burst > 0 {
                    wanted.insert(sid);
                } else {
                    next_deadline = Some(match next_deadline {
                        Some(existing) => existing.min(deadline),
                        None => deadline,
                    });
                }
            }
        }

        if let Some(cs) = sess.compositor.as_mut() {
            if blanket_requested {
                cs.last_blanket_frame_request = now;
            }

            // Gate: at most one RequestFrame per surface per millisecond.
            // This ensures each wl_callback.done carries a distinct
            // elapsed_ms timestamp (video players like mpv use these to
            // pace their presentation clock).  Supports up to 1 kHz.
            // The gate auto-expires: if the app doesn't commit, the next
            // tick ≥1 ms later will send a fresh request.
            const MIN_REQUEST_INTERVAL: Duration = Duration::from_millis(1);
            let mut sent_any = false;
            for sid in &wanted {
                let dominated = cs
                    .last_frame_request
                    .get(sid)
                    .is_some_and(|&t| now.duration_since(t) < MIN_REQUEST_INTERVAL);
                if !dominated {
                    cs.last_frame_request.insert(*sid, now);
                    let _ = cs
                        .handle
                        .command_tx
                        .send(CompositorCommand::RequestFrame { surface_id: *sid });
                    sent_any = true;
                }
            }
            if sent_any {
                cs.handle.wake();
            }
        }
    }

    // Yield the session lock briefly so pending encode deliveries from
    // previous ticks can acquire the lock and send their frames without
    // waiting for terminal processing to complete.  This reduces the
    // latency between encode completion and frame-on-wire.
    drop(sess);
    tokio::task::yield_now().await;
    sess = state.session.lock().await;

    let max_fps = sess
        .clients
        .values()
        .map(browser_pacing_fps)
        .fold(1.0_f32, f32::max);
    let title_interval = Duration::from_secs_f64(1.0 / max_fps as f64);
    let ids: Vec<u16> = sess.ptys.keys().copied().collect();
    let mut clipboard_msgs: Vec<Vec<u8>> = Vec::new();
    let mut title_msgs: Vec<Vec<u8>> = Vec::new();
    let mut used_rows_msgs: Vec<Vec<u8>> = Vec::new();
    for &id in &ids {
        let Some(pty) = sess.ptys.get_mut(&id) else {
            continue;
        };
        if pty.driver.take_title_dirty() {
            pty.mark_dirty();
            pty.title_pending = true;
        }
        if pty.driver.take_used_rows_dirty() {
            pty.mark_dirty();
        }
        for text in pty.driver.take_clipboard_stores() {
            clipboard_msgs.push(msg_s2c_clipboard_content(
                "text/plain;charset=utf-8",
                text.as_bytes(),
            ));
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
            title_msgs.push(msg);
        }
        let used_rows = pty.driver.used_rows();
        if used_rows != pty.last_used_rows_sent {
            pty.last_used_rows_sent = used_rows;
            used_rows_msgs.push(msg_s2c_used_rows(id, used_rows));
        }
    }
    for msg in clipboard_msgs {
        sess.send_to_all(&msg);
    }
    for msg in title_msgs {
        sess.send_to_all(&msg);
    }
    for msg in used_rows_msgs {
        sess.send_to_all(&msg);
    }

    // Drain bytes from PTY reader channels. This is the only place
    // process() is called, so there is no contention with the readers.
    //
    // End-to-end flow control, two brakes on the same chain (`byte_rx`
    // fills to its bounded capacity → the reader task's
    // `byte_tx.blocking_send` blocks → the kernel's PTY master buffer
    // fills → the child process's `write(stdout, ...)` blocks):
    //
    // 1. When at least one client is subscribed to a PTY and its
    //    `ready_frames` queue is full, stop draining that PTY.
    //    Sync-bracketed frames are never silently dropped; the producer
    //    is slowed instead.
    // 2. `PTY_PARSE_BUDGET_PER_TICK`, for output that never emits a sync
    //    boundary (so brake 1 never engages — `ready_frames` only fills
    //    on SyncBoundary) and for PTYs with no subscriber at all.
    //    Without it this loop parses a flooding PTY for as long as the
    //    reader can refill the channel, holding the session mutex the
    //    whole time.
    let ptys_with_subscribers: HashSet<u16> = sess
        .clients
        .values()
        .flat_map(|c| c.subscriptions.iter().copied())
        .collect();
    let mut eof_ptys: Vec<(u16, u64)> = Vec::with_capacity(ids.len());
    let mut cwd_msgs: Vec<Vec<u8>> = Vec::new();
    let mut parse_budget_hit = false;
    for &id in &ids {
        let Some(pty) = sess.ptys.get_mut(&id) else {
            continue;
        };
        let has_subscriber = ptys_with_subscribers.contains(&id);
        let mut budget = PTY_PARSE_BUDGET_PER_TICK;
        loop {
            if has_subscriber && pty.ready_frames.len() >= READY_FRAME_QUEUE_CAP {
                break;
            }
            if budget == 0 {
                parse_budget_hit = true;
                break;
            }
            let Ok(input) = pty.byte_rx.try_recv() else {
                break;
            };
            match input {
                PtyInput::Data(data) => {
                    budget = budget.saturating_sub(data.len());
                    let osc7 = pty::respond_to_queries(
                        &pty.handle,
                        &data,
                        pty.driver.size(),
                        pty.driver.cursor_position(),
                    );
                    if let Some(msg) = note_osc7_cwd(&mut pty.osc7_cwd, id, osc7) {
                        cwd_msgs.push(msg);
                    }
                    pty.driver.process(&data);
                    pty.mark_dirty();
                }
                PtyInput::SyncBoundary { before } => {
                    budget = budget.saturating_sub(before.len());
                    if !before.is_empty() {
                        let osc7 = pty::respond_to_queries(
                            &pty.handle,
                            &before,
                            pty.driver.size(),
                            pty.driver.cursor_position(),
                        );
                        if let Some(msg) = note_osc7_cwd(&mut pty.osc7_cwd, id, osc7) {
                            cwd_msgs.push(msg);
                        }
                        pty.driver.process(&before);
                        pty.mark_dirty();
                    }
                    if !pty.driver.synced_output() {
                        let frame = take_snapshot(pty);
                        enqueue_ready_frame(&mut pty.ready_frames, frame);
                        pty.clear_dirty();
                    }
                }
                PtyInput::Eof => {
                    eof_ptys.push((id, pty.generation));
                }
            }
        }
    }
    // Same fan-out as S2C_TITLE / S2C_USED_ROWS above: per-PTY state
    // events broadcast to every connected client, not just subscribers.
    for msg in cwd_msgs {
        sess.send_to_all(&msg);
    }
    if parse_budget_hit {
        // Leftover output is already queued, so re-tick right after this
        // round instead of waiting on the reader's notify — the permit for
        // the bytes we just budgeted away was consumed when this tick woke.
        // The tick loop releases the session mutex between rounds, and the
        // mutex is fair, so handlers that queued behind this round run
        // before the next one.
        state.delivery_notify.notify_one();
    }
    // Handle EOF outside the borrow loop.
    drop(sess);
    for (id, generation) in eof_ptys {
        tokio::time::sleep(Duration::from_millis(50)).await;
        cleanup_pty_internal(id, Some(generation), state).await;
    }
    let mut sess = state.session.lock().await;

    // Only snapshot PTYs that have at least one client ready to consume a fresh
    // frame right now. This avoids burning CPU on snapshot+diff+compress work
    // while the lead is merely waiting for its next pacing deadline.
    let needful_ptys: HashSet<u16> = sess
        .clients
        .values()
        .flat_map(|c| {
            let reserve_preview_slot = client_has_due_preview(&sess, c, now);
            c.subscriptions.iter().copied().filter(move |pid| {
                let scrolled = c.scroll_offsets.get(pid).copied().unwrap_or(0) > 0;
                if Some(*pid) == c.lead {
                    !scrolled && can_send_frame(c, now, reserve_preview_slot)
                } else {
                    !scrolled && can_send_preview(c, *pid, now)
                }
            })
        })
        .collect();

    let mut snapshots: HashMap<u16, FrameState> = HashMap::new();
    for &id in &ids {
        let Some(pty) = sess.ptys.get_mut(&id) else {
            continue;
        };
        if needful_ptys.contains(&id)
            && let Some(frame) = pty.ready_frames.pop_front()
        {
            snapshots.insert(id, frame);
            sess.tick_snaps += 1;
            continue;
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
            scrolled_ptys,
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
                c.scroll_offsets
                    .iter()
                    .map(|(&k, &v)| (k, v))
                    .collect::<Vec<_>>(),
                can_send_frame(c, now, reserve_preview_slot),
                lead_window_open(c, reserve_preview_slot),
                lead_window_open(c, reserve_preview_slot) || window_open(c),
                c.next_send_at,
            )
        };

        if subscriptions.is_empty() {
            continue;
        }

        // Send scrollback frames for any scrolled PTY.
        for &(scroll_pid, scroll_offset) in &scrolled_ptys {
            if scroll_offset == 0 {
                continue;
            }
            let is_lead = lead == Some(scroll_pid);
            let can_send = if is_lead { can_send_lead } else { true };
            if can_send {
                let prev_frame = {
                    let Some(c) = sess.clients.get(&cid) else {
                        continue;
                    };
                    c.scroll_caches
                        .get(&scroll_pid)
                        .cloned()
                        .unwrap_or_default()
                };
                let outcome = if let Some(pty) = sess.ptys.get_mut(&scroll_pid) {
                    if let Some((msg, new_frame)) =
                        build_scrollback_update(pty, scroll_pid, scroll_offset, &prev_frame)
                    {
                        let Some(c) = sess.clients.get_mut(&cid) else {
                            break;
                        };
                        let bytes = msg.len();
                        if send_outbox(c, msg).is_ok() {
                            c.scroll_caches.insert(scroll_pid, new_frame);
                            record_send(c, bytes, now, is_lead);
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
                    SendOutcome::Sent => {}
                    SendOutcome::Backpressured => {
                        if let Some(pty) = sess.ptys.get_mut(&scroll_pid) {
                            pty.mark_dirty();
                        }
                    }
                    SendOutcome::NoChange => {}
                }
            } else if is_lead && lead_has_window {
                next_deadline = Some(match next_deadline {
                    Some(existing) => existing.min(lead_deadline),
                    None => lead_deadline,
                });
            }
        }

        let lead_scroll_offset = lead
            .and_then(|pid| {
                scrolled_ptys
                    .iter()
                    .find(|&&(k, _)| k == pid)
                    .map(|&(_, v)| v)
            })
            .unwrap_or(0);

        if let Some(pid) = lead {
            if lead_scroll_offset == 0 && can_send_lead {
                if let Some(cur) = snapshots.get(&pid).cloned() {
                    let previous = sess
                        .clients
                        .get(&cid)
                        .and_then(|c| c.last_sent.get(&pid).cloned())
                        .unwrap_or_default();
                    drop(sess);
                    let msg = build_update_msg(pid, &cur, &previous);
                    sess = state.session.lock().await;
                    let Some(c) = sess.clients.get_mut(&cid) else {
                        continue;
                    };
                    match try_send_update(c, pid, cur, msg, now, true) {
                        SendOutcome::Sent => {}
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
            sess = state.session.lock().await;
            let Some(c) = sess.clients.get_mut(&cid) else {
                break;
            };
            match try_send_update(c, pid, cur, msg, now, false) {
                SendOutcome::Sent => {
                    record_preview_send(c, pid, now);
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

    // -- Audio frame delivery -----------------------------------------------
    //
    // Audio is no longer delivered from the tick loop — a dedicated
    // fan-out task (spawned in `AudioPipeline::spawn`) drains encoded
    // frames from the encoder mpsc and pushes them to each subscribed
    // client's `audio_tx` independently of compositor/video work.  This
    // keeps audio flowing at a steady 20 ms cadence even when a tick is
    // blocked by a long video write, and keeps the encoder's bounded
    // mpsc from overflowing into silent frame drops.
    //
    // Audio bytes are intentionally excluded from `goodput_window_bytes`:
    // at ~8 KB/s they're negligible next to video (MB/s) and keeping the
    // accounting on the tick loop would defeat the whole point of the
    // off-tick fan-out.  The has_listener flag is now managed by the
    // subscribe/unsubscribe API on `AudioBroadcast`.

    // -- Audio pipeline auto-restart ----------------------------------------
    // If the pipeline died (encoder crashed, PipeWire gone, capture stream dropped),
    // drop it, wait for a cooldown, and respawn.  This avoids permanent
    // audio loss that previously required a full client reconnect.
    //
    // Bitrate is pre-computed here to avoid borrowing sess.clients inside
    // the sess.compositor mutable borrow (they're the same MutexGuard).
    #[cfg(target_os = "linux")]
    let audio_restart_bitrate: i32 = sess
        .clients
        .values()
        .filter(|c| c.audio_subscribed)
        .map(|c| c.audio_bitrate_kbps)
        .max()
        .map(|kbps| kbps as i32 * 1000)
        .unwrap_or(0);
    #[cfg(target_os = "linux")]
    if let Some(ref mut cs) = sess.compositor {
        let pipeline_dead = cs.audio_pipeline.as_mut().is_some_and(|ap| !ap.is_alive());
        if pipeline_dead {
            const RESTART_COOLDOWN: Duration = Duration::from_secs(5);
            let can_restart = cs
                .last_audio_restart
                .is_none_or(|t| now.duration_since(t) >= RESTART_COOLDOWN);
            if can_restart {
                cs.last_audio_restart = Some(now);
                // Drop the dead pipeline — triggers shutdown() which kills
                // orphaned child processes and cleans up the runtime dir.
                cs.audio_pipeline = None;
                let runtime_dir = std::path::Path::new(&cs.handle.socket_name)
                    .parent()
                    .unwrap_or(std::path::Path::new("/tmp"));
                let session_id = cs.audio_session_id;
                let epoch = cs.created_at;
                let verbose = state.config.verbose;
                // Reuse the existing broadcast so currently-subscribed
                // clients pick up frames from the restarted pipeline
                // without re-subscribing.
                let broadcast = cs.audio_broadcast.clone();
                eprintln!("[audio] pipeline died, restarting...");
                let pipeline = tokio::task::block_in_place(|| {
                    audio::AudioPipeline::spawn(
                        runtime_dir,
                        session_id,
                        audio_restart_bitrate,
                        verbose,
                        epoch,
                        broadcast,
                    )
                });
                match pipeline {
                    Ok(p) => {
                        eprintln!(
                            "[audio] pipeline restarted, PULSE_SERVER={}",
                            p.pulse_server_path(),
                        );
                        cs.audio_pipeline = Some(p);
                    }
                    Err(e) => {
                        eprintln!("[audio] failed to restart pipeline: {e}");
                    }
                }
            }
        }
    }

    // Dispatch resizes whose settle window closed, and park until the next
    // one comes due.  Done last so sizes armed earlier in this same tick —
    // `receilinged_surfaces` after an encoder is created — are accounted for.
    if let Some(cs) = sess.compositor.as_mut()
        && let Some(due) = cs.flush_due_resizes(now)
    {
        next_deadline = Some(next_deadline.map_or(due, |d: Instant| d.min(due)));
    }

    // Guarantee the tick loop wakes up at least every blanket interval
    // even when other time-based work isn't pending.  When no client is
    // connected the interval is `None` and the loop sleeps purely on
    // delivery_notify, so a truly-idle server consumes ~zero CPU until
    // a client connects or the compositor emits an event.
    if let Some(interval) = blanket_frame_interval(&sess) {
        let blanket_deadline = now + interval;
        next_deadline = Some(next_deadline.map_or(blanket_deadline, |d| d.min(blanket_deadline)));
    }

    TickOutcome { next_deadline }
}

// ---------------------------------------------------------------------------
// Filesystem state sync (docs/fs-watch.md)
//
// FS_* messages are connection-scoped and never touch the session mutex:
// each sync runs a `blit-fssync` engine on its own thread, delivering
// serialized updates straight into the client's outbox channel, where the
// sender loop's S2C_FRAGMENT chunking and audio interleaving apply as for
// any bulk message.
// ---------------------------------------------------------------------------

struct FsSyncEntry {
    /// Dropping the last strong reference stops the engine, which releases
    /// its share of the root (watcher and reconciler are refcounted across
    /// syncs). The map entry is the only long-lived strong reference; the
    /// fetch queue holds `Weak`s, so removal still stops the engine.
    handle: std::sync::Arc<blit_fssync::SyncHandle>,
}

/// An `FS_FETCH` waiting for an in-flight slot; the target sync is held
/// weakly so a queued fetch never keeps a stopped engine alive.
struct QueuedFetch {
    nonce: u16,
    path: String,
    handle: std::sync::Weak<blit_fssync::SyncHandle>,
}

/// Caps `FS_FETCH`s in flight per connection, the write-family discipline
/// (docs/design/fs-watch.md `FS_FETCH`): each fetch can read + LZ4 up to
/// the 64 MiB protocol cap into the outbox. `FS_FILE` has no busy status
/// code, so over-cap requests queue (bounded) instead of erroring; a slot
/// frees when the engine's `FS_FILE` reply passes through the sync's sink.
#[derive(Default)]
struct FetchGate {
    inner: std::sync::Mutex<FetchGateInner>,
}

#[derive(Default)]
struct FetchGateInner {
    inflight: usize,
    queue: std::collections::VecDeque<QueuedFetch>,
}

/// Read a `usize` budget from the environment once.
///
/// These sit on per-message paths, so the read is cached; a value set after
/// the first request is ignored.
fn env_budget(name: &'static str, default: usize) -> usize {
    std::env::var(name)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

/// `FS_FETCH`es dispatched to engines concurrently, per connection.
///
/// Each can read and compress a file up to the decompression ceiling, so
/// this bounds real memory rather than just concurrency — left where it was.
fn fs_fetch_inflight() -> usize {
    static V: std::sync::LazyLock<usize> =
        std::sync::LazyLock::new(|| env_budget("BLIT_FS_FETCH_INFLIGHT", 8));
    *V
}

/// Fetches parked behind the in-flight cap; past this the request answers
/// `FS_FILE_OTHER` — the family's catch-all — rather than buffering
/// unboundedly.
fn fs_fetch_queue_max() -> usize {
    static V: std::sync::LazyLock<usize> =
        std::sync::LazyLock::new(|| env_budget("BLIT_FS_FETCH_QUEUE", 256));
    *V
}

/// Concurrent `FS_INDEX` / `FS_GREP` / `FS_SEARCH` walks, per connection.
///
/// Was a bare `2` at three call sites. Two concurrent greps is below what a
/// single IDE session asks for — the client carries retry-on-BUDGET code
/// specifically to absorb it — and a walk is a thread whose cost is already
/// bounded by the per-walk file, byte and entry budgets. Raising this spends
/// threads, not unbounded memory.
fn fs_walk_inflight() -> usize {
    static V: std::sync::LazyLock<usize> =
        std::sync::LazyLock::new(|| env_budget("BLIT_FS_WALK_INFLIGHT", 8));
    *V
}

/// Release one fetch slot and dispatch queued fetches while slots remain.
/// A queued fetch whose sync died answers `FS_FILE_OTHER` here, keeping
/// one reply per nonce.
fn fetch_finish(gate: &std::sync::Arc<FetchGate>, out: &mpsc::UnboundedSender<Vec<u8>>) {
    let mut inner = gate.inner.lock().unwrap();
    inner.inflight = inner.inflight.saturating_sub(1);
    while inner.inflight < fs_fetch_inflight() {
        let Some(q) = inner.queue.pop_front() else {
            break;
        };
        let dispatched = q.handle.upgrade().is_some_and(|handle| {
            handle.command(blit_fssync::Command::Fetch {
                nonce: q.nonce,
                path: q.path.clone(),
            })
        });
        if dispatched {
            inner.inflight += 1;
        } else {
            let _ = out.send(blit_remote::fs::msg_fs_file(
                q.nonce,
                blit_remote::fs::FS_FILE_OTHER,
                &[],
            ));
        }
    }
}

#[derive(Default)]
struct FsSyncs {
    map: HashMap<u16, FsSyncEntry>,
    next_id: u16,
    /// Nonces of writes/ops in flight on this connection (write-family
    /// nonce namespace). Membership dedups a duplicate nonce; the count is
    /// the in-flight cap. Freed by the engine's `InflightGuard` on reply.
    inflight_writes: std::sync::Arc<std::sync::Mutex<std::collections::HashSet<u16>>>,
    /// Nonces of `FS_INDEX` walks in flight — same discipline as
    /// `inflight_writes`, with its own small cap: an index walk can touch
    /// hundreds of thousands of entries, so a client only gets a couple at
    /// a time.
    inflight_indexes: std::sync::Arc<std::sync::Mutex<std::collections::HashSet<u16>>>,
    /// Nonces of `FS_SEARCH` walks in flight — the index-walk discipline
    /// (docs/design/fs-search.md § Budgets): same candidate walk, same
    /// cap, its own set so search and index nonce spaces stay independent.
    inflight_searches: std::sync::Arc<std::sync::Mutex<std::collections::HashSet<u16>>>,
    inflight_greps: std::sync::Arc<std::sync::Mutex<std::collections::HashSet<u16>>>,
    /// `FS_FETCH` in-flight cap and overflow queue.
    fetches: std::sync::Arc<FetchGate>,
}

impl FsSyncs {
    fn alloc_id(&mut self) -> Option<u16> {
        // Monotonic with wrap, skipping live ids and the 0xFFFF sentinel.
        for _ in 0..=u16::MAX {
            let id = self.next_id;
            self.next_id = self.next_id.wrapping_add(1);
            if id != blit_remote::fs::FS_SYNC_ID_INVALID && !self.map.contains_key(&id) {
                return Some(id);
            }
        }
        None
    }

    fn max_syncs() -> usize {
        // 128: each sync is one parked engine thread and roots are shared,
        // so the cap is headroom, not resources — an IDE session's tree
        // (one sync per expanded dir) plus editors plus dock previews sat
        // uncomfortably close to 64 (docs/design/fs-watch.md budgets).
        // Read once: this sits on the per-message path.
        static V: std::sync::LazyLock<usize> = std::sync::LazyLock::new(|| {
            std::env::var("BLIT_FS_MAX_SYNCS")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(128)
        });
        *V
    }

    fn max_write_inflight() -> usize {
        static V: std::sync::LazyLock<usize> = std::sync::LazyLock::new(|| {
            std::env::var("BLIT_FS_WRITE_INFLIGHT")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(16)
        });
        *V
    }

    /// Reserve a nonce for an in-flight write/op. `Ok(guard)` inserts it and
    /// returns a guard that frees the slot when dropped; `Err(status)` is
    /// `INVALID` for a duplicate nonce or `BUDGET` when the cap is reached.
    fn reserve_write(&self, nonce: u16) -> Result<std::sync::Arc<blit_fssync::InflightGuard>, u8> {
        use blit_remote::fs::{FS_DONE_BUDGET, FS_DONE_INVALID};
        let mut set = self.inflight_writes.lock().unwrap();
        if set.contains(&nonce) {
            return Err(FS_DONE_INVALID);
        }
        if set.len() >= Self::max_write_inflight() {
            return Err(FS_DONE_BUDGET);
        }
        set.insert(nonce);
        Ok(std::sync::Arc::new(blit_fssync::InflightGuard::new(
            self.inflight_writes.clone(),
            nonce,
        )))
    }

    /// Reserve a nonce for an in-flight `FS_INDEX` walk. Same contract as
    /// [`FsSyncs::reserve_write`]: `INVALID` for a duplicate, `BUDGET` at
    /// the cap.
    fn reserve_index(&self, nonce: u16) -> Result<std::sync::Arc<blit_fssync::InflightGuard>, u8> {
        use blit_remote::fs::{FS_DONE_BUDGET, FS_DONE_INVALID};
        let mut set = self.inflight_indexes.lock().unwrap();
        if set.contains(&nonce) {
            return Err(FS_DONE_INVALID);
        }
        if set.len() >= fs_walk_inflight() {
            return Err(FS_DONE_BUDGET);
        }
        set.insert(nonce);
        Ok(std::sync::Arc::new(blit_fssync::InflightGuard::new(
            self.inflight_indexes.clone(),
            nonce,
        )))
    }

    /// Reserve a nonce for an in-flight `FS_GREP` walk, same cap and
    /// failure split as [`FsSyncs::reserve_index`].
    fn reserve_grep(&self, nonce: u16) -> Result<std::sync::Arc<blit_fssync::InflightGuard>, u8> {
        use blit_remote::fs::{FS_DONE_BUDGET, FS_DONE_INVALID};
        let mut set = self.inflight_greps.lock().unwrap();
        if set.contains(&nonce) {
            return Err(FS_DONE_INVALID);
        }
        if set.len() >= fs_walk_inflight() {
            return Err(FS_DONE_BUDGET);
        }
        set.insert(nonce);
        Ok(std::sync::Arc::new(blit_fssync::InflightGuard::new(
            self.inflight_greps.clone(),
            nonce,
        )))
    }

    /// Reserve a nonce for an in-flight `FS_SEARCH` walk — the index-walk
    /// cap (docs/design/fs-search.md § Budgets). `FS_SEARCH.status` is the
    /// grandfathered `FS_SYNCED` table, so both a duplicate nonce and the
    /// cap answer `Err` and the caller maps it to `RESOURCE_LIMIT`.
    fn reserve_search(&self, nonce: u16) -> Option<std::sync::Arc<blit_fssync::InflightGuard>> {
        let mut set = self.inflight_searches.lock().unwrap();
        if set.contains(&nonce) || set.len() >= fs_walk_inflight() {
            return None;
        }
        set.insert(nonce);
        Some(std::sync::Arc::new(blit_fssync::InflightGuard::new(
            self.inflight_searches.clone(),
            nonce,
        )))
    }
}

// ── FS_GREP: project-wide content search (docs/design/fs-grep.md) ──────────

/// The only real bound on a grep response: the records buffer must stay
/// under the protocol's LZ4 decompression cap (`FS_MAX_DECOMPRESSED`,
/// 64 MiB), with the same headroom `FS_INDEX` leaves. There is deliberately
/// no match-count budget — a search that says "3 results" when there are
/// four is worse than a slow one, so the only thing allowed to stop it is
/// running out of wire.
const FS_GREP_MAX_RECORD_BYTES: usize = 48 * 1024 * 1024;

/// Records one file may contribute. Not a guessed number: a FILE record
/// carries its match count in a `u16` (docs/design/fs-grep.md), so past
/// this the count on the wire would wrap and a client grouping by it would
/// mis-associate every later match. It also bounds what one dense file
/// holds in memory before the byte budget below gets a look at it.
const FS_GREP_MAX_PER_FILE: usize = u16::MAX as usize;

/// Files opened per walk (docs/design/fs-grep.md "Budgets"). Sets
/// `TRUNCATED`: unlike the binary and size sniffs, a file we declined to
/// open may well have held matches.
const FS_GREP_MAX_FILES: usize = 1_000_000;

/// Per-record wire overhead used for budget accounting: length prefix,
/// kind byte and the four position fields. Deliberately a rough
/// over-estimate — the budget exists to stop before the wire cap, and
/// stopping a little early is the safe direction.
const FS_GREP_RECORD_OVERHEAD: usize = 24;

/// Largest file read. Past this a file is *out of scope* rather than
/// clipped — the same status as a binary or as `.git`, and so not a
/// truncation. Sized to hold any hand-written source file.
const FS_GREP_MAX_FILE: u64 = 64 * 1024 * 1024;

/// Bytes sniffed for NUL before deciding a file is binary.
const FS_GREP_SNIFF: usize = 8192;

/// Truncate `text` to at most `cap` bytes on a char boundary.
fn fs_grep_clip_to(text: &str, cap: usize) -> &str {
    if text.len() <= cap {
        return text;
    }
    let mut end = cap;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

/// Read a file for searching, or `None` when it is not searchable text.
///
/// Sniffs the first [`FS_GREP_SNIFF`] bytes for NUL before reading the
/// rest, so an unpruned walk meeting a tree full of build artifacts pays
/// 8 KiB per binary instead of its whole length. Bytes, not `String`: the
/// search runs on raw bytes, so there is no lossy-UTF-8 copy of every file.
fn fs_grep_read_text(abs: &std::path::Path, size: u64) -> Option<Vec<u8>> {
    use std::io::Read;
    if size > FS_GREP_MAX_FILE {
        return None;
    }
    let mut file = std::fs::File::open(abs).ok()?;
    let mut buf = vec![0u8; FS_GREP_SNIFF.min(size as usize)];
    let mut filled = 0usize;
    while filled < buf.len() {
        match file.read(&mut buf[filled..]) {
            Ok(0) => break,
            Ok(n) => filled += n,
            Err(_) => return None,
        }
    }
    buf.truncate(filled);
    if buf.contains(&0) {
        return None;
    }
    file.read_to_end(&mut buf).ok()?;
    Some(buf)
}

/// Find matches in one file's bytes — one record per *match*, not per
/// line, so a line containing the query twice yields two results and
/// clicking either lands on the one you clicked.
///
/// Searches the *whole buffer* and maps match offsets back to lines, rather
/// than running the regex once per line. That is most of the gap to `rg`:
/// the regex crate's literal prefilters (memchr, Teddy) only get to skip
/// ahead when handed a large haystack, and feeding them one short line at a
/// time throws that away along with the per-call overhead.
fn fs_grep_hits(
    re: &regex::bytes::Regex,
    bytes: &[u8],
    max_per_file: usize,
    budget: &std::sync::atomic::AtomicUsize,
) -> (Vec<blit_remote::fs::FsGrepRecord>, bool) {
    let mut hits = Vec::new();
    let mut truncated = false;
    // Record bytes produced but not yet added to the walk-wide budget.
    // Charging per match would put every walk thread on one cache line,
    // and charging once per *file* — which is what the walk used to do —
    // means a pattern matching every byte of a large file builds gigabytes
    // of records before anything notices. A chunk of slack per thread is
    // the whole cost of noticing in time.
    const PUBLISH_CHUNK: usize = 64 * 1024;
    let mut unpublished = 0usize;
    // Matches arrive in increasing offset order, so one forward cursor over
    // the newlines is enough to number them all. It stays on the match's
    // *first* line, which is where the next match's scan resumes from.
    let mut line_start = 0usize;
    let mut line = 0u32;
    for m in re.find_iter(bytes) {
        if hits.len() >= max_per_file {
            truncated = true;
            break;
        }
        // Advance to the line holding this match. Two matches on one line
        // find no newline between them, which is what gives them the same
        // line number without special-casing.
        while let Some(off) = memchr::memchr(b'\n', &bytes[line_start..m.start()]) {
            line_start += off + 1;
            line += 1;
        }
        // A pattern containing \n matches across lines. Report the range
        // the way an LSP range would, and carry *every* line it spans so
        // the client can show the whole match rather than its first line.
        let mut end_line = line;
        let mut last_line_start = line_start;
        for off in memchr::memchr_iter(b'\n', &bytes[m.start()..m.end()]) {
            end_line += 1;
            last_line_start = m.start() + off + 1;
        }
        let block_end = memchr::memchr(b'\n', &bytes[m.end()..])
            .map(|o| m.end() + o)
            .unwrap_or(bytes.len());
        // The cap is per line spanned, so a multi-line match is not clipped
        // to the budget of a single one, with an absolute ceiling so a
        // pathological pattern cannot ship a megabyte.
        let spanned = (end_line - line) as usize + 1;
        let cap = blit_remote::fs::FS_GREP_MAX_LINE
            .saturating_mul(spanned)
            .min(8192);
        let text = String::from_utf8_lossy(&bytes[line_start..block_end]);
        let text = fs_grep_clip_to(&text, cap).to_string();
        unpublished += text.len() + FS_GREP_RECORD_OVERHEAD;
        hits.push(blit_remote::fs::FsGrepRecord::Match {
            line,
            col: (m.start() - line_start) as u32,
            end_line,
            end_col: (m.end() - last_line_start) as u32,
            text,
        });
        if unpublished >= PUBLISH_CHUNK {
            let total =
                budget.fetch_add(unpublished, std::sync::atomic::Ordering::Relaxed) + unpublished;
            unpublished = 0;
            if total >= FS_GREP_MAX_RECORD_BYTES {
                truncated = true;
                break;
            }
        }
    }
    budget.fetch_add(unpublished, std::sync::atomic::Ordering::Relaxed);
    (hits, truncated)
}

/// One file's results, held until both passes are ordered.
type GrepHit = (String, Vec<blit_remote::fs::FsGrepRecord>);

/// Walk and search one pass in parallel. `skip` excludes paths an earlier
/// pass already covered. Returns per-file hits, every relative path the
/// walk yielded, and whether it stopped early.
fn fs_grep_pass(
    root: &std::path::Path,
    re: &regex::bytes::Regex,
    max_per_file: usize,
    use_ignores: bool,
    skip: Option<&std::collections::HashSet<String>>,
    budget: &std::sync::atomic::AtomicUsize,
    opened: &std::sync::atomic::AtomicUsize,
) -> (Vec<GrepHit>, Vec<String>, bool) {
    use ignore::{WalkBuilder, WalkState};
    use std::sync::atomic::Ordering;

    let hits: std::sync::Mutex<Vec<GrepHit>> = std::sync::Mutex::new(Vec::new());
    let seen: std::sync::Mutex<Vec<String>> = std::sync::Mutex::new(Vec::new());
    let stopped = std::sync::atomic::AtomicBool::new(false);

    let mut builder = WalkBuilder::new(root);
    builder
        .hidden(false)
        .follow_links(false)
        // `.git` is an object database, not source: nothing a textual query
        // can usefully match, and on a real repo it dwarfs the tree.
        .filter_entry(|e| e.file_name() != std::ffi::OsStr::new(".git"));
    if !use_ignores {
        builder.standard_filters(false);
    }

    builder.build_parallel().run(|| {
        Box::new(|entry| {
            let Ok(entry) = entry else {
                return WalkState::Continue;
            };
            if !entry.file_type().is_some_and(|ft| ft.is_file()) {
                return WalkState::Continue;
            }
            if budget.load(Ordering::Relaxed) >= FS_GREP_MAX_RECORD_BYTES
                || opened.fetch_add(1, Ordering::Relaxed) >= FS_GREP_MAX_FILES
            {
                stopped.store(true, Ordering::Relaxed);
                return WalkState::Quit;
            }
            let Ok(rel) = entry.path().strip_prefix(root) else {
                return WalkState::Continue;
            };
            if rel.as_os_str().is_empty() {
                return WalkState::Continue;
            }
            let rel = rel.to_string_lossy().into_owned();
            if skip.is_some_and(|s| s.contains(&rel)) {
                return WalkState::Continue;
            }
            if skip.is_none() {
                seen.lock().unwrap().push(rel.clone());
            }
            let size = entry.metadata().map(|m| m.len()).unwrap_or(0);
            let Some(bytes) = fs_grep_read_text(entry.path(), size) else {
                return WalkState::Continue;
            };
            // `fs_grep_hits` charges the budget as it matches, so a file
            // that matches millions of times stops partway rather than
            // after the fact.
            let (found, clipped) = fs_grep_hits(re, &bytes, max_per_file, budget);
            if clipped {
                stopped.store(true, Ordering::Relaxed);
            }
            if found.is_empty() {
                return WalkState::Continue;
            }
            budget.fetch_add(rel.len() + FS_GREP_RECORD_OVERHEAD, Ordering::Relaxed);
            hits.lock().unwrap().push((rel, found));
            WalkState::Continue
        })
    });

    (
        hits.into_inner().unwrap(),
        seen.into_inner().unwrap(),
        stopped.into_inner(),
    )
}

/// Two-phase content walk: every tracked (non-ignored) file first, then the
/// ignored ones. Ignore rules rank here rather than filter
/// (docs/design/fs-grep.md), and running the passes in order is what makes
/// the ordering fall out of the traversal instead of a sort over everything.
///
/// Each pass is `ignore`'s parallel walker, searching on the walk threads.
/// The tracked pass leaves its standard filters on, so it never descends
/// into `target/`; the ignored pass turns them off and skips what the first
/// already covered. Set membership, not a per-path ignore matcher —
/// `IncrementalIgnore` documents itself as too slow to drive a traversal,
/// and on a 56 GB tree it was the whole cost.
///
/// The bool is `truncated`, and it means exactly one thing: matches existed
/// that are not in this response. Files skipped as binary or oversized are
/// *out of scope*, not clipped, so they do not set it — otherwise any tree
/// with a `target/` in it would report every search as incomplete.
fn fs_grep_walk(
    root: &std::path::Path,
    re: &regex::bytes::Regex,
    max_matches: usize,
    max_per_file: usize,
    no_ignore: bool,
) -> (Vec<u8>, bool) {
    let budget = std::sync::atomic::AtomicUsize::new(0);
    // Per walk, not per pass: the second pass reopens nothing the first
    // covered, so the two share one allowance.
    let opened = std::sync::atomic::AtomicUsize::new(0);
    let (mut tracked, seen, trunc_a) =
        fs_grep_pass(root, re, max_per_file, true, None, &budget, &opened);
    // The ignored pass is the expensive half: it is the one that descends
    // into `target/`. Off by default, so an ordinary search costs the
    // tracked walk alone.
    let (mut ignored, trunc_b) = if no_ignore {
        let tracked_paths: std::collections::HashSet<String> = seen.into_iter().collect();
        let (hits, _, t) = fs_grep_pass(
            root,
            re,
            max_per_file,
            false,
            Some(&tracked_paths),
            &budget,
            &opened,
        );
        (hits, t)
    } else {
        (Vec::new(), false)
    };
    let mut truncated = trunc_a || trunc_b;

    tracked.sort_unstable_by(|a, b| a.0.cmp(&b.0));
    ignored.sort_unstable_by(|a, b| a.0.cmp(&b.0));

    let mut buf = Vec::new();
    let mut matched_files = 0usize;
    'outer: for (is_ignored, files) in [(false, &tracked), (true, &ignored)] {
        for (rel, found) in files {
            if max_matches != 0 && matched_files >= max_matches {
                truncated = true;
                break 'outer;
            }
            matched_files += 1;
            blit_remote::fs::append_fs_grep_record(
                &mut buf,
                &blit_remote::fs::FsGrepRecord::File {
                    flags: if is_ignored {
                        blit_remote::fs::FS_GREP_FILE_IGNORED
                    } else {
                        0
                    },
                    // Exact: FS_GREP_MAX_PER_FILE is this field's own
                    // ceiling, so the count never wraps.
                    n: found.len() as u16,
                    path: rel.clone(),
                },
            );
            for h in found {
                blit_remote::fs::append_fs_grep_record(&mut buf, h);
            }
            if buf.len() >= FS_GREP_MAX_RECORD_BYTES {
                truncated = true;
                break 'outer;
            }
        }
    }
    (buf, truncated)
}

/// Fuzzy match `needle` (already lowercased chars) against `hay`, whose
/// lowercased chars the caller supplies in `hay_lc` (a reused buffer — one
/// walk scores hundreds of thousands of candidates, so the per-candidate
/// `Vec` allocation is hoisted out); higher is better. Every needle char
/// must appear in order; contiguity, being in the basename, and a shorter
/// overall path all score higher. `None` = no match.
fn fuzzy_score(hay: &str, hay_lc: &[char], needle: &[char]) -> Option<i64> {
    if needle.is_empty() {
        return Some(-(hay_lc.len() as i64));
    }
    let base_start = match hay.rfind('/') {
        Some(i) => hay[..i].chars().count() + 1,
        None => 0,
    };
    let mut ni = 0usize;
    let mut score = 0i64;
    let mut last: Option<usize> = None;
    for (i, &hc) in hay_lc.iter().enumerate() {
        if ni >= needle.len() {
            break;
        }
        if hc == needle[ni] {
            score += 10;
            if last == Some(i.wrapping_sub(1)) {
                score += 15; // contiguous run
            }
            if i >= base_start {
                score += 8; // match lands in the basename
            }
            last = Some(i);
            ni += 1;
        }
    }
    if ni == needle.len() {
        Some(score - hay_lc.len() as i64)
    } else {
        None
    }
}

/// Entry budget for index/search walks (`BLIT_FS_INDEX_MAX`), counted over
/// files the ignore rules let through. Clamped to the protocol's
/// `FS_INDEX_MAX_COUNT` so a raised env can't emit an unparseable count.
fn fs_index_max_entries() -> usize {
    std::env::var("BLIT_FS_INDEX_MAX")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(400_000)
        .min(blit_remote::fs::FS_INDEX_MAX_COUNT)
}

/// Raw-bytes budget for an index payload, kept well under the protocol's
/// 64 MiB decompression cap (docs/protocol.md).
const FS_INDEX_MAX_BYTES: usize = 48 * 1024 * 1024;

/// Recursively list candidate file paths (root-relative, sorted) under
/// `root`, honoring gitignore rules — walking `.gitignore`d build output
/// out of search is what keeps the list small enough to ship. `.git`
/// itself is always pruned; other dotfiles are candidates. Returns the
/// list plus whether a budget truncated it.
///
/// A tree whose *filtered* walk comes back empty (a parent `.gitignore`
/// with a bare `*` — the dotfiles-repo-at-$HOME pattern — blanks every
/// non-repo subtree) falls back to an ignore-free walk: an empty index
/// would otherwise read as "no files here" and never consult the server.
fn fs_index_walk(root: &std::path::Path, max_entries: usize) -> (Vec<String>, bool) {
    let (paths, truncated) = fs_index_walk_inner(root, max_entries, true);
    if paths.is_empty() && !truncated {
        return fs_index_walk_inner(root, max_entries, false);
    }
    (paths, truncated)
}

fn fs_index_walk_inner(
    root: &std::path::Path,
    max_entries: usize,
    use_ignores: bool,
) -> (Vec<String>, bool) {
    let mut paths: Vec<String> = Vec::new();
    let mut bytes = 0usize;
    // Yielded-entry budget (directories included), so a directory-heavy
    // tree stays bounded like the pre-index walk was. Entries the ignore
    // rules suppress never surface here — that inner I/O is the one cost
    // this budget cannot see (docs/design/fs-search.md deferred list).
    let mut work = 0usize;
    let work_budget = max_entries.saturating_mul(4);
    let mut truncated = false;
    let mut builder = ignore::WalkBuilder::new(root);
    builder
        .hidden(false)
        .follow_links(false)
        .filter_entry(|e| e.file_name() != std::ffi::OsStr::new(".git"));
    if !use_ignores {
        builder.standard_filters(false);
    }
    for entry in builder.build().flatten() {
        work += 1;
        if work > work_budget {
            truncated = true;
            break;
        }
        if !entry.file_type().is_some_and(|ft| ft.is_file()) {
            continue;
        }
        let Ok(rel) = entry.path().strip_prefix(root) else {
            continue;
        };
        if rel.as_os_str().is_empty() {
            continue; // the root itself, when it is a file
        }
        // Truncation is exact: it fires only when a file would be dropped,
        // never on a trailing directory after the last counted file.
        if paths.len() >= max_entries || bytes >= FS_INDEX_MAX_BYTES {
            truncated = true;
            break;
        }
        let rel = rel.to_string_lossy().into_owned();
        bytes += 2 + rel.len();
        paths.push(rel);
    }
    paths.sort_unstable();
    (paths, truncated)
}

/// Walk `root` and return up to `limit` file paths (root-relative)
/// fuzzy-matching `query`, best first. Same candidate set as `FS_INDEX` —
/// this is the server-side fallback for clients without a local index.
fn fs_search_walk(root: &str, query: &str, limit: usize) -> Vec<String> {
    if limit == 0 {
        return Vec::new();
    }
    let needle: Vec<char> = query.chars().flat_map(|c| c.to_lowercase()).collect();
    let (paths, _) = fs_index_walk(std::path::Path::new(root), fs_index_max_entries());
    let mut hay_lc: Vec<char> = Vec::new();
    let mut scored: Vec<(i64, String)> = paths
        .into_iter()
        .filter_map(|p| {
            hay_lc.clear();
            hay_lc.extend(p.chars().flat_map(|c| c.to_lowercase()));
            fuzzy_score(&p, &hay_lc, &needle).map(|s| (s, p))
        })
        .collect();
    scored.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.len().cmp(&b.1.len())));
    scored.truncate(limit);
    scored.into_iter().map(|(_, p)| p).collect()
}

async fn handle_fs_message(
    data: &[u8],
    syncs: &mut FsSyncs,
    out: &mpsc::UnboundedSender<Vec<u8>>,
    verbose: bool,
) {
    use blit_fssync::{OpReq, WriteReq};
    use blit_remote::fs::{
        C2S_FS_ACK, C2S_FS_FETCH, C2S_FS_GREP, C2S_FS_INDEX, C2S_FS_OP, C2S_FS_SEARCH, C2S_FS_STOP,
        C2S_FS_SYNC, C2S_FS_WRITE, FS_DONE_INVALID, FS_DONE_NOT_FOUND, FS_DONE_OK, FS_DONE_OTHER,
        FS_DONE_PERMISSION, FS_DONE_WRONG_TYPE, FS_FILE_OTHER, FS_INDEX_TRUNCATED, FS_STATUS_OK,
        FS_STATUS_OTHER, FS_STATUS_RESOURCE_LIMIT, FS_SYNC_CONTENT, FS_SYNC_CROSS_FILESYSTEM,
        FS_SYNC_DOTIGNORE, FS_SYNC_EXCLUDE_GIT, FS_SYNC_FLAGS_KNOWN, FS_SYNC_GITIGNORE,
        FS_SYNC_HEADER, FS_SYNC_ID_INVALID, FS_SYNC_RECURSIVE, FS_SYNC_SINGLE, fs_sync_flags_valid,
        msg_fs_done, msg_fs_file, msg_fs_index_result, msg_fs_search_result, msg_fs_synced,
        parse_fs_index, parse_fs_op, parse_fs_search, parse_fs_write,
    };
    match data[0] {
        C2S_FS_SEARCH => {
            // Path-based fuzzy file search — no sync. Walk off-thread so the
            // connection loop never blocks on a large tree, capped like the
            // index walks it shares a candidate set with
            // (docs/design/fs-search.md § Budgets).
            if let Some((nonce, limit, root, query)) = parse_fs_search(data) {
                let Some(guard) = syncs.reserve_search(nonce) else {
                    let _ = out.send(msg_fs_search_result(nonce, FS_STATUS_RESOURCE_LIMIT, &[]));
                    return;
                };
                let out = out.clone();
                std::thread::spawn(move || {
                    let _guard = guard;
                    let paths = fs_search_walk(&root, &query, limit as usize);
                    let _ = out.send(msg_fs_search_result(nonce, FS_STATUS_OK, &paths));
                });
            }
        }
        C2S_FS_GREP => {
            // Project-wide content search (docs/design/fs-grep.md) — no
            // sync. Same off-thread shape and in-flight cap as the index
            // walk it shares a tree with.
            if let Some((nonce, flags, max_matches, max_per_file, root, query)) =
                blit_remote::fs::parse_fs_grep(data)
            {
                let reply = |status: u8, detail: &str| {
                    blit_remote::fs::msg_fs_grep_result(nonce, status, 0, detail, &[])
                };
                if flags & !blit_remote::fs::FS_GREP_FLAGS_KNOWN != 0 {
                    let _ = out.send(reply(FS_DONE_INVALID, "unknown flags"));
                    return;
                }
                // An empty pattern matches every line of every file and is
                // never what anyone meant.
                if query.is_empty() {
                    let _ = out.send(reply(FS_DONE_INVALID, "empty query"));
                    return;
                }
                // Literal mode escapes; case-insensitive is the default.
                // Same semantics as `blit terminal grep`, so the CLI and the
                // UI agree on what a query means.
                let mut pattern = if flags & blit_remote::fs::FS_GREP_REGEX != 0 {
                    query.clone()
                } else {
                    regex::escape(&query)
                };
                // Applied after escaping, so whole-word composes with
                // literal mode as well as regex mode.
                if flags & blit_remote::fs::FS_GREP_WORD != 0 {
                    pattern = format!(r"\b(?:{pattern})\b");
                }
                let re = match regex::bytes::RegexBuilder::new(&pattern)
                    .case_insensitive(flags & blit_remote::fs::FS_GREP_CASE_SENSITIVE == 0)
                    .build()
                {
                    Ok(re) => re,
                    // The engine's own message is the only useful thing to
                    // show someone mid-typing.
                    Err(err) => {
                        let _ = out.send(reply(FS_DONE_INVALID, &err.to_string()));
                        return;
                    }
                };
                let guard = match syncs.reserve_grep(nonce) {
                    Ok(guard) => guard,
                    Err(status) => {
                        let _ = out.send(reply(status, ""));
                        return;
                    }
                };
                // Zero means unlimited on both, and unlimited is the
                // default: the response is bounded by what the wire can
                // carry, not by a number someone guessed.
                let max_matches = max_matches as usize;
                let max_per_file = if max_per_file == 0 {
                    FS_GREP_MAX_PER_FILE
                } else {
                    (max_per_file as usize).min(FS_GREP_MAX_PER_FILE)
                };
                let out = out.clone();
                std::thread::spawn(move || {
                    let _guard = guard;
                    let io_status = |err: &std::io::Error| match err.kind() {
                        std::io::ErrorKind::NotFound => FS_DONE_NOT_FOUND,
                        std::io::ErrorKind::PermissionDenied => FS_DONE_PERMISSION,
                        _ => FS_DONE_OTHER,
                    };
                    let fail =
                        |status: u8| blit_remote::fs::msg_fs_grep_result(nonce, status, 0, "", &[]);
                    let msg = match std::fs::canonicalize(&root) {
                        Ok(canon) if !canon.is_dir() => fail(FS_DONE_WRONG_TYPE),
                        // Readability probe, as FS_INDEX does: canonicalize
                        // succeeds on a mode-000 dir and the walker swallows
                        // the EACCES, which would read as "no matches here".
                        Ok(canon) => match std::fs::read_dir(&canon) {
                            Err(err) => fail(io_status(&err)),
                            Ok(_) => {
                                let (records, truncated) = fs_grep_walk(
                                    &canon,
                                    &re,
                                    max_matches,
                                    max_per_file,
                                    flags & blit_remote::fs::FS_GREP_NO_IGNORE != 0,
                                );
                                let rflags = if truncated {
                                    blit_remote::fs::FS_GREP_TRUNCATED
                                } else {
                                    0
                                };
                                blit_remote::fs::msg_fs_grep_result(
                                    nonce, FS_DONE_OK, rflags, "", &records,
                                )
                            }
                        },
                        Err(err) => fail(io_status(&err)),
                    };
                    let _ = out.send(msg);
                });
            }
        }
        C2S_FS_INDEX => {
            // Candidate list for client-side fuzzy search
            // (docs/design/fs-search.md) — no sync. Walk off-thread, capped
            // by `reserve_index` so a client can't stack up walks.
            if let Some((nonce, flags, root)) = parse_fs_index(data) {
                if flags != 0 {
                    let _ = out.send(msg_fs_index_result(nonce, FS_DONE_INVALID, 0, &[]));
                    return;
                }
                let guard = match syncs.reserve_index(nonce) {
                    Ok(guard) => guard,
                    Err(status) => {
                        let _ = out.send(msg_fs_index_result(nonce, status, 0, &[]));
                        return;
                    }
                };
                let out = out.clone();
                std::thread::spawn(move || {
                    let _guard = guard;
                    let io_status = |err: &std::io::Error| match err.kind() {
                        std::io::ErrorKind::NotFound => FS_DONE_NOT_FOUND,
                        std::io::ErrorKind::PermissionDenied => FS_DONE_PERMISSION,
                        _ => FS_DONE_OTHER,
                    };
                    let msg = match std::fs::canonicalize(&root) {
                        Ok(canon) if !canon.is_dir() => {
                            msg_fs_index_result(nonce, FS_DONE_WRONG_TYPE, 0, &[])
                        }
                        // Probe readability: canonicalize succeeds on a
                        // mode-000 dir (it only needs parent search perms)
                        // and the walker swallows the EACCES, which would
                        // read as an authoritative "no files here".
                        Ok(canon) => match std::fs::read_dir(&canon) {
                            Err(err) => msg_fs_index_result(nonce, io_status(&err), 0, &[]),
                            Ok(_) => {
                                let (paths, truncated) =
                                    fs_index_walk(&canon, fs_index_max_entries());
                                let flags = if truncated { FS_INDEX_TRUNCATED } else { 0 };
                                msg_fs_index_result(nonce, FS_DONE_OK, flags, &paths)
                            }
                        },
                        Err(err) => msg_fs_index_result(nonce, io_status(&err), 0, &[]),
                    };
                    let _ = out.send(msg);
                });
            }
        }
        C2S_FS_SYNC if data.len() >= FS_SYNC_HEADER => {
            let nonce = u16::from_le_bytes([data[1], data[2]]);
            let flags = u16::from_le_bytes([data[3], data[4]]);
            let latency_ms = u16::from_le_bytes([data[5], data[6]]);
            let inline_max = u32::from_le_bytes([data[7], data[8], data[9], data[10]]);
            let path_len = u16::from_le_bytes([data[11], data[12]]) as usize;
            let refuse = |status: u8, detail: &str| {
                let _ = out.send(msg_fs_synced(nonce, FS_SYNC_ID_INVALID, status, detail));
            };
            let Some(path_bytes) = data.get(FS_SYNC_HEADER..FS_SYNC_HEADER + path_len) else {
                refuse(FS_STATUS_OTHER, "truncated request");
                return;
            };
            let Ok(path) = std::str::from_utf8(path_bytes) else {
                refuse(FS_STATUS_OTHER, "path is not UTF-8");
                return;
            };
            if flags & !FS_SYNC_FLAGS_KNOWN != 0 {
                refuse(FS_STATUS_OTHER, "unknown flags");
                return;
            }
            if !fs_sync_flags_valid(flags) {
                refuse(
                    FS_STATUS_OTHER,
                    "single sync cannot be recursive or exclude anything",
                );
                return;
            }
            // Exclusion (docs/design/fs-watch.md "Ignoring"): the client's
            // patterns are a virtual `.gitignore` at the root, and the
            // flags add the built-in sources. Together they form the
            // shared root's identity, so a malformed list has to be
            // refused here rather than silently narrowed.
            let Some(exclude) = blit_remote::fs::fs_sync_exclude(data) else {
                refuse(FS_STATUS_OTHER, "malformed exclude patterns");
                return;
            };
            let patterns = blit_fssync::IgnoreSpec::parse_patterns(exclude);
            if patterns.len() > blit_fssync::MAX_IGNORE_PATTERNS {
                refuse(FS_STATUS_OTHER, "too many exclude patterns");
                return;
            }
            let ignores = blit_fssync::IgnoreSpec {
                gitignore: flags & FS_SYNC_GITIGNORE != 0,
                dot_ignore: flags & FS_SYNC_DOTIGNORE != 0,
                exclude_git: flags & FS_SYNC_EXCLUDE_GIT != 0,
                patterns,
            };
            // Reap entries whose engine exited on its own (root gone,
            // resource limit, backend failure): the client got their
            // FS_CLOSED but never sent FS_STOP, so their slots would
            // otherwise leak against the budget until disconnect.
            syncs.map.retain(|_, entry| !entry.handle.is_done());
            if syncs.map.len() >= FsSyncs::max_syncs() {
                refuse(FS_STATUS_RESOURCE_LIMIT, "sync limit reached");
                return;
            }
            let recursive = flags & FS_SYNC_RECURSIVE != 0;
            let cross_filesystem = flags & FS_SYNC_CROSS_FILESYSTEM != 0;
            let single = flags & FS_SYNC_SINGLE != 0;
            // Canonicalize and join (or create) the shared root — one
            // native watcher and one canonical index per root, shared
            // across every sync of it — off the runtime: arming a
            // recursive watcher walks the tree on Linux, seconds on a big
            // root. The read loop awaits, so this connection's messages
            // stay strictly ordered (FS_SYNCED before the first
            // FS_UPDATE, and no later FS_* can observe a half-open sync);
            // only the worker thread is freed for other connections.
            let path_owned = path.to_string();
            let opened = tokio::task::spawn_blocking(move || {
                if single {
                    let root = blit_fssync::validate_single_root(&path_owned)?;
                    let shared = blit_fssync::open_single_root(root.clone())?;
                    Ok((root, shared))
                } else {
                    let root = blit_fssync::validate_root(&path_owned)?;
                    let shared = blit_fssync::open_root(blit_fssync::RootKey {
                        path: root.clone(),
                        recursive,
                        cross_filesystem,
                        ignores,
                    })?;
                    Ok((root, shared))
                }
            })
            .await
            .unwrap_or_else(|_| Err((FS_STATUS_OTHER, "open task failed".to_string())));
            let (root, shared) = match opened {
                Ok(opened) => opened,
                Err((status, detail)) => {
                    refuse(status, &detail);
                    return;
                }
            };
            let Some(sync_id) = syncs.alloc_id() else {
                refuse(FS_STATUS_RESOURCE_LIMIT, "no sync ids left");
                return;
            };
            let mut opts = blit_fssync::SyncOptions {
                recursive,
                content: flags & FS_SYNC_CONTENT != 0,
                cross_filesystem,
                ..Default::default()
            };
            if latency_ms != 0 {
                opts.latency = Duration::from_millis(u64::from(latency_ms).clamp(1, 1000));
            }
            if inline_max != 0 {
                // Never above the protocol's decompressed cap: an inlined file
                // rides an FS_UPDATE a client refuses past FS_MAX_DECOMPRESSED.
                opts.inline_max =
                    u64::from(inline_max).min(blit_remote::fs::FS_MAX_DECOMPRESSED as u64);
            }
            if verbose {
                eprintln!(
                    "C2S_FS_SYNC: sync_id={sync_id} root={} recursive={recursive} content={}",
                    root.display(),
                    opts.content
                );
            }
            // FS_SYNCED must precede the first FS_UPDATE on the wire; the
            // outbox is FIFO, so sending before the engine spawns suffices.
            let _ = out.send(msg_fs_synced(
                nonce,
                sync_id,
                FS_STATUS_OK,
                &blit_fssync::escape_path(&root),
            ));
            // The sink watches for FS_FILE replies to free fetch slots
            // (and dispatch queued fetches): the engine is the only
            // producer of them, so every reply pairs with one dispatched
            // Command::Fetch.
            let engine_out = out.clone();
            let gate = syncs.fetches.clone();
            let gate_out = out.clone();
            let handle = blit_fssync::start_sync(
                &shared,
                sync_id,
                opts,
                Box::new(move |msg| {
                    let is_file_reply = msg.first() == Some(&blit_remote::fs::S2C_FS_FILE);
                    let sent = engine_out.send(msg).is_ok();
                    if is_file_reply {
                        fetch_finish(&gate, &gate_out);
                    }
                    sent
                }),
            );
            syncs.map.insert(
                sync_id,
                FsSyncEntry {
                    handle: std::sync::Arc::new(handle),
                },
            );
        }
        C2S_FS_STOP if data.len() >= 3 => {
            let sync_id = u16::from_le_bytes([data[1], data[2]]);
            // The Stop command is queued before the entry (and with it the
            // channel senders) drops, so the engine still sees it and
            // answers FS_CLOSED(client request). Unknown ids are a no-op.
            if let Some(entry) = syncs.map.remove(&sync_id) {
                entry.handle.command(blit_fssync::Command::Stop);
            }
        }
        C2S_FS_ACK if data.len() >= 7 => {
            let sync_id = u16::from_le_bytes([data[1], data[2]]);
            let update_id = u32::from_le_bytes([data[3], data[4], data[5], data[6]]);
            if let Some(entry) = syncs.map.get(&sync_id) {
                entry.handle.command(blit_fssync::Command::Ack(update_id));
            }
        }
        C2S_FS_FETCH if data.len() >= 7 => {
            let nonce = u16::from_le_bytes([data[1], data[2]]);
            let sync_id = u16::from_le_bytes([data[3], data[4]]);
            let path_len = u16::from_le_bytes([data[5], data[6]]) as usize;
            let path = data
                .get(7..7 + path_len)
                .and_then(|b| std::str::from_utf8(b).ok());
            match (path, syncs.map.get(&sync_id)) {
                (Some(path), Some(entry)) => {
                    // In-flight cap (write-family discipline): dispatch when
                    // a slot is free, queue (bounded) otherwise — FS_FILE has
                    // no busy status, so over-cap must not error. Queued
                    // fetches dispatch as replies free slots (`fetch_finish`
                    // in the sync sink), and a queued fetch whose sync died
                    // is answered there, keeping one reply per nonce.
                    let mut gate = syncs.fetches.inner.lock().unwrap();
                    if gate.inflight < fs_fetch_inflight() {
                        gate.inflight += 1;
                        drop(gate);
                        // A false return means the engine already exited on
                        // its own (root gone, backend failure) but the entry
                        // has not been reaped yet: answer here so the nonce
                        // still gets its one FS_FILE reply.
                        let sent = entry.handle.command(blit_fssync::Command::Fetch {
                            nonce,
                            path: path.to_string(),
                        });
                        if !sent {
                            let _ = out.send(msg_fs_file(nonce, FS_FILE_OTHER, &[]));
                            fetch_finish(&syncs.fetches, out);
                        }
                    } else if gate.queue.len() >= fs_fetch_queue_max() {
                        drop(gate);
                        let _ = out.send(msg_fs_file(nonce, FS_FILE_OTHER, &[]));
                    } else {
                        gate.queue.push_back(QueuedFetch {
                            nonce,
                            path: path.to_string(),
                            handle: std::sync::Arc::downgrade(&entry.handle),
                        });
                    }
                }
                _ => {
                    let _ = out.send(msg_fs_file(nonce, FS_FILE_OTHER, &[]));
                }
            }
        }
        C2S_FS_WRITE => {
            // The engine replies one FS_DONE; a malformed request, an unknown
            // sync, a duplicate nonce, or an over-cap request answers here
            // (docs/design/fs-write.md).
            match parse_fs_write(data) {
                Some(w) => {
                    if !syncs.map.contains_key(&w.sync_id) {
                        let _ = out.send(msg_fs_done(w.nonce, FS_DONE_INVALID, 0, 0));
                        return;
                    }
                    let inflight = match syncs.reserve_write(w.nonce) {
                        Ok(guard) => guard,
                        Err(status) => {
                            let _ = out.send(msg_fs_done(w.nonce, status, 0, 0));
                            return;
                        }
                    };
                    if let Some(entry) = syncs.map.get(&w.sync_id) {
                        // A false return means the engine exited on its own
                        // but was not reaped yet; answer the nonce here (the
                        // dropped WriteReq releases its in-flight slot).
                        let sent = entry.handle.command(blit_fssync::Command::Write(WriteReq {
                            nonce: w.nonce,
                            path: w.path,
                            base: w.base,
                            mode: w.mode,
                            flags: w.flags,
                            content_kind: w.content_kind,
                            content: w.content,
                            inflight: Some(inflight),
                        }));
                        if !sent {
                            let _ = out.send(msg_fs_done(w.nonce, FS_DONE_INVALID, 0, 0));
                        }
                    }
                }
                None => {
                    // Recover the nonce for a best-effort reply if we can.
                    let nonce = data
                        .get(1..3)
                        .map(|b| u16::from_le_bytes([b[0], b[1]]))
                        .unwrap_or(0);
                    let _ = out.send(msg_fs_done(nonce, FS_DONE_INVALID, 0, 0));
                }
            }
        }
        C2S_FS_OP => match parse_fs_op(data) {
            Some(o) => {
                if !syncs.map.contains_key(&o.sync_id) {
                    let _ = out.send(msg_fs_done(o.nonce, FS_DONE_INVALID, 0, 0));
                    return;
                }
                let inflight = match syncs.reserve_write(o.nonce) {
                    Ok(guard) => guard,
                    Err(status) => {
                        let _ = out.send(msg_fs_done(o.nonce, status, 0, 0));
                        return;
                    }
                };
                if let Some(entry) = syncs.map.get(&o.sync_id) {
                    // A false return means the engine exited on its own but
                    // was not reaped yet; answer the nonce here (the dropped
                    // OpReq releases its in-flight slot).
                    let sent = entry.handle.command(blit_fssync::Command::Op(OpReq {
                        nonce: o.nonce,
                        op: o.op,
                        a: o.a,
                        b: o.b,
                        base: o.base,
                        mode: o.mode,
                        flags: o.flags,
                        inflight: Some(inflight),
                    }));
                    if !sent {
                        let _ = out.send(msg_fs_done(o.nonce, FS_DONE_INVALID, 0, 0));
                    }
                }
            }
            None => {
                let nonce = data
                    .get(1..3)
                    .map(|b| u16::from_le_bytes([b[0], b[1]]))
                    .unwrap_or(0);
                let _ = out.send(msg_fs_done(nonce, FS_DONE_INVALID, 0, 0));
            }
        },
        // A frame too short to clear the length guards above still carries a
        // nonce for these read opcodes: recover it and reply, so the request
        // is not left hanging (mirrors the FS_WRITE/FS_OP recovery).
        C2S_FS_SYNC => {
            let nonce = data
                .get(1..3)
                .map(|b| u16::from_le_bytes([b[0], b[1]]))
                .unwrap_or(0);
            let _ = out.send(msg_fs_synced(
                nonce,
                FS_SYNC_ID_INVALID,
                FS_STATUS_OTHER,
                "truncated request",
            ));
        }
        C2S_FS_FETCH => {
            let nonce = data
                .get(1..3)
                .map(|b| u16::from_le_bytes([b[0], b[1]]))
                .unwrap_or(0);
            let _ = out.send(msg_fs_file(nonce, FS_FILE_OTHER, &[]));
        }
        _ => {}
    }
}

// ---------------------------------------------------------------------------
// Git introspection (docs/git.md)
//
// GIT_* messages are connection-scoped and never touch the session mutex:
// each opened repo gets a `blit-git` state engine on its own thread, and
// object-read requests run on short-lived threads, both delivering wire
// messages straight into the client's outbox channel.
// ---------------------------------------------------------------------------

struct GitRepoEntry {
    handle: blit_git::RepoHandle,
    /// The state/log-watch engine, started on open (with a watch flag) or
    /// lazily on the first `GIT_LOG_WATCH`. Dropping it stops the engine.
    state: Option<blit_git::StateHandle>,
}

#[derive(Default)]
struct GitRepos {
    map: HashMap<u16, GitRepoEntry>,
    next_id: u16,
    /// In-flight request cancel flags by nonce (per-connection namespace).
    cancels: std::sync::Arc<std::sync::Mutex<HashMap<u16, blit_git::Cancel>>>,
}

impl GitRepos {
    fn alloc_id(&mut self) -> Option<u16> {
        for _ in 0..=u16::MAX {
            let id = self.next_id;
            self.next_id = self.next_id.wrapping_add(1);
            if id != blit_remote::git::GIT_REPO_ID_INVALID && !self.map.contains_key(&id) {
                return Some(id);
            }
        }
        None
    }

    fn max_repos() -> usize {
        // Read once: this sits on the per-message path.
        static V: std::sync::LazyLock<usize> = std::sync::LazyLock::new(|| {
            std::env::var("BLIT_GIT_MAX_REPOS")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(16)
        });
        *V
    }

    fn max_inflight() -> usize {
        static V: std::sync::LazyLock<usize> = std::sync::LazyLock::new(|| {
            std::env::var("BLIT_GIT_MAX_INFLIGHT")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(16)
        });
        *V
    }

    /// True while `nonce` is already in flight (docs/git.md: a duplicate is
    /// answered `INVALID` without executing).
    fn nonce_in_flight(&self, nonce: u16) -> bool {
        self.cancels.lock().unwrap().contains_key(&nonce)
    }

    /// Register a request nonce. `Err(status)` gives the status to answer:
    /// `INVALID` for a duplicate nonce, `BUDGET` when too many requests are
    /// already in flight (bounds live request threads per connection).
    fn begin(&self, nonce: u16) -> Result<blit_git::Cancel, u8> {
        let mut cancels = self.cancels.lock().unwrap();
        if cancels.contains_key(&nonce) {
            return Err(blit_remote::git::GIT_STATUS_INVALID);
        }
        if cancels.len() >= Self::max_inflight() {
            return Err(blit_remote::git::GIT_STATUS_BUDGET);
        }
        let cancel = blit_git::Cancel::default();
        cancels.insert(nonce, cancel.clone());
        Ok(cancel)
    }
}

impl Drop for GitRepos {
    fn drop(&mut self) {
        // Connection teardown: flip every in-flight request's cancel so a
        // disconnected client's walks stop at their next cancellation
        // point instead of running to completion against a dead outbox.
        for cancel in self.cancels.lock().unwrap().values() {
            cancel.cancel();
        }
    }
}

/// Run one request on the blocking pool (bounded per connection by
/// [`GitRepos::max_inflight`]); the nonce unregisters on completion.
/// `refuse` builds the response for a rejected (duplicate/at-capacity)
/// request from the status code.
fn git_request(
    repos: &GitRepos,
    nonce: u16,
    out: &mpsc::UnboundedSender<Vec<u8>>,
    refuse: impl FnOnce(u8) -> Vec<u8>,
    run: impl FnOnce(blit_git::Cancel) -> Vec<u8> + Send + 'static,
) {
    let cancel = match repos.begin(nonce) {
        Ok(cancel) => cancel,
        Err(status) => {
            let _ = out.send(refuse(status));
            return;
        }
    };
    let cancels = repos.cancels.clone();
    let out = out.clone();
    tokio::task::spawn_blocking(move || {
        let msg = run(cancel);
        cancels.lock().unwrap().remove(&nonce);
        let _ = out.send(msg);
    });
}

/// The nonce of a nonce-bearing git request (opcode then `[nonce:2]`).
fn git_nonce(data: &[u8]) -> Option<u16> {
    (data.len() >= 3).then(|| u16::from_le_bytes([data[1], data[2]]))
}

/// Refusal detail for a `FROM_PTY` fs/git/lsp open whose source pty has no
/// resolvable cwd — the pty is unknown, or its process has exited.
const NO_SOURCE_CWD: &str = "source terminal has no working directory";

async fn handle_git_message(
    data: &[u8],
    repos: &mut GitRepos,
    out: &mpsc::UnboundedSender<Vec<u8>>,
    verbose: bool,
) {
    use blit_remote::git::*;
    match data[0] {
        C2S_GIT_OPEN => {
            let Some(open_req) = parse_git_open(data) else {
                // A well-formed nonce with a truncated/non-UTF-8 trailing
                // field still gets its one reply (as the git read handlers do
                // via git_nonce), so the client promise resolves.
                if let Some(nonce) = git_nonce(data) {
                    let _ = out.send(msg_git_repo(
                        nonce,
                        GIT_REPO_ID_INVALID,
                        GIT_STATUS_INVALID,
                        0,
                        0,
                        "malformed request",
                        "",
                    ));
                }
                return;
            };
            let GitOpenRequest {
                nonce,
                flags,
                refs_latency_ms: refs_ms,
                status_latency_ms: status_ms,
                parent_repo_id,
                path,
                ..
            } = open_req;
            let ref_prefixes: Vec<String> = open_req
                .ref_prefixes
                .iter()
                .map(|p| (*p).to_string())
                .collect();
            let refuse = |status: u8, detail: &str| {
                let _ = out.send(msg_git_repo(
                    nonce,
                    GIT_REPO_ID_INVALID,
                    status,
                    0,
                    0,
                    detail,
                    "",
                ));
            };
            // GIT_OPEN is nonce-bearing too: a nonce already in flight is a
            // duplicate (docs/git.md), answered INVALID without executing.
            if repos.nonce_in_flight(nonce) {
                refuse(GIT_STATUS_INVALID, "duplicate nonce");
                return;
            }
            if flags & !GIT_OPEN_KNOWN != 0 {
                refuse(GIT_STATUS_INVALID, "unknown flags");
                return;
            }
            // Two ways of saying where `path` starts from, and they disagree:
            // a parent makes it relative to that parent's worktree, a source
            // pty makes it relative to the pty's cwd — which the rebase above
            // has already turned into an absolute path, so a submodule open
            // would fail deep inside as INVALID instead of here.
            if parent_repo_id != GIT_OPEN_NO_CONTEXT && open_req.src_pty_id != GIT_OPEN_NO_CONTEXT {
                refuse(
                    GIT_STATUS_INVALID,
                    "parent_repo_id and src_pty_id are mutually exclusive",
                );
                return;
            }
            if repos.map.len() >= GitRepos::max_repos() {
                refuse(GIT_STATUS_BUDGET, "repo limit reached");
                return;
            }
            // Repository discovery + open touch the filesystem; run them
            // off the runtime. The read loop awaits, so this connection's
            // messages stay ordered (GIT_REPO before the first GIT_STATE,
            // and no later GIT_* can observe a half-open repo).
            let path_owned = path.to_string();
            // A parent id makes `path` a submodule path: the server
            // resolves that submodule's own gitdir rather than making the
            // client guess where .gitmodules put its worktree.
            let parent = if parent_repo_id == GIT_OPEN_NO_CONTEXT {
                None
            } else {
                match repos.map.get(&parent_repo_id) {
                    Some(entry) => Some(entry.handle.clone()),
                    None => {
                        refuse(GIT_STATUS_UNKNOWN_ID, "unknown parent repo");
                        return;
                    }
                }
            };
            let opened = tokio::task::spawn_blocking(move || match parent {
                Some(parent) => blit_git::open_submodule(&parent, &path_owned),
                None => blit_git::open(&path_owned),
            })
            .await
            .unwrap_or_else(|_| Err((GIT_STATUS_OTHER, "open task failed".to_string())));
            let (handle, info) = match opened {
                Ok(opened) => opened,
                Err((status, detail)) => {
                    refuse(status, &detail);
                    return;
                }
            };
            let Some(repo_id) = repos.alloc_id() else {
                refuse(GIT_STATUS_BUDGET, "no repo ids left");
                return;
            };
            if verbose {
                eprintln!("C2S_GIT_OPEN: repo_id={repo_id} path={path} flags={flags:#x}");
            }
            // GIT_REPO must precede the first GIT_STATE; the outbox is
            // FIFO, so sending before the engine spawns suffices.
            let _ = out.send(msg_git_repo(
                nonce,
                repo_id,
                GIT_STATUS_OK,
                info.oid_format,
                info.flags,
                &info.workdir,
                &info.gitdir,
            ));
            // STATUS and TRACKING imply WATCH; IGNORED implies UNTRACKED
            // implies STATUS (docs/git.md).
            let status = flags & (GIT_OPEN_STATUS | GIT_OPEN_UNTRACKED | GIT_OPEN_IGNORED) != 0;
            let watch = flags & GIT_OPEN_WATCH != 0 || status || flags & GIT_OPEN_TRACKING != 0;
            let state = watch.then(|| {
                let mut opts = blit_git::StateOptions {
                    status,
                    untracked: flags & (GIT_OPEN_UNTRACKED | GIT_OPEN_IGNORED) != 0,
                    ignored: flags & GIT_OPEN_IGNORED != 0,
                    tracking: flags & GIT_OPEN_TRACKING != 0,
                    remotes: flags & GIT_OPEN_REMOTES != 0,
                    ref_prefixes,
                    ..Default::default()
                };
                if refs_ms != 0 {
                    opts.refs_latency = Duration::from_millis(u64::from(refs_ms).clamp(1, 1000));
                }
                if status_ms != 0 {
                    opts.status_latency =
                        Duration::from_millis(u64::from(status_ms).clamp(1, 10_000));
                }
                let engine_out = out.clone();
                handle.start_state(
                    repo_id,
                    opts,
                    Box::new(move |msg| engine_out.send(msg).is_ok()),
                )
            });
            repos.map.insert(repo_id, GitRepoEntry { handle, state });
        }
        C2S_GIT_CLOSE => {
            let Some(repo_id) = parse_git_close(data) else {
                return;
            };
            if repos.map.remove(&repo_id).is_some() {
                let _ = out.send(msg_git_closed(repo_id, GIT_CLOSED_CLIENT_REQUEST));
            }
        }
        C2S_GIT_ACK => {
            let Some((repo_id, state_id)) = parse_git_ack(data) else {
                return;
            };
            if let Some(entry) = repos.map.get(&repo_id)
                && let Some(state) = &entry.state
            {
                state.ack(state_id);
            }
        }
        C2S_GIT_CANCEL => {
            let Some(nonce) = parse_git_cancel(data) else {
                return;
            };
            if let Some(cancel) = repos.cancels.lock().unwrap().get(&nonce) {
                cancel.cancel();
            }
        }
        C2S_GIT_LOG => {
            let Some(req) = parse_git_log(data) else {
                if let Some(n) = git_nonce(data) {
                    let _ = out.send(msg_git_commits(n, GIT_STATUS_INVALID, 0, &[], &[]));
                }
                return;
            };
            let (nonce, entry) = (req.nonce, repos.map.get(&req.repo_id));
            let Some(entry) = entry else {
                let _ = out.send(msg_git_commits(nonce, GIT_STATUS_UNKNOWN_ID, 0, &[], &[]));
                return;
            };
            let handle = entry.handle.clone();
            let owned = (
                req.flags,
                req.limit,
                req.path.to_string(),
                req.tips.clone(),
                req.hides.clone(),
            );
            git_request(
                repos,
                nonce,
                out,
                move |status| msg_git_commits(nonce, status, 0, &[], &[]),
                move |cancel| {
                    handle.log(
                        &GitLogRequest {
                            nonce,
                            repo_id: 0,
                            flags: owned.0,
                            limit: owned.1,
                            path: &owned.2,
                            tips: owned.3,
                            hides: owned.4,
                        },
                        &cancel,
                    )
                },
            );
        }
        C2S_GIT_TREE => {
            let Some(req) = parse_git_tree(data) else {
                if let Some(n) = git_nonce(data) {
                    let _ = out.send(msg_git_tree_resp(n, GIT_STATUS_INVALID, 0, &[]));
                }
                return;
            };
            let nonce = req.nonce;
            let Some(entry) = repos.map.get(&req.repo_id) else {
                let _ = out.send(msg_git_tree_resp(nonce, GIT_STATUS_UNKNOWN_ID, 0, &[]));
                return;
            };
            let handle = entry.handle.clone();
            let owned = (
                req.flags,
                req.oid,
                req.path.to_string(),
                req.after.to_string(),
            );
            git_request(
                repos,
                nonce,
                out,
                move |status| msg_git_tree_resp(nonce, status, 0, &[]),
                move |cancel| {
                    handle.tree(
                        &GitTreeRequest {
                            nonce,
                            repo_id: 0,
                            flags: owned.0,
                            oid: owned.1,
                            path: &owned.2,
                            after: &owned.3,
                        },
                        &cancel,
                    )
                },
            );
        }
        C2S_GIT_BLOB => {
            let Some(req) = parse_git_blob(data) else {
                if let Some(n) = git_nonce(data) {
                    let _ = out.send(msg_git_blob_resp(n, GIT_STATUS_INVALID, 0, &[]));
                }
                return;
            };
            let nonce = req.nonce;
            let Some(entry) = repos.map.get(&req.repo_id) else {
                let _ = out.send(msg_git_blob_resp(nonce, GIT_STATUS_UNKNOWN_ID, 0, &[]));
                return;
            };
            let handle = entry.handle.clone();
            let owned = (
                req.flags,
                req.oid,
                req.path.to_string(),
                req.offset,
                req.max_len,
            );
            git_request(
                repos,
                nonce,
                out,
                move |status| msg_git_blob_resp(nonce, status, 0, &[]),
                move |_cancel| {
                    handle.blob(&GitBlobRequest {
                        nonce,
                        repo_id: 0,
                        flags: owned.0,
                        oid: owned.1,
                        path: &owned.2,
                        offset: owned.3,
                        max_len: owned.4,
                    })
                },
            );
        }
        C2S_GIT_DIFF => {
            let Some(req) = parse_git_diff(data) else {
                if let Some(n) = git_nonce(data) {
                    let _ = out.send(msg_git_diff_resp(n, GIT_STATUS_INVALID, 0, &[]));
                }
                return;
            };
            let nonce = req.nonce;
            let Some(entry) = repos.map.get(&req.repo_id) else {
                let _ = out.send(msg_git_diff_resp(nonce, GIT_STATUS_UNKNOWN_ID, 0, &[]));
                return;
            };
            let handle = entry.handle.clone();
            let owned = (
                req.flags,
                req.old,
                req.new,
                req.path.to_string(),
                req.rename,
                req.after.to_string(),
            );
            git_request(
                repos,
                nonce,
                out,
                move |status| msg_git_diff_resp(nonce, status, 0, &[]),
                move |cancel| {
                    handle.diff(
                        &GitDiffRequest {
                            nonce,
                            repo_id: 0,
                            flags: owned.0,
                            rename: owned.4,
                            old: owned.1,
                            new: owned.2,
                            path: &owned.3,
                            after: &owned.5,
                        },
                        &cancel,
                    )
                },
            );
        }
        C2S_GIT_PATCH => {
            let Some(req) = parse_git_patch(data) else {
                if let Some(n) = git_nonce(data) {
                    let _ = out.send(msg_git_patch_resp(n, GIT_STATUS_INVALID, 0, &[]));
                }
                return;
            };
            let nonce = req.nonce;
            let Some(entry) = repos.map.get(&req.repo_id) else {
                let _ = out.send(msg_git_patch_resp(nonce, GIT_STATUS_UNKNOWN_ID, 0, &[]));
                return;
            };
            let handle = entry.handle.clone();
            let owned = (
                req.flags,
                req.context,
                req.old,
                req.new,
                req.path.to_string(),
                req.max_len,
                req.rename,
                req.after.to_string(),
                req.after_pos,
            );
            git_request(
                repos,
                nonce,
                out,
                move |status| msg_git_patch_resp(nonce, status, 0, &[]),
                move |cancel| {
                    handle.patch(
                        &GitPatchRequest {
                            nonce,
                            repo_id: 0,
                            flags: owned.0,
                            context: owned.1,
                            rename: owned.6,
                            old: owned.2,
                            new: owned.3,
                            path: &owned.4,
                            max_len: owned.5,
                            after: &owned.7,
                            after_pos: owned.8,
                        },
                        &cancel,
                    )
                },
            );
        }
        C2S_GIT_INDEX => {
            let Some(req) = parse_git_index(data) else {
                if let Some(n) = git_nonce(data) {
                    let _ = out.send(msg_git_index_resp(n, GIT_STATUS_INVALID, 0, &[]));
                }
                return;
            };
            let nonce = req.nonce;
            let Some(entry) = repos.map.get(&req.repo_id) else {
                let _ = out.send(msg_git_index_resp(nonce, GIT_STATUS_UNKNOWN_ID, 0, &[]));
                return;
            };
            let handle = entry.handle.clone();
            let owned = (req.flags, req.path.to_string(), req.after.to_string());
            git_request(
                repos,
                nonce,
                out,
                move |status| msg_git_index_resp(nonce, status, 0, &[]),
                move |cancel| {
                    handle.index(
                        &GitIndexRequest {
                            nonce,
                            repo_id: 0,
                            flags: owned.0,
                            path: &owned.1,
                            after: &owned.2,
                        },
                        &cancel,
                    )
                },
            );
        }
        C2S_GIT_BASE => {
            let Some((nonce, repo_id, oids)) = parse_git_base(data) else {
                if let Some(n) = git_nonce(data) {
                    let _ = out.send(msg_git_base_resp(n, GIT_STATUS_INVALID, &[]));
                }
                return;
            };
            let Some(entry) = repos.map.get(&repo_id) else {
                let _ = out.send(msg_git_base_resp(nonce, GIT_STATUS_UNKNOWN_ID, &[]));
                return;
            };
            let handle = entry.handle.clone();
            git_request(
                repos,
                nonce,
                out,
                move |status| msg_git_base_resp(nonce, status, &[]),
                move |cancel| handle.base(nonce, &oids, &cancel),
            );
        }
        C2S_GIT_RESOLVE => {
            let Some((nonce, repo_id, spec)) = parse_git_resolve(data) else {
                if let Some(n) = git_nonce(data) {
                    let _ = out.send(msg_git_resolve_resp(n, GIT_STATUS_INVALID, &[], &[]));
                }
                return;
            };
            let Some(entry) = repos.map.get(&repo_id) else {
                let _ = out.send(msg_git_resolve_resp(nonce, GIT_STATUS_UNKNOWN_ID, &[], &[]));
                return;
            };
            let handle = entry.handle.clone();
            let spec = spec.to_string();
            git_request(
                repos,
                nonce,
                out,
                move |status| msg_git_resolve_resp(nonce, status, &[], &[]),
                move |cancel| handle.resolve(nonce, &spec, &cancel),
            );
        }
        C2S_GIT_LOG_WATCH => {
            let Some((log_id, repo_id, flags, limit, spec)) = parse_git_log_watch(data) else {
                return;
            };
            let Some(entry) = repos.map.get_mut(&repo_id) else {
                // No repo → report on the log stream so the client unblocks.
                let _ = out.send(msg_git_log_page(
                    log_id,
                    1,
                    GIT_STATUS_UNKNOWN_ID,
                    0,
                    &[],
                    &[],
                ));
                return;
            };
            // Reject undefined flag bits.
            const KNOWN: u8 = GIT_LOG_FIRST_PARENT
                | GIT_LOG_TOPO
                | GIT_LOG_FULL_MESSAGE
                | GIT_LOG_FOLLOW
                | GIT_LOG_PATH_OIDS;
            if flags & !KNOWN != 0 {
                let _ = out.send(msg_git_log_page(log_id, 1, GIT_STATUS_INVALID, 0, &[], &[]));
                return;
            }
            // Start a log-only engine if the repo was opened without a watch.
            if entry.state.is_none() {
                let engine_out = out.clone();
                let opts = blit_git::StateOptions {
                    wants_state: false,
                    ..Default::default()
                };
                entry.state = Some(entry.handle.start_state(
                    repo_id,
                    opts,
                    Box::new(move |msg| engine_out.send(msg).is_ok()),
                ));
            }
            if let Some(state) = &entry.state {
                state.watch_log(log_id, flags, limit, spec.to_string());
            }
        }
        C2S_GIT_LOG_UNWATCH => {
            let Some((log_id, repo_id)) = parse_git_log_unwatch(data) else {
                return;
            };
            if let Some(entry) = repos.map.get(&repo_id)
                && let Some(state) = &entry.state
            {
                state.unwatch_log(log_id);
            }
        }
        C2S_GIT_LOG_ACK => {
            let Some((log_id, repo_id, update_id)) = parse_git_log_ack(data) else {
                return;
            };
            if let Some(entry) = repos.map.get(&repo_id)
                && let Some(state) = &entry.state
            {
                state.log_ack(log_id, update_id);
            }
        }
        C2S_GIT_DISCOVER => {
            let Some(req) = parse_git_discover(data) else {
                if let Some(n) = git_nonce(data) {
                    let _ = out.send(msg_git_discover_resp(n, GIT_STATUS_INVALID, 0, &[]));
                }
                return;
            };
            // No repo id: discovery is an enumeration, not an open, so it
            // needs no handle and consumes no budget.
            let nonce = req.nonce;
            let owned = (
                req.flags,
                req.depth,
                req.path.to_string(),
                req.after.to_string(),
            );
            git_request(
                repos,
                nonce,
                out,
                move |status| msg_git_discover_resp(nonce, status, 0, &[]),
                move |cancel| {
                    blit_git::discover(
                        &GitDiscoverRequest {
                            nonce,
                            flags: owned.0,
                            depth: owned.1,
                            path: &owned.2,
                            after: &owned.3,
                        },
                        &cancel,
                    )
                },
            );
        }
        C2S_GIT_BLAME => {
            let Some(req) = parse_git_blame(data) else {
                if let Some(n) = git_nonce(data) {
                    let _ = out.send(msg_git_blame_resp(n, GIT_STATUS_INVALID, 0, &[]));
                }
                return;
            };
            let nonce = req.nonce;
            let Some(entry) = repos.map.get(&req.repo_id) else {
                let _ = out.send(msg_git_blame_resp(nonce, GIT_STATUS_UNKNOWN_ID, 0, &[]));
                return;
            };
            let handle = entry.handle.clone();
            let owned = (
                req.flags,
                req.oid,
                req.start_line,
                req.line_count,
                req.path.to_string(),
            );
            git_request(
                repos,
                nonce,
                out,
                move |status| msg_git_blame_resp(nonce, status, 0, &[]),
                move |cancel| {
                    handle.blame(
                        &GitBlameRequest {
                            nonce,
                            repo_id: 0,
                            flags: owned.0,
                            oid: owned.1,
                            start_line: owned.2,
                            line_count: owned.3,
                            path: &owned.4,
                        },
                        &cancel,
                    )
                },
            );
        }
        C2S_GIT_REFLOG => {
            let Some(req) = parse_git_reflog(data) else {
                if let Some(n) = git_nonce(data) {
                    let _ = out.send(msg_git_reflog_resp(n, GIT_STATUS_INVALID, 0, &[]));
                }
                return;
            };
            let nonce = req.nonce;
            let Some(entry) = repos.map.get(&req.repo_id) else {
                let _ = out.send(msg_git_reflog_resp(nonce, GIT_STATUS_UNKNOWN_ID, 0, &[]));
                return;
            };
            let handle = entry.handle.clone();
            let owned = (
                req.flags,
                req.limit,
                req.ref_name.to_string(),
                req.after_pos,
            );
            git_request(
                repos,
                nonce,
                out,
                move |status| msg_git_reflog_resp(nonce, status, 0, &[]),
                move |cancel| {
                    handle.reflog(
                        &GitReflogRequest {
                            nonce,
                            repo_id: 0,
                            flags: owned.0,
                            limit: owned.1,
                            ref_name: &owned.2,
                            after_pos: owned.3,
                        },
                        &cancel,
                    )
                },
            );
        }
        C2S_GIT_FETCH => {
            let Some(req) = parse_git_fetch(data) else {
                if let Some(n) = git_nonce(data) {
                    let _ = out.send(msg_git_fetch_resp(n, GIT_STATUS_INVALID, 0, &[]));
                }
                return;
            };
            let nonce = req.nonce;
            let Some(entry) = repos.map.get(&req.repo_id) else {
                let _ = out.send(msg_git_fetch_resp(nonce, GIT_STATUS_UNKNOWN_ID, 0, &[]));
                return;
            };
            let handle = entry.handle.clone();
            let owned = (
                req.flags,
                req.timeout_ms,
                req.remote.to_string(),
                req.refspecs
                    .iter()
                    .map(|s| (*s).to_string())
                    .collect::<Vec<_>>(),
            );
            git_request(
                repos,
                nonce,
                out,
                move |status| msg_git_fetch_resp(nonce, status, 0, &[]),
                move |cancel| {
                    handle.fetch(
                        &GitFetchRequest {
                            nonce,
                            repo_id: 0,
                            flags: owned.0,
                            timeout_ms: owned.1,
                            remote: &owned.2,
                            refspecs: owned.3.iter().map(String::as_str).collect(),
                        },
                        &cancel,
                    )
                },
            );
        }
        _ => {}
    }
}

/// Per-connection language-intelligence attachments (docs/design/lsp.md).
/// The `lsp_id`s are connection-scoped like `repo_id`s; the backends
/// they attach to are daemon-owned and warm inside `blit-lsp`.
#[derive(Default)]
struct LspConns {
    map: HashMap<u16, blit_lsp::Attachment>,
    next_id: u16,
    /// Query nonces in flight (per-connection namespace); a duplicate
    /// is answered `INVALID` without executing, and the size bounds
    /// pending queries per connection.
    inflight: std::sync::Arc<std::sync::Mutex<std::collections::HashSet<u16>>>,
}

impl LspConns {
    fn alloc_id(&mut self) -> Option<u16> {
        for _ in 0..=u16::MAX {
            let id = self.next_id;
            self.next_id = self.next_id.wrapping_add(1);
            if id != blit_remote::lsp::LSP_ID_INVALID && !self.map.contains_key(&id) {
                return Some(id);
            }
        }
        None
    }

    fn max_opens() -> usize {
        // Read once: this sits on the per-message path.
        static V: std::sync::LazyLock<usize> = std::sync::LazyLock::new(|| {
            std::env::var("BLIT_LSP_MAX_OPENS")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(16)
        });
        *V
    }

    fn max_inflight() -> usize {
        static V: std::sync::LazyLock<usize> = std::sync::LazyLock::new(|| {
            std::env::var("BLIT_LSP_MAX_INFLIGHT")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(16)
        });
        *V
    }
}

/// The streaming sink for an attachment: every pushed message rides the
/// connection outbox.
fn lsp_stream_sink(out: &mpsc::UnboundedSender<Vec<u8>>) -> blit_lsp::Sink {
    let out = out.clone();
    std::sync::Arc::new(move |msg| out.send(msg).is_ok())
}

/// The reply sink for one query: retires the nonce from the in-flight
/// set when its `S2C_LSP_QUERY` response passes through.
fn lsp_query_sink(
    out: &mpsc::UnboundedSender<Vec<u8>>,
    inflight: &std::sync::Arc<std::sync::Mutex<std::collections::HashSet<u16>>>,
    nonce: u16,
) -> blit_lsp::Sink {
    let out = out.clone();
    let inflight = inflight.clone();
    std::sync::Arc::new(move |msg: Vec<u8>| {
        if msg.first() == Some(&blit_remote::lsp::S2C_LSP_QUERY)
            && msg.len() >= 3
            && u16::from_le_bytes([msg[1], msg[2]]) == nonce
        {
            inflight.lock().unwrap().remove(&nonce);
        }
        out.send(msg).is_ok()
    })
}

async fn handle_lsp_message(
    data: &[u8],
    conns: &mut LspConns,
    out: &mpsc::UnboundedSender<Vec<u8>>,
    verbose: bool,
) {
    use blit_remote::lsp::*;
    match data[0] {
        C2S_LSP_OPEN => {
            let Some((nonce, flags, diag_latency_ms, path)) = parse_lsp_open(data) else {
                // A well-formed nonce with a truncated/non-UTF-8 path still
                // gets its one reply, so the client promise resolves.
                if let Some(b) = data.get(1..3) {
                    let nonce = u16::from_le_bytes([b[0], b[1]]);
                    let _ = out.send(msg_lsp_opened(
                        nonce,
                        LSP_ID_INVALID,
                        LSP_STATUS_INVALID,
                        0,
                        "",
                        "malformed request",
                    ));
                }
                return;
            };
            let refuse = |status: u8, detail: &str| {
                let _ = out.send(msg_lsp_opened(nonce, LSP_ID_INVALID, status, 0, "", detail));
            };
            // Nonce discipline (docs/design/lsp.md: git.md's rules): a
            // nonce already in flight is a duplicate, answered INVALID
            // without executing.
            if conns.inflight.lock().unwrap().contains(&nonce) {
                refuse(LSP_STATUS_INVALID, "duplicate nonce");
                return;
            }
            const KNOWN: u8 = LSP_OPEN_WATCH | LSP_OPEN_DIAGS;
            if flags & !KNOWN != 0 {
                refuse(LSP_STATUS_INVALID, "unknown flags");
                return;
            }
            if conns.map.len() >= LspConns::max_opens() {
                refuse(LSP_STATUS_BUDGET, "attachment limit reached");
                return;
            }
            // Discovery walks and PATH scans (plus a possible backend
            // spawn) are filesystem work; run them off the runtime. The
            // read loop awaits, so this connection's messages stay
            // ordered (LSP_OPENED before the first LSP_STATE, and no
            // later LSP_* can observe a half-open attachment).
            let path_owned = path.to_string();
            let prepared = tokio::task::spawn_blocking(move || blit_lsp::prepare(&path_owned))
                .await
                .unwrap_or_else(|_| Err((LSP_STATUS_OTHER, "open task failed".to_string())));
            let (prepared, root, absent) = match prepared {
                Ok(prepared) => prepared,
                Err((status, detail)) => {
                    refuse(status, &detail);
                    return;
                }
            };
            let Some(lsp_id) = conns.alloc_id() else {
                refuse(LSP_STATUS_BUDGET, "no attachment ids left");
                return;
            };
            if verbose {
                eprintln!("C2S_LSP_OPEN: lsp_id={lsp_id} path={path} flags={flags:#x}");
            }
            // LSP_OPENED must precede the first LSP_STATE; the outbox is
            // FIFO, so sending before the pacer spawns suffices. On
            // success `detail` names any matched-but-uninstalled servers
            // (docs/design/lsp.md), so a client learns what to install.
            let _ = out.send(msg_lsp_opened(
                nonce,
                lsp_id,
                LSP_STATUS_OK,
                0,
                &root,
                &absent,
            ));
            let attachment = prepared.attach(lsp_id, flags, diag_latency_ms, lsp_stream_sink(out));
            conns.map.insert(lsp_id, attachment);
        }
        C2S_LSP_CLOSE => {
            let Some(lsp_id) = parse_lsp_close(data) else {
                return;
            };
            if conns.map.remove(&lsp_id).is_some() {
                let _ = out.send(msg_lsp_closed(lsp_id, LSP_CLOSED_CLIENT_REQUEST));
            }
        }
        C2S_LSP_ACK => {
            let Some((lsp_id, stream, update_id)) = parse_lsp_ack(data) else {
                return;
            };
            if let Some(attachment) = conns.map.get(&lsp_id) {
                attachment.ack(stream, update_id);
            }
        }
        C2S_LSP_QUERY => {
            let Some(req) = parse_lsp_query(data) else {
                // A well-formed nonce with a truncated/non-UTF-8 tail still
                // gets its one reply, so the client promise resolves.
                if let Some(b) = data.get(1..3) {
                    let nonce = u16::from_le_bytes([b[0], b[1]]);
                    let _ = out.send(msg_lsp_query_resp(nonce, LSP_STATUS_INVALID, 0, "", &[]));
                }
                return;
            };
            let refuse = |status: u8| {
                let _ = out.send(msg_lsp_query_resp(req.nonce, status, 0, "", &[]));
            };
            let Some(attachment) = conns.map.get(&req.lsp_id) else {
                refuse(LSP_STATUS_UNKNOWN_ID);
                return;
            };
            {
                let mut inflight = conns.inflight.lock().unwrap();
                if inflight.contains(&req.nonce) {
                    refuse(LSP_STATUS_INVALID);
                    return;
                }
                if inflight.len() >= LspConns::max_inflight() {
                    refuse(LSP_STATUS_BUDGET);
                    return;
                }
                inflight.insert(req.nonce);
            }
            attachment.query(
                req.nonce,
                req.kind,
                req.flags,
                req.line,
                req.col,
                req.path,
                req.arg,
                lsp_query_sink(out, &conns.inflight, req.nonce),
            );
        }
        C2S_LSP_CANCEL => {
            let Some(nonce) = parse_lsp_cancel(data) else {
                return;
            };
            // Advisory, by nonce alone: every attachment forwards; an
            // unknown nonce is a no-op.
            for attachment in conns.map.values() {
                attachment.cancel(nonce);
            }
        }
        C2S_LSP_SERVERS => {
            let Some(nonce) = parse_lsp_servers(data) else {
                return;
            };
            if conns.inflight.lock().unwrap().contains(&nonce) {
                let _ = out.send(msg_lsp_servers_resp(nonce, LSP_STATUS_INVALID, 0, &[]));
                return;
            }
            let _ = out.send(blit_lsp::servers_response(nonce));
        }
        C2S_LSP_STOP => {
            let Some((nonce, server_ref)) = parse_lsp_stop(data) else {
                return;
            };
            if conns.inflight.lock().unwrap().contains(&nonce) {
                let _ = out.send(msg_lsp_stopped(nonce, LSP_STATUS_INVALID));
                return;
            }
            if verbose {
                eprintln!("C2S_LSP_STOP: server_ref={server_ref}");
            }
            let _ = out.send(blit_lsp::stop_response(nonce, server_ref));
        }
        C2S_LSP_BUFFER => {
            // Fire-and-forget by design (docs/design/lsp.md
            // "LSP_BUFFER"): no nonce, so malformed frames and unknown
            // attachments are dropped, and a RELEASE carrying text is
            // treated as the release it claims to be.
            let Some((lsp_id, flags, path, text)) = parse_lsp_buffer(data) else {
                return;
            };
            let Some(attachment) = conns.map.get(&lsp_id) else {
                return;
            };
            let release = flags & LSP_BUFFER_RELEASE != 0;
            attachment.buffer(path, if release { None } else { Some(text) });
        }
        _ => {}
    }
}

/// Refuse every nonce-bearing `LSP_*` with `PERMISSION`, the way KV and NET
/// do when their families are off.
///
/// `BLIT_LSP=0` leaves the feature bit unadvertised, but a client that
/// ignores feature bits — or one that races a mid-session disable — used to
/// have its request silently dropped, so a promise awaiting the one
/// guaranteed reply per nonce never resolved. Fire-and-forget opcodes
/// (`ACK`, `CANCEL`, `BUFFER`) have no reply to give and are dropped, which
/// is what they get when the family is on and the id is unknown.
fn refuse_lsp_message(data: &[u8], out: &mpsc::UnboundedSender<Vec<u8>>) {
    use blit_remote::lsp::*;
    let nonce = data
        .get(1..3)
        .map(|b| u16::from_le_bytes([b[0], b[1]]))
        .unwrap_or(0);
    const DETAIL: &str = "lsp disabled";
    match data[0] {
        C2S_LSP_OPEN => {
            let _ = out.send(msg_lsp_opened(
                nonce,
                LSP_ID_INVALID,
                LSP_STATUS_PERMISSION,
                0,
                "",
                DETAIL,
            ));
        }
        C2S_LSP_QUERY => {
            let _ = out.send(msg_lsp_query_resp(
                nonce,
                LSP_STATUS_PERMISSION,
                0,
                DETAIL,
                &[],
            ));
        }
        C2S_LSP_SERVERS => {
            let _ = out.send(msg_lsp_servers_resp(nonce, LSP_STATUS_PERMISSION, 0, &[]));
        }
        C2S_LSP_STOP => {
            let _ = out.send(msg_lsp_stopped(nonce, LSP_STATUS_PERMISSION));
        }
        // LSP_CLOSE names an attachment id, not a nonce, and there is no
        // attachment to close.
        _ => {}
    }
}

async fn handle_client<S: AsyncRead + AsyncWrite + Unpin + Send + 'static>(
    stream: S,
    state: AppState,
) {
    let config = &state.config;
    let notify_for_compositor = {
        let n = state.delivery_notify.clone();
        Arc::new(move || n.notify_one()) as Arc<dyn Fn() + Send + Sync>
    };
    let (mut reader, mut writer) = tokio::io::split(stream);

    let (out_tx, mut out_rx) = mpsc::unbounded_channel::<Vec<u8>>();
    // Filesystem syncs are connection-scoped; engines write into the same
    // outbox as everything else and die with this map on disconnect.
    let fs_out = out_tx.clone();
    let mut fs_syncs = FsSyncs::default();
    let mut git_repos = GitRepos::default();
    let mut lsp_conns = LspConns::default();
    let mut kv_subs = kv::KvSubs::default();
    // BLIT_NET=0 turns the relay off: the bit is unadvertised AND every
    // NET_OPEN is refused with PERMISSION, so a client that ignores
    // feature bits still gets its one reply.
    let net_enabled = !std::env::var("BLIT_NET").is_ok_and(|v| v == "0");
    let net_policy = net::Policy::new(config.allow_forward_insecure, &config.allow_forward);
    // BLIT_LSP=0 turns the whole family off: the bit is unadvertised AND
    // every nonce-bearing LSP_* is refused with PERMISSION, as KV and NET
    // do.
    let lsp_enabled = !std::env::var("BLIT_LSP").is_ok_and(|v| v == "0");
    // BLIT_KV=0 disables the store: the bit is unadvertised AND every
    // nonce-bearing KV_* is refused at dispatch with PERMISSION, so a
    // client that ignores feature bits still gets its one reply
    // (docs/design/kv.md "Security posture").
    let kv_enabled = !std::env::var("BLIT_KV").is_ok_and(|v| v == "0");
    // BLIT_FS_WRITE=0 offers read-only sync: FS_WRITE/FS_OP answer
    // FS_DONE_PERMISSION instead of dispatching (docs/design/fs-write.md
    // "Security"). The family shares FEATURE_FS, so there is no
    // separate bit to withhold.
    let fs_write_enabled = !std::env::var("BLIT_FS_WRITE").is_ok_and(|v| v == "0");
    #[cfg(target_os = "linux")]
    let (audio_tx, mut audio_rx) = mpsc::unbounded_channel::<Vec<u8>>();
    // On non-Linux, keep the audio sender alive for the lifetime of the
    // outer function so audio_rx.recv() never resolves to None — the
    // biased select below would otherwise hit the audio branch first and
    // break out of the sender loop before any HELLO/LIST/READY frames
    // are written.
    #[cfg(not(target_os = "linux"))]
    let (_audio_tx, mut audio_rx) = mpsc::unbounded_channel::<Vec<u8>>();
    let outbox_frame_counter = Arc::new(AtomicUsize::new(0));
    let outbox_byte_counter = Arc::new(AtomicUsize::new(0));
    let write_blocked_counter = Arc::new(AtomicU64::new(0));
    // Relayed sockets are connection-scoped: they die with this table on
    // disconnect, which is what releases forwarded sockets on a dropped
    // client rather than leaking them. Datagrams read the outbox depth to
    // decide whether to drop rather than pile on (docs/design/net.md).
    let mut net_sockets =
        net::NetSockets::with_outbox(outbox_frame_counter.clone(), outbox_byte_counter.clone());
    let sender_outbox_queued_frames = outbox_frame_counter.clone();
    let sender_outbox_queued_bytes = outbox_byte_counter.clone();
    let sender_write_blocked_us = write_blocked_counter.clone();
    let sender = tokio::spawn(async move {
        let audio_debug = std::env::var_os("BLIT_AUDIO_DEBUG").is_some();
        let mut audio_window_start = Instant::now();
        let mut last_audio_pick_at = Instant::now();
        let mut audio_sends_in_window: u32 = 0;
        let mut max_audio_pick_gap: u32 = 0;
        let mut max_audio_write_ms: u32 = 0;
        loop {
            // Drain all pending audio before waiting for the next message.
            // Audio frames are tiny (~160 B) so this is near-instant.
            while let Ok(audio_msg) = audio_rx.try_recv() {
                if !write_frame(&mut writer, &audio_msg).await {
                    return;
                }
                if audio_debug {
                    audio_sends_in_window += 1;
                    let now = Instant::now();
                    let pick_gap = now.duration_since(last_audio_pick_at).as_millis() as u32;
                    last_audio_pick_at = now;
                    if pick_gap > max_audio_pick_gap {
                        max_audio_pick_gap = pick_gap;
                    }
                }
            }

            // Wait for the next message from either channel.  Prefer audio
            // so that audio frames queued while we were writing are sent
            // before the next bulk message.
            let msg = tokio::select! {
                biased;
                msg = audio_rx.recv() => {
                    // Pure audio message — write it directly (tiny).
                    match msg {
                        Some(m) => {
                            let audio_write_start = Instant::now();
                            if !write_frame(&mut writer, &m).await {
                                break;
                            }
                            if audio_debug {
                                let now = Instant::now();
                                audio_sends_in_window += 1;
                                let pick_gap = now
                                    .duration_since(last_audio_pick_at)
                                    .as_millis() as u32;
                                last_audio_pick_at = now;
                                let write_ms =
                                    now.duration_since(audio_write_start).as_millis() as u32;
                                if pick_gap > max_audio_pick_gap {
                                    max_audio_pick_gap = pick_gap;
                                }
                                if write_ms > max_audio_write_ms {
                                    max_audio_write_ms = write_ms;
                                }
                                if now.duration_since(audio_window_start)
                                    >= Duration::from_secs(1)
                                {
                                    eprintln!(
                                        "[sender audio] writes={} max_pick_gap={}ms max_write={}ms",
                                        audio_sends_in_window,
                                        max_audio_pick_gap,
                                        max_audio_write_ms,
                                    );
                                    audio_sends_in_window = 0;
                                    max_audio_pick_gap = 0;
                                    max_audio_write_ms = 0;
                                    audio_window_start = now;
                                }
                            }
                            continue;
                        }
                        None => break,
                    }
                }
                msg = out_rx.recv() => msg,
            };

            // Non-audio message: may be large (video keyframe, terminal
            // snapshot).  Use interleaved write so audio frames that arrive
            // while the kernel TCP buffer drains are written between write
            // syscalls rather than piling up and being dropped.
            match msg {
                Some(m) => {
                    let bytes = m.len();
                    let write_start = Instant::now();
                    let wrote = write_frame_interleaved(&mut writer, &m, &mut audio_rx).await;
                    let write_elapsed = write_start.elapsed();
                    sender_write_blocked_us.fetch_add(
                        write_elapsed.as_micros().min(u64::MAX as u128) as u64,
                        Ordering::Relaxed,
                    );
                    // Threshold lowered from 100 ms to 30 ms so sub-chunk
                    // stalls on slow links (the band that can still
                    // block audio delivery for longer than the 20 ms
                    // Opus frame cadence) show up in the log.
                    if write_elapsed.as_millis() > 30 {
                        eprintln!(
                            "[sender] slow write: bytes={bytes} elapsed={}ms wrote={wrote}",
                            write_elapsed.as_millis(),
                        );
                    }
                    mark_outbox_drained(
                        &sender_outbox_queued_frames,
                        &sender_outbox_queued_bytes,
                        bytes,
                    );
                    if !wrote {
                        break;
                    }
                }
                None => break,
            }
        }
    });
    let client_id;

    {
        let mut sess = state.session.lock().await;
        client_id = sess.next_client_id;
        sess.next_client_id += 1;
        sess.clients.insert(
            client_id,
            ClientState {
                tx: out_tx,
                outbox_queued_frames: outbox_frame_counter,
                outbox_queued_bytes: outbox_byte_counter,
                write_blocked_us: write_blocked_counter,
                write_blocked_us_seen: 0,
                #[cfg(target_os = "linux")]
                audio_tx,
                lead: None,
                subscriptions: HashSet::new(),
                surface_subscriptions: HashSet::new(),
                #[cfg(target_os = "linux")]
                audio_subscribed: false,
                #[cfg(target_os = "linux")]
                audio_bitrate_kbps: 0,
                view_sizes: HashMap::new(),
                scroll_offsets: HashMap::new(),
                scroll_caches: HashMap::new(),
                last_sent: HashMap::new(),
                last_used_rows_sent: HashMap::new(),
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
                avg_surface_frame_bytes: 8_192.0,
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
                last_window_blocked_log: Instant::now(),
                last_skip_log: Instant::now(),
                skip_same_gen_count: 0,
                skip_in_flight_count: 0,
                skip_pacing_count: 0,
                skip_vulkan_await_count: 0,
                skip_no_subs_count: 0,
                skip_not_subbed_count: 0,
                skip_last_pixels_mismatch_count: 0,
                encode_loop_iters: 0,
                goodput_window_bytes: 0,
                goodput_window_start: Instant::now(),
                surface_subs: HashMap::new(),
                surface_inflight_frames: VecDeque::new(),
                vulkan_video_surfaces: HashMap::new(),
                surface_view_sizes: HashMap::new(),
                surface_codec_support: 0,
                surface_max_decode: (0, 0),
                pressed_surface_keys: HashSet::new(),
            },
        );
        // Wake the tick loop so the new client gets its first frame.
        state.delivery_notify.notify_one();
        if let Some(c) = sess.clients.get(&client_id) {
            let features = FEATURE_CREATE_NONCE
                | FEATURE_RESTART
                | FEATURE_RESIZE_BATCH
                | FEATURE_COPY_RANGE
                | FEATURE_COMPOSITOR
                | FEATURE_CREATE_STATUS
                | FEATURE_KILL_MODE
                | FEATURE_PTY_DEADLINE
                | blit_remote::fs::FEATURE_FS
                | blit_remote::git::FEATURE_GIT;
            // BLIT_LSP=0 disables the family: the bit is simply not
            // advertised, matching the dispatch gate.
            let mut features = features;
            if lsp_enabled {
                features |= blit_remote::lsp::FEATURE_LSP;
            }
            if kv_enabled {
                features |= blit_remote::kv::FEATURE_KV;
            }
            if net_enabled {
                features |= blit_remote::net::FEATURE_NET;
            }
            #[cfg(target_os = "linux")]
            {
                let audio_disabled = std::env::var("BLIT_AUDIO")
                    .map(|v| v == "0")
                    .unwrap_or(false);
                if !audio_disabled && audio::pipewire_available() {
                    features |= FEATURE_AUDIO;
                }
            }
            let _ = send_outbox(
                c,
                msg_hello(
                    1,
                    features,
                    state.boot_generation,
                    env!("CARGO_PKG_VERSION"),
                ),
            );
        }
        let mut initial_msgs = Vec::with_capacity(2 + sess.ptys.len() * 2);
        // Send surface-created messages BEFORE the PTY list so that
        // the client's surface store is populated before `ready` is
        // set — otherwise the BSP reconciliation runs with an empty
        // surface list and wipes restored surface assignments.
        if let Some(cs) = sess.compositor.as_ref() {
            for info in cs.surfaces.values() {
                // Use the authoritative native size if the stored
                // width/height is still 0 (surface created before first
                // commit).  Falling back to the largest pixel snapshot
                // entry would give a stale per-client downscale-target
                // size for surfaces that have already cycled through
                // resizes (see `compositor_native_for_sid`).
                let (w, h) = if info.width == 0 && info.height == 0 {
                    cs.native_sizes
                        .get(&info.surface_id)
                        .map(|&(w, h)| (w as u16, h as u16))
                        .or_else(|| {
                            cs.last_pixels
                                .iter()
                                .filter(|(k, _)| k.0 == info.surface_id)
                                .max_by_key(|(_, lp)| (lp.width as u64) * (lp.height as u64))
                                .map(|(_, lp)| (lp.width as u16, lp.height as u16))
                        })
                        .unwrap_or((0, 0))
                } else {
                    (info.width, info.height)
                };
                initial_msgs.push(msg_surface_created(
                    info.surface_id,
                    info.parent_id,
                    w,
                    h,
                    &info.title,
                    &info.app_id,
                ));
                // Also send a resize message so the client gets the
                // correct dimensions even if surface_created carried 0x0.
                if w > 0 && h > 0 {
                    initial_msgs.push(msg_surface_resized(info.surface_id, w, h));
                }
            }
        }
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
                // Carry the attribution, not just the status.  Arming a
                // deadline, disconnecting, and reconnecting to collect is the
                // case the reason byte exists for; replaying a bare
                // `signal(15)` here would put back exactly the ambiguity it
                // was added to remove.
                initial_msgs.push(blit_remote::msg_exited_reason(
                    id,
                    pty.exit_status,
                    pty.exit_reason,
                ));
            }
        }
        initial_msgs.push(vec![S2C_READY]);
        let tx = sess.clients.get(&client_id).map(|c| {
            (
                c.tx.clone(),
                c.outbox_queued_frames.clone(),
                c.outbox_queued_bytes.clone(),
            )
        });
        drop(sess);
        if let Some((tx, queued_frames, queued_bytes)) = tx {
            for msg in initial_msgs {
                if send_outbox_tracked(&tx, &queued_frames, &queued_bytes, msg).is_err() {
                    break;
                }
            }
        }
    }

    if state.config.verbose {
        eprintln!("client connected");
    }

    while let Some(data) = read_frame(&mut reader).await {
        if data.is_empty() {
            continue;
        }

        if data[0] == C2S_ACK {
            let mut sess = state.session.lock().await;
            if let Some(c) = sess.clients.get_mut(&client_id) {
                c.acks_recv += 1;
                record_ack(c);
            } else {
                continue;
            }
            maybe_log_pacing_metrics(&mut sess, client_id, config.verbose);
            nudge_delivery(&state);
            continue;
        }

        if data[0] == C2S_PING {
            // Application-level keepalive — no-op.  Its arrival is enough
            // to keep the connection alive (any received data resets
            // transport-level timeouts).
            continue;
        }

        // Filesystem sync: connection-scoped, engine-threaded, and
        // deliberately handled before the session mutex — no fs message
        // ever needs session state.
        if matches!(
            data[0],
            blit_remote::fs::C2S_FS_SYNC
                | blit_remote::fs::C2S_FS_STOP
                | blit_remote::fs::C2S_FS_ACK
                | blit_remote::fs::C2S_FS_FETCH
                | blit_remote::fs::C2S_FS_WRITE
                | blit_remote::fs::C2S_FS_OP
                | blit_remote::fs::C2S_FS_SEARCH
                | blit_remote::fs::C2S_FS_INDEX
                | blit_remote::fs::C2S_FS_GREP
        ) {
            // A FROM_PTY sync (docs/ide.md Decision 3) names a source pty
            // whose live cwd is session state; resolve it here — the sole
            // place these connection-scoped families touch the session
            // mutex, and only when the client opts in — then rebase to a
            // plain path-based sync so the handler stays path-only.
            let msg: std::borrow::Cow<[u8]> = if data[0] == blit_remote::fs::C2S_FS_SYNC {
                if let Some(src) = blit_remote::fs::fs_sync_src_pty(&data) {
                    let cwd = {
                        let sess = state.session.lock().await;
                        sess.ptys.get(&src).and_then(|p| pty::pty_cwd(&p.handle))
                    };
                    // No resolvable cwd — the pty is gone or its process has
                    // exited. Refuse rather than rebase: the pty-relative
                    // path (usually "") would be read as an absolute root.
                    let Some(cwd) = cwd else {
                        let nonce = u16::from_le_bytes([data[1], data[2]]);
                        let _ = fs_out.send(blit_remote::fs::msg_fs_synced(
                            nonce,
                            blit_remote::fs::FS_SYNC_ID_INVALID,
                            blit_remote::fs::FS_STATUS_NOT_FOUND,
                            NO_SOURCE_CWD,
                        ));
                        continue;
                    };
                    blit_remote::fs::fs_sync_rebase(&data, &cwd)
                        .map(std::borrow::Cow::Owned)
                        .unwrap_or(std::borrow::Cow::Borrowed(&data[..]))
                } else {
                    std::borrow::Cow::Borrowed(&data[..])
                }
            } else {
                std::borrow::Cow::Borrowed(&data[..])
            };
            // A read-only deployment (BLIT_FS_WRITE=0) shares the family's
            // feature bit, so writes are refused here rather than dropped:
            // every nonce still gets its one FS_DONE.
            if !fs_write_enabled
                && matches!(
                    msg[0],
                    blit_remote::fs::C2S_FS_WRITE | blit_remote::fs::C2S_FS_OP
                )
            {
                let nonce = msg
                    .get(1..3)
                    .map(|b| u16::from_le_bytes([b[0], b[1]]))
                    .unwrap_or(0);
                let _ = fs_out.send(blit_remote::fs::msg_fs_done(
                    nonce,
                    blit_remote::fs::FS_DONE_PERMISSION,
                    0,
                    0,
                ));
            } else {
                handle_fs_message(&msg, &mut fs_syncs, &fs_out, config.verbose).await;
            }
            continue;
        }

        // Git introspection: same discipline as fs — connection-scoped,
        // request threads and state engines, never the session mutex.
        if (blit_remote::git::C2S_GIT_OPEN..=blit_remote::git::C2S_GIT_FETCH).contains(&data[0]) {
            // A pty-relative open (docs/ide.md Decision 3): resolve the
            // source pty's live cwd (session state) and rebase to a plain
            // path-based open.
            let msg: std::borrow::Cow<[u8]> = if data[0] == blit_remote::git::C2S_GIT_OPEN {
                // A request naming both a source pty and a parent repo is
                // contradictory; it is left unrebased so the handler sees
                // both fields and refuses it, rather than being rewritten
                // into an absolute submodule path that fails later.
                let src = blit_remote::git::parse_git_open(&data)
                    .filter(|r| r.parent_repo_id == blit_remote::git::GIT_OPEN_NO_CONTEXT)
                    .map(|r| r.src_pty_id)
                    .filter(|&id| id != blit_remote::git::GIT_OPEN_NO_CONTEXT);
                if let Some(src) = src {
                    let cwd = {
                        let sess = state.session.lock().await;
                        sess.ptys.get(&src).and_then(|p| pty::pty_cwd(&p.handle))
                    };
                    // No resolvable cwd (pty gone / process exited) — refuse;
                    // see the fs sibling above.
                    let Some(cwd) = cwd else {
                        if let Some(nonce) = git_nonce(&data) {
                            let _ = fs_out.send(blit_remote::git::msg_git_repo(
                                nonce,
                                blit_remote::git::GIT_REPO_ID_INVALID,
                                blit_remote::git::GIT_STATUS_NOT_FOUND,
                                0,
                                0,
                                NO_SOURCE_CWD,
                                "",
                            ));
                        }
                        continue;
                    };
                    blit_remote::git::git_open_rebase(&data, &cwd)
                        .map(std::borrow::Cow::Owned)
                        .unwrap_or(std::borrow::Cow::Borrowed(&data[..]))
                } else {
                    std::borrow::Cow::Borrowed(&data[..])
                }
            } else {
                std::borrow::Cow::Borrowed(&data[..])
            };
            handle_git_message(&msg, &mut git_repos, &fs_out, config.verbose).await;
            continue;
        }

        // Language intelligence: connection-scoped attachments over
        // daemon-owned warm backends, never the session mutex. When
        // BLIT_LSP=0 the family is off — the feature bit is unadvertised
        // AND every nonce-bearing request is refused with PERMISSION, so no
        // client can spawn a language server against a disabled server and
        // none is left waiting for a reply that is never coming.
        if (blit_remote::lsp::C2S_LSP_OPEN..=blit_remote::lsp::C2S_LSP_BUFFER).contains(&data[0]) {
            if !lsp_enabled {
                refuse_lsp_message(&data, &fs_out);
                continue;
            }
            // FROM_PTY open (docs/ide.md Decision 3): resolve the source pty's
            // live cwd (session state) and rebase to a plain path-based open.
            let msg: std::borrow::Cow<[u8]> = if data[0] == blit_remote::lsp::C2S_LSP_OPEN {
                if let Some(src) = blit_remote::lsp::lsp_open_src_pty(&data) {
                    let cwd = {
                        let sess = state.session.lock().await;
                        sess.ptys.get(&src).and_then(|p| pty::pty_cwd(&p.handle))
                    };
                    // No resolvable cwd (pty gone / process exited) — refuse;
                    // see the fs sibling above.
                    let Some(cwd) = cwd else {
                        let nonce = u16::from_le_bytes([data[1], data[2]]);
                        let _ = fs_out.send(blit_remote::lsp::msg_lsp_opened(
                            nonce,
                            blit_remote::lsp::LSP_ID_INVALID,
                            blit_remote::lsp::LSP_STATUS_NOT_FOUND,
                            0,
                            "",
                            NO_SOURCE_CWD,
                        ));
                        continue;
                    };
                    blit_remote::lsp::lsp_open_rebase(&data, &cwd)
                        .map(std::borrow::Cow::Owned)
                        .unwrap_or(std::borrow::Cow::Borrowed(&data[..]))
                } else {
                    std::borrow::Cow::Borrowed(&data[..])
                }
            } else {
                std::borrow::Cow::Borrowed(&data[..])
            };
            handle_lsp_message(&msg, &mut lsp_conns, &fs_out, config.verbose).await;
            continue;
        }

        // Server KV store (docs/design/kv.md): connection-scoped
        // subscriptions over one process-global store, never the session
        // mutex. BLIT_KV=0 refuses nonce-bearing requests with PERMISSION.
        if (blit_remote::kv::C2S_KV_OPEN..=blit_remote::kv::C2S_KV_FETCH).contains(&data[0]) {
            if kv_enabled {
                kv::handle_kv_message(&data, &mut kv_subs, &fs_out, config.verbose);
            } else {
                kv::refuse_kv_message(&data, &fs_out);
            }
            continue;
        }

        // TCP and UDP relay (docs/design/net.md): connection-scoped sockets,
        // one task per socket, never the session mutex. BLIT_NET=0 refuses
        // every open with PERMISSION.
        if blit_remote::net::is_c2s_net(data[0]) {
            if net_enabled {
                net::handle_net_message(
                    &data,
                    &mut net_sockets,
                    &fs_out,
                    &net_policy,
                    config.verbose,
                )
                .await;
            } else {
                net::refuse_net_message(&data, &fs_out);
            }
            continue;
        }

        if data[0] == C2S_DISPLAY_RATE && data.len() >= 3 {
            // Clamped, not just checked for zero. This is a client-declared
            // number that drives frame pacing for this connection and, via
            // the max across clients, the compositor's advertised refresh
            // rate — so 65535 was reachable straight off the wire.
            let fps = u16::from_le_bytes([data[1], data[2]]).min(MAX_DISPLAY_FPS) as f32;
            if fps > 0.0 {
                let mut sess = state.session.lock().await;
                if let Some(c) = sess.clients.get_mut(&client_id) {
                    c.display_fps = fps;
                }
                // Advertise the highest refresh rate across all clients
                // to the compositor so Wayland apps render at full speed.
                let max_fps = sess
                    .clients
                    .values()
                    .map(|c| c.display_fps)
                    .fold(0.0f32, f32::max);
                let mhz = (max_fps * 1000.0).round() as u32;
                if mhz > 0
                    && let Some(cs) = &sess.compositor
                {
                    let _ = cs
                        .handle
                        .command_tx
                        .send(blit_compositor::CompositorCommand::SetRefreshRate { mhz });
                }
            }
            nudge_delivery(&state);
            continue;
        }

        if data[0] == C2S_CLIENT_METRICS && data.len() >= 7 {
            let backlog_frames = u16::from_le_bytes([data[1], data[2]]);
            let ack_ahead_frames = u16::from_le_bytes([data[3], data[4]]);
            let apply_ms = u16::from_le_bytes([data[5], data[6]]) as f32 * 0.1;
            let mut sess = state.session.lock().await;
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
            let sess = state.session.lock().await;
            if let Some(pty) = sess.ptys.get(&pid) {
                let (echo, icanon) = pty.lflag_cache;
                if let Some(seq) = pty
                    .driver
                    .mouse_event(type_, button, col, row, echo, icanon)
                    && let Some(&fd) = state.pty_fds.read().unwrap().get(&pid)
                {
                    pty::pty_write_all(fd, &seq);
                }
            }
            continue;
        }

        if data[0] == C2S_INPUT && data.len() >= 3 {
            let pid = u16::from_le_bytes([data[1], data[2]]);
            let mut need_nudge = false;
            {
                let mut sess = state.session.lock().await;
                if let Some(c) = sess.clients.get_mut(&client_id)
                    && update_client_scroll_state(c, pid, 0)
                    && let Some(pty) = sess.ptys.get_mut(&pid)
                {
                    pty.mark_dirty();
                    need_nudge = true;
                }
                // Write input to the PTY fd while still holding the session
                // lock. The fd's lifecycle (remove from pty_fds + libc::close)
                // is guarded by this lock, so releasing it before the write
                // would let a concurrent close run and the OS reuse the fd
                // integer between lookup and write — routing input to the
                // wrong fd. (Mirrors the C2S_MOUSE handler above.)
                if let Some(&fd) = state.pty_fds.read().unwrap().get(&pid) {
                    pty::pty_write_all(fd, &data[3..]);
                }
            }
            if need_nudge {
                nudge_delivery(&state);
            }
            continue;
        }

        if data[0] == C2S_SEARCH && data.len() >= 3 {
            let request_id = u16::from_le_bytes([data[1], data[2]]);
            let query = std::str::from_utf8(&data[3..]).unwrap_or("").trim();
            // Refuse rather than truncate: a clipped regex is a different
            // regex, and answering it as if it were the one asked for is
            // worse than answering nothing.
            let query = if query.len() > MAX_SEARCH_QUERY {
                ""
            } else {
                query
            };
            let mut sess = state.session.lock().await;
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
                let _ = send_outbox(client, build_search_results_msg(request_id, &ranked));
            }
            continue;
        }

        if data[0] == C2S_SURFACE_CAPTURE && data.len() >= 3 {
            let surface_id = u16::from_le_bytes([data[1], data[2]]);
            // Extended message includes format and quality bytes.
            let format = data.get(3).copied().unwrap_or(CAPTURE_FORMAT_PNG);
            let quality = data.get(4).copied().unwrap_or(0);
            let scale_120 = if data.len() >= 7 {
                u16::from_le_bytes([data[5], data[6]])
            } else {
                0
            };

            let mut reply_msg = vec![S2C_SURFACE_CAPTURE];
            reply_msg.extend_from_slice(&surface_id.to_le_bytes());

            eprintln!("[capture] acquiring lock for surface {surface_id}");
            let (snapshot, command_tx) = {
                let sess = state.session.lock().await;
                eprintln!("[capture] lock acquired");
                // Snapshot the largest cached entry for this surface
                // (the native composite) for the capture fallback path.
                let snap = sess.compositor.as_ref().and_then(|cs| {
                    cs.last_pixels
                        .iter()
                        .filter(|(k, _)| k.0 == surface_id)
                        .max_by_key(|(_, lp)| (lp.width as u64) * (lp.height as u64))
                        .map(|(_, lp)| (lp.width, lp.height, lp.pixels.clone()))
                });
                let cmd_tx = sess
                    .compositor
                    .as_ref()
                    .map(|cs| cs.handle.command_tx.clone());
                (snap, cmd_tx)
            };

            // Compositor direct capture (CPU compositing from the per-surface
            // pixel cache).  This is the primary path — it produces correct
            // lossless results for clients that use CPU-mappable DMA-BUFs
            // (Chromium/Brave) or SHM buffers.
            let mut captured: Option<(u32, u32, Vec<u8>)> = None;
            if let Some(ctx) = command_tx {
                captured = request_surface_capture_with_timeout(
                    ctx,
                    surface_id,
                    scale_120,
                    Duration::from_secs(5),
                )
                .await;
            }

            // Fallback: last_pixels from the video pipeline.  Used when
            // the compositor capture returns nothing (no cached buffers).
            if captured.is_none() {
                captured = snapshot.and_then(|(w, h, pixels)| {
                    if pixels.is_dmabuf() {
                        return None;
                    }
                    let rgba = pixels.to_rgba(w, h);
                    if rgba.is_empty() {
                        None
                    } else {
                        Some((w, h, rgba))
                    }
                });
            }

            eprintln!("[capture] acquiring client_tx lock");
            let client_tx = {
                let sess = state.session.lock().await;
                eprintln!("[capture] client_tx lock acquired");
                sess.clients.get(&client_id).map(|c| {
                    (
                        c.tx.clone(),
                        c.outbox_queued_frames.clone(),
                        c.outbox_queued_bytes.clone(),
                    )
                })
            };

            if let Some((w, h, rgba_pixels)) = captured {
                let image_data = encode_capture(&rgba_pixels, w, h, format, quality);
                reply_msg.extend_from_slice(&w.to_le_bytes());
                reply_msg.extend_from_slice(&h.to_le_bytes());
                reply_msg.extend_from_slice(&image_data);
            } else {
                reply_msg.extend_from_slice(&0u32.to_le_bytes());
                reply_msg.extend_from_slice(&0u32.to_le_bytes());
            }

            if let Some((client_tx, queued_frames, queued_bytes)) = client_tx {
                eprintln!("[capture] sending reply: {} bytes", reply_msg.len());
                match send_outbox_tracked(&client_tx, &queued_frames, &queued_bytes, reply_msg) {
                    Ok(()) => eprintln!("[capture] sent OK"),
                    Err(e) => eprintln!("[capture] send failed: {e}"),
                }
            } else {
                eprintln!("[capture] no client_tx");
            }
            continue;
        }

        if data[0] == C2S_QUIT {
            let sess = state.session.lock().await;
            sess.send_to_all(&[S2C_QUIT]);
            drop(sess);
            state.shutdown_notify.notify_one();
            break;
        }

        let mut sess = state.session.lock().await;
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
                let mut touched = Vec::with_capacity((data.len() - 1) / 6);
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
                            c.view_sizes.insert(pid, clamp_view_size(rows, cols));
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
                // Straight off the wire and straight into the grid allocation.
                let (rows, cols) = clamp_view_size(rows, cols);
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
                let Some(id) = sess.allocate_pty_id(config.max_ptys) else {
                    continue;
                };
                let socket_name = sess
                    .ensure_compositor(
                        config.verbose,
                        notify_for_compositor.clone(),
                        &config.vaapi_device,
                    )
                    .to_string();
                #[cfg(target_os = "linux")]
                let pulse_server = sess.pulse_server_path();
                #[cfg(not(target_os = "linux"))]
                let pulse_server: Option<String> = None;
                #[cfg(target_os = "linux")]
                let pipewire_remote = sess.pipewire_remote_path();
                #[cfg(not(target_os = "linux"))]
                let pipewire_remote: Option<String> = None;
                if let Some(pty) = pty::spawn_pty(
                    &config.shell,
                    &config.shell_flags,
                    rows,
                    cols,
                    id,
                    tag,
                    command,
                    argv.as_deref(),
                    dir.as_deref(),
                    config.scrollback,
                    state.clone(),
                    Some(&socket_name),
                    pulse_server.as_deref(),
                    pipewire_remote.as_deref(),
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
                // Straight off the wire and straight into the grid allocation.
                let (rows, cols) = clamp_view_size(rows, cols);
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
                let Some(id) = sess.allocate_pty_id(config.max_ptys) else {
                    continue;
                };
                let socket_name = sess
                    .ensure_compositor(
                        config.verbose,
                        notify_for_compositor.clone(),
                        &config.vaapi_device,
                    )
                    .to_string();
                #[cfg(target_os = "linux")]
                let pulse_server = sess.pulse_server_path();
                #[cfg(not(target_os = "linux"))]
                let pulse_server: Option<String> = None;
                #[cfg(target_os = "linux")]
                let pipewire_remote = sess.pipewire_remote_path();
                #[cfg(not(target_os = "linux"))]
                let pipewire_remote: Option<String> = None;
                if let Some(pty) = pty::spawn_pty(
                    &config.shell,
                    &config.shell_flags,
                    rows,
                    cols,
                    id,
                    tag,
                    command,
                    argv.as_deref(),
                    dir.as_deref(),
                    config.scrollback,
                    state.clone(),
                    Some(&socket_name),
                    pulse_server.as_deref(),
                    pipewire_remote.as_deref(),
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
                        reset_inflight(c);
                        let _ = send_outbox(c, nonce_msg);
                    }
                    for (&cid, c) in sess.clients.iter() {
                        if cid != client_id {
                            let _ = send_outbox(c, broadcast_msg.clone());
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
                // Straight off the wire and straight into the grid allocation.
                let (rows, cols) = clamp_view_size(rows, cols);
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
                    sess.ptys.get(&src_id).and_then(|p| pty::pty_cwd(&p.handle))
                } else {
                    None
                };
                let Some(id) = sess.allocate_pty_id(config.max_ptys) else {
                    continue;
                };
                let socket_name = sess
                    .ensure_compositor(
                        config.verbose,
                        notify_for_compositor.clone(),
                        &config.vaapi_device,
                    )
                    .to_string();
                #[cfg(target_os = "linux")]
                let pulse_server = sess.pulse_server_path();
                #[cfg(not(target_os = "linux"))]
                let pulse_server: Option<String> = None;
                #[cfg(target_os = "linux")]
                let pipewire_remote = sess.pipewire_remote_path();
                #[cfg(not(target_os = "linux"))]
                let pipewire_remote: Option<String> = None;
                if let Some(pty) = pty::spawn_pty(
                    &config.shell,
                    &config.shell_flags,
                    rows,
                    cols,
                    id,
                    tag,
                    None,
                    None,
                    dir.as_deref(),
                    config.scrollback,
                    state.clone(),
                    Some(&socket_name),
                    pulse_server.as_deref(),
                    pipewire_remote.as_deref(),
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
                        reset_inflight(c);
                    }
                    sess.send_to_all(&msg);
                    need_nudge = true;
                }
            }
            C2S_CREATE2 => {
                // The one-outcome contract arms as soon as the nonce and the
                // feature byte are decodable (docs/protocol.md, "Common status
                // registry").  A frame shorter than that cannot be correlated
                // to anything, so it stays a silent drop.
                if data.len() < 8 {
                    continue;
                }
                let nonce = u16::from_le_bytes([data[1], data[2]]);
                // Straight off the wire and straight into the grid allocation.
                let (rows, cols) = clamp_view_size(
                    u16::from_le_bytes([data[3], data[4]]),
                    u16::from_le_bytes([data[5], data[6]]),
                );
                let features = data[7];
                let want_status = features & CREATE2_WANT_STATUS != 0;
                if data.len() < 10 {
                    refuse_create(
                        &sess,
                        client_id,
                        want_status,
                        nonce,
                        STATUS_INVALID,
                        "truncated tag length",
                    );
                    continue;
                }
                let tag_len = u16::from_le_bytes([data[8], data[9]]) as usize;
                let tag = match create2_tag(&data) {
                    Ok(tag) => tag,
                    Err(detail) => {
                        refuse_create(&sess, client_id, want_status, nonce, STATUS_INVALID, detail);
                        continue;
                    }
                };
                let mut cursor = 10 + tag_len;
                let src_dir = if features & CREATE2_HAS_SRC_PTY != 0 && data.len() >= cursor + 2 {
                    let src_id = u16::from_le_bytes([data[cursor], data[cursor + 1]]);
                    cursor += 2;
                    sess.ptys.get(&src_id).and_then(|p| pty::pty_cwd(&p.handle))
                } else {
                    None
                };
                let explicit_dir = if features & CREATE2_HAS_CWD != 0 {
                    if data.len() < cursor + 2 {
                        refuse_create(
                            &sess,
                            client_id,
                            want_status,
                            nonce,
                            STATUS_INVALID,
                            "truncated cwd length",
                        );
                        continue;
                    }
                    let cwd_len = u16::from_le_bytes([data[cursor], data[cursor + 1]]) as usize;
                    cursor += 2;
                    if data.len() < cursor + cwd_len {
                        refuse_create(
                            &sess,
                            client_id,
                            want_status,
                            nonce,
                            STATUS_INVALID,
                            "truncated cwd",
                        );
                        continue;
                    }
                    let cwd = std::str::from_utf8(&data[cursor..cursor + cwd_len]).ok();
                    cursor += cwd_len;
                    cwd.filter(|p| !p.contains('\0'))
                        .map(str::trim)
                        .filter(|p| !p.is_empty())
                        .map(str::to_string)
                } else {
                    None
                };
                let dir = explicit_dir.or(src_dir);
                // Before the command, which has no length prefix and runs to
                // the end of the message.
                let deadline_ms = if features & CREATE2_HAS_DEADLINE != 0 {
                    if data.len() < cursor + 4 {
                        refuse_create(
                            &sess,
                            client_id,
                            want_status,
                            nonce,
                            STATUS_INVALID,
                            "truncated deadline",
                        );
                        continue;
                    }
                    let ms = u32::from_le_bytes([
                        data[cursor],
                        data[cursor + 1],
                        data[cursor + 2],
                        data[cursor + 3],
                    ]);
                    cursor += 4;
                    (ms > 0).then_some(ms)
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
                // A tag or command that cannot round-trip S2C_LIST's u16
                // length fields would truncate into a corrupt catalog frame
                // for every client, so refuse the mutation instead.
                if let Some(what) = oversize_list_field(tag, command) {
                    refuse_create(
                        &sess,
                        client_id,
                        want_status,
                        nonce,
                        STATUS_TOO_LARGE,
                        &format!("{what} exceeds 65535 bytes"),
                    );
                    continue;
                }
                let Some(id) = sess.allocate_pty_id(config.max_ptys) else {
                    refuse_create(
                        &sess,
                        client_id,
                        want_status,
                        nonce,
                        STATUS_BUDGET,
                        // The live count, matching what the cap actually
                        // tests — `ptys.len()` counts retained-exited slots
                        // too, so an id-space exhaustion under the cap would
                        // report itself as a cap the operator cannot raise
                        // their way out of.
                        &pty_budget_detail(sess.live_ptys(), config.max_ptys),
                    );
                    continue;
                };
                let socket_name = sess
                    .ensure_compositor(
                        config.verbose,
                        notify_for_compositor.clone(),
                        &config.vaapi_device,
                    )
                    .to_string();
                #[cfg(target_os = "linux")]
                let pulse_server = sess.pulse_server_path();
                #[cfg(not(target_os = "linux"))]
                let pulse_server: Option<String> = None;
                #[cfg(target_os = "linux")]
                let pipewire_remote = sess.pipewire_remote_path();
                #[cfg(not(target_os = "linux"))]
                let pipewire_remote: Option<String> = None;
                if let Some(pty) = pty::spawn_pty(
                    &config.shell,
                    &config.shell_flags,
                    rows,
                    cols,
                    id,
                    tag,
                    command,
                    argv.as_deref(),
                    dir.as_deref(),
                    config.scrollback,
                    state.clone(),
                    Some(&socket_name),
                    pulse_server.as_deref(),
                    pipewire_remote.as_deref(),
                ) {
                    let mut pty = pty;
                    // Armed before the terminal is reachable by anyone, so a
                    // client that dies immediately after creating it cannot
                    // leave an unbounded command behind.
                    pty.deadline =
                        deadline_ms.map(|ms| Instant::now() + Duration::from_millis(ms as u64));
                    let armed = pty.deadline.is_some();
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
                        reset_inflight(c);
                        let _ = send_outbox(c, nonce_msg);
                    }
                    for (&cid, c) in sess.clients.iter() {
                        if cid != client_id {
                            let _ = send_outbox(c, broadcast_msg.clone());
                        }
                    }
                    if armed {
                        state.supervisor_notify.notify_one();
                    }
                    need_nudge = true;
                } else {
                    // The id was handed out by allocate_pty_id but nothing
                    // was inserted, so it is free again on the next probe.
                    refuse_create(
                        &sess,
                        client_id,
                        want_status,
                        nonce,
                        STATUS_OTHER,
                        "failed to spawn terminal",
                    );
                }
            }
            C2S_SURFACE_INPUT if data.len() >= 8 => {
                let surface_id = u16::from_le_bytes([data[1], data[2]]);
                let keycode = u32::from_le_bytes([data[3], data[4], data[5], data[6]]);
                let pressed = data[7] != 0;
                if let Some(client) = sess.clients.get_mut(&client_id) {
                    if pressed {
                        client.pressed_surface_keys.insert(keycode);
                    } else {
                        client.pressed_surface_keys.remove(&keycode);
                    }
                }
                if let Some(cs) = sess.compositor.as_mut() {
                    let _ = cs.handle.command_tx.send(CompositorCommand::KeyInput {
                        surface_id,
                        keycode,
                        pressed,
                    });
                    cs.handle.wake();
                    state.delivery_notify.notify_one();
                }
            }
            C2S_SURFACE_TEXT if data.len() >= 3 => {
                let _surface_id = u16::from_le_bytes([data[1], data[2]]);
                if let Ok(text) = std::str::from_utf8(&data[3..])
                    && let Some(cs) = sess.compositor.as_mut()
                {
                    let _ = cs.handle.command_tx.send(CompositorCommand::TextInput {
                        text: text.to_string(),
                    });
                    cs.handle.wake();
                    state.delivery_notify.notify_one();
                }
            }
            C2S_SURFACE_POINTER if data.len() >= 9 => {
                let surface_id = u16::from_le_bytes([data[1], data[2]]);
                let ptype = data[3];
                let button = data[4];
                let x = u16::from_le_bytes([data[5], data[6]]) as f64;
                let y = u16::from_le_bytes([data[7], data[8]]) as f64;
                if let Some(cs) = sess.compositor.as_mut() {
                    match ptype {
                        0 | 1 => {
                            let _ = cs.handle.command_tx.send(CompositorCommand::PointerMotion {
                                surface_id,
                                x,
                                y,
                            });
                            let _ = cs.handle.command_tx.send(CompositorCommand::PointerButton {
                                surface_id,
                                button: match button {
                                    1 => 0x112,
                                    2 => 0x111,
                                    _ => 0x110,
                                },
                                pressed: ptype == 0,
                            });
                        }
                        2 => {
                            let _ = cs.handle.command_tx.send(CompositorCommand::PointerMotion {
                                surface_id,
                                x,
                                y,
                            });
                        }
                        _ => {}
                    }
                    cs.handle.wake();
                }
                state.delivery_notify.notify_one();
            }
            C2S_SURFACE_POINTER_AXIS if data.len() >= 8 => {
                // Legacy single-axis scroll. No source, so the compositor
                // emits no axis_source and the client is left to guess —
                // the behaviour this opcode always had.
                let surface_id = u16::from_le_bytes([data[1], data[2]]);
                let axis = data[3];
                let value =
                    f64::from(i32::from_le_bytes([data[4], data[5], data[6], data[7]])) / 100.0;
                let (dx, dy) = if axis == 0 {
                    (0.0, value)
                } else {
                    (value, 0.0)
                };
                if let Some(cs) = sess.compositor.as_mut() {
                    let _ = cs.handle.command_tx.send(CompositorCommand::PointerAxis {
                        surface_id,
                        dx,
                        dy,
                        v120_x: 0,
                        v120_y: 0,
                        source: None,
                        stop: false,
                    });
                    cs.handle.wake();
                }
                state.delivery_notify.notify_one();
            }
            C2S_SURFACE_POINTER_AXIS2 if data.len() >= SURFACE_POINTER_AXIS2_LEN => {
                if let Some(ev) = parse_surface_pointer_axis2(&data)
                    && let Some(cs) = sess.compositor.as_mut()
                {
                    let _ = cs.handle.command_tx.send(CompositorCommand::PointerAxis {
                        surface_id: ev.surface_id,
                        dx: ev.dx,
                        dy: ev.dy,
                        v120_x: ev.v120_x,
                        v120_y: ev.v120_y,
                        source: ev.source,
                        stop: ev.stop,
                    });
                    cs.handle.wake();
                }
                state.delivery_notify.notify_one();
            }
            C2S_SURFACE_RESIZE if data.len() >= 9 => {
                let surface_id = u16::from_le_bytes([data[1], data[2]]);
                let width = u16::from_le_bytes([data[3], data[4]]);
                let height = u16::from_le_bytes([data[5], data[6]]);
                // Scale in 1/120th units (Wayland convention): 240 = 2×.
                let scale_120 = u16::from_le_bytes([data[7], data[8]]);
                if state.config.verbose {
                    eprintln!(
                        "C2S_SURFACE_RESIZE: cid={client_id} sid={surface_id} {width}x{height} scale={scale_120}"
                    );
                }
                if let Some(c) = sess.clients.get_mut(&client_id) {
                    if is_unset_view_size(width, height) {
                        c.surface_view_sizes.remove(&surface_id);
                    } else if width > 0 && height > 0 {
                        c.surface_view_sizes
                            .insert(surface_id, (width, height, scale_120));
                    }
                    // Clear latched nal_data=None streak for this
                    // surface so the encoder can be recreated.  The
                    // streak is designed to stop infinite recreation
                    // loops (GBM fd leak), not to permanently black out
                    // a surface across a client-driven resize.
                    //
                    // Also wake the pacing gate: reset the burst window
                    // and clear next_send_at so the first frame after
                    // resize bypasses time-based pacing and flows at
                    // wire speed.  Without this, the client waits up to
                    // one send interval (~1/fps) after the encoder is
                    // recreated before seeing the first new frame.
                    let s = c.surface_subs.entry(surface_id).or_default();
                    s.nal_none_streak = 0;
                    s.nal_none_latched_at = None;
                    // The failures counted so far were about the size being
                    // replaced, and have no bearing on the new one.
                    s.create_failures = 0;
                    s.burst_remaining = SURFACE_BURST_FRAMES;
                    s.next_send_at = None;
                    s.has_keyframe = false;
                }
                if sess.resize_surfaces_to_mediated_sizes(
                    std::iter::once(surface_id),
                    &state.config.surface_encoders,
                    state.config.verbose,
                ) {
                    // The resize is held for the settle window, and only
                    // `tick` dispatches it.  On an otherwise idle session
                    // nothing else would run one, so the last size of a drag
                    // would sit undelivered until unrelated traffic arrived.
                    nudge_delivery(&state);
                }
            }
            C2S_SURFACE_FOCUS if data.len() >= 3 => {
                let surface_id = u16::from_le_bytes([data[1], data[2]]);
                if state.config.verbose {
                    eprintln!("C2S_SURFACE_FOCUS: cid={client_id} sid={surface_id}");
                }
                if let Some(cs) = sess.compositor.as_ref() {
                    let _ = cs
                        .handle
                        .command_tx
                        .send(CompositorCommand::SurfaceFocus { surface_id });
                }
            }
            C2S_SURFACE_CLOSE if data.len() >= 3 => {
                let surface_id = u16::from_le_bytes([data[1], data[2]]);
                if let Some(cs) = sess.compositor.as_ref() {
                    let _ = cs
                        .handle
                        .command_tx
                        .send(CompositorCommand::SurfaceClose { surface_id });
                    cs.handle.wake();
                }
            }
            C2S_SURFACE_SUBSCRIBE if data.len() >= 3 => {
                let surface_id = u16::from_le_bytes([data[1], data[2]]);
                // Extended fields (backward-compatible: absent = 0 = defaults).
                let codec_support = if data.len() >= 4 { data[3] } else { 0 };
                let bandwidth_wire = if data.len() >= 5 { data[4] } else { 0 };
                let speed_wire = if data.len() >= 6 { data[5] } else { 0 };
                // Scaled form: a fixed encode size for this client alone.
                // Absent or zero on either axis means mediated, which is what
                // every pre-scaled client sends.
                let scaled_target = if data.len() >= 10 {
                    let w = u16::from_le_bytes([data[6], data[7]]);
                    let h = u16::from_le_bytes([data[8], data[9]]);
                    (w > 0 && h > 0).then_some((w, h))
                } else {
                    None
                };
                if state.config.verbose {
                    eprintln!(
                        "C2S_SURFACE_SUBSCRIBE: cid={client_id} surface={surface_id} codec={codec_support:#04x} bandwidth={bandwidth_wire} speed={speed_wire} scaled={scaled_target:?}"
                    );
                }
                let mut destroy_vulkan_enc_sid = None;
                let mut first_subscribe = false;
                // Joining or leaving the scaled set changes who mediation
                // counts, so the surface has to be re-mediated even though
                // this client was already subscribed.
                let mut mediation_membership_changed = false;
                if let Some(c) = sess.clients.get_mut(&client_id) {
                    let congested = outbox_backpressured(c);
                    let was_subscribed = !c.surface_subscriptions.insert(surface_id);
                    let new_bandwidth = SurfaceBandwidth::from_wire(bandwidth_wire);
                    let new_speed = SurfaceSpeed::from_wire(speed_wire);

                    let state = c.surface_subs.entry(surface_id).or_default();
                    // A changed scaled target means a different encode size,
                    // so it invalidates the encoder exactly like the other
                    // three preferences do.
                    let prefs_changed = codec_support != state.codec_override
                        || new_bandwidth != state.bandwidth_override
                        || new_speed != state.speed_override
                        || scaled_target != state.scaled_target;

                    // A no-op resubscribe (same codec/bandwidth/speed,
                    // already subscribed) should not disturb the steady
                    // encode stream — resetting needs_keyframe/burst on
                    // every repeated subscribe makes keyframes churn and
                    // skews pacing.
                    let meaningful_change = !was_subscribed || prefs_changed;
                    mediation_membership_changed =
                        state.scaled_target.is_some() != scaled_target.is_some();
                    state.codec_override = codec_support;
                    state.bandwidth_override = new_bandwidth;
                    state.speed_override = new_speed;
                    state.scaled_target = scaled_target;
                    if prefs_changed {
                        // The encoder is about to be rebuilt and may not come
                        // back as the same backend — a client that just gained
                        // AV1 support has a different chain now.  Drop what we
                        // learned so sizing re-derives it instead of holding
                        // the surface to the old winner's ceiling.
                        state.selected_encoder = None;
                        state.encoder_cap_degraded = false;
                        // The tally that justified narrowing goes with it,
                        // or the very first failure on the new chain would
                        // narrow again on the strength of the old one's.
                        state.create_failures = 0;
                    }
                    let task_in_flight = state.encode_in_flight || state.creation_in_flight;
                    if meaningful_change {
                        // Reset burst window so the first frames after a
                        // (re)subscribe bypass time-based pacing and flow
                        // at wire speed.  Clear the nal_data=None streak
                        // too: a fresh subscription is a valid signal to
                        // retry a previously-latched encoder.
                        //
                        // Not while congested, though: a client whose
                        // decoder is struggling resubscribes to ask for a
                        // keyframe, and granting an unpaced burst then
                        // answers a congested link with the largest frames
                        // the encoder can produce.
                        if !congested {
                            state.burst_remaining = SURFACE_BURST_FRAMES;
                        }
                        state.nal_none_streak = 0;
                        state.nal_none_latched_at = None;
                    }
                    // Force encoder recreation when preferences change on
                    // resubscribe.  If an encode OR creation is in flight,
                    // flag the completion handler to discard its encoder
                    // instead of installing the stale one.
                    if was_subscribed && prefs_changed {
                        state.encoder = None;
                        if task_in_flight {
                            state.encoder_invalidated = true;
                        }
                    }
                    if meaningful_change {
                        state.has_keyframe = false;
                    }
                    first_subscribe = !was_subscribed;
                    if was_subscribed
                        && prefs_changed
                        && c.vulkan_video_surfaces.remove(&surface_id).is_some()
                    {
                        destroy_vulkan_enc_sid = Some(surface_id);
                    }
                }
                if let Some(sid) = destroy_vulkan_enc_sid
                    && let Some(cs) = sess.compositor.as_mut()
                {
                    cs.last_encoded.remove(&(sid, client_id));
                    let _ = cs.handle.command_tx.send(
                        blit_compositor::CompositorCommand::DestroyVulkanEncoder {
                            surface_id: sid as u32,
                            client_id: Some(client_id),
                        },
                    );
                    cs.handle.wake();
                }
                if first_subscribe || mediation_membership_changed {
                    sess.resize_surfaces_to_mediated_sizes(
                        std::iter::once(surface_id),
                        &state.config.surface_encoders,
                        state.config.verbose,
                    );
                }
                state.delivery_notify.notify_one();
            }
            C2S_SURFACE_UNSUBSCRIBE if data.len() >= 3 => {
                let surface_id = u16::from_le_bytes([data[1], data[2]]);
                let mut removed_vulkan = false;
                let mut clear_target: Option<(u32, u32)> = None;
                if let Some(c) = sess.clients.get_mut(&client_id) {
                    clear_target = c
                        .surface_subs
                        .get(&surface_id)
                        .and_then(|s| s.last_registered_target);
                    c.surface_subscriptions.remove(&surface_id);
                    c.surface_subs.remove(&surface_id);
                    forget_surface_inflight(c, surface_id);
                    removed_vulkan = c.vulkan_video_surfaces.remove(&surface_id).is_some();
                    c.surface_view_sizes.remove(&surface_id);
                }
                // Drop the per-client downscale target this client had
                // registered so the compositor stops blitting into a
                // BGRA buffer no encoder will ever read — unless someone
                // else is registered at the same size, in which case the
                // target is still theirs.
                //
                // Whoever is left also decides afresh whether it can take
                // NV12: a subscriber needing CPU pixels forces the whole
                // size onto BGRA, and when it leaves the remaining NVENC
                // readers should get the zero-copy path back rather than
                // stay on BGRA until something else happens to
                // re-register.
                if let Some((tw, th)) = clear_target {
                    sess.resettle_downscale_target(surface_id, tw, th);
                }
                // Destroy this client's Vulkan Video encoder.  Ownership is
                // per `(surface, client)`, so no refcount sweep over the
                // other subscribers is needed — theirs are untouched.
                if removed_vulkan && let Some(cs) = sess.compositor.as_mut() {
                    cs.last_encoded.remove(&(surface_id, client_id));
                    let _ = cs.handle.command_tx.send(
                        blit_compositor::CompositorCommand::DestroyVulkanEncoder {
                            surface_id: surface_id as u32,
                            client_id: Some(client_id),
                        },
                    );
                    cs.handle.wake();
                }
                if sess.resize_surfaces_to_mediated_sizes(
                    std::iter::once(surface_id),
                    &state.config.surface_encoders,
                    state.config.verbose,
                ) {
                    // As above: losing a subscriber raises the mediated size,
                    // and that resize can land inside an open settle window.
                    nudge_delivery(&state);
                }
            }
            #[cfg(target_os = "linux")]
            C2S_AUDIO_SUBSCRIBE if data.len() >= 3 => {
                let bitrate_kbps = u16::from_le_bytes([data[1], data[2]]);
                let audio_tx = sess.clients.get(&client_id).map(|c| c.audio_tx.clone());
                if let Some(c) = sess.clients.get_mut(&client_id) {
                    c.audio_subscribed = true;
                    c.audio_bitrate_kbps = bitrate_kbps;
                    if state.config.verbose {
                        eprintln!(
                            "C2S_AUDIO_SUBSCRIBE: cid={client_id} bitrate_kbps={bitrate_kbps}"
                        );
                    }
                }
                // Register with the audio broadcast — atomically enqueues
                // catch-up frames and registers for live frames from the
                // fan-out task.  Succeeds even if the pipeline itself is
                // currently down (frames resume once it's respawned).
                if let (Some(cs), Some(tx)) = (sess.compositor.as_ref(), audio_tx) {
                    cs.audio_broadcast.subscribe(client_id, tx);
                }
                // Recompute the effective audio bitrate across all
                // subscribed clients (use the max requested bitrate).
                if let Some(cs) = sess.compositor.as_ref()
                    && let Some(ref ap) = cs.audio_pipeline
                {
                    let max_kbps = sess
                        .clients
                        .values()
                        .filter(|c| c.audio_subscribed)
                        .map(|c| c.audio_bitrate_kbps)
                        .max()
                        .unwrap_or(0);
                    let bitrate = if max_kbps > 0 {
                        max_kbps as i32 * 1000
                    } else {
                        audio::DEFAULT_BITRATE
                    };
                    ap.set_bitrate(bitrate);
                }
                state.delivery_notify.notify_one();
            }
            #[cfg(target_os = "linux")]
            C2S_AUDIO_UNSUBSCRIBE if !data.is_empty() => {
                if let Some(c) = sess.clients.get_mut(&client_id) {
                    c.audio_subscribed = false;
                    c.audio_bitrate_kbps = 0;
                    if state.config.verbose {
                        eprintln!("C2S_AUDIO_UNSUBSCRIBE: cid={client_id}");
                    }
                }
                if let Some(cs) = sess.compositor.as_ref() {
                    cs.audio_broadcast.unsubscribe(client_id);
                }
                // Recompute effective bitrate after unsubscribe.
                if let Some(cs) = sess.compositor.as_ref()
                    && let Some(ref ap) = cs.audio_pipeline
                {
                    let max_kbps = sess
                        .clients
                        .values()
                        .filter(|c| c.audio_subscribed)
                        .map(|c| c.audio_bitrate_kbps)
                        .max()
                        .unwrap_or(0);
                    let bitrate = if max_kbps > 0 {
                        max_kbps as i32 * 1000
                    } else {
                        audio::DEFAULT_BITRATE
                    };
                    ap.set_bitrate(bitrate);
                }
            }
            C2S_SURFACE_ACK if data.len() >= 3 => {
                // Surface ACKs feed shared RTT / delivery_bps / goodput_bps
                // from a separate inflight queue so they don't corrupt
                // terminal frame-size averages or probe_frames.
                let surface_id = u16::from_le_bytes([data[1], data[2]]);
                if let Some(c) = sess.clients.get_mut(&client_id) {
                    c.acks_recv += 1;
                    record_surface_ack(c, surface_id);
                }
                state.delivery_notify.notify_one();
            }
            C2S_CLIENT_FEATURES if data.len() >= 2 => {
                // Byte 0: codec_support bitmask.  Bytes 1..5, when present:
                // the largest frame the client's decoder handles, as two
                // little-endian u16s.  Absent (older clients) leaves it at
                // (0, 0), which `surface_encode_cap` reads as "undeclared"
                // and holds to the H.264 ceiling.  Further bytes are still
                // ignored if unknown.
                let codec_support = data[1];
                let max_decode = if data.len() >= 6 {
                    (
                        u16::from_le_bytes([data[2], data[3]]),
                        u16::from_le_bytes([data[4], data[5]]),
                    )
                } else {
                    (0, 0)
                };
                if let Some(c) = sess.clients.get_mut(&client_id) {
                    c.surface_codec_support = codec_support;
                    c.surface_max_decode = max_decode;
                }
            }
            C2S_CLIPBOARD_SET if data.len() >= 5 => {
                let mime_len = u16::from_le_bytes([data[1], data[2]]) as usize;
                if data.len() >= 3 + mime_len + 4 {
                    let mime = std::str::from_utf8(&data[3..3 + mime_len])
                        .unwrap_or("text/plain")
                        .to_string();
                    let data_len = u32::from_le_bytes([
                        data[3 + mime_len],
                        data[4 + mime_len],
                        data[5 + mime_len],
                        data[6 + mime_len],
                    ]) as usize;
                    let payload_start = 7 + mime_len;
                    if data.len() >= payload_start + data_len {
                        let payload = data[payload_start..payload_start + data_len].to_vec();
                        if let Some(cs) = sess.compositor.as_ref() {
                            let _ = cs
                                .handle
                                .command_tx
                                .send(CompositorCommand::ClipboardOffer {
                                    mime_type: mime,
                                    data: payload,
                                });
                        }
                    }
                }
            }
            C2S_CLIPBOARD_LIST if !data.is_empty() => {
                if let Some(cs) = sess.compositor.as_ref() {
                    let command_tx = cs.handle.command_tx.clone();
                    let client_tx = sess.clients.get(&client_id).map(|c| {
                        (
                            c.tx.clone(),
                            c.outbox_queued_frames.clone(),
                            c.outbox_queued_bytes.clone(),
                        )
                    });
                    if let Some((client_tx, queued_frames, queued_bytes)) = client_tx {
                        tokio::task::spawn_blocking(move || {
                            let (tx, rx) = std::sync::mpsc::sync_channel(1);
                            if command_tx
                                .send(CompositorCommand::ClipboardListMimes { reply: tx })
                                .is_ok()
                                && let Ok(mimes) = rx.recv_timeout(Duration::from_secs(2))
                            {
                                let _ = send_outbox_tracked(
                                    &client_tx,
                                    &queued_frames,
                                    &queued_bytes,
                                    msg_s2c_clipboard_list(&mimes),
                                );
                            }
                        });
                    }
                } else {
                    // No compositor — respond with empty list.
                    if let Some(c) = sess.clients.get(&client_id) {
                        let _ = send_outbox(c, msg_s2c_clipboard_list(&[]));
                    }
                }
            }
            C2S_CLIPBOARD_GET if data.len() >= 3 => {
                let mime_len = u16::from_le_bytes([data[1], data[2]]) as usize;
                if data.len() >= 3 + mime_len {
                    let mime = std::str::from_utf8(&data[3..3 + mime_len])
                        .unwrap_or("text/plain")
                        .to_string();
                    if let Some(cs) = sess.compositor.as_ref() {
                        let command_tx = cs.handle.command_tx.clone();
                        let client_tx = sess.clients.get(&client_id).map(|c| {
                            (
                                c.tx.clone(),
                                c.outbox_queued_frames.clone(),
                                c.outbox_queued_bytes.clone(),
                            )
                        });
                        if let Some((client_tx, queued_frames, queued_bytes)) = client_tx {
                            tokio::task::spawn_blocking(move || {
                                let (tx, rx) = std::sync::mpsc::sync_channel(1);
                                if command_tx
                                    .send(CompositorCommand::ClipboardGet {
                                        mime_type: mime.clone(),
                                        reply: tx,
                                    })
                                    .is_ok()
                                    && let Ok(content) = rx.recv_timeout(Duration::from_secs(2))
                                {
                                    let data = content.unwrap_or_default();
                                    let _ = send_outbox_tracked(
                                        &client_tx,
                                        &queued_frames,
                                        &queued_bytes,
                                        msg_s2c_clipboard_content(&mime, &data),
                                    );
                                }
                            });
                        }
                    } else {
                        // No compositor — respond with empty clipboard.
                        if let Some(c) = sess.clients.get(&client_id) {
                            let _ = send_outbox(c, msg_s2c_clipboard_content(&mime, &[]));
                        }
                    }
                }
            }
            C2S_SURFACE_LIST if !data.is_empty() => {
                let msg = sess.surface_list_msg();
                if let Some(c) = sess.clients.get(&client_id) {
                    let _ = send_outbox(c, msg);
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
                let restart_info = sess.ptys.get(&pid).filter(|p| p.exited).map(|p| {
                    (
                        p.driver.size(),
                        p.command.clone(),
                        p.cwd.clone(),
                        p.tag.clone(),
                    )
                });
                if let Some(((rows, cols), command, cwd, tag)) = restart_info {
                    let wayland_display = sess
                        .compositor
                        .as_ref()
                        .map(|cs| cs.handle.socket_name.clone());
                    #[cfg(target_os = "linux")]
                    let pulse_server = sess.pulse_server_path();
                    #[cfg(not(target_os = "linux"))]
                    let pulse_server: Option<String> = None;
                    #[cfg(target_os = "linux")]
                    let pipewire_remote = sess.pipewire_remote_path();
                    #[cfg(not(target_os = "linux"))]
                    let pipewire_remote: Option<String> = None;
                    if let Some((new_handle, reader, byte_rx)) = pty::respawn_child(
                        &state.config.shell,
                        &state.config.shell_flags,
                        rows,
                        cols,
                        pid,
                        command.as_deref(),
                        cwd.as_deref(),
                        state.clone(),
                        wayland_display.as_deref(),
                        pulse_server.as_deref(),
                        pipewire_remote.as_deref(),
                    ) {
                        let Some(pty) = sess.ptys.get_mut(&pid) else {
                            break;
                        };
                        pty.handle = new_handle;
                        pty.reader_handle = reader;
                        pty.byte_rx = byte_rx;
                        pty.driver.reset_modes();
                        pty.exited = false;
                        pty.exited_at = None;
                        // New child in the same slot: anything queued against
                        // the old one must not land on this one.
                        pty.generation = pty.generation.wrapping_add(1);
                        pty.exit_status = blit_remote::EXIT_STATUS_UNKNOWN;
                        // A restart is a new command, not a continuation of
                        // the one the deadline was armed for.  Carrying the
                        // old attribution over would blame this exit on a
                        // deadline that already fired.
                        pty.deadline = None;
                        pty.stop_deadline = None;
                        pty.exit_reason = blit_remote::EXIT_REASON_NORMAL;
                        // The fresh shell hasn't reported OSC 7 yet; keeping
                        // the dead shell's cwd would shadow the kernel
                        // fallback in C2S_TERM_CWD with stale data.
                        pty.osc7_cwd = None;
                        pty.lflag_cache = pty::pty_lflag(&pty.handle);
                        pty.lflag_last = Instant::now();
                        pty.mark_dirty();
                        if let Some(c) = sess.clients.get_mut(&client_id) {
                            c.lead = Some(pid);
                            subscribe_client_to(c, pid);
                            update_client_scroll_state(c, pid, 0);
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
            C2S_TERM_CWD if data.len() >= 5 => {
                let nonce = u16::from_le_bytes([data[1], data[2]]);
                let pid = u16::from_le_bytes([data[3], data[4]]);
                let cwd = sess
                    .ptys
                    .get(&pid)
                    .and_then(|p| {
                        // Precedence rationale: see `resolve_term_cwd`.
                        resolve_term_cwd(p.osc7_cwd.as_deref(), || pty::pty_cwd(&p.handle))
                    })
                    .unwrap_or_default();
                if let Some(client) = sess.clients.get(&client_id) {
                    let _ = send_outbox(client, msg_term_cwd_reply(nonce, &cwd));
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

                    let mut all_lines: Vec<String> =
                        Vec::with_capacity(scrollback_lines + rows as usize);

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
                        let _ = send_outbox(client, msg);
                    }
                }
            }
            C2S_COPY_RANGE if data.len() >= 18 => {
                let nonce = u16::from_le_bytes([data[1], data[2]]);
                let pid = u16::from_le_bytes([data[3], data[4]]);
                let start_tail = u32::from_le_bytes([data[5], data[6], data[7], data[8]]);
                let start_col = u16::from_le_bytes([data[9], data[10]]);
                let end_tail = u32::from_le_bytes([data[11], data[12], data[13], data[14]]);
                let end_col = u16::from_le_bytes([data[15], data[16]]);

                if let Some(pty) = sess.ptys.get(&pid) {
                    let text = pty
                        .driver
                        .get_text_range(start_tail, start_col, end_tail, end_col);
                    let total_lines = pty.driver.total_lines();

                    let mut msg = Vec::with_capacity(13 + text.len());
                    msg.push(S2C_TEXT);
                    msg.extend_from_slice(&nonce.to_le_bytes());
                    msg.extend_from_slice(&pid.to_le_bytes());
                    msg.extend_from_slice(&total_lines.to_le_bytes());
                    msg.extend_from_slice(&start_tail.to_le_bytes());
                    msg.extend_from_slice(text.as_bytes());
                    if let Some(client) = sess.clients.get(&client_id) {
                        let _ = send_outbox(client, msg);
                    }
                }
            }
            C2S_DEADLINE if data.len() >= 7 => {
                let pid = u16::from_le_bytes([data[1], data[2]]);
                let ms = u32::from_le_bytes([data[3], data[4], data[5], data[6]]);
                if let Some(pty) = sess.ptys.get_mut(&pid)
                    && !pty.exited
                {
                    // Measured from now, so re-sending refreshes — that is
                    // what makes this a dead-man switch rather than a
                    // one-shot timeout.
                    let (deadline, stop, reason) = armed_deadline(Instant::now(), ms);
                    pty.deadline = deadline;
                    pty.stop_deadline = stop;
                    pty.exit_reason = reason;
                    state.supervisor_notify.notify_one();
                }
            }
            C2S_KILL if data.len() >= 7 => {
                let pid = u16::from_le_bytes([data[1], data[2]]);
                let signal = i32::from_le_bytes([data[3], data[4], data[5], data[6]]);
                // The flags byte is optional; a 7-byte message means the
                // default, which is now the whole group.
                let leader_only = data.len() >= 8 && data[7] & KILL_LEADER_ONLY != 0;
                // `exited` gates this because a group kill on a reaped pid
                // would land on whatever recycled it — a wider blast radius
                // than the leader-only kill this replaces.
                if let Some(pty) = sess.ptys.get(&pid)
                    && !pty.exited
                {
                    pty::kill_pty(&pty.handle, signal, !leader_only);
                }
            }
            C2S_CLOSE if data.len() >= 3 => {
                let pid = u16::from_le_bytes([data[1], data[2]]);
                if let Some(pty) = sess.ptys.remove(&pid) {
                    if !pty.exited {
                        state.pty_fds.write().unwrap().remove(&pid);
                        drop(pty.reader_handle);
                        pty::close_pty(&pty.handle);
                        // The SIGHUP only asks the child to die, and nobody
                        // will ever collect its status — but it still has to
                        // be waited or it stays a zombie for the life of the
                        // server.  Hand it to the reaper to finish.
                        pty::abandon_pty_pid(&pty.handle);
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
        let mut sess = state.session.lock().await;
        let mut need_nudge = false;
        // Drop any audio subscription before removing the client so the
        // fan-out task doesn't hold a dead tx for the full mpsc-buffered
        // lifetime.
        #[cfg(target_os = "linux")]
        if let Some(cs) = sess.compositor.as_ref() {
            cs.audio_broadcast.unsubscribe(client_id);
        }
        let client = sess.clients.remove(&client_id);
        // Downscale targets this client had registered. A disconnect is the
        // usual way a subscriber leaves — clients rarely send an explicit
        // unsubscribe first — so without re-deciding here, a target that
        // went to BGRA for a departed CPU-pixel reader would stay there.
        let departed_targets: Vec<(u16, (u32, u32))> = client
            .as_ref()
            .map(|c| {
                c.surface_subs
                    .iter()
                    .filter_map(|(sid, s)| s.last_registered_target.map(|t| (*sid, t)))
                    .collect()
            })
            .unwrap_or_default();
        for (sid, (tw, th)) in departed_targets {
            sess.resettle_downscale_target(sid, tw, th);
        }
        let affected_ptys = client
            .as_ref()
            .map(|c| c.view_sizes.keys().copied().collect::<Vec<_>>())
            .unwrap_or_default();
        let affected_surfaces = client
            .as_ref()
            .map(|c| c.surface_view_sizes.keys().copied().collect::<Vec<_>>())
            .unwrap_or_default();
        if sess.resize_ptys_to_mediated_sizes(affected_ptys) {
            need_nudge = true;
        }
        if sess.resize_surfaces_to_mediated_sizes(
            affected_surfaces,
            &state.config.surface_encoders,
            state.config.verbose,
        ) {
            need_nudge = true;
        }
        // Release any keys this client was holding when it disconnected.
        // Without this, modifier keys (Shift, Ctrl, etc.) stay stuck and
        // regular keys auto-repeat forever in the compositor.
        if let Some(ref client) = client
            && !client.pressed_surface_keys.is_empty()
            && let Some(cs) = sess.compositor.as_mut()
        {
            let keycodes: Vec<u32> = client.pressed_surface_keys.iter().copied().collect();
            let _ = cs
                .handle
                .command_tx
                .send(CompositorCommand::ReleaseKeys { keycodes });
            cs.handle.wake();
        }
        // Destroy Vulkan Video encoders for surfaces that no remaining
        // client needs.
        if let Some(ref client) = client
            && !client.vulkan_video_surfaces.is_empty()
        {
            let sids: Vec<u16> = client.vulkan_video_surfaces.keys().copied().collect();
            if let Some(cs) = sess.compositor.as_mut() {
                for sid in sids {
                    cs.last_encoded.remove(&(sid, client_id));
                    let _ = cs
                        .handle
                        .command_tx
                        .send(CompositorCommand::DestroyVulkanEncoder {
                            surface_id: sid as u32,
                            client_id: Some(client_id),
                        });
                }
                cs.handle.wake();
            }
        }
        drop(sess);
        if need_nudge {
            nudge_delivery(&state);
        }
    }
    sender.abort();
    // Relayed sockets outlive the read loop only as spawned tasks; drop the
    // table so every forwarded socket on this connection closes with it.
    net::shutdown(&mut net_sockets);
    if state.config.verbose {
        eprintln!("client disconnected");
    }
}

#[cfg(test)]
mod tests {

    /// The NV12 OPAQUE_FD buffer is GPU-only memory published under a
    /// single (surface, w, h) key, so it is only safe when *every*
    /// subscriber at that size can import it. These pin the rule that
    /// stops a software encoder being handed a handle it cannot map —
    /// which reaches the viewer as a black picture, not as an error.
    mod nv12_opaque_target {
        use super::super::nv12_opaque_safe_for_target;

        const T: (u32, u32) = (1280, 720);

        #[test]
        fn sole_nvenc_subscriber_gets_it() {
            assert!(nv12_opaque_safe_for_target(true, T, std::iter::empty()));
        }

        #[test]
        fn a_non_nvenc_encoder_at_the_same_size_rules_it_out() {
            assert!(!nv12_opaque_safe_for_target(
                true,
                T,
                [(Some(T), false)].into_iter()
            ));
        }

        #[test]
        fn all_nvenc_subscribers_keep_it() {
            assert!(nv12_opaque_safe_for_target(
                true,
                T,
                [(Some(T), true), (Some(T), true)].into_iter()
            ));
        }

        #[test]
        fn a_dissenter_at_another_size_is_irrelevant() {
            // It reads its own (sid, w, h) key, which still carries BGRA.
            assert!(nv12_opaque_safe_for_target(
                true,
                T,
                [(Some((640, 360)), false), (None, false)].into_iter()
            ));
        }

        #[test]
        fn one_dissenter_among_many_is_enough() {
            assert!(!nv12_opaque_safe_for_target(
                true,
                T,
                [(Some(T), true), (Some(T), false), (Some(T), true)].into_iter()
            ));
        }

        #[test]
        fn a_non_nvenc_encoder_never_asks_for_it() {
            assert!(!nv12_opaque_safe_for_target(
                false,
                T,
                [(Some(T), true)].into_iter()
            ));
        }
    }
    use super::*;

    /// The index walk (docs/design/fs-search.md) prunes `.git`, honors
    /// `.gitignore`, keeps other dotfiles, sorts, and reports truncation.
    #[test]
    fn fs_index_walk_honors_gitignore_and_budget() {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.subsec_nanos())
            .unwrap_or(0);
        let root =
            std::env::temp_dir().join(format!("blit_fsindex_{}_{}", std::process::id(), nanos));
        std::fs::create_dir_all(root.join("sub")).unwrap();
        std::fs::create_dir_all(root.join("target")).unwrap();
        // A bare `.git` dir marks the tree as a repository, which is what
        // makes the walker apply `.gitignore`.
        std::fs::create_dir_all(root.join(".git")).unwrap();
        std::fs::write(root.join(".gitignore"), "/target\n").unwrap();
        std::fs::write(root.join("a.txt"), "a").unwrap();
        std::fs::write(root.join(".hidden"), "h").unwrap();
        std::fs::write(root.join("sub/b.txt"), "b").unwrap();
        std::fs::write(root.join("target/junk.txt"), "j").unwrap();
        std::fs::write(root.join(".git/config"), "c").unwrap();

        let (paths, truncated) = fs_index_walk(&root, 100);
        assert!(!truncated);
        assert_eq!(paths, vec![".gitignore", ".hidden", "a.txt", "sub/b.txt"]);

        let (some, truncated) = fs_index_walk(&root, 2);
        assert!(truncated);
        assert!(some.len() <= 2);

        // Exactly-at-budget is complete, not truncated: nothing was dropped.
        let (all, truncated) = fs_index_walk(&root, 4);
        assert!(!truncated);
        assert_eq!(all.len(), 4);

        // The search fallback scores over the same candidate set.
        let hits = fs_search_walk(&root.to_string_lossy(), "btxt", 10);
        assert_eq!(hits, vec!["sub/b.txt"]);

        // A file root lists nothing (no bogus "" record).
        let (none, _) = fs_index_walk(&root.join("a.txt"), 100);
        assert!(none.is_empty());

        // A tree blanked by a glob ignore (`*`, the dotfiles-repo pattern)
        // falls back to the ignore-free walk instead of reporting empty.
        let blanked = root.join("blanked");
        std::fs::create_dir_all(blanked.join(".git")).unwrap();
        std::fs::write(blanked.join(".gitignore"), "*\n").unwrap();
        std::fs::write(blanked.join("kept.txt"), "k").unwrap();
        let (paths, truncated) = fs_index_walk(&blanked, 100);
        assert!(!truncated);
        assert_eq!(paths, vec![".gitignore", "kept.txt"]);

        let _ = std::fs::remove_dir_all(&root);
    }

    /// End-to-end for the `FROM_PTY` open family (docs/ide.md Decision 3):
    /// spawn a real shell, `cd` it into a fresh directory, then drive the
    /// exact chain the dispatch layer runs — `pty_cwd` on the live child,
    /// `*_rebase` onto that cwd, and `validate_root` / `blit_git::open` on
    /// the effective path — asserting each resolves to the terminal's cwd.
    #[cfg(unix)]
    #[test]
    fn from_pty_resolves_cwd_after_cd_e2e() {
        use std::process::Command;
        use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

        // A fresh directory the child will `cd` into.
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.subsec_nanos())
            .unwrap_or(0);
        let base =
            std::env::temp_dir().join(format!("blit_frompty_{}_{}", std::process::id(), nanos));
        std::fs::create_dir_all(&base).expect("create temp dir");
        let dir = std::fs::canonicalize(&base).expect("canonicalize temp dir");
        let dir_str = dir.to_str().unwrap().to_string();

        // A real shell that `cd`s into `dir` then `exec`s a long sleep, so
        // the process' cwd is `dir` for the test. The pid is stable across
        // exec, so `pty_cwd` reads `dir` straight from the kernel.
        let mut child = Command::new("/bin/sh")
            .arg("-c")
            .arg(r#"cd "$0" && exec sleep 60"#)
            .arg(&dir_str)
            .spawn()
            .expect("spawn shell");
        let pid = child.id() as libc::pid_t;

        // `pty_cwd` reads only `child_pid`; `master_fd` is unused here, and
        // `PtyHandle` has no `Drop`, so a hand-built handle is side-effect free.
        let handle = crate::pty::PtyHandle {
            master_fd: -1,
            child_pid: pid,
        };

        // Poll until the cd + exec has landed (bounded so CI never hangs).
        let deadline = Instant::now() + Duration::from_secs(5);
        let cwd = loop {
            if let Some(cwd) = crate::pty::pty_cwd(&handle)
                .filter(|c| std::fs::canonicalize(c).ok().as_ref() == Some(&dir))
            {
                break cwd;
            }
            assert!(
                Instant::now() < deadline,
                "pty_cwd never resolved to the cd'd directory"
            );
            std::thread::sleep(Duration::from_millis(40));
        };
        assert_eq!(std::fs::canonicalize(&cwd).unwrap(), dir);

        // fs: the dispatch rebases a FROM_PTY sync onto the resolved cwd,
        // then `validate_root`s the effective (empty => the cwd) path.
        let sync = blit_remote::fs::msg_fs_sync_from_pty(
            1,
            blit_remote::fs::FS_SYNC_RECURSIVE,
            0,
            0,
            "",
            1,
        );
        let rebased = blit_remote::fs::fs_sync_rebase(&sync, &cwd).expect("fs rebase");
        assert_eq!(
            blit_remote::fs::fs_sync_flags(&rebased).unwrap() & blit_remote::fs::FS_SYNC_FROM_PTY,
            0
        );
        let header = blit_remote::fs::FS_SYNC_HEADER;
        let plen = u16::from_le_bytes([rebased[11], rebased[12]]) as usize;
        let eff = std::str::from_utf8(&rebased[header..header + plen]).unwrap();
        let root = blit_fssync::validate_root(eff).expect("validate_root");
        assert_eq!(root, dir, "fs sync resolves to the terminal's cwd");

        // git: `cd`-ing into a repo lets a FROM_PTY open discover it from cwd.
        let git_ok = Command::new("git")
            .args(["init", "-q"])
            .current_dir(&dir)
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if git_ok {
            let gopen = blit_remote::git::msg_git_open(&blit_remote::git::GitOpenRequest {
                src_pty_id: 1,
                ..blit_remote::git::GitOpenRequest::new(2, blit_remote::git::GIT_OPEN_WATCH, "")
            });
            let greb = blit_remote::git::git_open_rebase(&gopen, &cwd).expect("git rebase");
            let rebased = blit_remote::git::parse_git_open(&greb).unwrap();
            assert_eq!(
                rebased.src_pty_id,
                blit_remote::git::GIT_OPEN_NO_CONTEXT,
                "rebase consumes the pty context"
            );
            let gpath = rebased.path;
            let (_, info) = blit_git::open(gpath).expect("git open at cwd");
            assert_eq!(
                std::fs::canonicalize(&info.workdir).unwrap(),
                dir,
                "git open resolves the repo at the terminal's cwd"
            );
        }

        let _ = child.kill();
        let _ = child.wait();
        let _ = std::fs::remove_dir_all(&base);
    }

    fn test_client_with_capacity(
        _capacity: usize,
    ) -> (ClientState, mpsc::UnboundedReceiver<Vec<u8>>) {
        let (tx, rx) = mpsc::unbounded_channel();
        #[cfg(target_os = "linux")]
        let (audio_tx, _audio_rx) = mpsc::unbounded_channel();
        let client = ClientState {
            tx,
            outbox_queued_frames: Arc::new(AtomicUsize::new(0)),
            outbox_queued_bytes: Arc::new(AtomicUsize::new(0)),
            write_blocked_us: Arc::new(AtomicU64::new(0)),
            write_blocked_us_seen: 0,
            #[cfg(target_os = "linux")]
            audio_tx,
            lead: None,
            subscriptions: HashSet::new(),
            surface_subscriptions: HashSet::new(),
            #[cfg(target_os = "linux")]
            audio_subscribed: false,
            #[cfg(target_os = "linux")]
            audio_bitrate_kbps: 0,
            view_sizes: HashMap::new(),
            scroll_offsets: HashMap::new(),
            scroll_caches: HashMap::new(),
            last_sent: HashMap::new(),
            last_used_rows_sent: HashMap::new(),
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
            avg_surface_frame_bytes: 8_192.0,
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
            last_window_blocked_log: Instant::now(),
            last_skip_log: Instant::now(),
            skip_same_gen_count: 0,
            skip_in_flight_count: 0,
            skip_pacing_count: 0,
            skip_vulkan_await_count: 0,
            skip_no_subs_count: 0,
            skip_not_subbed_count: 0,
            skip_last_pixels_mismatch_count: 0,
            encode_loop_iters: 0,
            goodput_window_bytes: 0,
            goodput_window_start: Instant::now(),
            surface_subs: HashMap::new(),
            surface_inflight_frames: VecDeque::new(),
            vulkan_video_surfaces: HashMap::new(),
            surface_view_sizes: HashMap::new(),
            surface_codec_support: 0,
            surface_max_decode: (0, 0),
            pressed_surface_keys: HashSet::new(),
        };
        (client, rx)
    }

    fn test_client() -> ClientState {
        let (client, _rx) = test_client_with_capacity(0);
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

    /// Full fs-sync flow through the connection-level handler: SYNC →
    /// GIT_REPO + first GIT_STATE, object reads, cancel no-op, close.
    #[tokio::test]
    async fn git_message_flow() {
        use blit_remote::git::*;
        use std::process::Command;

        let dir = std::env::temp_dir()
            .join(format!("blit-server-git-test-{}", std::process::id()))
            .canonicalize_or_create();
        let git = |args: &[&str]| {
            let ok = Command::new("git")
                .current_dir(&dir)
                .env("GIT_AUTHOR_NAME", "T")
                .env("GIT_AUTHOR_EMAIL", "t@example.com")
                .env("GIT_COMMITTER_NAME", "T")
                .env("GIT_COMMITTER_EMAIL", "t@example.com")
                .args(args)
                .output()
                .unwrap()
                .status
                .success();
            assert!(ok, "git {args:?}");
        };
        git(&["init", "-b", "main"]);
        std::fs::write(dir.join("f.txt"), b"hello\n").unwrap();
        git(&["add", "."]);
        git(&["commit", "-m", "first"]);

        let (out, mut rx) = mpsc::unbounded_channel::<Vec<u8>>();
        let mut repos = GitRepos::default();
        let wait_msg = |rx: &mut mpsc::UnboundedReceiver<Vec<u8>>, opcode: u8| -> Vec<u8> {
            let deadline = std::time::Instant::now() + Duration::from_secs(10);
            loop {
                match rx.try_recv() {
                    Ok(msg) if msg[0] == opcode => return msg,
                    Ok(_) => continue,
                    Err(_) => {
                        assert!(
                            std::time::Instant::now() < deadline,
                            "opcode {opcode:#x} never arrived"
                        );
                        std::thread::sleep(Duration::from_millis(10));
                    }
                }
            }
        };

        // A bad path refuses with the sentinel repo id.
        handle_git_message(
            &msg_git_open(&GitOpenRequest::new(1, 0, "/blit-no-such-path")),
            &mut repos,
            &out,
            false,
        )
        .await;
        let refusal_msg = rx.try_recv().expect("synchronous refusal");
        let refusal = parse_git_repo(&refusal_msg).unwrap();
        assert_eq!(refusal.repo_id, GIT_REPO_ID_INVALID);

        // Open with state streaming; GIT_REPO precedes the first GIT_STATE.
        handle_git_message(
            &msg_git_open(&GitOpenRequest::new(
                2,
                GIT_OPEN_STATUS | GIT_OPEN_UNTRACKED | GIT_OPEN_TRACKING,
                dir.to_str().unwrap(),
            )),
            &mut repos,
            &out,
            false,
        )
        .await;
        let info_msg = rx.try_recv().expect("synchronous GIT_REPO");
        let info = parse_git_repo(&info_msg).unwrap();
        assert_eq!(info.status, GIT_STATUS_OK);
        let repo_id = info.repo_id;
        let state = wait_msg(&mut rx, S2C_GIT_STATE);
        let mut mirror = GitStateMirror::new();
        mirror.apply_state(&state).complete().expect("valid state");
        assert_eq!(mirror.head.as_ref().unwrap().name, "refs/heads/main");
        let head_oid = mirror.head.as_ref().unwrap().oid;

        // Log, tree, blob, base round-trips through the dispatcher.
        handle_git_message(
            &msg_git_log(10, repo_id, 0, 0, "", &[], &[]),
            &mut repos,
            &out,
            false,
        )
        .await;
        let page = parse_git_commits(&wait_msg(&mut rx, S2C_GIT_COMMITS)).unwrap();
        assert_eq!(page.status, GIT_STATUS_OK);
        assert_eq!(git_commit_records(&page.records).count(), 1);
        handle_git_message(
            &msg_git_tree(&GitTreeRequest {
                nonce: 11,
                repo_id,
                flags: 0,
                oid: head_oid,
                path: "",
                after: "",
            }),
            &mut repos,
            &out,
            false,
        )
        .await;
        let (_, status, _, records) =
            parse_git_tree_resp(&wait_msg(&mut rx, S2C_GIT_TREE)).unwrap();
        assert_eq!(status, GIT_STATUS_OK);
        assert_eq!(git_tree_records(&records).count(), 1);
        handle_git_message(
            &msg_git_blob(&GitBlobRequest {
                nonce: 12,
                repo_id,
                flags: 0,
                oid: head_oid,
                path: "f.txt",
                offset: 0,
                max_len: 0,
            }),
            &mut repos,
            &out,
            false,
        )
        .await;
        let (_, status, _, data) = parse_git_blob_resp(&wait_msg(&mut rx, S2C_GIT_BLOB)).unwrap();
        assert_eq!(status, GIT_STATUS_OK);
        assert_eq!(data, b"hello\n");

        // Unknown repo id answers UNKNOWN_ID; unknown cancel is a no-op.
        handle_git_message(
            &msg_git_tree(&GitTreeRequest {
                nonce: 13,
                repo_id: 999,
                flags: 0,
                oid: head_oid,
                path: "",
                after: "",
            }),
            &mut repos,
            &out,
            false,
        )
        .await;
        let (_, status, _, _) = parse_git_tree_resp(&wait_msg(&mut rx, S2C_GIT_TREE)).unwrap();
        assert_eq!(status, GIT_STATUS_UNKNOWN_ID);
        handle_git_message(&msg_git_cancel(77), &mut repos, &out, false).await;

        // Close answers GIT_CLOSED(client request) and frees the slot.
        handle_git_message(&msg_git_close(repo_id), &mut repos, &out, false).await;
        let (closed_id, reason) = parse_git_closed(&wait_msg(&mut rx, S2C_GIT_CLOSED)).unwrap();
        assert_eq!((closed_id, reason), (repo_id, GIT_CLOSED_CLIENT_REQUEST));
        assert!(repos.map.is_empty());

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// SYNCED + staged snapshot, live change, FETCH, STOP → CLOSED.
    #[tokio::test]
    async fn fs_sync_message_flow() {
        use blit_remote::fs::{
            FS_CLOSED_CLIENT_REQUEST, FS_FILE_OK, FS_STATUS_NOT_FOUND, FS_STATUS_OK,
            FS_SYNC_CONTENT, FS_SYNC_ID_INVALID, FS_SYNC_RECURSIVE, FsMirror, S2C_FS_CLOSED,
            S2C_FS_FILE, S2C_FS_SYNCED, S2C_FS_UPDATE, msg_fs_ack, msg_fs_fetch, msg_fs_stop,
            msg_fs_sync, parse_fs_file,
        };

        let dir = std::env::temp_dir()
            .join(format!("blit-server-fs-test-{}", std::process::id()))
            .canonicalize_or_create();
        std::fs::write(dir.join("a.txt"), b"alpha").unwrap();

        let (out, mut rx) = mpsc::unbounded_channel::<Vec<u8>>();
        let mut syncs = FsSyncs::default();

        // Bad path fails with a sentinel sync_id.
        let missing = dir.join("does-not-exist");
        handle_fs_message(
            &msg_fs_sync(1, FS_SYNC_RECURSIVE, 5, 0, &missing.to_string_lossy()),
            &mut syncs,
            &out,
            false,
        )
        .await;
        let msg = rx.try_recv().expect("synchronous refusal");
        assert_eq!(msg[0], S2C_FS_SYNCED);
        assert_eq!(u16::from_le_bytes([msg[3], msg[4]]), FS_SYNC_ID_INVALID);
        assert_eq!(msg[5], FS_STATUS_NOT_FOUND);

        // Good path: SYNCED then a RESET…SYNC snapshot.
        handle_fs_message(
            &msg_fs_sync(
                2,
                FS_SYNC_RECURSIVE | FS_SYNC_CONTENT,
                5,
                0,
                &dir.to_string_lossy(),
            ),
            &mut syncs,
            &out,
            false,
        )
        .await;
        let msg = recv_blocking(&mut rx);
        assert_eq!(msg[0], S2C_FS_SYNCED);
        let sync_id = u16::from_le_bytes([msg[3], msg[4]]);
        assert_eq!(msg[5], FS_STATUS_OK);
        assert_ne!(sync_id, FS_SYNC_ID_INVALID);

        let mut mirror = FsMirror::new();
        while !mirror.live.contains_key("a.txt") {
            let msg = recv_blocking(&mut rx);
            if msg[0] == S2C_FS_UPDATE {
                let id = mirror.apply_update(&msg).expect("valid update");
                handle_fs_message(&msg_fs_ack(sync_id, id), &mut syncs, &out, false).await;
            }
        }
        assert_eq!(mirror.live["a.txt"].content.as_deref(), Some(&b"alpha"[..]));

        // Live change flows without further requests.
        std::fs::write(dir.join("b.txt"), b"beta").unwrap();
        while !mirror.live.contains_key("b.txt") {
            let msg = recv_blocking(&mut rx);
            if msg[0] == S2C_FS_UPDATE {
                let id = mirror.apply_update(&msg).expect("valid update");
                handle_fs_message(&msg_fs_ack(sync_id, id), &mut syncs, &out, false).await;
            }
        }

        // FETCH round-trips content by path.
        handle_fs_message(&msg_fs_fetch(7, sync_id, "b.txt"), &mut syncs, &out, false).await;
        loop {
            let msg = recv_blocking(&mut rx);
            if msg[0] == S2C_FS_FILE {
                assert_eq!(parse_fs_file(&msg), Some((7, FS_FILE_OK, b"beta".to_vec())));
                break;
            }
        }

        // STOP yields FS_CLOSED(client request) even though the entry drops.
        handle_fs_message(&msg_fs_stop(sync_id), &mut syncs, &out, false).await;
        loop {
            let msg = recv_blocking(&mut rx);
            if msg[0] == S2C_FS_CLOSED {
                assert_eq!(u16::from_le_bytes([msg[1], msg[2]]), sync_id);
                assert_eq!(msg[3], FS_CLOSED_CLIENT_REQUEST);
                break;
            }
        }
        assert!(syncs.map.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The exclusion flags and the trailing pattern list survive the wire
    /// and narrow what the client mirrors (docs/design/fs-watch.md
    /// "Ignoring"): without them a checkout costs `node_modules` and
    /// `.git` too, which is what closes big syncs at the entry budget.
    #[tokio::test]
    async fn fs_sync_exclusion_flow() {
        use blit_remote::fs::{
            FS_STATUS_OK, FS_STATUS_OTHER, FS_SYNC_EXCLUDE_GIT, FS_SYNC_GITIGNORE,
            FS_SYNC_ID_INVALID, FS_SYNC_RECURSIVE, FS_SYNC_SINGLE, FsMirror, S2C_FS_SYNCED,
            S2C_FS_UPDATE, msg_fs_ack, msg_fs_stop, msg_fs_sync, msg_fs_sync_excluding,
        };

        let dir = std::env::temp_dir()
            .join(format!("blit-server-fs-excl-{}", std::process::id()))
            .canonicalize_or_create();
        std::fs::create_dir_all(dir.join(".git")).unwrap();
        std::fs::write(dir.join(".git/HEAD"), b"ref: refs/heads/main").unwrap();
        std::fs::create_dir_all(dir.join("node_modules")).unwrap();
        std::fs::write(dir.join("node_modules/dep.js"), b"x").unwrap();
        std::fs::write(dir.join(".gitignore"), "node_modules/\n").unwrap();
        std::fs::write(dir.join("keep.rs"), b"fn main() {}").unwrap();
        std::fs::write(dir.join("scratch.tmp"), b"x").unwrap();

        let (out, mut rx) = mpsc::unbounded_channel::<Vec<u8>>();
        let mut syncs = FsSyncs::default();

        // Exclusion narrows enumeration; SINGLE enumerates nothing.
        handle_fs_message(
            &msg_fs_sync(
                1,
                FS_SYNC_SINGLE | FS_SYNC_GITIGNORE,
                5,
                0,
                &dir.join("keep.rs").to_string_lossy(),
            ),
            &mut syncs,
            &out,
            false,
        )
        .await;
        let msg = rx.try_recv().expect("synchronous refusal");
        assert_eq!(u16::from_le_bytes([msg[3], msg[4]]), FS_SYNC_ID_INVALID);
        assert_eq!(msg[5], FS_STATUS_OTHER);

        handle_fs_message(
            &msg_fs_sync_excluding(
                2,
                FS_SYNC_RECURSIVE | FS_SYNC_GITIGNORE | FS_SYNC_EXCLUDE_GIT,
                5,
                0,
                &dir.to_string_lossy(),
                "*.tmp\n",
            ),
            &mut syncs,
            &out,
            false,
        )
        .await;
        let msg = recv_blocking(&mut rx);
        assert_eq!(msg[0], S2C_FS_SYNCED);
        let sync_id = u16::from_le_bytes([msg[3], msg[4]]);
        assert_eq!(msg[5], FS_STATUS_OK);

        let mut mirror = FsMirror::new();
        while !mirror.live.contains_key("keep.rs") {
            let msg = recv_blocking(&mut rx);
            if msg[0] == S2C_FS_UPDATE {
                let id = mirror.apply_update(&msg).expect("valid update");
                handle_fs_message(&msg_fs_ack(sync_id, id), &mut syncs, &out, false).await;
            }
        }
        assert_eq!(
            mirror.live.keys().cloned().collect::<Vec<_>>(),
            ["", ".gitignore", "keep.rs"],
            "gitignored, .git, and client-excluded paths are all absent"
        );

        handle_fs_message(&msg_fs_stop(sync_id), &mut syncs, &out, false).await;
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// FS_SYNC_SINGLE end-to-end: a real file syncs as the `""` entry, the
    /// contradictory SINGLE|RECURSIVE combination refuses before any engine
    /// work, and a directory root answers the engine's refusal
    /// (docs/design/fs-watch.md "Single-file sync").
    #[tokio::test]
    async fn fs_sync_single_flow() {
        use blit_remote::fs::{
            FS_CLOSED_CLIENT_REQUEST, FS_STATUS_OK, FS_STATUS_OTHER, FS_SYNC_CONTENT,
            FS_SYNC_ID_INVALID, FS_SYNC_RECURSIVE, FS_SYNC_SINGLE, FsMirror, S2C_FS_CLOSED,
            S2C_FS_SYNCED, S2C_FS_UPDATE, msg_fs_ack, msg_fs_stop, msg_fs_sync,
        };

        let dir = std::env::temp_dir()
            .join(format!("blit-server-single-test-{}", std::process::id()))
            .canonicalize_or_create();
        let file = dir.join("solo.txt");
        std::fs::write(&file, b"solo").unwrap();

        let (out, mut rx) = mpsc::unbounded_channel::<Vec<u8>>();
        let mut syncs = FsSyncs::default();

        // SINGLE of a real file: SYNCED names the canonical file, then the
        // snapshot delivers it as the `""` entry.
        handle_fs_message(
            &msg_fs_sync(
                1,
                FS_SYNC_SINGLE | FS_SYNC_CONTENT,
                5,
                0,
                &file.to_string_lossy(),
            ),
            &mut syncs,
            &out,
            false,
        )
        .await;
        let msg = recv_blocking(&mut rx);
        assert_eq!(msg[0], S2C_FS_SYNCED);
        let sync_id = u16::from_le_bytes([msg[3], msg[4]]);
        assert_eq!(msg[5], FS_STATUS_OK);
        assert_ne!(sync_id, FS_SYNC_ID_INVALID);
        assert_eq!(
            std::str::from_utf8(&msg[8..]).unwrap(),
            blit_fssync::escape_path(&file)
        );

        let mut mirror = FsMirror::new();
        while !mirror.live.contains_key("") {
            let msg = recv_blocking(&mut rx);
            if msg[0] == S2C_FS_UPDATE {
                let id = mirror.apply_update(&msg).expect("valid update");
                handle_fs_message(&msg_fs_ack(sync_id, id), &mut syncs, &out, false).await;
            }
        }
        assert_eq!(mirror.live[""].content.as_deref(), Some(&b"solo"[..]));

        // Drain the sync so later replies are unambiguous.
        handle_fs_message(&msg_fs_stop(sync_id), &mut syncs, &out, false).await;
        loop {
            let msg = recv_blocking(&mut rx);
            if msg[0] == S2C_FS_CLOSED {
                assert_eq!(u16::from_le_bytes([msg[1], msg[2]]), sync_id);
                assert_eq!(msg[3], FS_CLOSED_CLIENT_REQUEST);
                break;
            }
        }

        // SINGLE|RECURSIVE is contradictory: refused synchronously.
        handle_fs_message(
            &msg_fs_sync(
                2,
                FS_SYNC_SINGLE | FS_SYNC_RECURSIVE,
                5,
                0,
                &file.to_string_lossy(),
            ),
            &mut syncs,
            &out,
            false,
        )
        .await;
        let msg = rx.try_recv().expect("synchronous refusal");
        assert_eq!(msg[0], S2C_FS_SYNCED);
        assert_eq!(u16::from_le_bytes([msg[3], msg[4]]), FS_SYNC_ID_INVALID);
        assert_eq!(msg[5], FS_STATUS_OTHER);
        assert_eq!(
            std::str::from_utf8(&msg[8..]).unwrap(),
            "single sync cannot be recursive or exclude anything"
        );

        // A directory root answers the engine's refusal.
        handle_fs_message(
            &msg_fs_sync(3, FS_SYNC_SINGLE, 5, 0, &dir.to_string_lossy()),
            &mut syncs,
            &out,
            false,
        )
        .await;
        let msg = recv_blocking(&mut rx);
        assert_eq!(msg[0], S2C_FS_SYNCED);
        assert_eq!(u16::from_le_bytes([msg[3], msg[4]]), FS_SYNC_ID_INVALID);
        assert_eq!(msg[5], FS_STATUS_OTHER);
        assert!(syncs.map.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// FS_FETCH rides a per-connection in-flight cap with a bounded queue
    /// instead of an error status (docs/design/fs-watch.md `FS_FETCH` has
    /// no busy code): a burst larger than the cap still answers every
    /// nonce exactly once, and the queue drains as replies free slots.
    #[tokio::test]
    async fn fs_fetch_burst_is_capped_and_fully_answered() {
        use blit_remote::fs::{
            FS_FILE_OK, FS_STATUS_OK, FS_SYNC_CONTENT, FS_SYNC_RECURSIVE, S2C_FS_FILE,
            S2C_FS_SYNCED, msg_fs_fetch, msg_fs_stop, msg_fs_sync, parse_fs_file,
        };

        let dir = std::env::temp_dir()
            .join(format!("blit-server-fetch-test-{}", std::process::id()))
            .canonicalize_or_create();
        std::fs::write(dir.join("a.txt"), b"payload").unwrap();

        let (out, mut rx) = mpsc::unbounded_channel::<Vec<u8>>();
        let mut syncs = FsSyncs::default();
        handle_fs_message(
            &msg_fs_sync(
                1,
                FS_SYNC_RECURSIVE | FS_SYNC_CONTENT,
                5,
                0,
                &dir.to_string_lossy(),
            ),
            &mut syncs,
            &out,
            false,
        )
        .await;
        let msg = recv_blocking(&mut rx);
        assert_eq!(msg[0], S2C_FS_SYNCED);
        assert_eq!(msg[5], FS_STATUS_OK);
        let sync_id = u16::from_le_bytes([msg[3], msg[4]]);

        let burst = (fs_fetch_inflight() + 5) as u16;
        for nonce in 0..burst {
            handle_fs_message(
                &msg_fs_fetch(nonce, sync_id, "a.txt"),
                &mut syncs,
                &out,
                false,
            )
            .await;
        }
        let mut answered = HashSet::new();
        while answered.len() < burst as usize {
            let msg = recv_blocking(&mut rx);
            if msg[0] == S2C_FS_FILE {
                let (nonce, status, data) = parse_fs_file(&msg).unwrap();
                assert_eq!(status, FS_FILE_OK);
                assert_eq!(data, b"payload");
                assert!(answered.insert(nonce), "nonce {nonce} answered twice");
            }
        }
        assert!(syncs.fetches.inner.lock().unwrap().queue.is_empty());
        handle_fs_message(&msg_fs_stop(sync_id), &mut syncs, &out, false).await;
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// FS_SEARCH walks are capped like index walks — over-cap (or a
    /// duplicate in-flight nonce) answers `RESOURCE_LIMIT`, and slots free
    /// on completion (docs/design/fs-search.md § Budgets).
    #[tokio::test]
    async fn fs_search_walks_are_capped() {
        use blit_remote::fs::{
            FS_STATUS_OK, FS_STATUS_RESOURCE_LIMIT, msg_fs_search, parse_fs_search_result,
        };

        let dir = std::env::temp_dir()
            .join(format!("blit-server-search-test-{}", std::process::id()))
            .canonicalize_or_create();
        std::fs::write(dir.join("alpha.rs"), b"x").unwrap();
        let root = dir.to_string_lossy().into_owned();

        let (out, mut rx) = mpsc::unbounded_channel::<Vec<u8>>();
        let mut syncs = FsSyncs::default();

        // Fill every slot; one more — or a duplicate nonce — refuses.
        // Driven off the knob so raising the default does not rewrite this.
        let cap = fs_walk_inflight();
        let guards: Vec<_> = (0..cap)
            .map(|i| {
                syncs
                    .reserve_search(i as u16 + 1)
                    .expect("slot below the cap")
            })
            .collect();
        assert!(syncs.reserve_search(cap as u16 + 1).is_none());
        assert!(syncs.reserve_search(1).is_none());

        // At the cap the request answers RESOURCE_LIMIT without walking.
        handle_fs_message(&msg_fs_search(22, 10, &root, "a"), &mut syncs, &out, false).await;
        let (nonce, status, paths) = parse_fs_search_result(&recv_blocking(&mut rx)).unwrap();
        assert_eq!(
            (nonce, status, paths.len()),
            (22, FS_STATUS_RESOURCE_LIMIT, 0)
        );

        // Dropping the guards frees the slots and the search runs.
        drop(guards);
        handle_fs_message(&msg_fs_search(9, 10, &root, "alp"), &mut syncs, &out, false).await;
        let (nonce, status, paths) = parse_fs_search_result(&recv_blocking(&mut rx)).unwrap();
        assert_eq!((nonce, status), (9, FS_STATUS_OK));
        assert!(paths.contains(&"alpha.rs".to_string()));

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// End-to-end grep: literal vs regex, case sensitivity, and the rule
    /// that `.gitignore` *ranks* rather than filters — ignored hits are
    /// present, flagged, and sorted after every tracked one.
    #[tokio::test]
    async fn fs_grep_matches_and_ranks_ignored_last() {
        use blit_remote::fs::{
            FS_DONE_INVALID, FS_DONE_OK, FS_GREP_CASE_SENSITIVE, FS_GREP_FILE_IGNORED,
            FS_GREP_NO_IGNORE, FS_GREP_REGEX, FsGrepRecord, fs_grep_records, msg_fs_grep,
            parse_fs_grep_result,
        };

        let root = std::env::temp_dir()
            .join(format!("blit-grep-{}", std::process::id()))
            .canonicalize_or_create();
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::create_dir_all(root.join("target")).unwrap();
        // A bare .git marks the tree as a repo, which is what makes the
        // walker apply .gitignore at all.
        std::fs::create_dir_all(root.join(".git")).unwrap();
        std::fs::write(root.join(".gitignore"), "target/\n").unwrap();
        std::fs::write(root.join("src/a.rs"), "let needle = 1;\nnothing\n").unwrap();
        std::fs::write(root.join("target/gen.rs"), "let needle = 2;\n").unwrap();
        // Binary files are skipped rather than returning unreadable lines.
        std::fs::write(root.join("src/blob.bin"), b"needle\0\0binary").unwrap();

        let run = |flags: u8, query: &str| {
            let root = root.clone();
            let query = query.to_string();
            async move {
                let (tx, mut rx) = mpsc::unbounded_channel::<Vec<u8>>();
                let mut syncs = FsSyncs::default();
                let msg = msg_fs_grep(1, flags, 0, 0, root.to_str().unwrap(), &query);
                handle_fs_message(&msg, &mut syncs, &tx, false).await;
                let (_, status, _, detail, records) =
                    parse_fs_grep_result(&recv_blocking(&mut rx)).unwrap();
                (status, detail, fs_grep_records(&records))
            }
        };

        // Ignore rules apply by default: the target/ hit is not searched.
        let (status, _, recs) = run(0, "NEEDLE").await;
        assert_eq!(status, FS_DONE_OK);
        assert!(
            !recs.iter().any(|r| matches!(
                r,
                FsGrepRecord::File { path, .. } if path.starts_with("target/")
            )),
            "gitignored files must be skipped unless NO_IGNORE is set"
        );

        // With NO_IGNORE they come back, ranked last and flagged.
        let (status, _, recs) = run(FS_GREP_NO_IGNORE, "NEEDLE").await;
        assert_eq!(status, FS_DONE_OK);
        let files: Vec<_> = recs
            .iter()
            .filter_map(|r| match r {
                FsGrepRecord::File { path, flags, .. } => Some((path.as_str(), *flags)),
                _ => None,
            })
            .collect();
        assert_eq!(
            files,
            vec![("src/a.rs", 0), ("target/gen.rs", FS_GREP_FILE_IGNORED)],
            "ignored files must be present, flagged, and last"
        );
        assert!(
            !files.iter().any(|(p, _)| p.ends_with(".bin")),
            "binary files must be skipped"
        );
        // The match record carries the line and its byte span.
        let m = recs
            .iter()
            .find_map(|r| match r {
                FsGrepRecord::Match {
                    line, col, text, ..
                } => Some((*line, *col, text.clone())),
                _ => None,
            })
            .unwrap();
        assert_eq!(m, (0, 4, "let needle = 1;".to_string()));

        // Case-sensitive finds nothing for the wrong case.
        let (_, _, recs) = run(FS_GREP_CASE_SENSITIVE | FS_GREP_NO_IGNORE, "NEEDLE").await;
        assert!(recs.is_empty(), "case-sensitive must not match 'needle'");

        // Literal mode escapes regex metacharacters.
        let (_, _, recs) = run(0, "needle.=").await;
        assert!(recs.is_empty(), "literal '.' must not match any char");
        let (_, _, recs) = run(FS_GREP_REGEX | FS_GREP_NO_IGNORE, "needle ?=").await;
        assert!(!recs.is_empty(), "regex mode must honour metacharacters");

        // A bad regex reports the engine's message rather than failing mute.
        let (status, detail, _) = run(FS_GREP_REGEX, "needle[").await;
        assert_eq!(status, FS_DONE_INVALID);
        assert!(!detail.is_empty(), "a compile error must carry a reason");

        // An empty query is refused before any I/O.
        let (status, _, _) = run(0, "").await;
        assert_eq!(status, FS_DONE_INVALID);

        let _ = std::fs::remove_dir_all(&root);
    }

    /// A file skipped as binary or oversized is *out of scope*, not a
    /// clipped result — the same status as `.git`. Reporting those as
    /// truncation made every search of a tree with a `target/` in it claim
    /// to be incomplete, which is worse than useless: it trains you to
    /// ignore the one signal that should mean "there is more to find".
    #[tokio::test]
    async fn fs_grep_skips_are_not_truncation() {
        use blit_remote::fs::{
            FS_DONE_OK, FS_GREP_TRUNCATED, fs_grep_records, msg_fs_grep, parse_fs_grep_result,
        };

        let root = std::env::temp_dir()
            .join(format!("blit-grep-trunc-{}", std::process::id()))
            .canonicalize_or_create();
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("hit.txt"), "needle here\n").unwrap();
        // Binary: sniffed and skipped.
        std::fs::write(root.join("blob.bin"), b"needle\0\0\0padding").unwrap();
        // Oversized: stat'd and skipped without reading.
        let big = vec![b'x'; (FS_GREP_MAX_FILE + 1) as usize];
        std::fs::write(root.join("huge.txt"), &big).unwrap();

        let (tx, mut rx) = mpsc::unbounded_channel::<Vec<u8>>();
        let mut syncs = FsSyncs::default();
        let msg = msg_fs_grep(1, 0, 0, 0, root.to_str().unwrap(), "needle");
        handle_fs_message(&msg, &mut syncs, &tx, false).await;
        let (_, status, flags, _, records) = parse_fs_grep_result(&recv_blocking(&mut rx)).unwrap();

        assert_eq!(status, FS_DONE_OK);
        assert_eq!(
            flags & FS_GREP_TRUNCATED,
            0,
            "skipped binary/oversized files must not report truncation"
        );
        // The real hit is still there.
        assert!(!fs_grep_records(&records).is_empty());

        let _ = std::fs::remove_dir_all(&root);
    }

    /// A pattern that matches most of one large file must be clipped by
    /// the walk's record budget, not collected in full first. The budget
    /// used to be checked only *between* files, so a dense pattern on a
    /// single large file built its whole match list — gigabytes, from a
    /// remotely-supplied pattern — before anything looked at it.
    #[tokio::test]
    async fn fs_grep_dense_single_file_stays_within_budget() {
        use blit_remote::fs::{
            FS_DONE_OK, FS_GREP_TRUNCATED, FsGrepRecord, fs_grep_records, msg_fs_grep,
            parse_fs_grep_result,
        };

        let root = std::env::temp_dir()
            .join(format!("blit-grep-dense-{}", std::process::id()))
            .canonicalize_or_create();
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        // One line per match keeps each record's text short, so the cap
        // that bites is the count rather than the bytes.
        let dense = "a\n".repeat(200_000);
        std::fs::write(root.join("dense.txt"), &dense).unwrap();

        let (tx, mut rx) = mpsc::unbounded_channel::<Vec<u8>>();
        let mut syncs = FsSyncs::default();
        let msg = msg_fs_grep(1, 0, 0, 0, root.to_str().unwrap(), "a");
        handle_fs_message(&msg, &mut syncs, &tx, false).await;
        let (_, status, flags, _, records) = parse_fs_grep_result(&recv_blocking(&mut rx)).unwrap();

        assert_eq!(status, FS_DONE_OK);
        assert_ne!(
            flags & FS_GREP_TRUNCATED,
            0,
            "dropping matches that exist must report truncation"
        );
        let decoded = fs_grep_records(&records);
        let matches = decoded
            .iter()
            .filter(|r| matches!(r, FsGrepRecord::Match { .. }))
            .count();
        assert!(
            matches <= FS_GREP_MAX_PER_FILE,
            "{matches} records for one file exceeds the u16 count field"
        );
        // And the count on the FILE record is the truth, not a wrapped one.
        let n = decoded
            .iter()
            .find_map(|r| match r {
                FsGrepRecord::File { n, .. } => Some(*n as usize),
                _ => None,
            })
            .expect("a FILE record");
        assert_eq!(n, matches, "FILE count must equal the records that follow");

        let _ = std::fs::remove_dir_all(&root);
    }

    /// Two matches on one line are two results, each carrying its own
    /// column — otherwise clicking the second hit would take you to the
    /// first, which is indistinguishable from the click not working.
    #[tokio::test]
    async fn fs_grep_reports_every_match_on_a_line() {
        use blit_remote::fs::{
            FS_DONE_OK, FsGrepRecord, fs_grep_records, msg_fs_grep, parse_fs_grep_result,
        };

        let root = std::env::temp_dir()
            .join(format!("blit-grep-multi-{}", std::process::id()))
            .canonicalize_or_create();
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        // "ab" twice on line 0, once on line 2.
        std::fs::write(root.join("f.txt"), "xx ab yy ab\nnope\nab\n").unwrap();

        let (tx, mut rx) = mpsc::unbounded_channel::<Vec<u8>>();
        let mut syncs = FsSyncs::default();
        let msg = msg_fs_grep(1, 0, 0, 0, root.to_str().unwrap(), "ab");
        handle_fs_message(&msg, &mut syncs, &tx, false).await;
        let (_, status, _, _, records) = parse_fs_grep_result(&recv_blocking(&mut rx)).unwrap();
        assert_eq!(status, FS_DONE_OK);

        let spans: Vec<(u32, u32, u32)> = fs_grep_records(&records)
            .into_iter()
            .filter_map(|r| match r {
                FsGrepRecord::Match {
                    line, col, end_col, ..
                } => Some((line, col, end_col)),
                _ => None,
            })
            .collect();
        assert_eq!(
            spans,
            vec![(0, 3, 5), (0, 9, 11), (2, 0, 2)],
            "each match gets its own row and column"
        );
        // The file record's count must agree with the rows that follow.
        let n = fs_grep_records(&records)
            .into_iter()
            .find_map(|r| match r {
                FsGrepRecord::File { n, .. } => Some(n),
                _ => None,
            })
            .unwrap();
        assert_eq!(n as usize, spans.len());

        let _ = std::fs::remove_dir_all(&root);
    }

    /// A pattern containing `\n` matches across lines. The record must
    /// report the full range and carry every line it spans — clamping to
    /// the first line showed a fragment of the thing you searched for.
    #[tokio::test]
    async fn fs_grep_multiline_match_carries_every_line() {
        use blit_remote::fs::{
            FS_DONE_OK, FS_GREP_REGEX, FsGrepRecord, fs_grep_records, msg_fs_grep,
            parse_fs_grep_result,
        };

        let root = std::env::temp_dir()
            .join(format!("blit-grep-ml-{}", std::process::id()))
            .canonicalize_or_create();
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("f.txt"), "pre\nalpha\nbeta\npost\n").unwrap();

        let (tx, mut rx) = mpsc::unbounded_channel::<Vec<u8>>();
        let mut syncs = FsSyncs::default();
        let msg = msg_fs_grep(
            1,
            FS_GREP_REGEX,
            0,
            0,
            root.to_str().unwrap(),
            "alpha\\nbeta",
        );
        handle_fs_message(&msg, &mut syncs, &tx, false).await;
        let (_, status, _, _, records) = parse_fs_grep_result(&recv_blocking(&mut rx)).unwrap();
        assert_eq!(status, FS_DONE_OK);

        let m = fs_grep_records(&records)
            .into_iter()
            .find_map(|r| match r {
                FsGrepRecord::Match { .. } => Some(r),
                _ => None,
            })
            .expect("a multi-line match");
        let FsGrepRecord::Match {
            line,
            col,
            end_line,
            end_col,
            text,
        } = m
        else {
            unreachable!()
        };
        assert_eq!((line, col, end_line, end_col), (1, 0, 2, 4));
        assert_eq!(text, "alpha\nbeta", "both lines, whole");

        let _ = std::fs::remove_dir_all(&root);
    }

    /// Whole-word matching wraps the pattern in \b(?:…)\b *after* literal
    /// escaping, so it composes with both modes — the same order
    /// `blit terminal grep --word-regexp` uses.
    #[tokio::test]
    async fn fs_grep_word_matches_whole_words_only() {
        use blit_remote::fs::{
            FS_DONE_OK, FS_GREP_WORD, FsGrepRecord, fs_grep_records, msg_fs_grep,
            parse_fs_grep_result,
        };

        let root = std::env::temp_dir()
            .join(format!("blit-grep-word-{}", std::process::id()))
            .canonicalize_or_create();
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("f.txt"), "cat\nconcatenate\nthe cat sat\n").unwrap();

        let run = |flags: u8| {
            let root = root.clone();
            async move {
                let (tx, mut rx) = mpsc::unbounded_channel::<Vec<u8>>();
                let mut syncs = FsSyncs::default();
                let msg = msg_fs_grep(1, flags, 0, 0, root.to_str().unwrap(), "cat");
                handle_fs_message(&msg, &mut syncs, &tx, false).await;
                let (_, status, _, _, records) =
                    parse_fs_grep_result(&recv_blocking(&mut rx)).unwrap();
                assert_eq!(status, FS_DONE_OK);
                fs_grep_records(&records)
                    .into_iter()
                    .filter_map(|r| match r {
                        FsGrepRecord::Match { line, .. } => Some(line),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
            }
        };

        // Without the flag, "concatenate" matches too.
        assert_eq!(run(0).await, vec![0, 1, 2]);
        // With it, only the standalone words.
        assert_eq!(run(FS_GREP_WORD).await, vec![0, 2]);

        let _ = std::fs::remove_dir_all(&root);
    }

    fn recv_blocking(rx: &mut mpsc::UnboundedReceiver<Vec<u8>>) -> Vec<u8> {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            match rx.try_recv() {
                Ok(msg) => return msg,
                Err(_) => {
                    assert!(Instant::now() < deadline, "timed out waiting for message");
                    std::thread::sleep(Duration::from_millis(5));
                }
            }
        }
    }

    trait CanonicalizeOrCreate {
        fn canonicalize_or_create(self) -> std::path::PathBuf;
    }

    impl CanonicalizeOrCreate for std::path::PathBuf {
        fn canonicalize_or_create(self) -> std::path::PathBuf {
            std::fs::create_dir_all(&self).unwrap();
            self.canonicalize().unwrap()
        }
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

    /// The first resize of a surface goes out immediately. Waiting out the
    /// settle window first would add its full latency to every isolated
    /// resize — a pane opening, a one-shot `blit surface capture --width`,
    /// the first frame of a drag — for no coalescing benefit, since there is
    /// nothing yet to coalesce with.
    #[test]
    fn first_surface_resize_dispatches_immediately() {
        assert_eq!(
            resize_action(None, None, Instant::now(), (800, 600, 120)),
            ResizeAction::Dispatch
        );
    }

    /// Everything arriving while the window is open is held, so a drag costs
    /// one configure (and one encoder rebuild, hence one keyframe) per
    /// window instead of one per frame.
    #[test]
    fn surface_resize_inside_the_settle_window_is_held() {
        let t0 = Instant::now();
        assert_eq!(
            resize_action(
                Some((800, 600, 120)),
                Some(t0),
                t0 + SURFACE_RESIZE_SETTLE / 2,
                (801, 600, 120),
            ),
            ResizeAction::Hold
        );
    }

    /// Once the window closes the next resize dispatches on arrival, so a
    /// sustained drag tracks at one configure per window rather than
    /// freezing until the user lets go.
    #[test]
    fn surface_resize_after_the_settle_window_dispatches() {
        let t0 = Instant::now();
        assert_eq!(
            resize_action(
                Some((800, 600, 120)),
                Some(t0),
                t0 + SURFACE_RESIZE_SETTLE,
                (801, 600, 120),
            ),
            ResizeAction::Dispatch
        );
    }

    /// The same thing against a live compositor rather than the policy
    /// function: the leading resize reaches it on arrival, a burst behind it
    /// collapses to a single held size rather than a queue, and closing the
    /// window delivers that one size.
    ///
    /// Multi-threaded because `ensure_compositor` starts the audio pipeline,
    /// which spawns blocking work.
    #[tokio::test(flavor = "multi_thread")]
    async fn surface_resize_burst_collapses_to_one_configure() {
        let mut session = Session::new();
        session.ensure_compositor(false, Arc::new(|| {}), "");

        // Leading edge: out at once, nothing held.
        assert!(session.resize_surface(1, 800, 600, 120));
        let cs = session.compositor.as_mut().unwrap();
        assert_eq!(cs.last_configured_size.get(&1), Some(&(800, 600, 120)));
        assert!(cs.pending_resize.is_empty());

        // A drag's worth of sizes behind it, all inside the window.
        for w in 801..=850 {
            assert!(!session.resize_surface(1, w, 600, 120));
        }
        let cs = session.compositor.as_mut().unwrap();
        assert_eq!(cs.pending_resize.get(&1), Some(&(850, 600, 120)));
        assert_eq!(
            cs.last_configured_size.get(&1),
            Some(&(800, 600, 120)),
            "the compositor must still be on the leading-edge size"
        );

        // Still inside the window: nothing goes out, and the caller is told
        // when to come back.
        let opened_at = cs.last_resize_at[&1];
        let due = cs.flush_due_resizes(opened_at);
        assert_eq!(due, Some(opened_at + SURFACE_RESIZE_SETTLE));
        assert_eq!(cs.last_configured_size.get(&1), Some(&(800, 600, 120)));

        // Window closed: the last size of the drag goes out, once.
        assert_eq!(
            cs.flush_due_resizes(opened_at + SURFACE_RESIZE_SETTLE),
            None
        );
        assert_eq!(cs.last_configured_size.get(&1), Some(&(850, 600, 120)));
        assert!(cs.pending_resize.is_empty());
    }

    /// Asking for the size the compositor was last given is a no-op whether
    /// or not the window is open — and it must beat the window check, so a
    /// drag that returns to its starting size clears the held intermediate
    /// instead of configuring to it after the fact.
    #[test]
    fn surface_resize_to_the_current_size_is_ignored() {
        let t0 = Instant::now();
        for now in [t0 + SURFACE_RESIZE_SETTLE / 2, t0 + SURFACE_RESIZE_SETTLE] {
            assert_eq!(
                resize_action(Some((800, 600, 120)), Some(t0), now, (800, 600, 120)),
                ResizeAction::Ignore
            );
        }
    }

    /// A lone viewer must get back exactly the size it asked for. The
    /// logical round trip does not give that: at 2× an odd physical extent
    /// comes back one pixel *larger* (1001 → 501 → 1002), so the surface was
    /// a pixel bigger than the pane, `per_client_encode_target` inscribed the
    /// native aspect into the smaller viewport, and the leftover showed as a
    /// letterbox bar. Tiled panes have fractional CSS widths, so odd physical
    /// extents are the common case rather than the corner one.
    #[test]
    fn mediated_surface_size_is_exact_for_one_viewer() {
        for &(w, h) in &[(1001u16, 563u16), (1000, 562), (1003, 999), (777, 1155)] {
            for &scale in &[120u16, 180, 240, 300] {
                let mut session = Session::new();
                let mut c = test_client();
                c.surface_subscriptions.insert(1);
                c.surface_view_sizes.insert(1, (w, h, scale));
                session.clients.insert(1, c);
                assert_eq!(
                    session.mediated_size_for_surface(1, &[]),
                    Some((w, h, scale.max(120))),
                    "one viewer at {w}x{h} scale={scale} must get its own size back"
                );
            }
        }
    }

    /// Mixed scales still go through logical space — there is no single
    /// physical size to preserve — but the client that set the minimum on an
    /// axis is the one whose pixels are honoured when it is at the chosen
    /// scale.
    #[test]
    fn mediated_surface_size_keeps_the_constraining_viewer_exact() {
        let mut session = Session::new();
        let mut c1 = test_client();
        let mut c2 = test_client();
        // c1 is narrower in logical terms and sits at the highest scale, so
        // its odd physical width must survive verbatim.
        c1.surface_subscriptions.insert(1);
        c1.surface_view_sizes.insert(1, (1001, 563, 240));
        c2.surface_subscriptions.insert(1);
        c2.surface_view_sizes.insert(1, (2000, 1200, 240));
        session.clients.insert(1, c1);
        session.clients.insert(2, c2);
        assert_eq!(
            session.mediated_size_for_surface(1, &[]),
            Some((1001, 563, 240))
        );
    }

    #[test]
    fn mediated_surface_size_picks_min_dimensions_max_scale() {
        let mut session = Session::new();
        let mut c1 = test_client();
        let mut c2 = test_client();
        // Client 1: 1920×1080 physical at 2× ⇒ 960×540 logical.
        // Client 2: 1280×720 physical at 1× ⇒ 1280×720 logical.
        // min logical = 960×540, max scale = 240 ⇒ 1920×1080 physical at 240.
        c1.surface_view_sizes.insert(1, (1920, 1080, 240));
        c1.surface_subscriptions.insert(1);
        c2.surface_view_sizes.insert(1, (1280, 720, 120));
        c2.surface_subscriptions.insert(1);
        session.clients.insert(1, c1);
        session.clients.insert(2, c2);
        assert_eq!(
            session.mediated_size_for_surface(1, &[]),
            Some((1920, 1080, 240))
        );
    }

    #[test]
    fn mediated_surface_size_same_logical_different_dpr_keeps_logical() {
        // Regression: with the old implementation that took
        // `min(physical), max(scale)` directly, two clients reporting the
        // SAME logical size at different DPRs produced a surface that was
        // half the intended logical size for the lower-DPR client.
        let mut session = Session::new();
        let mut c1 = test_client();
        let mut c2 = test_client();
        // Both clients want the surface at 800×600 logical.
        c1.surface_view_sizes.insert(1, (800, 600, 120)); // 1×
        c1.surface_subscriptions.insert(1);
        c2.surface_view_sizes.insert(1, (1600, 1200, 240)); // 2×
        c2.surface_subscriptions.insert(1);
        session.clients.insert(1, c1);
        session.clients.insert(2, c2);
        // Compositor must render at 800×600 logical (preserved across DPRs).
        // Highest scale wins (240) ⇒ 1600×1200 physical at scale 240.
        assert_eq!(
            session.mediated_size_for_surface(1, &[]),
            Some((1600, 1200, 240))
        );
    }

    #[test]
    fn mediated_surface_size_none_when_no_clients() {
        let session = Session::new();
        assert_eq!(session.mediated_size_for_surface(1, &[]), None);
    }

    #[test]
    fn mediated_surface_size_single_client() {
        let mut session = Session::new();
        let mut c1 = test_client();
        c1.surface_view_sizes.insert(3, (800, 600, 120));
        c1.surface_subscriptions.insert(3);
        session.clients.insert(1, c1);
        assert_eq!(
            session.mediated_size_for_surface(3, &[]),
            Some((800, 600, 120))
        );
    }

    #[test]
    fn mediated_surface_size_ignores_other_surfaces() {
        let mut session = Session::new();
        let mut c1 = test_client();
        c1.surface_view_sizes.insert(1, (1920, 1080, 240));
        c1.surface_view_sizes.insert(2, (640, 480, 120));
        c1.surface_subscriptions.insert(1);
        c1.surface_subscriptions.insert(2);
        session.clients.insert(1, c1);
        assert_eq!(
            session.mediated_size_for_surface(1, &[]),
            Some((1920, 1080, 240))
        );
        assert_eq!(
            session.mediated_size_for_surface(2, &[]),
            Some((640, 480, 120))
        );
        assert_eq!(session.mediated_size_for_surface(3, &[]), None);
    }

    #[test]
    fn mediated_surface_size_clamped_to_encoder_max() {
        let mut session = Session::new();
        let mut c1 = test_client();
        c1.surface_view_sizes.insert(1, (5000, 3000, 240));
        c1.surface_subscriptions.insert(1);
        session.clients.insert(1, c1);
        assert_eq!(
            session.mediated_size_for_surface(1, &[]),
            Some((5000, 3000, 240))
        );
        assert_eq!(
            session.mediated_size_for_surface(1, &[SurfaceEncoderPreference::H264Software]),
            Some((3840, 2160, 240))
        );
    }

    /// The default chain carries H.264 as a fallback, and folding it into a
    /// single ceiling used to hold every surface to 3840×2160 no matter what
    /// the viewer could actually decode.  An AV1 client on a 5K panel gets
    /// composited at 5K.
    #[test]
    fn mediated_surface_size_is_not_held_to_h264_by_a_fallback_in_the_chain() {
        let mut session = Session::new();
        let mut c1 = test_client();
        c1.surface_view_sizes.insert(1, (5120, 2880, 240));
        c1.surface_subscriptions.insert(1);
        c1.surface_codec_support = blit_remote::CODEC_SUPPORT_AV1 | blit_remote::CODEC_SUPPORT_H264;
        c1.surface_max_decode = (8192, 4352);
        session.clients.insert(1, c1);
        assert_eq!(
            session.mediated_size_for_surface(1, &SurfaceEncoderPreference::defaults()),
            Some((5120, 2880, 240))
        );
    }

    /// …but a client that only speaks H.264 still composites at 3840×2160,
    /// so nothing renders larger than it can possibly be sent.
    #[test]
    fn mediated_surface_size_stays_at_h264_ceiling_for_an_h264_only_client() {
        let mut session = Session::new();
        let mut c1 = test_client();
        c1.surface_view_sizes.insert(1, (5120, 2880, 240));
        c1.surface_subscriptions.insert(1);
        c1.surface_codec_support = blit_remote::CODEC_SUPPORT_H264;
        session.clients.insert(1, c1);
        assert_eq!(
            session.mediated_size_for_surface(1, &SurfaceEncoderPreference::defaults()),
            Some((3840, 2160, 240))
        );
    }

    /// Two viewers of one surface, one AV1 and one H.264-only, both asking
    /// for 5K.  The composite serves the more capable of them; the H.264
    /// viewer takes a downscale rather than dragging the surface to 4K.
    #[test]
    fn mediated_surface_size_composites_for_the_most_capable_subscriber() {
        let mut session = Session::new();
        let prefs = SurfaceEncoderPreference::defaults();
        let mut av1 = test_client();
        av1.surface_view_sizes.insert(1, (5120, 2880, 240));
        av1.surface_subscriptions.insert(1);
        av1.surface_codec_support = blit_remote::CODEC_SUPPORT_AV1;
        av1.surface_max_decode = (8192, 4352);
        let mut h264 = test_client();
        h264.surface_view_sizes.insert(1, (5120, 2880, 240));
        h264.surface_subscriptions.insert(1);
        h264.surface_codec_support = blit_remote::CODEC_SUPPORT_H264;
        h264.surface_max_decode = (3840, 2160);
        session.clients.insert(1, av1);
        session.clients.insert(2, h264);
        assert_eq!(
            session.mediated_size_for_surface(1, &prefs),
            Some((5120, 2880, 240))
        );
        // And the H.264 viewer is served an aspect-preserving downscale of
        // that composite, not a stream its decoder would reject.
        let h264 = &session.clients[&2];
        assert_eq!(
            Session::per_client_encode_target(
                Some((5120, 2880, 240)),
                5120,
                2880,
                surface_encode_cap(&prefs, h264, 1),
            ),
            (3840, 2160)
        );
    }

    /// A client subscribed to `surface_id` with the given codec support and
    /// declared decode ceiling.
    fn decoder_client(codec_support: u8, max_decode: (u16, u16)) -> ClientState {
        let mut c = test_client();
        c.surface_subscriptions.insert(1);
        c.surface_codec_support = codec_support;
        c.surface_max_decode = max_decode;
        c
    }

    #[test]
    fn surface_encode_cap_prefers_the_widest_eligible_backend_before_selection() {
        let prefs = SurfaceEncoderPreference::defaults();
        // Nothing selected yet: size for the best backend the client could
        // land on, and let `SurfaceEncoder::new` skip the ones that can't
        // carry it.
        assert_eq!(
            surface_encode_cap(
                &prefs,
                &decoder_client(blit_remote::CODEC_SUPPORT_AV1, (8192, 4352)),
                1
            ),
            Some(SurfaceEncoderPreference::NvencAV1.max_dimensions())
        );
        assert_eq!(
            surface_encode_cap(
                &prefs,
                &decoder_client(blit_remote::CODEC_SUPPORT_H264, (8192, 4352)),
                1
            ),
            Some((3840, 2160))
        );
        // An empty chain means no cap at all.
        assert_eq!(surface_encode_cap(&[], &decoder_client(0, (0, 0)), 1), None);
    }

    #[test]
    fn surface_encode_cap_follows_the_backend_that_actually_won() {
        let prefs = SurfaceEncoderPreference::defaults();
        let mut c = decoder_client(blit_remote::CODEC_SUPPORT_AV1, (8192, 4352));
        c.surface_subs.entry(1).or_default().selected_encoder =
            Some(SurfaceEncoderPreference::H264Vaapi);
        // The chain fell back to H.264 despite the client speaking AV1, so
        // the surface is sized for H.264 rather than for the AV1 it didn't
        // get.
        assert_eq!(surface_encode_cap(&prefs, &c, 1), Some((3840, 2160)));
        c.surface_subs.entry(1).or_default().selected_encoder =
            Some(SurfaceEncoderPreference::AV1Vaapi);
        assert_eq!(
            surface_encode_cap(&prefs, &c, 1),
            Some(SurfaceEncoderPreference::AV1Vaapi.max_dimensions())
        );
    }

    /// After a creation refused for size, the retry must be sized to what
    /// every eligible backend clears — otherwise the surface asks for the
    /// same impossible frame forever and never shows a picture.
    #[test]
    fn surface_encode_cap_degrades_to_the_tightest_after_an_oversized_refusal() {
        let prefs = SurfaceEncoderPreference::defaults();
        let mut c = decoder_client(blit_remote::CODEC_SUPPORT_AV1, (8192, 4352));
        {
            let sub = c.surface_subs.entry(1).or_default();
            // Degraded is set alongside a stale winner to pin the
            // precedence: the degrade wins, so a backend that has started
            // failing can't strand the surface at a size nothing accepts.
            sub.selected_encoder = Some(SurfaceEncoderPreference::AV1Vaapi);
            sub.encoder_cap_degraded = true;
        }
        assert_eq!(
            surface_encode_cap(&prefs, &c, 1),
            Some(SurfaceEncoderPreference::AV1Software.max_dimensions())
        );
    }

    /// Narrowing the ceiling is for frames nothing can carry.  A backend
    /// that fits and works gets another attempt at the same size instead —
    /// the degrade latches until the client resubscribes, so spending it on
    /// a momentary failure costs the viewer 5K for the rest of the session.
    #[test]
    fn only_a_frame_no_working_backend_fits_is_refused_for_size() {
        let prefs = SurfaceEncoderPreference::defaults();
        let av1 = blit_remote::CODEC_SUPPORT_AV1;
        let all_work = |_| true;

        // 5K on a host where hardware AV1 works: NvencAV1 could have taken
        // it, so this failure was not about the size.
        assert!(!refused_for_size(&prefs, av1, 5120, 2880, all_work));

        // Same frame once hardware AV1 is gone.  Only AV1Software is left
        // for an AV1 client, and it stops at 4K — so the surface has to
        // come down before anything can encode it.  `av1-vulkan` counts as
        // hardware AV1 and carries the same 8K ceiling, so leaving it in
        // would mean a backend still fits and the frame is not a size
        // problem.
        let no_hw_av1 = |p| {
            !matches!(
                p,
                SurfaceEncoderPreference::NvencAV1
                    | SurfaceEncoderPreference::AV1Vaapi
                    | SurfaceEncoderPreference::VulkanVideoAV1
            )
        };
        assert!(refused_for_size(&prefs, av1, 5120, 2880, no_hw_av1));

        // A frame everything clears is never a size problem, however much
        // of the chain is missing.
        assert!(!refused_for_size(&prefs, av1, 1920, 1080, no_hw_av1));

        // An H.264-only client is held to 3840x2160 by its own decoder, not
        // by which backends happen to be present.
        let h264 = blit_remote::CODEC_SUPPORT_H264;
        assert!(refused_for_size(&prefs, h264, 5120, 2880, all_work));
        assert!(!refused_for_size(&prefs, h264, 3840, 2160, all_work));
    }

    /// A backend can pass the 640x480 probe and still fail at 5K — VRAM for
    /// the frame buffers, a per-resolution driver limit the reported maximum
    /// doesn't admit to.  `refused_for_size` says no every time (the backend
    /// fits, and the host has seen it work), so without a second way down the
    /// surface would hold out for a size that never arrives and the viewer
    /// would watch black instead of the 4K it could have had.
    #[test]
    fn a_backend_that_keeps_failing_at_size_eventually_narrows_anyway() {
        let prefs = SurfaceEncoderPreference::defaults();
        let av1 = blit_remote::CODEC_SUPPORT_AV1;
        assert!(
            !refused_for_size(&prefs, av1, 5120, 2880, |_| true),
            "the size alone never explains this failure — hence the counter"
        );

        // What the creation loop does with that verdict, run out.
        let mut sub = SurfaceSubState::default();
        let mut narrowed_after = None;
        for attempt in 1..=CREATE_FAILURES_BEFORE_DEGRADE + 2 {
            sub.create_failures = sub.create_failures.saturating_add(1);
            let narrow = sub.create_failures >= CREATE_FAILURES_BEFORE_DEGRADE;
            if narrow && !sub.encoder_cap_degraded {
                sub.encoder_cap_degraded = true;
                narrowed_after.get_or_insert(attempt);
            }
        }
        assert_eq!(narrowed_after, Some(CREATE_FAILURES_BEFORE_DEGRADE));

        // And a run of failures that a success interrupts never gets there —
        // the resolution survives a momentary fault, which is the whole
        // reason the first failure doesn't narrow.
        let mut sub = SurfaceSubState::default();
        for _ in 0..CREATE_FAILURES_BEFORE_DEGRADE * 3 {
            sub.create_failures = sub.create_failures.saturating_add(1);
            assert!(sub.create_failures < CREATE_FAILURES_BEFORE_DEGRADE);
            sub.create_failures = 0; // the next creation succeeds
        }
        assert!(!sub.encoder_cap_degraded);
    }

    /// The decoder ceiling is a hard intersection: advertising AV1 says
    /// nothing about how large a frame the browser will actually accept, so
    /// a client that never declared one stays at 4K however capable the
    /// encoder is.
    #[test]
    fn surface_encode_cap_never_exceeds_the_declared_decoder_ceiling() {
        let prefs = SurfaceEncoderPreference::defaults();
        let av1 = blit_remote::CODEC_SUPPORT_AV1;
        assert_eq!(
            surface_encode_cap(&prefs, &decoder_client(av1, (0, 0)), 1),
            Some((3840, 2160)),
            "undeclared decode ceiling must not unlock >4K"
        );
        assert_eq!(
            surface_encode_cap(&prefs, &decoder_client(av1, (5120, 2880)), 1),
            Some((5120, 2880)),
            "a declared ceiling below the encoder's wins"
        );
        assert_eq!(
            surface_encode_cap(&prefs, &decoder_client(av1, (16384, 8704)), 1),
            Some(SurfaceEncoderPreference::NvencAV1.max_dimensions()),
            "a declared ceiling above the encoder's does not raise it"
        );
    }

    #[test]
    fn mediated_surface_size_ignores_unsubscribed_client() {
        // Stale view_size from a client that hasn't (re)subscribed
        // shouldn't drag the mediated size down for everyone.
        let mut session = Session::new();
        let mut c1 = test_client();
        let mut c2 = test_client();
        c1.surface_view_sizes.insert(1, (1920, 1080, 120));
        c1.surface_subscriptions.insert(1);
        // c2 has a tiny view_size but no subscription — should be skipped.
        c2.surface_view_sizes.insert(1, (100, 100, 120));
        session.clients.insert(1, c1);
        session.clients.insert(2, c2);
        assert_eq!(
            session.mediated_size_for_surface(1, &[]),
            Some((1920, 1080, 120))
        );
    }

    /// The whole point of a scaled subscription: a card-sized thumbnail must
    /// not drag the Wayland window down to a card for the viewer watching it
    /// full size.  It is subscribed and it has a view size, so both existing
    /// guards let it through — only the scaled target excludes it.
    #[test]
    fn mediated_surface_size_ignores_scaled_subscriber() {
        let mut session = Session::new();
        let mut full = test_client();
        let mut thumb = test_client();
        full.surface_subscriptions.insert(1);
        full.surface_view_sizes.insert(1, (1920, 1080, 120));
        thumb.surface_subscriptions.insert(1);
        thumb.surface_view_sizes.insert(1, (314, 176, 120));
        thumb.surface_subs.entry(1).or_default().scaled_target = Some((314, 176));
        session.clients.insert(1, full);
        session.clients.insert(2, thumb);
        assert_eq!(
            session.mediated_size_for_surface(1, &[]),
            Some((1920, 1080, 120))
        );
    }

    /// With no mediated viewer left there is nothing to mediate, so the
    /// surface keeps its last configured size rather than collapsing to the
    /// thumbnail's box.  `resize_surfaces_to_mediated_sizes` sends no resize
    /// for `None`, which is what leaves the compositor alone.
    #[test]
    fn mediated_surface_size_none_when_every_subscriber_is_scaled() {
        let mut session = Session::new();
        let mut thumb = test_client();
        thumb.surface_subscriptions.insert(1);
        thumb.surface_view_sizes.insert(1, (314, 176, 120));
        thumb.surface_subs.entry(1).or_default().scaled_target = Some((314, 176));
        session.clients.insert(1, thumb);
        assert_eq!(session.mediated_size_for_surface(1, &[]), None);
    }

    /// A scaled target is just a view box, so it inherits the same clamps —
    /// native aspect preserved, never upscaled past native.
    #[test]
    fn per_client_encode_target_honours_a_scaled_target() {
        // 314-wide box against a 16:9 native ⇒ width-bound, even-rounded.
        assert_eq!(
            Session::per_client_encode_target(Some((314, 176, 120)), 1920, 1080, None),
            (314, 176)
        );
        // A thumbnail asking for more than native still gets native.
        assert_eq!(
            Session::per_client_encode_target(Some((4000, 4000, 120)), 640, 480, None),
            (640, 480)
        );
    }

    #[test]
    fn per_client_encode_target_uses_view_size() {
        // 1280×720 viewport, 1920×1080 native (both 16:9) ⇒ 1280×720.
        assert_eq!(
            Session::per_client_encode_target(Some((1280, 720, 120)), 1920, 1080, None),
            (1280, 720)
        );
    }

    #[test]
    fn per_client_encode_target_clamps_to_native() {
        // Viewport 4000×3000 but native is only 1920×1080 — encoding bigger
        // would just upscale, so the encoder runs at native.
        assert_eq!(
            Session::per_client_encode_target(Some((4000, 3000, 240)), 1920, 1080, None),
            (1920, 1080)
        );
    }

    #[test]
    fn per_client_encode_target_clamps_to_encoder_max() {
        // Viewport 8000×4500 and native 8000×4500, but H.264 caps at
        // 3840×2160 — same 16:9 aspect, picks (3840, 2160).
        assert_eq!(
            Session::per_client_encode_target(
                Some((8000, 4500, 240)),
                8000,
                4500,
                Some((3840, 2160))
            ),
            (3840, 2160)
        );
    }

    #[test]
    fn per_client_encode_target_falls_back_to_native_without_view_size() {
        // Client hasn't sent C2S_SURFACE_RESIZE yet — encode at native.
        assert_eq!(
            Session::per_client_encode_target(None, 800, 600, None),
            (800, 600)
        );
        // Zero-dim viewport (cleared by client) ⇒ also fall back.
        assert_eq!(
            Session::per_client_encode_target(Some((0, 0, 120)), 800, 600, None),
            (800, 600)
        );
    }

    #[test]
    fn per_client_encode_target_preserves_native_aspect_landscape() {
        // Native 1920×1080 (16:9).  Client viewport 1000×1000 (square).
        // Width-bound at 1000 keeps height at 1000*1080/1920 = 562 →
        // round even = 562 (already even).
        assert_eq!(
            Session::per_client_encode_target(Some((1000, 1000, 120)), 1920, 1080, None),
            (1000, 562)
        );
    }

    #[test]
    fn per_client_encode_target_preserves_native_aspect_portrait_client() {
        // Native 1920×1080 (16:9).  Client viewport 500×1000 (1:2).
        // Width-bound at 500 keeps height at 500*1080/1920 = 281,
        // rounded even = 280.
        assert_eq!(
            Session::per_client_encode_target(Some((500, 1000, 120)), 1920, 1080, None),
            (500, 280)
        );
    }

    #[test]
    fn per_client_encode_target_preserves_native_aspect_landscape_client_portrait_native() {
        // Native 1080×1920 (9:16).  Client viewport 1000×500 (2:1).
        // Height-bound at 500 keeps width at 500*1080/1920 = 281,
        // rounded even = 280.
        assert_eq!(
            Session::per_client_encode_target(Some((1000, 500, 120)), 1080, 1920, None),
            (280, 500)
        );
    }

    #[test]
    fn per_client_encode_target_rounds_to_even() {
        // Native 101×51 — odd dimensions.  Same-shape viewport rounds
        // down to even.
        assert_eq!(
            Session::per_client_encode_target(Some((101, 51, 120)), 101, 51, None),
            (100, 50)
        );
    }

    #[test]
    fn per_client_encode_target_floors_at_two() {
        // Tiny viewport on a tall native — height-bound at 1 → width 0
        // → floor to 2.  Encoders reject 0-dim and most reject 1-dim
        // because chroma subsampling needs at least a 2×2 grid.
        assert_eq!(
            Session::per_client_encode_target(Some((1, 1, 120)), 100, 1000, None),
            (2, 2)
        );
    }

    /// Regression: after a resize-shrink, stale per-client downscale
    /// targets (registered for the prior, larger native) can still
    /// produce `last_pixels` entries at sizes larger than the actual
    /// new native.  `compositor_native_for_sid` MUST consult the
    /// authoritative `native_sizes` map first so
    /// `per_client_encode_target` is computed against the real native,
    /// not the stale entry.  Without this, the encoder rebuilds at the
    /// wrong size and visible frames freeze until the stale target is
    /// cleared.
    #[test]
    fn compositor_native_for_sid_prefers_resize_event_over_stale_pixel_snapshot() {
        let mut native_sizes = HashMap::new();
        native_sizes.insert(1u16, (640u32, 360u32));
        // Renderer just blitted into a stale 1920x1080 downscale target
        // and a fresh 640x360 native composite, so `last_pixels` (and
        // `pixel_snapshot`) carry both sizes.  The 1920x1080 entry is
        // larger, so the legacy `max_by_key((w, h))` pick would mis-
        // identify it as native.
        let pixel_snapshot: Vec<(u16, u32, u32, u64, u32)> =
            vec![(1, 640, 360, 10, 0), (1, 1920, 1080, 9, 0)];
        assert_eq!(
            compositor_native_for_sid(&native_sizes, &pixel_snapshot, 1),
            Some((640, 360)),
        );
    }

    /// First render after `SurfaceCreated` may arrive before the
    /// `SurfaceResized` event, so `native_sizes` is empty.  Falling
    /// back to the largest pixel-snapshot entry keeps the encode loop
    /// from skipping forever in that bootstrap window.
    #[test]
    fn compositor_native_for_sid_falls_back_to_largest_snapshot_entry() {
        let native_sizes = HashMap::new();
        let pixel_snapshot: Vec<(u16, u32, u32, u64, u32)> =
            vec![(1, 320, 240, 5, 0), (1, 800, 600, 6, 0)];
        assert_eq!(
            compositor_native_for_sid(&native_sizes, &pixel_snapshot, 1),
            Some((800, 600)),
        );
    }

    #[test]
    fn compositor_native_for_sid_returns_none_for_unknown_sid() {
        let native_sizes = HashMap::new();
        let pixel_snapshot: Vec<(u16, u32, u32, u64, u32)> = vec![(2, 640, 360, 1, 0)];
        assert_eq!(
            compositor_native_for_sid(&native_sizes, &pixel_snapshot, 1),
            None,
        );
    }

    #[test]
    fn mediated_surface_size_picks_min_across_clients() {
        let mut session = Session::new();
        let mut c1 = test_client();
        let mut c2 = test_client();
        c1.surface_view_sizes.insert(1, (1920, 1080, 120));
        c2.surface_view_sizes.insert(1, (640, 360, 120));
        c1.surface_subscriptions.insert(1);
        c2.surface_subscriptions.insert(1);
        session.clients.insert(1, c1);
        session.clients.insert(2, c2);
        assert_eq!(
            session.mediated_size_for_surface(1, &[]),
            Some((640, 360, 120))
        );
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
        assert_eq!(client.scroll_offsets.get(&7), Some(&12));
        assert_eq!(client.scroll_caches.get(&7), Some(&live));
    }

    #[test]
    fn leaving_scrollback_seeds_live_diff_from_scrollback_view() {
        let mut client = test_client();
        let history = sample_frame("hist");
        client.lead = Some(7);
        client.subscriptions.insert(7);
        client.scroll_offsets.insert(7, 12);
        client.scroll_caches.insert(7, history.clone());

        assert!(update_client_scroll_state(&mut client, 7, 0));
        assert_eq!(client.scroll_offsets.get(&7), None);
        assert_eq!(client.last_sent.get(&7), Some(&history));
        assert_eq!(client.scroll_caches.get(&7), None);
    }

    #[tokio::test]
    async fn request_surface_capture_returns_pixels_from_compositor() {
        let (command_tx, command_rx) = std::sync::mpsc::channel();
        std::thread::Builder::new()
            .name("test-capture-reply".into())
            .spawn(move || {
                let CompositorCommand::Capture {
                    surface_id,
                    scale_120: _,
                    reply,
                } = command_rx.recv().unwrap()
                else {
                    panic!("expected capture command");
                };
                assert_eq!(surface_id, 7);
                let _ = reply.send(Some((2, 3, vec![1, 2, 3, 4])));
            })
            .unwrap();

        let result =
            request_surface_capture_with_timeout(command_tx, 7, 0, Duration::from_millis(50)).await;

        assert_eq!(result, Some((2, 3, vec![1, 2, 3, 4])));
    }

    #[tokio::test]
    async fn request_surface_capture_returns_none_when_compositor_disconnects() {
        let (command_tx, command_rx) = std::sync::mpsc::channel();
        std::thread::Builder::new()
            .name("test-capture-drop".into())
            .spawn(move || {
                let _ = command_rx.recv().unwrap();
            })
            .unwrap();

        let result =
            request_surface_capture_with_timeout(command_tx, 7, 0, Duration::from_millis(50)).await;

        assert_eq!(result, None);
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

    // ── adaptive bandwidth ──

    fn sample(current: u8, budget: f32, observed: f32) -> RateSample {
        RateSample {
            ceiling: 120,
            current,
            budget_bytes: budget,
            observed_bytes: observed,
            congested: false,
            app_limited: false,
        }
    }

    #[test]
    fn an_app_limited_link_recovers_instead_of_degrading() {
        // Over budget on paper, but nothing on the path is straining.  The
        // budget is self-measured from our own traffic, so acting on it
        // would be the spinner death spiral: walk back toward the ceiling
        // instead.
        let mut s = sample(180, 1_000.0, 30_000.0);
        s.app_limited = true;
        assert_eq!(next_quantizer(s), 180 - ADAPTIVE_STEP);
        // Already at the ceiling: nothing to buy, hold.
        s.current = 120;
        assert_eq!(next_quantizer(s), 120);
        // Congestion outranks app-limited (they are mutually exclusive in
        // the caller, but the sample must not be trusted to be coherent).
        s.current = 180;
        s.congested = true;
        assert!(next_quantizer(s) > 180);
    }

    #[test]
    fn adaptive_bandwidth_never_spends_above_the_ceiling() {
        // Deep inside budget: the controller wants to improve, but the
        // configured ceiling is the best it may ever ask for.
        assert_eq!(next_quantizer(sample(120, 100_000.0, 1_000.0)), 120);
        // A current value below the ceiling (stale state) is pulled back up.
        assert_eq!(next_quantizer(sample(40, 100_000.0, 1_000.0)), 120);
    }

    #[test]
    fn adaptive_bandwidth_backs_off_when_over_budget_and_returns_when_under() {
        let over = next_quantizer(sample(140, 10_000.0, 30_000.0));
        assert_eq!(over, 140 + ADAPTIVE_STEP);
        let under = next_quantizer(sample(140, 30_000.0, 10_000.0));
        assert_eq!(under, 140 - ADAPTIVE_STEP);
        // On budget: hold, so the loop settles instead of hunting.
        assert_eq!(next_quantizer(sample(140, 10_000.0, 10_000.0)), 140);
    }

    #[test]
    fn adaptive_bandwidth_decreases_multiplicatively_when_congested() {
        let mut s = sample(160, 10_000.0, 1_000.0);
        s.congested = true;
        // Congestion outranks "comfortably inside budget": the queue is
        // already forming, so back off rather than improve.
        assert!(next_quantizer(s) > 160 + ADAPTIVE_STEP);
        // And never past the floor of usable picture.
        s.current = ADAPTIVE_MAX_QUANTIZER;
        assert_eq!(next_quantizer(s), ADAPTIVE_MAX_QUANTIZER);
    }

    #[test]
    fn adaptive_bandwidth_holds_without_measurements() {
        // No goodput estimate yet, or no frame measured: guessing here would
        // degrade a link that may be perfectly healthy.
        assert_eq!(next_quantizer(sample(150, 0.0, 10_000.0)), 150);
        assert_eq!(next_quantizer(sample(150, 10_000.0, 0.0)), 150);
    }

    #[test]
    fn surface_budget_splits_by_measured_share() {
        let (mut client, _rx) = test_client_with_capacity(64);
        client.goodput_bps = 1_000_000.0;
        client.display_fps = 10.0;
        client.surface_subs.entry(1).or_default().frame_bytes = 30_000.0;
        client.surface_subs.entry(2).or_default().frame_bytes = 10_000.0;
        let big = surface_budget_bytes(&client, 1);
        let small = surface_budget_bytes(&client, 2);
        assert!(big > small, "big={big} small={small}");
        assert!(
            (big / small - 3.0).abs() < 0.01,
            "3:1 split, got {big}/{small}"
        );
    }

    // ── surface pacing is independent of terminal backlog ──

    #[test]
    fn surface_pacing_ignores_terminal_backlog() {
        let (mut client, _rx) = test_client_with_capacity(64);
        client.display_fps = 60.0;
        client.surface_subs.entry(1).or_default();

        let clean = surface_pacing_fps(&client, 1);

        // A burst of shell output backs the terminal's paint loop up.  That
        // is what `browser_pacing_fps` reads, and it must not reach video.
        client.browser_backlog_frames = 20;
        client.browser_ack_ahead_frames = 20;
        assert!(
            browser_pacing_fps(&client) < clean,
            "precondition: terminal pacing should back off here"
        );
        assert_eq!(surface_pacing_fps(&client, 1), clean);
    }

    #[test]
    fn surface_pacing_backs_off_on_its_own_inflight_depth() {
        let (mut client, _rx) = test_client_with_capacity(64);
        client.display_fps = 60.0;
        client.rtt_ms = 1.0;
        client.min_rtt_ms = 1.0;
        client.surface_subs.entry(1).or_default();

        let clean = surface_pacing_fps(&client, 1);
        let now = Instant::now();
        let window = surface_frame_window(&client);
        for _ in 0..(window * 4) {
            record_surface_frame_sent(&mut client, 1, 1_000, false, now);
        }
        assert!(
            surface_pacing_fps(&client, 1) < clean,
            "a surface queue well past the link's window should slow it down"
        );
    }

    #[test]
    fn surface_pacing_tolerates_a_high_rtt_link() {
        // 100 ms RTT at 60 Hz legitimately keeps ~6 frames in flight.  A
        // constant threshold would read that as congestion and halve the
        // rate on a link that is behaving perfectly.
        let (mut client, _rx) = test_client_with_capacity(64);
        client.display_fps = 60.0;
        client.rtt_ms = 100.0;
        client.min_rtt_ms = 100.0;
        client.surface_subs.entry(1).or_default();

        let now = Instant::now();
        for _ in 0..6 {
            record_surface_frame_sent(&mut client, 1, 1_000, false, now);
        }
        assert_eq!(surface_pacing_fps(&client, 1), 60.0);
    }

    #[test]
    fn surface_pacing_is_per_surface() {
        let (mut client, _rx) = test_client_with_capacity(64);
        client.display_fps = 60.0;
        client.rtt_ms = 1.0;
        client.min_rtt_ms = 1.0;
        client.surface_subs.entry(1).or_default();
        client.surface_subs.entry(2).or_default();

        let now = Instant::now();
        let window = surface_frame_window(&client);
        for _ in 0..(window * 4) {
            record_surface_frame_sent(&mut client, 1, 1_000, false, now);
        }
        // Surface 1 is backed up; surface 2 keeps its full rate.
        assert!(surface_pacing_fps(&client, 1) < 60.0);
        assert_eq!(surface_pacing_fps(&client, 2), 60.0);
    }

    #[test]
    fn surface_pacing_never_reaches_zero() {
        let (mut client, _rx) = test_client_with_capacity(64);
        client.display_fps = 60.0;
        client.surface_subs.entry(1).or_default();
        let now = Instant::now();
        for _ in 0..surface_inflight_cap(&client) {
            record_surface_frame_sent(&mut client, 1, 1_000, false, now);
        }
        assert!(surface_pacing_fps(&client, 1) >= 1.0);
    }

    #[test]
    fn surface_inflight_cap_stays_above_the_window() {
        // The backoff compares `inflight > window`, so the tracking queue
        // has to be able to hold more than the window or the comparison is
        // unreachable and the controller is silently inert.  This is what
        // a flat cap of 64 got wrong at 1 s RTT.
        for (rtt, fps) in [
            (1.0f32, 60.0f32),
            (100.0, 60.0),
            (500.0, 60.0),
            (500.0, 240.0),
            (1000.0, 60.0),
            (1000.0, 120.0),
        ] {
            let (mut client, _rx) = test_client_with_capacity(64);
            client.rtt_ms = rtt;
            client.min_rtt_ms = rtt;
            client.display_fps = fps;
            let window = surface_frame_window(&client);
            let cap = surface_inflight_cap(&client);
            assert!(
                cap > window,
                "rtt={rtt} fps={fps}: cap {cap} must exceed window {window}"
            );
        }
    }

    #[test]
    fn app_limited_threshold_does_not_move_with_rtt() {
        // Regression: deriving this from surface_inflight_cap made it track
        // surface_frame_window, so a deep-but-healthy link read as
        // app-limited and the quality controller walked the quantizer back
        // toward the ceiling on a link that was merely far away.  The
        // quality controller and the pacer answer different questions and
        // must not share a threshold.
        let deep = {
            let (mut c, _rx) = test_client_with_capacity(64);
            c.rtt_ms = 1000.0;
            c.min_rtt_ms = 1000.0;
            c.display_fps = 60.0;
            c
        };
        let near = {
            let (mut c, _rx) = test_client_with_capacity(64);
            c.rtt_ms = 1.0;
            c.min_rtt_ms = 1.0;
            c.display_fps = 60.0;
            c
        };
        // The window legitimately differs by an order of magnitude...
        assert!(surface_frame_window(&deep) > surface_frame_window(&near) * 4);
        // ...but the app-limited boundary must not.
        assert_eq!(SURFACE_INFLIGHT_MIN / 2, 32);
    }

    #[test]
    fn surface_inflight_cap_is_bounded() {
        let (mut client, _rx) = test_client_with_capacity(64);
        client.rtt_ms = 60_000.0;
        client.min_rtt_ms = 60_000.0;
        client.display_fps = 480.0;
        assert_eq!(surface_inflight_cap(&client), SURFACE_INFLIGHT_HARD_MAX);
    }

    #[test]
    fn surface_backoff_engages_at_one_second_rtt() {
        // Regression for the reported case: a plain 60 Hz client on a 1 s
        // link.  The window is 71 there; with the old flat cap of 64 the
        // queue could never reach it and the rate never backed off.
        let (mut client, _rx) = test_client_with_capacity(64);
        client.rtt_ms = 1000.0;
        client.min_rtt_ms = 1000.0;
        client.display_fps = 60.0;
        client.surface_subs.entry(1).or_default();

        let now = Instant::now();
        let window = surface_frame_window(&client);
        // Steady state for this link is ~60 frames: still full rate.
        for _ in 0..60 {
            record_surface_frame_sent(&mut client, 1, 1_000, false, now);
        }
        assert_eq!(surface_pacing_fps(&client, 1), 60.0);

        // Past what the link should hold: the rate must come down.
        for _ in 0..(window * 2) {
            record_surface_frame_sent(&mut client, 1, 1_000, false, now);
        }
        assert!(
            surface_pacing_fps(&client, 1) < 60.0,
            "backoff must be reachable at 1 s RTT"
        );
    }

    #[test]
    fn surface_acks_are_matched_to_their_own_surface() {
        let (mut client, _rx) = test_client_with_capacity(64);
        client.surface_subs.entry(1).or_default();
        client.surface_subs.entry(2).or_default();
        let now = Instant::now();
        record_surface_frame_sent(&mut client, 1, 1_000, false, now);
        record_surface_frame_sent(&mut client, 2, 2_000, false, now);
        // Surface 2 acks first (its frame is smaller on the wire, or its
        // decoder is faster).  The queue must give up surface 2's entry, not
        // the one at the front.
        record_surface_ack(&mut client, 2);
        assert_eq!(client.surface_inflight_frames.len(), 1);
        assert_eq!(client.surface_inflight_frames[0].surface_id, 1);
    }

    #[test]
    fn adaptive_step_reports_a_quantizer_with_no_local_encoder() {
        // A Vulkan surface has no `SurfaceEncoder` on the server side, so
        // the step used to fall out silently.  It must still report where
        // the rate moved to, because that number is what gets forwarded to
        // the compositor's session for this client.
        let (mut client, _rx) = test_client_with_capacity(64);
        client.goodput_bps = 10_000.0;
        client.display_fps = 30.0;
        // Degrading needs pressure evidence, not just a budget verdict: the
        // browser reporting a backlog is what says the link truly can't
        // carry these frames.
        client.browser_backlog_frames = 20;
        let sub = client.surface_subs.entry(4).or_default();
        sub.frame_bytes = 60_000.0;
        assert!(sub.encoder.is_none());
        let step = step_adaptive_bandwidth(
            &mut client,
            SurfaceBandwidth::Medium,
            4,
            Instant::now(),
            false,
        );
        let q = step
            .quantizer
            .expect("over budget by 20x must move the rate");
        assert!(
            q > SurfaceBandwidth::Medium.av1_quantizer() as u8,
            "cheaper than the ceiling, got {q}",
        );
        // Nothing to rebuild: the compositor retargets in place.
        assert!(!step.rebuild);
    }

    #[test]
    fn a_lone_animation_on_a_quiet_link_is_not_walked_to_the_floor() {
        // The spinner case: tiny frames, forever changing, link otherwise
        // idle.  Goodput has collapsed to the spinner's own send rate, so
        // every frame reads as "over budget" — but nothing is congested,
        // nothing is backlogged, nothing is in flight.  The controller
        // must walk back toward the ceiling, not away from it.
        let (mut client, _rx) = test_client_with_capacity(64);
        client.goodput_bps = 50_000.0;
        client.display_fps = 60.0;
        let ceiling = SurfaceBandwidth::Medium.av1_quantizer() as u8;
        let sub = client.surface_subs.entry(7).or_default();
        sub.frame_bytes = 1_700.0;
        sub.adaptive_quantizer = Some(ADAPTIVE_MAX_QUANTIZER);

        let mut q = ADAPTIVE_MAX_QUANTIZER;
        for _ in 0..40 {
            client.surface_subs.entry(7).or_default().rate_stepped_at = None;
            let step = step_adaptive_bandwidth(
                &mut client,
                SurfaceBandwidth::Medium,
                7,
                Instant::now(),
                false,
            );
            match step.quantizer {
                Some(next) => {
                    assert!(next < q, "must only improve, {q} -> {next}");
                    q = next;
                }
                None => break,
            }
        }
        assert_eq!(q, ceiling, "must recover all the way to the ceiling");
        assert_eq!(
            resolve_bandwidth(&client, SurfaceBandwidth::Medium, 7).av1_quantizer(),
            ceiling as usize,
        );
    }

    #[test]
    fn a_frozen_picture_is_refined_back_to_the_ceiling() {
        // Whatever the controller backed off to during motion is what the
        // client is left staring at once the screen stops.  Walk it back.
        let ceiling = SurfaceBandwidth::Medium.av1_quantizer() as u8;
        let mut q = ADAPTIVE_MAX_QUANTIZER;
        let mut steps = 0;
        while q > ceiling {
            let next = refine_toward_ceiling(q, ceiling);
            assert!(next < q, "must improve, {q} -> {next}");
            assert!(next >= ceiling, "must not overshoot the ceiling: {next}");
            q = next;
            steps += 1;
            assert!(
                steps < 12,
                "converging too slowly, every step is a keyframe"
            );
        }
        assert_eq!(q, ceiling);
        // At the ceiling there is nothing left to buy.
        assert_eq!(refine_toward_ceiling(ceiling, ceiling), ceiling);
    }

    #[test]
    fn a_still_surface_ignores_a_stale_frame_size() {
        // `frame_bytes` still describes the motion that just stopped.  Judged
        // against it, a surface that had been over budget would keep getting
        // worse while nothing at all is being sent.
        let (mut client, _rx) = test_client_with_capacity(64);
        client.goodput_bps = 10_000.0;
        client.display_fps = 30.0;
        // Backlogged, so the moving half of the contrast is genuinely
        // strained (an unstrained link would recover instead).
        client.browser_backlog_frames = 20;
        let ceiling = SurfaceBandwidth::Medium.av1_quantizer() as u8;
        let sub = client.surface_subs.entry(9).or_default();
        sub.frame_bytes = 60_000.0;
        sub.adaptive_quantizer = Some(150);

        let moving = step_adaptive_bandwidth(
            &mut client,
            SurfaceBandwidth::Medium,
            9,
            Instant::now(),
            false,
        );
        assert_eq!(
            moving.quantizer,
            Some(150 + ADAPTIVE_STEP),
            "over budget while moving: get cheaper",
        );

        client.surface_subs.entry(9).or_default().adaptive_quantizer = Some(150);
        client.surface_subs.entry(9).or_default().rate_stepped_at = None;
        let still = step_adaptive_bandwidth(
            &mut client,
            SurfaceBandwidth::Medium,
            9,
            Instant::now(),
            true,
        );
        let q = still.quantizer.expect("a frozen picture must be refined");
        assert!(
            q < 150 && q >= ceiling,
            "same stale bytes, opposite direction: {q}"
        );
    }

    #[test]
    fn a_still_surface_does_not_refine_into_a_backlog() {
        // A keyframe is the most expensive thing we can send.  Piling one
        // onto a queue that is already forming makes recovery slower.
        let (mut client, _rx) = test_client_with_capacity(2);
        client.surface_subs.entry(9).or_default().adaptive_quantizer = Some(ADAPTIVE_MAX_QUANTIZER);
        for _ in 0..8 {
            let _ = send_outbox(&client, vec![0u8; 64]);
        }
        assert!(
            outbox_backpressured(&client),
            "fixture must be backpressured"
        );
        let step = step_adaptive_bandwidth(
            &mut client,
            SurfaceBandwidth::Medium,
            9,
            Instant::now(),
            true,
        );
        assert_eq!(step.quantizer, None, "held until the queue drains");
    }

    #[test]
    fn a_surface_that_was_sent_nothing_owes_a_keyframe() {
        // A subscription with no state yet, and one whose state exists but
        // has never carried a keyframe, are the same thing to a decoder:
        // there is no reference frame, so a delta is undecodable.
        let (mut client, _rx) = test_client_with_capacity(64);
        assert!(owes_keyframe(&client, 3), "no sub state at all");
        client.surface_subs.entry(3).or_default();
        assert!(owes_keyframe(&client, 3), "sub state, no keyframe yet");
        client.surface_subs.entry(3).or_default().has_keyframe = true;
        assert!(!owes_keyframe(&client, 3));
    }

    #[test]
    fn one_surfaces_keyframe_does_not_settle_anothers_debt() {
        // The flag used to live on the client, so the first surface to
        // deliver a keyframe cleared it for every other surface still
        // waiting on one — those surfaces then got deltas against a
        // reference their decoder never received.
        let (mut client, _rx) = test_client_with_capacity(64);
        for sid in [1u16, 2] {
            client.surface_subs.entry(sid).or_default();
        }
        assert!(owes_keyframe(&client, 1) && owes_keyframe(&client, 2));

        // Surface 1 gets its keyframe.  Surface 2 is untouched by that.
        client.surface_subs.entry(1).or_default().has_keyframe = true;
        assert!(!owes_keyframe(&client, 1));
        assert!(
            owes_keyframe(&client, 2),
            "surface 2 never received a keyframe of its own",
        );

        // And the reverse: breaking surface 2's chain leaves surface 1's
        // intact, so one surface resizing does not cost every other surface
        // an unnecessary keyframe.
        client.surface_subs.entry(2).or_default().has_keyframe = true;
        client.surface_subs.entry(2).or_default().has_keyframe = false;
        assert!(!owes_keyframe(&client, 1), "surface 1 still has its own");
        assert!(owes_keyframe(&client, 2));
    }

    #[test]
    fn dropping_a_subscription_drops_its_keyframe_standing() {
        // `surface_subs` entries are removed wholesale on UNSUBSCRIBE and
        // SurfaceDestroyed.  A later resubscribe reuses the id against a
        // fresh encoder, so it must not inherit the old chain's standing.
        let (mut client, _rx) = test_client_with_capacity(64);
        client.surface_subs.entry(8).or_default().has_keyframe = true;
        assert!(!owes_keyframe(&client, 8));
        client.surface_subs.remove(&8);
        assert!(owes_keyframe(&client, 8), "a reused id starts over");
    }

    #[test]
    fn a_generation_that_encoded_to_nothing_is_not_marked_sent() {
        // `unchanged` reads `last_encoded_gen` as "the client already has
        // this".  An encode that produced no bitstream sent nothing, so
        // claiming its generation strands that frame: the gate skips it on
        // every later tick, and only new pixels ever dislodge it.
        assert_eq!(encoded_generation(Some(4), 5, true), Some(5));
        assert_eq!(
            encoded_generation(Some(4), 5, false),
            Some(4),
            "an empty encode must leave the mark where it was",
        );
        // The first generation on a fresh sub is the one that matters most:
        // there is no earlier frame on screen to fall back to.
        assert_eq!(encoded_generation(None, 5, false), None);

        // The failure this guards, played out.  A surface paints, its last
        // encode comes back empty, and then it goes still — a video reaching
        // its final frame.  The generation must stay re-encodable.
        let unchanged = |mark: Option<u64>, latest: u64| mark == Some(latest);
        let mut mark = Some(11u64);
        mark = encoded_generation(mark, 12, false);
        assert!(
            !unchanged(mark, 12),
            "the last frame must still be owed to the client",
        );
        mark = encoded_generation(mark, 12, true);
        assert!(unchanged(mark, 12), "and settle once it is actually sent");
    }

    #[test]
    fn a_vulkan_still_is_judged_on_its_own_generation_stream() {
        // A client on a compositor-resident encoder is fed bitstreams, not
        // the pixel snapshot, and the two carry independent generations.
        // Comparing against the wrong one leaves `unchanged` permanently
        // false, so the picture it is left staring at is never refined.
        let mut encoded: HashMap<(u16, u64), u64> = HashMap::new();
        encoded.insert((5, 77), 42);

        let latest = |has_vulkan: bool, px_gen: u64| -> u64 {
            if has_vulkan {
                encoded.get(&(5, 77)).copied().unwrap_or(u64::MAX)
            } else {
                px_gen
            }
        };

        // The pixel stream has moved on past the bitstream this client holds;
        // that says nothing about whether its picture changed.
        assert_eq!(latest(true, 99), 42);
        assert_eq!(latest(false, 99), 99);
        // A session with nothing produced yet must never read as "still":
        // there is no picture on screen to refine.
        assert_eq!(
            HashMap::<(u16, u64), u64>::new()
                .get(&(5, 77))
                .copied()
                .unwrap_or(u64::MAX),
            u64::MAX,
        );
    }

    #[test]
    fn a_refined_still_stops_refining_once_it_is_clean() {
        // The refresh costs a keyframe per step.  Once the picture is at the
        // ceiling there is nothing left to buy, and a controller that keeps
        // reporting a step would spend one every interval forever.
        let (mut client, _rx) = test_client_with_capacity(64);
        client.surface_subs.entry(11).or_default();
        let mut sent = 0;
        for _ in 0..40 {
            client.surface_subs.entry(11).or_default().rate_stepped_at = None;
            let step = step_adaptive_bandwidth(
                &mut client,
                SurfaceBandwidth::Medium,
                11,
                Instant::now(),
                true,
            );
            if step.quantizer.is_none() {
                break;
            }
            sent += 1;
        }
        assert!(sent < 40, "never settled: {sent} keyframes and counting");
        // And it settled at the ceiling, not short of it.
        assert_eq!(
            resolve_bandwidth(&client, SurfaceBandwidth::Medium, 11).av1_quantizer(),
            SurfaceBandwidth::Medium.av1_quantizer(),
        );
    }

    #[test]
    fn a_ceiling_cheaper_than_the_controller_floor_is_still_the_ceiling() {
        // The controller floors at ADAPTIVE_MAX_QUANTIZER, so a surface
        // configured cheaper than that (quantizer 255 = minimum bandwidth)
        // must not be pulled back up to 200 and spend more than allowed.
        let (mut client, _rx) = test_client_with_capacity(64);
        let sub = client.surface_subs.entry(6).or_default();
        sub.bandwidth_override = Some(SurfaceBandwidth::Custom { quantizer: 255 });
        sub.adaptive_quantizer = Some(ADAPTIVE_MAX_QUANTIZER);
        let resolved = resolve_bandwidth(&client, SurfaceBandwidth::Medium, 6);
        assert_eq!(resolved.av1_quantizer(), 255);
    }

    #[test]
    fn a_gone_surface_leaves_no_frames_to_be_acked_later() {
        // Surface ids are recycled, so a stale entry would be matched by a
        // frame minutes later and report a garbage RTT.
        let (mut client, _rx) = test_client_with_capacity(64);
        let now = Instant::now();
        record_surface_frame_sent(&mut client, 1, 1_000, false, now);
        record_surface_frame_sent(&mut client, 2, 1_000, false, now);
        forget_surface_inflight(&mut client, 1);
        assert_eq!(client.surface_inflight_frames.len(), 1);
        assert_eq!(client.surface_inflight_frames[0].surface_id, 2);
    }

    #[test]
    fn compositor_bitstreams_are_dropped_per_surface_not_per_client() {
        let mut last_encoded: HashMap<(u16, u64), LastEncoded> = HashMap::new();
        for key in [(1u16, 10u64), (1, 11), (2, 10)] {
            last_encoded.insert(
                key,
                LastEncoded {
                    width: 8,
                    height: 8,
                    data: Arc::new(Vec::new()),
                    is_keyframe: true,
                    codec_flag: 0,
                    generation: 1,
                    timestamp_ms: 0,
                },
            );
        }
        // Surface 1 was resized, so every viewer's bitstream for it is
        // stale — but surface 2 is untouched.
        last_encoded_remove_for_sid(&mut last_encoded, 1);
        assert_eq!(last_encoded.len(), 1);
        assert!(last_encoded.contains_key(&(2, 10)));
    }

    #[test]
    fn surface_inflight_queue_is_bounded() {
        let (mut client, _rx) = test_client_with_capacity(64);
        let now = Instant::now();
        let cap = surface_inflight_cap(&client);
        for _ in 0..(cap * 2) {
            record_surface_frame_sent(&mut client, 7, 1_000, false, now);
        }
        assert_eq!(client.surface_inflight_frames.len(), cap);
    }

    #[test]
    fn reset_inflight_clears_unacked_surface_frames() {
        let (mut client, _rx) = test_client_with_capacity(64);
        record_surface_frame_sent(&mut client, 3, 1_000, false, Instant::now());
        reset_inflight(&mut client);
        assert!(client.surface_inflight_frames.is_empty());
    }

    // ── outbox_queued_frames / outbox_backpressured ──

    #[test]
    fn outbox_queued_frames_zero_when_empty() {
        let client = test_client();
        assert_eq!(outbox_queued_frames(&client), 0);
    }

    #[test]
    fn outbox_backpressured_when_queue_full() {
        let (client, _rx) = test_client_with_capacity(0);
        // Fill the channel to trigger backpressure
        for _ in 0..OUTBOX_SOFT_QUEUE_LIMIT_FRAMES {
            let _ = send_outbox(&client, vec![0u8]);
        }
        assert!(outbox_backpressured(&client));
    }

    #[test]
    fn outbox_not_backpressured_by_small_frames_under_byte_budget() {
        let (client, _rx) = test_client_with_capacity(0);
        for _ in 0..(OUTBOX_SOFT_QUEUE_LIMIT_FRAMES - 1) {
            let _ = send_outbox(&client, vec![0u8; 512]);
        }
        assert!(!outbox_backpressured(&client));
    }

    #[test]
    fn outbox_backpressured_by_large_queued_bytes() {
        let (client, _rx) = test_client_with_capacity(0);
        // First frame is always allowed through, even at the byte limit, so
        // pending encoders can make progress when keyframes exceed the cap.
        let _ = send_outbox(&client, vec![0u8; OUTBOX_SOFT_QUEUE_LIMIT_BYTES]);
        assert!(!outbox_backpressured(&client));
        // A second frame pushes total bytes past the soft limit.
        let _ = send_outbox(&client, vec![0u8; 1]);
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

    // ── parse_terminal_queries ──

    #[test]
    fn parse_tq_da1_bare() {
        let results = parse_terminal_queries(b"\x1b[c", (24, 80), (0, 0)).responses;
        assert_eq!(results.len(), 1);
        assert!(results[0].starts_with("\x1b[?64;"));
    }

    #[test]
    fn parse_tq_da1_with_zero_param() {
        let results = parse_terminal_queries(b"\x1b[0c", (24, 80), (0, 0)).responses;
        assert_eq!(results.len(), 1);
        assert!(results[0].starts_with("\x1b[?64;"));
    }

    #[test]
    fn parse_tq_dsr_cursor_position() {
        let results = parse_terminal_queries(b"\x1b[6n", (24, 80), (5, 10)).responses;
        assert_eq!(results.len(), 1);
        assert_eq!(results[0], "\x1b[6;11R");
    }

    #[test]
    fn parse_tq_dsr_status() {
        let results = parse_terminal_queries(b"\x1b[5n", (24, 80), (0, 0)).responses;
        assert_eq!(results.len(), 1);
        assert_eq!(results[0], "\x1b[0n");
    }

    #[test]
    fn parse_tq_window_size_cells() {
        let results = parse_terminal_queries(b"\x1b[18t", (24, 80), (0, 0)).responses;
        assert_eq!(results.len(), 1);
        assert_eq!(results[0], "\x1b[8;24;80t");
    }

    #[test]
    fn parse_tq_window_size_pixels() {
        let results = parse_terminal_queries(b"\x1b[14t", (30, 100), (0, 0)).responses;
        assert_eq!(results.len(), 1);
        assert_eq!(results[0], "\x1b[4;480;800t");
    }

    #[test]
    fn parse_tq_multiple_queries() {
        let data = b"\x1b[c\x1b[6n\x1b[5n";
        let results = parse_terminal_queries(data, (24, 80), (2, 3)).responses;
        assert_eq!(results.len(), 3);
        assert!(results[0].starts_with("\x1b[?64;"));
        assert_eq!(results[1], "\x1b[3;4R");
        assert_eq!(results[2], "\x1b[0n");
    }

    #[test]
    fn parse_tq_question_mark_sequences_skipped() {
        let results = parse_terminal_queries(b"\x1b[?1h", (24, 80), (0, 0)).responses;
        assert!(results.is_empty());
    }

    #[test]
    fn parse_tq_unknown_final_byte_ignored() {
        let results = parse_terminal_queries(b"\x1b[42z", (24, 80), (0, 0)).responses;
        assert!(results.is_empty());
    }

    #[test]
    fn parse_tq_empty_input() {
        let results = parse_terminal_queries(b"", (24, 80), (0, 0)).responses;
        assert!(results.is_empty());
    }

    #[test]
    fn parse_tq_plain_text_no_csi() {
        let results = parse_terminal_queries(b"hello world", (24, 80), (0, 0)).responses;
        assert!(results.is_empty());
    }

    #[test]
    fn parse_tq_interleaved_with_text() {
        let results = parse_terminal_queries(b"abc\x1b[cdef\x1b[6n", (24, 80), (1, 2)).responses;
        assert_eq!(results.len(), 2);
    }

    // ── parse_terminal_queries: OSC ──

    #[test]
    fn parse_tq_osc11_background_color_bel() {
        let results = parse_terminal_queries(b"\x1b]11;?\x07", (24, 80), (0, 0)).responses;
        assert_eq!(results.len(), 1);
        assert_eq!(results[0], "\x1b]11;rgb:0000/0000/0000\x1b\\");
    }

    #[test]
    fn parse_tq_osc11_background_color_st() {
        let results = parse_terminal_queries(b"\x1b]11;?\x1b\\", (24, 80), (0, 0)).responses;
        assert_eq!(results.len(), 1);
        assert_eq!(results[0], "\x1b]11;rgb:0000/0000/0000\x1b\\");
    }

    #[test]
    fn parse_tq_osc10_foreground_color() {
        let results = parse_terminal_queries(b"\x1b]10;?\x07", (24, 80), (0, 0)).responses;
        assert_eq!(results.len(), 1);
        assert_eq!(results[0], "\x1b]10;rgb:ffff/ffff/ffff\x1b\\");
    }

    #[test]
    fn parse_tq_osc4_palette_color_0() {
        let results = parse_terminal_queries(b"\x1b]4;0;?\x07", (24, 80), (0, 0)).responses;
        assert_eq!(results.len(), 1);
        assert_eq!(results[0], "\x1b]4;0;rgb:0000/0000/0000\x1b\\");
    }

    #[test]
    fn parse_tq_osc4_palette_color_1() {
        let results = parse_terminal_queries(b"\x1b]4;1;?\x07", (24, 80), (0, 0)).responses;
        assert_eq!(results.len(), 1);
        assert_eq!(results[0], "\x1b]4;1;rgb:8080/0000/0000\x1b\\");
    }

    #[test]
    fn parse_tq_osc_mixed_with_csi() {
        let results =
            parse_terminal_queries(b"\x1b]11;?\x07\x1b[c\x1b]4;0;?\x07", (24, 80), (0, 0))
                .responses;
        assert_eq!(results.len(), 3);
        assert!(results[0].starts_with("\x1b]11;"));
        assert!(results[1].starts_with("\x1b[?64;"));
        assert!(results[2].starts_with("\x1b]4;0;"));
    }

    // ── OSC 7 working-directory reports ──

    #[test]
    fn osc7_plain_bel_terminated() {
        let scan =
            parse_terminal_queries(b"\x1b]7;file:///home/user/project\x07", (24, 80), (0, 0));
        assert_eq!(scan.osc7_cwd.as_deref(), Some("/home/user/project"));
        // A cwd report is not a query — nothing goes back into the PTY.
        assert!(scan.responses.is_empty());
    }

    #[test]
    fn osc7_st_terminated_localhost() {
        let scan = parse_terminal_queries(b"\x1b]7;file://localhost/tmp\x1b\\", (24, 80), (0, 0));
        assert_eq!(scan.osc7_cwd.as_deref(), Some("/tmp"));
    }

    #[test]
    fn osc7_percent_decoded() {
        let scan =
            parse_terminal_queries(b"\x1b]7;file:///a%20dir/caf%C3%A9\x07", (24, 80), (0, 0));
        assert_eq!(scan.osc7_cwd.as_deref(), Some("/a dir/café"));
    }

    #[test]
    fn osc7_own_hostname_accepted() {
        let host = local_hostname();
        if host.is_empty() {
            return;
        }
        let payload = format!("\x1b]7;file://{host}/srv\x07");
        let scan = parse_terminal_queries(payload.as_bytes(), (24, 80), (0, 0));
        assert_eq!(scan.osc7_cwd.as_deref(), Some("/srv"));
    }

    #[test]
    fn osc7_foreign_host_ignored() {
        // A remote-ssh shell reports the remote host; its path is not local.
        let scan =
            parse_terminal_queries(b"\x1b]7;file://elsewhere.example/tmp\x07", (24, 80), (0, 0));
        assert_eq!(scan.osc7_cwd, None);
    }

    #[test]
    fn osc7_non_absolute_rejected() {
        // No path after the host at all.
        let scan = parse_terminal_queries(b"\x1b]7;file://localhost\x07", (24, 80), (0, 0));
        assert_eq!(scan.osc7_cwd, None);
        // Percent-encoded slash is not a literal path separator.
        let scan = parse_terminal_queries(b"\x1b]7;file://%2Ftmp\x07", (24, 80), (0, 0));
        assert_eq!(scan.osc7_cwd, None);
        // Not a file:// URL.
        let scan = parse_terminal_queries(b"\x1b]7;http://localhost/x\x07", (24, 80), (0, 0));
        assert_eq!(scan.osc7_cwd, None);
    }

    #[test]
    fn osc7_malformed_escapes_rejected() {
        for payload in [
            &b"\x1b]7;file:///a%GGb\x07"[..],    // non-hex escape
            &b"\x1b]7;file:///a%2\x07"[..],      // truncated escape
            &b"\x1b]7;file:///a%00b\x07"[..],    // embedded NUL
            &b"\x1b]7;file:///a%FF\x07"[..],     // invalid UTF-8 after decode
            &b"\x1b]7;file:///unterminated"[..], // no BEL/ST terminator
        ] {
            let scan = parse_terminal_queries(payload, (24, 80), (0, 0));
            assert_eq!(scan.osc7_cwd, None, "payload {payload:?}");
        }
    }

    #[test]
    fn osc7_oversize_dropped() {
        let max = "/".to_owned() + &"a".repeat(blit_remote::TERM_CWD_MAX - 1);
        let ok = format!("\x1b]7;file://{max}\x07");
        let scan = parse_terminal_queries(ok.as_bytes(), (24, 80), (0, 0));
        assert_eq!(scan.osc7_cwd.as_deref(), Some(max.as_str()));

        let over = format!("\x1b]7;file://{max}a\x07");
        let scan = parse_terminal_queries(over.as_bytes(), (24, 80), (0, 0));
        assert_eq!(scan.osc7_cwd, None);
    }

    #[test]
    fn osc7_last_report_in_chunk_wins() {
        let scan = parse_terminal_queries(
            b"\x1b]7;file:///first\x07output\x1b]7;file:///second\x07",
            (24, 80),
            (0, 0),
        );
        assert_eq!(scan.osc7_cwd.as_deref(), Some("/second"));
    }

    #[test]
    fn osc7_dedupe_same_cwd_one_push() {
        let mut stored = None;
        let first = note_osc7_cwd(&mut stored, 3, Some("/tmp".into()));
        assert_eq!(first, Some(blit_remote::msg_term_cwd_event(3, "/tmp")));
        // Shells re-emit per prompt: an identical repeat pushes nothing.
        assert_eq!(note_osc7_cwd(&mut stored, 3, Some("/tmp".into())), None);
        // A change pushes again (last write wins).
        assert_eq!(
            note_osc7_cwd(&mut stored, 3, Some("/var".into())),
            Some(blit_remote::msg_term_cwd_event(3, "/var"))
        );
        // Chunks without a report leave the store untouched.
        assert_eq!(note_osc7_cwd(&mut stored, 3, None), None);
        assert_eq!(stored.as_deref(), Some("/var"));
    }

    #[test]
    fn poll_prefers_osc7_over_kernel() {
        let mut kernel_called = false;
        let cwd = resolve_term_cwd(Some("/from-osc7"), || {
            kernel_called = true;
            Some("/from-kernel".into())
        });
        assert_eq!(cwd.as_deref(), Some("/from-osc7"));
        assert!(!kernel_called, "OSC 7 hit must not touch the kernel");

        let cwd = resolve_term_cwd(None, || Some("/from-kernel".into()));
        assert_eq!(cwd.as_deref(), Some("/from-kernel"));
        assert_eq!(resolve_term_cwd(None, || None), None);
    }

    // ── build_search_results_msg ──

    #[test]
    fn search_results_empty() {
        let msg = build_search_results_msg(42, &[]);
        assert_eq!(msg[0], S2C_SEARCH_RESULTS);
        assert_eq!(u16::from_le_bytes([msg[1], msg[2]]), 42);
        assert_eq!(u16::from_le_bytes([msg[3], msg[4]]), 0);
        assert_eq!(msg.len(), 5);
    }

    #[test]
    fn search_results_single() {
        let results = vec![SearchResultRow {
            pty_id: 7,
            score: 100,
            primary_source: 1,
            matched_sources: 3,
            context: "hello".into(),
            scroll_offset: Some(42),
        }];
        let msg = build_search_results_msg(1, &results);
        assert_eq!(msg[0], S2C_SEARCH_RESULTS);
        assert_eq!(u16::from_le_bytes([msg[3], msg[4]]), 1);
        let pty_id = u16::from_le_bytes([msg[5], msg[6]]);
        assert_eq!(pty_id, 7);
        let score = u32::from_le_bytes([msg[7], msg[8], msg[9], msg[10]]);
        assert_eq!(score, 100);
        assert_eq!(msg[11], 1);
        assert_eq!(msg[12], 3);
        let scroll = u32::from_le_bytes([msg[13], msg[14], msg[15], msg[16]]);
        assert_eq!(scroll, 42);
        let ctx_len = u16::from_le_bytes([msg[17], msg[18]]) as usize;
        assert_eq!(ctx_len, 5);
        assert_eq!(&msg[19..19 + ctx_len], b"hello");
    }

    #[test]
    fn search_results_none_scroll_offset() {
        let results = vec![SearchResultRow {
            pty_id: 1,
            score: 0,
            primary_source: 0,
            matched_sources: 0,
            context: String::new(),
            scroll_offset: None,
        }];
        let msg = build_search_results_msg(0, &results);
        let scroll = u32::from_le_bytes([msg[13], msg[14], msg[15], msg[16]]);
        assert_eq!(scroll, u32::MAX);
    }

    // ── client-supplied view sizes ──

    /// An ordinary viewport passes through untouched — the clamp must not
    /// quietly reshape real terminals.
    #[test]
    fn clamp_view_size_leaves_real_viewports_alone() {
        for (rows, cols) in [(24, 80), (60, 200), (1, 1), (540, 900)] {
            assert_eq!(clamp_view_size(rows, cols), (rows, cols), "{rows}x{cols}");
        }
    }

    /// `C2S_RESIZE` carries two raw u16s and only rejected zero, so one
    /// client could name a grid of 4.29 billion cells and — being the
    /// minimum across clients when it is the only one — have the terminal
    /// allocated at that size.
    #[test]
    fn clamp_view_size_bounds_a_hostile_resize() {
        let (rows, cols) = clamp_view_size(u16::MAX, u16::MAX);
        assert!(
            rows <= MAX_VIEW_DIM && cols <= MAX_VIEW_DIM,
            "{rows}x{cols}"
        );
        assert!(
            rows as usize * cols as usize <= blit_remote::MAX_CELL_COUNT,
            "{rows}x{cols} is past what a frame can describe"
        );
    }

    /// The cell budget binds before the per-axis cap: 4096x4096 is under both
    /// dimension limits but 16.7M cells, which no receiver would accept.
    #[test]
    fn clamp_view_size_respects_the_frame_cell_budget() {
        let (rows, cols) = clamp_view_size(MAX_VIEW_DIM, MAX_VIEW_DIM);
        assert_eq!(rows, MAX_VIEW_DIM);
        assert!(
            rows as usize * cols as usize <= blit_remote::MAX_CELL_COUNT,
            "{rows}x{cols}"
        );
        assert!(cols >= 1, "never clamps a dimension to zero");
    }

    /// A tall, narrow ask keeps its width rather than being squared off.
    #[test]
    fn clamp_view_size_never_yields_a_zero_dimension() {
        for rows in [1u16, 2, 1000, MAX_VIEW_DIM] {
            let (r, c) = clamp_view_size(rows, u16::MAX);
            assert!(r >= 1 && c >= 1, "{rows} -> {r}x{c}");
            assert!(r as usize * c as usize <= blit_remote::MAX_CELL_COUNT);
        }
    }

    // ── allocate_pty_id ──

    #[test]
    fn allocate_pty_id_empty_session() {
        let mut sess = Session::new();
        assert_eq!(sess.allocate_pty_id(0), Some(1));
    }

    #[test]
    fn allocate_pty_id_rotates() {
        let mut sess = Session::new();
        // Sequential allocations return increasing IDs (not always 1).
        assert_eq!(sess.allocate_pty_id(0), Some(1));
        assert_eq!(sess.allocate_pty_id(0), Some(2));
        assert_eq!(sess.allocate_pty_id(0), Some(3));
    }

    #[test]
    fn allocate_pty_id_wraps_at_max() {
        let mut sess = Session::new();
        sess.next_pty_id = u16::MAX;
        assert_eq!(sess.allocate_pty_id(0), Some(u16::MAX));
        // Next allocation wraps to 1.
        assert_eq!(sess.allocate_pty_id(0), Some(1));
    }

    // ── create refusal ──

    /// A `CREATE2` prefix: 1 opcode + 2 nonce + 2 rows + 2 cols + 1 features,
    /// then `[tag_len:2]` at offset 8 and the tag bytes at 10.
    fn create2_with_tag(tag_len: u16, tag: &[u8]) -> Vec<u8> {
        let mut msg = vec![0u8; 8];
        msg.extend_from_slice(&tag_len.to_le_bytes());
        msg.extend_from_slice(tag);
        msg
    }

    #[test]
    fn create2_tag_reads_a_well_formed_tag() {
        let msg = create2_with_tag(3, b"abc");
        assert_eq!(create2_tag(&msg), Ok("abc"));
        // Trailing bytes belong to the later fields, not the tag.
        let msg = create2_with_tag(3, b"abcdef");
        assert_eq!(create2_tag(&msg), Ok("abc"));
        assert_eq!(create2_tag(&create2_with_tag(0, b"")), Ok(""));
    }

    /// The dangerous one: an overrunning `tag_len` used to yield an empty tag
    /// and leave the cursor past the end, so a command-bearing create with no
    /// cwd or deadline to bounds-check it spawned the default shell instead.
    #[test]
    fn create2_tag_refuses_a_length_past_the_end() {
        assert_eq!(
            create2_tag(&create2_with_tag(9, b"abc")),
            Err("tag length past end of message")
        );
        // Exactly one byte short still overruns.
        assert_eq!(
            create2_tag(&create2_with_tag(4, b"abc")),
            Err("tag length past end of message")
        );
    }

    #[test]
    fn create2_tag_refuses_a_non_utf8_tag() {
        assert_eq!(
            create2_tag(&create2_with_tag(2, &[0xff, 0xfe])),
            Err("tag is not valid UTF-8")
        );
    }

    #[test]
    fn oversize_list_field_names_the_offender() {
        let big = "x".repeat(u16::MAX as usize + 1);
        assert_eq!(oversize_list_field("tag", None), None);
        assert_eq!(oversize_list_field("tag", Some("cmd")), None);
        assert_eq!(oversize_list_field(&big, None), Some("tag"));
        assert_eq!(oversize_list_field("tag", Some(&big)), Some("command"));
    }

    #[test]
    fn oversize_list_field_allows_exactly_u16_max() {
        // The length prefix holds this exactly; only one more byte truncates.
        let exact = "x".repeat(u16::MAX as usize);
        assert_eq!(oversize_list_field(&exact, Some(&exact)), None);
    }

    // ── retention ──

    fn at(base: Instant, secs: u64) -> Instant {
        base + Duration::from_secs(secs)
    }

    #[test]
    fn eviction_keeps_the_newest_when_over_the_count_bound() {
        let base = Instant::now();
        let exited = vec![(1, at(base, 0)), (2, at(base, 10)), (3, at(base, 20))];
        // Room for two: the oldest goes.
        assert_eq!(
            slots_to_evict(exited.clone(), at(base, 30), 2, Duration::ZERO),
            vec![1]
        );
        assert_eq!(
            slots_to_evict(exited.clone(), at(base, 30), 1, Duration::ZERO),
            vec![1, 2]
        );
        // Under the bound, nothing goes.
        assert!(slots_to_evict(exited, at(base, 30), 8, Duration::ZERO).is_empty());
    }

    #[test]
    fn eviction_count_bound_is_off_at_zero() {
        let base = Instant::now();
        let exited = vec![(1, at(base, 0)), (2, at(base, 1))];
        assert!(slots_to_evict(exited, at(base, 999), 0, Duration::ZERO).is_empty());
    }

    #[test]
    fn eviction_linger_is_off_by_default() {
        // The default has to leave old output alone — someone reading a
        // result back an hour later is a supported thing to do.
        let base = Instant::now();
        let exited = vec![(1, at(base, 0))];
        assert!(
            slots_to_evict(
                exited,
                at(base, 100_000),
                DEFAULT_MAX_EXITED,
                DEFAULT_EXITED_LINGER
            )
            .is_empty()
        );
    }

    #[test]
    fn eviction_applies_the_linger_bound_when_set() {
        let base = Instant::now();
        let exited = vec![(1, at(base, 0)), (2, at(base, 50)), (3, at(base, 90))];
        // At t=100 with a 60s linger only 1 is old enough: 2 has been gone
        // 50s and 3 only 10s.
        assert_eq!(
            slots_to_evict(exited.clone(), at(base, 100), 0, Duration::from_secs(60)),
            vec![1]
        );
        // Push `now` out far enough and 2 crosses the line too.
        assert_eq!(
            slots_to_evict(exited, at(base, 120), 0, Duration::from_secs(60)),
            vec![1, 2]
        );
    }

    #[test]
    fn eviction_does_not_repeat_an_id_caught_by_both_bounds() {
        let base = Instant::now();
        let exited = vec![(1, at(base, 0)), (2, at(base, 1)), (3, at(base, 2))];
        // 1 is both the oldest over the count bound and past the linger.
        let doomed = slots_to_evict(exited, at(base, 100), 2, Duration::from_secs(50));
        let mut unique = doomed.clone();
        unique.dedup();
        assert_eq!(doomed, unique);
        assert_eq!(doomed, vec![1, 2, 3]);
    }

    #[test]
    fn arming_a_deadline_stands_down_a_pending_kill() {
        let now = Instant::now();
        // The case that matters: a refresh arriving inside the
        // SIGTERM→SIGKILL grace. It must cancel the pending kill, or the
        // terminal dies anyway a few seconds after the client said keep it.
        let (deadline, stop, reason) = armed_deadline(now, 30_000);
        assert_eq!(deadline, Some(now + Duration::from_secs(30)));
        assert_eq!(stop, None);
        assert_eq!(reason, blit_remote::EXIT_REASON_NORMAL);
    }

    #[test]
    fn clearing_a_deadline_disarms_everything() {
        let now = Instant::now();
        let (deadline, stop, reason) = armed_deadline(now, 0);
        assert_eq!(deadline, None);
        assert_eq!(stop, None);
        assert_eq!(reason, blit_remote::EXIT_REASON_NORMAL);
    }

    #[test]
    fn pty_budget_detail_separates_the_two_exhaustions() {
        // The operator fix differs — raise the cap, versus wait for ids to
        // free up — so the detail has to tell them apart.
        assert!(pty_budget_detail(256, 256).contains("cap reached (256)"));
        assert!(pty_budget_detail(300, 256).contains("cap reached"));
        // Uncapped, or under the cap: the only way to get here is a full
        // id space.
        assert!(pty_budget_detail(65535, 0).contains("id space"));
        assert!(pty_budget_detail(10, 256).contains("id space"));
        // The caller must pass the live count, not `ptys.len()`.  With
        // retention holding tens of thousands of exited slots the id space
        // can run out while the live cap is nowhere near — reporting that as
        // "cap reached" sends the operator to raise a limit that is not the
        // one they hit.
        assert!(pty_budget_detail(3, 256).contains("id space"));
    }

    // ── try_send_update ──

    #[test]
    fn try_send_no_change() {
        let mut client = test_client();
        let frame = sample_frame("x");
        let now = Instant::now();
        let outcome = try_send_update(&mut client, 1, frame, None, now, false);
        assert!(matches!(outcome, SendOutcome::NoChange));
    }

    #[test]
    fn try_send_sent() {
        let (mut client, _rx) = test_client_with_capacity(8);
        let frame = sample_frame("x");
        let now = Instant::now();
        let outcome = try_send_update(
            &mut client,
            1,
            frame.clone(),
            Some(vec![1, 2, 3]),
            now,
            true,
        );
        assert!(matches!(outcome, SendOutcome::Sent));
        assert!(client.last_sent.contains_key(&1));
    }

    #[test]
    fn try_send_backpressured_on_disconnect() {
        let (mut client, rx) = test_client_with_capacity(0);
        let frame = sample_frame("x");
        let now = Instant::now();
        // Drop the receiver to simulate a disconnected client.
        drop(rx);
        let outcome = try_send_update(
            &mut client,
            1,
            frame.clone(),
            Some(vec![1, 2, 3]),
            now,
            true,
        );
        assert!(matches!(outcome, SendOutcome::Backpressured));
        assert!(
            client.last_sent.contains_key(&1),
            "last_sent should advance even on disconnect"
        );
    }

    /// With the family disabled, every nonce-bearing LSP request still gets
    /// its one reply. Dropping them left a client that ignores the feature
    /// bit — or that raced a mid-session disable — awaiting a promise that
    /// could never resolve, where KV and NET both answer PERMISSION.
    #[test]
    fn disabled_lsp_refuses_every_nonce() {
        use blit_remote::lsp::*;

        let (out, mut rx) = mpsc::unbounded_channel::<Vec<u8>>();

        refuse_lsp_message(&msg_lsp_open(1, 0, 0, "/tmp"), &out);
        let reply = rx.try_recv().expect("a reply");
        let opened = parse_lsp_opened(&reply).unwrap();
        assert_eq!(opened.nonce, 1);
        assert_eq!(opened.lsp_id, LSP_ID_INVALID);
        assert_eq!(opened.status, LSP_STATUS_PERMISSION);

        let query = msg_lsp_query(&LspQueryRequest {
            nonce: 2,
            lsp_id: 7,
            kind: LSP_QUERY_DEFINITION,
            flags: 0,
            line: 0,
            col: 0,
            path: "a.rs",
            arg: "",
        });
        refuse_lsp_message(&query, &out);
        let resp = parse_lsp_query_resp(&rx.try_recv().expect("a reply")).unwrap();
        assert_eq!((resp.nonce, resp.status), (2, LSP_STATUS_PERMISSION));

        refuse_lsp_message(&msg_lsp_servers(3), &out);
        let (nonce, status, ..) = parse_lsp_servers_resp(&rx.try_recv().expect("a reply")).unwrap();
        assert_eq!((nonce, status), (3, LSP_STATUS_PERMISSION));

        refuse_lsp_message(&msg_lsp_stop(4, 1), &out);
        let (nonce, status) = parse_lsp_stopped(&rx.try_recv().expect("a reply")).unwrap();
        assert_eq!((nonce, status), (4, LSP_STATUS_PERMISSION));

        // Fire-and-forget opcodes have no reply to give, disabled or not.
        refuse_lsp_message(&msg_lsp_ack(1, 0, 1), &out);
        refuse_lsp_message(&msg_lsp_buffer(1, 0, "a.rs", b"x"), &out);
        assert!(rx.try_recv().is_err(), "no reply is owed for these");
    }

    /// LSP dispatch glue: refusals, unknown ids, and the daemon-wide
    /// verbs answer synchronously and correctly without any language
    /// server installed (engine behavior is covered in blit-lsp).
    #[tokio::test]
    async fn lsp_message_flow() {
        use blit_remote::lsp::*;

        let (out, mut rx) = mpsc::unbounded_channel::<Vec<u8>>();
        let mut conns = LspConns::default();

        // A bad path refuses with the sentinel id.
        handle_lsp_message(
            &msg_lsp_open(1, 0, 0, "/blit-no-such-path"),
            &mut conns,
            &out,
            false,
        )
        .await;
        let refusal = rx.try_recv().expect("synchronous refusal");
        let opened = parse_lsp_opened(&refusal).unwrap();
        assert_eq!(opened.lsp_id, LSP_ID_INVALID);
        assert_eq!(opened.status, LSP_STATUS_NOT_FOUND);

        // A markerless directory names the problem.
        let dir = std::env::temp_dir().join(format!("blit-lsp-none-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        handle_lsp_message(
            &msg_lsp_open(2, 0, 0, dir.to_str().unwrap()),
            &mut conns,
            &out,
            false,
        )
        .await;
        let refusal = rx.try_recv().expect("synchronous refusal");
        let opened = parse_lsp_opened(&refusal).unwrap();
        assert_eq!(opened.lsp_id, LSP_ID_INVALID);
        assert_eq!(opened.status, LSP_STATUS_NOT_FOUND);
        assert!(opened.detail.contains("no known project markers"));
        let _ = std::fs::remove_dir_all(&dir);

        // Unknown flag bits are INVALID.
        handle_lsp_message(&msg_lsp_open(3, 0x80, 0, "/"), &mut conns, &out, false).await;
        let refusal = rx.try_recv().expect("synchronous refusal");
        assert_eq!(
            parse_lsp_opened(&refusal).unwrap().status,
            LSP_STATUS_INVALID
        );

        // A query against an unknown attachment answers UNKNOWN_ID.
        let query = msg_lsp_query(&LspQueryRequest {
            nonce: 9,
            lsp_id: 42,
            kind: LSP_QUERY_DEFINITION,
            flags: 0,
            line: 0,
            col: 0,
            path: "a.rs",
            arg: "",
        });
        handle_lsp_message(&query, &mut conns, &out, false).await;
        let resp = rx.try_recv().expect("synchronous refusal");
        let r = parse_lsp_query_resp(&resp).unwrap();
        let (nonce, status) = (r.nonce, r.status);
        assert_eq!((nonce, status), (9, LSP_STATUS_UNKNOWN_ID));

        // The daemon-wide verbs answer without any backend running.
        handle_lsp_message(&msg_lsp_servers(4), &mut conns, &out, false).await;
        let resp = rx.try_recv().expect("synchronous LSP_SERVERS");
        let (nonce, status, _, _) = parse_lsp_servers_resp(&resp).unwrap();
        assert_eq!((nonce, status), (4, LSP_STATUS_OK));

        handle_lsp_message(&msg_lsp_stop(5, 999), &mut conns, &out, false).await;
        let resp = rx.try_recv().expect("synchronous LSP_STOPPED");
        assert_eq!(parse_lsp_stopped(&resp), Some((5, LSP_STATUS_NOT_FOUND)));

        // ACK and CLOSE on unknown ids are silent no-ops.
        handle_lsp_message(
            &msg_lsp_ack(7, LSP_STREAM_STATE, 1),
            &mut conns,
            &out,
            false,
        )
        .await;
        handle_lsp_message(&msg_lsp_close(7), &mut conns, &out, false).await;
        assert!(rx.try_recv().is_err());
    }
}
