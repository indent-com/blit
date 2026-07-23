//! Filesystem state sync wire protocol (docs/fs-watch.md).
//!
//! The server maintains a canonical replica of a watched tree and streams
//! per-client state diffs (`FS_UPDATE`). Clients apply records to a map and
//! acknowledge. Snapshots and recovery are `RESET … SYNC` staged series;
//! loss and overflow are not wire concepts.
//!
//! All integers little-endian, tightly packed, as everywhere in the protocol.

use std::collections::BTreeMap;

/// Start (or replace) a sync: [0x40][nonce:2][flags:1][latency_ms:2][inline_max:4][path_len:2][path:N]
pub const C2S_FS_SYNC: u8 = 0x40;
/// Stop a sync: [0x41][sync_id:2]
pub const C2S_FS_STOP: u8 = 0x41;
/// Cumulative acknowledgement: [0x42][sync_id:2][update_id:4]
pub const C2S_FS_ACK: u8 = 0x42;
/// Fetch full content of one file: [0x43][nonce:2][sync_id:2][path_len:2][path:N]
pub const C2S_FS_FETCH: u8 = 0x43;
/// Write file content (CAS): [0x44][nonce:2][sync_id:2][flags:1][base:16][mode:4][content_kind:1][path_len:2][path:N][content:LZ4]
pub const C2S_FS_WRITE: u8 = 0x44;
/// Metadata op (mkdir/remove/rename): [0x45][nonce:2][sync_id:2][op:1][flags:1][base:16][mode:4][a_len:2][a:N][b_len:2][b:N]
pub const C2S_FS_OP: u8 = 0x45;

/// Sync accepted or rejected: [0x40][nonce:2][sync_id:2][status:1][detail_len:2][detail:N]
/// On success detail is the canonical root (UTF-8); on failure a diagnostic.
pub const S2C_FS_SYNCED: u8 = 0x40;
/// State diff: [0x41][sync_id:2][update_id:4][flags:1][records:LZ4]
pub const S2C_FS_UPDATE: u8 = 0x41;
/// Fetch response: [0x42][nonce:2][status:1][data:LZ4]
pub const S2C_FS_FILE: u8 = 0x42;
/// Sync terminated: [0x43][sync_id:2][reason:1]
pub const S2C_FS_CLOSED: u8 = 0x43;
/// Write/op result: [0x44][nonce:2][status:1][hash:16][mtime_ns:8]
pub const S2C_FS_DONE: u8 = 0x44;

/// `S2C_HELLO` feature bit: server supports the `FS_*` message family.
pub const FEATURE_FS_SYNC: u32 = 1 << 6;

/// `S2C_HELLO` feature bit: server supports the write family (`FS_WRITE`,
/// `FS_OP`). Separately advertised so a deployment can offer read-only
/// sync without writes (docs/design/fs-write.md "Security").
pub const FEATURE_FS_WRITE: u32 = 1 << 9;

/// `sync_id` reported by a failed `FS_SYNCED`.
pub const FS_SYNC_ID_INVALID: u16 = 0xFFFF;

// C2S_FS_SYNC flags.
pub const FS_SYNC_RECURSIVE: u8 = 1 << 0;
pub const FS_SYNC_CONTENT: u8 = 1 << 1;
pub const FS_SYNC_CROSS_FILESYSTEM: u8 = 1 << 2;

// S2C_FS_UPDATE flags.
/// Begin a staged snapshot: apply this and subsequent records to an empty
/// staging map instead of the live map.
pub const FS_UPDATE_RESET: u8 = 1 << 0;
/// Atomically replace the live map with the staging map (no-op without one).
pub const FS_UPDATE_SYNC: u8 = 1 << 1;

// S2C_FS_SYNCED status.
pub const FS_STATUS_OK: u8 = 0;
pub const FS_STATUS_NOT_FOUND: u8 = 1;
pub const FS_STATUS_PERMISSION_DENIED: u8 = 2;
pub const FS_STATUS_RESOURCE_LIMIT: u8 = 3;
pub const FS_STATUS_OTHER: u8 = 4;

// S2C_FS_FILE status.
pub const FS_FILE_OK: u8 = 0;
pub const FS_FILE_NOT_FOUND: u8 = 1;
pub const FS_FILE_UNREADABLE: u8 = 2;
pub const FS_FILE_OTHER: u8 = 3;

// FS_DONE status — the unified git/lsp status table (docs/git.md
// "Statuses"), NOT FS_SYNCED's grandfathered 0-4, plus one fs addition.
// Same numeric values as `GIT_STATUS_*` where they overlap.
pub const FS_DONE_OK: u8 = 0;
pub const FS_DONE_NOT_FOUND: u8 = 2;
pub const FS_DONE_WRONG_TYPE: u8 = 3;
pub const FS_DONE_PERMISSION: u8 = 4;
pub const FS_DONE_TOO_LARGE: u8 = 5;
pub const FS_DONE_BUDGET: u8 = 6;
pub const FS_DONE_INVALID: u8 = 7;
pub const FS_DONE_OTHER: u8 = 9;
/// A precondition failed (CAS mismatch, create-exclusive on an existing
/// path, conditional remove on a changed file). On `CONFLICT`,
/// `FS_DONE.hash` carries the current on-disk hash so the client rebases
/// without a round trip. Added in lsp's `10 WARMING` extension style.
pub const FS_DONE_CONFLICT: u8 = 11;

/// Human-readable name for an `FS_DONE` status code.
pub fn fs_done_status_text(status: u8) -> &'static str {
    match status {
        FS_DONE_OK => "ok",
        FS_DONE_NOT_FOUND => "not found",
        FS_DONE_WRONG_TYPE => "wrong type",
        FS_DONE_PERMISSION => "permission denied",
        FS_DONE_TOO_LARGE => "too large",
        FS_DONE_BUDGET => "budget exhausted",
        FS_DONE_INVALID => "invalid request",
        FS_DONE_CONFLICT => "conflict",
        _ => "error",
    }
}

