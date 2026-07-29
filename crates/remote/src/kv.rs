//! Server KV store wire protocol (docs/design/kv.md).
//!
//! A host-local key→value store with CAS writes and prefix-watch
//! subscriptions. Keys are raw UTF-8 (≤ [`KV_MAX_KEY`] bytes, no NUL,
//! non-empty); values are opaque bytes, LZ4 on the wire. CAS is
//! BLAKE3-128 over value bytes with the zero-hash absent sentinel,
//! [docs/design/fs-write.md]'s conflict model verbatim.
//!
//! All integers little-endian, tightly packed, as everywhere in the protocol.

use std::collections::BTreeMap;

/// Subscribe to a prefix: [0x70][nonce:2][flags:1][inline_max:4][prefix_len:2][prefix:N]
/// The prefix is a literal byte prefix (no glob); empty = whole store.
pub const C2S_KV_OPEN: u8 = 0x70;
/// Close a subscription: [0x71][kv_id:2]
pub const C2S_KV_STOP: u8 = 0x71;
/// Cumulative acknowledgement: [0x72][kv_id:2][update_id:4]
pub const C2S_KV_ACK: u8 = 0x72;
/// CAS put/delete: [0x73][nonce:2][flags:1][base:16][key_len:2][key:N][value:LZ4]
/// `base` is the CAS precondition on the current value bytes: non-zero =
/// match-or-CONFLICT, zero = create-exclusive, ignored under `KV_PUT_NO_CAS`.
pub const C2S_KV_PUT: u8 = 0x73;
/// Fetch one value: [0x74][nonce:2][key_len:2][key:N]
pub const C2S_KV_FETCH: u8 = 0x74;

/// Subscription accepted or refused: [0x70][nonce:2][kv_id:2][status:1][detail_len:2][detail:N]
pub const S2C_KV_OPENED: u8 = 0x70;
/// Snapshot/live records: [0x71][kv_id:2][update_id:4][flags:1][records:LZ4]
pub const S2C_KV_UPDATE: u8 = 0x71;
/// Put result: [0x72][nonce:2][status:1][hash:16][mtime_ns:8] — one per
/// `KV_PUT`. On success `hash` is the new value hash (zero for a delete);
/// on `CONFLICT` it carries the current hash so the client rebases without
/// a round trip.
pub const S2C_KV_DONE: u8 = 0x72;
/// Fetch result: [0x73][nonce:2][status:1][hash:16][data:LZ4]
pub const S2C_KV_VALUE: u8 = 0x73;
/// Subscription ended server-side: [0x74][kv_id:2][reason:1]
/// After it the `kv_id` is dead; a client that still wants the prefix
/// re-opens with `KV_OPEN` and receives a fresh snapshot — lossless,
/// because updates carry state, not events (docs/design/kv.md § Watch).
pub const S2C_KV_CLOSED: u8 = 0x74;

/// `S2C_HELLO` feature bit: server supports the `KV_*` family
/// (docs/design/kv.md). `BLIT_KV=0` refuses every `KV_*` at dispatch with
/// `PERMISSION` instead of un-advertising.
pub const FEATURE_KV: u32 = 1 << 9;

/// `kv_id` reported by a failed `KV_OPENED`.
pub const KV_ID_INVALID: u16 = 0xFFFF;

/// Maximum key length in bytes (fixed, not an env knob).
pub const KV_MAX_KEY: usize = 256;

// C2S_KV_PUT flags.
/// Ignore `base`; unconditional put/delete.
pub const KV_PUT_NO_CAS: u8 = 1 << 0;
/// Remove the entry (value must be empty). A delete is a put of absence;
/// `base` zero with DELETE is INVALID (delete-iff-absent is meaningless).
pub const KV_PUT_DELETE: u8 = 1 << 1;
/// fsync the store before replying; default trades durability for latency.
pub const KV_PUT_DURABLE: u8 = 1 << 2;

