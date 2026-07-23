//! Language intelligence wire protocol (docs/design/lsp.md).
//!
//! The server terminates LSP and projects it into blit-native records:
//! per-backend phase/capabilities are *pushed* as whole-snapshot
//! `LSP_STATE` messages ([`LspStateMirror`]), diagnostics are *pushed* as
//! per-file replacement sets against a server-held cache
//! ([`LspDiagMirror`]), and point-in-time answers are *pulled* through the
//! single nonce-correlated `LSP_QUERY` opcode whose `kind` byte selects
//! the operation.
//!
//! Positions are 0-based lines with UTF-8 byte columns in both
//! directions; the server transcodes to each backend's negotiated
//! encoding. All integers little-endian, tightly packed, as everywhere in
//! the protocol.

use std::collections::BTreeMap;

/// `S2C_HELLO` feature bit: server supports the `LSP_*` message family.
pub const FEATURE_LSP: u32 = 1 << 8;

// C2S opcodes.

/// Attach to the workspace containing a path: [0x60][nonce:2][flags:1][diag_latency_ms:2][path_len:2][path:N]
/// `path` is plain UTF-8 (client-chosen filesystem location, like
/// `FS_SYNC`); the server walks upward for root markers.
pub const C2S_LSP_OPEN: u8 = 0x60;
/// Release an attachment (backends stay warm): [0x61][lsp_id:2]
pub const C2S_LSP_CLOSE: u8 = 0x61;
/// Acknowledge a pushed update: [0x62][lsp_id:2][stream:1][update_id:4]
/// `stream` is [`LSP_STREAM_STATE`] or [`LSP_STREAM_DIAG`].
pub const C2S_LSP_ACK: u8 = 0x62;
/// Point-in-time query: [0x63][nonce:2][lsp_id:2][kind:1][flags:1][line:4][col:4][path_len:2][path:N][arg_len:2][arg:N]
/// `kind` is one of `LSP_QUERY_*`; `line`/`col` are ignored by the symbol
/// kinds (for `WS_SYMBOLS` the `line` field is reserved as a future
/// SymbolKind bitmask filter); `arg` carries the `WS_SYMBOLS` query
/// string or the `RENAME` new name.
pub const C2S_LSP_QUERY: u8 = 0x63;
/// Advisory cancel of an in-flight query: [0x64][nonce:2]
pub const C2S_LSP_CANCEL: u8 = 0x64;
/// Enumerate every live backend, daemon-wide: [0x65][nonce:2]
pub const C2S_LSP_SERVERS: u8 = 0x65;
/// Shut one backend down by `server_ref`: [0x66][nonce:2][server_ref:2]
/// A later query respawns it; observability before force.
pub const C2S_LSP_STOP: u8 = 0x66;

// S2C opcodes.

/// Open outcome: [0x60][nonce:2][lsp_id:2][status:1][flags:1][root_len:2][root:N][detail_len:2][detail:N]
/// On failure `lsp_id` = [`LSP_ID_INVALID`] and `detail` carries a
/// diagnostic; on success `root` is the canonical workspace root,
/// escaped.
pub const S2C_LSP_OPENED: u8 = 0x60;
/// Whole-state snapshot: [0x61][lsp_id:2][state_id:4][flags:1][records:LZ4]
/// One `SERVER` record per live backend of the attachment.
pub const S2C_LSP_STATE: u8 = 0x61;
/// Diagnostics update: [0x62][lsp_id:2][update_id:4][flags:1][records:LZ4]
/// Per-file replacement sets; bit 0 [`LSP_DIAG_FULL`] carries the
/// complete workspace state (drop everything, then apply).
pub const S2C_LSP_DIAG: u8 = 0x62;
/// Query response: [0x63][nonce:2][status:1][flags:1][detail_len:2][detail:N][records:LZ4]
/// `detail` is a human-readable failure reason (empty on success).
pub const S2C_LSP_QUERY: u8 = 0x63;
/// Attachment ended server-side: [0x64][lsp_id:2][reason:1]
pub const S2C_LSP_CLOSED: u8 = 0x64;
/// Backend enumeration: [0x65][nonce:2][status:1][flags:1][records:LZ4]
/// `SERVER` records as in `LSP_STATE` plus the escaped root.
pub const S2C_LSP_SERVERS: u8 = 0x65;
/// Stop outcome: [0x66][nonce:2][status:1]
pub const S2C_LSP_STOPPED: u8 = 0x66;

// Unified status table (docs/design/lsp.md "Statuses"): the git.md codes
// 0-9 with the same numbers and semantics where they overlap, plus
// WARMING.
pub const LSP_STATUS_OK: u8 = 0;
/// `lsp_id` unknown or already closed.
pub const LSP_STATUS_UNKNOWN_ID: u8 = 1;
/// Path, symbol, or backend does not exist; discovery failures name the
/// missing binary in the detail field.
pub const LSP_STATUS_NOT_FOUND: u8 = 2;
/// The element cannot answer this query (e.g. rename on a non-symbol).
pub const LSP_STATUS_WRONG_TYPE: u8 = 3;
pub const LSP_STATUS_PERMISSION: u8 = 4;
/// Over a size cap; truncation flags cover the paginatable cases.
pub const LSP_STATUS_TOO_LARGE: u8 = 5;
/// A budget was exhausted with no way to truncate.
pub const LSP_STATUS_BUDGET: u8 = 6;
/// Malformed request (unknown flags, kind, or field combination).
pub const LSP_STATUS_INVALID: u8 = 7;
/// Ended by `LSP_CANCEL`.
pub const LSP_STATUS_CANCELLED: u8 = 8;
/// Diagnostic in the message's detail field where it has one.
pub const LSP_STATUS_OTHER: u8 = 9;
/// The backing server has not finished initialize/indexing; retryable.
pub const LSP_STATUS_WARMING: u8 = 10;

/// Human-readable name for an `LSP_STATUS_*` code.
pub fn lsp_status_text(status: u8) -> &'static str {
    match status {
        LSP_STATUS_OK => "ok",
        LSP_STATUS_UNKNOWN_ID => "unknown attachment",
        LSP_STATUS_NOT_FOUND => "not found",
        LSP_STATUS_WRONG_TYPE => "wrong type",
        LSP_STATUS_PERMISSION => "permission denied",
        LSP_STATUS_TOO_LARGE => "too large",
        LSP_STATUS_BUDGET => "budget exhausted",
        LSP_STATUS_INVALID => "invalid request",
        LSP_STATUS_CANCELLED => "cancelled",
        LSP_STATUS_WARMING => "warming up",
        _ => "error",
    }
}

// C2S_LSP_OPEN flags.

/// Stream `LSP_STATE`.
pub const LSP_OPEN_WATCH: u8 = 1 << 0;
/// Stream `LSP_DIAG`; implies `WATCH`.
pub const LSP_OPEN_DIAGS: u8 = 1 << 1;

/// `lsp_id` value reporting an open failure.
pub const LSP_ID_INVALID: u16 = 0xFFFF;

// C2S_LSP_ACK streams.

pub const LSP_STREAM_STATE: u8 = 0;
pub const LSP_STREAM_DIAG: u8 = 1;

// C2S_LSP_QUERY kinds.

/// → `LOCATION` records.
pub const LSP_QUERY_DEFINITION: u8 = 1;
/// → `LOCATION` records; flags bit 0 [`LSP_REFS_INCLUDE_DECLARATION`].
pub const LSP_QUERY_REFERENCES: u8 = 2;
/// → one `MARKUP` record (plus an optional `LOCATION` for the range).
pub const LSP_QUERY_HOVER: u8 = 3;
/// → `SYMBOL` records, pre-order; `line`/`col` ignored.
pub const LSP_QUERY_DOC_SYMBOLS: u8 = 4;
/// → `SYMBOL` records; `path` empty, `arg` = query string.
pub const LSP_QUERY_WS_SYMBOLS: u8 = 5;
/// → `EDIT` records; `arg` = new name. Data, never applied.
pub const LSP_QUERY_RENAME: u8 = 6;

// C2S_LSP_QUERY flags.

/// `REFERENCES`: include the declaration itself.
pub const LSP_REFS_INCLUDE_DECLARATION: u8 = 1 << 0;

// S2C_LSP_DIAG flags.

