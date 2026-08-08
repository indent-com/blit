//! One client attachment: the per-connection view of a workspace's
//! backends, owning the paced `LSP_STATE` and `LSP_DIAG` streams
//! (docs/design/lsp.md `LSP_STATE` / `LSP_DIAG`).
//!
//! The pacing thread mirrors fssync's per-sync engine: one in-flight
//! update per stream, coalescing while unacked — a slow client gets
//! fewer, larger updates and never falls behind. The first diagnostics
//! update is a `FULL` cache replay, so a late joiner or one-shot CLI
//! never sees a blank gutter.
//!
//! Backends can be stopped out from under an attachment (`LSP_STOP`, the
//! idle sweep). The attachment holds its backends behind a shared lock:
//! the pacer drops a stopped backend's `SERVER` record from the next
//! snapshot, and a query to a stopped backend respawns it, matching the
//! spec's "subscribers see LSP_STATE lose the record; a later query
//! respawns it".

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, Sender};
use std::time::{Duration, Instant};

use blit_remote::lsp::{
    LSP_CAP_COMPLETION, LSP_CAP_DEFINITION, LSP_CAP_DOC_SYMBOLS, LSP_CAP_HOVER, LSP_CAP_REFERENCES,
    LSP_CAP_RENAME, LSP_CAP_SIGNATURE, LSP_CAP_WS_SYMBOLS, LSP_DIAG_FULL, LSP_PHASE_FAILED,
    LSP_PHASE_READY, LSP_QUERY_COMPLETION, LSP_QUERY_DEFINITION, LSP_QUERY_DOC_SYMBOLS,
    LSP_QUERY_HOVER, LSP_QUERY_REFERENCES, LSP_QUERY_RENAME, LSP_QUERY_SIGNATURE,
    LSP_QUERY_WS_SYMBOLS, LSP_STATUS_NOT_FOUND, LSP_STATUS_OTHER, LSP_STATUS_WARMING,
    LSP_STREAM_DIAG, LSP_STREAM_STATE, LspDiagRecord, LspStateRecord, append_lsp_diag_record,
    append_lsp_state_record, msg_lsp_diag, msg_lsp_query_resp, msg_lsp_state,
};

use crate::backend::{Backend, Cmd};
use crate::discovery::ServerSpec;
use crate::text;
use crate::{Budgets, Sink};

/// Append one file's FILE record and as many DIAG records as the byte
/// budget and the `u16` count allow, returning how many were written.
///
/// The caller's budget checks run *between* files, so a single pathological
/// one — a generated bundle, or one syntax error cascading into hundreds of
/// thousands of diagnostics — used to append all of them. The payload could
/// then pass the client's decompression guard, at which point the client
/// refuses the whole update, never acks, and the one-in-flight pacer never
/// clears: that client receives no diagnostics again for the life of the
/// attachment.
///
/// The DIAG records are built first so the count in the FILE record is the
/// number that actually follow it, rather than a promise made before the
/// budget was consulted.
fn append_file_diags(
    records: &mut Vec<u8>,
    wire: &str,
    hash: blit_remote::lsp::LspHash,
    diags: &[crate::backend::WireDiag],
    bytes_max: usize,
) -> usize {
    let mut tail = Vec::new();
    let mut n = 0usize;
    for d in diags {
        if n >= LSP_DIAG_MAX_PER_FILE || records.len() + tail.len() >= bytes_max {
            break;
        }
        append_lsp_diag_record(
            &mut tail,
            &LspDiagRecord::Diag {
                severity: d.severity,
                flags: d.flags,
                line: d.line,
                col: d.col,
                end_line: d.end_line,
                end_col: d.end_col,
                code: &d.code,
                source: &d.source,
                msg: &d.msg,
            },
        );
        n += 1;
    }
    append_lsp_diag_record(
        records,
        &LspDiagRecord::File {
            hash,
            n: n as u16,
            path: wire,
        },
    );
    records.extend_from_slice(&tail);
    n
}

/// Diagnostics one file may report in an `LSP_DIAG`. Not a guessed number:
/// a FILE record states its count in a `u16`, so past this the count on the
/// wire would wrap and the client would attribute diagnostics to the wrong
/// file. No editor displays this many for one file either — a cascade that
/// long is one broken parse, and its first entries are the informative ones.
const LSP_DIAG_MAX_PER_FILE: usize = u16::MAX as usize;