// S2C_KV_UPDATE flags.
/// This batch completes the initial snapshot; subsequent updates are live.
pub const KV_UPDATE_SNAPSHOT_END: u8 = 1 << 0;

// S2C_KV_CLOSED reasons — numbered as the fs family's closed table
// (docs/design/fs-watch.md § FS_CLOSED); 0-3 (client request, gone,
// permission lost, backend failed) are reserved until the store can
// produce them.
/// Queued-unacked bytes exceeded `BLIT_KV_UNACKED_MAX`
/// (docs/design/kv.md § Budgets); the subscription was dropped.
pub const KV_CLOSED_RESOURCE_LIMIT: u8 = 4;

// KV_DONE / KV_OPENED / KV_VALUE status — the unified git/lsp status table
// (docs/git.md "Statuses") plus fs-write's `11 CONFLICT`. Same numeric
// values as `FS_DONE_*` / `GIT_STATUS_*` where they overlap.
pub const KV_STATUS_OK: u8 = 0;
pub const KV_STATUS_NOT_FOUND: u8 = 2;
pub const KV_STATUS_PERMISSION: u8 = 4;
pub const KV_STATUS_TOO_LARGE: u8 = 5;
pub const KV_STATUS_BUDGET: u8 = 6;
pub const KV_STATUS_INVALID: u8 = 7;
pub const KV_STATUS_OTHER: u8 = 9;
/// A CAS precondition failed (mismatch, or create-exclusive on an existing
/// key). `KV_DONE.hash` carries the current value hash.
pub const KV_STATUS_CONFLICT: u8 = 11;

/// Human-readable name for a `KV_*` status code.
pub fn kv_status_text(status: u8) -> &'static str {
    match status {
        KV_STATUS_OK => "ok",
        KV_STATUS_NOT_FOUND => "not found",
        KV_STATUS_PERMISSION => "permission denied",
        KV_STATUS_TOO_LARGE => "too large",
        KV_STATUS_BUDGET => "budget exhausted",
        KV_STATUS_INVALID => "invalid request",
        KV_STATUS_CONFLICT => "conflict",
        _ => "error",
    }
}

/// Wire-key validity: non-empty UTF-8 (guaranteed by `&str`), ≤
/// [`KV_MAX_KEY`] bytes, no NUL.
pub fn kv_key_valid(key: &str) -> bool {
    !key.is_empty() && key.len() <= KV_MAX_KEY && !key.as_bytes().contains(&0)
}

// Record kinds inside KV_UPDATE.
pub const KV_RECORD_UPSERT: u8 = 0x01;
pub const KV_RECORD_DELETE: u8 = 0x02;

// UPSERT content kinds.
pub const KV_CONTENT_NONE: u8 = 0;
pub const KV_CONTENT_FULL: u8 = 1;

/// One decoded record from a `KV_UPDATE` payload.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum KvRecord<'a> {
    Upsert {
        key: &'a str,
        /// BLAKE3-128 of the value bytes.
        hash: u128,
        size: u32,
        mtime_ns: u64,
        /// Inline value iff `size` ≤ the subscription's `inline_max`;
        /// `None` = fetch on demand.
        value: Option<&'a [u8]>,
    },
    Delete {
        key: &'a str,
    },
}