/// The update carries complete workspace diagnostic state: drop
/// everything, then apply. Every `DIAGS` subscribe begins with one (the
/// cache replay), and the server may send one at any time instead of an
/// incremental update.
pub const LSP_DIAG_FULL: u8 = 1 << 0;

// S2C response flags (query, state, diag, servers).

/// The entries budget was hit; records present are valid.
pub const LSP_RESP_TRUNCATED: u8 = 1 << 0;
/// A `RENAME` plan dropped file operations it cannot project (create /
/// rename / delete of whole files in a `WorkspaceEdit`): the returned
/// `EDIT` records are the text edits only, so the plan is incomplete.
pub const LSP_RESP_INCOMPLETE: u8 = 1 << 1;

// S2C_LSP_CLOSED reasons.

pub const LSP_CLOSED_CLIENT_REQUEST: u8 = 0;
pub const LSP_CLOSED_ROOT_GONE: u8 = 1;
pub const LSP_CLOSED_PERMISSION_LOST: u8 = 2;
pub const LSP_CLOSED_BACKEND_FAILED: u8 = 3;
pub const LSP_CLOSED_RESOURCE_LIMIT: u8 = 4;

// SERVER record phases.

pub const LSP_PHASE_SPAWNING: u8 = 0;
pub const LSP_PHASE_INITIALIZING: u8 = 1;
pub const LSP_PHASE_INDEXING: u8 = 2;
pub const LSP_PHASE_READY: u8 = 3;
pub const LSP_PHASE_FAILED: u8 = 4;

/// `progress_pct` value when the backend reports no percentage.
pub const LSP_PROGRESS_UNKNOWN: u8 = 255;

// SERVER record capability bits (`caps:4`), aligned with query kinds:
// bit `kind - 1`.

pub const LSP_CAP_DEFINITION: u32 = 1 << 0;
pub const LSP_CAP_REFERENCES: u32 = 1 << 1;
pub const LSP_CAP_HOVER: u32 = 1 << 2;
pub const LSP_CAP_DOC_SYMBOLS: u32 = 1 << 3;
pub const LSP_CAP_WS_SYMBOLS: u32 = 1 << 4;
pub const LSP_CAP_RENAME: u32 = 1 << 5;

// DIAG record severities (LSP values).

pub const LSP_SEVERITY_ERROR: u8 = 1;
pub const LSP_SEVERITY_WARNING: u8 = 2;
pub const LSP_SEVERITY_INFO: u8 = 3;
pub const LSP_SEVERITY_HINT: u8 = 4;

// DIAG record flags (LSP diagnostic tags).

pub const LSP_DIAG_UNNECESSARY: u8 = 1 << 0;
pub const LSP_DIAG_DEPRECATED: u8 = 1 << 1;

// MARKUP record formats.

pub const LSP_MARKUP_PLAIN: u8 = 0;
pub const LSP_MARKUP_MARKDOWN: u8 = 1;

// SYMBOL record flags.

pub const LSP_SYMBOL_DEPRECATED: u8 = 1 << 0;

// Record kinds, namespaced per message type.

pub const LSP_STATE_RECORD_SERVER: u8 = 0x01;
pub const LSP_DIAG_RECORD_FILE: u8 = 0x01;
pub const LSP_DIAG_RECORD_DIAG: u8 = 0x02;
pub const LSP_QUERY_RECORD_LOCATION: u8 = 0x01;
pub const LSP_QUERY_RECORD_MARKUP: u8 = 0x02;
pub const LSP_QUERY_RECORD_SYMBOL: u8 = 0x03;
pub const LSP_QUERY_RECORD_EDIT: u8 = 0x04;

/// BLAKE3 truncated to 128 bits, as in the fs family: the content
/// version a record describes.
pub type LspHash = [u8; 16];

/// The all-zero hash: content version unknown.
pub const LSP_HASH_NONE: LspHash = [0; 16];

/// Decompress a `compress_prepend_size` payload, refusing declared sizes
/// over the protocol-wide [`crate::MAX_DECOMPRESSED`] *before* allocating
/// (docs/protocol.md "Compressed payloads").
fn decompress_guarded(data: &[u8]) -> Option<Vec<u8>> {
    if data.len() < 4 {
        return None;
    }
    let declared = u32::from_le_bytes(data[0..4].try_into().unwrap()) as usize;
    if declared > crate::MAX_DECOMPRESSED {
        return None;
    }
    lz4_flex::decompress_size_prepended(data).ok()
}

// ---------------------------------------------------------------------------
// Field codec helpers
// ---------------------------------------------------------------------------

fn push_str(buf: &mut Vec<u8>, s: &str) {
    let b = s.as_bytes();
    buf.extend_from_slice(&(b.len() as u16).to_le_bytes());
    buf.extend_from_slice(b);
}

/// A u32-length-prefixed byte string (diagnostic messages, markup, edit
/// text).
fn push_bytes(buf: &mut Vec<u8>, b: &[u8]) {
    buf.extend_from_slice(&(b.len() as u32).to_le_bytes());
    buf.extend_from_slice(b);
}

fn take_u8(b: &mut &[u8]) -> Option<u8> {
    let (&x, rest) = b.split_first()?;
    *b = rest;
    Some(x)
}

fn take_u16(b: &mut &[u8]) -> Option<u16> {
    if b.len() < 2 {
        return None;
    }
    let v = u16::from_le_bytes([b[0], b[1]]);
    *b = &b[2..];
    Some(v)
}

fn take_u32(b: &mut &[u8]) -> Option<u32> {
    if b.len() < 4 {
        return None;
    }
    let v = u32::from_le_bytes(b[0..4].try_into().unwrap());
    *b = &b[4..];
    Some(v)
}

fn take_u64(b: &mut &[u8]) -> Option<u64> {
    if b.len() < 8 {
        return None;
    }
    let v = u64::from_le_bytes(b[0..8].try_into().unwrap());
    *b = &b[8..];
    Some(v)
}

fn take_hash(b: &mut &[u8]) -> Option<LspHash> {
    if b.len() < 16 {
        return None;
    }
    let hash: LspHash = b[0..16].try_into().unwrap();
    *b = &b[16..];
    Some(hash)
}

fn take_str<'a>(b: &mut &'a [u8]) -> Option<&'a str> {
    let len = take_u16(b)? as usize;
    if b.len() < len {
        return None;
    }
    let s = std::str::from_utf8(&b[..len]).ok()?;
    *b = &b[len..];
    Some(s)
}

fn take_text<'a>(b: &mut &'a [u8]) -> Option<&'a str> {
    let len = take_u32(b)? as usize;
    if b.len() < len {
        return None;
    }
    let s = std::str::from_utf8(&b[..len]).ok()?;
    *b = &b[len..];
    Some(s)
}

/// Check `msg` starts with `opcode` and return the body after it.
fn body_of(msg: &[u8], opcode: u8) -> Option<&[u8]> {
    if msg.first() != Some(&opcode) {
        return None;
    }
    Some(&msg[1..])
}

// Record framing: [record_len:4][kind:1][…], unknown kinds skippable,
// malformed records end the payload.

fn begin_record(buf: &mut Vec<u8>) -> usize {
    let start = buf.len();
    buf.extend_from_slice(&0u32.to_le_bytes());
    start
}

fn end_record(buf: &mut [u8], start: usize) {
    let len = (buf.len() - start - 4) as u32;
    buf[start..start + 4].copy_from_slice(&len.to_le_bytes());
}

/// Pop the next record: `(kind, body)`. `None` ends iteration (clean end
/// or malformed framing).
fn next_record<'a>(data: &mut &'a [u8]) -> Option<(u8, &'a [u8])> {
    let mut b = *data;
    let len = take_u32(&mut b)? as usize;
    if b.len() < len || len == 0 {
        return None;
    }
    let record = &b[..len];
    *data = &b[len..];
    Some((record[0], &record[1..]))
}

// ---------------------------------------------------------------------------
// C2S message builders and parsers
// ---------------------------------------------------------------------------

pub fn msg_lsp_open(nonce: u16, flags: u8, diag_latency_ms: u16, path: &str) -> Vec<u8> {
    let mut msg = Vec::with_capacity(8 + path.len());
    msg.push(C2S_LSP_OPEN);
    msg.extend_from_slice(&nonce.to_le_bytes());
    msg.push(flags);
    msg.extend_from_slice(&diag_latency_ms.to_le_bytes());
    push_str(&mut msg, path);
    msg
}

