//! Server KV store (docs/design/kv.md): a host-local key→value map with
//! CAS puts/deletes, prefix-watch subscriptions, and a redb write-behind.
//!
//! The in-memory map is the source of truth for CAS and watches; redb is
//! its write-behind on a dedicated writer thread, fed in mutation order
//! from under the store lock (`Durability::Immediate` under
//! `KV_PUT_DURABLE`, `Eventual` otherwise). Jobs queued behind one another
//! batch into a single transaction, so a `DURABLE` put's fsynced commit
//! covers every mutation ordered before it and crash durability stays
//! prefix-consistent with the mutation order. Non-durable puts are acked
//! as soon as the in-memory mutation lands; a `DURABLE` put's `KV_DONE`
//! (and its own echo) wait for the commit to confirm.

use std::collections::{BTreeMap, VecDeque};
use std::sync::atomic::{AtomicU32, AtomicUsize, Ordering};
use std::sync::{Arc, LazyLock, Mutex, OnceLock, Weak};

use blit_remote::kv::{
    C2S_KV_ACK, C2S_KV_FETCH, C2S_KV_OPEN, C2S_KV_PUT, C2S_KV_STOP, KV_CLOSED_RESOURCE_LIMIT,
    KV_ID_INVALID, KV_PUT_DELETE, KV_PUT_DURABLE, KV_PUT_NO_CAS, KV_STATUS_BUDGET,
    KV_STATUS_CONFLICT, KV_STATUS_INVALID, KV_STATUS_NOT_FOUND, KV_STATUS_OK, KV_STATUS_OTHER,
    KV_STATUS_TOO_LARGE, KV_UPDATE_SNAPSHOT_END, KvRecord, append_kv_record, kv_key_valid,
    msg_kv_closed, msg_kv_done, msg_kv_opened, msg_kv_update, msg_kv_value, parse_kv_ack,
    parse_kv_fetch, parse_kv_open, parse_kv_put, parse_kv_stop,
};
use redb::ReadableTable;
use rustc_hash::FxHashMap;
use tokio::sync::mpsc;

const TABLE: redb::TableDefinition<&str, &[u8]> = redb::TableDefinition::new("kv");

/// Snapshot updates chunk at this many record bytes so one subscription of
/// a large store never rides a single giant frame (non-WebSocket transports
/// cap frames at 16 MiB; docs/protocol.md).
const SNAPSHOT_CHUNK: usize = 2 * 1024 * 1024;

fn env_budget(name: &str, default: u64) -> u64 {
    std::env::var(name)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

// Budgets (docs/design/kv.md § Budgets), read once: these sit on the
// per-message hot path, where a getenv per PUT is pure overhead.
fn value_max() -> u64 {
    static V: LazyLock<u64> = LazyLock::new(|| env_budget("BLIT_KV_VALUE_MAX", 4 * 1024 * 1024));
    *V
}
fn total_max() -> u64 {
    static V: LazyLock<u64> = LazyLock::new(|| env_budget("BLIT_KV_TOTAL_MAX", 256 * 1024 * 1024));
    *V
}
fn max_entries() -> u64 {
    static V: LazyLock<u64> = LazyLock::new(|| env_budget("BLIT_KV_MAX_ENTRIES", 16384));
    *V
}
fn max_subs() -> usize {
    static V: LazyLock<usize> = LazyLock::new(|| env_budget("BLIT_KV_MAX_SUBS", 16) as usize);
    *V
}
fn max_inflight() -> usize {
    static V: LazyLock<usize> = LazyLock::new(|| env_budget("BLIT_KV_INFLIGHT", 16) as usize);
    *V
}
fn unacked_max() -> u64 {
    static V: LazyLock<u64> = LazyLock::new(|| env_budget("BLIT_KV_UNACKED_MAX", 16 * 1024 * 1024));
    *V
}

fn now_ns() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::SystemTime::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0)
}

/// `$BLIT_KV_PATH`, else the platform state path (docs/design/kv.md
/// § Storage): `$XDG_STATE_HOME/blit/kv.redb` (`~/.local/state` fallback)
/// on Unix, `~/Library/Application Support/blit/kv.redb` on macOS,
/// `%APPDATA%\blit\kv.redb` on Windows. `None` = no resolvable home.
fn db_path() -> Option<std::path::PathBuf> {
    if let Some(p) = std::env::var_os("BLIT_KV_PATH") {
        return Some(std::path::PathBuf::from(p));
    }
    #[cfg(target_os = "macos")]
    let base = std::env::var_os("HOME")
        .map(|h| std::path::PathBuf::from(h).join("Library/Application Support"));
    #[cfg(all(unix, not(target_os = "macos")))]
    let base = std::env::var_os("XDG_STATE_HOME")
        .map(std::path::PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME").map(|h| std::path::PathBuf::from(h).join(".local/state"))
        });
    #[cfg(windows)]
    let base = std::env::var_os("APPDATA").map(std::path::PathBuf::from);
    base.map(|b| b.join("blit").join("kv.redb"))
}

struct Entry {
    value: Arc<Vec<u8>>,
    hash: u128,
    mtime_ns: u64,
}

/// Reply route for a `DURABLE` put, carried by its write job: `KV_DONE`
/// is sent only once the fsynced commit confirms. Dropping it releases
/// the put's in-flight slot (docs/design/kv.md § Budgets).
struct DurableReply {
    out: mpsc::UnboundedSender<Vec<u8>>,
    nonce: u16,
    hash: u128,
    mtime_ns: u64,
    pending: Arc<AtomicUsize>,
}

impl Drop for DurableReply {
    fn drop(&mut self) {
        self.pending.fetch_sub(1, Ordering::Relaxed);
    }
}

/// One mutation for the writer thread. Enqueued under the store lock, so
/// the channel order is the mutation order.
struct WriteJob {
    key: String,
    /// `None` = delete; the `Arc` shares bytes with the live entry.
    value: Option<(Arc<Vec<u8>>, u64)>,
    /// `Some` marks a `DURABLE` put: commit `Immediate`, reply deferred.
    reply: Option<DurableReply>,
}

/// A subscription starts `Snapshotting`: mutations broadcast while its
/// snapshot is encoded off-lock queue here (update ids unassigned) and
/// flush after the snapshot chunks, so wire order equals mutation order
/// with strictly increasing update ids.
enum SubPhase {
    Snapshotting(Vec<Vec<u8>>),
    Live,
}

/// Send-side state, one lock: the phase buffer plus the retention window
/// `C2S_KV_ACK` drains (docs/design/kv.md § Watch).
struct SubState {
    phase: SubPhase,
    /// `(update_id, wire bytes)` per queued-unacked update, id-ascending;
    /// the cumulative acked floor drains it from the front.
    unacked: VecDeque<(u32, u64)>,
    unacked_bytes: u64,
    /// Budget breached: `KV_CLOSED` sent, nothing further queues. The
    /// connection task sweeps closed subs out of its map on `KV_OPEN`.
    closed: bool,
}