/// Append one record to an uncompressed `KV_UPDATE` records buffer.
pub fn append_kv_record(buf: &mut Vec<u8>, record: &KvRecord<'_>) {
    let start = buf.len();
    buf.extend_from_slice(&0u32.to_le_bytes()); // record_len placeholder
    match record {
        KvRecord::Upsert {
            key,
            hash,
            size,
            mtime_ns,
            value,
        } => {
            buf.push(KV_RECORD_UPSERT);
            let kb = key.as_bytes();
            buf.extend_from_slice(&(kb.len() as u16).to_le_bytes());
            buf.extend_from_slice(kb);
            buf.extend_from_slice(&hash.to_le_bytes());
            buf.extend_from_slice(&size.to_le_bytes());
            buf.extend_from_slice(&mtime_ns.to_le_bytes());
            match value {
                None => buf.push(KV_CONTENT_NONE),
                Some(data) => {
                    buf.push(KV_CONTENT_FULL);
                    buf.extend_from_slice(&(data.len() as u32).to_le_bytes());
                    buf.extend_from_slice(data);
                }
            }
        }
        KvRecord::Delete { key } => {
            buf.push(KV_RECORD_DELETE);
            let kb = key.as_bytes();
            buf.extend_from_slice(&(kb.len() as u16).to_le_bytes());
            buf.extend_from_slice(kb);
        }
    }
    let len = (buf.len() - start - 4) as u32;
    buf[start..start + 4].copy_from_slice(&len.to_le_bytes());
}

/// Iterate records in an uncompressed `KV_UPDATE` payload. Unknown kinds
/// are skipped via `record_len`; a malformed record ends iteration
/// (forward-compatible with future record extensions, the fs rule).
pub struct KvRecordIter<'a> {
    data: &'a [u8],
}

pub fn kv_records(data: &[u8]) -> KvRecordIter<'_> {
    KvRecordIter { data }
}

fn take_key<'a>(body: &mut &'a [u8]) -> Option<&'a str> {
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

impl<'a> Iterator for KvRecordIter<'a> {
    type Item = KvRecord<'a>;

    fn next(&mut self) -> Option<KvRecord<'a>> {
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
                KV_RECORD_UPSERT => {
                    let key = take_key(&mut body)?;
                    if body.len() < 16 + 4 + 8 + 1 {
                        return None;
                    }
                    let hash = u128::from_le_bytes(body[0..16].try_into().unwrap());
                    let size = u32::from_le_bytes(body[16..20].try_into().unwrap());
                    let mtime_ns = u64::from_le_bytes(body[20..28].try_into().unwrap());
                    let content_kind = body[28];
                    body = &body[29..];
                    let value = match content_kind {
                        KV_CONTENT_NONE => None,
                        KV_CONTENT_FULL => {
                            if body.len() < 4 {
                                return None;
                            }
                            let len = u32::from_le_bytes(body[0..4].try_into().unwrap()) as usize;
                            if body.len() < 4 + len {
                                return None;
                            }
                            Some(&body[4..4 + len])
                        }
                        _ => return None,
                    };
                    return Some(KvRecord::Upsert {
                        key,
                        hash,
                        size,
                        mtime_ns,
                        value,
                    });
                }
                KV_RECORD_DELETE => {
                    let key = take_key(&mut body)?;
                    return Some(KvRecord::Delete { key });
                }
                _ => continue, // unknown kind: skip via record_len
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Message builders and parsers
// ---------------------------------------------------------------------------

pub fn msg_kv_open(nonce: u16, flags: u8, inline_max: u32, prefix: &str) -> Vec<u8> {
    let pb = prefix.as_bytes();
    let mut msg = Vec::with_capacity(10 + pb.len());
    msg.push(C2S_KV_OPEN);
    msg.extend_from_slice(&nonce.to_le_bytes());
    msg.push(flags);
    msg.extend_from_slice(&inline_max.to_le_bytes());
    msg.extend_from_slice(&(pb.len() as u16).to_le_bytes());
    msg.extend_from_slice(pb);
    msg
}

/// Parse a `C2S_KV_OPEN` → `(nonce, flags, inline_max, prefix)`.
pub fn parse_kv_open(msg: &[u8]) -> Option<(u16, u8, u32, String)> {
    if msg.len() < 10 || msg[0] != C2S_KV_OPEN {
        return None;
    }
    let nonce = u16::from_le_bytes([msg[1], msg[2]]);
    let flags = msg[3];
    let inline_max = u32::from_le_bytes(msg[4..8].try_into().unwrap());
    let prefix_len = u16::from_le_bytes([msg[8], msg[9]]) as usize;
    let prefix = std::str::from_utf8(msg.get(10..10 + prefix_len)?)
        .ok()?
        .to_string();
    Some((nonce, flags, inline_max, prefix))
}

pub fn msg_kv_stop(kv_id: u16) -> Vec<u8> {
    let mut msg = Vec::with_capacity(3);
    msg.push(C2S_KV_STOP);
    msg.extend_from_slice(&kv_id.to_le_bytes());
    msg
}

/// Parse a `C2S_KV_STOP` → `kv_id`.
pub fn parse_kv_stop(msg: &[u8]) -> Option<u16> {
    if msg.len() < 3 || msg[0] != C2S_KV_STOP {
        return None;
    }
    Some(u16::from_le_bytes([msg[1], msg[2]]))
}

pub fn msg_kv_ack(kv_id: u16, update_id: u32) -> Vec<u8> {
    let mut msg = Vec::with_capacity(7);
    msg.push(C2S_KV_ACK);
    msg.extend_from_slice(&kv_id.to_le_bytes());
    msg.extend_from_slice(&update_id.to_le_bytes());
    msg
}

/// Parse a `C2S_KV_ACK` → `(kv_id, update_id)`.
pub fn parse_kv_ack(msg: &[u8]) -> Option<(u16, u32)> {
    if msg.len() < 7 || msg[0] != C2S_KV_ACK {
        return None;
    }
    let kv_id = u16::from_le_bytes([msg[1], msg[2]]);
    let update_id = u32::from_le_bytes(msg[3..7].try_into().unwrap());
    Some((kv_id, update_id))
}

/// A CAS put or delete (`C2S_KV_PUT`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct KvPut {
    pub nonce: u16,
    pub flags: u8,
    pub base: u128,
    pub key: String,
    pub value: Vec<u8>,
}