/// Parse `C2S_LSP_OPEN` into `(nonce, flags, diag_latency_ms, path)`.
pub fn parse_lsp_open(msg: &[u8]) -> Option<(u16, u8, u16, &str)> {
    let mut b = body_of(msg, C2S_LSP_OPEN)?;
    let nonce = take_u16(&mut b)?;
    let flags = take_u8(&mut b)?;
    let diag_latency_ms = take_u16(&mut b)?;
    let path = take_str(&mut b)?;
    Some((nonce, flags, diag_latency_ms, path))
}

pub fn msg_lsp_close(lsp_id: u16) -> Vec<u8> {
    let mut msg = Vec::with_capacity(3);
    msg.push(C2S_LSP_CLOSE);
    msg.extend_from_slice(&lsp_id.to_le_bytes());
    msg
}

pub fn parse_lsp_close(msg: &[u8]) -> Option<u16> {
    let mut b = body_of(msg, C2S_LSP_CLOSE)?;
    take_u16(&mut b)
}

pub fn msg_lsp_ack(lsp_id: u16, stream: u8, update_id: u32) -> Vec<u8> {
    let mut msg = Vec::with_capacity(8);
    msg.push(C2S_LSP_ACK);
    msg.extend_from_slice(&lsp_id.to_le_bytes());
    msg.push(stream);
    msg.extend_from_slice(&update_id.to_le_bytes());
    msg
}

/// Parse `C2S_LSP_ACK` into `(lsp_id, stream, update_id)`.
pub fn parse_lsp_ack(msg: &[u8]) -> Option<(u16, u8, u32)> {
    let mut b = body_of(msg, C2S_LSP_ACK)?;
    let lsp_id = take_u16(&mut b)?;
    let stream = take_u8(&mut b)?;
    let update_id = take_u32(&mut b)?;
    Some((lsp_id, stream, update_id))
}

/// A decoded `C2S_LSP_QUERY` request.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LspQueryRequest<'a> {
    pub nonce: u16,
    pub lsp_id: u16,
    /// One of `LSP_QUERY_*`.
    pub kind: u8,
    pub flags: u8,
    /// 0-based; ignored by the symbol kinds.
    pub line: u32,
    /// UTF-8 byte offset within the line; ignored by the symbol kinds.
    pub col: u32,
    /// Escaped form; empty for `WS_SYMBOLS`.
    pub path: &'a str,
    /// `WS_SYMBOLS` query string or `RENAME` new name; empty otherwise.
    pub arg: &'a str,
}

pub fn msg_lsp_query(req: &LspQueryRequest<'_>) -> Vec<u8> {
    let mut msg = Vec::with_capacity(19 + req.path.len() + req.arg.len());
    msg.push(C2S_LSP_QUERY);
    msg.extend_from_slice(&req.nonce.to_le_bytes());
    msg.extend_from_slice(&req.lsp_id.to_le_bytes());
    msg.push(req.kind);
    msg.push(req.flags);
    msg.extend_from_slice(&req.line.to_le_bytes());
    msg.extend_from_slice(&req.col.to_le_bytes());
    push_str(&mut msg, req.path);
    push_str(&mut msg, req.arg);
    msg
}

pub fn parse_lsp_query(msg: &[u8]) -> Option<LspQueryRequest<'_>> {
    let mut b = body_of(msg, C2S_LSP_QUERY)?;
    let nonce = take_u16(&mut b)?;
    let lsp_id = take_u16(&mut b)?;
    let kind = take_u8(&mut b)?;
    let flags = take_u8(&mut b)?;
    let line = take_u32(&mut b)?;
    let col = take_u32(&mut b)?;
    let path = take_str(&mut b)?;
    let arg = take_str(&mut b)?;
    Some(LspQueryRequest {
        nonce,
        lsp_id,
        kind,
        flags,
        line,
        col,
        path,
        arg,
    })
}

pub fn msg_lsp_cancel(nonce: u16) -> Vec<u8> {
    let mut msg = Vec::with_capacity(3);
    msg.push(C2S_LSP_CANCEL);
    msg.extend_from_slice(&nonce.to_le_bytes());
    msg
}

pub fn parse_lsp_cancel(msg: &[u8]) -> Option<u16> {
    let mut b = body_of(msg, C2S_LSP_CANCEL)?;
    take_u16(&mut b)
}

pub fn msg_lsp_servers(nonce: u16) -> Vec<u8> {
    let mut msg = Vec::with_capacity(3);
    msg.push(C2S_LSP_SERVERS);
    msg.extend_from_slice(&nonce.to_le_bytes());
    msg
}

pub fn parse_lsp_servers(msg: &[u8]) -> Option<u16> {
    let mut b = body_of(msg, C2S_LSP_SERVERS)?;
    take_u16(&mut b)
}

pub fn msg_lsp_stop(nonce: u16, server_ref: u16) -> Vec<u8> {
    let mut msg = Vec::with_capacity(5);
    msg.push(C2S_LSP_STOP);
    msg.extend_from_slice(&nonce.to_le_bytes());
    msg.extend_from_slice(&server_ref.to_le_bytes());
    msg
}

/// Parse `C2S_LSP_STOP` into `(nonce, server_ref)`.
pub fn parse_lsp_stop(msg: &[u8]) -> Option<(u16, u16)> {
    let mut b = body_of(msg, C2S_LSP_STOP)?;
    let nonce = take_u16(&mut b)?;
    let server_ref = take_u16(&mut b)?;
    Some((nonce, server_ref))
}

// ---------------------------------------------------------------------------
// S2C message builders and parsers
// ---------------------------------------------------------------------------

pub fn msg_lsp_opened(
    nonce: u16,
    lsp_id: u16,
    status: u8,
    flags: u8,
    root: &str,
    detail: &str,
) -> Vec<u8> {
    let mut msg = Vec::with_capacity(11 + root.len() + detail.len());
    msg.push(S2C_LSP_OPENED);
    msg.extend_from_slice(&nonce.to_le_bytes());
    msg.extend_from_slice(&lsp_id.to_le_bytes());
    msg.push(status);
    msg.push(flags);
    push_str(&mut msg, root);
    push_str(&mut msg, detail);
    msg
}

/// A decoded `S2C_LSP_OPENED`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LspOpened<'a> {
    pub nonce: u16,
    pub lsp_id: u16,
    pub status: u8,
    pub flags: u8,
    /// Escaped canonical workspace root; empty on failure.
    pub root: &'a str,
    /// Diagnostic on failure.
    pub detail: &'a str,
}

pub fn parse_lsp_opened(msg: &[u8]) -> Option<LspOpened<'_>> {
    let mut b = body_of(msg, S2C_LSP_OPENED)?;
    let nonce = take_u16(&mut b)?;
    let lsp_id = take_u16(&mut b)?;
    let status = take_u8(&mut b)?;
    let flags = take_u8(&mut b)?;
    let root = take_str(&mut b)?;
    let detail = take_str(&mut b)?;
    Some(LspOpened {
        nonce,
        lsp_id,
        status,
        flags,
        root,
        detail,
    })
}

pub fn msg_lsp_state(lsp_id: u16, state_id: u32, flags: u8, records: &[u8]) -> Vec<u8> {
    let compressed = lz4_flex::compress_prepend_size(records);
    let mut msg = Vec::with_capacity(8 + compressed.len());
    msg.push(S2C_LSP_STATE);
    msg.extend_from_slice(&lsp_id.to_le_bytes());
    msg.extend_from_slice(&state_id.to_le_bytes());
    msg.push(flags);
    msg.extend_from_slice(&compressed);
    msg
}

/// Parse `S2C_LSP_STATE` into `(lsp_id, state_id, flags, records)` with
/// the records decompressed.
pub fn parse_lsp_state(msg: &[u8]) -> Option<(u16, u32, u8, Vec<u8>)> {
    let mut b = body_of(msg, S2C_LSP_STATE)?;
    let lsp_id = take_u16(&mut b)?;
    let state_id = take_u32(&mut b)?;
    let flags = take_u8(&mut b)?;
    let records = decompress_guarded(b)?;
    Some((lsp_id, state_id, flags, records))
}

pub fn msg_lsp_diag(lsp_id: u16, update_id: u32, flags: u8, records: &[u8]) -> Vec<u8> {
    let compressed = lz4_flex::compress_prepend_size(records);
    let mut msg = Vec::with_capacity(8 + compressed.len());
    msg.push(S2C_LSP_DIAG);
    msg.extend_from_slice(&lsp_id.to_le_bytes());
    msg.extend_from_slice(&update_id.to_le_bytes());
    msg.push(flags);
    msg.extend_from_slice(&compressed);
    msg
}