impl SubState {
    fn new() -> Self {
        SubState {
            phase: SubPhase::Snapshotting(Vec::new()),
            unacked: VecDeque::new(),
            unacked_bytes: 0,
            closed: false,
        }
    }
}

/// One live prefix subscription; owned by a connection's [`KvSubs`], the
/// store holds a `Weak` and prunes on broadcast and on `KV_OPEN`.
struct SubEntry {
    kv_id: u16,
    prefix: String,
    inline_max: u32,
    /// Queued-unacked byte budget, captured from `BLIT_KV_UNACKED_MAX` at
    /// open (docs/design/kv.md § Budgets).
    unacked_max: u64,
    out: mpsc::UnboundedSender<Vec<u8>>,
    update_id: AtomicU32,
    state: Mutex<SubState>,
}

impl SubEntry {
    fn next_update(&self) -> u32 {
        self.update_id.fetch_add(1, Ordering::Relaxed) + 1
    }

    fn inline_limit(&self) -> u64 {
        // inline_max 0 = no limit (docs/design/kv.md: values are already
        // bounded by the per-value cap, so "default" means inline all).
        if self.inline_max == 0 {
            u64::MAX
        } else {
            u64::from(self.inline_max)
        }
    }

    /// Assign the update id at send order, charge the frame against the
    /// unacked budget, and send — or, past the budget, drop the
    /// subscription instead: `KV_CLOSED{RESOURCE_LIMIT}` rides the same
    /// outbox and the client re-opens for a fresh snapshot
    /// (docs/design/kv.md § Watch). `false` = closed or connection gone.
    fn charge_and_send(&self, st: &mut SubState, mut msg: Vec<u8>) -> bool {
        let len = msg.len() as u64;
        if st.unacked_bytes + len > self.unacked_max {
            st.closed = true;
            let _ = self
                .out
                .send(msg_kv_closed(self.kv_id, KV_CLOSED_RESOURCE_LIMIT));
            return false;
        }
        let id = self.next_update();
        msg[3..7].copy_from_slice(&id.to_le_bytes());
        st.unacked_bytes += len;
        st.unacked.push_back((id, len));
        self.out.send(msg).is_ok()
    }

    /// Deliver one `KV_UPDATE` frame whose `kv_id` is already set — queued
    /// if the snapshot is still going out, sent (id assigned, budget
    /// charged) otherwise. `false` = subscription closed or connection
    /// gone; the caller prunes.
    fn deliver(&self, msg: Vec<u8>) -> bool {
        let mut st = self.state.lock().unwrap();
        if st.closed {
            return false;
        }
        if let SubPhase::Snapshotting(pending) = &mut st.phase {
            pending.push(msg);
            return true;
        }
        self.charge_and_send(&mut st, msg)
    }

    /// Send one snapshot chunk, before the sub goes live: the same id
    /// assignment and budget accounting as the live path — a client that
    /// never drains its snapshot is the same failure mode as one that
    /// never acks (docs/design/kv.md § Watch).
    fn send_chunk(&self, msg: Vec<u8>) -> bool {
        let mut st = self.state.lock().unwrap();
        if st.closed {
            return false;
        }
        self.charge_and_send(&mut st, msg)
    }

    /// Flush updates queued during the snapshot (in mutation order, ids
    /// assigned now — after the snapshot chunks') and go live.
    fn go_live(&self) {
        let mut st = self.state.lock().unwrap();
        let pending = match &mut st.phase {
            SubPhase::Snapshotting(pending) => std::mem::take(pending),
            SubPhase::Live => Vec::new(),
        };
        st.phase = SubPhase::Live;
        for msg in pending {
            if st.closed || !self.charge_and_send(&mut st, msg) {
                break;
            }
        }
    }

    /// Advance the cumulative acked floor: release every queued update
    /// with id ≤ `update_id` from the retention window.
    fn ack(&self, update_id: u32) {
        let mut st = self.state.lock().unwrap();
        while let Some(&(id, len)) = st.unacked.front() {
            if id > update_id {
                break;
            }
            st.unacked_bytes -= len;
            st.unacked.pop_front();
        }
    }

    fn is_closed(&self) -> bool {
        self.state.lock().unwrap().closed
    }
}

/// Connection-scoped subscription registry; dies with the connection, and
/// the store's `Weak` references die with it.
#[derive(Default)]
pub struct KvSubs {
    map: FxHashMap<u16, Arc<SubEntry>>,
    next_id: u16,
    /// `DURABLE` puts awaiting their commit on this connection — the puts
    /// in flight docs/design/kv.md § Budgets caps (non-durable puts are
    /// answered before the next message is read, so they never pend).
    pending_puts: Arc<AtomicUsize>,
}

impl KvSubs {
    fn alloc_id(&mut self) -> Option<u16> {
        // Monotonic with wrap, skipping live ids and the 0xFFFF sentinel.
        for _ in 0..=u16::MAX {
            let id = self.next_id;
            self.next_id = self.next_id.wrapping_add(1);
            if id != KV_ID_INVALID && !self.map.contains_key(&id) {
                return Some(id);
            }
        }
        None
    }
}

struct Store {
    entries: BTreeMap<String, Entry>,
    subs: Vec<Weak<SubEntry>>,
    /// Ordered queue to the writer thread; `None` = memory-only store.
    writer: Option<std::sync::mpsc::Sender<WriteJob>>,
    total_bytes: u64,
}