pub fn msg_kv_put(p: &KvPut) -> Vec<u8> {
    let kb = p.key.as_bytes();
    let compressed = lz4_flex::compress_prepend_size(&p.value);
    let mut msg = Vec::with_capacity(22 + kb.len() + compressed.len());
    msg.push(C2S_KV_PUT);
    msg.extend_from_slice(&p.nonce.to_le_bytes());
    msg.push(p.flags);
    msg.extend_from_slice(&p.base.to_le_bytes());
    msg.extend_from_slice(&(kb.len() as u16).to_le_bytes());
    msg.extend_from_slice(kb);
    msg.extend_from_slice(&compressed);
    msg
}

/// Parse a `C2S_KV_PUT`. `None` = malformed, a non-UTF-8 key, or a value
/// whose declared decompressed size exceeds the protocol cap.
/// The decompressed size a `C2S_KV_PUT` claims for its value, read without
/// inflating it.
///
/// `decompress_size_prepended` allocates the declared size up front, so a
/// server whose own `value_max` is tighter than [`crate::MAX_DECOMPRESSED`]
/// can refuse the put before it costs anything. Otherwise rejecting a
/// 4 MiB-limit violation paid a 64 MiB allocation first — a sixteenfold
/// amplification available to any client, one message at a time.
pub fn kv_put_declared_value_len(msg: &[u8]) -> Option<usize> {
    if msg.len() < 22 || msg[0] != C2S_KV_PUT {
        return None;
    }
    let key_len = u16::from_le_bytes([msg[20], msg[21]]) as usize;
    let value = msg.get(22 + key_len..)?;
    let head = value.get(0..4)?;
    Some(u32::from_le_bytes(head.try_into().unwrap()) as usize)
}