// FS_WRITE flags.
/// Ignore `base`; unconditional overwrite/create ("Save As, replace").
pub const FS_WRITE_NO_CAS: u8 = 1 << 0;
/// Create missing parent directories.
pub const FS_WRITE_MKPARENTS: u8 = 1 << 1;
/// fsync the file and its parent (F_FULLFSYNC on macOS) before returning.
pub const FS_WRITE_DURABLE: u8 = 1 << 2;
/// Write through a final-component symlink whose resolved target stays
/// under the root; default refuses one.
pub const FS_WRITE_FOLLOW_SYMLINK: u8 = 1 << 3;

// FS_WRITE content_kind: 0/1 are full bytes (v1); 2 is a reserved
// delta-against-`base` encoding (v2). A client may always send full.
pub const FS_WRITE_CONTENT_FULL: u8 = 1;
pub const FS_WRITE_CONTENT_DELTA: u8 = 2;

// FS_OP op selector.
pub const FS_OP_MKDIR: u8 = 1;
pub const FS_OP_REMOVE: u8 = 2;
pub const FS_OP_RENAME: u8 = 3;

// FS_OP flags (subset of FS_WRITE's, same bit positions).
pub const FS_OP_NO_CAS: u8 = 1 << 0;
pub const FS_OP_MKPARENTS: u8 = 1 << 1;

// S2C_FS_CLOSED reasons.
pub const FS_CLOSED_CLIENT_REQUEST: u8 = 0;
pub const FS_CLOSED_ROOT_GONE: u8 = 1;
pub const FS_CLOSED_PERMISSION_LOST: u8 = 2;
pub const FS_CLOSED_BACKEND_FAILED: u8 = 3;
pub const FS_CLOSED_RESOURCE_LIMIT: u8 = 4;

// Record kinds inside FS_UPDATE.
pub const FS_RECORD_UPSERT: u8 = 0x01;
pub const FS_RECORD_DELETE: u8 = 0x02;
pub const FS_RECORD_MOVE: u8 = 0x03;

// UPSERT entry_flags: bits 0-1 node type, higher bits flags.
pub const FS_ENTRY_TYPE_MASK: u8 = 0b11;
pub const FS_ENTRY_FILE: u8 = 0;
pub const FS_ENTRY_DIR: u8 = 1;
pub const FS_ENTRY_SYMLINK: u8 = 2;
pub const FS_ENTRY_OTHER: u8 = 3;
/// Entry exists but its content could not be read.
pub const FS_ENTRY_UNREADABLE: u8 = 1 << 2;
/// Content omitted: over `inline_max` or the sync did not request content.
pub const FS_ENTRY_NO_CONTENT: u8 = 1 << 3;
/// File changed repeatedly while being read; content omitted, another
/// upsert follows once it settles.
pub const FS_ENTRY_UNSTABLE: u8 = 1 << 4;

// UPSERT content kinds.
pub const FS_CONTENT_NONE: u8 = 0;
pub const FS_CONTENT_FULL: u8 = 1;
pub const FS_CONTENT_DELTA: u8 = 2;

/// One decoded record from an `FS_UPDATE` payload.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FsRecord<'a> {
    Upsert {
        path: &'a str,
        entry_flags: u8,
        size: u64,
        mtime_ns: u64,
        mode: u32,
        /// BLAKE3 truncated to 128 bits; zero for non-files or unknown.
        hash: u128,
        content: FsContent<'a>,
    },
    /// Remove `path` and every path under it.
    Delete { path: &'a str },
    /// Rename the `from` subtree to `to`.
    Move { from: &'a str, to: &'a str },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FsContent<'a> {
    None,
    Full(&'a [u8]),
    /// LEB128 instruction stream against the last content this client
    /// acked for this path: 0x01 COPY [offset][len], 0x02 INSERT [len][bytes].
    Delta(&'a [u8]),
}

/// Append one record to an uncompressed `FS_UPDATE` records buffer.
pub fn append_fs_record(buf: &mut Vec<u8>, record: &FsRecord<'_>) {
    let start = buf.len();
    buf.extend_from_slice(&0u32.to_le_bytes()); // record_len placeholder
    match record {
        FsRecord::Upsert {
            path,
            entry_flags,
            size,
            mtime_ns,
            mode,
            hash,
            content,
        } => {
            buf.push(FS_RECORD_UPSERT);
            buf.push(*entry_flags);
            let pb = path.as_bytes();
            buf.extend_from_slice(&(pb.len() as u16).to_le_bytes());
            buf.extend_from_slice(pb);
            buf.extend_from_slice(&size.to_le_bytes());
            buf.extend_from_slice(&mtime_ns.to_le_bytes());
            buf.extend_from_slice(&mode.to_le_bytes());
            buf.extend_from_slice(&hash.to_le_bytes());
            match content {
                FsContent::None => buf.push(FS_CONTENT_NONE),
                FsContent::Full(data) => {
                    buf.push(FS_CONTENT_FULL);
                    buf.extend_from_slice(&(data.len() as u32).to_le_bytes());
                    buf.extend_from_slice(data);
                }
                FsContent::Delta(ops) => {
                    buf.push(FS_CONTENT_DELTA);
                    buf.extend_from_slice(&(ops.len() as u32).to_le_bytes());
                    buf.extend_from_slice(ops);
                }
            }
        }
        FsRecord::Delete { path } => {
            buf.push(FS_RECORD_DELETE);
            let pb = path.as_bytes();
            buf.extend_from_slice(&(pb.len() as u16).to_le_bytes());
            buf.extend_from_slice(pb);
        }
        FsRecord::Move { from, to } => {
            buf.push(FS_RECORD_MOVE);
            let fb = from.as_bytes();
            buf.extend_from_slice(&(fb.len() as u16).to_le_bytes());
            buf.extend_from_slice(fb);
            let tb = to.as_bytes();
            buf.extend_from_slice(&(tb.len() as u16).to_le_bytes());
            buf.extend_from_slice(tb);
        }
    }
    let len = (buf.len() - start - 4) as u32;
    buf[start..start + 4].copy_from_slice(&len.to_le_bytes());
}

/// Iterate records in an uncompressed `FS_UPDATE` payload.
/// Unknown kinds are skipped via `record_len`; a malformed record ends
/// iteration (the update is applied up to that point and the rest dropped —
/// forward-compatible with future record extensions).
pub struct FsRecordIter<'a> {
    data: &'a [u8],
}

pub fn fs_records(data: &[u8]) -> FsRecordIter<'_> {
    FsRecordIter { data }
}