/// The capability bit a query kind requires of its backend, so routing
/// never sends an unsupported request (which would surface as a bare
/// error). `0` for unknown kinds — no backend advertises it, so the
/// query answers `NOT_FOUND`.
fn required_cap(kind: u8) -> u32 {
    match kind {
        LSP_QUERY_DEFINITION => LSP_CAP_DEFINITION,
        LSP_QUERY_REFERENCES => LSP_CAP_REFERENCES,
        LSP_QUERY_HOVER => LSP_CAP_HOVER,
        LSP_QUERY_DOC_SYMBOLS => LSP_CAP_DOC_SYMBOLS,
        LSP_QUERY_WS_SYMBOLS => LSP_CAP_WS_SYMBOLS,
        LSP_QUERY_RENAME => LSP_CAP_RENAME,
        LSP_QUERY_COMPLETION => LSP_CAP_COMPLETION,
        LSP_QUERY_SIGNATURE => LSP_CAP_SIGNATURE,
        _ => 0,
    }
}

static NEXT_SUB: AtomicU64 = AtomicU64::new(1);

pub(crate) enum AttCmd {
    /// A backend changed state or diagnostics; re-check both streams.
    Ping,
    Ack {
        stream: u8,
        update_id: u32,
    },
    Close,
}

/// One `lsp_id`: a client's attachment to a workspace.
pub struct Attachment {
    pub root: PathBuf,
    /// Current live backends, shared with the pacer; entries are
    /// replaced in place when a query respawns a stopped backend.
    backends: Arc<Mutex<Vec<Arc<Backend>>>>,
    /// `(spec, root)` parallel to `backends`, for respawn.
    specs: Vec<(ServerSpec, PathBuf)>,
    budgets: Budgets,
    sub: u64,
    ctl: Sender<AttCmd>,
    /// What backends are attached with: `None` when no stream is wanted
    /// (no pacer thread runs, so nothing may hold a ping sender).
    ping: Option<Sender<AttCmd>>,
    wants_diags: bool,
}

