use std::collections::BTreeMap;

use lz4_flex::{compress_prepend_size, decompress_size_prepended};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

/// Filesystem state sync (docs/fs-watch.md): opcodes, record
/// codecs, and the client-side mirror reducer.
pub mod fs;

/// Git introspection (docs/git.md): opcodes, record codecs, and the
/// client-side state mirror reducer.
pub mod git;

/// Language intelligence (docs/design/lsp.md): opcodes, record codecs,
/// and the client-side state/diagnostics mirror reducers.
pub mod lsp;

/// Server KV store (docs/design/kv.md): opcodes, record codecs, and the
/// client-side mirror reducer.
pub mod kv;

/// TCP and UDP relay (docs/design/net.md): opcodes, flags, and the
/// message builders both ends share.
pub mod net;

/// Cap on any single LZ4-decompressed payload, protocol-wide
/// (docs/protocol.md "Compressed payloads"). Receivers check the prepended
/// size against it *before* allocating, so a hostile or corrupt length
/// cannot force a giant allocation.
pub const MAX_DECOMPRESSED: usize = 64 * 1024 * 1024;

/// Most cells (`rows * cols`) a single frame may describe.
///
/// 500 rows x 1000 cols = 500,000 cells x 12 bytes = 6 MB — generous for any
/// real terminal, while keeping a frame claiming `rows=65535, cols=65535`
/// from asking for 48 GiB. Public because it binds at both ends: a receiver
/// rejects a frame above it, so a server that sizes a grid past it produces
/// frames no client will render.
pub const MAX_CELL_COUNT: usize = 500_000;

/// Longest string a `u16` length prefix can describe.
pub(crate) const MAX_STR: usize = u16::MAX as usize;

/// Write a `u16`-length-prefixed UTF-8 string, clipping rather than wrapping
/// the prefix.
///
/// `len as u16` on an overlong string writes a length the reader believes, so
/// every following field of the message is read at the wrong offset — one
/// oversized value corrupts the whole response rather than just itself. The
/// inputs are paths, ref names, symbol names and match text, none of which any
/// protocol rule bounds, and the escaping some of them go through expands a
/// non-UTF-8 byte roughly sixfold, so ~11 KB of raw bytes can pass 64 KiB. A
/// visibly shortened value costs one unhelpful row; a wrapped prefix costs the
/// whole message.
pub(crate) fn push_str(buf: &mut Vec<u8>, s: &str) {
    let b = s.as_bytes();
    let b = if b.len() > MAX_STR {
        // Back off to a char boundary so the field stays valid UTF-8, which
        // the decoders require.
        let mut end = MAX_STR;
        while end > 0 && !s.is_char_boundary(end) {
            end -= 1;
        }
        &b[..end]
    } else {
        b
    };
    buf.extend_from_slice(&(b.len() as u16).to_le_bytes());
    buf.extend_from_slice(b);
}

pub const CELL_SIZE: usize = 12;
const TITLE_PRESENT: u16 = 1 << 15;
const OPS_PRESENT: u16 = 1 << 14;
const STRINGS_PRESENT: u16 = 1 << 13;
const LINE_FLAGS_PRESENT: u16 = 1 << 12;
const TITLE_LEN_MASK: u16 = LINE_FLAGS_PRESENT - 1;

/// Per-row flag: this row's content continues on the next row (line wrap).
pub const ROW_FLAG_WRAPPED: u8 = 1 << 0;

/// Sentinel value for content_len indicating the cell's text lives in the
/// overflow string table.  Bytes 8-11 then hold an FNV-1a hash of the full
/// UTF-8 string (for diff correctness), and the actual string is stored in
/// `FrameState::overflow` keyed by cell index.
const CONTENT_OVERFLOW: u8 = 7;

/// Cell flags1 bit 6: this cell carries an OSC 8 hyperlink.  The link's
/// identity lives in `FrameState::cell_links`; the bit exists so that (a) the
/// render hot loop can style a link without a side-table lookup and (b) a cell
/// gaining or losing a link is visible to the byte-wise cell diff.
pub const CELL_FLAG1_LINK: u8 = 1 << 6;

/// Sentinel `uri_count` meaning "hyperlink state is unchanged from the previous
/// frame — keep what you have".  Distinguishes "no links" (count 0, clear the
/// table) from "nothing to say" without spending bytes on an unchanged table.
const LINKS_UNCHANGED: u16 = u16::MAX;

/// Upper bound on distinct hyperlink URIs tracked per frame.  Ids are u16 and
/// 0 means "no link", so 0xFFFF is reserved as the `LINKS_UNCHANGED` sentinel.
pub const MAX_LINK_ID: u16 = u16::MAX - 1;

/// Longest OSC 8 URI accepted from the PTY.  Anything longer is dropped rather
/// than truncated — a truncated URI is a *different* URI, and silently
/// rewriting a link target is worse than showing no link at all.
pub const MAX_LINK_URI: usize = 4096;

const ENABLE_SCROLL_OPS: bool = true;
const MODE_ECHO: u16 = 1 << 9;
const MODE_ICANON: u16 = 1 << 10;

const OP_COPY_RECT: u8 = 0x01;
const OP_FILL_RECT: u8 = 0x02;
const OP_PATCH_CELLS: u8 = 0x03;

pub const C2S_INPUT: u8 = 0x00;
/// Desired viewport size(s): [0x01][pty_id:2][rows:2][cols:2]...
/// Clients may batch multiple PTY resize entries in one message. The server
/// mediates these per-client desired sizes into each PTY's effective size.
/// A `rows, cols` pair of `0, 0` clears this client's desired size for that PTY.
pub const C2S_RESIZE: u8 = 0x01;
pub const C2S_SCROLL: u8 = 0x02;
pub const C2S_ACK: u8 = 0x03;
pub const C2S_DISPLAY_RATE: u8 = 0x04;
pub const C2S_CLIENT_METRICS: u8 = 0x05;
/// Application-level keepalive: [0x08].  No payload.
/// Sent periodically by the client; the server treats it as a no-op
/// (but its arrival resets any server-side receive timeout).
pub const C2S_PING: u8 = 0x08;
/// Mouse event: [0x06][pty_id:2][type:1][button:1][col:2][row:2]
/// type: 0=down, 1=up, 2=move
/// button: 0=left, 1=mid, 2=right, 3=release, 64=wheel_up, 65=wheel_down
/// The server generates the correct escape sequence based on mouse_mode and mouse_encoding.
pub const C2S_MOUSE: u8 = 0x06;
/// Restart an exited PTY: [0x07][pty_id:2]
/// Server spawns a new shell in the same PTY slot, preserving the pty_id.
pub const C2S_RESTART: u8 = 0x07;
pub const C2S_CREATE: u8 = 0x10;
pub const C2S_FOCUS: u8 = 0x11;
pub const C2S_CLOSE: u8 = 0x12;
pub const C2S_SUBSCRIBE: u8 = 0x13;
pub const C2S_UNSUBSCRIBE: u8 = 0x14;
pub const C2S_SEARCH: u8 = 0x15;
pub const C2S_CREATE_AT: u8 = 0x16;
pub const C2S_CREATE_N: u8 = 0x17;
/// Generic create: [0x18][nonce:2][rows:2][cols:2][features:1][tag_len:2][tag:N][...optional fields]
/// Features: bit 0 = has src_pty_id (2 bytes after tag), bit 1 = has command (remaining bytes after length-prefixed cwd if present), bit 2 = has cwd ([len:2][utf8])
/// Server responds with S2C_CREATED_N using the same nonce.
pub const C2S_CREATE2: u8 = 0x18;
pub const CREATE2_HAS_SRC_PTY: u8 = 1 << 0;
pub const CREATE2_HAS_COMMAND: u8 = 1 << 1;
pub const CREATE2_HAS_CWD: u8 = 1 << 2;
/// Request exactly one correlated creation outcome: `S2C_CREATED_N` on
/// success, [`S2C_CREATE_FAILED`] on refusal.  Adds no trailing field.
/// Only set this when `S2C_HELLO` advertised [`FEATURE_CREATE_STATUS`] —
/// without the flag a refused create sends nothing at all and a
/// nonce-bearing client waits forever (docs/protocol.md, "Common status
/// registry").
pub const CREATE2_WANT_STATUS: u8 = 1 << 3;
/// Arm a server-enforced deadline at creation time: followed by `[ms:4]`,
/// placed after any cwd and *before* the command bytes, since `HAS_COMMAND`
/// runs to the end of the message and nothing can follow it.
///
/// Arming at create closes the window between spawning a terminal and
/// protecting it — a client that sends `C2S_DEADLINE` as a second message
/// leaves the terminal unbounded if it dies in between.
pub const CREATE2_HAS_DEADLINE: u8 = 1 << 4;
/// Read text from a PTY's scrollback + viewport: [0x19][nonce:2][pty_id:2][offset:4][limit:4][flags:1]
/// offset: number of lines to skip from the top (oldest = 0), or from the end if READ_TAIL is set
/// limit: max lines to return (0 = all)
/// flags: bit 0 = include ANSI styling, bit 1 = offset counts from the end
/// Server responds with S2C_TEXT using the same nonce.
pub const C2S_READ: u8 = 0x19;
pub const READ_ANSI: u8 = 1 << 0;
pub const READ_TAIL: u8 = 1 << 1;
/// Copy text from a range of absolute row/col positions in scrollback + viewport:
/// [0x1B][nonce:2][pty_id:2][start_tail:4][start_col:2][end_tail:4][end_col:2][flags:1]
/// start_tail/end_tail: physical row distance from the bottom (0 = last row).
/// start is the earlier position (closer to top), so start_tail >= end_tail.
/// flags: reserved (0 for now).
/// Server responds with S2C_TEXT using the same nonce.
pub const C2S_COPY_RANGE: u8 = 0x1B;
/// Send a signal to a PTY: [0x1A][pty_id:2][signal:4][flags:1]
/// signal is a raw libc signal number (e.g. SIGTERM=15, SIGKILL=9).
///
/// `flags` is optional and armed by `data.len() >= 8`; a 7-byte message keeps
/// the default.  That default is now the process group, not the session
/// leader alone — killing a shell used to leave its children running.
pub const C2S_KILL: u8 = 0x1A;
/// Signal only the session leader, the pre-`FEATURE_KILL_MODE` behaviour.
///
/// This is for the narrow case of addressing the leader *itself* — telling a
/// shell to exit without disturbing the jobs under it. It is not the right
/// choice for emulating a keystroke: the kernel delivers `^C` to the
/// terminal's foreground process group, which is what the default already
/// does, and sending `SIGINT` to the shell alone mostly gets ignored.
pub const KILL_LEADER_ONLY: u8 = 1 << 0;
/// Request a PTY's live working directory: [0x1C][nonce:2][pty_id:2]
pub const C2S_TERM_CWD: u8 = 0x1C;
/// Arm, refresh, or clear a terminal's deadline: [0x1D][pty_id:2][ms:4].
///
/// `ms` counts from when the server receives the message, so re-sending it
/// refreshes — that is what makes it usable as a dead-man switch: re-arm
/// every 30s and the terminal dies ~30s after the orchestrator does.
/// `ms = 0` clears the deadline entirely.
///
/// On expiry the server signals the process group with SIGTERM, waits
/// [`DEADLINE_STOP_GRACE_MS`], then SIGKILLs it, and the resulting
/// `S2C_EXITED` carries [`EXIT_REASON_DEADLINE`].
pub const C2S_DEADLINE: u8 = 0x1D;
/// Grace between the deadline's SIGTERM and its SIGKILL.  systemd's 90s is
/// wrong for the workload this exists for — an agent that has already
/// abandoned the terminal is not coming back to flush anything.
pub const DEADLINE_STOP_GRACE_MS: u32 = 5_000;
/// Move a scrolled view by a signed number of lines, relative to wherever
/// the server currently holds it: [0x1E][pty_id:2][delta:4 i32].
///
/// `C2S_SCROLL`'s absolute offset is measured from the live bottom, so it
/// only means what the client intended for as long as the bottom hasn't
/// moved.  Under a chatty app it moves while the message is in flight, and
/// the server re-anchors the client in the same window
/// (`S2C_SCROLL_OFFSET`) — so an absolute request computed from what the
/// user was looking at lands short by however many lines scrolled in
/// between.  A wheel notch, a page key and a drag are all relative motions
/// anyway; sent as one, they compose with the re-anchor instead of racing
/// it.  Absolute `C2S_SCROLL` remains right for the jumps that really are
/// absolute: home, end, and dragging the scrollbar.
///
/// The server clamps the result to the scrollback and answers with
/// `S2C_SCROLL_OFFSET` carrying where the view actually ended up.
/// Requires [`FEATURE_SCROLL_BY`].
pub const C2S_SCROLL_BY: u8 = 0x1E;

/// Keyboard input for a Wayland surface: [0x20][surface_id:2][data:N]
/// data contains evdev keycodes encoded as [keycode:4][pressed:1] sequences.
pub const C2S_SURFACE_INPUT: u8 = 0x20;
/// Pointer motion/button for a Wayland surface: [0x21][surface_id:2][type:1][button:1][x:2][y:2]
/// type: 0=down, 1=up, 2=move
/// button: DOM numbering — 0=left, 1=middle, 2=right, 3=back, 4=forward
/// x,y: pixel coordinates relative to the surface origin
pub const C2S_SURFACE_POINTER: u8 = 0x21;
/// Pointer axis/scroll for a Wayland surface: [0x22][surface_id:2][axis:1][value_x100:4_signed]
/// axis: 0=vertical, 1=horizontal
/// value_x100: scroll amount * 100 (signed, positive = down/right)
///
/// Superseded by [`C2S_SURFACE_POINTER_AXIS2`], which carries the device
/// source and discrete-detent count the Wayland protocol wants.  Kept for
/// older clients; the server maps it onto the same path with an unknown
/// source, so no `wl_pointer.axis_source` is emitted.
pub const C2S_SURFACE_POINTER_AXIS: u8 = 0x22;
/// Resize a Wayland surface: [0x23][surface_id:2][width:2][height:2][scale_120:2]
/// scale_120: requested presentation scale in 1/120th units
/// (60 = 0.5×, 120 = 1×, 240 = 2×; 0 = unspecified/1×).
pub const C2S_SURFACE_RESIZE: u8 = 0x23;
/// Set keyboard/pointer focus to a Wayland surface: [0x24][surface_id:2]
pub const C2S_SURFACE_FOCUS: u8 = 0x24;
/// Set clipboard content:
/// [0x25][mime_len:2][mime:N][data_len:4][data:N]
pub const C2S_CLIPBOARD_SET: u8 = 0x25;
/// Take ownership of the primary selection (what middle click pastes):
/// [0x33][mime_len:2][mime:N][data_len:4][data:N]
///
/// Same framing as [`C2S_CLIPBOARD_SET`], a different selection. PRIMARY is
/// not readable from the web platform, so a client that wants to own it
/// pushes the bytes up front instead of the compositor fetching them from
/// the owner on demand; send it immediately before the
/// [`C2S_SURFACE_POINTER`] carrying the middle button, so the offer is
/// advertised by the time the app reacts. Displaces any Wayland client
/// that held the selection, and is displaced in turn when one claims it.
pub const C2S_PRIMARY_SET: u8 = 0x33;
/// Request a list of all compositor surfaces: [0x26]
pub const C2S_SURFACE_LIST: u8 = 0x26;
/// Request a screenshot of a surface:
/// [0x27][surface_id:2]              — legacy (defaults to PNG lossless)
/// [0x27][surface_id:2][format:1][quality:1] — extended
/// format: 0 = PNG, 1 = AVIF.  quality: 0 = lossless, 1–100 = lossy (AVIF only).
pub const C2S_SURFACE_CAPTURE: u8 = 0x27;
pub const CAPTURE_FORMAT_PNG: u8 = 0;
pub const CAPTURE_FORMAT_AVIF: u8 = 1;
/// Subscribe to surface frame updates:
/// [0x28][surface_id:2]                                              — legacy (server defaults)
/// [0x28][surface_id:2][codec:1][bandwidth:1][speed:1]                — extended
/// [0x28][surface_id:2][codec:1][bandwidth:1][speed:1][w:2][h:2]      — scaled
///
/// codec: CODEC_SUPPORT_* bitmask restricting which codecs the server may use
///        for this surface.  0 = use connection-level default (from C2S_CLIENT_FEATURES).
///
/// bandwidth: how many bits this surface may spend.
///   0 = server default, 1 = low, 2 = medium, 3 = high, 4 = ultra.
///   10–255 = custom AV1 quantizer (wire value IS the quantizer).
///   See `SURFACE_BANDWIDTH_*` constants.
///
/// speed: how much encoder time a frame may cost.  Independent of bandwidth.
///   0 = server default, 1 = slow, 2 = medium, 3 = fast, 4 = realtime.
///   10–255 = custom (10 = slowest, 255 = fastest).
///   See `SURFACE_SPEED_*` constants.
///
/// width, height: optional fixed target size (in pixels) for this subscription.
///   When both are nonzero, the server encodes this surface at exactly
///   `width × height` for this client by scaling the native frame down
///   server-side, independent of the compositor's surface size.  Such
///   "scaled" subscriptions are excluded from the server's surface-size
///   mediation (they never pull the compositor surface smaller for other
///   viewers).  Both 0 (or fields absent) means the client participates in
///   mediation via C2S_SURFACE_RESIZE like today.
///
/// Re-subscribing to an already-subscribed surface updates the codec /
/// bandwidth / speed preferences and/or scaled size and forces encoder
/// recreation.
pub const C2S_SURFACE_SUBSCRIBE: u8 = 0x28;

/// Values for the `bandwidth` byte in C2S_SURFACE_SUBSCRIBE.
/// 0 means "use server default" (from the BLIT_SURFACE_BANDWIDTH env var).
pub const SURFACE_BANDWIDTH_DEFAULT: u8 = 0;
pub const SURFACE_BANDWIDTH_LOW: u8 = 1;
pub const SURFACE_BANDWIDTH_MEDIUM: u8 = 2;
pub const SURFACE_BANDWIDTH_HIGH: u8 = 3;
pub const SURFACE_BANDWIDTH_ULTRA: u8 = 4;

/// Values for the `speed` byte in C2S_SURFACE_SUBSCRIBE.
/// 0 means "use server default" (from the BLIT_SURFACE_SPEED env var).
pub const SURFACE_SPEED_DEFAULT: u8 = 0;
pub const SURFACE_SPEED_SLOW: u8 = 1;
pub const SURFACE_SPEED_MEDIUM: u8 = 2;
pub const SURFACE_SPEED_FAST: u8 = 3;
pub const SURFACE_SPEED_REALTIME: u8 = 4;
/// Unsubscribe from surface frame updates: [0x29][surface_id:2]
pub const C2S_SURFACE_UNSUBSCRIBE: u8 = 0x29;
/// Acknowledge receipt of a surface video frame: [0x2A][surface_id:2]
pub const C2S_SURFACE_ACK: u8 = 0x2A;
/// Request close of a Wayland surface (sends xdg_toplevel close event):
/// [0x2B][surface_id:2]
pub const C2S_SURFACE_CLOSE: u8 = 0x2B;
/// Request a list of MIME types available on the clipboard: [0x2C]
/// Server responds with S2C_CLIPBOARD_LIST.
pub const C2S_CLIPBOARD_LIST: u8 = 0x2C;
/// Client feature/capability advertisement: [0x2D][payload:N]
/// Currently defined payload bytes:
///   [0] codec_support — bitmask of CODEC_SUPPORT_* flags the client can
///       decode.  0 = accept anything (legacy).
/// Sent once after connection when capability probing completes.  The
/// message is extensible: the server ignores trailing bytes it doesn't
/// understand, and missing bytes default to 0.
pub const C2S_CLIENT_FEATURES: u8 = 0x2D;
/// Composed text input for a Wayland surface (UTF-8):
/// [0x2F][surface_id:2][text:N]
/// The server synthesises the corresponding evdev key sequences (US-QWERTY)
/// for ASCII characters.  Non-ASCII characters are delivered via
/// zwp_text_input_v3 commit_string when available.
pub const C2S_SURFACE_TEXT: u8 = 0x2F;
/// Composition still in progress for a Wayland surface (UTF-8):
/// [0x34][surface_id:2][cursor:2][text:N]
///
/// `cursor` is a byte offset into `text`.  An empty `text` withdraws the
/// composition, which is what a cancelled one leaves behind.
///
/// Sent while the user is still choosing characters, so the app can show the
/// pending text inline.  Without it a composition is invisible until it is
/// committed: the browser captures it in an off-screen textarea, so there is
/// nowhere for the user to read what they have typed so far.  Delivered via
/// `zwp_text_input_v3` preedit_string, and dropped when the focused client
/// has no input method enabled — a preedit has nowhere to go but the app's
/// own text field.
pub const C2S_SURFACE_PREEDIT: u8 = 0x34;

// -- Browser-initiated drag-and-drop (docs/protocol.md "Drag and drop") --
//
// `surface_id`, `x` and `y` are encoded exactly as in
// [`C2S_SURFACE_POINTER`]: LE u16s, coordinates in the composited frame's
// physical pixel space, converted to surface-local logical coordinates by
// the compositor the same way pointer motion is.

/// Begin (or retarget) a drag session over a Wayland surface:
/// [0x35][surface_id:2][x:2][y:2][mime_count:2][mime entries]
/// where a mime entry is [mime_len:2][mime:N].  The mime list is what the
/// browser can offer; it is advertised to the app unchanged, and the data
/// arrives later, inside [`C2S_SURFACE_DRAG_DROP`].
pub const C2S_SURFACE_DRAG_ENTER: u8 = 0x35;
/// Move the drag over a surface: [0x36][surface_id:2][x:2][y:2]
pub const C2S_SURFACE_DRAG_MOTION: u8 = 0x36;
/// The drag left the surface: [0x37][surface_id:2]
pub const C2S_SURFACE_DRAG_LEAVE: u8 = 0x37;
/// Complete the drop:
/// [0x38][surface_id:2][x:2][y:2][item_count:2][items]
/// where an item is [mime_len:2][mime:N][name_len:2][name:M][data_len:4][data:D].
/// Items with a non-empty `name` are files the client already uploaded into
/// the connection's drag staging dir through the fs family
/// (`FS_SYNC_STAGING` + chunked upload); `name` is the path relative to the
/// staging root and `data` is empty.  The server offers their `file://`
/// URIs as `text/uri-list`.  Name-less items are dragged content (text,
/// HTML, …) offered directly under their own mime; only they carry inline
/// `data`, so they are the only part of a drop the 16 MiB frame cap still
/// bounds.
pub const C2S_SURFACE_DRAG_DROP: u8 = 0x38;
/// Abort the drag (Escape, or the drag left the window): [0x39].  No payload.
pub const C2S_SURFACE_DRAG_CANCEL: u8 = 0x39;
/// Read clipboard content for a specific MIME type:
/// [0x2E][mime_len:2][mime:N]
/// Server responds with S2C_CLIPBOARD_CONTENT (0x25) containing the data.
pub const C2S_CLIPBOARD_GET: u8 = 0x2E;
/// Request server shutdown: [0x0F].  No payload.
/// The server broadcasts S2C_QUIT to all connected clients and exits.
pub const C2S_QUIT: u8 = 0x0F;

pub const S2C_UPDATE: u8 = 0x00;
pub const S2C_CREATED: u8 = 0x01;
pub const S2C_CLOSED: u8 = 0x02;
pub const S2C_LIST: u8 = 0x03;
pub const S2C_TITLE: u8 = 0x04;
pub const S2C_SEARCH_RESULTS: u8 = 0x05;
pub const S2C_CREATED_N: u8 = 0x06;
pub const S2C_HELLO: u8 = 0x07;
/// The PTY's subprocess has exited but the terminal state is retained.
/// Clients can still read/scroll the last frame. Send C2S_CLOSE to dismiss.
/// Wire: [0x08][pty_id:2][exit_status:4][reason:1]
/// exit_status: WEXITSTATUS if normal exit, negative signal number if signalled,
///              EXIT_STATUS_UNKNOWN if not yet collected.
/// reason: why the terminal ended (see EXIT_REASON_*).  Appended after the
///         fact and length-gated, like the trailing fields on S2C_HELLO, so a
///         7-byte message from an older server parses as EXIT_REASON_NORMAL.
pub const S2C_EXITED: u8 = 0x08;
pub const EXIT_STATUS_UNKNOWN: i32 = i32::MIN;

/// Why a terminal ended.  Without this a deadline kill arrives as `-9`,
/// indistinguishable from a user's `blit terminal kill -9`.
///
/// The numbering keeps slots for the lifecycle causes designed alongside
/// this one in docs/design/units.md, so those can land without renumbering
/// a shipped wire value.
pub const EXIT_REASON_NORMAL: u8 = 0;
pub const EXIT_REASON_DEADLINE: u8 = 1;
/// Reserved: lease expiry (docs/design/units.md).
pub const EXIT_REASON_LEASE: u8 = 2;
/// The server evicted an exited terminal to stay under its retention bound.
pub const EXIT_REASON_GC: u8 = 3;
/// Reserved: unit stop (docs/design/units.md).
pub const EXIT_REASON_UNIT_STOP: u8 = 4;

pub fn exit_reason_text(reason: u8) -> &'static str {
    match reason {
        EXIT_REASON_NORMAL => "exited",
        EXIT_REASON_DEADLINE => "killed by deadline",
        EXIT_REASON_LEASE => "killed by lease expiry",
        EXIT_REASON_GC => "evicted",
        EXIT_REASON_UNIT_STOP => "stopped by unit",
        _ => "unknown reason",
    }
}
/// Sent after the initial burst (HELLO, LIST, TITLE*, EXITED*) is complete.
/// Clients can use this to know when the initial state has been fully transmitted.
pub const S2C_READY: u8 = 0x09;
/// Application-level keepalive: [0x0B].  No payload.
/// Sent periodically by the server so clients can detect dead connections
/// even when no other traffic is flowing (e.g. idle terminal, WebRTC).
pub const S2C_PING: u8 = 0x0B;
/// Server is shutting down: [0x0C].  No payload.
/// Broadcast to all connected clients before the server exits.
pub const S2C_QUIT: u8 = 0x0C;
/// Terminal used visible rows changed: [0x0D][pty_id:2][used_rows:2]
///
/// `used_rows` is the highest visible row reached since the last terminal
/// reset/clear-like sequence, capped to the current PTY height.
pub const S2C_USED_ROWS: u8 = 0x0D;
/// Reply to `C2S_TERM_CWD`: [0x0E][nonce:2][cwd_len:2][cwd:N]. Empty = unknown.
/// The server answers with the shell's own OSC 7 report when it has seen
/// one, and falls back to asking the kernel about the PTY child otherwise
/// (docs/protocol.md, "Working directory tracking").
pub const S2C_TERM_CWD: u8 = 0x0E;
/// Unsolicited working-directory push: [0x0F][pty_id:2][cwd:N].
/// `cwd` is the remainder of the message (no length prefix, like S2C_TITLE):
/// an absolute UTF-8 path of at most [`TERM_CWD_MAX`] bytes.
/// Broadcast to every connected client when the cwd reported by shell
/// integration (OSC 7) changes; identical per-prompt re-reports are not
/// re-sent.  Shells without OSC 7 integration never trigger this — clients
/// keep the `C2S_TERM_CWD` poll as the fallback
/// (docs/protocol.md, "Working directory tracking").
pub const S2C_TERM_CWD_EVENT: u8 = 0x0F;
/// Upper bound on the `cwd` path in `S2C_TERM_CWD_EVENT` (and on the
/// server-side OSC 7 store feeding `S2C_TERM_CWD`).  4096 is Linux's
/// PATH_MAX — no kernel-accepted cwd is longer (macOS caps at 1024) —
/// and it sits well under the u16 length the family's `cwd_len:2`
/// framing already imposes.  Oversize OSC 7 reports are dropped.
pub const TERM_CWD_MAX: usize = 4096;
/// Correlated creation refusal: [0x10][nonce:2][status:1][detail:N].
/// `status` comes from the common registry below; `detail` is diagnostic
/// UTF-8 capped at [`CREATE_FAILED_DETAIL_MAX`].  Sent only in answer to a
/// `C2S_CREATE2` that set [`CREATE2_WANT_STATUS`] — never for a plain
/// `CREATE`, `CREATE_AT`, `CREATE_N`, or unflagged `CREATE2`, which keep
/// their success-only contract.
pub const S2C_CREATE_FAILED: u8 = 0x10;
/// Cap on `S2C_CREATE_FAILED`'s `detail`.  Matches the 1 KiB the other
/// diagnostic-detail families use; the text is for humans, not parsing.
pub const CREATE_FAILED_DETAIL_MAX: usize = 1024;
/// A scrolled-back client's view was re-anchored: [0x11][pty_id:2][offset:4]
///
/// `C2S_SCROLL` names a position as a distance from the live bottom, so
/// output from the app moves the text under a client that is reading its
/// scrollback.  The server holds that client still by growing its offset
/// as lines scroll away, and reports the result here so the client's own
/// idea of where it is — scrollbar, selection anchors, the next
/// `C2S_SCROLL` it sends — keeps agreeing with the frames it receives.
/// Sent only to a client with a non-zero offset, only when that offset
/// actually moved.
pub const S2C_SCROLL_OFFSET: u8 = 0x11;
/// Text response: [0x0A][nonce:2][pty_id:2][total_lines:4][offset:4][text:N]
/// nonce: echoed from C2S_READ request
/// total_lines: total available lines (scrollback + viewport rows)
/// offset: the offset that was requested
/// text: UTF-8 text, lines separated by \n
pub const S2C_TEXT: u8 = 0x0A;

pub fn msg_s2c_used_rows(pty_id: u16, used_rows: u16) -> Vec<u8> {
    let mut msg = Vec::with_capacity(5);
    msg.push(S2C_USED_ROWS);
    msg.extend_from_slice(&pty_id.to_le_bytes());
    msg.extend_from_slice(&used_rows.to_le_bytes());
    msg
}

pub fn msg_s2c_scroll_offset(pty_id: u16, offset: u32) -> Vec<u8> {
    let mut msg = Vec::with_capacity(7);
    msg.push(S2C_SCROLL_OFFSET);
    msg.extend_from_slice(&pty_id.to_le_bytes());
    msg.extend_from_slice(&offset.to_le_bytes());
    msg
}