fn take_path<'a>(body: &mut &'a [u8]) -> Option<&'a str> {
    if body.len() < 2 {
        return None;
    }
    let len = u16::from_le_bytes([body[0], body[1]]) as usize;
    if body.len() < 2 + len {
        return None;
    }
    let s = std::str::from_utf8(&body[2..2 + len]).ok()?;
    *body = &body[2 + len..];
    Some(s)
}

impl<'a> Iterator for FsRecordIter<'a> {
    type Item = FsRecord<'a>;

    fn next(&mut self) -> Option<FsRecord<'a>> {
        loop {
            if self.data.len() < 4 {
                return None;
            }
            let rec_len =
                u32::from_le_bytes([self.data[0], self.data[1], self.data[2], self.data[3]])
                    as usize;
            if self.data.len() < 4 + rec_len || rec_len == 0 {
                return None;
            }
            let mut body = &self.data[4..4 + rec_len];
            self.data = &self.data[4 + rec_len..];
            let kind = body[0];
            body = &body[1..];
            match kind {
                FS_RECORD_UPSERT => {
                    if body.is_empty() {
                        return None;
                    }
                    let entry_flags = body[0];
                    body = &body[1..];
                    let path = take_path(&mut body)?;
                    if body.len() < 8 + 8 + 4 + 16 + 1 {
                        return None;
                    }
                    let size = u64::from_le_bytes(body[0..8].try_into().unwrap());
                    let mtime_ns = u64::from_le_bytes(body[8..16].try_into().unwrap());
                    let mode = u32::from_le_bytes(body[16..20].try_into().unwrap());
                    let hash = u128::from_le_bytes(body[20..36].try_into().unwrap());
                    let content_kind = body[36];
                    body = &body[37..];
                    let content = match content_kind {
                        FS_CONTENT_NONE => FsContent::None,
                        FS_CONTENT_FULL | FS_CONTENT_DELTA => {
                            if body.len() < 4 {
                                return None;
                            }
                            let len = u32::from_le_bytes(body[0..4].try_into().unwrap()) as usize;
                            if body.len() < 4 + len {
                                return None;
                            }
                            let data = &body[4..4 + len];
                            if content_kind == FS_CONTENT_FULL {
                                FsContent::Full(data)
                            } else {
                                FsContent::Delta(data)
                            }
                        }
                        _ => return None,
                    };
                    return Some(FsRecord::Upsert {
                        path,
                        entry_flags,
                        size,
                        mtime_ns,
                        mode,
                        hash,
                        content,
                    });
                }
                FS_RECORD_DELETE => {
                    let path = take_path(&mut body)?;
                    return Some(FsRecord::Delete { path });
                }
                FS_RECORD_MOVE => {
                    let from = take_path(&mut body)?;
                    let to = take_path(&mut body)?;
                    return Some(FsRecord::Move { from, to });
                }
                _ => continue, // unknown kind: skip via record_len
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Message builders
// ---------------------------------------------------------------------------

pub fn msg_fs_sync(nonce: u16, flags: u8, latency_ms: u16, inline_max: u32, path: &str) -> Vec<u8> {
    let pb = path.as_bytes();
    let mut msg = Vec::with_capacity(12 + pb.len());
    msg.push(C2S_FS_SYNC);
    msg.extend_from_slice(&nonce.to_le_bytes());
    msg.push(flags);
    msg.extend_from_slice(&latency_ms.to_le_bytes());
    msg.extend_from_slice(&inline_max.to_le_bytes());
    msg.extend_from_slice(&(pb.len() as u16).to_le_bytes());
    msg.extend_from_slice(pb);
    msg
}

pub fn msg_fs_stop(sync_id: u16) -> Vec<u8> {
    let mut msg = Vec::with_capacity(3);
    msg.push(C2S_FS_STOP);
    msg.extend_from_slice(&sync_id.to_le_bytes());
    msg
}

pub fn msg_fs_ack(sync_id: u16, update_id: u32) -> Vec<u8> {
    let mut msg = Vec::with_capacity(7);
    msg.push(C2S_FS_ACK);
    msg.extend_from_slice(&sync_id.to_le_bytes());
    msg.extend_from_slice(&update_id.to_le_bytes());
    msg
}

pub fn msg_fs_fetch(nonce: u16, sync_id: u16, path: &str) -> Vec<u8> {
    let pb = path.as_bytes();
    let mut msg = Vec::with_capacity(7 + pb.len());
    msg.push(C2S_FS_FETCH);
    msg.extend_from_slice(&nonce.to_le_bytes());
    msg.extend_from_slice(&sync_id.to_le_bytes());
    msg.extend_from_slice(&(pb.len() as u16).to_le_bytes());
    msg.extend_from_slice(pb);
    msg
}

pub fn msg_fs_synced(nonce: u16, sync_id: u16, status: u8, detail: &str) -> Vec<u8> {
    let db = detail.as_bytes();
    let mut msg = Vec::with_capacity(8 + db.len());
    msg.push(S2C_FS_SYNCED);
    msg.extend_from_slice(&nonce.to_le_bytes());
    msg.extend_from_slice(&sync_id.to_le_bytes());
    msg.push(status);
    msg.extend_from_slice(&(db.len() as u16).to_le_bytes());
    msg.extend_from_slice(db);
    msg
}

/// Build an `FS_UPDATE` from an uncompressed records buffer.
pub fn msg_fs_update(sync_id: u16, update_id: u32, flags: u8, records: &[u8]) -> Vec<u8> {
    let compressed = lz4_flex::compress_prepend_size(records);
    let mut msg = Vec::with_capacity(8 + compressed.len());
    msg.push(S2C_FS_UPDATE);
    msg.extend_from_slice(&sync_id.to_le_bytes());
    msg.extend_from_slice(&update_id.to_le_bytes());
    msg.push(flags);
    msg.extend_from_slice(&compressed);
    msg
}

pub fn msg_fs_file(nonce: u16, status: u8, data: &[u8]) -> Vec<u8> {
    let compressed = lz4_flex::compress_prepend_size(data);
    let mut msg = Vec::with_capacity(4 + compressed.len());
    msg.push(S2C_FS_FILE);
    msg.extend_from_slice(&nonce.to_le_bytes());
    msg.push(status);
    msg.extend_from_slice(&compressed);
    msg
}

pub fn msg_fs_closed(sync_id: u16, reason: u8) -> Vec<u8> {
    let mut msg = Vec::with_capacity(4);
    msg.push(S2C_FS_CLOSED);
    msg.extend_from_slice(&sync_id.to_le_bytes());
    msg.push(reason);
    msg
}

// ---------------------------------------------------------------------------
// Client-side reducer
// ---------------------------------------------------------------------------

/// One node in a mirrored tree.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FsNode {
    pub entry_flags: u8,
    pub size: u64,
    pub mtime_ns: u64,
    pub mode: u32,
    pub hash: u128,
    /// Present when the sync requested content and the file fits the
    /// inline limit. `None` does not mean empty — check `entry_flags`.
    pub content: Option<Vec<u8>>,
}

/// Cap on any single LZ4-decompressed fs payload — the protocol-wide
/// [`crate::MAX_DECOMPRESSED`] guard (docs/protocol.md). Checked against
/// the prepended size *before* allocating, so a hostile or corrupt length
/// cannot force a giant allocation (the terminal path has the same guard).
/// Large trees arrive as many bounded updates, never one huge one; content
/// records are bounded by the sync's `inline_max` (16 MiB default).
pub const FS_MAX_DECOMPRESSED: usize = crate::MAX_DECOMPRESSED;

/// Decompress a `compress_prepend_size` payload, refusing declared sizes
/// over [`FS_MAX_DECOMPRESSED`].
fn decompress_guarded(data: &[u8]) -> Option<Vec<u8>> {
    if data.len() < 4 {
        return None;
    }
    let declared = u32::from_le_bytes(data[0..4].try_into().unwrap()) as usize;
    if declared > FS_MAX_DECOMPRESSED {
        return None;
    }
    lz4_flex::decompress_size_prepended(data).ok()
}

/// Decompress an `FS_UPDATE`'s records buffer (for consumers that want the
/// records themselves, e.g. event display), with the standard guard.
pub fn fs_update_records(msg: &[u8]) -> Option<Vec<u8>> {
    if msg.len() < 8 || msg[0] != S2C_FS_UPDATE {
        return None;
    }
    decompress_guarded(&msg[8..])
}

/// Parse an `S2C_FS_FILE` message (starting at the opcode byte) into
/// `(nonce, status, data)`. Applies the same decompression guard as
/// [`FsMirror::apply_update`]; `None` = malformed or over-sized.
pub fn parse_fs_file(msg: &[u8]) -> Option<(u16, u8, Vec<u8>)> {
    if msg.len() < 4 || msg[0] != S2C_FS_FILE {
        return None;
    }
    let nonce = u16::from_le_bytes([msg[1], msg[2]]);
    let status = msg[3];
    let data = decompress_guarded(&msg[4..])?;
    Some((nonce, status, data))
}

// ---------------------------------------------------------------------------
// Write family (docs/design/fs-write.md): nonce request/response side-band
// operations against disk. The write itself echoes nothing — the existing
// per-client differ re-emits UPSERT/MOVE/DELETE once the reconciler
// re-indexes the landed change.
// ---------------------------------------------------------------------------

/// A content write (`C2S_FS_WRITE`). `base` is the CAS precondition: the
/// current on-disk content hash to match (non-zero), zero for
/// create-exclusive, ignored under `FS_WRITE_NO_CAS`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FsWrite {
    pub nonce: u16,
    pub sync_id: u16,
    pub flags: u8,
    pub base: u128,
    pub mode: u32,
    pub content_kind: u8,
    pub path: String,
    pub content: Vec<u8>,
}

pub fn msg_fs_write(w: &FsWrite) -> Vec<u8> {
    let pb = w.path.as_bytes();
    let compressed = lz4_flex::compress_prepend_size(&w.content);
    let mut msg = Vec::with_capacity(29 + pb.len() + compressed.len());
    msg.push(C2S_FS_WRITE);
    msg.extend_from_slice(&w.nonce.to_le_bytes());
    msg.extend_from_slice(&w.sync_id.to_le_bytes());
    msg.push(w.flags);
    msg.extend_from_slice(&w.base.to_le_bytes());
    msg.extend_from_slice(&w.mode.to_le_bytes());
    msg.push(w.content_kind);
    msg.extend_from_slice(&(pb.len() as u16).to_le_bytes());
    msg.extend_from_slice(pb);
    msg.extend_from_slice(&compressed);
    msg
}

/// Parse a `C2S_FS_WRITE`. `None` = malformed, non-UTF-8 path, or content
/// whose declared decompressed size exceeds the protocol cap.
pub fn parse_fs_write(msg: &[u8]) -> Option<FsWrite> {
    // [nonce:2][sync_id:2][flags:1][base:16][mode:4][content_kind:1][path_len:2][path:N][content:LZ4]
    if msg.len() < 29 || msg[0] != C2S_FS_WRITE {
        return None;
    }
    let nonce = u16::from_le_bytes([msg[1], msg[2]]);
    let sync_id = u16::from_le_bytes([msg[3], msg[4]]);
    let flags = msg[5];
    let base = u128::from_le_bytes(msg[6..22].try_into().unwrap());
    let mode = u32::from_le_bytes(msg[22..26].try_into().unwrap());
    let content_kind = msg[26];
    let path_len = u16::from_le_bytes([msg[27], msg[28]]) as usize;
    let path = std::str::from_utf8(msg.get(29..29 + path_len)?)
        .ok()?
        .to_string();
    let content = decompress_guarded(&msg[29 + path_len..])?;
    Some(FsWrite {
        nonce,
        sync_id,
        flags,
        base,
        mode,
        content_kind,
        path,
        content,
    })
}

/// A metadata op (`C2S_FS_OP`): `op` selects mkdir/remove/rename; `a` is
/// the primary path, `b` the rename destination. `base`/`mode` are used
/// by only some ops (like `LSP_QUERY`'s `line`/`col`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FsOp {
    pub nonce: u16,
    pub sync_id: u16,
    pub op: u8,
    pub flags: u8,
    pub base: u128,
    pub mode: u32,
    pub a: String,
    pub b: String,
}