impl Store {
    /// Open (or create) the database, load every entry — computing hashes
    /// at memory speed (hashes are not persisted; docs/design/kv.md
    /// § Storage) — and hand the database to the writer thread. Any
    /// failure degrades to a memory-only store with a warning — the wire
    /// contract holds, durability does not.
    fn open() -> Store {
        let mut store = Store {
            entries: BTreeMap::new(),
            subs: Vec::new(),
            writer: None,
            total_bytes: 0,
        };
        let Some(path) = db_path() else {
            eprintln!("kv: no resolvable state dir; store is memory-only");
            return store;
        };
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let _ = std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700));
            }
        }
        match redb::Database::create(&path) {
            Ok(db) => {
                // 0600, as docs/design/kv.md states. `Database::create` uses
                // the umask (0644 by default), and the 0700 parent is only
                // defense in depth if the parent is ours — it may predate
                // this process with looser modes.
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
                }
                #[allow(clippy::result_large_err)]
                // redb::Error is big; local + immediately consumed
                let load = || -> Result<Vec<(String, Vec<u8>)>, redb::Error> {
                    let txn = db.begin_read()?;
                    let mut out = Vec::new();
                    match txn.open_table(TABLE) {
                        Ok(table) => {
                            for item in table.iter()? {
                                let (k, v) = item?;
                                out.push((k.value().to_string(), v.value().to_vec()));
                            }
                        }
                        // A fresh database has no table yet.
                        Err(redb::TableError::TableDoesNotExist(_)) => {}
                        Err(e) => return Err(e.into()),
                    }
                    Ok(out)
                };
                match load() {
                    Ok(rows) => {
                        for (key, raw) in rows {
                            if raw.len() < 8 {
                                continue;
                            }
                            let mtime_ns = u64::from_le_bytes(raw[0..8].try_into().unwrap());
                            let value = raw[8..].to_vec();
                            store.total_bytes += (key.len() + value.len()) as u64;
                            store.entries.insert(
                                key,
                                Entry {
                                    hash: blit_fssync::blake3_128(&value),
                                    value: Arc::new(value),
                                    mtime_ns,
                                },
                            );
                        }
                        let (tx, rx) = std::sync::mpsc::channel::<WriteJob>();
                        let spawned = std::thread::Builder::new()
                            .name("kv-writer".into())
                            .spawn(move || writer_loop(db, rx));
                        match spawned {
                            Ok(_) => store.writer = Some(tx),
                            Err(e) => {
                                eprintln!("kv: writer thread failed ({e}); store is memory-only");
                            }
                        }
                    }
                    Err(e) => {
                        eprintln!("kv: load failed ({e}); store is memory-only");
                    }
                }
            }
            Err(e) => {
                eprintln!(
                    "kv: cannot open {} ({e}); store is memory-only",
                    path.display()
                );
            }
        }
        store
    }

    /// Queue one mutation for the writer thread. Must be called with the
    /// store lock held so the queue order is the mutation order. `reply`
    /// `Some` marks a `DURABLE` put; `true` = the job is queued and (for
    /// `DURABLE`) the writer thread owns the deferred `KV_DONE`; `false`
    /// = no durable path exists (memory-only store or dead writer) and
    /// the caller must reply now.
    fn enqueue(
        &mut self,
        key: &str,
        value: Option<(Arc<Vec<u8>>, u64)>,
        reply: Option<DurableReply>,
    ) -> bool {
        let Some(writer) = &self.writer else {
            return false;
        };
        let job = WriteJob {
            key: key.to_string(),
            value,
            reply,
        };
        match writer.send(job) {
            Ok(()) => true,
            Err(_) => {
                self.writer = None;
                eprintln!("kv: writer thread gone; store is memory-only");
                false
            }
        }
    }

    /// Push one mutation to every live matching subscription, pruning dead
    /// ones. Called with the store lock held. The compressed records
    /// payload is built at most twice — value-inlined and metadata-only,
    /// per the subscribers' `inline_max` — and each subscriber gets a
    /// clone with only the small uncompressed header rewritten (`kv_id`
    /// here, `update_id` at send; crates/remote/src/kv.rs layout).
    fn broadcast(&mut self, key: &str) {
        if self.subs.is_empty() {
            return;
        }
        let entry = self.entries.get(key);
        let build = |value: Option<&[u8]>| -> Vec<u8> {
            let mut records = Vec::new();
            match entry {
                Some(e) => append_kv_record(
                    &mut records,
                    &KvRecord::Upsert {
                        key,
                        hash: e.hash,
                        size: e.value.len() as u32,
                        mtime_ns: e.mtime_ns,
                        value,
                    },
                ),
                None => append_kv_record(&mut records, &KvRecord::Delete { key }),
            }
            msg_kv_update(0, 0, 0, &records)
        };
        let mut inlined: Option<Vec<u8>> = None;
        let mut metadata: Option<Vec<u8>> = None;
        self.subs.retain(|weak| {
            let Some(sub) = weak.upgrade() else {
                return false;
            };
            if !key.starts_with(sub.prefix.as_str()) {
                return true;
            }
            let inline = match entry {
                Some(e) => e.value.len() as u64 <= sub.inline_limit(),
                None => false,
            };
            let template = if inline {
                inlined.get_or_insert_with(|| build(entry.map(|e| e.value.as_slice())))
            } else {
                metadata.get_or_insert_with(|| build(None))
            };
            let mut msg = template.clone();
            msg[1..3].copy_from_slice(&sub.kv_id.to_le_bytes());
            // A closed outbox means the connection is going away; drop the sub.
            sub.deliver(msg)
        });
    }
}

/// The writer thread: drains queued mutations into one transaction per
/// wakeup, `Immediate` (fsynced) when any job in the batch is `DURABLE`,
/// `Eventual` otherwise — so a `DURABLE` commit also hardens everything
/// ordered before it. Failures degrade (memory truth holds, durability is
/// lost) and are logged; a `DURABLE` job whose commit failed reports
/// `OTHER` so the reply is honest.
fn writer_loop(db: redb::Database, rx: std::sync::mpsc::Receiver<WriteJob>) {
    while let Ok(first) = rx.recv() {
        let mut batch = vec![first];
        while let Ok(job) = rx.try_recv() {
            batch.push(job);
        }
        let durable = batch.iter().any(|j| j.reply.is_some());
        #[allow(clippy::result_large_err)] // redb::Error is big; local + immediately consumed
        let run = || -> Result<(), redb::Error> {
            let mut txn = db.begin_write()?;
            txn.set_durability(if durable {
                redb::Durability::Immediate
            } else {
                redb::Durability::Eventual
            });
            {
                let mut table = txn.open_table(TABLE)?;
                for job in &batch {
                    match &job.value {
                        Some((bytes, mtime_ns)) => {
                            let mut raw = Vec::with_capacity(8 + bytes.len());
                            raw.extend_from_slice(&mtime_ns.to_le_bytes());
                            raw.extend_from_slice(bytes);
                            table.insert(job.key.as_str(), raw.as_slice())?;
                        }
                        None => {
                            table.remove(job.key.as_str())?;
                        }
                    }
                }
            }
            txn.commit()?;
            Ok(())
        };
        let ok = match run() {
            Ok(()) => true,
            Err(e) => {
                eprintln!("kv: persist failed ({e})");
                false
            }
        };
        for job in batch {
            let Some(reply) = job.reply else { continue };
            let status = if ok { KV_STATUS_OK } else { KV_STATUS_OTHER };
            let _ = reply
                .out
                .send(msg_kv_done(reply.nonce, status, reply.hash, reply.mtime_ns));
            // The echo follows the deferred KV_DONE, preserving the
            // client's `lastWrittenHash` discipline (docs/design/kv.md
            // § Watch). Broadcast reads the *current* entry, so a
            // mutation that landed meanwhile re-broadcasts its own newer
            // state — idempotent for full-value upserts.
            store().lock().unwrap().broadcast(&job.key);
        }
    }
}

fn store() -> &'static Mutex<Store> {
    static STORE: OnceLock<Mutex<Store>> = OnceLock::new();
    STORE.get_or_init(|| Mutex::new(Store::open()))
}