impl Attachment {
    /// Wire an attachment over already-resolved backends. `sink` is the
    /// connection's serialized-message sender.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn start(
        lsp_id: u16,
        root: PathBuf,
        backends: Vec<Arc<Backend>>,
        specs: Vec<(ServerSpec, PathBuf)>,
        flags: u8,
        diag_latency_ms: u16,
        sink: Sink,
        budgets: &Budgets,
    ) -> Attachment {
        use blit_remote::lsp::{LSP_OPEN_DIAGS, LSP_OPEN_WATCH};
        let sub = NEXT_SUB.fetch_add(1, Ordering::Relaxed);
        let (ctl, inbox) = std::sync::mpsc::channel();
        let wants_diags = flags & LSP_OPEN_DIAGS != 0;
        let wants_state = wants_diags || flags & LSP_OPEN_WATCH != 0;
        // Every attachment registers with every backend regardless of
        // flags, so the idle sweeper counts query-only attachments and
        // never stops a backend that is actively answering queries. A
        // stream-less attachment registers without a ping channel — no
        // pacer thread runs for it, so nothing would drain pings.
        let ping = wants_state.then(|| ctl.clone());
        for backend in &backends {
            backend.send(Cmd::Attach {
                sub,
                ping: ping.clone(),
                wants_diags,
            });
        }
        let backends = Arc::new(Mutex::new(backends));
        if wants_state {
            let latency = if diag_latency_ms == 0 {
                Duration::from_millis(500)
            } else {
                Duration::from_millis(u64::from(diag_latency_ms).clamp(1, 10_000))
            };
            let pacer = Pacer {
                lsp_id,
                root: root.clone(),
                backends: backends.clone(),
                inbox,
                sink,
                sub,
                wants_diags,
                latency,
                entries_max: budgets.entries_max,
                bytes_max: budgets.bytes_max,
                state_floors: HashMap::new(),
                diag_floors: HashMap::new(),
                state_id: 0,
                diag_id: 0,
                inflight_state: None,
                inflight_diag: None,
                sent_full: false,
                next_diag_at: Instant::now(),
            };
            std::thread::Builder::new()
                .name("blit-lsp-att".into())
                .spawn(move || pacer.run())
                .expect("spawn lsp attachment thread");
        }
        Attachment {
            root,
            backends,
            specs,
            budgets: budgets.clone(),
            sub,
            ctl,
            ping,
            wants_diags,
        }
    }

    pub fn ack(&self, stream: u8, update_id: u32) {
        let _ = self.ctl.send(AttCmd::Ack { stream, update_id });
    }

    /// Route one `LSP_QUERY` to the right backend; immediate statuses
    /// (no backend for the language) answer on the spot. A stopped
    /// backend is respawned and the routing slot updated in place.
    #[allow(clippy::too_many_arguments)]
    pub fn query(
        &self,
        nonce: u16,
        kind: u8,
        flags: u8,
        line: u32,
        col: u32,
        wire_path: &str,
        arg: &str,
        sink: Sink,
    ) {
        let refuse = |status: u8| {
            let _ = sink(msg_lsp_query_resp(nonce, status, 0, "", &[]));
        };
        let want = required_cap(kind);
        // Which backends are candidates for this query: any backend for
        // a workspace-wide symbol search, else the ones registered for
        // the queried file's extension.
        let path = if kind == LSP_QUERY_WS_SYMBOLS {
            None
        } else {
            match text::resolve_wire(&self.root, wire_path) {
                Some(p) => Some(p),
                None => return refuse(blit_remote::lsp::LSP_STATUS_INVALID),
            }
        };
        let ext = path
            .as_ref()
            .and_then(|p| p.extension())
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase());
        let candidate = |b: &Arc<Backend>| {
            kind == LSP_QUERY_WS_SYMBOLS
                || ext
                    .as_ref()
                    .is_some_and(|e| b.extensions.iter().any(|x| x == e))
        };

        let mut backends = self.backends.lock().unwrap();
        // Route only to a backend that both applies to the query and
        // advertises the capability — never fall back to an incapable
        // one, or an unsupported request degrades to a bare "error".
        let idx = backends
            .iter()
            .position(|b| candidate(b) && b.caps() & want != 0);
        let Some(idx) = idx else {
            // No capable backend. If a candidate is still warming, the
            // capability may simply be unknown yet — say WARMING (retry)
            // rather than a misleading NOT_FOUND.
            let warming = backends
                .iter()
                .any(|b| candidate(b) && !matches!(b.phase(), LSP_PHASE_READY | LSP_PHASE_FAILED));
            drop(backends);
            return refuse(if warming {
                LSP_STATUS_WARMING
            } else {
                LSP_STATUS_NOT_FOUND
            });
        };
        // Respawn a stopped backend and update the slot in place, so a
        // later query brings the language server back (spec).
        if backends[idx].is_gone()
            && let Some((spec, root)) = self.specs.get(idx)
            && let Some(fresh) = crate::reacquire(spec, root, &self.budgets)
        {
            fresh.send(Cmd::Attach {
                sub: self.sub,
                ping: self.ping.clone(),
                wants_diags: self.wants_diags,
            });
            backends[idx] = fresh;
        }
        let backend = backends[idx].clone();
        drop(backends);
        let sent = backend.send(Cmd::Query {
            sub: self.sub,
            nonce,
            kind,
            flags,
            line,
            col,
            path,
            arg: arg.to_string(),
            wire_root: self.root.clone(),
            sink: sink.clone(),
        });
        if !sent {
            refuse(LSP_STATUS_OTHER);
        }
    }

    pub fn cancel(&self, nonce: u16) {
        for backend in self.backends.lock().unwrap().iter() {
            backend.send(Cmd::Cancel {
                sub: self.sub,
                nonce,
            });
        }
    }

    /// Route one buffer overlay write (`text` `None` releases) to every
    /// backend registered for the path's extension. Fire-and-forget by
    /// design (docs/design/lsp.md "LSP_BUFFER"): a gone backend simply
    /// misses this write and heals on the editor's next debounced send
    /// after a query respawns it.
    pub fn buffer(&self, wire_path: &str, text: Option<Vec<u8>>) {
        let Some(path) = text::resolve_wire(&self.root, wire_path) else {
            return;
        };
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase());
        let Some(ext) = ext else {
            return;
        };
        // Decode once at this boundary and Arc-share: every registered
        // backend gets a handle, not a copy of the buffer. A non-UTF-8
        // buffer degrades to the release the engine would have made of
        // it (docs/design/lsp.md "LSP_BUFFER").
        let text: Option<Arc<String>> =
            text.and_then(|bytes| String::from_utf8(bytes).ok().map(Arc::new));
        for backend in self.backends.lock().unwrap().iter() {
            if backend.extensions.iter().any(|x| x == &ext) {
                backend.send(Cmd::Buffer {
                    sub: self.sub,
                    path: path.clone(),
                    text: text.clone(),
                });
            }
        }
    }
}