pub fn msg_fs_op(o: &FsOp) -> Vec<u8> {
    let ab = o.a.as_bytes();
    let bb = o.b.as_bytes();
    let mut msg = Vec::with_capacity(29 + ab.len() + bb.len());
    msg.push(C2S_FS_OP);
    msg.extend_from_slice(&o.nonce.to_le_bytes());
    msg.extend_from_slice(&o.sync_id.to_le_bytes());
    msg.push(o.op);
    msg.push(o.flags);
    msg.extend_from_slice(&o.base.to_le_bytes());
    msg.extend_from_slice(&o.mode.to_le_bytes());
    msg.extend_from_slice(&(ab.len() as u16).to_le_bytes());
    msg.extend_from_slice(ab);
    msg.extend_from_slice(&(bb.len() as u16).to_le_bytes());
    msg.extend_from_slice(bb);
    msg
}

/// Parse a `C2S_FS_OP`. `None` = malformed or a non-UTF-8 path.
pub fn parse_fs_op(msg: &[u8]) -> Option<FsOp> {
    // [nonce:2][sync_id:2][op:1][flags:1][base:16][mode:4][a_len:2][a:N][b_len:2][b:N]
    if msg.len() < 29 || msg[0] != C2S_FS_OP {
        return None;
    }
    let nonce = u16::from_le_bytes([msg[1], msg[2]]);
    let sync_id = u16::from_le_bytes([msg[3], msg[4]]);
    let op = msg[5];
    let flags = msg[6];
    let base = u128::from_le_bytes(msg[7..23].try_into().unwrap());
    let mode = u32::from_le_bytes(msg[23..27].try_into().unwrap());
    let a_len = u16::from_le_bytes([msg[27], msg[28]]) as usize;
    let a = std::str::from_utf8(msg.get(29..29 + a_len)?)
        .ok()?
        .to_string();
    let b_off = 29 + a_len;
    let b_len = u16::from_le_bytes([*msg.get(b_off)?, *msg.get(b_off + 1)?]) as usize;
    let b = std::str::from_utf8(msg.get(b_off + 2..b_off + 2 + b_len)?)
        .ok()?
        .to_string();
    Some(FsOp {
        nonce,
        sync_id,
        op,
        flags,
        base,
        mode,
        a,
        b,
    })
}