/// Load and hash the store off the serving paths, so the first KV frame
/// of the first connection doesn't pay the whole-database load+hash
/// (≤ 256 MiB of BLAKE3) inline.
pub fn warm() {
    let _ = std::thread::Builder::new()
        .name("kv-warm".into())
        .spawn(|| {
            let _ = store();
        });
}

/// Handle one `KV_*` message on the connection task. In-memory mutations
/// run inline under the store lock; redb commits ride the writer thread.
/// Nonce request/response is the C2S backpressure: non-durable requests
/// answer before the next message is read, and pending `DURABLE` puts are
/// capped per connection (docs/design/kv.md § Budgets).
pub fn handle_kv_message(
    data: &[u8],
    subs: &mut KvSubs,
    out: &mpsc::UnboundedSender<Vec<u8>>,
    verbose: bool,
) {
    match data[0] {
        C2S_KV_OPEN => {
            let nonce = data
                .get(1..3)
                .map(|b| u16::from_le_bytes([b[0], b[1]]))
                .unwrap_or(0);
            let refuse = |status: u8, detail: &str| {
                let _ = out.send(msg_kv_opened(nonce, KV_ID_INVALID, status, detail));
            };
            let Some((nonce, flags, inline_max, prefix)) = parse_kv_open(data) else {
                refuse(KV_STATUS_INVALID, "malformed request");
                return;
            };
            if flags != 0 {
                refuse(KV_STATUS_INVALID, "unknown flags");
                return;
            }
            // A budget-dropped sub stays in the map until the connection
            // task looks; sweep here so it neither counts against the sub
            // limit nor pins its id (docs/design/kv.md § Watch: the client
            // recovers from KV_CLOSED by re-opening).
            subs.map.retain(|_, sub| !sub.is_closed());
            if subs.map.len() >= max_subs() {
                refuse(KV_STATUS_BUDGET, "subscription limit reached");
                return;
            }
            let Some(kv_id) = subs.alloc_id() else {
                refuse(KV_STATUS_BUDGET, "no ids left");
                return;
            };
            if verbose {
                eprintln!("C2S_KV_OPEN: kv_id={kv_id} prefix={prefix:?} inline_max={inline_max}");
            }
            let sub = Arc::new(SubEntry {
                kv_id,
                prefix,
                inline_max,
                unacked_max: unacked_max(),
                out: out.clone(),
                update_id: AtomicU32::new(0),
                state: Mutex::new(SubState::new()),
            });
            // KV_OPENED must precede the snapshot on the wire; the outbox is
            // FIFO, so sending first suffices.
            let _ = out.send(msg_kv_opened(nonce, kv_id, KV_STATUS_OK, ""));
            // Under the lock: register the sub and clone out the matching
            // entries — `Arc` values, no bytes copied. Serialization and
            // compression happen after the guard drops; a mutation racing
            // in broadcasts into the sub's `Snapshotting` buffer and
            // flushes after the snapshot, so wire order holds.
            let matching: Vec<(String, u128, u64, Arc<Vec<u8>>)> = {
                let mut st = store().lock().unwrap();
                // Broadcast prunes dead watches on mutation; prune here too
                // so an idle store doesn't accumulate them across reconnects.
                st.subs.retain(|w| w.strong_count() > 0);
                st.subs.push(Arc::downgrade(&sub));
                st.entries
                    .range(sub.prefix.clone()..)
                    .take_while(|(key, _)| key.starts_with(sub.prefix.as_str()))
                    .map(|(key, e)| (key.clone(), e.hash, e.mtime_ns, e.value.clone()))
                    .collect()
            };
            // Snapshot: every matching entry, chunked; the final chunk (or an
            // empty store's single empty update) carries SNAPSHOT_END.
            let limit = sub.inline_limit();
            let mut records = Vec::new();
            let mut chunks: Vec<Vec<u8>> = Vec::new();
            for (key, hash, mtime_ns, value) in &matching {
                append_kv_record(
                    &mut records,
                    &KvRecord::Upsert {
                        key,
                        hash: *hash,
                        size: value.len() as u32,
                        mtime_ns: *mtime_ns,
                        value: (value.len() as u64 <= limit).then_some(value.as_slice()),
                    },
                );
                if records.len() >= SNAPSHOT_CHUNK {
                    chunks.push(std::mem::take(&mut records));
                }
            }
            chunks.push(records);
            let last = chunks.len() - 1;
            for (i, chunk) in chunks.iter().enumerate() {
                let flags = if i == last { KV_UPDATE_SNAPSHOT_END } else { 0 };
                // The id is assigned (and the budget charged) at send; a
                // breach mid-snapshot closes the sub before it goes live.
                if !sub.send_chunk(msg_kv_update(sub.kv_id, 0, flags, chunk)) {
                    break;
                }
            }
            sub.go_live();
            if !sub.is_closed() {
                subs.map.insert(kv_id, sub);
            }
        }
        C2S_KV_STOP => {
            if let Some(kv_id) = parse_kv_stop(data) {
                subs.map.remove(&kv_id);
            }
        }
        C2S_KV_ACK => {
            // Cumulative: advance the sub's acked floor, releasing retained
            // wire bytes from the unacked window (docs/design/kv.md
            // § Watch). An unknown kv_id is ignored — the ack may have
            // crossed a KV_CLOSED or KV_STOP in flight, and that race is
            // benign.
            if let Some((kv_id, update_id)) = parse_kv_ack(data)
                && let Some(sub) = subs.map.get(&kv_id)
            {
                sub.ack(update_id);
            }
        }
        C2S_KV_PUT => {
            let nonce = data
                .get(1..3)
                .map(|b| u16::from_le_bytes([b[0], b[1]]))
                .unwrap_or(0);
            let done = |status: u8, hash: u128, mtime_ns: u64| {
                let _ = out.send(msg_kv_done(nonce, status, hash, mtime_ns));
            };
            // Check the declared size before inflating: the LZ4 header says
            // how much `parse_kv_put` is about to allocate, and paying a
            // 64 MiB allocation to discover a 4 MiB limit was exceeded is an
            // amplification any client can ask for repeatedly.
            if blit_remote::kv::kv_put_declared_value_len(data)
                .is_some_and(|len| len as u64 > value_max())
            {
                done(KV_STATUS_TOO_LARGE, 0, 0);
                return;
            }
            let Some(put) = parse_kv_put(data) else {
                done(KV_STATUS_INVALID, 0, 0);
                return;
            };
            const KNOWN: u8 = KV_PUT_NO_CAS | KV_PUT_DELETE | KV_PUT_DURABLE;
            if put.flags & !KNOWN != 0 || !kv_key_valid(&put.key) {
                done(KV_STATUS_INVALID, 0, 0);
                return;
            }
            let delete = put.flags & KV_PUT_DELETE != 0;
            let no_cas = put.flags & KV_PUT_NO_CAS != 0;
            let durable = put.flags & KV_PUT_DURABLE != 0;
            if delete && !put.value.is_empty() {
                done(KV_STATUS_INVALID, 0, 0);
                return;
            }
            // Delete-iff-absent is meaningless (docs/design/kv.md § Wire).
            if delete && !no_cas && put.base == 0 {
                done(KV_STATUS_INVALID, 0, 0);
                return;
            }
            if !delete && put.value.len() as u64 > value_max() {
                done(KV_STATUS_TOO_LARGE, 0, 0);
                return;
            }
            if durable && subs.pending_puts.load(Ordering::Relaxed) >= max_inflight() {
                done(KV_STATUS_BUDGET, 0, 0);
                return;
            }
            if verbose {
                eprintln!(
                    "C2S_KV_PUT: key={:?} len={} delete={delete} no_cas={no_cas}",
                    put.key,
                    put.value.len()
                );
            }
            // Hash outside the lock — BLAKE3 over ≤ 4 MiB has no business
            // inside the global store critical section.
            let hash = if delete {
                0
            } else {
                blit_fssync::blake3_128(&put.value)
            };
            let durable_reply = |hash: u128, mtime_ns: u64| {
                subs.pending_puts.fetch_add(1, Ordering::Relaxed);
                DurableReply {
                    out: out.clone(),
                    nonce,
                    hash,
                    mtime_ns,
                    pending: subs.pending_puts.clone(),
                }
            };
            let mut st = store().lock().unwrap();
            let current = st.entries.get(&put.key);
            let current_hash = current.map(|e| e.hash).unwrap_or(0);
            if !no_cas && put.base != current_hash {
                done(KV_STATUS_CONFLICT, current_hash, 0);
                return;
            }
            if delete {
                if let Some(removed) = st.entries.remove(&put.key) {
                    st.total_bytes = st
                        .total_bytes
                        .saturating_sub((put.key.len() + removed.value.len()) as u64);
                    let queued = st.enqueue(&put.key, None, durable.then(|| durable_reply(0, 0)));
                    // A queued DURABLE job defers KV_DONE and the echo to
                    // the writer thread; everything else replies here.
                    // Either way KV_DONE precedes the writer's own echo on
                    // the wire, so the client records the outcome before
                    // the update lands (fs-write's echo-suppression
                    // discipline). A non-durable enqueue failure degrades
                    // silently: memory truth holds, durability is lost.
                    if !(durable && queued) {
                        let status = if durable {
                            KV_STATUS_OTHER
                        } else {
                            KV_STATUS_OK
                        };
                        done(status, 0, 0);
                        st.broadcast(&put.key);
                    }
                } else {
                    // NO_CAS delete of an absent key: idempotent success.
                    done(KV_STATUS_OK, 0, 0);
                }
                return;
            }
            let new_bytes = (put.key.len() + put.value.len()) as u64;
            let old_bytes = current
                .map(|e| (put.key.len() + e.value.len()) as u64)
                .unwrap_or(0);
            let inserting = current.is_none();
            if inserting && st.entries.len() as u64 >= max_entries() {
                done(KV_STATUS_BUDGET, 0, 0);
                return;
            }
            if st.total_bytes - old_bytes + new_bytes > total_max() {
                done(KV_STATUS_BUDGET, 0, 0);
                return;
            }
            let mtime_ns = now_ns();
            // One allocation for the value: the entry, the write job, and
            // every broadcast share the same `Arc`.
            let value = Arc::new(put.value);
            st.total_bytes = st.total_bytes - old_bytes + new_bytes;
            st.entries.insert(
                put.key.clone(),
                Entry {
                    value: value.clone(),
                    hash,
                    mtime_ns,
                },
            );
            let queued = st.enqueue(
                &put.key,
                Some((value, mtime_ns)),
                durable.then(|| durable_reply(hash, mtime_ns)),
            );
            // As the delete path: a queued DURABLE job defers KV_DONE (and
            // its echo) to the writer thread; everything else replies here,
            // with KV_DONE preceding the writer's own echo so the client
            // records `lastWrittenHash` before the update lands (fs-write's
            // echo-suppression discipline).
            if !(durable && queued) {
                let status = if durable {
                    KV_STATUS_OTHER
                } else {
                    KV_STATUS_OK
                };
                done(status, hash, mtime_ns);
                st.broadcast(&put.key);
            }
        }
        C2S_KV_FETCH => {
            let nonce = data
                .get(1..3)
                .map(|b| u16::from_le_bytes([b[0], b[1]]))
                .unwrap_or(0);
            let Some((nonce, key)) = parse_kv_fetch(data) else {
                let _ = out.send(msg_kv_value(nonce, KV_STATUS_INVALID, 0, &[]));
                return;
            };
            // Clone the `Arc` under the lock; compress outside it.
            let found = {
                let st = store().lock().unwrap();
                st.entries.get(&key).map(|e| (e.hash, e.value.clone()))
            };
            match found {
                Some((hash, value)) => {
                    let _ = out.send(msg_kv_value(nonce, KV_STATUS_OK, hash, &value));
                }
                None => {
                    let _ = out.send(msg_kv_value(nonce, KV_STATUS_NOT_FOUND, 0, &[]));
                }
            }
        }
        _ => {}
    }
}