impl Drop for Attachment {
    fn drop(&mut self) {
        let _ = self.ctl.send(AttCmd::Close);
        for backend in self.backends.lock().unwrap().iter() {
            backend.send(Cmd::Detach { sub: self.sub });
        }
    }
}

struct Pacer {
    lsp_id: u16,
    /// The attachment root wire paths relativize against.
    root: PathBuf,
    backends: Arc<Mutex<Vec<Arc<Backend>>>>,
    inbox: Receiver<AttCmd>,
    sink: Sink,
    /// The attachment's subscriber id, under which acked diag floors
    /// are published for tombstone pruning.
    sub: u64,
    wants_diags: bool,
    latency: Duration,
    entries_max: usize,
    bytes_max: usize,
    /// Per-backend cursors keyed by `server_ref`, so the maps survive a
    /// respawn swapping one backend for another.
    state_floors: HashMap<u16, u64>,
    diag_floors: HashMap<u16, u64>,
    state_id: u32,
    diag_id: u32,
    inflight_state: Option<(u32, HashMap<u16, u64>)>,
    inflight_diag: Option<(u32, HashMap<u16, u64>)>,
    sent_full: bool,
    next_diag_at: Instant,
}

impl Pacer {
    fn run(mut self) {
        loop {
            match self.inbox.recv_timeout(Duration::from_millis(150)) {
                Ok(AttCmd::Close) => return,
                Ok(AttCmd::Ping) => {}
                Ok(AttCmd::Ack { stream, update_id }) => match stream {
                    LSP_STREAM_STATE => {
                        if let Some((id, floors)) = &self.inflight_state
                            && *id == update_id
                        {
                            self.state_floors = floors.clone();
                            self.inflight_state = None;
                        }
                    }
                    LSP_STREAM_DIAG => {
                        if let Some((id, floors)) = &self.inflight_diag
                            && *id == update_id
                        {
                            self.diag_floors = floors.clone();
                            self.inflight_diag = None;
                            self.publish_acked_floors();
                        }
                    }
                    _ => {}
                },
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => return,
            }
            if !self.try_send_state() {
                return;
            }
            if self.wants_diags && !self.try_send_diags() {
                return;
            }
        }
    }

    /// Tombstone pruning (backend.rs `prune_diag_tombstones`) waits on
    /// every diag subscriber's acked floor; publish ours after each
    /// diag ack.
    fn publish_acked_floors(&self) {
        let backends = self.backends.lock().unwrap().clone();
        for backend in backends {
            if let Some(floor) = self.diag_floors.get(&backend.server_ref) {
                backend
                    .shared
                    .diag_acked
                    .lock()
                    .unwrap()
                    .insert(self.sub, *floor);
            }
        }
    }