/// Build an `S2C_FS_DONE`. On success `hash`/`mtime_ns` are the post-op
/// stat; on `CONFLICT`, `hash` is the current on-disk hash.
pub fn msg_fs_done(nonce: u16, status: u8, hash: u128, mtime_ns: u64) -> Vec<u8> {
    let mut msg = Vec::with_capacity(28);
    msg.push(S2C_FS_DONE);
    msg.extend_from_slice(&nonce.to_le_bytes());
    msg.push(status);
    msg.extend_from_slice(&hash.to_le_bytes());
    msg.extend_from_slice(&mtime_ns.to_le_bytes());
    msg
}

/// Parse an `S2C_FS_DONE` into `(nonce, status, hash, mtime_ns)`.
pub fn parse_fs_done(msg: &[u8]) -> Option<(u16, u8, u128, u64)> {
    // [nonce:2][status:1][hash:16][mtime_ns:8]
    if msg.len() < 28 || msg[0] != S2C_FS_DONE {
        return None;
    }
    let nonce = u16::from_le_bytes([msg[1], msg[2]]);
    let status = msg[3];
    let hash = u128::from_le_bytes(msg[4..20].try_into().unwrap());
    let mtime_ns = u64::from_le_bytes(msg[20..28].try_into().unwrap());
    Some((nonce, status, hash, mtime_ns))
}

/// The complete client obligation: apply updates, read `live`.
///
/// Paths are relative to the sync root, `/`-separated, "" = the root itself.
#[derive(Debug, Default)]
pub struct FsMirror {
    pub live: BTreeMap<String, FsNode>,
    staging: Option<BTreeMap<String, FsNode>>,
}

impl FsMirror {
    pub fn new() -> Self {
        Self::default()
    }

    /// Apply one `FS_UPDATE` message (starting at the opcode byte).
    /// Returns `Some(update_id)` to acknowledge, `None` if malformed.
    pub fn apply_update(&mut self, msg: &[u8]) -> Option<u32> {
        if msg.len() < 8 || msg[0] != S2C_FS_UPDATE {
            return None;
        }
        let update_id = u32::from_le_bytes([msg[3], msg[4], msg[5], msg[6]]);
        let flags = msg[7];
        let records = decompress_guarded(&msg[8..])?;
        if flags & FS_UPDATE_RESET != 0 {
            self.staging = Some(BTreeMap::new());
        }
        let map = self.staging.as_mut().unwrap_or(&mut self.live);
        for record in fs_records(&records) {
            match record {
                FsRecord::Upsert {
                    path,
                    entry_flags,
                    size,
                    mtime_ns,
                    mode,
                    hash,
                    content,
                } => {
                    let prev = map.get(path);
                    let content = match content {
                        FsContent::None => {
                            if entry_flags
                                & (FS_ENTRY_NO_CONTENT | FS_ENTRY_UNREADABLE | FS_ENTRY_UNSTABLE)
                                != 0
                            {
                                None
                            } else {
                                // Metadata-only upsert keeps previous content.
                                prev.and_then(|n| n.content.clone())
                            }
                        }
                        FsContent::Full(data) => Some(data.to_vec()),
                        FsContent::Delta(ops) => {
                            let base = prev.and_then(|n| n.content.as_deref()).unwrap_or(&[]);
                            Some(apply_fs_delta(base, ops)?)
                        }
                    };
                    map.insert(
                        path.to_string(),
                        FsNode {
                            entry_flags,
                            size,
                            mtime_ns,
                            mode,
                            hash,
                            content,
                        },
                    );
                }
                FsRecord::Delete { path } => {
                    remove_subtree(map, path);
                }
                FsRecord::Move { from, to } => {
                    let moved = take_subtree(map, from);
                    for (suffix, node) in moved {
                        let new_path = join_moved(to, &suffix);
                        map.insert(new_path, node);
                    }
                }
            }
        }
        if flags & FS_UPDATE_SYNC != 0
            && let Some(staged) = self.staging.take()
        {
            self.live = staged;
        }
        Some(update_id)
    }
}