/// Parse `S2C_LSP_DIAG` into `(lsp_id, update_id, flags, records)` with
/// the records decompressed.
pub fn parse_lsp_diag(msg: &[u8]) -> Option<(u16, u32, u8, Vec<u8>)> {
    let mut b = body_of(msg, S2C_LSP_DIAG)?;
    let lsp_id = take_u16(&mut b)?;
    let update_id = take_u32(&mut b)?;
    let flags = take_u8(&mut b)?;
    let records = decompress_guarded(b)?;
    Some((lsp_id, update_id, flags, records))
}

pub fn msg_lsp_query_resp(
    nonce: u16,
    status: u8,
    flags: u8,
    detail: &str,
    records: &[u8],
) -> Vec<u8> {
    let compressed = lz4_flex::compress_prepend_size(records);
    let mut msg = Vec::with_capacity(7 + detail.len() + compressed.len());
    msg.push(S2C_LSP_QUERY);
    msg.extend_from_slice(&nonce.to_le_bytes());
    msg.push(status);
    msg.push(flags);
    push_str(&mut msg, detail);
    msg.extend_from_slice(&compressed);
    msg
}

/// A decoded `S2C_LSP_QUERY` response.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LspQueryResp {
    pub nonce: u16,
    pub status: u8,
    pub flags: u8,
    /// Human-readable failure reason (e.g. the upstream server's own
    /// error message on `OTHER`); empty on success.
    pub detail: String,
    /// Decompressed response records.
    pub records: Vec<u8>,
}

/// Parse `S2C_LSP_QUERY` with the records decompressed.
pub fn parse_lsp_query_resp(msg: &[u8]) -> Option<LspQueryResp> {
    let mut b = body_of(msg, S2C_LSP_QUERY)?;
    let nonce = take_u16(&mut b)?;
    let status = take_u8(&mut b)?;
    let flags = take_u8(&mut b)?;
    let detail = take_str(&mut b)?.to_string();
    let records = decompress_guarded(b)?;
    Some(LspQueryResp {
        nonce,
        status,
        flags,
        detail,
        records,
    })
}

pub fn msg_lsp_closed(lsp_id: u16, reason: u8) -> Vec<u8> {
    let mut msg = Vec::with_capacity(4);
    msg.push(S2C_LSP_CLOSED);
    msg.extend_from_slice(&lsp_id.to_le_bytes());
    msg.push(reason);
    msg
}

/// Parse `S2C_LSP_CLOSED` into `(lsp_id, reason)`.
pub fn parse_lsp_closed(msg: &[u8]) -> Option<(u16, u8)> {
    let mut b = body_of(msg, S2C_LSP_CLOSED)?;
    let lsp_id = take_u16(&mut b)?;
    let reason = take_u8(&mut b)?;
    Some((lsp_id, reason))
}

pub fn msg_lsp_servers_resp(nonce: u16, status: u8, flags: u8, records: &[u8]) -> Vec<u8> {
    let compressed = lz4_flex::compress_prepend_size(records);
    let mut msg = Vec::with_capacity(5 + compressed.len());
    msg.push(S2C_LSP_SERVERS);
    msg.extend_from_slice(&nonce.to_le_bytes());
    msg.push(status);
    msg.push(flags);
    msg.extend_from_slice(&compressed);
    msg
}

/// Parse `S2C_LSP_SERVERS` into `(nonce, status, flags, records)` with
/// the records decompressed.
pub fn parse_lsp_servers_resp(msg: &[u8]) -> Option<(u16, u8, u8, Vec<u8>)> {
    let mut b = body_of(msg, S2C_LSP_SERVERS)?;
    let nonce = take_u16(&mut b)?;
    let status = take_u8(&mut b)?;
    let flags = take_u8(&mut b)?;
    let records = decompress_guarded(b)?;
    Some((nonce, status, flags, records))
}

pub fn msg_lsp_stopped(nonce: u16, status: u8) -> Vec<u8> {
    let mut msg = Vec::with_capacity(4);
    msg.push(S2C_LSP_STOPPED);
    msg.extend_from_slice(&nonce.to_le_bytes());
    msg.push(status);
    msg
}

/// Parse `S2C_LSP_STOPPED` into `(nonce, status)`.
pub fn parse_lsp_stopped(msg: &[u8]) -> Option<(u16, u8)> {
    let mut b = body_of(msg, S2C_LSP_STOPPED)?;
    let nonce = take_u16(&mut b)?;
    let status = take_u8(&mut b)?;
    Some((nonce, status))
}

// ---------------------------------------------------------------------------
// LSP_STATE records
// ---------------------------------------------------------------------------

/// One decoded record from an `LSP_STATE` payload.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LspStateRecord<'a> {
    /// SERVER 0x01: [kind:1][server_ref:2][phase:1][progress_pct:1][caps:4]
    /// [epoch:4][refused_edits:4][rss:8][id_len:2][id:N][msg_len:2][msg:N]
    Server {
        /// Daemon-scoped backend id (`LSP_STOP` target).
        server_ref: u16,
        /// One of `LSP_PHASE_*`.
        phase: u8,
        /// 0-100, or [`LSP_PROGRESS_UNKNOWN`].
        progress_pct: u8,
        /// `LSP_CAP_*` bits.
        caps: u32,
        /// Increments on dynamic capability (re)registration.
        epoch: u32,
        /// `workspace/applyEdit` requests answered `applied:false`.
        refused_edits: u32,
        /// Best-effort resident set size in bytes; 0 = unknown.
        rss: u64,
        /// Server id from the discovery table (e.g. `rust-analyzer`).
        id: &'a str,
        /// Last progress or showMessage line.
        msg: &'a str,
    },
}

/// Append one record to an uncompressed `LSP_STATE` records buffer.
pub fn append_lsp_state_record(buf: &mut Vec<u8>, record: &LspStateRecord<'_>) {
    let start = begin_record(buf);
    match record {
        LspStateRecord::Server {
            server_ref,
            phase,
            progress_pct,
            caps,
            epoch,
            refused_edits,
            rss,
            id,
            msg,
        } => {
            buf.push(LSP_STATE_RECORD_SERVER);
            buf.extend_from_slice(&server_ref.to_le_bytes());
            buf.push(*phase);
            buf.push(*progress_pct);
            buf.extend_from_slice(&caps.to_le_bytes());
            buf.extend_from_slice(&epoch.to_le_bytes());
            buf.extend_from_slice(&refused_edits.to_le_bytes());
            buf.extend_from_slice(&rss.to_le_bytes());
            push_str(buf, id);
            push_str(buf, msg);
        }
    }
    end_record(buf, start);
}

pub struct LspStateRecordIter<'a> {
    data: &'a [u8],
}

/// Iterate records in an uncompressed `LSP_STATE` payload.
pub fn lsp_state_records(data: &[u8]) -> LspStateRecordIter<'_> {
    LspStateRecordIter { data }
}

impl<'a> Iterator for LspStateRecordIter<'a> {
    type Item = LspStateRecord<'a>;

    fn next(&mut self) -> Option<LspStateRecord<'a>> {
        loop {
            let (kind, mut b) = next_record(&mut self.data)?;
            match kind {
                LSP_STATE_RECORD_SERVER => {
                    let server_ref = take_u16(&mut b)?;
                    let phase = take_u8(&mut b)?;
                    let progress_pct = take_u8(&mut b)?;
                    let caps = take_u32(&mut b)?;
                    let epoch = take_u32(&mut b)?;
                    let refused_edits = take_u32(&mut b)?;
                    let rss = take_u64(&mut b)?;
                    let id = take_str(&mut b)?;
                    let msg = take_str(&mut b)?;
                    return Some(LspStateRecord::Server {
                        server_ref,
                        phase,
                        progress_pct,
                        caps,
                        epoch,
                        refused_edits,
                        rss,
                        id,
                        msg,
                    });
                }
                _ => continue, // unknown kind: skip via record_len
            }
        }
    }
}

// ---------------------------------------------------------------------------
// LSP_SERVERS records
// ---------------------------------------------------------------------------