    fn try_send_state(&mut self) -> bool {
        if self.inflight_state.is_some() {
            return true;
        }
        let backends = self.backends.lock().unwrap().clone();
        // Current per-backend sequences, live backends only — matching
        // the floor snapshot below. A stopped backend drops out of both
        // maps, so its departure surfaces once as a floor key with no
        // seq (the second `unchanged` clause) and then goes quiet
        // instead of re-triggering a send on every ack.
        let seqs: HashMap<u16, u64> = backends
            .iter()
            .filter(|b| !b.is_gone())
            .map(|b| (b.server_ref, b.shared.state_seq.load(Ordering::Relaxed)))
            .collect();
        // Nothing to send only if every current backend is at or below
        // its floor AND no backend the client still knows has vanished.
        let unchanged = seqs
            .iter()
            .all(|(r, seq)| self.state_floors.get(r).is_some_and(|f| seq <= f))
            && self.state_floors.keys().all(|r| seqs.contains_key(r));
        if unchanged {
            return true;
        }
        // Whole snapshot: one SERVER record per live backend. A stopped
        // (gone) backend is omitted, so its record disappears.
        let mut records = Vec::new();
        for backend in &backends {
            if backend.is_gone() {
                continue;
            }
            let info = backend.shared.info.lock().unwrap().clone();
            append_lsp_state_record(
                &mut records,
                &LspStateRecord::Server {
                    server_ref: backend.server_ref,
                    phase: info.phase,
                    progress_pct: info.progress_pct,
                    caps: info.caps,
                    epoch: info.epoch,
                    refused_edits: info.refused_edits,
                    rss: backend.rss_bytes(),
                    id: &backend.id,
                    msg: &info.msg,
                },
            );
        }
        self.state_id = self.state_id.wrapping_add(1);
        if !(self.sink)(msg_lsp_state(self.lsp_id, self.state_id, 0, &records)) {
            return false;
        }
        // The floor snapshot covers only live backends, so a vanished
        // one drops out of the cursor map too.
        let floors: HashMap<u16, u64> = backends
            .iter()
            .filter(|b| !b.is_gone())
            .map(|b| (b.server_ref, b.shared.state_seq.load(Ordering::Relaxed)))
            .collect();
        self.inflight_state = Some((self.state_id, floors));
        true
    }

    fn try_send_diags(&mut self) -> bool {
        if self.inflight_diag.is_some() || Instant::now() < self.next_diag_at {
            return true;
        }
        let backends = self.backends.lock().unwrap().clone();
        // The first update after subscribe is a FULL cache replay (the
        // drop-everything reset); afterwards, incrementals from the
        // floor. A FULL too large for one message is split: the first
        // chunk carries the FULL flag and advances the floor, so the
        // remainder flows as ordinary incrementals under the same
        // one-in-flight pacing — the payload never trips the receiver's
        // MAX_DECOMPRESSED guard.
        let full = !self.sent_full;
        let mut records = Vec::new();
        let mut new_floors = self.diag_floors.clone();
        let mut entries = 0usize;
        let mut any = false;
        'outer: for backend in &backends {
            let floor = *self.diag_floors.get(&backend.server_ref).unwrap_or(&0);
            // The seq atomic is the cheap gate: skip the map lock (and
            // the full scan behind it) when this backend published
            // nothing past our floor.
            if !full && backend.shared.diag_seq.load(Ordering::Relaxed) <= floor {
                continue;
            }
            // Snapshot the changed entries and encode after dropping the
            // lock, so record encoding never stalls the engine's
            // publishes.
            let mut changed: Vec<(PathBuf, crate::backend::FileDiags)> = {
                let diags = backend.shared.diags.lock().unwrap();
                diags
                    .iter()
                    .filter(|(_, f)| full || f.seq > floor)
                    .map(|(p, f)| (p.clone(), f.clone()))
                    .collect()
            };
            // Files in seq order so a chunk boundary leaves everything
            // unsent strictly above the new floor.
            changed.sort_by_key(|(_, f)| f.seq);
            for (path, file) in &changed {
                // In a FULL replay, absent files are unknown; empty
                // tombstones carry no information.
                if full && file.is_empty() {
                    let e = new_floors.entry(backend.server_ref).or_insert(0);
                    *e = (*e).max(file.seq);
                    continue;
                }
                if entries >= self.entries_max || records.len() >= self.bytes_max {
                    break 'outer;
                }
                // One file's diagnostics are bounded too, not only the
                // number of files. The checks above run *between* files, so
                // a single pathological one — a generated bundle, or one
                // syntax error cascading into hundreds of thousands of
                // diagnostics — appended all of them: the payload could pass
                // the client's decompression guard, at which point the
                // client refuses the whole update, never acks, and the
                // one-in-flight pacer never clears, so that client receives
                // no diagnostics again for the life of the attachment. The
                // count also has to fit `n`, which is a `u16`.
                //
                // Built into a scratch buffer so the count in the FILE
                // record is the number of DIAG records that actually
                // follow it, rather than a promise made before the budget
                // was consulted.
                let wire = wire_of(&self.root, path);
                // Cold entries decode here, off the cache lock.
                let diags = file.diags();
                append_file_diags(&mut records, &wire, file.hash, &diags, self.bytes_max);
                let e = new_floors.entry(backend.server_ref).or_insert(0);
                *e = (*e).max(file.seq);
                entries += 1;
                any = true;
            }
        }
        // Drop cursor entries for backends that are gone.
        new_floors.retain(|r, _| backends.iter().any(|b| b.server_ref == *r));
        if !any && !full {
            return true;
        }
        self.diag_id = self.diag_id.wrapping_add(1);
        let flags = if full { LSP_DIAG_FULL } else { 0 };
        if !(self.sink)(msg_lsp_diag(self.lsp_id, self.diag_id, flags, &records)) {
            return false;
        }
        // The reset has now been delivered (possibly as the first of
        // several chunks); the rest flows as incrementals from the
        // advanced floor.
        self.sent_full = true;
        self.next_diag_at = Instant::now() + self.latency;
        self.inflight_diag = Some((self.diag_id, new_floors));
        true
    }
}