/// Refuse a `KV_*` message at dispatch (`BLIT_KV=0`): every nonce-bearing
/// request gets its one `PERMISSION` reply, subscriptions and acks are
/// dropped (docs/design/kv.md § Security posture).
pub fn refuse_kv_message(data: &[u8], out: &mpsc::UnboundedSender<Vec<u8>>) {
    use blit_remote::kv::KV_STATUS_PERMISSION;
    let nonce = data
        .get(1..3)
        .map(|b| u16::from_le_bytes([b[0], b[1]]))
        .unwrap_or(0);
    match data[0] {
        C2S_KV_OPEN => {
            let _ = out.send(msg_kv_opened(
                nonce,
                KV_ID_INVALID,
                KV_STATUS_PERMISSION,
                "kv disabled",
            ));
        }
        C2S_KV_PUT => {
            let _ = out.send(msg_kv_done(nonce, KV_STATUS_PERMISSION, 0, 0));
        }
        C2S_KV_FETCH => {
            let _ = out.send(msg_kv_value(nonce, KV_STATUS_PERMISSION, 0, &[]));
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {

    /// An oversized value is refused from its LZ4 header, not after being
    /// inflated. `decompress_size_prepended` allocates the declared size, so
    /// rejecting a 4 MiB-limit violation used to cost a 64 MiB allocation
    /// first — sixteenfold amplification, one message at a time.
    #[test]
    fn oversized_put_is_refused_before_inflating() {
        let over = value_max() as usize + 1;
        let put = msg_kv_put(&KvPut {
            nonce: 1,
            flags: KV_PUT_NO_CAS,
            base: 0,
            key: "big".into(),
            value: vec![0u8; over],
        });
        assert_eq!(
            blit_remote::kv::kv_put_declared_value_len(&put),
            Some(over),
            "the declared size must be readable without inflating"
        );

        let (out, mut rx) = mpsc::unbounded_channel::<Vec<u8>>();
        let mut subs = KvSubs::default();
        handle_kv_message(&put, &mut subs, &out, false);
        let (nonce, status, ..) = parse_kv_done(&rx.try_recv().expect("a reply")).unwrap();
        assert_eq!((nonce, status), (1, KV_STATUS_TOO_LARGE));
    }

    use super::*;
    use blit_remote::kv::{
        KV_STATUS_PERMISSION, KvMirror, KvPut, S2C_KV_CLOSED, S2C_KV_DONE, S2C_KV_UPDATE,
        msg_kv_ack, msg_kv_fetch, msg_kv_open, msg_kv_put, parse_kv_closed, parse_kv_done,
        parse_kv_opened, parse_kv_value,
    };

    /// Incompressible bytes (xorshift64): LZ4 cannot shrink them, so
    /// wire-frame sizes track value sizes and the unacked budget is
    /// exercised for real. `seed` must be non-zero.
    fn noise(len: usize, mut seed: u64) -> Vec<u8> {
        let mut out = Vec::with_capacity(len + 8);
        while out.len() < len {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            out.extend_from_slice(&seed.to_le_bytes());
        }
        out.truncate(len);
        out
    }

    /// Blocking receive with a deadline, for replies that ride the writer
    /// thread (deferred `DURABLE` acks and their echoes).
    fn recv_wait(rx: &mut mpsc::UnboundedReceiver<Vec<u8>>) -> Vec<u8> {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            if let Ok(msg) = rx.try_recv() {
                return msg;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "message never arrived"
            );
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
    }

    /// The whole put/CAS/fetch/watch flow in one test: the store is a
    /// process-global OnceLock, so a single test owns its lifecycle (and
    /// the `BLIT_KV_PATH` redirect must land before first use).
    #[test]
    fn kv_store_end_to_end() {
        let dir = std::env::temp_dir().join(format!("blit-kv-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        unsafe { std::env::set_var("BLIT_KV_PATH", dir.join("kv.redb")) };
        // The retention sections below breach the unacked budget with
        // ~1.5 KiB incompressible values; everything before them stays far
        // under 4 KiB per sub. `unacked_max()` is read only on the
        // `KV_OPEN` path, so this test — the store lifecycle's single
        // owner — also owns the LazyLock's first read.
        unsafe { std::env::set_var("BLIT_KV_UNACKED_MAX", "4096") };

        let (out, mut rx) = mpsc::unbounded_channel::<Vec<u8>>();
        let mut subs = KvSubs::default();
        let mut recv = move || rx.try_recv();

        // Subscribe to editor/ — an empty snapshot completes immediately.
        handle_kv_message(&msg_kv_open(1, 0, 1024, "editor/"), &mut subs, &out, false);
        let (_, kv_id, status, _) = parse_kv_opened(&recv().unwrap()).unwrap();
        assert_eq!(status, KV_STATUS_OK);
        let mut mirror = KvMirror::new();
        assert_eq!(mirror.apply_update(&recv().unwrap()), Some(1));
        assert!(mirror.snapshot_done);
        assert!(mirror.live.is_empty());

        // Create-exclusive put lands and reaches the watcher.
        let put = |nonce, flags, base, key: &str, value: &[u8]| {
            msg_kv_put(&KvPut {
                nonce,
                flags,
                base,
                key: key.to_string(),
                value: value.to_vec(),
            })
        };
        handle_kv_message(&put(2, 0, 0, "editor/x", b"one"), &mut subs, &out, false);
        let (_, status, h1, _) = parse_kv_done(&recv().unwrap()).unwrap();
        assert_eq!(status, KV_STATUS_OK);
        assert_ne!(h1, 0);
        mirror.apply_update(&recv().unwrap()).unwrap();
        assert_eq!(
            mirror.live.get("editor/x").unwrap().value.as_deref(),
            Some(b"one".as_slice())
        );

        // A stale base conflicts and carries the current hash.
        let (_, status, cur, _) = parse_kv_done(&{
            handle_kv_message(&put(3, 0, 0, "editor/x", b"stale"), &mut subs, &out, false);
            recv().unwrap()
        })
        .unwrap();
        assert_eq!(status, KV_STATUS_CONFLICT);
        assert_eq!(cur, h1);

        // CAS off the returned hash succeeds.
        handle_kv_message(&put(4, 0, h1, "editor/x", b"two"), &mut subs, &out, false);
        let (_, status, h2, _) = parse_kv_done(&recv().unwrap()).unwrap();
        assert_eq!(status, KV_STATUS_OK);
        mirror.apply_update(&recv().unwrap()).unwrap();

        // Fetch returns the live value.
        handle_kv_message(&msg_kv_fetch(5, "editor/x"), &mut subs, &out, false);
        let (_, status, hash, data) = parse_kv_value(&recv().unwrap()).unwrap();
        assert_eq!(status, KV_STATUS_OK);
        assert_eq!(hash, h2);
        assert_eq!(data, b"two");

        // A put outside the prefix does not reach the watcher.
        handle_kv_message(
            &put(6, 0, 0, "roots", b"main = /x\n"),
            &mut subs,
            &out,
            false,
        );
        parse_kv_done(&recv().unwrap()).unwrap();

        // CAS delete removes and broadcasts.
        handle_kv_message(
            &put(7, KV_PUT_DELETE, h2, "editor/x", b""),
            &mut subs,
            &out,
            false,
        );
        let (_, status, hash, _) = parse_kv_done(&recv().unwrap()).unwrap();
        assert_eq!(status, KV_STATUS_OK);
        assert_eq!(hash, 0);
        mirror.apply_update(&recv().unwrap()).unwrap();
        assert!(mirror.live.is_empty());

        handle_kv_message(&msg_kv_fetch(8, "editor/x"), &mut subs, &out, false);
        let (_, status, _, _) = parse_kv_value(&recv().unwrap()).unwrap();
        assert_eq!(status, KV_STATUS_NOT_FOUND);

        // KV_STOP drops the subscription; further mutations stay silent.
        handle_kv_message(&blit_remote::kv::msg_kv_stop(kv_id), &mut subs, &out, false);
        handle_kv_message(&put(9, 0, 0, "editor/y", b"z"), &mut subs, &out, false);
        parse_kv_done(&recv().unwrap()).unwrap();
        assert!(recv().is_err());

        // The dispatch-gate refusals answer every nonce.
        refuse_kv_message(&msg_kv_fetch(10, "editor/x"), &out);
        let (_, status, _, _) = parse_kv_value(&recv().unwrap()).unwrap();
        assert_eq!(status, KV_STATUS_PERMISSION);

        // --- DURABLE flow rides the writer thread ------------------------
        // Re-subscribe so the echo path is observable again.
        let (out2, mut rx2) = mpsc::unbounded_channel::<Vec<u8>>();
        handle_kv_message(
            &msg_kv_open(11, 0, 1024, "editor/"),
            &mut subs,
            &out2,
            false,
        );
        let (_, _, status, _) = parse_kv_opened(&recv_wait(&mut rx2)).unwrap();
        assert_eq!(status, KV_STATUS_OK);
        let snap = recv_wait(&mut rx2);
        assert_eq!(snap[0], S2C_KV_UPDATE);

        // The DURABLE ack is deferred to the commit, and the writer's own
        // echo follows its KV_DONE on the wire (docs/design/kv.md § Watch).
        handle_kv_message(
            &put(12, KV_PUT_DURABLE, 0, "editor/d", b"parked"),
            &mut subs,
            &out2,
            false,
        );
        let first = recv_wait(&mut rx2);
        assert_eq!(
            first[0], S2C_KV_DONE,
            "durable KV_DONE must precede the writer's own echo"
        );
        let (dn, status, hd, mt) = parse_kv_done(&first).unwrap();
        assert_eq!(dn, 12);
        assert_eq!(status, KV_STATUS_OK);
        assert_ne!(hd, 0);
        assert_ne!(mt, 0);
        let echo = recv_wait(&mut rx2);
        assert_eq!(echo[0], S2C_KV_UPDATE);
        let mut mirror2 = KvMirror::new();
        mirror2.apply_update(&snap).unwrap();
        mirror2.apply_update(&echo).unwrap();
        assert_eq!(
            mirror2.live.get("editor/d").unwrap().value.as_deref(),
            Some(b"parked".as_slice())
        );
        assert_eq!(subs.pending_puts.load(Ordering::Relaxed), 0);

        // Writer ordering: a non-durable put queued behind a DURABLE one
        // lands after it — the fetch observes the later value.
        handle_kv_message(
            &put(13, KV_PUT_DURABLE, hd, "editor/d", b"first"),
            &mut subs,
            &out2,
            false,
        );
        handle_kv_message(
            &put(14, KV_PUT_NO_CAS, 0, "editor/d", b"second"),
            &mut subs,
            &out2,
            false,
        );
        // Non-durable ack is immediate (rides the connection task), the
        // durable one arrives whenever the commit lands.
        let mut got_13 = false;
        let mut got_14 = false;
        while !(got_13 && got_14) {
            let msg = recv_wait(&mut rx2);
            if msg[0] == S2C_KV_DONE {
                let (nonce, status, _, _) = parse_kv_done(&msg).unwrap();
                assert_eq!(status, KV_STATUS_OK);
                match nonce {
                    13 => got_13 = true,
                    14 => got_14 = true,
                    _ => panic!("unexpected nonce {nonce}"),
                }
            }
        }
        handle_kv_message(&msg_kv_fetch(15, "editor/d"), &mut subs, &out2, false);
        loop {
            let msg = recv_wait(&mut rx2);
            if msg[0] == blit_remote::kv::S2C_KV_VALUE {
                let (_, status, _, data) = parse_kv_value(&msg).unwrap();
                assert_eq!(status, KV_STATUS_OK);
                assert_eq!(data, b"second");
                break;
            }
        }

        // --- Retention (docs/design/kv.md § Watch) -----------------------

        // A sub that never acks past the budget is dropped: the breaching
        // frame is withheld, KV_CLOSED{RESOURCE_LIMIT} rides the same
        // outbox, and later mutations no longer queue to it.
        let (out3, mut rx3) = mpsc::unbounded_channel::<Vec<u8>>();
        handle_kv_message(&msg_kv_open(20, 0, 0, "ret/"), &mut subs, &out3, false);
        let (_, ret_id, status, _) = parse_kv_opened(&rx3.try_recv().unwrap()).unwrap();
        assert_eq!(status, KV_STATUS_OK);
        let snap = rx3.try_recv().unwrap(); // empty snapshot, well under budget
        assert_eq!(snap[0], S2C_KV_UPDATE);
        for i in 0..4u16 {
            let v = noise(1500, u64::from(i) + 1);
            handle_kv_message(
                &put(21 + i, KV_PUT_NO_CAS, 0, "ret/x", &v),
                &mut subs,
                &out3,
                false,
            );
        }
        // Non-durable puts reply and broadcast synchronously, so the whole
        // exchange is already queued.
        let msgs: Vec<Vec<u8>> = std::iter::from_fn(|| rx3.try_recv().ok()).collect();
        assert_eq!(msgs.iter().filter(|m| m[0] == S2C_KV_DONE).count(), 4);
        assert_eq!(
            msgs.iter().filter(|m| m[0] == S2C_KV_UPDATE).count(),
            2,
            "the third ~1.5 KiB frame breached the 4 KiB budget and was withheld"
        );
        let closed_at = msgs.iter().position(|m| m[0] == S2C_KV_CLOSED).unwrap();
        assert_eq!(
            parse_kv_closed(&msgs[closed_at]),
            Some((ret_id, KV_CLOSED_RESOURCE_LIMIT))
        );
        assert!(
            msgs[closed_at + 1..].iter().all(|m| m[0] == S2C_KV_DONE),
            "after KV_CLOSED only put replies reach this connection"
        );

        // A later mutation lands in the store but no longer queues to the
        // dropped sub, and no second KV_CLOSED fires.
        let last = noise(1500, 99);
        handle_kv_message(
            &put(25, KV_PUT_NO_CAS, 0, "ret/x", &last),
            &mut subs,
            &out3,
            false,
        );
        let (_, status, _, _) = parse_kv_done(&rx3.try_recv().unwrap()).unwrap();
        assert_eq!(status, KV_STATUS_OK);
        assert!(rx3.try_recv().is_err());

        // Re-open after the drop: a fresh, coherent snapshot carrying the
        // state the dropped sub never saw — the drop was lossless.
        handle_kv_message(&msg_kv_open(26, 0, 0, "ret/"), &mut subs, &out3, false);
        let (_, ret2_id, status, _) = parse_kv_opened(&rx3.try_recv().unwrap()).unwrap();
        assert_eq!(status, KV_STATUS_OK);
        assert_ne!(ret2_id, ret_id);
        let mut mirror3 = KvMirror::new();
        mirror3.apply_update(&rx3.try_recv().unwrap()).unwrap();
        assert!(mirror3.snapshot_done);
        assert_eq!(
            mirror3.live.get("ret/x").unwrap().value.as_deref(),
            Some(last.as_slice())
        );

        // An acking sub survives indefinitely: the floor advances, the
        // window drains, and updates keep flowing well past the budget.
        let (out4, mut rx4) = mpsc::unbounded_channel::<Vec<u8>>();
        handle_kv_message(&msg_kv_open(27, 0, 0, "ack/"), &mut subs, &out4, false);
        let (_, ack_id, status, _) = parse_kv_opened(&rx4.try_recv().unwrap()).unwrap();
        assert_eq!(status, KV_STATUS_OK);
        let mut mirror4 = KvMirror::new();
        let uid = mirror4.apply_update(&rx4.try_recv().unwrap()).unwrap();
        handle_kv_message(&msg_kv_ack(ack_id, uid), &mut subs, &out4, false);
        let mut expect = Vec::new();
        for i in 0..8u16 {
            expect = noise(1500, 200 + u64::from(i));
            handle_kv_message(
                &put(28 + i, KV_PUT_NO_CAS, 0, "ack/x", &expect),
                &mut subs,
                &out4,
                false,
            );
            let done = rx4.try_recv().unwrap();
            assert_eq!(done[0], S2C_KV_DONE);
            let update = rx4.try_recv().unwrap();
            assert_eq!(update[0], S2C_KV_UPDATE, "an acking sub keeps receiving");
            let uid = mirror4.apply_update(&update).unwrap();
            handle_kv_message(&msg_kv_ack(ack_id, uid), &mut subs, &out4, false);
        }
        // 8 × ~1.5 KiB flowed through the 4 KiB window without a drop.
        assert!(rx4.try_recv().is_err());
        assert_eq!(
            mirror4.live.get("ack/x").unwrap().value.as_deref(),
            Some(expect.as_slice())
        );

        // Snapshot chunks count toward the budget too: seed enough state
        // that the initial snapshot alone breaches, then open and never
        // drain — KV_OPENED, then KV_CLOSED, no update, no later queuing.
        for i in 0..4u16 {
            let v = noise(1500, 300 + u64::from(i));
            let key = format!("snap/{i}");
            handle_kv_message(
                &put(40 + i, KV_PUT_NO_CAS, 0, &key, &v),
                &mut subs,
                &out3,
                false,
            );
            let (_, status, _, _) = parse_kv_done(&rx3.try_recv().unwrap()).unwrap();
            assert_eq!(status, KV_STATUS_OK);
        }
        let (out5, mut rx5) = mpsc::unbounded_channel::<Vec<u8>>();
        handle_kv_message(&msg_kv_open(44, 0, 0, "snap/"), &mut subs, &out5, false);
        let (_, snap_id, status, _) = parse_kv_opened(&rx5.try_recv().unwrap()).unwrap();
        assert_eq!(status, KV_STATUS_OK);
        assert_eq!(
            parse_kv_closed(&rx5.try_recv().unwrap()),
            Some((snap_id, KV_CLOSED_RESOURCE_LIMIT))
        );
        let v = noise(1500, 400);
        handle_kv_message(
            &put(45, KV_PUT_NO_CAS, 0, "snap/0", &v),
            &mut subs,
            &out3,
            false,
        );
        parse_kv_done(&rx3.try_recv().unwrap()).unwrap();
        assert!(rx5.try_recv().is_err());

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Direct `SubEntry` with an explicit unacked budget — the env-backed
    /// `unacked_max()` is deliberately untouched so these tests cannot
    /// race the end-to-end test's env setup.
    fn test_sub(kv_id: u16, unacked_max: u64, out: mpsc::UnboundedSender<Vec<u8>>) -> SubEntry {
        SubEntry {
            kv_id,
            prefix: String::new(),
            inline_max: 0,
            unacked_max,
            out,
            update_id: AtomicU32::new(0),
            state: Mutex::new(SubState::new()),
        }
    }

    /// A `Snapshotting` subscription buffers live updates and flushes them
    /// after the snapshot chunks with strictly increasing update ids —
    /// the wire-order guarantee the off-lock snapshot encoding relies on.
    #[test]
    fn sub_snapshot_buffering_orders_updates() {
        let (tx, mut rx) = mpsc::unbounded_channel::<Vec<u8>>();
        let sub = test_sub(7, u64::MAX, tx);
        // A "broadcast" lands while the snapshot is still encoding: it
        // must buffer, not send.
        assert!(sub.deliver(msg_kv_update(7, 0, 0, b"")));
        assert!(rx.try_recv().is_err());
        // The snapshot chunk goes out first, taking update id 1.
        assert!(sub.send_chunk(msg_kv_update(sub.kv_id, 0, KV_UPDATE_SNAPSHOT_END, b"")));
        sub.go_live();
        // Live delivery after the flush keeps incrementing.
        assert!(sub.deliver(msg_kv_update(7, 0, 0, b"")));
        let ids: Vec<u32> = std::iter::from_fn(|| rx.try_recv().ok())
            .map(|m| u32::from_le_bytes([m[3], m[4], m[5], m[6]]))
            .collect();
        assert_eq!(ids, vec![1, 2, 3]);
    }

    /// Past the unacked budget a subscription is dropped, not throttled:
    /// the breaching frame is withheld, `KV_CLOSED{RESOURCE_LIMIT}` rides
    /// the same outbox, and nothing further queues
    /// (docs/design/kv.md § Watch).
    #[test]
    fn sub_budget_breach_closes_and_silences() {
        let (tx, mut rx) = mpsc::unbounded_channel::<Vec<u8>>();
        let frame = msg_kv_update(7, 0, 0, b"records");
        // Budget = exactly two frames: the third breaches.
        let sub = test_sub(7, 2 * frame.len() as u64, tx);
        sub.go_live();
        assert!(sub.deliver(frame.clone()));
        assert!(sub.deliver(frame.clone()));
        assert!(!sub.deliver(frame.clone()));
        assert!(sub.is_closed());
        // Closed means closed: no revival, no more frames.
        assert!(!sub.deliver(frame.clone()));
        let msgs: Vec<Vec<u8>> = std::iter::from_fn(|| rx.try_recv().ok()).collect();
        assert_eq!(msgs.len(), 3);
        assert_eq!(msgs[0][0], S2C_KV_UPDATE);
        assert_eq!(msgs[1][0], S2C_KV_UPDATE);
        assert_eq!(
            parse_kv_closed(&msgs[2]),
            Some((7, KV_CLOSED_RESOURCE_LIMIT))
        );
    }

    /// The cumulative acked floor releases retained bytes, so an acking
    /// subscriber sends unbounded traffic through a bounded window.
    #[test]
    fn sub_ack_floor_releases_budget() {
        let (tx, mut rx) = mpsc::unbounded_channel::<Vec<u8>>();
        let frame = msg_kv_update(7, 0, 0, b"records");
        // Budget = one frame in flight: only the ack keeps this alive.
        let sub = test_sub(7, frame.len() as u64, tx);
        sub.go_live();
        for _ in 0..5 {
            assert!(sub.deliver(frame.clone()));
            let sent = rx.try_recv().unwrap();
            let id = u32::from_le_bytes([sent[3], sent[4], sent[5], sent[6]]);
            // A stale ack (floor already past) releases nothing and must
            // not underflow; the real ack drains the window.
            sub.ack(id.wrapping_sub(1));
            sub.ack(id);
        }
        assert!(!sub.is_closed());
        assert_eq!(sub.state.lock().unwrap().unacked_bytes, 0);
    }
}