pub fn parse_kv_put(msg: &[u8]) -> Option<KvPut> {
    // [nonce:2][flags:1][base:16][key_len:2][key:N][value:LZ4]
    if msg.len() < 22 || msg[0] != C2S_KV_PUT {
        return None;
    }
    let nonce = u16::from_le_bytes([msg[1], msg[2]]);
    let flags = msg[3];
    let base = u128::from_le_bytes(msg[4..20].try_into().unwrap());
    let key_len = u16::from_le_bytes([msg[20], msg[21]]) as usize;
    let key = std::str::from_utf8(msg.get(22..22 + key_len)?)
        .ok()?
        .to_string();
    let value = decompress_guarded(&msg[22 + key_len..])?;
    Some(KvPut {
        nonce,
        flags,
        base,
        key,
        value,
    })
}

pub fn msg_kv_fetch(nonce: u16, key: &str) -> Vec<u8> {
    let kb = key.as_bytes();
    let mut msg = Vec::with_capacity(5 + kb.len());
    msg.push(C2S_KV_FETCH);
    msg.extend_from_slice(&nonce.to_le_bytes());
    msg.extend_from_slice(&(kb.len() as u16).to_le_bytes());
    msg.extend_from_slice(kb);
    msg
}

/// Parse a `C2S_KV_FETCH` → `(nonce, key)`.
pub fn parse_kv_fetch(msg: &[u8]) -> Option<(u16, String)> {
    if msg.len() < 5 || msg[0] != C2S_KV_FETCH {
        return None;
    }
    let nonce = u16::from_le_bytes([msg[1], msg[2]]);
    let key_len = u16::from_le_bytes([msg[3], msg[4]]) as usize;
    let key = std::str::from_utf8(msg.get(5..5 + key_len)?)
        .ok()?
        .to_string();
    Some((nonce, key))
}

pub fn msg_kv_opened(nonce: u16, kv_id: u16, status: u8, detail: &str) -> Vec<u8> {
    let db = detail.as_bytes();
    let mut msg = Vec::with_capacity(8 + db.len());
    msg.push(S2C_KV_OPENED);
    msg.extend_from_slice(&nonce.to_le_bytes());
    msg.extend_from_slice(&kv_id.to_le_bytes());
    msg.push(status);
    msg.extend_from_slice(&(db.len() as u16).to_le_bytes());
    msg.extend_from_slice(db);
    msg
}

/// Parse an `S2C_KV_OPENED` → `(nonce, kv_id, status, detail)`.
pub fn parse_kv_opened(msg: &[u8]) -> Option<(u16, u16, u8, String)> {
    if msg.len() < 8 || msg[0] != S2C_KV_OPENED {
        return None;
    }
    let nonce = u16::from_le_bytes([msg[1], msg[2]]);
    let kv_id = u16::from_le_bytes([msg[3], msg[4]]);
    let status = msg[5];
    let detail_len = u16::from_le_bytes([msg[6], msg[7]]) as usize;
    let detail = String::from_utf8_lossy(msg.get(8..8 + detail_len)?).into_owned();
    Some((nonce, kv_id, status, detail))
}

/// Build a `KV_UPDATE` from an uncompressed records buffer.
pub fn msg_kv_update(kv_id: u16, update_id: u32, flags: u8, records: &[u8]) -> Vec<u8> {
    let compressed = lz4_flex::compress_prepend_size(records);
    let mut msg = Vec::with_capacity(8 + compressed.len());
    msg.push(S2C_KV_UPDATE);
    msg.extend_from_slice(&kv_id.to_le_bytes());
    msg.extend_from_slice(&update_id.to_le_bytes());
    msg.push(flags);
    msg.extend_from_slice(&compressed);
    msg
}

/// Build an `S2C_KV_DONE`. On success `hash` is the new value hash (zero
/// for a delete); on `CONFLICT` the current hash.
pub fn msg_kv_done(nonce: u16, status: u8, hash: u128, mtime_ns: u64) -> Vec<u8> {
    let mut msg = Vec::with_capacity(28);
    msg.push(S2C_KV_DONE);
    msg.extend_from_slice(&nonce.to_le_bytes());
    msg.push(status);
    msg.extend_from_slice(&hash.to_le_bytes());
    msg.extend_from_slice(&mtime_ns.to_le_bytes());
    msg
}