/// Wire path for a diag cache entry: relative to the backend root when
/// under it (the usual case; attachment roots contain backend roots),
/// escaped absolute otherwise.
fn wire_of(root: &Path, path: &Path) -> String {
    text::wire_path(root, path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::WireDiag;
    use blit_remote::lsp::lsp_diag_records;

    fn diag(msg: &str) -> WireDiag {
        WireDiag {
            severity: 1,
            flags: 0,
            line: 0,
            col: 0,
            end_line: 0,
            end_col: 1,
            code: String::new(),
            source: "test".into(),
            msg: msg.to_string(),
        }
    }

    /// The FILE record's count must equal the DIAG records that follow it,
    /// under every budget. A file whose diagnostics run past the payload
    /// budget used to append them all — the client then refuses the update,
    /// never acks, and diagnostics wedge for that attachment.
    #[test]
    fn one_file_cannot_outgrow_the_payload_budget() {
        let diags: Vec<WireDiag> = (0..500).map(|i| diag(&format!("problem {i}"))).collect();

        // Generous budget: everything lands, and `n` says so.
        let mut records = Vec::new();
        let n = append_file_diags(&mut records, "a.rs", [0u8; 16], &diags, 1 << 20);
        assert_eq!(n, diags.len());
        assert_eq!(count_diags(&records), n);

        // Tight budget: fewer land, and `n` still matches what followed.
        let mut records = Vec::new();
        let n = append_file_diags(&mut records, "a.rs", [0u8; 16], &diags, 512);
        assert!(n > 0 && n < diags.len(), "expected a partial file, got {n}");
        assert_eq!(count_diags(&records), n);

        // Zero budget still emits the FILE record — the file is known to
        // have diagnostics, and a client that receives none for it would
        // read that as "clean".
        let mut records = Vec::new();
        let n = append_file_diags(&mut records, "a.rs", [0u8; 16], &diags, 0);
        assert_eq!(n, 0);
        assert_eq!(count_diags(&records), 0);
        assert!(!records.is_empty(), "the FILE record is always written");
    }

    /// Count DIAG records, and assert every FILE record's `n` matches the
    /// run that follows it.
    fn count_diags(records: &[u8]) -> usize {
        let mut total = 0usize;
        let mut declared = None;
        let mut run = 0usize;
        for rec in lsp_diag_records(records) {
            match rec {
                LspDiagRecord::File { n, .. } => {
                    if let Some(d) = declared.replace(n as usize) {
                        assert_eq!(d, run, "FILE count must match the DIAGs after it");
                    }
                    run = 0;
                }
                LspDiagRecord::Diag { .. } => {
                    run += 1;
                    total += 1;
                }
            }
        }
        if let Some(d) = declared {
            assert_eq!(d, run, "FILE count must match the DIAGs after it");
        }
        total
    }
}