/// One decoded record from an `LSP_SERVERS` payload: the `LSP_STATE`
/// `SERVER` layout plus the escaped workspace root.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LspServersRecord<'a> {
    /// SERVER 0x01: the `LSP_STATE` layout + [root_len:2][root:N]
    Server {
        server_ref: u16,
        phase: u8,
        progress_pct: u8,
        caps: u32,
        epoch: u32,
        refused_edits: u32,
        rss: u64,
        id: &'a str,
        msg: &'a str,
        /// Escaped canonical workspace root.
        root: &'a str,
    },
}

/// Append one record to an uncompressed `LSP_SERVERS` records buffer.
pub fn append_lsp_servers_record(buf: &mut Vec<u8>, record: &LspServersRecord<'_>) {
    let start = begin_record(buf);
    match record {
        LspServersRecord::Server {
            server_ref,
            phase,
            progress_pct,
            caps,
            epoch,
            refused_edits,
            rss,
            id,
            msg,
            root,
        } => {
            buf.push(LSP_STATE_RECORD_SERVER);
            buf.extend_from_slice(&server_ref.to_le_bytes());
            buf.push(*phase);
            buf.push(*progress_pct);
            buf.extend_from_slice(&caps.to_le_bytes());
            buf.extend_from_slice(&epoch.to_le_bytes());
            buf.extend_from_slice(&refused_edits.to_le_bytes());
            buf.extend_from_slice(&rss.to_le_bytes());
            push_str(buf, id);
            push_str(buf, msg);
            push_str(buf, root);
        }
    }
    end_record(buf, start);
}

pub struct LspServersRecordIter<'a> {
    data: &'a [u8],
}

/// Iterate records in an uncompressed `LSP_SERVERS` payload.
pub fn lsp_servers_records(data: &[u8]) -> LspServersRecordIter<'_> {
    LspServersRecordIter { data }
}

impl<'a> Iterator for LspServersRecordIter<'a> {
    type Item = LspServersRecord<'a>;

    fn next(&mut self) -> Option<LspServersRecord<'a>> {
        loop {
            let (kind, mut b) = next_record(&mut self.data)?;
            match kind {
                LSP_STATE_RECORD_SERVER => {
                    let server_ref = take_u16(&mut b)?;
                    let phase = take_u8(&mut b)?;
                    let progress_pct = take_u8(&mut b)?;
                    let caps = take_u32(&mut b)?;
                    let epoch = take_u32(&mut b)?;
                    let refused_edits = take_u32(&mut b)?;
                    let rss = take_u64(&mut b)?;
                    let id = take_str(&mut b)?;
                    let msg = take_str(&mut b)?;
                    let root = take_str(&mut b)?;
                    return Some(LspServersRecord::Server {
                        server_ref,
                        phase,
                        progress_pct,
                        caps,
                        epoch,
                        refused_edits,
                        rss,
                        id,
                        msg,
                        root,
                    });
                }
                _ => continue, // unknown kind: skip via record_len
            }
        }
    }
}

// ---------------------------------------------------------------------------
// LSP_DIAG records
// ---------------------------------------------------------------------------

/// One decoded record from an `LSP_DIAG` payload.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LspDiagRecord<'a> {
    /// FILE 0x01: [kind:1][hash:16][n:2][path_len:2][path:N]
    /// Replaces the file's entire diagnostic set (the following `n`
    /// `Diag` records); `n` = 0 clears. `hash` names the content version
    /// the set describes; zero when unknown.
    File {
        hash: LspHash,
        n: u16,
        path: &'a str,
    },
    /// DIAG 0x02: [kind:1][severity:1][flags:1][line:4][col:4][end_line:4][end_col:4]
    /// [code_len:2][code:N][src_len:2][source:N][msg_len:4][msg:N]
    Diag {
        /// One of `LSP_SEVERITY_*`.
        severity: u8,
        /// `LSP_DIAG_UNNECESSARY` / `LSP_DIAG_DEPRECATED`.
        flags: u8,
        line: u32,
        col: u32,
        end_line: u32,
        end_col: u32,
        code: &'a str,
        /// The producing backend (e.g. `rust-analyzer`, `clippy`).
        source: &'a str,
        msg: &'a str,
    },
}

/// Append one record to an uncompressed `LSP_DIAG` records buffer.
pub fn append_lsp_diag_record(buf: &mut Vec<u8>, record: &LspDiagRecord<'_>) {
    let start = begin_record(buf);
    match record {
        LspDiagRecord::File { hash, n, path } => {
            buf.push(LSP_DIAG_RECORD_FILE);
            buf.extend_from_slice(hash);
            buf.extend_from_slice(&n.to_le_bytes());
            push_str(buf, path);
        }
        LspDiagRecord::Diag {
            severity,
            flags,
            line,
            col,
            end_line,
            end_col,
            code,
            source,
            msg,
        } => {
            buf.push(LSP_DIAG_RECORD_DIAG);
            buf.push(*severity);
            buf.push(*flags);
            buf.extend_from_slice(&line.to_le_bytes());
            buf.extend_from_slice(&col.to_le_bytes());
            buf.extend_from_slice(&end_line.to_le_bytes());
            buf.extend_from_slice(&end_col.to_le_bytes());
            push_str(buf, code);
            push_str(buf, source);
            push_bytes(buf, msg.as_bytes());
        }
    }
    end_record(buf, start);
}

pub struct LspDiagRecordIter<'a> {
    data: &'a [u8],
}

/// Iterate records in an uncompressed `LSP_DIAG` payload.
pub fn lsp_diag_records(data: &[u8]) -> LspDiagRecordIter<'_> {
    LspDiagRecordIter { data }
}

impl<'a> Iterator for LspDiagRecordIter<'a> {
    type Item = LspDiagRecord<'a>;

    fn next(&mut self) -> Option<LspDiagRecord<'a>> {
        loop {
            let (kind, mut b) = next_record(&mut self.data)?;
            match kind {
                LSP_DIAG_RECORD_FILE => {
                    let hash = take_hash(&mut b)?;
                    let n = take_u16(&mut b)?;
                    let path = take_str(&mut b)?;
                    return Some(LspDiagRecord::File { hash, n, path });
                }
                LSP_DIAG_RECORD_DIAG => {
                    let severity = take_u8(&mut b)?;
                    let flags = take_u8(&mut b)?;
                    let line = take_u32(&mut b)?;
                    let col = take_u32(&mut b)?;
                    let end_line = take_u32(&mut b)?;
                    let end_col = take_u32(&mut b)?;
                    let code = take_str(&mut b)?;
                    let source = take_str(&mut b)?;
                    let msg = take_text(&mut b)?;
                    return Some(LspDiagRecord::Diag {
                        severity,
                        flags,
                        line,
                        col,
                        end_line,
                        end_col,
                        code,
                        source,
                        msg,
                    });
                }
                _ => continue, // unknown kind: skip via record_len
            }
        }
    }
}

// ---------------------------------------------------------------------------
// LSP_QUERY response records
// ---------------------------------------------------------------------------

/// One decoded record from an `LSP_QUERY` response payload.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LspQueryRecord<'a> {
    /// LOCATION 0x01: [kind:1][flags:1][hash:16][line:4][col:4][end_line:4][end_col:4][path_len:2][path:N]
    Location {
        flags: u8,
        /// Content version the location refers into; zero when unknown.
        hash: LspHash,
        line: u32,
        col: u32,
        end_line: u32,
        end_col: u32,
        path: &'a str,
    },
    /// MARKUP 0x02: [kind:1][format:1][text_len:4][text:N]
    Markup {
        /// [`LSP_MARKUP_PLAIN`] or [`LSP_MARKUP_MARKDOWN`].
        format: u8,
        text: &'a str,
    },
    /// SYMBOL 0x03: [kind:1][sym_kind:1][flags:1][depth:1][line:4][col:4]
    /// [end_line:4][end_col:4][name_len:2][name:N][path_len:2][path:N]
    /// `depth` nests document outlines (pre-order); 0 at the top level.
    Symbol {
        /// LSP SymbolKind value.
        sym_kind: u8,
        /// [`LSP_SYMBOL_DEPRECATED`].
        flags: u8,
        depth: u8,
        line: u32,
        col: u32,
        end_line: u32,
        end_col: u32,
        name: &'a str,
        path: &'a str,
    },
    /// EDIT 0x04: [kind:1][flags:1][hash:16][line:4][col:4][end_line:4][end_col:4]
    /// [new_len:4][new_text:N][path_len:2][path:N]
    /// One ordered edit of a rename plan, against the content version
    /// named by `hash`. Data, never applied.
    Edit {
        flags: u8,
        hash: LspHash,
        line: u32,
        col: u32,
        end_line: u32,
        end_col: u32,
        new_text: &'a str,
        path: &'a str,
    },
}