/// Parse an `S2C_KV_DONE` → `(nonce, status, hash, mtime_ns)`.
pub fn parse_kv_done(msg: &[u8]) -> Option<(u16, u8, u128, u64)> {
    if msg.len() < 28 || msg[0] != S2C_KV_DONE {
        return None;
    }
    let nonce = u16::from_le_bytes([msg[1], msg[2]]);
    let status = msg[3];
    let hash = u128::from_le_bytes(msg[4..20].try_into().unwrap());
    let mtime_ns = u64::from_le_bytes(msg[20..28].try_into().unwrap());
    Some((nonce, status, hash, mtime_ns))
}

pub fn msg_kv_value(nonce: u16, status: u8, hash: u128, data: &[u8]) -> Vec<u8> {
    let compressed = lz4_flex::compress_prepend_size(data);
    let mut msg = Vec::with_capacity(20 + compressed.len());
    msg.push(S2C_KV_VALUE);
    msg.extend_from_slice(&nonce.to_le_bytes());
    msg.push(status);
    msg.extend_from_slice(&hash.to_le_bytes());
    msg.extend_from_slice(&compressed);
    msg
}

/// Parse an `S2C_KV_VALUE` → `(nonce, status, hash, data)`.
pub fn parse_kv_value(msg: &[u8]) -> Option<(u16, u8, u128, Vec<u8>)> {
    if msg.len() < 20 || msg[0] != S2C_KV_VALUE {
        return None;
    }
    let nonce = u16::from_le_bytes([msg[1], msg[2]]);
    let status = msg[3];
    let hash = u128::from_le_bytes(msg[4..20].try_into().unwrap());
    let data = decompress_guarded(&msg[20..])?;
    Some((nonce, status, hash, data))
}

pub fn msg_kv_closed(kv_id: u16, reason: u8) -> Vec<u8> {
    let mut msg = Vec::with_capacity(4);
    msg.push(S2C_KV_CLOSED);
    msg.extend_from_slice(&kv_id.to_le_bytes());
    msg.push(reason);
    msg
}

/// Parse an `S2C_KV_CLOSED` → `(kv_id, reason)`.
pub fn parse_kv_closed(msg: &[u8]) -> Option<(u16, u8)> {
    if msg.len() < 4 || msg[0] != S2C_KV_CLOSED {
        return None;
    }
    Some((u16::from_le_bytes([msg[1], msg[2]]), msg[3]))
}

/// Decompress a `compress_prepend_size` payload, refusing declared sizes
/// over the protocol-wide [`crate::MAX_DECOMPRESSED`] cap.
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
// Client-side reducer
// ---------------------------------------------------------------------------

/// One mirrored entry.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct KvEntry {
    /// BLAKE3-128 of the value bytes.
    pub hash: u128,
    pub size: u32,
    pub mtime_ns: u64,
    /// Present iff the value arrived inline (`size` ≤ the subscription's
    /// `inline_max`); `None` = fetch on demand.
    pub value: Option<Vec<u8>>,
}

/// The complete watcher obligation: apply updates, read `live`.
///
/// One mirror per subscription; a re-established connection means a new
/// `KV_OPEN`, a new `kv_id`, and a fresh mirror (nothing survives, the
/// fs-family rule).
#[derive(Debug, Default)]
pub struct KvMirror {
    pub live: BTreeMap<String, KvEntry>,
    /// True once the initial snapshot is complete (`KV_UPDATE_SNAPSHOT_END`).
    pub snapshot_done: bool,
}

impl KvMirror {
    pub fn new() -> Self {
        Self::default()
    }