/// A new Wayland toplevel surface was created:
/// [0x20][surface_id:2][parent_id:2][width:2][height:2][title_len:2][title:N][app_id_len:2][app_id:N]
/// parent_id: 0 = no parent (top-level), non-zero = dialog/child of that surface
pub const S2C_SURFACE_CREATED: u8 = 0x20;
/// A Wayland surface was destroyed: [0x21][surface_id:2]
pub const S2C_SURFACE_DESTROYED: u8 = 0x21;
/// An encoded video frame for a Wayland surface:
/// [0x22][surface_id:2][timestamp:4][flags:1][width:2][height:2][data:N]
/// With `SURFACE_FRAME_FLAG_TIMESTAMP_SUB_US`:
/// [0x22][surface_id:2][timestamp:4][flags:1][width:2][height:2][sub_us:2][data:N]
/// flags: bit 0 = keyframe, bits 1-2 = codec (0 = H.264, 1 = AV1),
/// bit 3 = sub-millisecond timestamp field present.
/// timestamp: milliseconds since compositor session start.
pub const S2C_SURFACE_FRAME: u8 = 0x22;
/// A Wayland surface's title changed: [0x23][surface_id:2][title:N]
pub const S2C_SURFACE_TITLE: u8 = 0x23;
/// A Wayland surface was resized by the app:
/// [0x24][surface_id:2][width:2][height:2]                            — legacy
/// [0x24][surface_id:2][width:2][height:2][logical_w:2][logical_h:2]  — current
///
/// `width`/`height` are the composited frame's *physical* pixels.  The
/// logical pair is the same size in surface-logical pixels — the window as
/// its Wayland client measures it, before the mediated output scale.  The
/// two differ whenever any subscriber is high-DPI, because mediation gives
/// the surface the *highest* scale any viewer asked for (see
/// `mediated_size_for_surface`), so a 1x viewer that assumes physical ==
/// logical draws a 3x window three times too large.
///
/// A viewer should present the surface at `logical * its requested scale`
/// device pixels, capped to its pane. In the default relative mode that scale
/// includes its DPR. Older servers omit the pair; treat it as absent
/// (not as 0x0) and fall back to filling the pane.
pub const S2C_SURFACE_RESIZED: u8 = 0x24;
/// A Wayland surface's app_id changed: [0x28][surface_id:2][app_id:N]
pub const S2C_SURFACE_APP_ID: u8 = 0x28;
/// A Wayland client asked for its toplevel to be activated
/// (xdg_activation_v1) — raise and focus the matching pane:
/// [0x2D][surface_id:2]
pub const S2C_SURFACE_ACTIVATED: u8 = 0x2D;
/// Clipboard content (response to C2S_CLIPBOARD_GET or unsolicited broadcast on change):
/// [0x25][mime_len:2][mime:N][data_len:4][data:N]
pub const S2C_CLIPBOARD_CONTENT: u8 = 0x25;
/// List of all compositor surfaces:
/// [0x26][count:2] repeated{ [surface_id:2][parent_id:2][width:2][height:2][title_len:2][title:N][app_id_len:2][app_id:N] }
pub const S2C_SURFACE_LIST: u8 = 0x26;
/// Screenshot of a surface: [0x27][surface_id:2][width:4][height:4][image_data:N]
/// image_data is PNG or AVIF depending on the request format.
/// If the surface was not found or has no buffer, width=0 and height=0 with empty data.
pub const S2C_SURFACE_CAPTURE: u8 = 0x27;

/// Cursor shape changed for a surface: [0x29][surface_id:2][shape_len:1][shape:N]
/// shape is a CSS cursor keyword (e.g. "default", "pointer", "text").
pub const S2C_SURFACE_CURSOR: u8 = 0x29;

/// Encoder backend for a surface: [0x2A][surface_id:2][name:N]
/// name is a short ASCII string like "h264-nvenc", "h264-vaapi", "h264-software", etc.
/// Sent when a new encoder is created for a surface (initial subscribe or resize).
pub const S2C_SURFACE_ENCODER: u8 = 0x2A;

/// List of MIME types available on the clipboard:
/// [0x2C][count:2] repeated{ [mime_len:2][mime:N] }
pub const S2C_CLIPBOARD_LIST: u8 = 0x2C;
/// Which side currently owns the compositor clipboard selection:
/// [0x2E][wayland:1].  `1` means a Wayland client owns it and browser paste
/// must not replace it; `0` means the selection is empty or externally owned.
pub const S2C_CLIPBOARD_OWNER: u8 = 0x2E;

// -- Audio forwarding ---------------------------------------------------

/// Subscribe to audio: [0x30][bitrate_kbps:2]
/// Audio is per-compositor (one mixed stream), not per-surface.
/// The server begins sending S2C_AUDIO_FRAME.
/// bitrate_kbps: 0 = server default.
pub const C2S_AUDIO_SUBSCRIBE: u8 = 0x30;
/// Unsubscribe from audio: [0x31]
pub const C2S_AUDIO_UNSUBSCRIBE: u8 = 0x31;

// -- Pointer axis v2 ----------------------------------------------------

/// Pointer axis/scroll for a Wayland surface, both axes in one event:
/// [0x32][surface_id:2][flags:1][dx_x100:4_signed][dy_x100:4_signed][v120_x:2_signed][v120_y:2_signed]
///
/// `dx`/`dy` are the smooth scroll distance × 100, positive = right/down,
/// in the composited frame's pixel space — the same coordinate space
/// `C2S_SURFACE_POINTER` uses, which the compositor converts to
/// surface-logical pixels on the way in. `wl_pointer.axis` requires
/// exactly that: "a coordinate space identical to those of motion events".
/// Keeping both in frame space means the client never has to guess the
/// scale the compositor settled on.
///
/// `v120_x`/`v120_y` are discrete wheel travel in 120ths of a detent, the
/// `wl_pointer.axis_value120` convention: 120 = one notch. Zero for
/// devices without detents.
///
/// flags bits 0-1: source, matching `wl_pointer.axis_source` — 0 = wheel,
/// 1 = finger, 2 = continuous, 3 = wheel tilt.
/// flags bit 2: source is known. When clear, the other source bits are
/// ignored and no `axis_source` is emitted (what [`C2S_SURFACE_POINTER_AXIS`]
/// does).
/// flags bit 3: stop — the scroll sequence ended. Sent with zero deltas;
/// becomes `wl_pointer.axis_stop`.
pub const C2S_SURFACE_POINTER_AXIS2: u8 = 0x32;

/// `wl_pointer.axis_source` values, as carried in the low bits of the
/// [`C2S_SURFACE_POINTER_AXIS2`] flags byte.
pub const AXIS_SOURCE_WHEEL: u8 = 0;
pub const AXIS_SOURCE_FINGER: u8 = 1;
pub const AXIS_SOURCE_CONTINUOUS: u8 = 2;
pub const AXIS_SOURCE_WHEEL_TILT: u8 = 3;

/// Set when the source bits are meaningful.
pub const AXIS_FLAG_SOURCE_KNOWN: u8 = 1 << 2;
/// Set when this event ends a scroll sequence.
pub const AXIS_FLAG_STOP: u8 = 1 << 3;
/// An encoded audio frame (Opus) from the compositor's mixed output:
/// [0x30][timestamp:4][flags:1][data:N]
/// timestamp: sample offset in 48 kHz ticks from an arbitrary epoch.
/// flags: bits 1-2 = codec (0 = Opus). Other bits reserved.
pub const S2C_AUDIO_FRAME: u8 = 0x30;

pub const AUDIO_FRAME_CODEC_MASK: u8 = 0b110;
pub const AUDIO_FRAME_CODEC_OPUS: u8 = 0 << 1;

/// A fragment of a larger S2C message: [0x2B][flags:1][chunk:N]
///
/// Large bulk messages (video keyframes, terminal snapshots) are split
/// into multiple fragments so audio frames can be written between them
/// on the same stream.  Without this the writer task would hold the
/// socket for the full duration of a multi-hundred-KB write, starving
/// audio delivery and producing audible gaps on the client.
///
/// Flags:
///   bit 0 (FRAGMENT_FLAG_LAST) — this is the last fragment; the receiver
///     should concatenate all fragments of this message (in order) and
///     dispatch the reassembled buffer as if it were a single message.
///
/// Fragments of different messages do NOT interleave: TCP preserves
/// order and the server only splits one message at a time, so the
/// receiver can use a single pending-reassembly buffer with no fragment
/// id or sequence number.  S2C_AUDIO_FRAME messages may appear between
/// fragments and are handled normally — they don't contribute to the
/// reassembly buffer.
pub const S2C_FRAGMENT: u8 = 0x2B;
pub const FRAGMENT_FLAG_LAST: u8 = 1 << 0;

pub const SURFACE_FRAME_FLAG_KEYFRAME: u8 = 1 << 0;
pub const SURFACE_FRAME_CODEC_MASK: u8 = 0b110;
pub const SURFACE_FRAME_CODEC_H264: u8 = 0 << 1;
pub const SURFACE_FRAME_CODEC_AV1: u8 = 1 << 1;
pub const SURFACE_FRAME_CODEC_PNG: u8 = 2 << 1;
pub const SURFACE_FRAME_FLAG_TIMESTAMP_SUB_US: u8 = 1 << 3;

/// Optional byte 6 of `C2S_CLIENT_FEATURES`.
pub const CLIENT_FEATURE_SURFACE_TIMESTAMP_SUB_US: u8 = 1 << 0;

/// Bitmask for client-supported codecs in C2S_CLIENT_FEATURES and
/// C2S_SURFACE_SUBSCRIBE.  0 means "accept anything".
pub const CODEC_SUPPORT_H264: u8 = 1 << 0;
pub const CODEC_SUPPORT_AV1: u8 = 1 << 1;
pub const CODEC_SUPPORT_H264_444: u8 = 1 << 2;
pub const CODEC_SUPPORT_AV1_444: u8 = 1 << 3;

// ---------------------------------------------------------------------------
// Common status registry (docs/protocol.md, "Common status registry")
//
// A one-byte `status` shared by families that do not declare a message-local
// table.  The `KV_STATUS_*` / `NET_STATUS_*` / `FS_*` tables are grandfathered
// and keep their shipped values; new families use these.  Values 0-127 are
// centrally allocated (13-127 reserved), 128-255 are family-local and must be
// defined by the packet carrying them.
// ---------------------------------------------------------------------------

pub const STATUS_OK: u8 = 0;
pub const STATUS_UNKNOWN_ID: u8 = 1;
pub const STATUS_NOT_FOUND: u8 = 2;
pub const STATUS_WRONG_TYPE: u8 = 3;
pub const STATUS_PERMISSION: u8 = 4;
pub const STATUS_TOO_LARGE: u8 = 5;
pub const STATUS_BUDGET: u8 = 6;
pub const STATUS_INVALID: u8 = 7;
pub const STATUS_CANCELLED: u8 = 8;
pub const STATUS_OTHER: u8 = 9;
pub const STATUS_WARMING: u8 = 10;
pub const STATUS_CONFLICT: u8 = 11;
pub const STATUS_NO_MERGE_BASE: u8 = 12;

/// Render a common-registry status for humans.  An unallocated value reads
/// distinctly from [`STATUS_OTHER`] so a newer server's status is not
/// mistaken for a generic backend failure.
pub fn status_text(status: u8) -> &'static str {
    match status {
        STATUS_OK => "ok",
        STATUS_UNKNOWN_ID => "unknown id",
        STATUS_NOT_FOUND => "not found",
        STATUS_WRONG_TYPE => "wrong type",
        STATUS_PERMISSION => "permission denied",
        STATUS_TOO_LARGE => "too large",
        STATUS_BUDGET => "budget exhausted",
        STATUS_INVALID => "invalid request",
        STATUS_CANCELLED => "cancelled",
        STATUS_OTHER => "backend error",
        STATUS_WARMING => "warming up",
        STATUS_CONFLICT => "conflict",
        STATUS_NO_MERGE_BASE => "no merge base",
        _ => "unknown status",
    }
}