/// Append one record to an uncompressed `LSP_QUERY` response buffer.
pub fn append_lsp_query_record(buf: &mut Vec<u8>, record: &LspQueryRecord<'_>) {
    let start = begin_record(buf);
    match record {
        LspQueryRecord::Location {
            flags,
            hash,
            line,
            col,
            end_line,
            end_col,
            path,
        } => {
            buf.push(LSP_QUERY_RECORD_LOCATION);
            buf.push(*flags);
            buf.extend_from_slice(hash);
            buf.extend_from_slice(&line.to_le_bytes());
            buf.extend_from_slice(&col.to_le_bytes());
            buf.extend_from_slice(&end_line.to_le_bytes());
            buf.extend_from_slice(&end_col.to_le_bytes());
            push_str(buf, path);
        }
        LspQueryRecord::Markup { format, text } => {
            buf.push(LSP_QUERY_RECORD_MARKUP);
            buf.push(*format);
            push_bytes(buf, text.as_bytes());
        }
        LspQueryRecord::Symbol {
            sym_kind,
            flags,
            depth,
            line,
            col,
            end_line,
            end_col,
            name,
            path,
        } => {
            buf.push(LSP_QUERY_RECORD_SYMBOL);
            buf.push(*sym_kind);
            buf.push(*flags);
            buf.push(*depth);
            buf.extend_from_slice(&line.to_le_bytes());
            buf.extend_from_slice(&col.to_le_bytes());
            buf.extend_from_slice(&end_line.to_le_bytes());
            buf.extend_from_slice(&end_col.to_le_bytes());
            push_str(buf, name);
            push_str(buf, path);
        }
        LspQueryRecord::Edit {
            flags,
            hash,
            line,
            col,
            end_line,
            end_col,
            new_text,
            path,
        } => {
            buf.push(LSP_QUERY_RECORD_EDIT);
            buf.push(*flags);
            buf.extend_from_slice(hash);
            buf.extend_from_slice(&line.to_le_bytes());
            buf.extend_from_slice(&col.to_le_bytes());
            buf.extend_from_slice(&end_line.to_le_bytes());
            buf.extend_from_slice(&end_col.to_le_bytes());
            push_bytes(buf, new_text.as_bytes());
            push_str(buf, path);
        }
    }
    end_record(buf, start);
}

pub struct LspQueryRecordIter<'a> {
    data: &'a [u8],
}

/// Iterate records in an uncompressed `LSP_QUERY` response payload.
pub fn lsp_query_records(data: &[u8]) -> LspQueryRecordIter<'_> {
    LspQueryRecordIter { data }
}

impl<'a> Iterator for LspQueryRecordIter<'a> {
    type Item = LspQueryRecord<'a>;

    fn next(&mut self) -> Option<LspQueryRecord<'a>> {
        loop {
            let (kind, mut b) = next_record(&mut self.data)?;
            match kind {
                LSP_QUERY_RECORD_LOCATION => {
                    let flags = take_u8(&mut b)?;
                    let hash = take_hash(&mut b)?;
                    let line = take_u32(&mut b)?;
                    let col = take_u32(&mut b)?;
                    let end_line = take_u32(&mut b)?;
                    let end_col = take_u32(&mut b)?;
                    let path = take_str(&mut b)?;
                    return Some(LspQueryRecord::Location {
                        flags,
                        hash,
                        line,
                        col,
                        end_line,
                        end_col,
                        path,
                    });
                }
                LSP_QUERY_RECORD_MARKUP => {
                    let format = take_u8(&mut b)?;
                    let text = take_text(&mut b)?;
                    return Some(LspQueryRecord::Markup { format, text });
                }
                LSP_QUERY_RECORD_SYMBOL => {
                    let sym_kind = take_u8(&mut b)?;
                    let flags = take_u8(&mut b)?;
                    let depth = take_u8(&mut b)?;
                    let line = take_u32(&mut b)?;
                    let col = take_u32(&mut b)?;
                    let end_line = take_u32(&mut b)?;
                    let end_col = take_u32(&mut b)?;
                    let name = take_str(&mut b)?;
                    let path = take_str(&mut b)?;
                    return Some(LspQueryRecord::Symbol {
                        sym_kind,
                        flags,
                        depth,
                        line,
                        col,
                        end_line,
                        end_col,
                        name,
                        path,
                    });
                }
                LSP_QUERY_RECORD_EDIT => {
                    let flags = take_u8(&mut b)?;
                    let hash = take_hash(&mut b)?;
                    let line = take_u32(&mut b)?;
                    let col = take_u32(&mut b)?;
                    let end_line = take_u32(&mut b)?;
                    let end_col = take_u32(&mut b)?;
                    let new_text = take_text(&mut b)?;
                    let path = take_str(&mut b)?;
                    return Some(LspQueryRecord::Edit {
                        flags,
                        hash,
                        line,
                        col,
                        end_line,
                        end_col,
                        new_text,
                        path,
                    });
                }
                _ => continue, // unknown kind: skip via record_len
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Client-side reducers
// ---------------------------------------------------------------------------

/// One backend's projected state, from a `SERVER` record.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LspServerState {
    pub phase: u8,
    pub progress_pct: u8,
    pub caps: u32,
    pub epoch: u32,
    pub refused_edits: u32,
    pub rss: u64,
    pub id: String,
    pub msg: String,
}

/// The complete client obligation for `LSP_STATE`: replace the whole
/// map, ack.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LspStateMirror {
    /// Keyed by `server_ref`.
    pub servers: BTreeMap<u16, LspServerState>,
    /// The last snapshot's flags.
    pub flags: u8,
}

impl LspStateMirror {
    pub fn new() -> Self {
        Self::default()
    }

    /// Apply one `LSP_STATE` message (starting at the opcode byte),
    /// replacing the whole state. Returns `Some(state_id)` to
    /// acknowledge, `None` if malformed.
    pub fn apply_state(&mut self, msg: &[u8]) -> Option<u32> {
        let (_lsp_id, state_id, flags, records) = parse_lsp_state(msg)?;
        let mut next = LspStateMirror {
            flags,
            ..Default::default()
        };
        for record in lsp_state_records(&records) {
            match record {
                LspStateRecord::Server {
                    server_ref,
                    phase,
                    progress_pct,
                    caps,
                    epoch,
                    refused_edits,
                    rss,
                    id,
                    msg,
                } => {
                    next.servers.insert(
                        server_ref,
                        LspServerState {
                            phase,
                            progress_pct,
                            caps,
                            epoch,
                            refused_edits,
                            rss,
                            id: id.to_string(),
                            msg: msg.to_string(),
                        },
                    );
                }
            }
        }
        *self = next;
        Some(state_id)
    }
}

/// One diagnostic, owned, from a `DIAG` record.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LspDiagnostic {
    pub severity: u8,
    pub flags: u8,
    pub line: u32,
    pub col: u32,
    pub end_line: u32,
    pub end_col: u32,
    pub code: String,
    pub source: String,
    pub msg: String,
}

/// One file's diagnostic set.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LspFileDiags {
    /// Content version the set describes; zero when unknown.
    pub hash: LspHash,
    pub diags: Vec<LspDiagnostic>,
}

/// The complete client obligation for `LSP_DIAG`: apply per-file
/// replacement sets, ack. Absence of a path means unknown, not clean.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LspDiagMirror {
    /// Keyed by escaped workspace-relative path.
    pub files: BTreeMap<String, LspFileDiags>,
}

impl LspDiagMirror {
    pub fn new() -> Self {
        Self::default()
    }