fn is_under(path: &str, root: &str) -> bool {
    root.is_empty()
        || path == root
        || (path.len() > root.len()
            && path.starts_with(root)
            && path.as_bytes()[root.len()] == b'/')
}

fn remove_subtree(map: &mut BTreeMap<String, FsNode>, root: &str) {
    map.retain(|path, _| !is_under(path, root));
}

/// Remove and return `(suffix, node)` pairs for `root` and everything under
/// it. The suffix is "" for the root itself.
fn take_subtree(map: &mut BTreeMap<String, FsNode>, root: &str) -> Vec<(String, FsNode)> {
    let keys: Vec<String> = map.keys().filter(|p| is_under(p, root)).cloned().collect();
    keys.into_iter()
        .map(|key| {
            let node = map.remove(&key).unwrap();
            let suffix = if key.len() > root.len() {
                key[root.len() + if root.is_empty() { 0 } else { 1 }..].to_string()
            } else {
                String::new()
            };
            (suffix, node)
        })
        .collect()
}

fn join_moved(to: &str, suffix: &str) -> String {
    if suffix.is_empty() {
        to.to_string()
    } else if to.is_empty() {
        suffix.to_string()
    } else {
        format!("{to}/{suffix}")
    }
}

/// Apply a content delta (LEB128 COPY/INSERT instruction stream) to a base.
pub fn apply_fs_delta(base: &[u8], mut ops: &[u8]) -> Option<Vec<u8>> {
    fn leb128(data: &mut &[u8]) -> Option<u64> {
        let mut value = 0u64;
        let mut shift = 0u32;
        loop {
            let (&byte, rest) = data.split_first()?;
            *data = rest;
            if shift >= 64 {
                return None;
            }
            value |= u64::from(byte & 0x7F) << shift;
            if byte & 0x80 == 0 {
                return Some(value);
            }
            shift += 7;
        }
    }
    let mut out = Vec::new();
    while let Some((&op, rest)) = ops.split_first() {
        ops = rest;
        match op {
            0x01 => {
                let offset = leb128(&mut ops)? as usize;
                let len = leb128(&mut ops)? as usize;
                out.extend_from_slice(base.get(offset..offset.checked_add(len)?)?);
            }
            0x02 => {
                let len = leb128(&mut ops)? as usize;
                if ops.len() < len {
                    return None;
                }
                out.extend_from_slice(&ops[..len]);
                ops = &ops[len..];
            }
            _ => return None,
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn upsert(path: &str, content: &[u8]) -> Vec<u8> {
        let mut buf = Vec::new();
        append_fs_record(
            &mut buf,
            &FsRecord::Upsert {
                path,
                entry_flags: FS_ENTRY_FILE,
                size: content.len() as u64,
                mtime_ns: 42,
                mode: 0o644,
                hash: 7,
                content: FsContent::Full(content),
            },
        );
        buf
    }

    #[test]
    fn record_roundtrip() {
        let mut buf = Vec::new();
        append_fs_record(
            &mut buf,
            &FsRecord::Upsert {
                path: "a/b.txt",
                entry_flags: FS_ENTRY_FILE | FS_ENTRY_NO_CONTENT,
                size: 10,
                mtime_ns: 1_700_000_000_000_000_000,
                mode: 0o755,
                hash: 0xDEAD_BEEF_DEAD_BEEF_DEAD_BEEF,
                content: FsContent::None,
            },
        );
        append_fs_record(&mut buf, &FsRecord::Delete { path: "old" });
        append_fs_record(
            &mut buf,
            &FsRecord::Move {
                from: "src",
                to: "dst",
            },
        );
        let records: Vec<_> = fs_records(&buf).collect();
        assert_eq!(records.len(), 3);
        match &records[0] {
            FsRecord::Upsert {
                path,
                entry_flags,
                size,
                mtime_ns,
                mode,
                hash,
                content,
            } => {
                assert_eq!(*path, "a/b.txt");
                assert_eq!(*entry_flags, FS_ENTRY_FILE | FS_ENTRY_NO_CONTENT);
                assert_eq!(*size, 10);
                assert_eq!(*mtime_ns, 1_700_000_000_000_000_000);
                assert_eq!(*mode, 0o755);
                assert_eq!(*hash, 0xDEAD_BEEF_DEAD_BEEF_DEAD_BEEF);
                assert_eq!(*content, FsContent::None);
            }
            other => panic!("unexpected {other:?}"),
        }
        assert_eq!(records[1], FsRecord::Delete { path: "old" });
        assert_eq!(
            records[2],
            FsRecord::Move {
                from: "src",
                to: "dst"
            }
        );
    }

    /// Byte fixtures shared with the TypeScript codecs
    /// (`js/core/src/__tests__/fs.test.ts` pins the same hex), so codec
    /// drift fails on one side or the other. The compressed `FS_UPDATE`
    /// variant is pinned only in TS — LZ4 output may legitimately change
    /// across `lz4_flex` versions, while these buffers never can.
    #[test]
    fn wire_fixtures() {
        fn hex(b: &[u8]) -> String {
            b.iter().map(|x| format!("{x:02x}")).collect()
        }

        assert_eq!(
            hex(&msg_fs_sync(
                0x0102,
                FS_SYNC_RECURSIVE | FS_SYNC_CONTENT,
                25,
                65536,
                "/tmp/watch me"
            )),
            "400201031900000001000d002f746d702f7761746368206d65"
        );
        assert_eq!(hex(&msg_fs_stop(0x0102)), "410201");
        assert_eq!(hex(&msg_fs_ack(0x0102, 0x01020304)), "42020104030201");
        assert_eq!(
            hex(&msg_fs_fetch(3, 0x0102, "sub/%FF.bin")),
            "43030002010b007375622f2546462e62696e"
        );
        assert_eq!(
            hex(&msg_fs_synced(0x0102, 3, 0, "/w")),
            "40020103000002002f77"
        );

        let mut records = Vec::new();
        append_fs_record(
            &mut records,
            &FsRecord::Upsert {
                path: "a.txt",
                entry_flags: FS_ENTRY_FILE,
                size: 5,
                mtime_ns: 1_700_000_000_123_456_789,
                mode: 0o100644,
                hash: 0x0123_4567_89ab_cdef_1122_3344_5566_7788,
                content: FsContent::Full(b"hello"),
            },
        );
        append_fs_record(
            &mut records,
            &FsRecord::Upsert {
                path: "sub",
                entry_flags: FS_ENTRY_DIR,
                size: 0,
                mtime_ns: 0,
                mode: 0o40755,
                hash: 0,
                content: FsContent::None,
            },
        );
        append_fs_record(
            &mut records,
            &FsRecord::Upsert {
                path: "sub/%FF.bin", // server-escaped non-UTF-8 name
                entry_flags: FS_ENTRY_FILE | FS_ENTRY_NO_CONTENT,
                size: 1 << 20,
                mtime_ns: 1,
                mode: 0o100600,
                hash: 0xff,
                content: FsContent::None,
            },
        );
        append_fs_record(&mut records, &FsRecord::Delete { path: "old" });
        append_fs_record(
            &mut records,
            &FsRecord::Move {
                from: "src",
                to: "dst",
            },
        );
        assert_eq!(
            hex(&records),
            "3700000001000500612e747874050000000000000015cd853dfe9c9717a48100008877665544332211efcdab8967452301010500000068656c6c6f2c0000000101030073756200000000000000000000000000000000ed41000000000000000000000000000000000000003400000001080b007375622f2546462e62696e0000100000000000010000000000000080810000ff00000000000000000000000000000000060000000203006f6c640b0000000303007372630300647374"
        );

        // Decode direction: the pinned bytes parse back to the same records.
        let decoded: Vec<_> = fs_records(&records).collect();
        assert_eq!(decoded.len(), 5);
        assert!(matches!(
            &decoded[0],
            FsRecord::Upsert {
                path: "a.txt",
                size: 5,
                mtime_ns: 1_700_000_000_123_456_789,
                hash: 0x0123_4567_89ab_cdef_1122_3344_5566_7788,
                content: FsContent::Full(b"hello"),
                ..
            }
        ));
        assert_eq!(decoded[3], FsRecord::Delete { path: "old" });
        assert_eq!(
            decoded[4],
            FsRecord::Move {
                from: "src",
                to: "dst"
            }
        );
    }

    #[test]
    fn oversized_declared_length_is_rejected_before_allocation() {
        // A hand-forged FS_UPDATE whose LZ4 size prefix declares 1 GiB.
        let mut msg = vec![S2C_FS_UPDATE];
        msg.extend_from_slice(&1u16.to_le_bytes()); // sync_id
        msg.extend_from_slice(&1u32.to_le_bytes()); // update_id
        msg.push(0); // flags
        msg.extend_from_slice(&(1u32 << 30).to_le_bytes()); // declared size
        msg.extend_from_slice(&[0u8; 16]); // bogus compressed bytes
        let mut mirror = FsMirror::new();
        assert_eq!(mirror.apply_update(&msg), None);

        let mut file = vec![S2C_FS_FILE];
        file.extend_from_slice(&7u16.to_le_bytes()); // nonce
        file.push(FS_FILE_OK);
        file.extend_from_slice(&(1u32 << 30).to_le_bytes());
        file.extend_from_slice(&[0u8; 16]);
        assert_eq!(parse_fs_file(&file), None);
    }

    #[test]
    fn fs_file_roundtrip() {
        let msg = msg_fs_file(9, FS_FILE_OK, b"contents");
        assert_eq!(
            parse_fs_file(&msg),
            Some((9, FS_FILE_OK, b"contents".to_vec()))
        );
    }

    #[test]
    fn fs_write_roundtrip() {
        let w = FsWrite {
            nonce: 7,
            sync_id: 3,
            flags: FS_WRITE_MKPARENTS | FS_WRITE_DURABLE,
            base: 0x0123_4567_89ab_cdef_0123_4567_89ab_cdef,
            mode: 0o644,
            content_kind: FS_WRITE_CONTENT_FULL,
            path: "dir/50%25.txt".to_string(),
            content: b"hello world".to_vec(),
        };
        assert_eq!(parse_fs_write(&msg_fs_write(&w)), Some(w));
        // Empty content (create-empty) and zero base (create-exclusive).
        let w0 = FsWrite {
            nonce: 1,
            sync_id: 1,
            flags: 0,
            base: 0,
            mode: 0,
            content_kind: FS_WRITE_CONTENT_FULL,
            path: "new.txt".to_string(),
            content: Vec::new(),
        };
        assert_eq!(parse_fs_write(&msg_fs_write(&w0)), Some(w0));
        // Truncated header and wrong opcode are rejected.
        assert_eq!(parse_fs_write(&[C2S_FS_WRITE, 0, 0]), None);
        assert_eq!(parse_fs_write(&msg_fs_file(1, 0, b"x")), None);
    }

    #[test]
    fn fs_op_roundtrip() {
        let rename = FsOp {
            nonce: 42,
            sync_id: 9,
            op: FS_OP_RENAME,
            flags: FS_OP_MKPARENTS,
            base: 0,
            mode: 0,
            a: "old/name".to_string(),
            b: "new/name".to_string(),
        };
        assert_eq!(parse_fs_op(&msg_fs_op(&rename)), Some(rename));
        let mkdir = FsOp {
            nonce: 2,
            sync_id: 1,
            op: FS_OP_MKDIR,
            flags: 0,
            base: 0,
            mode: 0o700,
            a: "sub".to_string(),
            b: String::new(),
        };
        assert_eq!(parse_fs_op(&msg_fs_op(&mkdir)), Some(mkdir));
        assert_eq!(parse_fs_op(&[C2S_FS_OP, 0],), None);
    }

    #[test]
    fn fs_write_family_byte_fixtures() {
        // Pinned bytes, cross-checked with js/core/src/__tests__/fs.test.ts.
        let hex = |b: &[u8]| b.iter().map(|x| format!("{x:02x}")).collect::<String>();
        let w = FsWrite {
            nonce: 0x0102,
            sync_id: 0x0304,
            flags: FS_WRITE_MKPARENTS,
            base: 0x0f0e_0d0c_0b0a_0908_0706_0504_0302_0100,
            mode: 0o644,
            content_kind: FS_WRITE_CONTENT_FULL,
            path: "a/b.txt".into(),
            content: b"hi".to_vec(),
        };
        assert_eq!(
            hex(&msg_fs_write(&w)),
            "440201040302000102030405060708090a0b0c0d0e0fa4010000010700612f622e74787402000000206869"
        );
        let o = FsOp {
            nonce: 0x0102,
            sync_id: 0x0304,
            op: FS_OP_RENAME,
            flags: FS_OP_MKPARENTS,
            base: 0,
            mode: 0,
            a: "x".into(),
            b: "y".into(),
        };
        assert_eq!(
            hex(&msg_fs_op(&o)),
            "450201040303020000000000000000000000000000000000000000010078010079"
        );
        assert_eq!(
            hex(&msg_fs_done(
                0x0102,
                FS_DONE_CONFLICT,
                0x0f0e_0d0c_0b0a_0908_0706_0504_0302_0100,
                0x1122_3344_5566_7788
            )),
            "4402010b000102030405060708090a0b0c0d0e0f8877665544332211"
        );
    }

    #[test]
    fn fs_done_roundtrip() {
        let hash = 0xdead_beef_dead_beef_dead_beef_dead_beefu128;
        let msg = msg_fs_done(5, FS_DONE_OK, hash, 1_700_000_000_000_000_000);
        assert_eq!(
            parse_fs_done(&msg),
            Some((5, FS_DONE_OK, hash, 1_700_000_000_000_000_000))
        );
        // CONFLICT carries the current disk hash.
        let c = msg_fs_done(6, FS_DONE_CONFLICT, hash, 0);
        assert_eq!(parse_fs_done(&c), Some((6, FS_DONE_CONFLICT, hash, 0)));
    }

    #[test]
    fn unknown_record_kind_is_skipped() {
        let mut buf = Vec::new();
        // A future record kind 0x7F with 3 payload bytes.
        buf.extend_from_slice(&4u32.to_le_bytes());
        buf.push(0x7F);
        buf.extend_from_slice(&[1, 2, 3]);
        append_fs_record(&mut buf, &FsRecord::Delete { path: "x" });
        let records: Vec<_> = fs_records(&buf).collect();
        assert_eq!(records, vec![FsRecord::Delete { path: "x" }]);
    }

    #[test]
    fn mirror_staged_snapshot_and_live() {
        let mut mirror = FsMirror::new();
        // Snapshot: RESET+SYNC with two files.
        let mut records = upsert("a.txt", b"alpha");
        records.extend_from_slice(&upsert("d/b.txt", b"beta"));
        let msg = msg_fs_update(1, 1, FS_UPDATE_RESET | FS_UPDATE_SYNC, &records);
        assert_eq!(mirror.apply_update(&msg), Some(1));
        assert_eq!(mirror.live.len(), 2);
        assert_eq!(mirror.live["a.txt"].content.as_deref(), Some(&b"alpha"[..]));

        // Live delete + move.
        let mut records = Vec::new();
        append_fs_record(&mut records, &FsRecord::Delete { path: "a.txt" });
        append_fs_record(&mut records, &FsRecord::Move { from: "d", to: "e" });
        let msg = msg_fs_update(1, 2, 0, &records);
        assert_eq!(mirror.apply_update(&msg), Some(2));
        assert_eq!(mirror.live.len(), 1);
        assert_eq!(
            mirror.live["e/b.txt"].content.as_deref(),
            Some(&b"beta"[..])
        );

        // Mid-stream RESET without SYNC leaves live untouched…
        let msg = msg_fs_update(1, 3, FS_UPDATE_RESET, &upsert("n.txt", b"new"));
        assert_eq!(mirror.apply_update(&msg), Some(3));
        assert_eq!(mirror.live.len(), 1);
        // …until SYNC swaps atomically.
        let msg = msg_fs_update(1, 4, FS_UPDATE_SYNC, &[]);
        assert_eq!(mirror.apply_update(&msg), Some(4));
        assert_eq!(mirror.live.len(), 1);
        assert!(mirror.live.contains_key("n.txt"));
    }

    #[test]
    fn delta_content() {
        let mut mirror = FsMirror::new();
        let msg = msg_fs_update(
            1,
            1,
            FS_UPDATE_RESET | FS_UPDATE_SYNC,
            &upsert("f", b"hello world"),
        );
        mirror.apply_update(&msg).unwrap();

        // COPY(0,6) + INSERT("blit") == "hello blit"
        let ops: Vec<u8> = vec![0x01, 0, 6, 0x02, 4, b'b', b'l', b'i', b't'];
        let mut records = Vec::new();
        append_fs_record(
            &mut records,
            &FsRecord::Upsert {
                path: "f",
                entry_flags: FS_ENTRY_FILE,
                size: 10,
                mtime_ns: 43,
                mode: 0o644,
                hash: 8,
                content: FsContent::Delta(&ops),
            },
        );
        let msg = msg_fs_update(1, 2, 0, &records);
        mirror.apply_update(&msg).unwrap();
        assert_eq!(
            mirror.live["f"].content.as_deref(),
            Some(&b"hello blit"[..])
        );
    }

    #[test]
    fn subtree_semantics() {
        let mut map = BTreeMap::new();
        for p in ["a", "a/b", "a/b/c", "ab", "z"] {
            map.insert(
                p.to_string(),
                FsNode {
                    entry_flags: FS_ENTRY_FILE,
                    size: 0,
                    mtime_ns: 0,
                    mode: 0,
                    hash: 0,
                    content: None,
                },
            );
        }
        // "ab" must not match subtree "a".
        remove_subtree(&mut map, "a");
        let left: Vec<_> = map.keys().cloned().collect();
        assert_eq!(left, vec!["ab".to_string(), "z".to_string()]);
    }
}