pub const FEATURE_CREATE_NONCE: u32 = 1 << 0;
pub const FEATURE_RESTART: u32 = 1 << 1;
pub const FEATURE_RESIZE_BATCH: u32 = 1 << 2;
pub const FEATURE_COPY_RANGE: u32 = 1 << 3;
pub const FEATURE_COMPOSITOR: u32 = 1 << 4;
pub const FEATURE_AUDIO: u32 = 1 << 5;
/// Bits 6-13 are allocated to the per-family modules (`fs`, `git`, `lsp`,
/// `kv`, `net`) and to the proposed extension/channel/process families.
///
/// The server answers a `C2S_CREATE2` carrying [`CREATE2_WANT_STATUS`] with
/// exactly one of `S2C_CREATED_N` or [`S2C_CREATE_FAILED`].  Not gated by any
/// family kill switch.
pub const FEATURE_CREATE_STATUS: u32 = 1 << 14;
/// `C2S_KILL` and `C2S_CLOSE` reach the child's process group (Unix) or job
/// object (Windows) rather than the session leader alone, and `C2S_KILL`
/// accepts a trailing [`KILL_LEADER_ONLY`] flag byte to opt back out.
pub const FEATURE_KILL_MODE: u32 = 1 << 15;
/// Server-enforced terminal deadlines: `C2S_DEADLINE`,
/// `CREATE2(HAS_DEADLINE)`, and the `reason` byte on `S2C_EXITED`.
pub const FEATURE_PTY_DEADLINE: u32 = 1 << 16;
/// Scrollback that holds still under output: the server re-anchors a
/// scrolled client and reports it with [`S2C_SCROLL_OFFSET`], and accepts
/// the relative [`C2S_SCROLL_BY`] that goes with it.
pub const FEATURE_SCROLL_BY: u32 = 1 << 17;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Color {
    #[default]
    Default,
    Indexed(u8),
    Rgb(u8, u8, u8),
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CellStyle {
    pub fg: Color,
    pub bg: Color,
    pub bold: bool,
    pub dim: bool,
    pub italic: bool,
    pub underline: bool,
    pub inverse: bool,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Rect {
    pub row: u16,
    pub col: u16,
    pub rows: u16,
    pub cols: u16,
}

impl Rect {
    pub const fn new(row: u16, col: u16, rows: u16, cols: u16) -> Self {
        Self {
            row,
            col,
            rows,
            cols,
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct FrameState {
    rows: u16,
    cols: u16,
    cells: Vec<u8>,
    cursor_row: u16,
    cursor_col: u16,
    mode: u16,
    title: String,
    /// Overflow strings for cells whose content exceeds 4 bytes.
    /// Keyed by flat cell index (row * cols + col).
    overflow: BTreeMap<usize, String>,
    /// Per-row flags. `ROW_FLAG_WRAPPED` means the row continues on the next.
    line_flags: Vec<u8>,
    /// Total scrollback lines available for this PTY.
    scrollback_lines: u32,
    /// OSC 8 hyperlink id per cell, parallel to `cells` (one entry per cell).
    /// 0 means "no link". Empty when the frame has no hyperlinks at all, so
    /// the overwhelmingly common link-free case costs no allocation.
    cell_links: Vec<u16>,
    /// Hyperlink id -> URI. Ids are frame-local and assigned by the producer.
    link_uris: BTreeMap<u16, String>,
}

impl FrameState {
    pub fn new(rows: u16, cols: u16) -> Self {
        let total = rows as usize * cols as usize;
        Self {
            rows,
            cols,
            cells: vec![0; total * CELL_SIZE],
            cursor_row: 0,
            cursor_col: 0,
            mode: 0,
            title: String::new(),
            overflow: BTreeMap::new(),
            line_flags: vec![0; rows as usize],
            scrollback_lines: 0,
            cell_links: Vec::new(),
            link_uris: BTreeMap::new(),
        }
    }

    pub fn from_parts(
        rows: u16,
        cols: u16,
        cursor_row: u16,
        cursor_col: u16,
        mode: u16,
        title: impl Into<String>,
        cells: Vec<u8>,
    ) -> Self {
        let mut state = Self::new(rows, cols);
        if cells.len() == state.cells.len() {
            state.cells = cells;
        }
        state.cursor_row = cursor_row;
        state.cursor_col = cursor_col;
        state.mode = mode;
        state.title = title.into();
        state
    }

    pub fn rows(&self) -> u16 {
        self.rows
    }

    pub fn cols(&self) -> u16 {
        self.cols
    }

    pub fn cursor_row(&self) -> u16 {
        self.cursor_row
    }

    pub fn cursor_col(&self) -> u16 {
        self.cursor_col
    }

    pub fn mode(&self) -> u16 {
        self.mode
    }

    pub fn title(&self) -> &str {
        &self.title
    }

    pub fn cells(&self) -> &[u8] {
        &self.cells
    }

    pub fn cells_mut(&mut self) -> &mut [u8] {
        &mut self.cells
    }

    pub fn overflow(&self) -> &BTreeMap<usize, String> {
        &self.overflow
    }

    pub fn overflow_mut(&mut self) -> &mut BTreeMap<usize, String> {
        &mut self.overflow
    }

    pub fn cell_links(&self) -> &[u16] {
        &self.cell_links
    }

    pub fn link_uris(&self) -> &BTreeMap<u16, String> {
        &self.link_uris
    }

    pub fn has_links(&self) -> bool {
        !self.link_uris.is_empty()
    }

    /// The OSC 8 URI attached to a cell, if any.
    ///
    /// A wide character's continuation cell resolves to the same link as the
    /// character itself, so clicking either half of a CJK glyph follows the
    /// link rather than only its left column.
    pub fn cell_link(&self, row: u16, col: u16) -> Option<&str> {
        if row >= self.rows || col >= self.cols || self.cell_links.is_empty() {
            return None;
        }
        let mut flat = row as usize * self.cols as usize + col as usize;
        if self.cells[flat * CELL_SIZE + 1] & 4 != 0 && col > 0 {
            flat -= 1; // wide continuation: inherit the lead cell's link
        }
        let id = *self.cell_links.get(flat)?;
        if id == 0 {
            return None;
        }
        self.link_uris.get(&id).map(String::as_str)
    }

    /// Raw link id at a cell, resolving wide-character continuations the same
    /// way `cell_link` does. Used to compare identity without materialising the
    /// URI string for every cell walked.
    fn link_id_at(&self, row: u16, col: u16) -> u16 {
        if row >= self.rows || col >= self.cols || self.cell_links.is_empty() {
            return 0;
        }
        let mut flat = row as usize * self.cols as usize + col as usize;
        if self.cells[flat * CELL_SIZE + 1] & 4 != 0 && col > 0 {
            flat -= 1;
        }
        self.cell_links.get(flat).copied().unwrap_or(0)
    }

    /// The full extent of the hyperlink covering `(row, col)`, as one
    /// `(row, start_col, end_col)` segment per screen row it occupies.
    ///
    /// A hyperlink is a property of the *logical* line, so one that runs past
    /// the right edge continues on the next screen row and must be highlighted
    /// as a single link rather than two. Rows are joined only across a wrap
    /// (`ROW_FLAG_WRAPPED`) and only when the span actually touches both the
    /// last column of one row and the first of the next — two unrelated links
    /// that happen to share a target stay separate.
    pub fn link_segments(&self, row: u16, col: u16) -> Vec<(u16, u16, u16)> {
        let id = self.link_id_at(row, col);
        if id == 0 || self.cols == 0 {
            return Vec::new();
        }
        let last_col = self.cols - 1;

        // Walk back to the first row/column of the span.
        let (mut start_row, mut start_col) = (row, col);
        loop {
            while start_col > 0 && self.link_id_at(start_row, start_col - 1) == id {
                start_col -= 1;
            }
            if start_col != 0 || start_row == 0 {
                break;
            }
            let prev = start_row - 1;
            if !self.is_wrapped(prev) || self.link_id_at(prev, last_col) != id {
                break;
            }
            start_row = prev;
            start_col = last_col;
        }

        // Walk forward to the last row/column, emitting a segment per row.
        let mut segments = Vec::new();
        let (mut cur_row, mut seg_start) = (start_row, start_col);
        loop {
            let mut end_col = seg_start;
            while end_col < last_col && self.link_id_at(cur_row, end_col + 1) == id {
                end_col += 1;
            }
            segments.push((cur_row, seg_start, end_col));
            if end_col != last_col
                || cur_row + 1 >= self.rows
                || !self.is_wrapped(cur_row)
                || self.link_id_at(cur_row + 1, 0) != id
            {
                break;
            }
            cur_row += 1;
            seg_start = 0;
        }
        segments
    }

    /// Replace the frame's hyperlink state wholesale. `cell_links` is accepted
    /// only at exactly one entry per cell; anything else is treated as "no
    /// links" rather than silently mapping links onto the wrong cells.
    ///
    /// A URI longer than [`MAX_LINK_URI`] is dropped, along with the cells
    /// referencing it. This is the single chokepoint every producer passes
    /// through — the PTY collector, the wire decoder, and any caller building a
    /// `FrameState` by hand — so enforcing the cap here is what makes it an
    /// invariant of the type rather than a convention each producer repeats.
    pub fn set_links(&mut self, cell_links: Vec<u16>, link_uris: BTreeMap<u16, String>) {
        let total = self.rows as usize * self.cols as usize;
        if link_uris.is_empty() || cell_links.len() != total {
            self.clear_links();
            return;
        }
        let (mut cell_links, mut link_uris) = (cell_links, link_uris);
        // Checked over the URI table (a handful of entries) rather than the
        // cell grid, so the overwhelmingly common clean case costs nothing.
        if link_uris.values().any(|uri| uri.len() > MAX_LINK_URI) {
            link_uris.retain(|_, uri| uri.len() <= MAX_LINK_URI);
            if link_uris.is_empty() {
                self.clear_links();
                return;
            }
            for slot in cell_links.iter_mut() {
                if *slot != 0 && !link_uris.contains_key(slot) {
                    *slot = 0;
                }
            }
        }
        self.cell_links = cell_links;
        self.link_uris = link_uris;
    }

    pub fn clear_links(&mut self) {
        self.cell_links.clear();
        self.link_uris.clear();
    }

    pub fn line_flags(&self) -> &[u8] {
        &self.line_flags
    }

    pub fn line_flags_mut(&mut self) -> &mut Vec<u8> {
        &mut self.line_flags
    }

    pub fn scrollback_lines(&self) -> u32 {
        self.scrollback_lines
    }

    pub fn set_scrollback_lines(&mut self, lines: u32) {
        self.scrollback_lines = lines;
    }

    pub fn is_wrapped(&self, row: u16) -> bool {
        self.line_flags.get(row as usize).copied().unwrap_or(0) & ROW_FLAG_WRAPPED != 0
    }

    pub fn set_wrapped(&mut self, row: u16, wrapped: bool) {
        if let Some(flags) = self.line_flags.get_mut(row as usize) {
            if wrapped {
                *flags |= ROW_FLAG_WRAPPED;
            } else {
                *flags &= !ROW_FLAG_WRAPPED;
            }
        }
    }

    /// Returns the text content of a cell, resolving overflow if needed.
    pub fn cell_content(&self, row: u16, col: u16) -> &str {
        if row >= self.rows || col >= self.cols {
            return "";
        }
        let flat = row as usize * self.cols as usize + col as usize;
        let idx = flat * CELL_SIZE;
        let f1 = self.cells[idx + 1];
        if f1 & 4 != 0 {
            return ""; // wide continuation
        }
        let content_len = ((f1 >> 3) & 7) as usize;
        if content_len == CONTENT_OVERFLOW as usize {
            if let Some(s) = self.overflow.get(&flat) {
                return s.as_str();
            }
            return "";
        }
        if content_len == 0 {
            return " ";
        }
        std::str::from_utf8(&self.cells[idx + 8..idx + 8 + content_len]).unwrap_or(" ")
    }

    pub fn resize(&mut self, rows: u16, cols: u16) {
        if rows == self.rows && cols == self.cols {
            return;
        }
        self.rows = rows;
        self.cols = cols;
        self.cells = vec![0; rows as usize * cols as usize * CELL_SIZE];
        self.overflow.clear();
        // Link ids are indexed by flat cell position, so they are meaningless
        // once the grid is reshaped. The next frame re-sends them.
        self.clear_links();
        self.line_flags = vec![0; rows as usize];
        self.cursor_row = self.cursor_row.min(rows.saturating_sub(1));
        self.cursor_col = self.cursor_col.min(cols.saturating_sub(1));
    }

    pub fn set_cursor(&mut self, row: u16, col: u16) {
        self.cursor_row = row.min(self.rows.saturating_sub(1));
        self.cursor_col = col.min(self.cols.saturating_sub(1));
    }

    pub fn set_mode(&mut self, mode: u16) {
        self.mode = mode;
    }

    pub fn set_title(&mut self, title: impl Into<String>) -> bool {
        let title = title.into();
        if self.title == title {
            return false;
        }
        self.title = title;
        true
    }

    pub fn clear(&mut self, style: CellStyle) {
        for row in 0..self.rows {
            for col in 0..self.cols {
                self.set_blank_cell(row, col, style);
            }
        }
    }

    pub fn fill_rect(&mut self, rect: Rect, ch: char, style: CellStyle) {
        let row_end = rect.row.saturating_add(rect.rows).min(self.rows);
        let col_end = rect.col.saturating_add(rect.cols).min(self.cols);
        for row in rect.row..row_end {
            let mut col = rect.col;
            while col < col_end {
                let width = self.set_cell(row, col, ch, style);
                if width == 0 {
                    break;
                }
                col = col.saturating_add(width);
            }
        }
    }

    pub fn write_text(&mut self, row: u16, col: u16, text: &str, style: CellStyle) -> u16 {
        if row >= self.rows || col >= self.cols {
            return col;
        }
        let mut cur_col = col;
        for ch in text.chars() {
            if cur_col >= self.cols {
                break;
            }
            let width = self.set_cell(row, cur_col, ch, style);
            if width == 0 {
                continue;
            }
            cur_col = cur_col.saturating_add(width);
        }
        cur_col
    }

    pub fn write_wrapped_text(&mut self, rect: Rect, text: &str, style: CellStyle) -> usize {
        if rect.rows == 0 || rect.cols == 0 {
            return 0;
        }
        let lines = wrap_text_lines(text, rect.cols as usize);
        let max_rows = rect.rows.min(self.rows.saturating_sub(rect.row));
        for (idx, line) in lines.iter().take(max_rows as usize).enumerate() {
            let row = rect.row + idx as u16;
            self.write_text(row, rect.col, line, style);
        }
        lines.len()
    }

    pub fn write_scrolling_text<S: AsRef<str>>(
        &mut self,
        rect: Rect,
        lines: &[S],
        offset_from_bottom: usize,
        style: CellStyle,
    ) {
        if rect.rows == 0 || rect.cols == 0 {
            return;
        }
        let mut wrapped = Vec::with_capacity(lines.len());
        for line in lines {
            let line = line.as_ref();
            let out = wrap_text_lines(line, rect.cols as usize);
            if out.is_empty() {
                wrapped.push(String::new());
            } else {
                wrapped.extend(out);
            }
        }
        let visible = rect.rows as usize;
        let end = wrapped.len().saturating_sub(offset_from_bottom);
        let start = end.saturating_sub(visible);
        for row in 0..rect.rows {
            self.fill_rect(
                Rect::new(rect.row + row, rect.col, 1, rect.cols),
                ' ',
                style,
            );
        }
        for (idx, line) in wrapped[start..end].iter().enumerate() {
            self.write_text(rect.row + idx as u16, rect.col, line, style);
        }
    }

    pub fn get_text(&self, start_row: u16, start_col: u16, end_row: u16, end_col: u16) -> String {
        let mut result = String::new();
        if self.rows == 0 || self.cols == 0 {
            return result;
        }
        for row in start_row..=end_row.min(self.rows.saturating_sub(1)) {
            let c0 = if row == start_row { start_col } else { 0 };
            let c1 = if row == end_row {
                end_col
            } else {
                self.cols - 1
            };
            let mut line = String::new();
            let mut col = c0;
            while col <= c1.min(self.cols - 1) {
                line.push_str(self.cell_content(row, col));
                col += 1;
            }
            let wrapped = self.is_wrapped(row);
            // Keep a soft-wrapped row's trailing space: it's the gap between words ("for all", not "forall").
            if wrapped {
                result.push_str(&line);
            } else {
                result.push_str(line.trim_end());
            }
            if row < end_row.min(self.rows.saturating_sub(1)) && !wrapped {
                result.push('\n');
            }
        }
        result
    }

    pub fn get_all_text(&self) -> String {
        if self.rows == 0 || self.cols == 0 {
            return String::new();
        }
        self.get_text(0, 0, self.rows - 1, self.cols - 1)
    }

    fn cell_style(&self, row: u16, col: u16) -> CellStyle {
        if row >= self.rows || col >= self.cols {
            return CellStyle::default();
        }
        let idx = self.cell_offset(row, col);
        let f0 = self.cells[idx];
        let f1 = self.cells[idx + 1];
        let fg_type = f0 & 3;
        let bg_type = (f0 >> 2) & 3;
        let fg = match fg_type {
            1 => Color::Indexed(self.cells[idx + 2]),
            2 => Color::Rgb(
                self.cells[idx + 2],
                self.cells[idx + 3],
                self.cells[idx + 4],
            ),
            _ => Color::Default,
        };
        let bg = match bg_type {
            1 => Color::Indexed(self.cells[idx + 5]),
            2 => Color::Rgb(
                self.cells[idx + 5],
                self.cells[idx + 6],
                self.cells[idx + 7],
            ),
            _ => Color::Default,
        };
        CellStyle {
            fg,
            bg,
            bold: (f0 >> 4) & 1 != 0,
            dim: (f0 >> 5) & 1 != 0,
            italic: (f0 >> 6) & 1 != 0,
            underline: (f0 >> 7) & 1 != 0,
            inverse: f1 & 1 != 0,
        }
    }

    pub fn get_ansi_text(&self) -> String {
        if self.rows == 0 || self.cols == 0 {
            return String::new();
        }
        let mut result = String::new();
        let mut cur_style = CellStyle::default();
        let mut cur_link: Option<&str> = None;
        for row in 0..self.rows {
            let mut line = String::new();
            let mut col = 0u16;
            while col < self.cols {
                let style = self.cell_style(row, col);
                if style != cur_style {
                    push_sgr(&mut line, &style);
                    cur_style = style;
                }
                // Re-emit OSC 8 so a dump of the screen stays as clickable as
                // the screen itself. Only transitions are written, matching how
                // the sequence arrived.
                let link = self.cell_link(row, col);
                if link != cur_link {
                    push_osc8(&mut line, link);
                    cur_link = link;
                }
                line.push_str(self.cell_content(row, col));
                col += 1;
            }
            // Close the span before trimming: trailing blanks inside a link
            // would otherwise carry the closing sequence away with them.
            if cur_link.is_some() {
                push_osc8(&mut line, None);
                cur_link = None;
            }
            let trimmed = line.trim_end();
            result.push_str(trimmed);
            if cur_style != CellStyle::default() {
                result.push_str("\x1b[0m");
                cur_style = CellStyle::default();
            }
            if row < self.rows - 1 {
                result.push('\n');
            }
        }
        result
    }

    pub fn get_cell(&self, row: u16, col: u16) -> Vec<u8> {
        if row >= self.rows || col >= self.cols {
            return Vec::new();
        }
        let idx = self.cell_offset(row, col);
        self.cells[idx..idx + CELL_SIZE].to_vec()
    }

    fn cell_offset(&self, row: u16, col: u16) -> usize {
        (row as usize * self.cols as usize + col as usize) * CELL_SIZE
    }

    fn set_cell(&mut self, row: u16, col: u16, ch: char, style: CellStyle) -> u16 {
        if row >= self.rows || col >= self.cols {
            return 0;
        }
        let raw_width = UnicodeWidthChar::width(ch).unwrap_or(0);
        if raw_width == 0 {
            return 0;
        }
        let width = if raw_width > 1 && col + 1 < self.cols {
            2
        } else {
            1
        };
        let idx = self.cell_offset(row, col);
        encode_cell(
            &mut self.cells[idx..idx + CELL_SIZE],
            Some(ch),
            style,
            width == 2,
            false,
        );
        if width == 2 {
            let cont_idx = self.cell_offset(row, col + 1);
            encode_cell(
                &mut self.cells[cont_idx..cont_idx + CELL_SIZE],
                None,
                style,
                false,
                true,
            );
        }
        width
    }

    fn set_blank_cell(&mut self, row: u16, col: u16, style: CellStyle) {
        if row >= self.rows || col >= self.cols {
            return;
        }
        let idx = self.cell_offset(row, col);
        encode_cell(
            &mut self.cells[idx..idx + CELL_SIZE],
            None,
            style,
            false,
            false,
        );
    }
}

#[derive(Clone, Debug)]
pub struct TerminalState {
    frame: FrameState,
}

impl TerminalState {
    pub fn new(rows: u16, cols: u16) -> Self {
        let frame = FrameState::new(rows, cols);
        Self { frame }
    }

    pub fn frame(&self) -> &FrameState {
        &self.frame
    }

    pub fn frame_mut(&mut self) -> &mut FrameState {
        &mut self.frame
    }

    pub fn title(&self) -> &str {
        self.frame.title()
    }

    pub fn rows(&self) -> u16 {
        self.frame.rows()
    }

    pub fn cols(&self) -> u16 {
        self.frame.cols()
    }

    pub fn is_wrapped(&self, row: u16) -> bool {
        self.frame.is_wrapped(row)
    }

    pub fn cursor_row(&self) -> u16 {
        self.frame.cursor_row()
    }

    pub fn cursor_col(&self) -> u16 {
        self.frame.cursor_col()
    }

    pub fn mode(&self) -> u16 {
        self.frame.mode()
    }

    pub fn cells(&self) -> &[u8] {
        self.frame.cells()
    }

    pub fn set_title(&mut self, title: &str) -> bool {
        self.frame.set_title(title.to_owned())
    }

    pub fn get_text(&self, start_row: u16, start_col: u16, end_row: u16, end_col: u16) -> String {
        self.frame.get_text(start_row, start_col, end_row, end_col)
    }

    pub fn get_all_text(&self) -> String {
        self.frame.get_all_text()
    }

    pub fn get_ansi_text(&self) -> String {
        self.frame.get_ansi_text()
    }

    pub fn get_cell(&self, row: u16, col: u16) -> Vec<u8> {
        self.frame.get_cell(row, col)
    }

    /// Read the LZ4 prepended uncompressed size without allocating, and reject
    /// payloads that claim to decompress beyond [`MAX_DECOMPRESSED`].
    fn safe_decompress(data: &[u8]) -> Result<Vec<u8>, ()> {
        if data.len() < 4 {
            return Err(());
        }
        let claimed = u32::from_le_bytes([data[0], data[1], data[2], data[3]]) as usize;
        if claimed > MAX_DECOMPRESSED {
            return Err(());
        }
        decompress_size_prepended(data).map_err(|_| ())
    }

    pub fn feed_compressed(&mut self, data: &[u8]) -> bool {
        let payload = match Self::safe_decompress(data) {
            Ok(d) => d,
            Err(_) => return false,
        };
        self.apply_payload(&payload)
    }

    pub fn feed_compressed_batch(&mut self, batch: &[u8]) -> bool {
        let mut changed = false;
        let mut off = 0usize;
        while off + 4 <= batch.len() {
            let len =
                u32::from_le_bytes([batch[off], batch[off + 1], batch[off + 2], batch[off + 3]])
                    as usize;
            off += 4;
            if len == 0 {
                break;
            }
            if off + len > batch.len() {
                break;
            }
            if let Ok(payload) = Self::safe_decompress(&batch[off..off + len]) {
                changed |= self.apply_payload(&payload);
            }
            off += len;
        }
        changed
    }

    fn apply_payload(&mut self, payload: &[u8]) -> bool {
        if payload.len() < 12 {
            return false;
        }

        let new_rows = u16::from_le_bytes([payload[0], payload[1]]);
        let new_cols = u16::from_le_bytes([payload[2], payload[3]]);

        // Reject absurd dimensions that would cause multi-GiB allocations.
        if (new_rows as usize) * (new_cols as usize) > MAX_CELL_COUNT {
            return false;
        }
        let new_cursor_row = u16::from_le_bytes([payload[4], payload[5]]);
        let new_cursor_col = u16::from_le_bytes([payload[6], payload[7]]);
        let new_mode = u16::from_le_bytes([payload[8], payload[9]]);
        let title_field = u16::from_le_bytes([payload[10], payload[11]]);
        let title_present = title_field & TITLE_PRESENT != 0;
        let ops_present = title_field & OPS_PRESENT != 0;
        let strings_present = title_field & STRINGS_PRESENT != 0;
        let line_flags_present = title_field & LINE_FLAGS_PRESENT != 0;
        let title_len = (title_field & TITLE_LEN_MASK) as usize;

        let title_start = 12usize;
        let title_end = title_start.saturating_add(title_len);
        if payload.len() < title_end {
            return false;
        }
        let title_changed = if title_present {
            let title = String::from_utf8_lossy(&payload[title_start..title_end]).into_owned();
            self.frame.set_title(title)
        } else {
            false
        };

        let resized = new_rows != self.frame.rows || new_cols != self.frame.cols;
        if resized {
            self.frame.resize(new_rows, new_cols);
        }

        let old_cursor_row = self.frame.cursor_row;
        let old_cursor_col = self.frame.cursor_col;
        let old_mode = self.frame.mode;

        let (content_changed, ops_end) = if ops_present {
            let ops_start = title_end;
            if payload.len() < ops_start + 2 {
                return false;
            }
            let (changed, consumed) = self
                .apply_ops_payload(&payload[ops_start..])
                .unwrap_or((false, 0));
            (changed, ops_start + consumed)
        } else {
            let (changed, consumed) = self
                .apply_legacy_patch_payload(&payload[title_end..])
                .unwrap_or((false, 0));
            (changed, title_end + consumed)
        };

        let mut after_strings = ops_end;
        if strings_present {
            after_strings = self.apply_overflow_strings(&payload[ops_end..]);
            after_strings += ops_end;
        }

        let (line_flags_changed, after_line_flags) = if line_flags_present {
            let lf_start = after_strings;
            let lf_end = lf_start + new_rows as usize;
            if payload.len() >= lf_end {
                let new_flags = &payload[lf_start..lf_end];
                let changed = self.frame.line_flags != new_flags;
                self.frame.line_flags.clear();
                self.frame.line_flags.extend_from_slice(new_flags);
                (changed, lf_end)
            } else {
                (false, after_strings)
            }
        } else {
            (false, after_strings)
        };

        // Trailing scrollback count (backward-compatible extension).
        if payload.len() >= after_line_flags + 4 {
            self.frame.scrollback_lines = u32::from_le_bytes([
                payload[after_line_flags],
                payload[after_line_flags + 1],
                payload[after_line_flags + 2],
                payload[after_line_flags + 3],
            ]);
            // Trailing OSC 8 hyperlink section. A server that predates it
            // simply ends the payload here, which reads as "no links".
            self.apply_links_section(&payload[after_line_flags + 4..]);
        }

        self.frame.cursor_row = new_cursor_row.min(self.frame.rows.saturating_sub(1));
        self.frame.cursor_col = new_cursor_col.min(self.frame.cols.saturating_sub(1));
        self.frame.mode = new_mode;
        resized
            || title_changed
            || content_changed
            || line_flags_changed
            || new_cursor_row != old_cursor_row
            || new_cursor_col != old_cursor_col
            || new_mode != old_mode
    }

    /// Decode the trailing hyperlink section. Any malformed or truncated
    /// section clears links rather than leaving a half-applied mapping —
    /// showing no link is always safe, showing the *wrong* link is not.
    fn apply_links_section(&mut self, data: &[u8]) {
        if data.len() < 2 {
            self.frame.clear_links();
            return;
        }
        let uri_count = u16::from_le_bytes([data[0], data[1]]);
        if uri_count == LINKS_UNCHANGED {
            return;
        }
        let mut off = 2usize;
        let mut link_uris = BTreeMap::new();
        for _ in 0..uri_count {
            if off + 4 > data.len() {
                self.frame.clear_links();
                return;
            }
            let id = u16::from_le_bytes([data[off], data[off + 1]]);
            let len = u16::from_le_bytes([data[off + 2], data[off + 3]]) as usize;
            off += 4;
            if off + len > data.len() || len > MAX_LINK_URI {
                self.frame.clear_links();
                return;
            }
            // A URI that is not valid UTF-8, or that carries an id of 0 (the
            // "no link" sentinel), is dropped instead of being coerced.
            if id != 0
                && let Ok(uri) = std::str::from_utf8(&data[off..off + len])
            {
                link_uris.insert(id, uri.to_owned());
            }
            off += len;
        }
        if off + 2 > data.len() {
            self.frame.clear_links();
            return;
        }
        let run_count = u16::from_le_bytes([data[off], data[off + 1]]) as usize;
        off += 2;

        let total = self.frame.rows as usize * self.frame.cols as usize;
        let mut cell_links = vec![0u16; total];
        for _ in 0..run_count {
            if off + 8 > data.len() {
                self.frame.clear_links();
                return;
            }
            let start = u32::from_le_bytes([data[off], data[off + 1], data[off + 2], data[off + 3]])
                as usize;
            let len = u16::from_le_bytes([data[off + 4], data[off + 5]]) as usize;
            let id = u16::from_le_bytes([data[off + 6], data[off + 7]]);
            off += 8;
            // Ignore runs that fall outside the grid or name an unknown id
            // rather than rejecting the whole frame.
            if id == 0 || !link_uris.contains_key(&id) {
                continue;
            }
            let end = start.saturating_add(len).min(total);
            if start >= total {
                continue;
            }
            cell_links[start..end].fill(id);
        }
        self.frame.set_links(cell_links, link_uris);
    }

    fn apply_legacy_patch_payload(&mut self, payload: &[u8]) -> Option<(bool, usize)> {
        let total_cells = self.frame.rows as usize * self.frame.cols as usize;
        let bitmask_len = total_cells.div_ceil(8);
        if payload.len() < bitmask_len {
            return None;
        }
        let bitmask = &payload[..bitmask_len];
        let dirty_count = (0..total_cells)
            .filter(|&i| bitmask[i / 8] & (1 << (i % 8)) != 0)
            .count();
        let data = &payload[bitmask_len..];
        if data.len() < dirty_count * CELL_SIZE {
            return None;
        }
        self.apply_patch_cells(bitmask, &data[..dirty_count * CELL_SIZE], dirty_count);
        Some((dirty_count > 0, bitmask_len + dirty_count * CELL_SIZE))
    }

    fn apply_ops_payload(&mut self, payload: &[u8]) -> Option<(bool, usize)> {
        if payload.len() < 2 {
            return None;
        }
        let op_count = u16::from_le_bytes([payload[0], payload[1]]) as usize;
        let total_cells = self.frame.rows as usize * self.frame.cols as usize;
        let bitmask_len = total_cells.div_ceil(8);
        let mut off = 2usize;
        let mut changed = false;

        for _ in 0..op_count {
            if off >= payload.len() {
                return None;
            }
            let op = payload[off];
            off += 1;
            match op {
                OP_COPY_RECT => {
                    if payload.len() < off + 12 {
                        return None;
                    }
                    let src_row = u16::from_le_bytes([payload[off], payload[off + 1]]);
                    let src_col = u16::from_le_bytes([payload[off + 2], payload[off + 3]]);
                    let dst_row = u16::from_le_bytes([payload[off + 4], payload[off + 5]]);
                    let dst_col = u16::from_le_bytes([payload[off + 6], payload[off + 7]]);
                    let rows = u16::from_le_bytes([payload[off + 8], payload[off + 9]]);
                    let cols = u16::from_le_bytes([payload[off + 10], payload[off + 11]]);
                    off += 12;
                    changed |= self.apply_copy_rect(src_row, src_col, dst_row, dst_col, rows, cols);
                }
                OP_FILL_RECT => {
                    if payload.len() < off + 8 + CELL_SIZE {
                        return None;
                    }
                    let row = u16::from_le_bytes([payload[off], payload[off + 1]]);
                    let col = u16::from_le_bytes([payload[off + 2], payload[off + 3]]);
                    let rows = u16::from_le_bytes([payload[off + 4], payload[off + 5]]);
                    let cols = u16::from_le_bytes([payload[off + 6], payload[off + 7]]);
                    off += 8;
                    let mut cell = [0u8; CELL_SIZE];
                    cell.copy_from_slice(&payload[off..off + CELL_SIZE]);
                    off += CELL_SIZE;
                    changed |= self.apply_fill_rect(row, col, rows, cols, &cell);
                }
                OP_PATCH_CELLS => {
                    if payload.len() < off + bitmask_len {
                        return None;
                    }
                    let bitmask = &payload[off..off + bitmask_len];
                    off += bitmask_len;
                    let dirty_count = (0..total_cells)
                        .filter(|&i| bitmask[i / 8] & (1 << (i % 8)) != 0)
                        .count();
                    if payload.len() < off + dirty_count * CELL_SIZE {
                        return None;
                    }
                    self.apply_patch_cells(
                        bitmask,
                        &payload[off..off + dirty_count * CELL_SIZE],
                        dirty_count,
                    );
                    off += dirty_count * CELL_SIZE;
                    changed |= dirty_count > 0;
                }
                _ => return None,
            }
        }

        Some((changed, off))
    }

    fn apply_patch_cells(&mut self, bitmask: &[u8], data: &[u8], dirty_count: usize) {
        let total_cells = self.frame.rows as usize * self.frame.cols as usize;
        let mut dirty_idx = 0usize;
        for i in 0..total_cells {
            if bitmask[i / 8] & (1 << (i % 8)) == 0 {
                continue;
            }
            let cell_idx = i * CELL_SIZE;
            for byte_pos in 0..CELL_SIZE {
                self.frame.cells[cell_idx + byte_pos] = data[byte_pos * dirty_count + dirty_idx];
            }
            // Remove stale overflow entry when a cell is updated — it may
            // have transitioned from overflow (content_len=7) to inline.
            let new_content_len = (self.frame.cells[cell_idx + 1] >> 3) & 7;
            if new_content_len != CONTENT_OVERFLOW {
                self.frame.overflow.remove(&i);
            }
            dirty_idx += 1;
        }
    }

    fn apply_copy_rect(
        &mut self,
        src_row: u16,
        src_col: u16,
        dst_row: u16,
        dst_col: u16,
        rows: u16,
        cols: u16,
    ) -> bool {
        let rows = rows
            .min(self.frame.rows.saturating_sub(src_row))
            .min(self.frame.rows.saturating_sub(dst_row));
        let cols = cols
            .min(self.frame.cols.saturating_sub(src_col))
            .min(self.frame.cols.saturating_sub(dst_col));
        if rows == 0 || cols == 0 {
            return false;
        }

        let frame_cols = self.frame.cols as usize;

        // Copy overflow strings for the source region.
        let mut overflow_temp: Vec<(usize, String)> = Vec::new();
        for r in 0..rows as usize {
            for c in 0..cols as usize {
                let src_flat = (src_row as usize + r) * frame_cols + src_col as usize + c;
                if let Some(s) = self.frame.overflow.get(&src_flat) {
                    let dst_flat = (dst_row as usize + r) * frame_cols + dst_col as usize + c;
                    overflow_temp.push((dst_flat, s.clone()));
                }
            }
        }

        let mut temp = vec![0u8; rows as usize * cols as usize * CELL_SIZE];
        for r in 0..rows as usize {
            let src_off = self.frame.cell_offset(src_row + r as u16, src_col);
            let src_end = src_off + cols as usize * CELL_SIZE;
            let dst_off = r * cols as usize * CELL_SIZE;
            temp[dst_off..dst_off + cols as usize * CELL_SIZE]
                .copy_from_slice(&self.frame.cells[src_off..src_end]);
        }
        for r in 0..rows as usize {
            let dst_off = self.frame.cell_offset(dst_row + r as u16, dst_col);
            let dst_end = dst_off + cols as usize * CELL_SIZE;
            let src_off = r * cols as usize * CELL_SIZE;
            self.frame.cells[dst_off..dst_end]
                .copy_from_slice(&temp[src_off..src_off + cols as usize * CELL_SIZE]);
        }

        for r in 0..rows as usize {
            for c in 0..cols as usize {
                let dst_flat = (dst_row as usize + r) * frame_cols + dst_col as usize + c;
                self.frame.overflow.remove(&dst_flat);
            }
        }
        for (idx, s) in overflow_temp {
            self.frame.overflow.insert(idx, s);
        }

        true
    }

    fn apply_fill_rect(
        &mut self,
        row: u16,
        col: u16,
        rows: u16,
        cols: u16,
        cell: &[u8; CELL_SIZE],
    ) -> bool {
        let row_end = row.saturating_add(rows).min(self.frame.rows);
        let col_end = col.saturating_add(cols).min(self.frame.cols);
        // Fill cells never have overflow content — clear stale entries.
        let frame_cols = self.frame.cols as usize;
        for r in row..row_end {
            for c in col..col_end {
                self.frame
                    .overflow
                    .remove(&(r as usize * frame_cols + c as usize));
            }
        }
        if row >= row_end || col >= col_end {
            return false;
        }
        for r in row..row_end {
            for c in col..col_end {
                let off = self.frame.cell_offset(r, c);
                self.frame.cells[off..off + CELL_SIZE].copy_from_slice(cell);
            }
        }
        true
    }

    fn apply_overflow_strings(&mut self, data: &[u8]) -> usize {
        if data.len() < 2 {
            return 0;
        }
        let count = u16::from_le_bytes([data[0], data[1]]) as usize;
        let mut off = 2usize;
        for _ in 0..count {
            if off + 6 > data.len() {
                break;
            }
            let cell_idx =
                u32::from_le_bytes([data[off], data[off + 1], data[off + 2], data[off + 3]])
                    as usize;
            let len = u16::from_le_bytes([data[off + 4], data[off + 5]]) as usize;
            off += 6;
            if off + len > data.len() {
                break;
            }
            if let Ok(s) = std::str::from_utf8(&data[off..off + len]) {
                // Only accept indices within the current grid to prevent
                // unbounded BTreeMap growth from malicious wire data.
                let max_idx = self.frame.rows as usize * self.frame.cols as usize;
                if cell_idx < max_idx {
                    self.frame.overflow.insert(cell_idx, s.to_owned());
                }
            }
            off += len;
        }
        off
    }
}

#[derive(Clone, Debug)]
pub enum Node {
    Fill {
        rect: Rect,
        ch: char,
        style: CellStyle,
    },
    Text {
        row: u16,
        col: u16,
        text: String,
        style: CellStyle,
    },
    WrappedText {
        rect: Rect,
        text: String,
        style: CellStyle,
    },
    ScrollingText {
        rect: Rect,
        lines: Vec<String>,
        offset_from_bottom: usize,
        style: CellStyle,
    },
}

#[derive(Clone, Debug, Default)]
pub struct Dom {
    background: CellStyle,
    title: Option<String>,
    nodes: Vec<Node>,
}

impl Dom {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn clear(&mut self) {
        self.title = None;
        self.nodes.clear();
    }

    pub fn set_background(&mut self, style: CellStyle) {
        self.background = style;
    }

    pub fn set_title(&mut self, title: impl Into<String>) {
        self.title = Some(title.into());
    }

    pub fn fill(&mut self, rect: Rect, ch: char, style: CellStyle) {
        self.nodes.push(Node::Fill { rect, ch, style });
    }

    pub fn text(&mut self, row: u16, col: u16, text: impl Into<String>, style: CellStyle) {
        self.nodes.push(Node::Text {
            row,
            col,
            text: text.into(),
            style,
        });
    }

    pub fn wrapped_text(&mut self, rect: Rect, text: impl Into<String>, style: CellStyle) {
        self.nodes.push(Node::WrappedText {
            rect,
            text: text.into(),
            style,
        });
    }

    pub fn scrolling_text<S, I>(
        &mut self,
        rect: Rect,
        lines: I,
        offset_from_bottom: usize,
        style: CellStyle,
    ) where
        S: Into<String>,
        I: IntoIterator<Item = S>,
    {
        self.nodes.push(Node::ScrollingText {
            rect,
            lines: lines.into_iter().map(Into::into).collect(),
            offset_from_bottom,
            style,
        });
    }

    pub fn render_to(&self, frame: &mut FrameState) {
        frame.clear(self.background);
        frame.set_title(self.title.clone().unwrap_or_default());
        for node in &self.nodes {
            match node {
                Node::Fill { rect, ch, style } => frame.fill_rect(*rect, *ch, *style),
                Node::Text {
                    row,
                    col,
                    text,
                    style,
                } => {
                    frame.write_text(*row, *col, text, *style);
                }
                Node::WrappedText { rect, text, style } => {
                    frame.write_wrapped_text(*rect, text, *style);
                }
                Node::ScrollingText {
                    rect,
                    lines,
                    offset_from_bottom,
                    style,
                } => {
                    frame.write_scrolling_text(*rect, lines, *offset_from_bottom, *style);
                }
            }
        }
    }
}

#[derive(Clone, Debug)]
pub struct CallbackRenderer {
    dom: Dom,
    frame: FrameState,
}

impl CallbackRenderer {
    pub fn new(rows: u16, cols: u16) -> Self {
        Self {
            dom: Dom::new(),
            frame: FrameState::new(rows, cols),
        }
    }

    pub fn resize(&mut self, rows: u16, cols: u16) {
        self.frame.resize(rows, cols);
    }

    pub fn frame(&self) -> &FrameState {
        &self.frame
    }

    pub fn render<F>(&mut self, render: F) -> &FrameState
    where
        F: FnOnce(&mut Dom),
    {
        self.dom.clear();
        render(&mut self.dom);
        self.dom.render_to(&mut self.frame);
        &self.frame
    }
}

pub enum ServerMsg<'a> {
    Hello {
        version: u16,
        features: u32,
        boot_generation: Option<u64>,
        /// The server's crate version (e.g. `"0.40.1"`).  `None` from servers
        /// that predate the field.
        server_version: Option<&'a str>,
    },
    Update {
        pty_id: u16,
        payload: &'a [u8],
    },
    Created {
        pty_id: u16,
        tag: &'a str,
    },
    CreatedN {
        nonce: u16,
        pty_id: u16,
        tag: &'a str,
    },
    /// The other half of a `CREATE2(WANT_STATUS)` outcome.  `status` is a
    /// common-registry value; render it with [`status_text`] when `detail`
    /// is empty.
    CreateFailed {
        nonce: u16,
        status: u8,
        detail: &'a str,
    },
    Closed {
        pty_id: u16,
    },
    Exited {
        pty_id: u16,
        exit_status: i32,
        /// `EXIT_REASON_NORMAL` from servers that predate the field.
        reason: u8,
    },
    List {
        entries: Vec<PtyListEntry<'a>>,
    },
    Title {
        pty_id: u16,
        title: &'a [u8],
    },
    SearchResults {
        request_id: u16,
        results: Vec<SearchResultEntry<'a>>,
    },
    Ready,
    Text {
        nonce: u16,
        pty_id: u16,
        total_lines: u32,
        offset: u32,
        text: &'a str,
    },
    SurfaceCreated {
        surface_id: u16,
        parent_id: u16,
        width: u16,
        height: u16,
        title: &'a str,
        app_id: &'a str,
    },
    SurfaceDestroyed {
        surface_id: u16,
    },
    SurfaceFrame {
        surface_id: u16,
        timestamp: u32,
        timestamp_sub_us: Option<u16>,
        flags: u8,
        width: u16,
        height: u16,
        data: &'a [u8],
    },
    SurfaceTitle {
        surface_id: u16,
        title: &'a str,
    },
    SurfaceAppId {
        surface_id: u16,
        app_id: &'a str,
    },
    SurfaceActivated {
        surface_id: u16,
    },
    SurfaceResized {
        surface_id: u16,
        width: u16,
        height: u16,
        /// Surface-logical size, or `None` from a server that predates the
        /// field.  See [`S2C_SURFACE_RESIZED`].
        logical: Option<(u16, u16)>,
    },
    ClipboardContent {
        mime_type: &'a str,
        data: &'a [u8],
    },
    SurfaceList {
        entries: Vec<SurfaceListEntry>,
    },
    SurfaceCapture {
        surface_id: u16,
        width: u32,
        height: u32,
        image_data: &'a [u8],
    },
    ClipboardList {
        mime_types: Vec<String>,
    },
    ClipboardOwner {
        wayland: bool,
    },
    Quit,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PtyListEntry<'a> {
    pub pty_id: u16,
    pub tag: &'a str,
    pub command: &'a str,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SurfaceListEntry {
    pub surface_id: u16,
    pub parent_id: u16,
    pub width: u16,
    pub height: u16,
    pub title: String,
    pub app_id: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SearchResultEntry<'a> {
    pub pty_id: u16,
    pub score: u32,
    pub primary_source: u8,
    pub matched_sources: u8,
    pub scroll_offset: Option<u32>,
    pub context: &'a [u8],
}

pub fn parse_server_msg(data: &[u8]) -> Option<ServerMsg<'_>> {
    if data.is_empty() {
        return None;
    }
    match data[0] {
        S2C_HELLO => {
            if data.len() < 7 {
                return None;
            }
            let version = u16::from_le_bytes([data[1], data[2]]);
            let features = u32::from_le_bytes([data[3], data[4], data[5], data[6]]);
            // The boot generation was appended to HELLO without changing the
            // protocol version, so continue to accept HELLOs from older servers.
            let boot_generation = (data.len() >= 15)
                .then(|| u64::from_le_bytes(data[7..15].try_into().expect("checked HELLO length")));
            // The server's crate version came later still, same deal: a
            // length-prefixed string appended after the boot generation.
            let server_version = (data.len() >= 17)
                .then(|| {
                    let len = u16::from_le_bytes([data[15], data[16]]) as usize;
                    data.get(17..17 + len)
                        .and_then(|v| std::str::from_utf8(v).ok())
                        .filter(|v| !v.is_empty())
                })
                .flatten();
            Some(ServerMsg::Hello {
                version,
                features,
                boot_generation,
                server_version,
            })
        }
        S2C_UPDATE => {
            if data.len() < 3 {
                return None;
            }
            Some(ServerMsg::Update {
                pty_id: u16::from_le_bytes([data[1], data[2]]),
                payload: &data[3..],
            })
        }
        S2C_CREATED => {
            if data.len() < 3 {
                return None;
            }
            let tag = std::str::from_utf8(data.get(3..).unwrap_or_default()).unwrap_or_default();
            Some(ServerMsg::Created {
                pty_id: u16::from_le_bytes([data[1], data[2]]),
                tag,
            })
        }
        S2C_CREATED_N => {
            if data.len() < 5 {
                return None;
            }
            let nonce = u16::from_le_bytes([data[1], data[2]]);
            let pty_id = u16::from_le_bytes([data[3], data[4]]);
            let tag = std::str::from_utf8(data.get(5..).unwrap_or_default()).unwrap_or_default();
            Some(ServerMsg::CreatedN { nonce, pty_id, tag })
        }
        S2C_CREATE_FAILED => {
            if data.len() < 4 {
                return None;
            }
            let nonce = u16::from_le_bytes([data[1], data[2]]);
            let detail = std::str::from_utf8(data.get(4..).unwrap_or_default()).unwrap_or_default();
            Some(ServerMsg::CreateFailed {
                nonce,
                status: data[3],
                detail,
            })
        }
        S2C_CLOSED => {
            if data.len() < 3 {
                return None;
            }
            Some(ServerMsg::Closed {
                pty_id: u16::from_le_bytes([data[1], data[2]]),
            })
        }
        S2C_EXITED => {
            if data.len() < 7 {
                return None;
            }
            Some(ServerMsg::Exited {
                pty_id: u16::from_le_bytes([data[1], data[2]]),
                exit_status: i32::from_le_bytes([data[3], data[4], data[5], data[6]]),
                // Appended field: a server that predates it sends 7 bytes.
                reason: data.get(7).copied().unwrap_or(EXIT_REASON_NORMAL),
            })
        }
        S2C_LIST => {
            if data.len() < 3 {
                return None;
            }
            let count = u16::from_le_bytes([data[1], data[2]]) as usize;
            let mut entries = Vec::with_capacity(count);
            let mut offset = 3;
            for _ in 0..count {
                if offset + 4 > data.len() {
                    break;
                }
                let pty_id = u16::from_le_bytes([data[offset], data[offset + 1]]);
                let tag_len = u16::from_le_bytes([data[offset + 2], data[offset + 3]]) as usize;
                offset += 4;
                if offset + tag_len > data.len() {
                    break;
                }
                let tag = std::str::from_utf8(&data[offset..offset + tag_len]).unwrap_or_default();
                offset += tag_len;
                let command = if offset + 2 <= data.len() {
                    let cmd_len = u16::from_le_bytes([data[offset], data[offset + 1]]) as usize;
                    offset += 2;
                    if offset + cmd_len <= data.len() {
                        let cmd = std::str::from_utf8(&data[offset..offset + cmd_len])
                            .unwrap_or_default();
                        offset += cmd_len;
                        cmd
                    } else {
                        // Truncated command — don't advance offset past
                        // available data; stop parsing this entry.
                        offset = data.len();
                        ""
                    }
                } else {
                    ""
                };
                entries.push(PtyListEntry {
                    pty_id,
                    tag,
                    command,
                });
            }
            Some(ServerMsg::List { entries })
        }
        S2C_TITLE => {
            if data.len() < 3 {
                return None;
            }
            Some(ServerMsg::Title {
                pty_id: u16::from_le_bytes([data[1], data[2]]),
                title: &data[3..],
            })
        }
        S2C_SEARCH_RESULTS => {
            if data.len() < 5 {
                return None;
            }
            let request_id = u16::from_le_bytes([data[1], data[2]]);
            let count = u16::from_le_bytes([data[3], data[4]]) as usize;
            let mut results = Vec::with_capacity(count);
            let mut offset = 5usize;
            for _ in 0..count {
                if offset + 14 > data.len() {
                    return None;
                }
                let pty_id = u16::from_le_bytes([data[offset], data[offset + 1]]);
                let score = u32::from_le_bytes([
                    data[offset + 2],
                    data[offset + 3],
                    data[offset + 4],
                    data[offset + 5],
                ]);
                let primary_source = data[offset + 6];
                let matched_sources = data[offset + 7];
                let scroll_offset = u32::from_le_bytes([
                    data[offset + 8],
                    data[offset + 9],
                    data[offset + 10],
                    data[offset + 11],
                ]);
                let context_len =
                    u16::from_le_bytes([data[offset + 12], data[offset + 13]]) as usize;
                offset += 14;
                if offset + context_len > data.len() {
                    return None;
                }
                results.push(SearchResultEntry {
                    pty_id,
                    score,
                    primary_source,
                    matched_sources,
                    scroll_offset: if scroll_offset == u32::MAX {
                        None
                    } else {
                        Some(scroll_offset)
                    },
                    context: &data[offset..offset + context_len],
                });
                offset += context_len;
            }
            Some(ServerMsg::SearchResults {
                request_id,
                results,
            })
        }
        S2C_READY => Some(ServerMsg::Ready),
        S2C_TEXT => {
            if data.len() < 13 {
                return None;
            }
            let nonce = u16::from_le_bytes([data[1], data[2]]);
            let pty_id = u16::from_le_bytes([data[3], data[4]]);
            let total_lines = u32::from_le_bytes([data[5], data[6], data[7], data[8]]);
            let offset = u32::from_le_bytes([data[9], data[10], data[11], data[12]]);
            let text = std::str::from_utf8(data.get(13..).unwrap_or_default()).unwrap_or_default();
            Some(ServerMsg::Text {
                nonce,
                pty_id,
                total_lines,
                offset,
                text,
            })
        }
        S2C_SURFACE_CREATED => {
            if data.len() < 13 {
                return None;
            }
            let surface_id = u16::from_le_bytes([data[1], data[2]]);
            let parent_id = u16::from_le_bytes([data[3], data[4]]);
            let width = u16::from_le_bytes([data[5], data[6]]);
            let height = u16::from_le_bytes([data[7], data[8]]);
            let title_len = u16::from_le_bytes([data[9], data[10]]) as usize;
            let mut off = 11;
            if off + title_len + 2 > data.len() {
                return None;
            }
            let title = std::str::from_utf8(&data[off..off + title_len]).unwrap_or_default();
            off += title_len;
            let app_id_len = u16::from_le_bytes([data[off], data[off + 1]]) as usize;
            off += 2;
            if off + app_id_len > data.len() {
                return None;
            }
            let app_id = std::str::from_utf8(&data[off..off + app_id_len]).unwrap_or_default();
            Some(ServerMsg::SurfaceCreated {
                surface_id,
                parent_id,
                width,
                height,
                title,
                app_id,
            })
        }
        S2C_SURFACE_DESTROYED => {
            if data.len() < 3 {
                return None;
            }
            Some(ServerMsg::SurfaceDestroyed {
                surface_id: u16::from_le_bytes([data[1], data[2]]),
            })
        }
        S2C_SURFACE_FRAME => {
            if data.len() < 12 {
                return None;
            }
            let flags = data[7];
            let has_sub_us = flags & SURFACE_FRAME_FLAG_TIMESTAMP_SUB_US != 0;
            if has_sub_us && data.len() < 14 {
                return None;
            }
            Some(ServerMsg::SurfaceFrame {
                surface_id: u16::from_le_bytes([data[1], data[2]]),
                timestamp: u32::from_le_bytes([data[3], data[4], data[5], data[6]]),
                timestamp_sub_us: has_sub_us.then(|| u16::from_le_bytes([data[12], data[13]])),
                flags,
                width: u16::from_le_bytes([data[8], data[9]]),
                height: u16::from_le_bytes([data[10], data[11]]),
                data: data
                    .get(if has_sub_us { 14.. } else { 12.. })
                    .unwrap_or_default(),
            })
        }
        S2C_SURFACE_TITLE => {
            if data.len() < 3 {
                return None;
            }
            let title = std::str::from_utf8(data.get(3..).unwrap_or_default()).unwrap_or_default();
            Some(ServerMsg::SurfaceTitle {
                surface_id: u16::from_le_bytes([data[1], data[2]]),
                title,
            })
        }
        S2C_SURFACE_APP_ID => {
            if data.len() < 3 {
                return None;
            }
            let app_id = std::str::from_utf8(data.get(3..).unwrap_or_default()).unwrap_or_default();
            Some(ServerMsg::SurfaceAppId {
                surface_id: u16::from_le_bytes([data[1], data[2]]),
                app_id,
            })
        }
        S2C_SURFACE_ACTIVATED => {
            if data.len() < 3 {
                return None;
            }
            Some(ServerMsg::SurfaceActivated {
                surface_id: u16::from_le_bytes([data[1], data[2]]),
            })
        }
        S2C_SURFACE_RESIZED => {
            if data.len() < 7 {
                return None;
            }
            Some(ServerMsg::SurfaceResized {
                surface_id: u16::from_le_bytes([data[1], data[2]]),
                width: u16::from_le_bytes([data[3], data[4]]),
                height: u16::from_le_bytes([data[5], data[6]]),
                logical: (data.len() >= 11).then(|| {
                    (
                        u16::from_le_bytes([data[7], data[8]]),
                        u16::from_le_bytes([data[9], data[10]]),
                    )
                }),
            })
        }
        S2C_CLIPBOARD_CONTENT => {
            if data.len() < 7 {
                return None;
            }
            let mime_len = u16::from_le_bytes([data[1], data[2]]) as usize;
            let mut off = 3;
            if off + mime_len + 4 > data.len() {
                return None;
            }
            let mime_type = std::str::from_utf8(&data[off..off + mime_len]).unwrap_or_default();
            off += mime_len;
            let data_len =
                u32::from_le_bytes([data[off], data[off + 1], data[off + 2], data[off + 3]])
                    as usize;
            off += 4;
            if off + data_len > data.len() {
                return None;
            }
            Some(ServerMsg::ClipboardContent {
                mime_type,
                data: &data[off..off + data_len],
            })
        }
        S2C_SURFACE_LIST => {
            if data.len() < 3 {
                return None;
            }
            let count = u16::from_le_bytes([data[1], data[2]]) as usize;
            let mut entries = Vec::with_capacity(count);
            let mut offset = 3;
            for _ in 0..count {
                if offset + 8 > data.len() {
                    break;
                }
                let surface_id = u16::from_le_bytes([data[offset], data[offset + 1]]);
                let parent_id = u16::from_le_bytes([data[offset + 2], data[offset + 3]]);
                let width = u16::from_le_bytes([data[offset + 4], data[offset + 5]]);
                let height = u16::from_le_bytes([data[offset + 6], data[offset + 7]]);
                offset += 8;
                if offset + 2 > data.len() {
                    break;
                }
                let title_len = u16::from_le_bytes([data[offset], data[offset + 1]]) as usize;
                offset += 2;
                if offset + title_len > data.len() {
                    break;
                }
                let title =
                    std::str::from_utf8(&data[offset..offset + title_len]).unwrap_or_default();
                offset += title_len;
                if offset + 2 > data.len() {
                    break;
                }
                let app_id_len = u16::from_le_bytes([data[offset], data[offset + 1]]) as usize;
                offset += 2;
                if offset + app_id_len > data.len() {
                    break;
                }
                let app_id =
                    std::str::from_utf8(&data[offset..offset + app_id_len]).unwrap_or_default();
                offset += app_id_len;
                entries.push(SurfaceListEntry {
                    surface_id,
                    parent_id,
                    width,
                    height,
                    title: title.to_string(),
                    app_id: app_id.to_string(),
                });
            }
            Some(ServerMsg::SurfaceList { entries })
        }
        S2C_SURFACE_CAPTURE => {
            if data.len() < 11 {
                return None;
            }
            let surface_id = u16::from_le_bytes([data[1], data[2]]);
            let width = u32::from_le_bytes([data[3], data[4], data[5], data[6]]);
            let height = u32::from_le_bytes([data[7], data[8], data[9], data[10]]);
            let image_data = data.get(11..).unwrap_or_default();
            Some(ServerMsg::SurfaceCapture {
                surface_id,
                width,
                height,
                image_data,
            })
        }
        S2C_CLIPBOARD_LIST => {
            if data.len() < 3 {
                return None;
            }
            let count = u16::from_le_bytes([data[1], data[2]]) as usize;
            let mut mime_types = Vec::with_capacity(count);
            let mut offset = 3;
            for _ in 0..count {
                if offset + 2 > data.len() {
                    break;
                }
                let mime_len = u16::from_le_bytes([data[offset], data[offset + 1]]) as usize;
                offset += 2;
                if offset + mime_len > data.len() {
                    break;
                }
                let mime =
                    std::str::from_utf8(&data[offset..offset + mime_len]).unwrap_or_default();
                mime_types.push(mime.to_string());
                offset += mime_len;
            }
            Some(ServerMsg::ClipboardList { mime_types })
        }
        S2C_CLIPBOARD_OWNER => {
            if data.len() != 2 || data[1] > 1 {
                return None;
            }
            Some(ServerMsg::ClipboardOwner {
                wayland: data[1] != 0,
            })
        }
        S2C_QUIT => Some(ServerMsg::Quit),
        _ => None,
    }
}

pub fn msg_hello(
    version: u16,
    features: u32,
    boot_generation: u64,
    server_version: &str,
) -> Vec<u8> {
    let ver_bytes = server_version.as_bytes();
    let ver_len = ver_bytes.len().min(u16::MAX as usize);
    let mut msg = Vec::with_capacity(17 + ver_len);
    msg.push(S2C_HELLO);
    msg.extend_from_slice(&version.to_le_bytes());
    msg.extend_from_slice(&features.to_le_bytes());
    msg.extend_from_slice(&boot_generation.to_le_bytes());
    msg.extend_from_slice(&(ver_len as u16).to_le_bytes());
    msg.extend_from_slice(&ver_bytes[..ver_len]);
    msg
}

pub fn msg_create(rows: u16, cols: u16) -> Vec<u8> {
    msg_create_tagged(rows, cols, "")
}

pub fn msg_create_tagged(rows: u16, cols: u16, tag: &str) -> Vec<u8> {
    let tag_bytes = tag.as_bytes();
    let tag_len = tag_bytes.len().min(u16::MAX as usize);
    let mut msg = Vec::with_capacity(7 + tag_len);
    msg.push(C2S_CREATE);
    msg.extend_from_slice(&rows.to_le_bytes());
    msg.extend_from_slice(&cols.to_le_bytes());
    msg.extend_from_slice(&(tag_len as u16).to_le_bytes());
    msg.extend_from_slice(&tag_bytes[..tag_len]);
    msg
}

/// Spawn a new PTY in the same working directory as `src_pty_id`.
pub fn msg_create_at(rows: u16, cols: u16, tag: &str, src_pty_id: u16) -> Vec<u8> {
    let tag_bytes = tag.as_bytes();
    let tag_len = tag_bytes.len().min(u16::MAX as usize);
    let mut msg = Vec::with_capacity(9 + tag_len);
    msg.push(C2S_CREATE_AT);
    msg.extend_from_slice(&rows.to_le_bytes());
    msg.extend_from_slice(&cols.to_le_bytes());
    msg.extend_from_slice(&(tag_len as u16).to_le_bytes());
    msg.extend_from_slice(&tag_bytes[..tag_len]);
    msg.extend_from_slice(&src_pty_id.to_le_bytes());
    msg
}

pub fn msg_create_n(nonce: u16, rows: u16, cols: u16, tag: &str) -> Vec<u8> {
    let tag_bytes = tag.as_bytes();
    let tag_len = tag_bytes.len().min(u16::MAX as usize);
    let mut msg = Vec::with_capacity(9 + tag_len);
    msg.push(C2S_CREATE_N);
    msg.extend_from_slice(&nonce.to_le_bytes());
    msg.extend_from_slice(&rows.to_le_bytes());
    msg.extend_from_slice(&cols.to_le_bytes());
    msg.extend_from_slice(&(tag_len as u16).to_le_bytes());
    msg.extend_from_slice(&tag_bytes[..tag_len]);
    msg
}

pub fn msg_create_n_command(nonce: u16, rows: u16, cols: u16, tag: &str, command: &str) -> Vec<u8> {
    let mut msg = msg_create_n(nonce, rows, cols, tag);
    msg.extend_from_slice(command.as_bytes());
    msg
}

pub fn msg_create2(
    nonce: u16,
    rows: u16,
    cols: u16,
    tag: &str,
    command: &str,
    features: u8,
) -> Vec<u8> {
    msg_create2_with_cwd(nonce, rows, cols, tag, command, features, None)
}

pub fn msg_create2_with_cwd(
    nonce: u16,
    rows: u16,
    cols: u16,
    tag: &str,
    command: &str,
    features: u8,
    cwd: Option<&str>,
) -> Vec<u8> {
    msg_create2_full(nonce, rows, cols, tag, command, features, cwd, None)
}

/// `C2S_CREATE2` with every optional field.
///
/// Field order is load-bearing: the command has no length prefix and runs to
/// the end of the message, so everything else has to precede it.
///
/// Only pass `deadline_ms` to a server advertising [`FEATURE_PTY_DEADLINE`].
/// An older one does not know bit 4, so it will not skip the four bytes and
/// will read them as the first four bytes of the command — spawning something
/// other than what was asked for, silently. Every other optional field is
/// safe against an old server, because an unknown flag with no trailing bytes
/// is merely ignored; this one is not.
#[allow(clippy::too_many_arguments)]
pub fn msg_create2_full(
    nonce: u16,
    rows: u16,
    cols: u16,
    tag: &str,
    command: &str,
    features: u8,
    cwd: Option<&str>,
    deadline_ms: Option<u32>,
) -> Vec<u8> {
    let tag_bytes = tag.as_bytes();
    let cmd_bytes = command.as_bytes();
    let cwd_bytes = cwd.unwrap_or_default().as_bytes();
    let has_cmd = !command.is_empty();
    let cwd_len = cwd_bytes.len().min(u16::MAX as usize);
    let has_cwd = cwd_len > 0;
    let feat = features
        | if has_cmd { CREATE2_HAS_COMMAND } else { 0 }
        | if has_cwd { CREATE2_HAS_CWD } else { 0 }
        | if deadline_ms.is_some() {
            CREATE2_HAS_DEADLINE
        } else {
            0
        };
    let tag_len = tag_bytes.len().min(u16::MAX as usize);
    let mut msg = Vec::with_capacity(
        10 + tag_len
            + if has_cwd { 2 + cwd_len } else { 0 }
            + if deadline_ms.is_some() { 4 } else { 0 }
            + cmd_bytes.len(),
    );
    msg.push(C2S_CREATE2);
    msg.extend_from_slice(&nonce.to_le_bytes());
    msg.extend_from_slice(&rows.to_le_bytes());
    msg.extend_from_slice(&cols.to_le_bytes());
    msg.push(feat);
    msg.extend_from_slice(&(tag_len as u16).to_le_bytes());
    msg.extend_from_slice(&tag_bytes[..tag_len]);
    if has_cwd {
        msg.extend_from_slice(&(cwd_len as u16).to_le_bytes());
        msg.extend_from_slice(&cwd_bytes[..cwd_len]);
    }
    if let Some(ms) = deadline_ms {
        msg.extend_from_slice(&ms.to_le_bytes());
    }
    if has_cmd {
        msg.extend_from_slice(cmd_bytes);
    }
    msg
}

pub fn msg_create_command(rows: u16, cols: u16, command: &str) -> Vec<u8> {
    msg_create_tagged_command(rows, cols, "", command)
}

pub fn msg_create_tagged_command(rows: u16, cols: u16, tag: &str, command: &str) -> Vec<u8> {
    let mut msg = msg_create_tagged(rows, cols, tag);
    msg.extend_from_slice(command.as_bytes());
    msg
}

pub fn msg_input(pty_id: u16, data: &[u8]) -> Vec<u8> {
    let mut msg = Vec::with_capacity(3 + data.len());
    msg.push(C2S_INPUT);
    msg.extend_from_slice(&pty_id.to_le_bytes());
    msg.extend_from_slice(data);
    msg
}

pub fn msg_mouse(pty_id: u16, type_: u8, button: u8, col: u16, row: u16) -> Vec<u8> {
    let mut msg = Vec::with_capacity(9);
    msg.push(C2S_MOUSE);
    msg.extend_from_slice(&pty_id.to_le_bytes());
    msg.push(type_);
    msg.push(button);
    msg.extend_from_slice(&col.to_le_bytes());
    msg.extend_from_slice(&row.to_le_bytes());
    msg
}

pub fn msg_resize(pty_id: u16, rows: u16, cols: u16) -> Vec<u8> {
    let mut msg = Vec::with_capacity(7);
    msg.push(C2S_RESIZE);
    msg.extend_from_slice(&pty_id.to_le_bytes());
    msg.extend_from_slice(&rows.to_le_bytes());
    msg.extend_from_slice(&cols.to_le_bytes());
    msg
}

pub fn msg_resize_batch(entries: &[(u16, u16, u16)]) -> Vec<u8> {
    let mut msg = Vec::with_capacity(1 + entries.len() * 6);
    msg.push(C2S_RESIZE);
    for &(pty_id, rows, cols) in entries {
        msg.extend_from_slice(&pty_id.to_le_bytes());
        msg.extend_from_slice(&rows.to_le_bytes());
        msg.extend_from_slice(&cols.to_le_bytes());
    }
    msg
}

pub fn msg_focus(pty_id: u16) -> Vec<u8> {
    let mut msg = Vec::with_capacity(3);
    msg.push(C2S_FOCUS);
    msg.extend_from_slice(&pty_id.to_le_bytes());
    msg
}

pub fn msg_close(pty_id: u16) -> Vec<u8> {
    let mut msg = Vec::with_capacity(3);
    msg.push(C2S_CLOSE);
    msg.extend_from_slice(&pty_id.to_le_bytes());
    msg
}

pub fn msg_kill(pty_id: u16, signal: i32) -> Vec<u8> {
    let mut msg = Vec::with_capacity(7);
    msg.push(C2S_KILL);
    msg.extend_from_slice(&pty_id.to_le_bytes());
    msg.extend_from_slice(&signal.to_le_bytes());
    msg
}

/// `C2S_KILL` with an explicit mode.  Only send this to a server advertising
/// [`FEATURE_KILL_MODE`] — an older one ignores the trailing byte and signals
/// the leader alone, which is the opposite of what `leader_only = false`
/// asks for.
pub fn msg_kill_mode(pty_id: u16, signal: i32, leader_only: bool) -> Vec<u8> {
    let mut msg = msg_kill(pty_id, signal);
    msg.push(if leader_only { KILL_LEADER_ONLY } else { 0 });
    msg
}

pub fn msg_restart(pty_id: u16) -> Vec<u8> {
    let mut msg = Vec::with_capacity(3);
    msg.push(C2S_RESTART);
    msg.extend_from_slice(&pty_id.to_le_bytes());
    msg
}

pub fn msg_subscribe(pty_id: u16) -> Vec<u8> {
    let mut msg = Vec::with_capacity(3);
    msg.push(C2S_SUBSCRIBE);
    msg.extend_from_slice(&pty_id.to_le_bytes());
    msg
}

pub fn msg_unsubscribe(pty_id: u16) -> Vec<u8> {
    let mut msg = Vec::with_capacity(3);
    msg.push(C2S_UNSUBSCRIBE);
    msg.extend_from_slice(&pty_id.to_le_bytes());
    msg
}

pub fn msg_search(request_id: u16, query: &str) -> Vec<u8> {
    let query = query.as_bytes();
    let mut msg = Vec::with_capacity(3 + query.len());
    msg.push(C2S_SEARCH);
    msg.extend_from_slice(&request_id.to_le_bytes());
    msg.extend_from_slice(query);
    msg
}

pub fn msg_ack() -> Vec<u8> {
    vec![C2S_ACK]
}

pub fn msg_scroll(pty_id: u16, offset: u32) -> Vec<u8> {
    let mut msg = Vec::with_capacity(7);
    msg.push(C2S_SCROLL);
    msg.extend_from_slice(&pty_id.to_le_bytes());
    msg.extend_from_slice(&offset.to_le_bytes());
    msg
}

pub fn msg_display_rate(fps: u16) -> Vec<u8> {
    let mut msg = Vec::with_capacity(3);
    msg.push(C2S_DISPLAY_RATE);
    msg.extend_from_slice(&fps.to_le_bytes());
    msg
}

pub fn msg_client_metrics(backlog: u16, ack_ahead: u16, apply_ms_x10: u16) -> Vec<u8> {
    let mut msg = Vec::with_capacity(7);
    msg.push(C2S_CLIENT_METRICS);
    msg.extend_from_slice(&backlog.to_le_bytes());
    msg.extend_from_slice(&ack_ahead.to_le_bytes());
    msg.extend_from_slice(&apply_ms_x10.to_le_bytes());
    msg
}

pub fn msg_read(nonce: u16, pty_id: u16, offset: u32, limit: u32, flags: u8) -> Vec<u8> {
    let mut msg = Vec::with_capacity(14);
    msg.push(C2S_READ);
    msg.extend_from_slice(&nonce.to_le_bytes());
    msg.extend_from_slice(&pty_id.to_le_bytes());
    msg.extend_from_slice(&offset.to_le_bytes());
    msg.extend_from_slice(&limit.to_le_bytes());
    msg.push(flags);
    msg
}

/// Build a `C2S_TERM_CWD`.
pub fn msg_term_cwd(nonce: u16, pty_id: u16) -> Vec<u8> {
    let mut m = Vec::with_capacity(5);
    m.push(C2S_TERM_CWD);
    m.extend_from_slice(&nonce.to_le_bytes());
    m.extend_from_slice(&pty_id.to_le_bytes());
    m
}

/// Parse a `C2S_TERM_CWD` → `(nonce, pty_id)`.
pub fn parse_term_cwd(data: &[u8]) -> Option<(u16, u16)> {
    if data.first().copied() != Some(C2S_TERM_CWD) || data.len() < 5 {
        return None;
    }
    Some((
        u16::from_le_bytes([data[1], data[2]]),
        u16::from_le_bytes([data[3], data[4]]),
    ))
}

/// Build an `S2C_TERM_CWD` reply (empty `cwd` = unavailable).
pub fn msg_term_cwd_reply(nonce: u16, cwd: &str) -> Vec<u8> {
    let cb = cwd.as_bytes();
    let mut m = Vec::with_capacity(5 + cb.len());
    m.push(S2C_TERM_CWD);
    m.extend_from_slice(&nonce.to_le_bytes());
    m.extend_from_slice(&(cb.len() as u16).to_le_bytes());
    m.extend_from_slice(cb);
    m
}

/// Parse an `S2C_TERM_CWD` reply → `(nonce, cwd)`.
pub fn parse_term_cwd_reply(data: &[u8]) -> Option<(u16, String)> {
    if data.first().copied() != Some(S2C_TERM_CWD) || data.len() < 5 {
        return None;
    }
    let nonce = u16::from_le_bytes([data[1], data[2]]);
    let len = u16::from_le_bytes([data[3], data[4]]) as usize;
    if data.len() < 5 + len {
        return None;
    }
    Some((
        nonce,
        String::from_utf8_lossy(&data[5..5 + len]).into_owned(),
    ))
}

/// Build an `S2C_TERM_CWD_EVENT` push (unsolicited cwd change).
pub fn msg_term_cwd_event(pty_id: u16, cwd: &str) -> Vec<u8> {
    let cb = cwd.as_bytes();
    let mut m = Vec::with_capacity(3 + cb.len());
    m.push(S2C_TERM_CWD_EVENT);
    m.extend_from_slice(&pty_id.to_le_bytes());
    m.extend_from_slice(cb);
    m
}

/// Parse an `S2C_TERM_CWD_EVENT` → `(pty_id, cwd)`.
pub fn parse_term_cwd_event(data: &[u8]) -> Option<(u16, String)> {
    if data.first().copied() != Some(S2C_TERM_CWD_EVENT) || data.len() < 3 {
        return None;
    }
    Some((
        u16::from_le_bytes([data[1], data[2]]),
        String::from_utf8_lossy(&data[3..]).into_owned(),
    ))
}

pub fn msg_copy_range(
    nonce: u16,
    pty_id: u16,
    start_tail: u32,
    start_col: u16,
    end_tail: u32,
    end_col: u16,
    flags: u8,
) -> Vec<u8> {
    let mut msg = Vec::with_capacity(18);
    msg.push(C2S_COPY_RANGE);
    msg.extend_from_slice(&nonce.to_le_bytes());
    msg.extend_from_slice(&pty_id.to_le_bytes());
    msg.extend_from_slice(&start_tail.to_le_bytes());
    msg.extend_from_slice(&start_col.to_le_bytes());
    msg.extend_from_slice(&end_tail.to_le_bytes());
    msg.extend_from_slice(&end_col.to_le_bytes());
    msg.push(flags);
    msg
}

/// Build an `S2C_CREATE_FAILED`.  `detail` is truncated to
/// [`CREATE_FAILED_DETAIL_MAX`] on a UTF-8 boundary so the message stays
/// decodable as text.
pub fn msg_create_failed(nonce: u16, status: u8, detail: &str) -> Vec<u8> {
    let mut end = detail.len().min(CREATE_FAILED_DETAIL_MAX);
    while end > 0 && !detail.is_char_boundary(end) {
        end -= 1;
    }
    let detail = &detail.as_bytes()[..end];
    let mut msg = Vec::with_capacity(4 + detail.len());
    msg.push(S2C_CREATE_FAILED);
    msg.extend_from_slice(&nonce.to_le_bytes());
    msg.push(status);
    msg.extend_from_slice(detail);
    msg
}

pub fn msg_exited(pty_id: u16, exit_status: i32) -> Vec<u8> {
    msg_exited_reason(pty_id, exit_status, EXIT_REASON_NORMAL)
}

pub fn msg_exited_reason(pty_id: u16, exit_status: i32, reason: u8) -> Vec<u8> {
    let mut msg = Vec::with_capacity(8);
    msg.push(S2C_EXITED);
    msg.extend_from_slice(&pty_id.to_le_bytes());
    msg.extend_from_slice(&exit_status.to_le_bytes());
    msg.push(reason);
    msg
}

/// Arm, refresh (`ms > 0`) or clear (`ms = 0`) a terminal's deadline.
pub fn msg_deadline(pty_id: u16, ms: u32) -> Vec<u8> {
    let mut msg = Vec::with_capacity(7);
    msg.push(C2S_DEADLINE);
    msg.extend_from_slice(&pty_id.to_le_bytes());
    msg.extend_from_slice(&ms.to_le_bytes());
    msg
}

/// Build a C2S_QUIT message (client requests server shutdown).
pub fn msg_quit() -> Vec<u8> {
    vec![C2S_QUIT]
}

/// Build an S2C_QUIT message (server notifies clients of shutdown).
pub fn msg_s2c_quit() -> Vec<u8> {
    vec![S2C_QUIT]
}

pub fn msg_surface_created(
    surface_id: u16,
    parent_id: u16,
    width: u16,
    height: u16,
    title: &str,
    app_id: &str,
) -> Vec<u8> {
    let title_bytes = title.as_bytes();
    let app_id_bytes = app_id.as_bytes();
    let mut msg = Vec::with_capacity(13 + title_bytes.len() + app_id_bytes.len());
    msg.push(S2C_SURFACE_CREATED);
    msg.extend_from_slice(&surface_id.to_le_bytes());
    msg.extend_from_slice(&parent_id.to_le_bytes());
    msg.extend_from_slice(&width.to_le_bytes());
    msg.extend_from_slice(&height.to_le_bytes());
    msg.extend_from_slice(&(title_bytes.len() as u16).to_le_bytes());
    msg.extend_from_slice(title_bytes);
    msg.extend_from_slice(&(app_id_bytes.len() as u16).to_le_bytes());
    msg.extend_from_slice(app_id_bytes);
    msg
}

pub fn msg_surface_destroyed(surface_id: u16) -> Vec<u8> {
    let mut msg = Vec::with_capacity(3);
    msg.push(S2C_SURFACE_DESTROYED);
    msg.extend_from_slice(&surface_id.to_le_bytes());
    msg
}

pub fn msg_surface_frame(
    surface_id: u16,
    timestamp: u32,
    flags: u8,
    width: u16,
    height: u16,
    data: &[u8],
) -> Vec<u8> {
    let mut msg = Vec::with_capacity(12 + data.len());
    msg.push(S2C_SURFACE_FRAME);
    msg.extend_from_slice(&surface_id.to_le_bytes());
    msg.extend_from_slice(&timestamp.to_le_bytes());
    msg.push(flags);
    msg.extend_from_slice(&width.to_le_bytes());
    msg.extend_from_slice(&height.to_le_bytes());
    msg.extend_from_slice(data);
    msg
}

pub fn msg_surface_frame_precise(
    surface_id: u16,
    timestamp: u32,
    timestamp_sub_us: u16,
    flags: u8,
    width: u16,
    height: u16,
    data: &[u8],
) -> Vec<u8> {
    let mut msg = Vec::with_capacity(14 + data.len());
    msg.push(S2C_SURFACE_FRAME);
    msg.extend_from_slice(&surface_id.to_le_bytes());
    msg.extend_from_slice(&timestamp.to_le_bytes());
    msg.push(flags | SURFACE_FRAME_FLAG_TIMESTAMP_SUB_US);
    msg.extend_from_slice(&width.to_le_bytes());
    msg.extend_from_slice(&height.to_le_bytes());
    msg.extend_from_slice(&timestamp_sub_us.min(999).to_le_bytes());
    msg.extend_from_slice(data);
    msg
}

pub fn msg_surface_title(surface_id: u16, title: &str) -> Vec<u8> {
    let title_bytes = title.as_bytes();
    let mut msg = Vec::with_capacity(3 + title_bytes.len());
    msg.push(S2C_SURFACE_TITLE);
    msg.extend_from_slice(&surface_id.to_le_bytes());
    msg.extend_from_slice(title_bytes);
    msg
}

pub fn msg_surface_app_id(surface_id: u16, app_id: &str) -> Vec<u8> {
    let app_id_bytes = app_id.as_bytes();
    let mut msg = Vec::with_capacity(3 + app_id_bytes.len());
    msg.push(S2C_SURFACE_APP_ID);
    msg.extend_from_slice(&surface_id.to_le_bytes());
    msg.extend_from_slice(app_id_bytes);
    msg
}

pub fn msg_surface_activated(surface_id: u16) -> Vec<u8> {
    let mut msg = Vec::with_capacity(3);
    msg.push(S2C_SURFACE_ACTIVATED);
    msg.extend_from_slice(&surface_id.to_le_bytes());
    msg
}

/// Build S2C_SURFACE_ENCODER: `[0x2A][surface_id:2][name\0codec_string]`.
/// The codec_string is the WebCodecs codec string (e.g. "av01.2.05M.08")
/// appended after a NUL separator.  Old clients that don't split on NUL
/// will just display the full string as the encoder name, which is fine.
pub fn msg_surface_encoder(surface_id: u16, encoder_name: &str, codec_string: &str) -> Vec<u8> {
    let name_bytes = encoder_name.as_bytes();
    let codec_bytes = codec_string.as_bytes();
    let mut msg = Vec::with_capacity(3 + name_bytes.len() + 1 + codec_bytes.len());
    msg.push(S2C_SURFACE_ENCODER);
    msg.extend_from_slice(&surface_id.to_le_bytes());
    msg.extend_from_slice(name_bytes);
    msg.push(0); // NUL separator
    msg.extend_from_slice(codec_bytes);
    msg
}

/// `logical_width`/`logical_height` are the surface-logical size; pass the
/// physical size when the scale is 1x or unknown, never 0 — a zero would
/// tell a viewer the window has no size at all, where equal-to-physical is
/// the honest "no scaling in play".
pub fn msg_surface_resized(
    surface_id: u16,
    width: u16,
    height: u16,
    logical_width: u16,
    logical_height: u16,
) -> Vec<u8> {
    let mut msg = Vec::with_capacity(11);
    msg.push(S2C_SURFACE_RESIZED);
    msg.extend_from_slice(&surface_id.to_le_bytes());
    msg.extend_from_slice(&width.to_le_bytes());
    msg.extend_from_slice(&height.to_le_bytes());
    msg.extend_from_slice(&logical_width.to_le_bytes());
    msg.extend_from_slice(&logical_height.to_le_bytes());
    msg
}

pub fn msg_s2c_clipboard_content(mime_type: &str, data: &[u8]) -> Vec<u8> {
    let mime_bytes = mime_type.as_bytes();
    let mut msg = Vec::with_capacity(7 + mime_bytes.len() + data.len());
    msg.push(S2C_CLIPBOARD_CONTENT);
    msg.extend_from_slice(&(mime_bytes.len() as u16).to_le_bytes());
    msg.extend_from_slice(mime_bytes);
    msg.extend_from_slice(&(data.len() as u32).to_le_bytes());
    msg.extend_from_slice(data);
    msg
}

/// Announce whether a Wayland client owns the clipboard selection.
pub fn msg_s2c_clipboard_owner(wayland: bool) -> Vec<u8> {
    vec![S2C_CLIPBOARD_OWNER, u8::from(wayland)]
}

pub fn msg_surface_input(surface_id: u16, data: &[u8]) -> Vec<u8> {
    let mut msg = Vec::with_capacity(3 + data.len());
    msg.push(C2S_SURFACE_INPUT);
    msg.extend_from_slice(&surface_id.to_le_bytes());
    msg.extend_from_slice(data);
    msg
}

pub fn msg_surface_pointer(surface_id: u16, event_type: u8, button: u8, x: u16, y: u16) -> Vec<u8> {
    let mut msg = Vec::with_capacity(8);
    msg.push(C2S_SURFACE_POINTER);
    msg.extend_from_slice(&surface_id.to_le_bytes());
    msg.push(event_type);
    msg.push(button);
    msg.extend_from_slice(&x.to_le_bytes());
    msg.extend_from_slice(&y.to_le_bytes());
    msg
}

pub fn msg_surface_pointer_axis(surface_id: u16, axis: u8, value_x100: i32) -> Vec<u8> {
    let mut msg = Vec::with_capacity(8);
    msg.push(C2S_SURFACE_POINTER_AXIS);
    msg.extend_from_slice(&surface_id.to_le_bytes());
    msg.push(axis);
    msg.extend_from_slice(&value_x100.to_le_bytes());
    msg
}

/// A scroll event as it travels the wire and reaches the compositor.
///
/// Distances are in the composited frame's pixel space, like pointer
/// motion — the compositor converts them to surface-logical pixels on the
/// way out. `v120_*` counts detents in 120ths. `source` is `None` when the
/// sender did not classify the device, in which case no
/// `wl_pointer.axis_source` is emitted.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PointerAxisEvent {
    pub surface_id: u16,
    pub dx: f64,
    pub dy: f64,
    pub v120_x: i16,
    pub v120_y: i16,
    pub source: Option<u8>,
    pub stop: bool,
}

/// Wire size of a [`C2S_SURFACE_POINTER_AXIS2`] message.
pub const SURFACE_POINTER_AXIS2_LEN: usize = 16;

pub fn msg_surface_pointer_axis2(ev: &PointerAxisEvent) -> Vec<u8> {
    let mut flags = match ev.source {
        Some(src) => (src & 0b11) | AXIS_FLAG_SOURCE_KNOWN,
        None => 0,
    };
    if ev.stop {
        flags |= AXIS_FLAG_STOP;
    }
    let mut msg = Vec::with_capacity(SURFACE_POINTER_AXIS2_LEN);
    msg.push(C2S_SURFACE_POINTER_AXIS2);
    msg.extend_from_slice(&ev.surface_id.to_le_bytes());
    msg.push(flags);
    msg.extend_from_slice(&scroll_to_x100(ev.dx).to_le_bytes());
    msg.extend_from_slice(&scroll_to_x100(ev.dy).to_le_bytes());
    msg.extend_from_slice(&ev.v120_x.to_le_bytes());
    msg.extend_from_slice(&ev.v120_y.to_le_bytes());
    msg
}

/// Saturating conversion to the wire's hundredths, so a NaN or absurd
/// delta from a misbehaving client cannot wrap into a scroll the other
/// direction.
fn scroll_to_x100(v: f64) -> i32 {
    if v.is_nan() {
        return 0;
    }
    (v * 100.0)
        .round()
        .clamp(f64::from(i32::MIN), f64::from(i32::MAX)) as i32
}

/// Parse a [`C2S_SURFACE_POINTER_AXIS2`] payload. `data` includes the
/// opcode byte. Returns `None` if the message is truncated.
pub fn parse_surface_pointer_axis2(data: &[u8]) -> Option<PointerAxisEvent> {
    if data.len() < SURFACE_POINTER_AXIS2_LEN {
        return None;
    }
    let flags = data[3];
    Some(PointerAxisEvent {
        surface_id: u16::from_le_bytes([data[1], data[2]]),
        dx: f64::from(i32::from_le_bytes([data[4], data[5], data[6], data[7]])) / 100.0,
        dy: f64::from(i32::from_le_bytes([data[8], data[9], data[10], data[11]])) / 100.0,
        v120_x: i16::from_le_bytes([data[12], data[13]]),
        v120_y: i16::from_le_bytes([data[14], data[15]]),
        source: (flags & AXIS_FLAG_SOURCE_KNOWN != 0).then_some(flags & 0b11),
        stop: flags & AXIS_FLAG_STOP != 0,
    })
}

/// `scale_120` is the requested presentation scale in 1/120th units:
/// 60 = 0.5×, 120 = 1×, 180 = 1.5×, 240 = 2×. A value of 0 means
/// "unspecified" (server defaults to 1×). Values below 120 zoom out by
/// enlarging the logical window at Wayland's 1× output floor.
pub fn msg_surface_resize(surface_id: u16, width: u16, height: u16, scale_120: u16) -> Vec<u8> {
    let mut msg = Vec::with_capacity(9);
    msg.push(C2S_SURFACE_RESIZE);
    msg.extend_from_slice(&surface_id.to_le_bytes());
    msg.extend_from_slice(&width.to_le_bytes());
    msg.extend_from_slice(&height.to_le_bytes());
    msg.extend_from_slice(&scale_120.to_le_bytes());
    msg
}

pub fn msg_surface_focus(surface_id: u16) -> Vec<u8> {
    let mut msg = Vec::with_capacity(3);
    msg.push(C2S_SURFACE_FOCUS);
    msg.extend_from_slice(&surface_id.to_le_bytes());
    msg
}

/// Build a `C2S_SURFACE_TEXT`: [0x2F][surface_id:2][text:N].
///
/// Unlike `SURFACE_INPUT`, which carries evdev keycodes, this hands the
/// server UTF-8 and lets it choose how to deliver it — synthesised
/// US-QWERTY keys for ASCII, `zwp_text_input_v3` commit_string for
/// anything else. It is the only way to type a character the client
/// cannot map to a keycode.
pub fn msg_surface_text(surface_id: u16, text: &str) -> Vec<u8> {
    let tb = text.as_bytes();
    let mut msg = Vec::with_capacity(3 + tb.len());
    msg.push(C2S_SURFACE_TEXT);
    msg.extend_from_slice(&surface_id.to_le_bytes());
    msg.extend_from_slice(tb);
    msg
}

/// Build a `C2S_SURFACE_PREEDIT`: [0x34][surface_id:2][cursor:2][text:N].
///
/// `cursor` is a byte offset into `text`, clamped to its length — an offset
/// past the end would put the caret outside the string the app is drawing.
pub fn msg_surface_preedit(surface_id: u16, text: &str, cursor: u16) -> Vec<u8> {
    let tb = text.as_bytes();
    let cursor = cursor.min(tb.len().min(u16::MAX as usize) as u16);
    let mut msg = Vec::with_capacity(5 + tb.len());
    msg.push(C2S_SURFACE_PREEDIT);
    msg.extend_from_slice(&surface_id.to_le_bytes());
    msg.extend_from_slice(&cursor.to_le_bytes());
    msg.extend_from_slice(tb);
    msg
}

// -- Drag-and-drop codecs --

/// A [`C2S_SURFACE_DRAG_ENTER`] payload, parsed.
#[derive(Debug, Clone, PartialEq)]
pub struct SurfaceDragEnter {
    pub surface_id: u16,
    pub x: u16,
    pub y: u16,
    /// MIME types the browser can offer, advertised to the app unchanged.
    pub mimes: Vec<String>,
    /// The drop plan: one MIME type per dragged item, from the optional
    /// trailer.  During hover the browser cannot read file bytes, but it
    /// knows the item count and types, so the server pre-creates the
    /// planned staging files (see [`surface_drag_planned_name`]) and the
    /// `text/uri-list` offer becomes servable immediately — Chromium
    /// fetches it at `wl_data_device.enter`.  `None` = no plan (legacy
    /// ENTER, park-until-drop behavior).
    pub items: Option<Vec<String>>,
}

/// One item of a [`C2S_SURFACE_DRAG_DROP`] payload.
///
/// A non-empty `name` makes the item a file the client pre-uploaded into
/// the connection's drag staging dir — `name` is the path relative to the
/// staging root and `data` is empty — offered by `file://` URI; an empty
/// `name` is dragged content offered directly under `mime`.
#[derive(Debug, Clone, PartialEq)]
pub struct SurfaceDragDropItem {
    pub mime: String,
    pub name: String,
    pub data: Vec<u8>,
}

/// A [`C2S_SURFACE_DRAG_DROP`] payload, parsed.
#[derive(Debug, Clone, PartialEq)]
pub struct SurfaceDragDrop {
    pub surface_id: u16,
    pub x: u16,
    pub y: u16,
    pub items: Vec<SurfaceDragDropItem>,
}

/// Header size shared by ENTER/MOTION/DROP: opcode + surface_id + x + y.
const SURFACE_DRAG_POS_LEN: usize = 7;

/// Build a `C2S_SURFACE_DRAG_ENTER`:
/// [0x35][surface_id:2][x:2][y:2][mime_count:2][mime entries].
pub fn msg_surface_drag_enter(surface_id: u16, x: u16, y: u16, mimes: &[String]) -> Vec<u8> {
    let mut msg = Vec::with_capacity(SURFACE_DRAG_POS_LEN + 2);
    msg.push(C2S_SURFACE_DRAG_ENTER);
    msg.extend_from_slice(&surface_id.to_le_bytes());
    msg.extend_from_slice(&x.to_le_bytes());
    msg.extend_from_slice(&y.to_le_bytes());
    msg.extend_from_slice(&(mimes.len() as u16).to_le_bytes());
    for mime in mimes {
        push_str(&mut msg, mime);
    }
    msg
}

/// Build a `C2S_SURFACE_DRAG_ENTER` with the item-plan trailer:
/// [0x35][surface_id:2][x:2][y:2][mime_count:2][mime entries]
/// [item_count:2] then per item [mime_len:2][mime bytes].
///
/// `items` carries one MIME type per dragged item; the server derives each
/// item's planned staging name with [`surface_drag_planned_name`].  The
/// trailer is append-only: a reader that finds no bytes after the mime
/// list parses the message as a legacy no-plan ENTER.
pub fn msg_surface_drag_enter_with_items(
    surface_id: u16,
    x: u16,
    y: u16,
    mimes: &[String],
    items: &[String],
) -> Vec<u8> {
    let mut msg = msg_surface_drag_enter(surface_id, x, y, mimes);
    msg.extend_from_slice(&(items.len() as u16).to_le_bytes());
    for mime in items {
        push_str(&mut msg, mime);
    }
    msg
}

/// The planned staging-relative name both sides derive for drag item
/// `index` with MIME type `mime`: `{index}.{ext}` where `ext` is the
/// conventional extension for the image types the browser reports and
/// `bin` for anything else.  The browser uploads the item's real bytes to
/// this path before it sends the DROP; the server pre-creates it empty at
/// ENTER so the planned `text/uri-list` can name it immediately.
pub fn surface_drag_planned_name(index: usize, mime: &str) -> String {
    let ext = match mime {
        "image/png" => "png",
        "image/jpeg" => "jpg",
        "image/webp" => "webp",
        "image/gif" => "gif",
        "image/avif" => "avif",
        "image/heic" => "heic",
        "image/heif" => "heif",
        "image/tiff" => "tiff",
        "image/bmp" => "bmp",
        _ => "bin",
    };
    format!("{index}.{ext}")
}

/// Build a `C2S_SURFACE_DRAG_MOTION`: [0x36][surface_id:2][x:2][y:2].
pub fn msg_surface_drag_motion(surface_id: u16, x: u16, y: u16) -> Vec<u8> {
    let mut msg = Vec::with_capacity(SURFACE_DRAG_POS_LEN);
    msg.push(C2S_SURFACE_DRAG_MOTION);
    msg.extend_from_slice(&surface_id.to_le_bytes());
    msg.extend_from_slice(&x.to_le_bytes());
    msg.extend_from_slice(&y.to_le_bytes());
    msg
}

/// Build a `C2S_SURFACE_DRAG_LEAVE`: [0x37][surface_id:2].
pub fn msg_surface_drag_leave(surface_id: u16) -> Vec<u8> {
    let mut msg = Vec::with_capacity(3);
    msg.push(C2S_SURFACE_DRAG_LEAVE);
    msg.extend_from_slice(&surface_id.to_le_bytes());
    msg
}

/// Build a `C2S_SURFACE_DRAG_DROP`:
/// [0x38][surface_id:2][x:2][y:2][item_count:2][items].
pub fn msg_surface_drag_drop(
    surface_id: u16,
    x: u16,
    y: u16,
    items: &[SurfaceDragDropItem],
) -> Vec<u8> {
    let mut msg = Vec::with_capacity(SURFACE_DRAG_POS_LEN + 2);
    msg.push(C2S_SURFACE_DRAG_DROP);
    msg.extend_from_slice(&surface_id.to_le_bytes());
    msg.extend_from_slice(&x.to_le_bytes());
    msg.extend_from_slice(&y.to_le_bytes());
    msg.extend_from_slice(&(items.len() as u16).to_le_bytes());
    for item in items {
        push_str(&mut msg, &item.mime);
        push_str(&mut msg, &item.name);
        msg.extend_from_slice(&(item.data.len() as u32).to_le_bytes());
        msg.extend_from_slice(&item.data);
    }
    msg
}

/// Build a `C2S_SURFACE_DRAG_CANCEL`: [0x39]. No payload.
pub fn msg_surface_drag_cancel() -> Vec<u8> {
    vec![C2S_SURFACE_DRAG_CANCEL]
}

/// Read one `[len:2][bytes]` field as a string at `pos`, advancing it.
/// Returns `None` when the field overruns the message — a corrupt count
/// must not panic the server on its hot input path.
fn take_str(data: &[u8], pos: &mut usize) -> Option<String> {
    let len = u16::from_le_bytes([*data.get(*pos)?, *data.get(*pos + 1)?]) as usize;
    *pos += 2;
    let bytes = data.get(*pos..*pos + len)?;
    *pos += len;
    Some(String::from_utf8_lossy(bytes).into_owned())
}

/// Parse a [`C2S_SURFACE_DRAG_ENTER`] message. `data` includes the opcode
/// byte. Returns `None` if the message is truncated.
pub fn parse_surface_drag_enter(data: &[u8]) -> Option<SurfaceDragEnter> {
    if data.len() < SURFACE_DRAG_POS_LEN + 2 || data[0] != C2S_SURFACE_DRAG_ENTER {
        return None;
    }
    let (surface_id, x, y) = drag_pos(data);
    let mut pos = SURFACE_DRAG_POS_LEN;
    let count = u16::from_le_bytes([data[pos], data[pos + 1]]) as usize;
    pos += 2;
    let mut mimes = Vec::with_capacity(count.min(64));
    for _ in 0..count {
        mimes.push(take_str(data, &mut pos)?);
    }
    // The item-plan trailer is append-only: present iff bytes remain.
    let items = if pos < data.len() {
        let count = u16::from_le_bytes([*data.get(pos)?, *data.get(pos + 1)?]) as usize;
        pos += 2;
        let mut items = Vec::with_capacity(count.min(64));
        for _ in 0..count {
            items.push(take_str(data, &mut pos)?);
        }
        Some(items)
    } else {
        None
    };
    Some(SurfaceDragEnter {
        surface_id,
        x,
        y,
        mimes,
        items,
    })
}

/// Parse the `[surface_id:2][x:2][y:2]` head shared by ENTER/MOTION/DROP.
fn drag_pos(data: &[u8]) -> (u16, u16, u16) {
    (
        u16::from_le_bytes([data[1], data[2]]),
        u16::from_le_bytes([data[3], data[4]]),
        u16::from_le_bytes([data[5], data[6]]),
    )
}

/// Parse a [`C2S_SURFACE_DRAG_DROP`] message. `data` includes the opcode
/// byte. Returns `None` if the message is truncated.
pub fn parse_surface_drag_drop(data: &[u8]) -> Option<SurfaceDragDrop> {
    if data.len() < SURFACE_DRAG_POS_LEN + 2 || data[0] != C2S_SURFACE_DRAG_DROP {
        return None;
    }
    let (surface_id, x, y) = drag_pos(data);
    let mut pos = SURFACE_DRAG_POS_LEN;
    let count = u16::from_le_bytes([data[pos], data[pos + 1]]) as usize;
    pos += 2;
    let mut items = Vec::with_capacity(count.min(64));
    for _ in 0..count {
        let mime = take_str(data, &mut pos)?;
        let name = take_str(data, &mut pos)?;
        let data_len = u32::from_le_bytes([
            *data.get(pos)?,
            *data.get(pos + 1)?,
            *data.get(pos + 2)?,
            *data.get(pos + 3)?,
        ]) as usize;
        pos += 4;
        let bytes = data.get(pos..pos + data_len)?;
        pos += data_len;
        items.push(SurfaceDragDropItem {
            mime,
            name,
            data: bytes.to_vec(),
        });
    }
    Some(SurfaceDragDrop {
        surface_id,
        x,
        y,
        items,
    })
}

pub fn msg_surface_subscribe(surface_id: u16) -> Vec<u8> {
    let mut msg = Vec::with_capacity(3);
    msg.push(C2S_SURFACE_SUBSCRIBE);
    msg.extend_from_slice(&surface_id.to_le_bytes());
    msg
}

/// Extended surface subscribe with per-surface codec, bandwidth and speed
/// overrides.
///
/// `codec_support`: CODEC_SUPPORT_* bitmask (0 = use connection default).
/// `bandwidth`: SURFACE_BANDWIDTH_* constant (0 = use server default).
/// `speed`: SURFACE_SPEED_* constant (0 = use server default).
pub fn msg_surface_subscribe_ext(
    surface_id: u16,
    codec_support: u8,
    bandwidth: u8,
    speed: u8,
) -> Vec<u8> {
    let mut msg = Vec::with_capacity(6);
    msg.push(C2S_SURFACE_SUBSCRIBE);
    msg.extend_from_slice(&surface_id.to_le_bytes());
    msg.push(codec_support);
    msg.push(bandwidth);
    msg.push(speed);
    msg
}

/// Scaled surface subscribe: ask the server to encode this surface at
/// exactly `width × height` for this client, bypassing surface-size
/// mediation.  Intended for side-panel thumbnails and any viewer that
/// wants a fixed-size stream independent of the compositor's native
/// surface size and of other clients' view sizes.
///
/// `width` / `height` in pixels.  Passing `0, 0` is equivalent to
/// `msg_surface_subscribe_ext` (mediated subscription).
pub fn msg_surface_subscribe_scaled(
    surface_id: u16,
    codec_support: u8,
    bandwidth: u8,
    speed: u8,
    width: u16,
    height: u16,
) -> Vec<u8> {
    let mut msg = Vec::with_capacity(10);
    msg.push(C2S_SURFACE_SUBSCRIBE);
    msg.extend_from_slice(&surface_id.to_le_bytes());
    msg.push(codec_support);
    msg.push(bandwidth);
    msg.push(speed);
    msg.extend_from_slice(&width.to_le_bytes());
    msg.extend_from_slice(&height.to_le_bytes());
    msg
}

pub fn msg_surface_unsubscribe(surface_id: u16) -> Vec<u8> {
    let mut msg = Vec::with_capacity(3);
    msg.push(C2S_SURFACE_UNSUBSCRIBE);
    msg.extend_from_slice(&surface_id.to_le_bytes());
    msg
}

pub fn msg_surface_close(surface_id: u16) -> Vec<u8> {
    let mut msg = Vec::with_capacity(3);
    msg.push(C2S_SURFACE_CLOSE);
    msg.extend_from_slice(&surface_id.to_le_bytes());
    msg
}

/// Build a C2S_CLIPBOARD_LIST message (request available MIME types).
pub fn msg_c2s_clipboard_list() -> Vec<u8> {
    vec![C2S_CLIPBOARD_LIST]
}

/// Build a C2S_CLIPBOARD_GET message (request clipboard content for a specific MIME type).
pub fn msg_c2s_clipboard_get(mime_type: &str) -> Vec<u8> {
    let mime_bytes = mime_type.as_bytes();
    let mut msg = Vec::with_capacity(3 + mime_bytes.len());
    msg.push(C2S_CLIPBOARD_GET);
    msg.extend_from_slice(&(mime_bytes.len() as u16).to_le_bytes());
    msg.extend_from_slice(mime_bytes);
    msg
}

/// Build an S2C_CLIPBOARD_LIST message (response with available MIME types).
pub fn msg_s2c_clipboard_list(mime_types: &[String]) -> Vec<u8> {
    let count = mime_types.len().min(u16::MAX as usize);
    let mut msg = Vec::with_capacity(3 + count * 20);
    msg.push(S2C_CLIPBOARD_LIST);
    msg.extend_from_slice(&(count as u16).to_le_bytes());
    for mime in mime_types.iter().take(count) {
        let bytes = mime.as_bytes();
        msg.extend_from_slice(&(bytes.len() as u16).to_le_bytes());
        msg.extend_from_slice(bytes);
    }
    msg
}

pub fn msg_c2s_clipboard_set(mime_type: &str, data: &[u8]) -> Vec<u8> {
    msg_c2s_selection_set(C2S_CLIPBOARD_SET, mime_type, data)
}

/// Take ownership of the primary selection: see [`C2S_PRIMARY_SET`].
pub fn msg_c2s_primary_set(mime_type: &str, data: &[u8]) -> Vec<u8> {
    msg_c2s_selection_set(C2S_PRIMARY_SET, mime_type, data)
}

/// Shared framing for the two selection setters, which differ only in tag.
fn msg_c2s_selection_set(tag: u8, mime_type: &str, data: &[u8]) -> Vec<u8> {
    let mime_bytes = mime_type.as_bytes();
    let mut msg = Vec::with_capacity(7 + mime_bytes.len() + data.len());
    msg.push(tag);
    msg.extend_from_slice(&(mime_bytes.len() as u16).to_le_bytes());
    msg.extend_from_slice(mime_bytes);
    msg.extend_from_slice(&(data.len() as u32).to_le_bytes());
    msg.extend_from_slice(data);
    msg
}

/// Write an OSC 8 open (`Some`) or close (`None`) sequence, ST-terminated.
///
/// The URI is emitted verbatim except for the two bytes that would break out of
/// the sequence itself: a control byte inside the URI would terminate the OSC
/// early and let the remainder be interpreted as terminal input by whatever
/// consumes this dump.
fn push_osc8(out: &mut String, uri: Option<&str>) {
    out.push_str("\x1b]8;;");
    if let Some(uri) = uri {
        for ch in uri.chars() {
            if ch != '\x1b' && ch != '\x07' {
                out.push(ch);
            }
        }
    }
    out.push_str("\x1b\\");
}

fn push_sgr(out: &mut String, style: &CellStyle) {
    use std::fmt::Write;
    out.push_str("\x1b[0");
    if style.bold {
        out.push_str(";1");
    }
    if style.dim {
        out.push_str(";2");
    }
    if style.italic {
        out.push_str(";3");
    }
    if style.underline {
        out.push_str(";4");
    }
    if style.inverse {
        out.push_str(";7");
    }
    match style.fg {
        Color::Indexed(n) => {
            let _ = write!(out, ";38;5;{n}");
        }
        Color::Rgb(r, g, b) => {
            let _ = write!(out, ";38;2;{r};{g};{b}");
        }
        Color::Default => {}
    }
    match style.bg {
        Color::Indexed(n) => {
            let _ = write!(out, ";48;5;{n}");
        }
        Color::Rgb(r, g, b) => {
            let _ = write!(out, ";48;2;{r};{g};{b}");
        }
        Color::Default => {}
    }
    out.push('m');
}

const MODE_ALT_SCREEN: u16 = 1 << 11;

fn mode_is_cooked(mode: u16) -> bool {
    mode & MODE_ECHO != 0 && mode & MODE_ICANON != 0 && mode & MODE_ALT_SCREEN == 0
}

pub fn build_update_msg(
    pty_id: u16,
    current: &FrameState,
    previous: &FrameState,
) -> Option<Vec<u8>> {
    let same_size = previous.rows == current.rows
        && previous.cols == current.cols
        && previous.cells.len() == current.cells.len();
    // A baseline of different (or unknown — `previous` defaults to 0x0 after
    // a baseline reset) dimensions means nothing can be assumed about what
    // the client's grid holds: it keeps its cells whenever the frame's
    // dimensions match its own. Such frames are keyframes and must fully
    // determine client state — clear the grid, resend title and line flags
    // even when they match the blank baseline.
    let keyframe = !same_size;
    let title_changed = keyframe || current.title != previous.title;

    // Try scroll-aware ops when dimensions match and content differs.
    let mut ops = Vec::new();
    let mut op_count = 0u16;

    // Scroll-aware ops apply when content is "cooked" (shell output) or when
    // either frame has mode 0 (scrollback frames use mode=0, and their content
    // is always static text that benefits from COPY_RECT).
    let scroll_eligible = (mode_is_cooked(current.mode) && mode_is_cooked(previous.mode))
        || current.mode == 0
        || previous.mode == 0;
    if ENABLE_SCROLL_OPS
        && same_size
        && previous.cells != current.cells
        && scroll_eligible
        && let Some(delta_rows) = detect_vertical_scroll(current, previous)
    {
        let mut basis = previous.clone();
        encode_copy_rect_op(&mut ops, current, delta_rows);
        apply_vertical_scroll_copy(&mut basis, delta_rows);
        op_count += 1;
        append_full_width_fill_ops(current, &mut basis, &mut ops, &mut op_count);
        if let Some(patch_op) = build_patch_op(current, &basis) {
            ops.extend_from_slice(&patch_op);
            op_count += 1;
        }
    }

    // Fallback: bare PATCH_CELLS against previous, or a keyframe. A patch
    // against a blank basis only rewrites non-blank cells, so a keyframe
    // leads with a whole-grid FILL_RECT — without it, a client whose grid
    // already has content at the same dimensions would keep stale glyphs in
    // every cell the patch skips.
    if op_count == 0 {
        let blank;
        let basis = if same_size {
            previous
        } else {
            ops.push(OP_FILL_RECT);
            ops.extend_from_slice(&0u16.to_le_bytes());
            ops.extend_from_slice(&0u16.to_le_bytes());
            ops.extend_from_slice(&current.rows.to_le_bytes());
            ops.extend_from_slice(&current.cols.to_le_bytes());
            ops.extend_from_slice(&[0u8; CELL_SIZE]);
            op_count = 1;
            blank = FrameState::new(current.rows, current.cols);
            &blank
        };
        if let Some(patch_op) = build_patch_op(current, basis) {
            ops.extend_from_slice(&patch_op);
            op_count += 1;
        }
    }

    // Hyperlink identity lives outside the 12-byte cell, so retargeting a span
    // at a new URI leaves every cell byte-identical. That must still produce a
    // frame, or the client keeps following the old link.
    let links_changed = keyframe
        || current.link_uris != previous.link_uris
        || current.cell_links != previous.cell_links;

    if op_count == 0 {
        // No cell changes — still emit a frame if cursor/mode/title changed.
        //
        // The scrollback count belongs in that list: a client parked in the
        // scrollback is held on the same rows while the app prints, so its
        // frames stop changing while the history under it keeps growing.
        // Skipping those updates leaves the client's idea of how deep the
        // scrollback goes frozen at whenever it last saw a cell change —
        // and that number is what its scrollbar, its clamping, and the
        // offset it sends back are all built on.
        if !title_changed
            && !links_changed
            && current.cursor_row == previous.cursor_row
            && current.cursor_col == previous.cursor_col
            && current.mode == previous.mode
            && current.scrollback_lines == previous.scrollback_lines
        {
            return None;
        }
    }

    // Collect overflow strings that need to be transmitted.
    // We send all overflow entries from the current frame that correspond
    // to cells that changed (are in the dirty set).  For a resize (not
    // same_size), all cells are "dirty", so we send all overflow entries.
    let has_overflow = !current.overflow.is_empty();
    let overflow_section = if has_overflow {
        serialize_overflow_strings(current)
    } else {
        Vec::new()
    };

    let line_flags_changed =
        current.line_flags != previous.line_flags || current.rows != previous.rows;
    let has_line_flags =
        keyframe || (line_flags_changed && !current.line_flags.iter().all(|&f| f == 0));

    let title_bytes = if title_changed {
        current.title.as_bytes()
    } else {
        &[]
    };
    let title_len = title_bytes.len().min(TITLE_LEN_MASK as usize);
    let title_field = OPS_PRESENT
        | if has_overflow { STRINGS_PRESENT } else { 0 }
        | if has_line_flags {
            LINE_FLAGS_PRESENT
        } else {
            0
        }
        | if title_changed {
            TITLE_PRESENT | title_len as u16
        } else {
            0
        };

    let mut payload = Vec::with_capacity(
        12 + title_len
            + 2
            + ops.len()
            + overflow_section.len()
            + if has_line_flags {
                current.rows as usize
            } else {
                0
            }
            + 4,
    );
    payload.extend_from_slice(&current.rows.to_le_bytes());
    payload.extend_from_slice(&current.cols.to_le_bytes());
    payload.extend_from_slice(&current.cursor_row.to_le_bytes());
    payload.extend_from_slice(&current.cursor_col.to_le_bytes());
    payload.extend_from_slice(&current.mode.to_le_bytes());
    payload.extend_from_slice(&title_field.to_le_bytes());
    if title_changed {
        payload.extend_from_slice(&title_bytes[..title_len]);
    }
    payload.extend_from_slice(&op_count.to_le_bytes());
    payload.extend_from_slice(&ops);
    payload.extend_from_slice(&overflow_section);
    if has_line_flags {
        payload.extend_from_slice(&current.line_flags);
    }
    // Trailing scrollback count — old clients ignore extra bytes.
    payload.extend_from_slice(&current.scrollback_lines.to_le_bytes());
    // Trailing OSC 8 hyperlink section — likewise ignored by old clients, and
    // its absence reads as "no links" on new clients talking to an old server.
    append_links_section(&mut payload, current, links_changed);

    let compressed = compress_prepend_size(&payload);
    let mut msg = Vec::with_capacity(3 + compressed.len());
    msg.push(S2C_UPDATE);
    msg.extend_from_slice(&pty_id.to_le_bytes());
    msg.extend_from_slice(&compressed);
    Some(msg)
}

/// Serialize the OSC 8 hyperlink section:
///
/// ```text
/// [u16 uri_count]                                  0xFFFF = unchanged, stop
///   uri_count x [u16 link_id][u16 uri_len][uri utf8]
/// [u16 run_count]
///   run_count x [u32 start_cell][u16 run_len][u16 link_id]
/// ```
///
/// The cell->id mapping is run-length encoded because a hyperlink always spans
/// a contiguous span of cells, and it is sent in full rather than diffed: cell
/// ops (`OP_COPY_RECT` / `OP_FILL_RECT`) relocate cells, and replaying those
/// transforms against a parallel id array is a correctness trap for a section
/// that is nearly always empty. When the state is unchanged the whole section
/// costs two bytes.
/// `changed` is computed by the caller and is always true for a keyframe, so a
/// client resyncing from an unknown baseline is told the link state outright
/// instead of being asked to keep whatever it happened to be holding.
fn append_links_section(out: &mut Vec<u8>, current: &FrameState, changed: bool) {
    if !changed {
        out.extend_from_slice(&LINKS_UNCHANGED.to_le_bytes());
        return;
    }
    // Skip an over-long URI rather than clamping it: a truncated URI is a
    // *different* URI, and a cut landing mid-codepoint would additionally make
    // the receiver's behaviour depend on where the bytes happened to split.
    // `set_links` already refuses them, so this is belt-and-braces on a field
    // whose whole purpose is to say where a click goes. Runs naming a skipped
    // id are ignored by the decoder, which drops runs with an unknown id.
    let emitted: Vec<(&u16, &String)> = current
        .link_uris
        .iter()
        .filter(|(_, uri)| uri.len() <= MAX_LINK_URI)
        .take(MAX_LINK_ID as usize)
        .collect();
    let uri_count = emitted.len();
    out.extend_from_slice(&(uri_count as u16).to_le_bytes());
    for (&id, uri) in emitted {
        let bytes = uri.as_bytes();
        out.extend_from_slice(&id.to_le_bytes());
        out.extend_from_slice(&(bytes.len() as u16).to_le_bytes());
        out.extend_from_slice(bytes);
    }
    if uri_count == 0 {
        out.extend_from_slice(&0u16.to_le_bytes());
        return;
    }

    // Run-length encode the cell -> id map, skipping id 0 (unlinked) runs.
    let mut runs: Vec<(u32, u16, u16)> = Vec::new();
    let mut i = 0usize;
    while i < current.cell_links.len() {
        let id = current.cell_links[i];
        if id == 0 {
            i += 1;
            continue;
        }
        let start = i;
        while i < current.cell_links.len()
            && current.cell_links[i] == id
            && i - start < u16::MAX as usize
        {
            i += 1;
        }
        runs.push((start as u32, (i - start) as u16, id));
    }
    let run_count = runs.len().min(u16::MAX as usize);
    out.extend_from_slice(&(run_count as u16).to_le_bytes());
    for &(start, len, id) in runs.iter().take(run_count) {
        out.extend_from_slice(&start.to_le_bytes());
        out.extend_from_slice(&len.to_le_bytes());
        out.extend_from_slice(&id.to_le_bytes());
    }
}

/// Serialize overflow strings: [u16 count] [for each: u32 cell_index, u16 len, utf8 bytes]
fn serialize_overflow_strings(frame: &FrameState) -> Vec<u8> {
    let count = frame.overflow.len().min(u16::MAX as usize);
    let mut out = Vec::with_capacity(2 + count * 8);
    out.extend_from_slice(&(count as u16).to_le_bytes());
    for (&cell_idx, s) in frame.overflow.iter().take(count) {
        let bytes = s.as_bytes();
        let len = bytes.len().min(u16::MAX as usize);
        out.extend_from_slice(&(cell_idx as u32).to_le_bytes());
        out.extend_from_slice(&(len as u16).to_le_bytes());
        out.extend_from_slice(&bytes[..len]);
    }
    out
}

fn build_patch_op(current: &FrameState, previous: &FrameState) -> Option<Vec<u8>> {
    let total_cells = current.rows as usize * current.cols as usize;
    let total_bytes = total_cells * CELL_SIZE;
    // Fast path: a single bulk memcmp short-circuits the common idle case
    // where nothing has changed, avoiding both the per-cell loop and the
    // bitmask allocation.
    if current.cells.len() >= total_bytes
        && previous.cells.len() >= total_bytes
        && current.cells[..total_bytes] == previous.cells[..total_bytes]
    {
        return None;
    }
    let bitmask_len = total_cells.div_ceil(8);
    let mut bitmask = vec![0u8; bitmask_len];
    let mut dirty_count = 0usize;
    for i in 0..total_cells {
        let off = i * CELL_SIZE;
        if current.cells[off..off + CELL_SIZE] != previous.cells[off..off + CELL_SIZE] {
            bitmask[i / 8] |= 1 << (i % 8);
            dirty_count += 1;
        }
    }
    if dirty_count == 0 {
        return None;
    }

    let mut op = Vec::with_capacity(1 + bitmask_len + dirty_count * CELL_SIZE);
    op.push(OP_PATCH_CELLS);
    op.extend_from_slice(&bitmask);
    for byte_pos in 0..CELL_SIZE {
        for i in 0..total_cells {
            if bitmask[i / 8] & (1 << (i % 8)) != 0 {
                op.push(current.cells[i * CELL_SIZE + byte_pos]);
            }
        }
    }
    Some(op)
}

fn detect_vertical_scroll(current: &FrameState, previous: &FrameState) -> Option<i16> {
    let rows = current.rows as usize;
    let cols = current.cols as usize;
    if rows < 4 || cols == 0 {
        return None;
    }
    let row_bytes = cols * CELL_SIZE;
    let max_delta = rows.saturating_sub(1).min(8);
    let mut best: Option<(usize, i16)> = None;

    for delta in 1..=max_delta {
        let overlap = rows - delta;
        if overlap < 3 {
            continue;
        }
        for signed_delta in [-(delta as i16), delta as i16] {
            let mut matched = 0usize;
            for row in 0..rows {
                let src_row = row as i32 - signed_delta as i32;
                if src_row < 0 || src_row >= rows as i32 {
                    continue;
                }
                let cur_off = row * row_bytes;
                let prev_off = src_row as usize * row_bytes;
                if current.cells[cur_off..cur_off + row_bytes]
                    == previous.cells[prev_off..prev_off + row_bytes]
                {
                    matched += 1;
                }
            }
            if matched * 5 < overlap * 4 {
                continue;
            }
            let replace = match best {
                None => true,
                Some((best_matched, best_delta)) => {
                    matched > best_matched
                        || (matched == best_matched
                            && signed_delta.unsigned_abs() < best_delta.unsigned_abs())
                }
            };
            if replace {
                best = Some((matched, signed_delta));
            }
        }
    }

    best.map(|(_, delta)| delta)
}

fn encode_copy_rect_op(out: &mut Vec<u8>, current: &FrameState, delta_rows: i16) {
    let rows = current.rows;
    let cols = current.cols;
    let delta = delta_rows.unsigned_abs();
    let (src_row, dst_row, copy_rows) = if delta_rows > 0 {
        (0, delta, rows.saturating_sub(delta))
    } else {
        (delta, 0, rows.saturating_sub(delta))
    };
    out.push(OP_COPY_RECT);
    out.extend_from_slice(&src_row.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes());
    out.extend_from_slice(&dst_row.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes());
    out.extend_from_slice(&copy_rows.to_le_bytes());
    out.extend_from_slice(&cols.to_le_bytes());
}

fn apply_vertical_scroll_copy(frame: &mut FrameState, delta_rows: i16) {
    let delta = delta_rows.unsigned_abs();
    if delta == 0 || delta >= frame.rows {
        return;
    }
    let (src_row, dst_row, rows) = if delta_rows > 0 {
        (0, delta, frame.rows - delta)
    } else {
        (delta, 0, frame.rows - delta)
    };
    apply_copy_rect_frame(frame, src_row, 0, dst_row, 0, rows, frame.cols);
}

fn apply_copy_rect_frame(
    frame: &mut FrameState,
    src_row: u16,
    src_col: u16,
    dst_row: u16,
    dst_col: u16,
    rows: u16,
    cols: u16,
) {
    let rows = rows
        .min(frame.rows.saturating_sub(src_row))
        .min(frame.rows.saturating_sub(dst_row));
    let cols = cols
        .min(frame.cols.saturating_sub(src_col))
        .min(frame.cols.saturating_sub(dst_col));
    if rows == 0 || cols == 0 {
        return;
    }
    let mut temp = vec![0u8; rows as usize * cols as usize * CELL_SIZE];
    for r in 0..rows as usize {
        let src_off = frame.cell_offset(src_row + r as u16, src_col);
        let src_end = src_off + cols as usize * CELL_SIZE;
        let dst_off = r * cols as usize * CELL_SIZE;
        temp[dst_off..dst_off + cols as usize * CELL_SIZE]
            .copy_from_slice(&frame.cells[src_off..src_end]);
    }
    for r in 0..rows as usize {
        let dst_off = frame.cell_offset(dst_row + r as u16, dst_col);
        let dst_end = dst_off + cols as usize * CELL_SIZE;
        let src_off = r * cols as usize * CELL_SIZE;
        frame.cells[dst_off..dst_end]
            .copy_from_slice(&temp[src_off..src_off + cols as usize * CELL_SIZE]);
    }
}

fn append_full_width_fill_ops(
    current: &FrameState,
    basis: &mut FrameState,
    out: &mut Vec<u8>,
    op_count: &mut u16,
) {
    let rows = current.rows as usize;
    let cols = current.cols as usize;
    if rows == 0 || cols == 0 {
        return;
    }

    let row_bytes = cols * CELL_SIZE;
    let mut row = 0usize;
    while row < rows {
        let row_off = row * row_bytes;
        if current.cells[row_off..row_off + row_bytes] == basis.cells[row_off..row_off + row_bytes]
        {
            row += 1;
            continue;
        }
        let Some(cell) = uniform_row_cell(current, row) else {
            row += 1;
            continue;
        };
        let mut end = row + 1;
        while end < rows {
            if uniform_row_cell(current, end).as_ref() != Some(&cell) {
                break;
            }
            end += 1;
        }

        if *op_count == u16::MAX {
            break;
        }
        out.push(OP_FILL_RECT);
        out.extend_from_slice(&(row as u16).to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&((end - row) as u16).to_le_bytes());
        out.extend_from_slice(&current.cols.to_le_bytes());
        out.extend_from_slice(&cell);
        *op_count = op_count.saturating_add(1);

        for r in row..end {
            let row_off = basis.cell_offset(r as u16, 0);
            for c in 0..cols {
                let off = row_off + c * CELL_SIZE;
                basis.cells[off..off + CELL_SIZE].copy_from_slice(&cell);
            }
        }

        row = end;
    }
}

fn uniform_row_cell(frame: &FrameState, row: usize) -> Option<[u8; CELL_SIZE]> {
    let cols = frame.cols as usize;
    if row >= frame.rows as usize || cols == 0 {
        return None;
    }
    let start = row * cols * CELL_SIZE;
    let mut first = [0u8; CELL_SIZE];
    first.copy_from_slice(&frame.cells[start..start + CELL_SIZE]);
    if first[1] & 0b110 != 0 {
        return None;
    }
    for col in 1..cols {
        let off = start + col * CELL_SIZE;
        if frame.cells[off..off + CELL_SIZE] != first {
            return None;
        }
    }
    Some(first)
}

fn encode_cell(dst: &mut [u8], ch: Option<char>, style: CellStyle, wide: bool, wide_cont: bool) {
    dst.fill(0);

    let mut f0 = 0u8;
    encode_color(style.fg, &mut f0, &mut dst[2..5], false);
    encode_color(style.bg, &mut f0, &mut dst[5..8], true);
    if style.bold {
        f0 |= 1 << 4;
    }
    if style.dim {
        f0 |= 1 << 5;
    }
    if style.italic {
        f0 |= 1 << 6;
    }
    if style.underline {
        f0 |= 1 << 7;
    }
    dst[0] = f0;

    let mut f1 = 0u8;
    if style.inverse {
        f1 |= 1;
    }
    if wide {
        f1 |= 1 << 1;
    }
    if wide_cont {
        f1 |= 1 << 2;
    }
    if let Some(ch) = ch {
        let mut buf = [0u8; 4];
        let encoded = ch.encode_utf8(&mut buf).as_bytes();
        let len = encoded.len().min(4);
        dst[8..8 + len].copy_from_slice(&encoded[..len]);
        f1 |= (len as u8) << 3;
    }
    dst[1] = f1;
}

fn encode_color(color: Color, flags: &mut u8, dst: &mut [u8], is_bg: bool) {
    let shift = if is_bg { 2 } else { 0 };
    match color {
        Color::Default => {}
        Color::Indexed(idx) => {
            *flags |= 1 << shift;
            dst[0] = idx;
        }
        Color::Rgb(r, g, b) => {
            *flags |= 2 << shift;
            dst[0] = r;
            dst[1] = g;
            dst[2] = b;
        }
    }
}

fn wrap_text_lines(text: &str, width: usize) -> Vec<String> {
    if width == 0 {
        return Vec::new();
    }
    let mut out = Vec::new();
    for paragraph in text.split('\n') {
        if paragraph.is_empty() {
            out.push(String::new());
            continue;
        }
        let mut line = String::new();
        let mut line_width = 0usize;
        for word in paragraph.split_whitespace() {
            push_wrapped_word(word, width, &mut out, &mut line, &mut line_width);
        }
        if !line.is_empty() {
            out.push(line);
        }
    }
    if out.is_empty() {
        out.push(String::new());
    }
    out
}

fn push_wrapped_word(
    word: &str,
    width: usize,
    out: &mut Vec<String>,
    line: &mut String,
    line_width: &mut usize,
) {
    let word_width = UnicodeWidthStr::width(word);
    if line.is_empty() {
        if word_width <= width {
            line.push_str(word);
            *line_width = word_width;
            return;
        }
    } else if *line_width + 1 + word_width <= width {
        line.push(' ');
        line.push_str(word);
        *line_width += 1 + word_width;
        return;
    } else {
        out.push(std::mem::take(line));
        *line_width = 0;
        if word_width <= width {
            line.push_str(word);
            *line_width = word_width;
            return;
        }
    }

    for ch in word.chars() {
        let ch_width = UnicodeWidthChar::width(ch).unwrap_or(1).max(1);
        if *line_width + ch_width > width && !line.is_empty() {
            out.push(std::mem::take(line));
            *line_width = 0;
        }
        line.push(ch);
        *line_width += ch_width;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clipboard_owner_round_trips_and_rejects_invalid_states() {
        assert!(matches!(
            parse_server_msg(&msg_s2c_clipboard_owner(true)),
            Some(ServerMsg::ClipboardOwner { wayland: true })
        ));
        assert!(matches!(
            parse_server_msg(&msg_s2c_clipboard_owner(false)),
            Some(ServerMsg::ClipboardOwner { wayland: false })
        ));
        assert!(parse_server_msg(&[S2C_CLIPBOARD_OWNER]).is_none());
        assert!(parse_server_msg(&[S2C_CLIPBOARD_OWNER, 2]).is_none());
        assert!(parse_server_msg(&[S2C_CLIPBOARD_OWNER, 1, 0]).is_none());
    }

    #[test]
    fn pointer_axis2_round_trips() {
        let ev = PointerAxisEvent {
            surface_id: 9,
            dx: -12.34,
            dy: 56.78,
            v120_x: 0,
            v120_y: -240,
            source: Some(AXIS_SOURCE_WHEEL),
            stop: false,
        };
        let msg = msg_surface_pointer_axis2(&ev);
        assert_eq!(msg.len(), SURFACE_POINTER_AXIS2_LEN);
        assert_eq!(msg[0], C2S_SURFACE_POINTER_AXIS2);
        assert_eq!(parse_surface_pointer_axis2(&msg), Some(ev));
    }

    // Drag-and-drop codecs are pinned byte-for-byte: js/core builds these
    // messages against the same fixtures, so a drift on either side fails a
    // test rather than a user's drop.

    /// `[0x35][surface_id:2][x:2][y:2][mime_count:2][mime entries]` with
    /// `[len:2][bytes]` entries — the exact bytes for surface 7 at
    /// (100, 200) offering `text/uri-list` and `application/octet-stream`.
    #[test]
    fn drag_enter_matches_the_pinned_fixture() {
        let msg = msg_surface_drag_enter(
            7,
            100,
            200,
            &[
                "text/uri-list".to_string(),
                "application/octet-stream".to_string(),
            ],
        );
        let mut fixture = vec![
            0x35, 0x07, 0x00, 0x64, 0x00, 0xc8, 0x00, 0x02, 0x00, 0x0d, 0x00,
        ];
        fixture.extend_from_slice(b"text/uri-list");
        fixture.extend_from_slice(&[0x18, 0x00]);
        fixture.extend_from_slice(b"application/octet-stream");
        assert_eq!(msg, fixture);
        assert_eq!(
            parse_surface_drag_enter(&msg),
            Some(SurfaceDragEnter {
                surface_id: 7,
                x: 100,
                y: 200,
                mimes: vec![
                    "text/uri-list".to_string(),
                    "application/octet-stream".to_string(),
                ],
                // No trailer: a legacy ENTER parses as plan-less.
                items: None,
            })
        );
    }

    /// The ENTER with the item-plan trailer:
    /// `[0x35][surface_id:2][x:2][y:2][mime_count:2][mime entries]`
    /// `[item_count:2]` then per item `[mime_len:2][mime bytes]` — surface 7
    /// at (100, 200), mimes `["text/uri-list","application/octet-stream"]`,
    /// items `["image/png","image/jpeg"]`.
    #[test]
    fn drag_enter_with_items_matches_the_pinned_fixture() {
        let msg = msg_surface_drag_enter_with_items(
            7,
            100,
            200,
            &[
                "text/uri-list".to_string(),
                "application/octet-stream".to_string(),
            ],
            &["image/png".to_string(), "image/jpeg".to_string()],
        );
        let mut fixture = vec![
            0x35, 0x07, 0x00, 0x64, 0x00, 0xc8, 0x00, 0x02, 0x00, 0x0d, 0x00,
        ];
        fixture.extend_from_slice(b"text/uri-list");
        fixture.extend_from_slice(&[0x18, 0x00]);
        fixture.extend_from_slice(b"application/octet-stream");
        fixture.extend_from_slice(&[0x02, 0x00, 0x09, 0x00]);
        fixture.extend_from_slice(b"image/png");
        fixture.extend_from_slice(&[0x0a, 0x00]);
        fixture.extend_from_slice(b"image/jpeg");
        assert_eq!(msg, fixture);
        assert_eq!(
            parse_surface_drag_enter(&msg),
            Some(SurfaceDragEnter {
                surface_id: 7,
                x: 100,
                y: 200,
                mimes: vec![
                    "text/uri-list".to_string(),
                    "application/octet-stream".to_string(),
                ],
                items: Some(vec!["image/png".to_string(), "image/jpeg".to_string()]),
            })
        );
        // A truncated trailer is a corrupt message, not a plan-less one.
        assert!(parse_surface_drag_enter(&msg[..msg.len() - 1]).is_none());
    }

    #[test]
    fn planned_names_use_the_conventional_extension_per_type() {
        assert_eq!(surface_drag_planned_name(0, "image/png"), "0.png");
        assert_eq!(surface_drag_planned_name(1, "image/jpeg"), "1.jpg");
        assert_eq!(surface_drag_planned_name(2, "image/webp"), "2.webp");
        assert_eq!(surface_drag_planned_name(3, "image/gif"), "3.gif");
        assert_eq!(surface_drag_planned_name(4, "image/avif"), "4.avif");
        assert_eq!(surface_drag_planned_name(5, "image/heic"), "5.heic");
        assert_eq!(surface_drag_planned_name(6, "image/heif"), "6.heif");
        assert_eq!(surface_drag_planned_name(7, "image/tiff"), "7.tiff");
        assert_eq!(surface_drag_planned_name(8, "image/bmp"), "8.bmp");
        assert_eq!(surface_drag_planned_name(9, "application/pdf"), "9.bin");
        assert_eq!(surface_drag_planned_name(10, "text/plain"), "10.bin");
    }

    /// `[0x38][surface_id:2][x:2][y:2][item_count:2][items]` with
    /// `[mime_len:2][mime][name_len:2][name][data_len:4][data]` items — the
    /// exact bytes for surface 7 at (100, 200), one `image/png` item named
    /// `a b.png` with data `[0x89, 0x50]`.
    #[test]
    fn drag_drop_matches_the_pinned_fixture() {
        let msg = msg_surface_drag_drop(
            7,
            100,
            200,
            &[SurfaceDragDropItem {
                mime: "image/png".to_string(),
                name: "a b.png".to_string(),
                data: vec![0x89, 0x50],
            }],
        );
        let mut fixture = vec![
            0x38, 0x07, 0x00, 0x64, 0x00, 0xc8, 0x00, 0x01, 0x00, 0x09, 0x00,
        ];
        fixture.extend_from_slice(b"image/png");
        fixture.extend_from_slice(&[0x07, 0x00]);
        fixture.extend_from_slice(b"a b.png");
        fixture.extend_from_slice(&[0x02, 0x00, 0x00, 0x00, 0x89, 0x50]);
        assert_eq!(msg, fixture);
        assert_eq!(
            parse_surface_drag_drop(&msg),
            Some(SurfaceDragDrop {
                surface_id: 7,
                x: 100,
                y: 200,
                items: vec![SurfaceDragDropItem {
                    mime: "image/png".to_string(),
                    name: "a b.png".to_string(),
                    data: vec![0x89, 0x50],
                }],
            })
        );
    }

    #[test]
    fn drag_motion_leave_and_cancel_match_their_layouts() {
        assert_eq!(
            msg_surface_drag_motion(7, 100, 200),
            vec![0x36, 0x07, 0x00, 0x64, 0x00, 0xc8, 0x00]
        );
        assert_eq!(msg_surface_drag_leave(7), vec![0x37, 0x07, 0x00]);
        assert_eq!(msg_surface_drag_cancel(), vec![0x39]);
    }

    #[test]
    fn drag_parsers_reject_truncated_and_foreign_messages() {
        let enter = msg_surface_drag_enter(7, 100, 200, &["text/uri-list".to_string()]);
        assert!(parse_surface_drag_enter(&enter[..enter.len() - 1]).is_none());
        // A corrupt mime count must not read past the end.
        assert!(parse_surface_drag_enter(&enter[..9]).is_none());
        assert!(parse_surface_drag_enter(&msg_surface_drag_drop(7, 100, 200, &[])).is_none());

        let drop_msg = msg_surface_drag_drop(
            7,
            100,
            200,
            &[SurfaceDragDropItem {
                mime: "image/png".to_string(),
                name: "a.png".to_string(),
                data: vec![0x89, 0x50],
            }],
        );
        assert!(parse_surface_drag_drop(&drop_msg[..drop_msg.len() - 1]).is_none());
        // A corrupt item count must not read past the end.
        assert!(parse_surface_drag_drop(&drop_msg[..9]).is_none());
        assert!(parse_surface_drag_drop(&enter).is_none());

        // A name-less item is dragged content, not a file.
        let text = msg_surface_drag_drop(
            7,
            100,
            200,
            &[SurfaceDragDropItem {
                mime: "text/plain".to_string(),
                name: String::new(),
                data: b"hello".to_vec(),
            }],
        );
        let parsed = parse_surface_drag_drop(&text).unwrap();
        assert_eq!(parsed.items[0].name, "");
        assert_eq!(parsed.items[0].data, b"hello");
    }

    /// A wheel source is 0, so it only survives the round trip because the
    /// "source known" bit is separate — the bug this bit exists to prevent.
    #[test]
    fn pointer_axis2_distinguishes_wheel_from_unknown_source() {
        let mk = |source| PointerAxisEvent {
            surface_id: 1,
            dx: 0.0,
            dy: 1.0,
            v120_x: 0,
            v120_y: 0,
            source,
            stop: false,
        };
        let wheel =
            parse_surface_pointer_axis2(&msg_surface_pointer_axis2(&mk(Some(AXIS_SOURCE_WHEEL))))
                .unwrap();
        let unknown = parse_surface_pointer_axis2(&msg_surface_pointer_axis2(&mk(None))).unwrap();
        assert_eq!(wheel.source, Some(AXIS_SOURCE_WHEEL));
        assert_eq!(unknown.source, None);
    }

    #[test]
    fn pointer_axis2_carries_a_stop_with_no_deltas() {
        let ev = PointerAxisEvent {
            surface_id: 3,
            dx: 0.0,
            dy: 0.0,
            v120_x: 0,
            v120_y: 0,
            source: Some(AXIS_SOURCE_FINGER),
            stop: true,
        };
        let parsed = parse_surface_pointer_axis2(&msg_surface_pointer_axis2(&ev)).unwrap();
        assert!(parsed.stop);
        assert_eq!(parsed.source, Some(AXIS_SOURCE_FINGER));
    }

    #[test]
    fn pointer_axis2_rejects_a_truncated_message() {
        let msg = msg_surface_pointer_axis2(&PointerAxisEvent {
            surface_id: 1,
            dx: 1.0,
            dy: 1.0,
            v120_x: 0,
            v120_y: 0,
            source: None,
            stop: false,
        });
        assert!(parse_surface_pointer_axis2(&msg[..msg.len() - 1]).is_none());
    }

    /// A NaN or overflowing delta must not wrap into a scroll the other
    /// direction.
    #[test]
    fn pointer_axis2_saturates_absurd_deltas() {
        let mk = |dy| PointerAxisEvent {
            surface_id: 1,
            dx: 0.0,
            dy,
            v120_x: 0,
            v120_y: 0,
            source: None,
            stop: false,
        };
        let huge = parse_surface_pointer_axis2(&msg_surface_pointer_axis2(&mk(1e18))).unwrap();
        assert!(huge.dy > 0.0, "positive delta stayed positive");
        let nan = parse_surface_pointer_axis2(&msg_surface_pointer_axis2(&mk(f64::NAN))).unwrap();
        assert_eq!(nan.dy, 0.0);
    }

    /// `[opcode][surface_id:2][keycode:4][pressed:1]`, as the server decodes
    /// it and `buildSurfaceInputMessage` writes it.
    #[test]
    fn surface_input_puts_the_surface_id_first() {
        let mut payload = 30u32.to_le_bytes().to_vec(); // KEY_A
        payload.push(1); // pressed
        let msg = msg_surface_input(7, &payload);

        assert_eq!(msg.len(), 8);
        assert_eq!(msg[0], C2S_SURFACE_INPUT);
        assert_eq!(u16::from_le_bytes([msg[1], msg[2]]), 7);
        assert_eq!(u32::from_le_bytes([msg[3], msg[4], msg[5], msg[6]]), 30);
        assert_eq!(msg[7], 1);
    }

    #[test]
    fn hello_roundtrip_with_boot_generation() {
        let msg = msg_hello(1, 0x1234_5678, 0xfedc_ba98_7654_3210, "0.40.1");
        assert_eq!(msg.len(), 17 + 6);
        assert_eq!(&msg[7..15], &0xfedc_ba98_7654_3210_u64.to_le_bytes());
        assert!(matches!(
            parse_server_msg(&msg),
            Some(ServerMsg::Hello {
                version: 1,
                features: 0x1234_5678,
                boot_generation: Some(0xfedc_ba98_7654_3210),
                server_version: Some("0.40.1"),
            })
        ));

        // Servers that predate either trailing field still parse.
        assert!(matches!(
            parse_server_msg(&msg[..15]),
            Some(ServerMsg::Hello {
                boot_generation: Some(0xfedc_ba98_7654_3210),
                server_version: None,
                ..
            })
        ));
        assert!(matches!(
            parse_server_msg(&msg[..7]),
            Some(ServerMsg::Hello {
                boot_generation: None,
                server_version: None,
                ..
            })
        ));

        // A truncated version string is ignored rather than fatal.
        assert!(matches!(
            parse_server_msg(&msg[..20]),
            Some(ServerMsg::Hello {
                server_version: None,
                ..
            })
        ));
    }

    #[test]
    fn term_cwd_roundtrip() {
        let req = msg_term_cwd(12, 42);
        assert_eq!(parse_term_cwd(&req), Some((12, 42)));
        let reply = msg_term_cwd_reply(12, "/home/user/src/linux");
        assert_eq!(
            parse_term_cwd_reply(&reply),
            Some((12, "/home/user/src/linux".to_string()))
        );
        // Empty cwd (unavailable) round-trips too.
        assert_eq!(
            parse_term_cwd_reply(&msg_term_cwd_reply(1, "")),
            Some((1, String::new()))
        );
    }

    #[test]
    fn term_cwd_event_roundtrip() {
        let msg = msg_term_cwd_event(0x1234, "/home/user/src/linux");
        // Wire layout: [opcode][pty_id:2 LE][cwd bytes, no length prefix].
        assert_eq!(msg[0], S2C_TERM_CWD_EVENT);
        assert_eq!(&msg[1..3], &[0x34, 0x12]);
        assert_eq!(&msg[3..], b"/home/user/src/linux");
        assert_eq!(
            parse_term_cwd_event(&msg),
            Some((0x1234, "/home/user/src/linux".to_string()))
        );
        // Too short / wrong opcode.
        assert_eq!(parse_term_cwd_event(&[S2C_TERM_CWD_EVENT, 0]), None);
        assert_eq!(parse_term_cwd_event(&msg_term_cwd(1, 2)), None);
    }

    #[test]
    fn update_round_trip_preserves_title_and_cells() {
        let style = CellStyle::default();
        let mut prev = FrameState::new(2, 8);
        prev.set_title("one");
        prev.write_text(0, 0, "hello", style);

        let mut next = prev.clone();
        next.set_title("two");
        next.write_text(1, 0, "world", style);

        let baseline = build_update_msg(7, &prev, &FrameState::default()).unwrap();
        let delta = build_update_msg(7, &next, &prev).unwrap();

        let mut term = TerminalState::new(2, 8);
        let ServerMsg::Update { payload, .. } = parse_server_msg(&baseline).unwrap() else {
            panic!("expected update");
        };
        assert!(term.feed_compressed(payload));
        assert_eq!(term.title(), "one");

        let ServerMsg::Update { payload, .. } = parse_server_msg(&delta).unwrap() else {
            panic!("expected update");
        };
        assert!(term.feed_compressed(payload));
        assert_eq!(term.title(), "two");
        assert_eq!(term.get_all_text(), "hello\nworld");
    }

    /// Build a frame whose cells 0..len on row 0 all point at `uri`.
    fn frame_with_link(rows: u16, cols: u16, text: &str, uri: &str) -> FrameState {
        let mut frame = FrameState::new(rows, cols);
        frame.write_text(0, 0, text, CellStyle::default());
        let mut cell_links = vec![0u16; rows as usize * cols as usize];
        for slot in cell_links.iter_mut().take(text.len()) {
            *slot = 1;
        }
        let mut uris = BTreeMap::new();
        uris.insert(1u16, uri.to_string());
        frame.set_links(cell_links, uris);
        frame
    }

    fn feed(term: &mut TerminalState, msg: &[u8]) {
        let ServerMsg::Update { payload, .. } = parse_server_msg(msg).unwrap() else {
            panic!("expected update");
        };
        term.feed_compressed(payload);
    }

    #[test]
    fn link_segments_cover_a_single_row_span() {
        let frame = frame_with_link(2, 16, "click", "https://blit.sh");
        assert_eq!(frame.link_segments(0, 2), vec![(0, 0, 4)]);
        // Anywhere inside the span resolves to the same extent.
        assert_eq!(frame.link_segments(0, 0), frame.link_segments(0, 4));
        assert!(frame.link_segments(0, 5).is_empty());
        assert!(frame.link_segments(1, 0).is_empty());
    }

    #[test]
    fn link_segments_join_across_a_wrapped_row() {
        // A link occupying the last 3 cells of row 0 and the first 2 of row 1,
        // with row 0 marked as wrapping into row 1.
        let mut frame = FrameState::new(3, 8);
        frame.write_text(0, 0, "aaaaabbb", CellStyle::default());
        frame.write_text(1, 0, "bbcccccc", CellStyle::default());
        frame.set_wrapped(0, true);
        let mut cell_links = vec![0u16; 24];
        cell_links[5..8].fill(1); // row 0, cols 5-7
        cell_links[8..10].fill(1); // row 1, cols 0-1
        let mut uris = BTreeMap::new();
        uris.insert(1u16, "https://wrapped.example".to_string());
        frame.set_links(cell_links, uris);

        let expected = vec![(0, 5, 7), (1, 0, 1)];
        // Reachable from either half, and from either end.
        assert_eq!(frame.link_segments(0, 5), expected);
        assert_eq!(frame.link_segments(0, 7), expected);
        assert_eq!(frame.link_segments(1, 0), expected);
        assert_eq!(frame.link_segments(1, 1), expected);
    }

    #[test]
    fn link_segments_do_not_join_unwrapped_rows() {
        // Same layout, but row 0 does not wrap: two separate links that happen
        // to share a target must not merge into one highlight.
        let mut frame = FrameState::new(3, 8);
        frame.write_text(0, 0, "aaaaabbb", CellStyle::default());
        frame.write_text(1, 0, "bbcccccc", CellStyle::default());
        let mut cell_links = vec![0u16; 24];
        cell_links[5..8].fill(1);
        cell_links[8..10].fill(1);
        let mut uris = BTreeMap::new();
        uris.insert(1u16, "https://same.example".to_string());
        frame.set_links(cell_links, uris);

        assert_eq!(frame.link_segments(0, 6), vec![(0, 5, 7)]);
        assert_eq!(frame.link_segments(1, 0), vec![(1, 0, 1)]);
    }

    #[test]
    fn link_segments_span_three_rows() {
        let mut frame = FrameState::new(4, 4);
        frame.set_wrapped(0, true);
        frame.set_wrapped(1, true);
        let mut cell_links = vec![0u16; 16];
        cell_links[2..14].fill(1); // row 0 col 2 .. row 3 col 1
        let mut uris = BTreeMap::new();
        uris.insert(1u16, "https://long.example".to_string());
        frame.set_links(cell_links, uris);

        // Row 2 does not wrap, so the span stops at the end of row 2.
        assert_eq!(
            frame.link_segments(1, 1),
            vec![(0, 2, 3), (1, 0, 3), (2, 0, 3)]
        );
    }

    /// `set_links` is public, so a producer other than the PTY collector can
    /// hand it an over-long URI. It must be dropped rather than delivered
    /// truncated — a truncated URI points somewhere else.
    #[test]
    fn set_links_drops_an_overlong_uri_and_unlinks_its_cells() {
        let mut frame = FrameState::new(1, 8);
        frame.write_text(0, 0, "abcdefgh", CellStyle::default());
        let long = format!("https://e.example/{}", "a".repeat(MAX_LINK_URI));
        let mut uris = BTreeMap::new();
        uris.insert(1u16, long);
        uris.insert(2u16, "https://ok.example".to_string());
        let mut cell_links = vec![0u16; 8];
        cell_links[0..4].fill(1); // the over-long one
        cell_links[4..8].fill(2); // a fine one alongside it
        frame.set_links(cell_links, uris);

        assert_eq!(frame.cell_link(0, 0), None, "over-long link must be gone");
        assert_eq!(frame.cell_link(0, 3), None);
        assert_eq!(frame.cell_link(0, 4), Some("https://ok.example"));
        assert_eq!(frame.link_uris().len(), 1);
        // The dropped id must not linger in the cell map either.
        assert!(frame.cell_links()[0..4].iter().all(|&id| id == 0));
    }

    #[test]
    fn set_links_with_only_an_overlong_uri_clears() {
        let mut frame = FrameState::new(1, 4);
        let mut uris = BTreeMap::new();
        uris.insert(1u16, "x".repeat(MAX_LINK_URI + 1));
        frame.set_links(vec![1u16; 4], uris);
        assert!(!frame.has_links());
    }

    /// Even if a `FrameState` somehow carried one, the encoder must not put a
    /// truncated URI on the wire.
    #[test]
    fn encoder_skips_rather_than_truncates_an_overlong_uri() {
        let mut frame = FrameState::new(1, 8);
        frame.write_text(0, 0, "abcdefgh", CellStyle::default());
        // Bypass `set_links` to construct the state it would have refused.
        let long = "z".repeat(MAX_LINK_URI + 1);
        let mut uris = BTreeMap::new();
        uris.insert(1u16, long.clone());
        frame.set_links(vec![1u16; 8], uris.clone());
        frame.cell_links = vec![1u16; 8];
        frame.link_uris = uris;

        let mut payload = Vec::new();
        append_links_section(&mut payload, &frame, true);
        // uri_count == 0, run_count == 0: nothing of the URI is transmitted.
        assert_eq!(payload, vec![0, 0, 0, 0]);

        let msg = build_update_msg(1, &frame, &FrameState::default()).unwrap();
        let mut term = TerminalState::new(1, 8);
        feed(&mut term, &msg);
        assert_eq!(term.frame().cell_link(0, 0), None);
        // And no prefix of it leaked into the frame.
        assert!(
            !term
                .frame()
                .link_uris()
                .values()
                .any(|u| long.starts_with(u.as_str()))
        );
    }

    #[test]
    fn links_round_trip_over_the_wire() {
        let frame = frame_with_link(2, 16, "click", "https://blit.sh/docs");

        let mut term = TerminalState::new(2, 16);
        feed(
            &mut term,
            &build_update_msg(3, &frame, &FrameState::default()).unwrap(),
        );

        assert_eq!(term.frame().cell_link(0, 0), Some("https://blit.sh/docs"));
        assert_eq!(term.frame().cell_link(0, 4), Some("https://blit.sh/docs"));
        assert_eq!(term.frame().cell_link(0, 5), None);
        assert_eq!(term.frame().cell_link(1, 0), None);
    }

    #[test]
    fn links_can_be_cleared_and_retargeted() {
        let first = frame_with_link(1, 16, "click", "https://one.example");
        let mut term = TerminalState::new(1, 16);
        feed(
            &mut term,
            &build_update_msg(3, &first, &FrameState::default()).unwrap(),
        );
        assert_eq!(term.frame().cell_link(0, 0), Some("https://one.example"));

        // Retarget the same glyphs at a different URI. The cell bytes are
        // byte-identical, so this only survives if the link section is diffed
        // independently of the cell grid.
        let second = frame_with_link(1, 16, "click", "https://two.example");
        assert_eq!(second.cells(), first.cells());
        feed(&mut term, &build_update_msg(3, &second, &first).unwrap());
        assert_eq!(term.frame().cell_link(0, 0), Some("https://two.example"));

        // Dropping the link clears it client-side.
        let mut third = second.clone();
        third.clear_links();
        feed(&mut term, &build_update_msg(3, &third, &second).unwrap());
        assert_eq!(term.frame().cell_link(0, 0), None);
    }

    #[test]
    fn unchanged_links_cost_two_bytes_and_survive() {
        let frame = frame_with_link(1, 16, "click", "https://blit.sh");
        let mut term = TerminalState::new(1, 16);
        feed(
            &mut term,
            &build_update_msg(3, &frame, &FrameState::default()).unwrap(),
        );

        // A frame that only moves the cursor must not resend the link table,
        // but the client must still hold on to it.
        let mut moved = frame.clone();
        moved.set_cursor(0, 7);
        let delta = build_update_msg(3, &moved, &frame).unwrap();
        feed(&mut term, &delta);
        assert_eq!(term.frame().cell_link(0, 0), Some("https://blit.sh"));

        let mut payload = Vec::new();
        payload.extend_from_slice(&LINKS_UNCHANGED.to_le_bytes());
        assert_eq!(payload.len(), 2);
    }

    #[test]
    fn frame_without_links_section_reads_as_no_links() {
        // Simulates an old server: a payload that stops right after the
        // trailing scrollback count must not leave stale links behind.
        let frame = frame_with_link(1, 8, "click", "https://blit.sh");
        let mut term = TerminalState::new(1, 8);
        feed(
            &mut term,
            &build_update_msg(3, &frame, &FrameState::default()).unwrap(),
        );
        assert!(term.frame().has_links());

        let mut plain = FrameState::new(1, 8);
        plain.write_text(0, 0, "click", CellStyle::default());
        feed(
            &mut term,
            &build_update_msg(3, &plain, &FrameState::default()).unwrap(),
        );
        assert!(!term.frame().has_links());
        assert_eq!(term.frame().cell_link(0, 0), None);
    }

    #[test]
    fn malformed_link_section_clears_rather_than_half_applies() {
        let mut term = TerminalState::new(1, 8);
        // uri_count claims one entry, but the payload ends mid-header.
        term.apply_links_section(&[1, 0, 9, 0]);
        assert!(!term.frame().has_links());

        // A run naming an id absent from the table is skipped, not applied.
        let mut data = Vec::new();
        data.extend_from_slice(&1u16.to_le_bytes()); // uri_count
        data.extend_from_slice(&1u16.to_le_bytes()); // id 1
        data.extend_from_slice(&3u16.to_le_bytes()); // len
        data.extend_from_slice(b"a:b");
        data.extend_from_slice(&1u16.to_le_bytes()); // run_count
        data.extend_from_slice(&0u32.to_le_bytes()); // start
        data.extend_from_slice(&2u16.to_le_bytes()); // len
        data.extend_from_slice(&7u16.to_le_bytes()); // unknown id
        term.apply_links_section(&data);
        assert_eq!(term.frame().cell_link(0, 0), None);
    }

    #[test]
    fn link_run_past_the_grid_is_clamped() {
        let mut term = TerminalState::new(1, 4);
        let mut data = Vec::new();
        data.extend_from_slice(&1u16.to_le_bytes());
        data.extend_from_slice(&1u16.to_le_bytes());
        data.extend_from_slice(&15u16.to_le_bytes());
        data.extend_from_slice(b"https://ok.test");
        data.extend_from_slice(&1u16.to_le_bytes());
        data.extend_from_slice(&0u32.to_le_bytes());
        data.extend_from_slice(&9999u16.to_le_bytes()); // run overruns the grid
        data.extend_from_slice(&1u16.to_le_bytes());
        term.apply_links_section(&data);

        assert_eq!(term.frame().cell_link(0, 3), Some("https://ok.test"));
        assert_eq!(term.frame().cell_link(0, 4), None); // out of bounds
    }

    #[test]
    fn title_can_be_cleared_via_update() {
        let style = CellStyle::default();
        let mut prev = FrameState::new(1, 4);
        prev.set_title("busy");
        prev.write_text(0, 0, "ping", style);

        let mut next = prev.clone();
        next.set_title("");

        let baseline = build_update_msg(1, &prev, &FrameState::default()).unwrap();
        let delta = build_update_msg(1, &next, &prev).unwrap();

        let mut term = TerminalState::new(1, 4);
        let ServerMsg::Update { payload, .. } = parse_server_msg(&baseline).unwrap() else {
            panic!("expected update");
        };
        term.feed_compressed(payload);
        let ServerMsg::Update { payload, .. } = parse_server_msg(&delta).unwrap() else {
            panic!("expected update");
        };
        term.feed_compressed(payload);
        assert_eq!(term.title(), "");
    }

    #[test]
    fn scroll_heavy_update_can_use_ops_payload() {
        let style = CellStyle::default();
        let mut prev = FrameState::new(5, 6);
        prev.write_text(0, 0, "one", style);
        prev.write_text(1, 0, "two", style);
        prev.write_text(2, 0, "three", style);
        prev.write_text(3, 0, "four", style);
        prev.write_text(4, 0, "five", style);

        let mut next = FrameState::new(5, 6);
        next.write_text(0, 0, "two", style);
        next.write_text(1, 0, "three", style);
        next.write_text(2, 0, "four", style);
        next.write_text(3, 0, "five", style);

        let delta = build_update_msg(9, &next, &prev).unwrap();
        let ServerMsg::Update { payload, .. } = parse_server_msg(&delta).unwrap() else {
            panic!("expected update");
        };
        let decoded = decompress_size_prepended(payload).unwrap();
        let title_field = u16::from_le_bytes([decoded[10], decoded[11]]);
        assert_ne!(title_field & OPS_PRESENT, 0);

        let mut term = TerminalState::new(5, 6);
        let baseline = build_update_msg(9, &prev, &FrameState::default()).unwrap();
        let ServerMsg::Update { payload, .. } = parse_server_msg(&baseline).unwrap() else {
            panic!("expected update");
        };
        assert!(term.feed_compressed(payload));
        let ServerMsg::Update { payload, .. } = parse_server_msg(&delta).unwrap() else {
            panic!("expected update");
        };
        assert!(term.feed_compressed(payload));
        assert_eq!(term.get_all_text(), "two\nthree\nfour\nfive\n");
    }

    #[test]
    fn cooked_scroll_heavy_update_uses_copy_rect_op() {
        let style = CellStyle::default();
        let mut prev = FrameState::new(5, 6);
        prev.set_mode(MODE_ECHO | MODE_ICANON);
        prev.write_text(0, 0, "one", style);
        prev.write_text(1, 0, "two", style);
        prev.write_text(2, 0, "three", style);
        prev.write_text(3, 0, "four", style);
        prev.write_text(4, 0, "five", style);

        let mut next = FrameState::new(5, 6);
        next.set_mode(MODE_ECHO | MODE_ICANON);
        next.write_text(0, 0, "two", style);
        next.write_text(1, 0, "three", style);
        next.write_text(2, 0, "four", style);
        next.write_text(3, 0, "five", style);

        let delta = build_update_msg(9, &next, &prev).unwrap();
        let ServerMsg::Update { payload, .. } = parse_server_msg(&delta).unwrap() else {
            panic!("expected update");
        };
        let decoded = decompress_size_prepended(payload).unwrap();
        let op_count = u16::from_le_bytes([decoded[12], decoded[13]]);
        assert!(op_count >= 1);
        assert_eq!(decoded[14], OP_COPY_RECT);
    }

    #[test]
    fn mode_zero_scroll_uses_copy_rect() {
        let style = CellStyle::default();
        let mut prev = FrameState::new(5, 6);
        prev.write_text(0, 0, "one", style);
        prev.write_text(1, 0, "two", style);
        prev.write_text(2, 0, "three", style);
        prev.write_text(3, 0, "four", style);
        prev.write_text(4, 0, "five", style);

        let mut next = FrameState::new(5, 6);
        next.write_text(0, 0, "two", style);
        next.write_text(1, 0, "three", style);
        next.write_text(2, 0, "four", style);
        next.write_text(3, 0, "five", style);

        let delta = build_update_msg(9, &next, &prev).unwrap();
        let ServerMsg::Update { payload, .. } = parse_server_msg(&delta).unwrap() else {
            panic!("expected update");
        };
        let decoded = decompress_size_prepended(payload).unwrap();
        let op_count = u16::from_le_bytes([decoded[12], decoded[13]]);
        assert!(op_count >= 1);
        // mode=0 frames (scrollback) now use COPY_RECT for efficient scrolling
        assert_eq!(decoded[14], OP_COPY_RECT);

        // Verify round-trip correctness
        let baseline = build_update_msg(9, &prev, &FrameState::new(5, 6)).unwrap();
        let mut state = TerminalState::new(5, 6);
        let ServerMsg::Update { payload: bp, .. } = parse_server_msg(&baseline).unwrap() else {
            panic!("expected update");
        };
        state.feed_compressed(bp);
        state.feed_compressed(payload);
        assert_eq!(state.frame().cells(), next.cells());
    }

    #[test]
    fn callback_renderer_wraps_text() {
        let mut renderer = CallbackRenderer::new(2, 8);
        renderer.render(|dom| {
            dom.wrapped_text(
                Rect::new(0, 0, 2, 8),
                "alpha beta gamma",
                CellStyle::default(),
            );
        });
        assert_eq!(renderer.frame().get_all_text(), "alpha\nbeta");
    }

    #[test]
    fn scrolling_text_shows_tail() {
        let mut frame = FrameState::new(3, 8);
        frame.write_scrolling_text(
            Rect::new(0, 0, 3, 8),
            &["one", "two", "three", "four"],
            0,
            CellStyle::default(),
        );
        assert_eq!(frame.get_all_text(), "two\nthree\nfour");
    }

    #[test]
    fn search_results_round_trip_with_context() {
        let msg = [
            vec![S2C_SEARCH_RESULTS],
            7u16.to_le_bytes().to_vec(),
            1u16.to_le_bytes().to_vec(),
            42u16.to_le_bytes().to_vec(),
            1234u32.to_le_bytes().to_vec(),
            vec![1, 0b111],
            9u32.to_le_bytes().to_vec(),
            5u16.to_le_bytes().to_vec(),
            b"hello".to_vec(),
        ]
        .concat();

        let ServerMsg::SearchResults {
            request_id,
            results,
        } = parse_server_msg(&msg).unwrap()
        else {
            panic!("expected search results");
        };
        assert_eq!(request_id, 7);
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].pty_id, 42);
        assert_eq!(results[0].score, 1234);
        assert_eq!(results[0].primary_source, 1);
        assert_eq!(results[0].matched_sources, 0b111);
        assert_eq!(results[0].scroll_offset, Some(9));
        assert_eq!(results[0].context, b"hello");
    }

    // --- Tag tests ---

    #[test]
    fn msg_create_no_tag_has_zero_tag_len() {
        let msg = msg_create(24, 80);
        assert_eq!(msg.len(), 7);
        assert_eq!(msg[0], C2S_CREATE);
        assert_eq!(u16::from_le_bytes([msg[1], msg[2]]), 24);
        assert_eq!(u16::from_le_bytes([msg[3], msg[4]]), 80);
        assert_eq!(u16::from_le_bytes([msg[5], msg[6]]), 0);
    }

    #[test]
    fn msg_create_tagged_encodes_tag() {
        let msg = msg_create_tagged(24, 80, "my-pty");
        assert_eq!(msg[0], C2S_CREATE);
        let tag_len = u16::from_le_bytes([msg[5], msg[6]]) as usize;
        assert_eq!(tag_len, 6);
        assert_eq!(&msg[7..7 + tag_len], b"my-pty");
        assert_eq!(msg.len(), 7 + tag_len);
    }

    #[test]
    fn msg_create_tagged_command_encodes_both() {
        let msg = msg_create_tagged_command(30, 120, "editor", "vim");
        let tag_len = u16::from_le_bytes([msg[5], msg[6]]) as usize;
        assert_eq!(tag_len, 6);
        assert_eq!(&msg[7..13], b"editor");
        assert_eq!(&msg[13..], b"vim");
    }

    #[test]
    fn msg_create_command_has_empty_tag() {
        let msg = msg_create_command(24, 80, "ls");
        let tag_len = u16::from_le_bytes([msg[5], msg[6]]) as usize;
        assert_eq!(tag_len, 0);
        assert_eq!(&msg[7..], b"ls");
    }

    #[test]
    fn msg_create_tagged_empty_tag() {
        let msg = msg_create_tagged(24, 80, "");
        assert_eq!(msg.len(), 7);
        assert_eq!(u16::from_le_bytes([msg[5], msg[6]]), 0);
    }

    #[test]
    fn msg_create_tagged_unicode_tag() {
        let msg = msg_create_tagged(24, 80, "日本語");
        let tag_len = u16::from_le_bytes([msg[5], msg[6]]) as usize;
        assert_eq!(tag_len, "日本語".len());
        assert_eq!(std::str::from_utf8(&msg[7..7 + tag_len]).unwrap(), "日本語");
    }

    #[test]
    fn parse_created_with_tag() {
        let mut wire = vec![S2C_CREATED, 0x05, 0x00];
        wire.extend_from_slice(b"hello");
        let msg = parse_server_msg(&wire).unwrap();
        match msg {
            ServerMsg::Created { pty_id, tag } => {
                assert_eq!(pty_id, 5);
                assert_eq!(tag, "hello");
            }
            _ => panic!("expected Created"),
        }
    }

    #[test]
    fn parse_created_without_tag() {
        let wire = vec![S2C_CREATED, 0x03, 0x00];
        let msg = parse_server_msg(&wire).unwrap();
        match msg {
            ServerMsg::Created { pty_id, tag } => {
                assert_eq!(pty_id, 3);
                assert_eq!(tag, "");
            }
            _ => panic!("expected Created"),
        }
    }

    #[test]
    fn parse_created_n_with_tag() {
        let mut wire = vec![S2C_CREATED_N, 0x2a, 0x00, 0x05, 0x00];
        wire.extend_from_slice(b"hello");
        let msg = parse_server_msg(&wire).unwrap();
        match msg {
            ServerMsg::CreatedN { nonce, pty_id, tag } => {
                assert_eq!(nonce, 42);
                assert_eq!(pty_id, 5);
                assert_eq!(tag, "hello");
            }
            _ => panic!("expected CreatedN"),
        }
    }

    #[test]
    fn create_failed_roundtrip() {
        let wire = msg_create_failed(42, STATUS_BUDGET, "terminal cap reached (256)");
        assert_eq!(wire[0], S2C_CREATE_FAILED);
        match parse_server_msg(&wire).unwrap() {
            ServerMsg::CreateFailed {
                nonce,
                status,
                detail,
            } => {
                assert_eq!(nonce, 42);
                assert_eq!(status, STATUS_BUDGET);
                assert_eq!(detail, "terminal cap reached (256)");
            }
            _ => panic!("expected CreateFailed"),
        }
    }

    #[test]
    fn create_failed_accepts_empty_detail() {
        let wire = msg_create_failed(7, STATUS_OTHER, "");
        assert_eq!(wire.len(), 4);
        match parse_server_msg(&wire).unwrap() {
            ServerMsg::CreateFailed { detail, .. } => assert_eq!(detail, ""),
            _ => panic!("expected CreateFailed"),
        }
    }

    #[test]
    fn create_failed_truncates_detail_on_a_char_boundary() {
        // Splitting mid-codepoint would make `detail` undecodable and the
        // whole message useless to the client it is meant to inform.
        let detail = "é".repeat(CREATE_FAILED_DETAIL_MAX);
        let wire = msg_create_failed(1, STATUS_INVALID, &detail);
        match parse_server_msg(&wire).unwrap() {
            ServerMsg::CreateFailed { detail, .. } => {
                assert!(detail.len() <= CREATE_FAILED_DETAIL_MAX);
                assert!(detail.chars().all(|c| c == 'é'));
            }
            _ => panic!("expected CreateFailed"),
        }
    }

    #[test]
    fn status_text_distinguishes_unallocated_from_other() {
        assert_eq!(status_text(STATUS_OTHER), "backend error");
        assert_eq!(status_text(200), "unknown status");
    }

    #[test]
    fn create2_want_status_does_not_disturb_the_payload() {
        // WANT_STATUS adds no trailing field, so a flagged and an unflagged
        // create must differ in exactly one byte.
        let plain = msg_create2(3, 24, 80, "tag", "echo hi", 0);
        let flagged = msg_create2(3, 24, 80, "tag", "echo hi", CREATE2_WANT_STATUS);
        assert_eq!(plain.len(), flagged.len());
        assert_eq!(flagged[7], plain[7] | CREATE2_WANT_STATUS);
        assert_eq!(plain[8..], flagged[8..]);
    }

    #[test]
    fn create2_puts_the_deadline_before_the_command() {
        // The command has no length prefix and runs to the end of the
        // message, so a trailing deadline would be swallowed into it and the
        // command would gain four bytes of garbage.
        let msg = msg_create2_full(1, 24, 80, "tg", "echo hi", 0, Some("/tmp"), Some(5_000));
        assert_eq!(msg[7] & CREATE2_HAS_DEADLINE, CREATE2_HAS_DEADLINE);
        let tag_len = u16::from_le_bytes([msg[8], msg[9]]) as usize;
        let mut cursor = 10 + tag_len;
        assert_eq!(&msg[10..cursor], b"tg");
        let cwd_len = u16::from_le_bytes([msg[cursor], msg[cursor + 1]]) as usize;
        cursor += 2;
        assert_eq!(&msg[cursor..cursor + cwd_len], b"/tmp");
        cursor += cwd_len;
        let ms = u32::from_le_bytes([
            msg[cursor],
            msg[cursor + 1],
            msg[cursor + 2],
            msg[cursor + 3],
        ]);
        assert_eq!(ms, 5_000);
        cursor += 4;
        assert_eq!(&msg[cursor..], b"echo hi");
    }

    #[test]
    fn create2_omits_the_deadline_field_when_unarmed() {
        let armed = msg_create2_full(1, 24, 80, "", "sh", 0, None, Some(1));
        let plain = msg_create2_full(1, 24, 80, "", "sh", 0, None, None);
        assert_eq!(plain[7] & CREATE2_HAS_DEADLINE, 0);
        assert_eq!(armed.len(), plain.len() + 4);
    }

    #[test]
    fn exited_reason_roundtrips() {
        let wire = msg_exited_reason(3, -15, EXIT_REASON_DEADLINE);
        assert_eq!(wire.len(), 8);
        match parse_server_msg(&wire).unwrap() {
            ServerMsg::Exited {
                pty_id,
                exit_status,
                reason,
            } => {
                assert_eq!((pty_id, exit_status), (3, -15));
                assert_eq!(reason, EXIT_REASON_DEADLINE);
            }
            _ => panic!("expected Exited"),
        }
    }

    #[test]
    fn exited_without_a_reason_byte_reads_as_normal() {
        // What a server predating the field sends.  It must not parse as a
        // deadline kill, and it must not fail to parse.
        let legacy = vec![S2C_EXITED, 3, 0, 0, 0, 0, 0];
        match parse_server_msg(&legacy).unwrap() {
            ServerMsg::Exited { reason, .. } => assert_eq!(reason, EXIT_REASON_NORMAL),
            _ => panic!("expected Exited"),
        }
    }

    #[test]
    fn exit_reason_text_distinguishes_unallocated() {
        assert_eq!(exit_reason_text(EXIT_REASON_DEADLINE), "killed by deadline");
        assert_eq!(exit_reason_text(200), "unknown reason");
    }

    #[test]
    fn msg_create_n_format() {
        let msg = msg_create_n(42, 24, 80, "test");
        assert_eq!(msg[0], C2S_CREATE_N);
        assert_eq!(u16::from_le_bytes([msg[1], msg[2]]), 42);
        assert_eq!(u16::from_le_bytes([msg[3], msg[4]]), 24);
        assert_eq!(u16::from_le_bytes([msg[5], msg[6]]), 80);
        assert_eq!(u16::from_le_bytes([msg[7], msg[8]]), 4);
        assert_eq!(&msg[9..], b"test");
    }

    #[test]
    fn msg_create_n_command_format() {
        let msg = msg_create_n_command(7, 30, 120, "bg", "make build");
        assert_eq!(msg[0], C2S_CREATE_N);
        assert_eq!(u16::from_le_bytes([msg[1], msg[2]]), 7);
        assert_eq!(u16::from_le_bytes([msg[3], msg[4]]), 30);
        assert_eq!(u16::from_le_bytes([msg[5], msg[6]]), 120);
        let tag_len = u16::from_le_bytes([msg[7], msg[8]]) as usize;
        assert_eq!(tag_len, 2);
        assert_eq!(&msg[9..9 + tag_len], b"bg");
        assert_eq!(&msg[9 + tag_len..], b"make build");
    }

    #[test]
    fn parse_list_with_tags() {
        // 2 entries: id=1 tag="ab", id=2 tag=""
        let mut wire = vec![S2C_LIST, 0x02, 0x00];
        // entry 1: id=1, tag_len=2, tag="ab", cmd_len=0
        wire.extend_from_slice(&1u16.to_le_bytes());
        wire.extend_from_slice(&2u16.to_le_bytes());
        wire.extend_from_slice(b"ab");
        wire.extend_from_slice(&0u16.to_le_bytes());
        // entry 2: id=2, tag_len=0, cmd_len=0
        wire.extend_from_slice(&2u16.to_le_bytes());
        wire.extend_from_slice(&0u16.to_le_bytes());
        wire.extend_from_slice(&0u16.to_le_bytes());

        let msg = parse_server_msg(&wire).unwrap();
        match msg {
            ServerMsg::List { entries } => {
                assert_eq!(entries.len(), 2);
                assert_eq!(entries[0].pty_id, 1);
                assert_eq!(entries[0].tag, "ab");
                assert_eq!(entries[1].pty_id, 2);
                assert_eq!(entries[1].tag, "");
            }
            _ => panic!("expected List"),
        }
    }

    #[test]
    fn parse_list_empty() {
        let wire = vec![S2C_LIST, 0x00, 0x00];
        let msg = parse_server_msg(&wire).unwrap();
        match msg {
            ServerMsg::List { entries } => assert_eq!(entries.len(), 0),
            _ => panic!("expected List"),
        }
    }

    #[test]
    fn parse_list_truncated_gracefully() {
        // count=2 but only 1 complete entry
        let mut wire = vec![S2C_LIST, 0x02, 0x00];
        wire.extend_from_slice(&1u16.to_le_bytes());
        wire.extend_from_slice(&0u16.to_le_bytes());
        // missing second entry
        let msg = parse_server_msg(&wire).unwrap();
        match msg {
            ServerMsg::List { entries } => assert_eq!(entries.len(), 1),
            _ => panic!("expected List"),
        }
    }

    #[test]
    fn parse_list_with_long_tags() {
        let long_tag = "a".repeat(300);
        let mut wire = vec![S2C_LIST, 0x01, 0x00];
        wire.extend_from_slice(&42u16.to_le_bytes());
        wire.extend_from_slice(&(long_tag.len() as u16).to_le_bytes());
        wire.extend_from_slice(long_tag.as_bytes());

        let msg = parse_server_msg(&wire).unwrap();
        match msg {
            ServerMsg::List { entries } => {
                assert_eq!(entries.len(), 1);
                assert_eq!(entries[0].pty_id, 42);
                assert_eq!(entries[0].tag, long_tag);
            }
            _ => panic!("expected List"),
        }
    }

    #[test]
    fn create_and_created_tag_round_trip() {
        // Simulate: client sends create with tag, server echoes tag in created
        let create_msg = msg_create_tagged(24, 80, "my-session");
        let tag_len = u16::from_le_bytes([create_msg[5], create_msg[6]]) as usize;
        let tag = std::str::from_utf8(&create_msg[7..7 + tag_len]).unwrap();

        // Server builds S2C_CREATED with the tag
        let mut created_wire = vec![S2C_CREATED, 0x07, 0x00]; // pty_id = 7
        created_wire.extend_from_slice(tag.as_bytes());

        let msg = parse_server_msg(&created_wire).unwrap();
        match msg {
            ServerMsg::Created {
                pty_id,
                tag: parsed_tag,
            } => {
                assert_eq!(pty_id, 7);
                assert_eq!(parsed_tag, "my-session");
            }
            _ => panic!("expected Created"),
        }
    }

    // --- FrameState tests ---

    #[test]
    fn frame_state_accessors() {
        let mut f = FrameState::new(4, 10);
        assert_eq!(f.rows(), 4);
        assert_eq!(f.cols(), 10);
        assert_eq!(f.cursor_row(), 0);
        assert_eq!(f.cursor_col(), 0);
        assert_eq!(f.mode(), 0);
        assert_eq!(f.title(), "");
        assert_eq!(f.cells().len(), 4 * 10 * CELL_SIZE);
        assert_eq!(f.cells_mut().len(), 4 * 10 * CELL_SIZE);
        assert!(f.overflow().is_empty());
        assert!(f.overflow_mut().is_empty());
    }

    #[test]
    fn frame_state_from_parts() {
        let cells = vec![0u8; 2 * 4 * CELL_SIZE];
        let f = FrameState::from_parts(2, 4, 1, 3, 0x0F, "hello", cells.clone());
        assert_eq!(f.rows(), 2);
        assert_eq!(f.cols(), 4);
        assert_eq!(f.cursor_row(), 1);
        assert_eq!(f.cursor_col(), 3);
        assert_eq!(f.mode(), 0x0F);
        assert_eq!(f.title(), "hello");
        assert_eq!(f.cells(), &cells[..]);
    }

    #[test]
    fn frame_state_from_parts_wrong_size() {
        // cells with wrong size should be ignored (stays zeroed)
        let cells = vec![0u8; 10]; // wrong size
        let f = FrameState::from_parts(2, 4, 0, 0, 0, "", cells);
        assert_eq!(f.cells().len(), 2 * 4 * CELL_SIZE);
    }

    #[test]
    fn frame_state_resize() {
        let mut f = FrameState::new(4, 10);
        f.set_cursor(3, 9);
        f.resize(2, 5);
        assert_eq!(f.rows(), 2);
        assert_eq!(f.cols(), 5);
        assert_eq!(f.cursor_row(), 1); // clamped
        assert_eq!(f.cursor_col(), 4); // clamped
        assert_eq!(f.cells().len(), 2 * 5 * CELL_SIZE);
    }

    #[test]
    fn frame_state_resize_noop() {
        let mut f = FrameState::new(4, 10);
        let ptr_before = f.cells().as_ptr();
        f.resize(4, 10); // same size
        let ptr_after = f.cells().as_ptr();
        assert_eq!(ptr_before, ptr_after); // no realloc
    }

    #[test]
    fn frame_state_set_cursor_clamps() {
        let mut f = FrameState::new(4, 10);
        f.set_cursor(100, 200);
        assert_eq!(f.cursor_row(), 3);
        assert_eq!(f.cursor_col(), 9);
    }

    #[test]
    fn frame_state_set_title() {
        let mut f = FrameState::new(2, 2);
        assert!(f.set_title("new title"));
        assert_eq!(f.title(), "new title");
        assert!(!f.set_title("new title")); // same title returns false
        assert!(f.set_title("other"));
    }

    #[test]
    fn frame_state_get_text_and_write_text() {
        let mut f = FrameState::new(2, 10);
        f.write_text(0, 0, "Hello", CellStyle::default());
        f.write_text(1, 0, "World", CellStyle::default());
        let text = f.get_text(0, 0, 1, 9);
        assert!(text.contains("Hello"));
        assert!(text.contains("World"));
        let all = f.get_all_text();
        assert!(all.contains("Hello"));
    }

    #[test]
    fn frame_state_get_text_empty() {
        let f = FrameState::new(0, 0);
        assert_eq!(f.get_text(0, 0, 0, 0), "");
        assert_eq!(f.get_all_text(), "");
    }

    #[test]
    fn frame_state_get_cell() {
        let f = FrameState::new(2, 4);
        let cell = f.get_cell(0, 0);
        assert_eq!(cell.len(), CELL_SIZE);
        // Out of bounds
        assert!(f.get_cell(100, 100).is_empty());
    }

    #[test]
    fn frame_state_cell_content_blank() {
        let f = FrameState::new(2, 4);
        assert_eq!(f.cell_content(0, 0), " "); // blank cell
        assert_eq!(f.cell_content(100, 0), ""); // out of bounds
    }

    #[test]
    fn frame_state_cell_content_with_text() {
        let mut f = FrameState::new(2, 10);
        f.write_text(0, 0, "A", CellStyle::default());
        assert_eq!(f.cell_content(0, 0), "A");
    }

    #[test]
    fn frame_state_fill_rect() {
        let mut f = FrameState::new(4, 10);
        f.fill_rect(Rect::new(0, 0, 2, 5), 'X', CellStyle::default());
        assert_eq!(f.cell_content(0, 0), "X");
        assert_eq!(f.cell_content(1, 4), "X");
        assert_eq!(f.cell_content(2, 0), " "); // outside rect
    }

    #[test]
    fn frame_state_wrapped_text() {
        let mut f = FrameState::new(4, 10);
        let lines =
            f.write_wrapped_text(Rect::new(0, 0, 4, 5), "hello world", CellStyle::default());
        assert!(lines >= 2); // "hello world" wraps at width 5
    }

    #[test]
    fn frame_state_wrapped_text_empty_rect() {
        let mut f = FrameState::new(4, 10);
        assert_eq!(
            f.write_wrapped_text(Rect::new(0, 0, 0, 0), "hi", CellStyle::default()),
            0
        );
    }

    #[test]
    fn frame_state_scrolling_text() {
        let mut f = FrameState::new(4, 10);
        f.write_scrolling_text(
            Rect::new(0, 0, 3, 10),
            &["line1", "line2", "line3", "line4"],
            0,
            CellStyle::default(),
        );
        // Last 3 lines visible with offset_from_bottom=0
        assert_eq!(f.cell_content(0, 0), "l"); // "line2"
    }

    #[test]
    fn frame_state_scrolling_text_empty_rect() {
        let mut f = FrameState::new(4, 10);
        f.write_scrolling_text(Rect::new(0, 0, 0, 0), &["hi"], 0, CellStyle::default());
        // Should not panic
    }

    #[test]
    fn frame_state_clear() {
        let mut f = FrameState::new(2, 4);
        f.write_text(0, 0, "AB", CellStyle::default());
        f.clear(CellStyle::default());
        assert_eq!(f.cell_content(0, 0), " ");
    }

    // --- TerminalState tests ---

    #[test]
    fn terminal_state_accessors() {
        let t = TerminalState::new(24, 80);
        assert_eq!(t.rows(), 24);
        assert_eq!(t.cols(), 80);
        assert_eq!(t.cursor_row(), 0);
        assert_eq!(t.cursor_col(), 0);
        assert_eq!(t.mode(), 0);
        assert_eq!(t.title(), "");
        assert_eq!(t.cells().len(), 24 * 80 * CELL_SIZE);
        assert_eq!(t.frame().rows(), 24);
    }

    #[test]
    fn terminal_state_mutators() {
        let mut t = TerminalState::new(4, 10);
        t.frame_mut().set_title("test");
        assert_eq!(t.title(), "test");
    }

    #[test]
    fn terminal_state_set_title() {
        let mut t = TerminalState::new(4, 10);
        assert!(t.frame_mut().set_title("hello"));
        assert_eq!(t.title(), "hello");
        assert!(!t.frame_mut().set_title("hello")); // same
    }

    #[test]
    fn terminal_state_get_text() {
        let t = TerminalState::new(2, 10);
        let text = t.get_text(0, 0, 0, 9);
        assert!(text.is_empty() || text.chars().all(|c| c == ' ' || c == '\n'));
        assert!(t.get_cell(0, 0).len() == CELL_SIZE);
        assert!(t.get_cell(100, 100).is_empty());
    }

    #[test]
    fn terminal_state_resize() {
        let mut t = TerminalState::new(4, 10);
        t.frame_mut().resize(2, 5);
        // Note: TerminalState.dirty isn't updated by frame_mut().resize()
        // directly — that happens through feed_compressed. So just check frame.
        assert_eq!(t.rows(), 2);
        assert_eq!(t.cols(), 5);
    }

    #[test]
    fn terminal_state_feed_compressed_invalid() {
        let mut t = TerminalState::new(4, 10);
        assert!(!t.feed_compressed(b"garbage"));
        assert!(!t.feed_compressed(&[]));
    }

    /// The LZ4 size prefix is attacker-controlled: four bytes claiming a
    /// multi-GiB output must be refused before anything is allocated. The
    /// fs, git and lsp families each pin this; the terminal path is the one
    /// that fed a decompressor without a test holding its ceiling in place.
    #[test]
    fn terminal_state_oversized_declared_length_is_rejected() {
        let mut t = TerminalState::new(4, 10);
        let mut forged = (MAX_DECOMPRESSED as u32 + 1).to_le_bytes().to_vec();
        forged.extend_from_slice(b"whatever");
        assert!(!t.feed_compressed(&forged));

        let mut forged = u32::MAX.to_le_bytes().to_vec();
        forged.extend_from_slice(b"whatever");
        assert!(!t.feed_compressed(&forged));
    }

    #[test]
    fn terminal_state_feed_compressed_batch_empty() {
        let mut t = TerminalState::new(4, 10);
        assert!(!t.feed_compressed_batch(&[]));
    }

    #[test]
    fn terminal_state_feed_compressed_batch_truncated() {
        let mut t = TerminalState::new(4, 10);
        // Length header says 100 bytes but only 4 bytes present
        let batch = &[100, 0, 0, 0];
        assert!(!t.feed_compressed_batch(batch));
    }

    // --- Client message builder tests ---

    #[test]
    fn msg_input_format() {
        let msg = msg_input(5, b"hello");
        assert_eq!(msg[0], C2S_INPUT);
        assert_eq!(u16::from_le_bytes([msg[1], msg[2]]), 5);
        assert_eq!(&msg[3..], b"hello");
    }

    #[test]
    fn msg_resize_format() {
        let msg = msg_resize(3, 24, 80);
        assert_eq!(msg[0], C2S_RESIZE);
        assert_eq!(u16::from_le_bytes([msg[1], msg[2]]), 3);
        assert_eq!(u16::from_le_bytes([msg[3], msg[4]]), 24);
        assert_eq!(u16::from_le_bytes([msg[5], msg[6]]), 80);
    }

    #[test]
    fn msg_resize_batch_format() {
        let msg = msg_resize_batch(&[(3, 24, 80), (5, 40, 120)]);
        assert_eq!(msg[0], C2S_RESIZE);
        assert_eq!(u16::from_le_bytes([msg[1], msg[2]]), 3);
        assert_eq!(u16::from_le_bytes([msg[3], msg[4]]), 24);
        assert_eq!(u16::from_le_bytes([msg[5], msg[6]]), 80);
        assert_eq!(u16::from_le_bytes([msg[7], msg[8]]), 5);
        assert_eq!(u16::from_le_bytes([msg[9], msg[10]]), 40);
        assert_eq!(u16::from_le_bytes([msg[11], msg[12]]), 120);
    }

    #[test]
    fn msg_focus_format() {
        let msg = msg_focus(7);
        assert_eq!(msg[0], C2S_FOCUS);
        assert_eq!(u16::from_le_bytes([msg[1], msg[2]]), 7);
        assert_eq!(msg.len(), 3);
    }

    #[test]
    fn msg_close_format() {
        let msg = msg_close(9);
        assert_eq!(msg[0], C2S_CLOSE);
        assert_eq!(u16::from_le_bytes([msg[1], msg[2]]), 9);
    }

    #[test]
    fn msg_subscribe_unsubscribe_format() {
        let sub = msg_subscribe(1);
        assert_eq!(sub[0], C2S_SUBSCRIBE);
        assert_eq!(u16::from_le_bytes([sub[1], sub[2]]), 1);

        let unsub = msg_unsubscribe(2);
        assert_eq!(unsub[0], C2S_UNSUBSCRIBE);
        assert_eq!(u16::from_le_bytes([unsub[1], unsub[2]]), 2);
    }

    #[test]
    fn msg_search_format() {
        let msg = msg_search(42, "test query");
        assert_eq!(msg[0], C2S_SEARCH);
        assert_eq!(u16::from_le_bytes([msg[1], msg[2]]), 42);
        assert_eq!(&msg[3..], b"test query");
    }

    #[test]
    fn msg_ack_format() {
        let msg = msg_ack();
        assert_eq!(msg, vec![C2S_ACK]);
    }

    #[test]
    fn msg_scroll_format() {
        let msg = msg_scroll(5, 1000);
        assert_eq!(msg[0], C2S_SCROLL);
        assert_eq!(u16::from_le_bytes([msg[1], msg[2]]), 5);
        assert_eq!(u32::from_le_bytes([msg[3], msg[4], msg[5], msg[6]]), 1000);
    }

    #[test]
    fn msg_display_rate_format() {
        let msg = msg_display_rate(120);
        assert_eq!(msg[0], C2S_DISPLAY_RATE);
        assert_eq!(u16::from_le_bytes([msg[1], msg[2]]), 120);
    }

    #[test]
    fn msg_client_metrics_format() {
        let msg = msg_client_metrics(3, 5, 100);
        assert_eq!(msg[0], C2S_CLIENT_METRICS);
        assert_eq!(u16::from_le_bytes([msg[1], msg[2]]), 3);
        assert_eq!(u16::from_le_bytes([msg[3], msg[4]]), 5);
        assert_eq!(u16::from_le_bytes([msg[5], msg[6]]), 100);
    }

    // --- CallbackRenderer tests ---

    #[test]
    fn callback_renderer_resize() {
        let mut r = CallbackRenderer::new(2, 8);
        assert_eq!(r.frame().rows(), 2);
        r.resize(4, 16);
        assert_eq!(r.frame().rows(), 4);
        assert_eq!(r.frame().cols(), 16);
    }

    #[test]
    fn callback_renderer_fill() {
        let mut r = CallbackRenderer::new(4, 10);
        r.render(|dom| {
            dom.fill(Rect::new(0, 0, 2, 5), '#', CellStyle::default());
        });
        assert_eq!(r.frame().cell_content(0, 0), "#");
        assert_eq!(r.frame().cell_content(1, 4), "#");
    }

    #[test]
    fn callback_renderer_text() {
        let mut r = CallbackRenderer::new(4, 20);
        r.render(|dom| {
            dom.text(0, 0, "Hello", CellStyle::default());
        });
        assert_eq!(r.frame().cell_content(0, 0), "H");
        assert_eq!(r.frame().cell_content(0, 4), "o");
    }

    #[test]
    fn callback_renderer_set_title() {
        let mut r = CallbackRenderer::new(2, 8);
        r.render(|dom| {
            dom.set_title("my title");
        });
        assert_eq!(r.frame().title(), "my title");
    }

    #[test]
    fn callback_renderer_set_background() {
        let mut r = CallbackRenderer::new(2, 4);
        let style = CellStyle {
            bg: Color::Rgb(255, 0, 0),
            ..CellStyle::default()
        };
        r.render(|dom| {
            dom.set_background(style);
        });
        // Background fill should have been applied to all cells
        assert_eq!(r.frame().cells().len(), 2 * 4 * CELL_SIZE);
    }

    #[test]
    fn callback_renderer_scrolling_text() {
        let mut r = CallbackRenderer::new(4, 20);
        r.render(|dom| {
            dom.scrolling_text(
                Rect::new(0, 0, 3, 20),
                ["a", "b", "c", "d", "e"].map(String::from),
                0,
                CellStyle::default(),
            );
        });
        // Should show the last 3 lines
        assert_eq!(r.frame().cell_content(0, 0), "c");
    }

    // --- parse_server_msg edge cases ---

    #[test]
    fn parse_empty_returns_none() {
        assert!(parse_server_msg(&[]).is_none());
    }

    #[test]
    fn parse_unknown_type_returns_none() {
        assert!(parse_server_msg(&[0xFF, 0x00, 0x00]).is_none());
    }

    #[test]
    fn parse_update_too_short() {
        assert!(parse_server_msg(&[S2C_UPDATE, 0x00]).is_none());
    }

    #[test]
    fn parse_closed() {
        let msg = parse_server_msg(&[S2C_CLOSED, 0x05, 0x00]).unwrap();
        match msg {
            ServerMsg::Closed { pty_id } => assert_eq!(pty_id, 5),
            _ => panic!("expected Closed"),
        }
    }

    #[test]
    fn parse_title() {
        let mut wire = vec![S2C_TITLE, 0x01, 0x00];
        wire.extend_from_slice(b"mytitle");
        let msg = parse_server_msg(&wire).unwrap();
        match msg {
            ServerMsg::Title { pty_id, title } => {
                assert_eq!(pty_id, 1);
                assert_eq!(title, b"mytitle");
            }
            _ => panic!("expected Title"),
        }
    }

    // --- build_update_msg round-trip ---

    #[test]
    fn build_update_msg_round_trip_with_resize() {
        let style = CellStyle::default();
        let mut prev = FrameState::new(2, 4);
        prev.write_text(0, 0, "AB", style);

        let mut next = FrameState::new(3, 5); // different size
        next.write_text(0, 0, "XY", style);
        next.set_title("resized");

        let msg = build_update_msg(1, &next, &prev).unwrap();
        assert!(!msg.is_empty());

        // Apply to a terminal
        let mut t = TerminalState::new(2, 4);
        assert!(t.feed_compressed(&msg[3..])); // skip pty_id header
        assert_eq!(t.rows(), 3);
        assert_eq!(t.cols(), 5);
        assert_eq!(t.title(), "resized");
    }

    /// A baseline reset (fresh subscribe, PTY resize, scroll-cache miss)
    /// makes the server diff against a blank frame. That frame is a
    /// keyframe: it must repaint a client whose grid already holds content
    /// at the same dimensions, where `apply_payload` does not clear cells.
    #[test]
    fn keyframe_clears_stale_client_grid() {
        let style = CellStyle::default();

        // Client showing stale full-width content, a stale title, and a
        // stale wrapped-line flag at the same dimensions the keyframe uses.
        let mut t = TerminalState::new(2, 8);
        t.frame_mut().write_text(0, 0, "GARBAGE!", style);
        t.frame_mut().write_text(1, 0, "LEFTOVER", style);
        t.frame_mut().set_title("stale");
        t.frame_mut().line_flags[0] = ROW_FLAG_WRAPPED;

        // Current server frame: mostly blank, no title, no wrapped lines.
        let mut cur = FrameState::new(2, 8);
        cur.write_text(0, 0, "ok", style);

        let msg = build_update_msg(1, &cur, &FrameState::default()).unwrap();
        assert!(t.feed_compressed(&msg[3..]));
        assert_eq!(t.frame().cells(), cur.cells());
        assert_eq!(t.title(), "");
        assert!(!t.is_wrapped(0));
    }

    /// An all-blank current frame still emits a keyframe — suppressing it
    /// would leave a stale client grid uncorrected forever.
    #[test]
    fn keyframe_emitted_for_blank_frame() {
        let style = CellStyle::default();
        let mut t = TerminalState::new(2, 8);
        t.frame_mut().write_text(0, 0, "GARBAGE!", style);

        let cur = FrameState::new(2, 8);
        let msg = build_update_msg(1, &cur, &FrameState::default())
            .expect("blank keyframe must still be sent");
        assert!(t.feed_compressed(&msg[3..]));
        assert_eq!(t.frame().cells(), cur.cells());
    }

    /// The keyframe's leading op is a whole-grid FILL_RECT with the blank
    /// cell — an op every deployed client already implements, so no
    /// protocol version bump is needed.
    #[test]
    fn keyframe_leads_with_whole_grid_fill() {
        let style = CellStyle::default();
        let mut cur = FrameState::new(3, 5);
        cur.write_text(0, 0, "hi", style);

        let msg = build_update_msg(1, &cur, &FrameState::default()).unwrap();
        let ServerMsg::Update { payload, .. } = parse_server_msg(&msg).unwrap() else {
            panic!("expected update");
        };
        let decoded = decompress_size_prepended(payload).unwrap();
        let op_count = u16::from_le_bytes([decoded[12], decoded[13]]);
        assert_eq!(op_count, 2, "FILL_RECT + PATCH_CELLS");
        assert_eq!(decoded[14], OP_FILL_RECT);
        let row = u16::from_le_bytes([decoded[15], decoded[16]]);
        let col = u16::from_le_bytes([decoded[17], decoded[18]]);
        let rows = u16::from_le_bytes([decoded[19], decoded[20]]);
        let cols = u16::from_le_bytes([decoded[21], decoded[22]]);
        assert_eq!((row, col, rows, cols), (0, 0, 3, 5));
        assert_eq!(&decoded[23..23 + CELL_SIZE], &[0u8; CELL_SIZE]);
        assert_eq!(decoded[23 + CELL_SIZE], OP_PATCH_CELLS);
    }

    #[test]
    fn build_update_msg_cursor_change() {
        let mut prev = FrameState::new(4, 10);
        prev.set_cursor(0, 0);

        let mut next = prev.clone();
        next.set_cursor(2, 5);

        let msg = build_update_msg(0, &next, &prev).unwrap();

        let mut t = TerminalState::new(4, 10);
        assert!(t.feed_compressed(&msg[3..]));
        assert_eq!(t.cursor_row(), 2);
        assert_eq!(t.cursor_col(), 5);
    }

    /// A client held still in the scrollback sees the same rows tick after
    /// tick while the history under it grows.  If that costs no frame, its
    /// scrollback depth freezes — and the scrollbar, the clamping, and the
    /// offset it sends back are all built on that number.
    #[test]
    fn build_update_msg_reports_a_deeper_scrollback_under_still_content() {
        let mut prev = FrameState::new(2, 4);
        prev.set_scrollback_lines(120);
        let mut next = prev.clone();
        next.set_scrollback_lines(123);

        let msg = build_update_msg(0, &next, &prev).expect("depth change is a frame");
        let mut t = TerminalState::new(2, 4);
        // The bool answers "did anything visible change", which is exactly
        // what a deeper scrollback under still content does not do.
        t.feed_compressed(&msg[3..]);
        assert_eq!(t.frame.scrollback_lines(), 123);

        assert!(build_update_msg(0, &next, &next).is_none());
    }

    #[test]
    fn build_update_msg_mode_change() {
        let prev = FrameState::new(2, 4);
        let mut next = prev.clone();
        next.set_mode(0x0F);

        let msg = build_update_msg(0, &next, &prev).unwrap();
        let mut t = TerminalState::new(2, 4);
        assert!(t.feed_compressed(&msg[3..]));
        assert_eq!(t.mode(), 0x0F);
    }

    #[test]
    fn feed_compressed_batch_multiple_frames() {
        let style = CellStyle::default();
        let prev = FrameState::new(2, 4);

        let mut mid = prev.clone();
        mid.write_text(0, 0, "AB", style);
        let msg1 = build_update_msg(0, &mid, &prev).unwrap();

        let mut next = mid.clone();
        next.write_text(1, 0, "CD", style);
        let msg2 = build_update_msg(0, &next, &mid).unwrap();

        // Build batch: [len1:4][compressed1][len2:4][compressed2]
        let payload1 = &msg1[3..];
        let payload2 = &msg2[3..];
        let mut batch = Vec::new();
        batch.extend_from_slice(&(payload1.len() as u32).to_le_bytes());
        batch.extend_from_slice(payload1);
        batch.extend_from_slice(&(payload2.len() as u32).to_le_bytes());
        batch.extend_from_slice(payload2);

        let mut t = TerminalState::new(2, 4);
        assert!(t.feed_compressed_batch(&batch));
        let text = t.get_all_text();
        assert!(text.contains("AB"));
        assert!(text.contains("CD"));
    }

    /// The physical/logical split is the whole point of the message: a 1x
    /// viewer needs the logical half to know it is watching a 400x300
    /// window, not a 1200x900 one, and must not have to guess it from its
    /// own scale — which is exactly the scale the surface was *not* sized at.
    #[test]
    fn surface_resized_carries_the_logical_size() {
        let msg = msg_surface_resized(7, 1200, 900, 400, 300);
        match parse_server_msg(&msg) {
            Some(ServerMsg::SurfaceResized {
                surface_id,
                width,
                height,
                logical,
            }) => {
                assert_eq!((surface_id, width, height), (7, 1200, 900));
                assert_eq!(logical, Some((400, 300)));
            }
            other => panic!("expected SurfaceResized, got {}", other.is_some()),
        }
    }

    /// A server that predates the field stops at 7 bytes.  Absent must stay
    /// distinguishable from 0x0 — a viewer that read a missing logical size
    /// as an empty window would draw nothing at all.
    #[test]
    fn surface_resized_without_a_logical_size_reports_absent_not_zero() {
        let mut legacy = msg_surface_resized(7, 1200, 900, 400, 300);
        legacy.truncate(7);
        match parse_server_msg(&legacy) {
            Some(ServerMsg::SurfaceResized {
                width,
                height,
                logical,
                ..
            }) => {
                assert_eq!((width, height), (1200, 900));
                assert_eq!(logical, None);
            }
            other => panic!("expected SurfaceResized, got {}", other.is_some()),
        }
    }

    #[test]
    fn precise_surface_timestamp_uses_one_flag_bit_and_a_u16_field() {
        let msg = msg_surface_frame_precise(7, 123, 987, SURFACE_FRAME_CODEC_AV1, 8, 9, &[4, 5]);
        match parse_server_msg(&msg) {
            Some(ServerMsg::SurfaceFrame {
                timestamp_sub_us,
                flags,
                data,
                ..
            }) => {
                assert_eq!(timestamp_sub_us, Some(987));
                assert_eq!(flags & SURFACE_FRAME_FLAG_TIMESTAMP_SUB_US, 1 << 3);
                assert_eq!(data, &[4, 5]);
            }
            _ => panic!("expected surface frame"),
        }
    }
}