    /// Apply one `LSP_DIAG` message (starting at the opcode byte).
    /// Returns `Some(update_id)` to acknowledge, `None` if malformed.
    pub fn apply_diag(&mut self, msg: &[u8]) -> Option<u32> {
        let (_lsp_id, update_id, flags, records) = parse_lsp_diag(msg)?;
        if flags & LSP_DIAG_FULL != 0 {
            self.files.clear();
        }
        // `Diag` records attach to the most recent `File` record.
        let mut current: Option<String> = None;
        for record in lsp_diag_records(&records) {
            match record {
                LspDiagRecord::File { hash, n, path } => {
                    if n == 0 {
                        self.files.remove(path);
                        current = None;
                    } else {
                        let entry = self.files.entry(path.to_string()).or_default();
                        entry.hash = hash;
                        entry.diags.clear();
                        current = Some(path.to_string());
                    }
                }
                LspDiagRecord::Diag {
                    severity,
                    flags,
                    line,
                    col,
                    end_line,
                    end_col,
                    code,
                    source,
                    msg,
                } => {
                    if let Some(file) = current.as_ref().and_then(|p| self.files.get_mut(p)) {
                        file.diags.push(LspDiagnostic {
                            severity,
                            flags,
                            line,
                            col,
                            end_line,
                            end_col,
                            code: code.to_string(),
                            source: source.to_string(),
                            msg: msg.to_string(),
                        });
                    }
                }
            }
        }
        Some(update_id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(b: &[u8]) -> String {
        b.iter().map(|x| format!("{x:02x}")).collect()
    }

    fn hash(fill: u8) -> LspHash {
        [fill; 16]
    }

    #[test]
    fn request_roundtrips() {
        let msg = msg_lsp_open(7, LSP_OPEN_WATCH | LSP_OPEN_DIAGS, 500, "/src");
        assert_eq!(parse_lsp_open(&msg), Some((7, 3, 500, "/src")));

        let msg = msg_lsp_close(9);
        assert_eq!(parse_lsp_close(&msg), Some(9));

        let msg = msg_lsp_ack(9, LSP_STREAM_DIAG, 0x01020304);
        assert_eq!(parse_lsp_ack(&msg), Some((9, LSP_STREAM_DIAG, 0x01020304)));

        let req = LspQueryRequest {
            nonce: 3,
            lsp_id: 9,
            kind: LSP_QUERY_REFERENCES,
            flags: LSP_REFS_INCLUDE_DECLARATION,
            line: 10,
            col: 4,
            path: "src/lib.rs",
            arg: "",
        };
        assert_eq!(parse_lsp_query(&msg_lsp_query(&req)), Some(req));

        let msg = msg_lsp_cancel(3);
        assert_eq!(parse_lsp_cancel(&msg), Some(3));

        let msg = msg_lsp_servers(4);
        assert_eq!(parse_lsp_servers(&msg), Some(4));

        let msg = msg_lsp_stop(5, 2);
        assert_eq!(parse_lsp_stop(&msg), Some((5, 2)));
    }

    #[test]
    fn response_roundtrips() {
        let msg = msg_lsp_opened(7, 1, LSP_STATUS_OK, 0, "/src", "");
        assert_eq!(
            parse_lsp_opened(&msg),
            Some(LspOpened {
                nonce: 7,
                lsp_id: 1,
                status: LSP_STATUS_OK,
                flags: 0,
                root: "/src",
                detail: "",
            })
        );

        let msg = msg_lsp_opened(
            8,
            LSP_ID_INVALID,
            LSP_STATUS_NOT_FOUND,
            0,
            "",
            "gopls: not found on PATH",
        );
        let opened = parse_lsp_opened(&msg).unwrap();
        assert_eq!(opened.lsp_id, LSP_ID_INVALID);
        assert_eq!(opened.detail, "gopls: not found on PATH");

        let mut records = Vec::new();
        append_lsp_state_record(
            &mut records,
            &LspStateRecord::Server {
                server_ref: 1,
                phase: LSP_PHASE_INDEXING,
                progress_pct: 42,
                caps: LSP_CAP_DEFINITION | LSP_CAP_HOVER,
                epoch: 2,
                refused_edits: 1,
                rss: 3 << 30,
                id: "rust-analyzer",
                msg: "indexing 42%",
            },
        );
        let msg = msg_lsp_state(1, 5, 0, &records);
        let (lsp_id, state_id, flags, decoded) = parse_lsp_state(&msg).unwrap();
        assert_eq!((lsp_id, state_id, flags), (1, 5, 0));
        assert_eq!(decoded, records);

        let msg = msg_lsp_diag(1, 6, LSP_DIAG_FULL, &[]);
        assert_eq!(parse_lsp_diag(&msg), Some((1, 6, LSP_DIAG_FULL, vec![])));

        let msg = msg_lsp_query_resp(3, LSP_STATUS_OK, 0, "", &[]);
        assert_eq!(
            parse_lsp_query_resp(&msg),
            Some(LspQueryResp {
                nonce: 3,
                status: LSP_STATUS_OK,
                flags: 0,
                detail: String::new(),
                records: vec![],
            })
        );
        // A failure carries the reason.
        let msg = msg_lsp_query_resp(4, LSP_STATUS_OTHER, 0, "gopls: boom", &[]);
        let resp = parse_lsp_query_resp(&msg).unwrap();
        assert_eq!(resp.status, LSP_STATUS_OTHER);
        assert_eq!(resp.detail, "gopls: boom");

        let msg = msg_lsp_closed(1, LSP_CLOSED_BACKEND_FAILED);
        assert_eq!(parse_lsp_closed(&msg), Some((1, LSP_CLOSED_BACKEND_FAILED)));

        let msg = msg_lsp_servers_resp(4, LSP_STATUS_OK, 0, &[]);
        assert_eq!(
            parse_lsp_servers_resp(&msg),
            Some((4, LSP_STATUS_OK, 0, vec![]))
        );

        let msg = msg_lsp_stopped(5, LSP_STATUS_OK);
        assert_eq!(parse_lsp_stopped(&msg), Some((5, LSP_STATUS_OK)));
    }

    #[test]
    fn state_record_roundtrip() {
        let record = LspStateRecord::Server {
            server_ref: 2,
            phase: LSP_PHASE_READY,
            progress_pct: LSP_PROGRESS_UNKNOWN,
            caps: LSP_CAP_RENAME,
            epoch: 0,
            refused_edits: 0,
            rss: 0,
            id: "gopls",
            msg: "",
        };
        let mut buf = Vec::new();
        append_lsp_state_record(&mut buf, &record);
        let decoded: Vec<_> = lsp_state_records(&buf).collect();
        assert_eq!(decoded, vec![record]);
    }

    #[test]
    fn servers_record_roundtrip() {
        let record = LspServersRecord::Server {
            server_ref: 2,
            phase: LSP_PHASE_READY,
            progress_pct: 100,
            caps: 0x3F,
            epoch: 1,
            refused_edits: 0,
            rss: 1 << 20,
            id: "gopls",
            msg: "ready",
            root: "/work/api",
        };
        let mut buf = Vec::new();
        append_lsp_servers_record(&mut buf, &record);
        let decoded: Vec<_> = lsp_servers_records(&buf).collect();
        assert_eq!(decoded, vec![record]);
    }

    #[test]
    fn diag_record_roundtrip() {
        let records = vec![
            LspDiagRecord::File {
                hash: hash(0xAB),
                n: 1,
                path: "src/lib.rs",
            },
            LspDiagRecord::Diag {
                severity: LSP_SEVERITY_ERROR,
                flags: LSP_DIAG_UNNECESSARY,
                line: 10,
                col: 4,
                end_line: 10,
                end_col: 9,
                code: "E0308",
                source: "rustc",
                msg: "mismatched types",
            },
            LspDiagRecord::File {
                hash: LSP_HASH_NONE,
                n: 0,
                path: "src/old.rs",
            },
        ];
        let mut buf = Vec::new();
        for r in &records {
            append_lsp_diag_record(&mut buf, r);
        }
        let decoded: Vec<_> = lsp_diag_records(&buf).collect();
        assert_eq!(decoded, records);
    }

    #[test]
    fn query_record_roundtrip() {
        let records = vec![
            LspQueryRecord::Location {
                flags: 0,
                hash: hash(0xCD),
                line: 5,
                col: 0,
                end_line: 5,
                end_col: 12,
                path: "src/main.rs",
            },
            LspQueryRecord::Markup {
                format: LSP_MARKUP_MARKDOWN,
                text: "```rust\nfn main()\n```",
            },
            LspQueryRecord::Symbol {
                sym_kind: 12, // Function
                flags: LSP_SYMBOL_DEPRECATED,
                depth: 1,
                line: 3,
                col: 4,
                end_line: 9,
                end_col: 1,
                name: "main",
                path: "src/main.rs",
            },
            LspQueryRecord::Edit {
                flags: 0,
                hash: hash(0xEF),
                line: 3,
                col: 7,
                end_line: 3,
                end_col: 11,
                new_text: "run",
                path: "src/main.rs",
            },
        ];
        let mut buf = Vec::new();
        for r in &records {
            append_lsp_query_record(&mut buf, r);
        }
        let decoded: Vec<_> = lsp_query_records(&buf).collect();
        assert_eq!(decoded, records);
    }

    #[test]
    fn unknown_record_kind_is_skipped() {
        let mut buf = Vec::new();
        // A future record kind: [record_len][0x7F][payload].
        let start = begin_record(&mut buf);
        buf.push(0x7F);
        buf.extend_from_slice(b"future");
        end_record(&mut buf, start);
        append_lsp_query_record(
            &mut buf,
            &LspQueryRecord::Markup {
                format: LSP_MARKUP_PLAIN,
                text: "hi",
            },
        );
        let decoded: Vec<_> = lsp_query_records(&buf).collect();
        assert_eq!(
            decoded,
            vec![LspQueryRecord::Markup {
                format: LSP_MARKUP_PLAIN,
                text: "hi",
            }]
        );
    }

    #[test]
    fn malformed_record_ends_iteration() {
        let mut buf = Vec::new();
        append_lsp_diag_record(
            &mut buf,
            &LspDiagRecord::File {
                hash: LSP_HASH_NONE,
                n: 0,
                path: "a",
            },
        );
        // Declared length overruns the buffer: iteration must end, not panic.
        buf.extend_from_slice(&99u32.to_le_bytes());
        buf.push(LSP_DIAG_RECORD_FILE);
        let decoded: Vec<_> = lsp_diag_records(&buf).collect();
        assert_eq!(decoded.len(), 1);
    }

    #[test]
    fn oversized_declared_length_is_rejected_before_allocation() {
        let mut msg = vec![S2C_LSP_STATE];
        msg.extend_from_slice(&1u16.to_le_bytes());
        msg.extend_from_slice(&1u32.to_le_bytes());
        msg.push(0);
        // A hostile 4 GiB declared size must be refused before allocating.
        msg.extend_from_slice(&(u32::MAX).to_le_bytes());
        msg.extend_from_slice(&[0; 8]);
        assert_eq!(parse_lsp_state(&msg), None);
    }

    #[test]
    fn state_mirror_replaces_whole_state() {
        let mut records = Vec::new();
        append_lsp_state_record(
            &mut records,
            &LspStateRecord::Server {
                server_ref: 1,
                phase: LSP_PHASE_INDEXING,
                progress_pct: 10,
                caps: 0,
                epoch: 0,
                refused_edits: 0,
                rss: 0,
                id: "rust-analyzer",
                msg: "indexing",
            },
        );
        let mut mirror = LspStateMirror::new();
        assert_eq!(
            mirror.apply_state(&msg_lsp_state(1, 1, 0, &records)),
            Some(1)
        );
        assert_eq!(mirror.servers.len(), 1);
        assert_eq!(mirror.servers[&1].phase, LSP_PHASE_INDEXING);

        // The next snapshot replaces, never merges.
        let mut records = Vec::new();
        append_lsp_state_record(
            &mut records,
            &LspStateRecord::Server {
                server_ref: 2,
                phase: LSP_PHASE_READY,
                progress_pct: 100,
                caps: 0x3F,
                epoch: 0,
                refused_edits: 0,
                rss: 0,
                id: "gopls",
                msg: "",
            },
        );
        assert_eq!(
            mirror.apply_state(&msg_lsp_state(1, 2, 0, &records)),
            Some(2)
        );
        assert_eq!(mirror.servers.len(), 1);
        assert!(mirror.servers.contains_key(&2));
    }

    #[test]
    fn diag_mirror_applies_replacement_sets() {
        let mut mirror = LspDiagMirror::new();

        // FULL replay: one file, one diagnostic.
        let mut records = Vec::new();
        append_lsp_diag_record(
            &mut records,
            &LspDiagRecord::File {
                hash: hash(1),
                n: 1,
                path: "a.rs",
            },
        );
        append_lsp_diag_record(
            &mut records,
            &LspDiagRecord::Diag {
                severity: LSP_SEVERITY_ERROR,
                flags: 0,
                line: 1,
                col: 0,
                end_line: 1,
                end_col: 5,
                code: "E1",
                source: "t",
                msg: "boom",
            },
        );
        assert_eq!(
            mirror.apply_diag(&msg_lsp_diag(1, 1, LSP_DIAG_FULL, &records)),
            Some(1)
        );
        assert_eq!(mirror.files.len(), 1);
        assert_eq!(mirror.files["a.rs"].diags.len(), 1);

        // Incremental: replace a.rs with an empty set (n=0 removes).
        let mut records = Vec::new();
        append_lsp_diag_record(
            &mut records,
            &LspDiagRecord::File {
                hash: hash(2),
                n: 0,
                path: "a.rs",
            },
        );
        append_lsp_diag_record(
            &mut records,
            &LspDiagRecord::File {
                hash: hash(3),
                n: 1,
                path: "b.rs",
            },
        );
        append_lsp_diag_record(
            &mut records,
            &LspDiagRecord::Diag {
                severity: LSP_SEVERITY_WARNING,
                flags: 0,
                line: 2,
                col: 1,
                end_line: 2,
                end_col: 2,
                code: "",
                source: "t",
                msg: "hm",
            },
        );
        assert_eq!(mirror.apply_diag(&msg_lsp_diag(1, 2, 0, &records)), Some(2));
        assert!(!mirror.files.contains_key("a.rs"));
        assert_eq!(mirror.files["b.rs"].hash, hash(3));

        // A later FULL drops files it does not re-list.
        assert_eq!(
            mirror.apply_diag(&msg_lsp_diag(1, 3, LSP_DIAG_FULL, &[])),
            Some(3)
        );
        assert!(mirror.files.is_empty());
    }

    /// Byte-exact fixtures pinned across the Rust and TypeScript codecs
    /// (js/core/src/lsp.ts): drift fails on one side or the other.
    #[test]
    fn wire_fixtures() {
        assert_eq!(
            hex(&msg_lsp_open(
                0x0102,
                LSP_OPEN_WATCH | LSP_OPEN_DIAGS,
                500,
                "/src"
            )),
            "60020103f40104002f737263"
        );
        assert_eq!(hex(&msg_lsp_close(7)), "610700");
        assert_eq!(
            hex(&msg_lsp_ack(7, LSP_STREAM_DIAG, 0x01020304)),
            "6207000104030201"
        );
        assert_eq!(
            hex(&msg_lsp_query(&LspQueryRequest {
                nonce: 3,
                lsp_id: 7,
                kind: LSP_QUERY_DEFINITION,
                flags: 0,
                line: 10,
                col: 4,
                path: "a.rs",
                arg: "",
            })),
            "630300070001000a000000040000000400612e72730000"
        );
        assert_eq!(hex(&msg_lsp_cancel(10)), "640a00");
        assert_eq!(hex(&msg_lsp_servers(11)), "650b00");
        assert_eq!(hex(&msg_lsp_stop(12, 2)), "660c000200");
        assert_eq!(
            hex(&msg_lsp_opened(5, 1, LSP_STATUS_OK, 0, "/w", "")),
            "6005000100000002002f770000"
        );
        assert_eq!(hex(&msg_lsp_closed(1, LSP_CLOSED_ROOT_GONE)), "64010001");
        assert_eq!(hex(&msg_lsp_stopped(6, LSP_STATUS_OK)), "66060000");

        // Uncompressed record-buffer fixtures (LZ4 output is
        // implementation-specific, so cross-codec pinning happens on the
        // record bytes, not the compressed message).
        let mut buf = Vec::new();
        append_lsp_diag_record(
            &mut buf,
            &LspDiagRecord::File {
                hash: hash(0xAB),
                n: 1,
                path: "a.rs",
            },
        );
        assert_eq!(
            hex(&buf),
            format!("1900000001{}01000400612e7273", "ab".repeat(16))
        );
        let mut buf = Vec::new();
        append_lsp_query_record(
            &mut buf,
            &LspQueryRecord::Markup {
                format: LSP_MARKUP_MARKDOWN,
                text: "hi",
            },
        );
        assert_eq!(hex(&buf), "080000000201020000006869");
    }
}