    /// Apply one `KV_UPDATE` message (starting at the opcode byte).
    /// Returns `Some(update_id)` to acknowledge, `None` if malformed.
    pub fn apply_update(&mut self, msg: &[u8]) -> Option<u32> {
        if msg.len() < 8 || msg[0] != S2C_KV_UPDATE {
            return None;
        }
        let update_id = u32::from_le_bytes([msg[3], msg[4], msg[5], msg[6]]);
        let flags = msg[7];
        let records = decompress_guarded(&msg[8..])?;
        for record in kv_records(&records) {
            match record {
                KvRecord::Upsert {
                    key,
                    hash,
                    size,
                    mtime_ns,
                    value,
                } => {
                    self.live.insert(
                        key.to_string(),
                        KvEntry {
                            hash,
                            size,
                            mtime_ns,
                            value: value.map(|v| v.to_vec()),
                        },
                    );
                }
                KvRecord::Delete { key } => {
                    self.live.remove(key);
                }
            }
        }
        if flags & KV_UPDATE_SNAPSHOT_END != 0 {
            self.snapshot_done = true;
        }
        Some(update_id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kv_open_roundtrip_and_bytes() {
        let m = msg_kv_open(7, 0, 4096, "editor/");
        // Lock the byte layout: [0x70][nonce:2][flags:1][inline_max:4][prefix_len:2][prefix]
        assert_eq!(
            m,
            vec![
                0x70, 0x07, 0x00, 0x00, 0x00, 0x10, 0x00, 0x00, 0x07, 0x00, 0x65, 0x64, 0x69, 0x74,
                0x6F, 0x72, 0x2F
            ]
        );
        let (nonce, flags, inline_max, prefix) = parse_kv_open(&m).unwrap();
        assert_eq!(nonce, 7);
        assert_eq!(flags, 0);
        assert_eq!(inline_max, 4096);
        assert_eq!(prefix, "editor/");
    }

    #[test]
    fn kv_stop_ack_roundtrip() {
        assert_eq!(parse_kv_stop(&msg_kv_stop(3)), Some(3));
        assert_eq!(parse_kv_ack(&msg_kv_ack(3, 99)), Some((3, 99)));
    }

    #[test]
    fn kv_put_roundtrip() {
        let p = KvPut {
            nonce: 21,
            flags: KV_PUT_DURABLE,
            base: 0xDEAD_BEEF,
            key: "roots".to_string(),
            value: b"main = /src/blit\n".to_vec(),
        };
        let out = parse_kv_put(&msg_kv_put(&p)).unwrap();
        assert_eq!(out, p);
    }

    #[test]
    fn kv_put_delete_empty_value() {
        let p = KvPut {
            nonce: 1,
            flags: KV_PUT_DELETE,
            base: 42,
            key: "editor/buf//tmp/x".to_string(),
            value: Vec::new(),
        };
        let out = parse_kv_put(&msg_kv_put(&p)).unwrap();
        assert_eq!(out, p);
    }

    #[test]
    fn kv_fetch_roundtrip() {
        let m = msg_kv_fetch(5, "editor/open//x/y.rs");
        assert_eq!(parse_kv_fetch(&m), Some((5, "editor/open//x/y.rs".into())));
    }

    #[test]
    fn kv_opened_roundtrip() {
        let m = msg_kv_opened(9, 2, KV_STATUS_OK, "");
        assert_eq!(parse_kv_opened(&m), Some((9, 2, KV_STATUS_OK, "".into())));
        let m = msg_kv_opened(9, KV_ID_INVALID, KV_STATUS_PERMISSION, "kv disabled");
        assert_eq!(
            parse_kv_opened(&m),
            Some((9, KV_ID_INVALID, KV_STATUS_PERMISSION, "kv disabled".into()))
        );
    }

    #[test]
    fn kv_closed_roundtrip_and_bytes() {
        let m = msg_kv_closed(3, KV_CLOSED_RESOURCE_LIMIT);
        // Lock the byte layout: [0x74][kv_id:2][reason:1]
        assert_eq!(m, vec![0x74, 0x03, 0x00, 0x04]);
        assert_eq!(parse_kv_closed(&m), Some((3, KV_CLOSED_RESOURCE_LIMIT)));
        assert_eq!(parse_kv_closed(&m[..3]), None);
        assert_eq!(parse_kv_closed(&msg_kv_stop(3)), None);
    }

    #[test]
    fn kv_done_value_roundtrip() {
        let m = msg_kv_done(4, KV_STATUS_CONFLICT, 77, 123_456);
        assert_eq!(
            parse_kv_done(&m),
            Some((4, KV_STATUS_CONFLICT, 77, 123_456))
        );
        let m = msg_kv_value(6, KV_STATUS_OK, 88, b"payload");
        assert_eq!(
            parse_kv_value(&m),
            Some((6, KV_STATUS_OK, 88, b"payload".to_vec()))
        );
    }

    #[test]
    fn record_roundtrip_and_mirror() {
        let mut buf = Vec::new();
        append_kv_record(
            &mut buf,
            &KvRecord::Upsert {
                key: "editor/open//a.rs",
                hash: 11,
                size: 2,
                mtime_ns: 5,
                value: Some(b"{}"),
            },
        );
        append_kv_record(
            &mut buf,
            &KvRecord::Upsert {
                key: "editor/buf//a.rs",
                hash: 12,
                size: 9_999_999,
                mtime_ns: 6,
                value: None, // over inline_max: metadata only
            },
        );
        append_kv_record(&mut buf, &KvRecord::Delete { key: "roots" });
        let records: Vec<_> = kv_records(&buf).collect();
        assert_eq!(records.len(), 3);
        assert_eq!(
            records[0],
            KvRecord::Upsert {
                key: "editor/open//a.rs",
                hash: 11,
                size: 2,
                mtime_ns: 5,
                value: Some(b"{}"),
            }
        );

        let mut mirror = KvMirror::new();
        let msg = msg_kv_update(1, 10, KV_UPDATE_SNAPSHOT_END, &buf);
        assert_eq!(mirror.apply_update(&msg), Some(10));
        assert!(mirror.snapshot_done);
        assert_eq!(mirror.live.len(), 2);
        assert_eq!(
            mirror.live.get("editor/open//a.rs").unwrap().value,
            Some(b"{}".to_vec())
        );
        assert_eq!(mirror.live.get("editor/buf//a.rs").unwrap().value, None);

        // A live delete removes the entry.
        let mut buf2 = Vec::new();
        append_kv_record(
            &mut buf2,
            &KvRecord::Delete {
                key: "editor/open//a.rs",
            },
        );
        let msg2 = msg_kv_update(1, 11, 0, &buf2);
        assert_eq!(mirror.apply_update(&msg2), Some(11));
        assert_eq!(mirror.live.len(), 1);
    }

    #[test]
    fn unknown_record_kind_skipped() {
        let mut buf = Vec::new();
        // A future record kind (0x7F) with a 3-byte body.
        buf.extend_from_slice(&4u32.to_le_bytes());
        buf.push(0x7F);
        buf.extend_from_slice(&[1, 2, 3]);
        append_kv_record(&mut buf, &KvRecord::Delete { key: "k" });
        let records: Vec<_> = kv_records(&buf).collect();
        assert_eq!(records, vec![KvRecord::Delete { key: "k" }]);
    }

    #[test]
    fn key_validity() {
        assert!(kv_key_valid("roots"));
        assert!(kv_key_valid("editor/buf//x/y.rs"));
        assert!(!kv_key_valid(""));
        assert!(!kv_key_valid(&"k".repeat(KV_MAX_KEY + 1)));
        assert!(!kv_key_valid("a\0b"));
    }
}
