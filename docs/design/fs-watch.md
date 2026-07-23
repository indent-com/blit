# Filesystem State Sync

- **Status:** Accepted and implemented (`FEATURE_FS`, protocol feature bit 6)
- **Date:** 2026-07-21

Chosen over four sibling proposals (since removed from the tree) that
streamed filesystem _events_; their best ideas — staged snapshots,
torn-read handling, byte-window ACK, churn bound, 128-bit hashes — were
adopted here.

## Summary

The rejected proposals streamed _events_ and made the client cope with what
native watch APIs actually are: lossy, coalescing, platform-flavored
invalidation streams. Clients had to track generations, validate sequences,
request resyncs, pair renames, and re-fetch content on mismatch.

This proposal streams _state_. The server maintains a canonical replica of the
watched tree — names, metadata, and content — and sends each client ordered
diffs between the client's last-acknowledged view and the current view. This is
exactly blit's terminal model applied to a filesystem: `S2C_UPDATE` diffs a
grid against what the client last saw; `FS_UPDATE` diffs a tree.

The consequence is that loss, overflow, races, rename pairing, and recovery
stop being protocol concepts. A native queue overflow, a client that stalls
for a minute, and the initial snapshot are all the same thing on the wire: a
(possibly large) diff, delivered as a staged snapshot when incremental
delivery is not possible. The complete client obligation fits in a dozen
lines:

```text
live = {}; staging = none
on FS_UPDATE(sync_id, update_id, flags, records):
    if flags.RESET: staging = empty map
    for r in records:                      # into staging if active, else live
        UPSERT → m[r.path] = r             # metadata, and content if attached
        DELETE → drop m[r.path] and every path under it
        MOVE   → rename r.from subtree to r.to
    if flags.SYNC: live = staging; staging = none
    send FS_ACK(sync_id, update_id)
```

The staging map keeps the visible mirror coherent while a snapshot streams in:
applications never observe a half-enumerated tree, and recovery never empties
a UI. Snapshots stream as ordinary bounded updates rather than one giant
message.

A client that does only this is always correct. Everything else — hashing,
delta encoding, rename detection, overflow rescans, snapshot retention,
non-UTF-8 names — is the server's problem, by design. Server cost is higher
than the event-stream proposals and that trade is intentional: one server
implementation, many trivially thin clients (browser panes, CLI agents,
skills, future sync).

## Goals

- The thinnest possible correct client: apply records, ack. No state machine
  beyond a map, no error recovery paths, no platform knowledge.
- Content included, not bolted on: a synced client holds the current bytes of
  every regular file under the root (up to a size limit), kept current via
  server-computed deltas.
- Identical semantics on Linux, macOS, and Windows; native event backends, no
  idle polling.
- Bounded memory on both sides regardless of client speed, without a
  client-visible desync/resync protocol.
- Fit blit conventions: 1-byte opcodes, little-endian, LZ4, feature-bit gated,
  `S2C_FRAGMENT` for large messages, ACK-based pacing.

## Non-goals

- Delivering discrete filesystem _events_. Consumers that want "a file was
  saved" derive it from map transitions (the client library can surface the
  applied records as callbacks — it just applied them). A change that leaves
  state identical (touch-then-revert within one tick) is invisible. This is a
  feature of the model, not an accident.
- Bidirectional sync (client writes). The state model extends naturally
  (client sends UPSERTs), but that is a separate RFC.
- Hardlink identity, xattrs, atime, durability assertions.
- Persisting sync state across connections. Reconnect = new sync = one
  snapshot diff.

## Protocol

New `S2C_HELLO` feature bit (mutually exclusive with the other proposals —
whichever design is adopted takes bit 6):

```text
FEATURE_FS = 1 << 6
```

Opcodes occupy the `0x40` block in both directions. Gateway, proxy, and mux
forward them unmodified. All integers little-endian; 16 MiB frame limit and
[protocol.md](protocol.md) framing apply.

| Direction | Opcode | Name        | Layout                                                               |
| --------- | ------ | ----------- | -------------------------------------------------------------------- |
| C2S       | `0x40` | `FS_SYNC`   | `[nonce:2][flags:1][latency_ms:2][inline_max:4][path_len:2][path:N]` |
| C2S       | `0x41` | `FS_STOP`   | `[sync_id:2]`                                                        |
| C2S       | `0x42` | `FS_ACK`    | `[sync_id:2][update_id:4]`                                           |
| C2S       | `0x43` | `FS_FETCH`  | `[nonce:2][sync_id:2][path_len:2][path:N]`                           |
| S2C       | `0x40` | `FS_SYNCED` | `[nonce:2][sync_id:2][status:1][detail_len:2][detail:N]`             |
| S2C       | `0x41` | `FS_UPDATE` | `[sync_id:2][update_id:4][flags:1][records:LZ4]`                     |
| S2C       | `0x42` | `FS_FILE`   | `[nonce:2][status:1][data:LZ4]`                                      |
| S2C       | `0x43` | `FS_CLOSED` | `[sync_id:2][reason:1]`                                              |

### `FS_SYNC`

`flags`: bit 0 `RECURSIVE`, bit 1 `CONTENT` (attach file bytes to UPSERTs),
bit 2 `CROSS_FILESYSTEM` (descend into mount points), bit 3 `EXCLUDE_GIT`
(landed with `FEATURE_GIT`, see [git.md](git.md): any entry whose final
component is exactly `.git` — directory or gitfile — is omitted from
enumeration, watching, hashing, and all records; paths beneath it do not
exist for this sync. A pure name filter: fs sync never reads git data).
Symlinks are reported, never followed. `latency_ms` is the batching/settle window (0 → server default
20 ms, clamped to 1–1000). `inline_max` caps per-file inline content (0 →
server default 16 MiB); larger files sync metadata + hash only, bytes on
demand via `FS_FETCH`.

`path` is UTF-8, absolute or relative to the server's working directory, and
must exist. `FS_SYNCED.status`: `0` ok (detail = canonical root, UTF-8),
`1` not found, `2` permission denied, `3` resource limit, `4` other (detail =
message); on failure `sync_id` = `0xFFFF`.

**Paths and non-UTF-8 names:** every path a client sees is valid UTF-8,
relative to the root, `/`-separated. The server escapes bytes that are not
valid UTF-8 (Linux) and unpaired surrogates (Windows) as `%XX` / `%uXXXX`,
escaping literal `%` as `%25`, and keeps the reverse mapping so escaped paths
round-trip through `FS_FETCH`. Clients never see `OsStr`, WTF-8, or a
component encoding — the server does more so the client can do less.

### `FS_UPDATE`

`flags`: bit 0 `RESET` — begin a staged snapshot: create an empty staging map
and apply this and subsequent records to it. Bit 1 `SYNC` — atomically replace
the live map with staging (no-op if none is active). Both bits set on one
update is valid for small trees. Every sync starts with a `RESET … SYNC`
series (initial state = snapshot of the tree), split into bounded updates so
snapshot delivery interleaves with terminal, surface, and audio traffic. The
server may start a new `RESET … SYNC` series at _any_ time instead of
computing an incremental diff — after retention eviction, backend replacement,
native overflow, or whenever a full send is cheaper. Clients cannot
distinguish recovery from normal operation, which is the point. Updates
between `RESET` and `SYNC` may include live reconciliation caught during
enumeration; the swapped-in map is coherent as of the `SYNC`.

`records`, LZ4-compressed (`lz4_flex::compress_prepend_size`), decompressed a
sequence of length-prefixed records (`[record_len:4]` first, so unknown kinds
are skippable):

```text
UPSERT 0x01: [kind:1][entry_flags:1][path_len:2][path:N]
             [size:8][mtime_ns:8][mode:4][hash:16]
             [content_kind:1][content…]
DELETE 0x02: [kind:1][path_len:2][path:N]                  # prunes subtree
MOVE   0x03: [kind:1][from_len:2][from:N][to_len:2][to:M]  # moves subtree
```

`entry_flags` bits 0–1: type (0 file, 1 dir, 2 symlink, 3 other); bit 2
`UNREADABLE` (exists, content unavailable); bit 3 `NO_CONTENT` (over
`inline_max`, or `CONTENT` unset); bit 4 `UNSTABLE` (file changed repeatedly
while being read — content omitted, another upsert follows once it settles).
A symlink's content is its **target bytes** (git's model: blob = target),
so its `size` is the target length and `FS_FETCH` of a symlink returns the
target, never the file it points at — which is also what makes symlink
retargeting an ordinary CAS on the write side
([fs-write.md](fs-write.md) "Links").
`hash` is BLAKE3 truncated to 128 bits, computed over content — file bytes,
or a symlink's target bytes (zero for directories and `other`): ample
collision resistance for content addressing at half the per-record cost of
a full digest, and BLAKE3 is fast enough that hashing changed files is not
the bottleneck. `mode` is the Unix mode, synthesized on Windows.

`content_kind`: `0` none, `1` full bytes `[len:4][bytes]`, `2` delta
`[len:4][ops]` against the last content this client acked for this path —
LEB128 instruction stream: `0x01 COPY [offset][len]`, `0x02 INSERT [len][bytes]`.
Deltas are correct by construction (the server knows exactly what the client
holds, because updates are ordered over a reliable transport and acked); the
hash lets a paranoid client verify, but no mismatch-recovery path exists or is
needed. Deltas are a server optimization, never a client obligation: kind `1`
(full) is always valid, so a server may ship full-content-only and grow the
delta encoder later without any protocol or client change. The decoder a
client must carry is two instruction types. Updates exceeding the frame budget use
`S2C_FRAGMENT`, which the client transport already reassembles — record
encoding never sees it.

Content is read with verification: the server compares file identity, size,
and mtime before and after each read, retries once on mismatch, then emits the
entry with `UNSTABLE` and reschedules reconciliation. Torn reads are never
delivered as content.

Applying an update is atomic from the client's perspective: after the ack, the
map equals the tree as the server observed it at one settle tick (a consistent
cut of the server's index, not a filesystem-atomic snapshot — no such thing
exists on any of the three platforms).

### Pacing

`FS_ACK` is cumulative: it acknowledges every update through `update_id` as
applied. The server retains serialized sizes per unacked update and stops
producing when unacknowledged bytes reach the window (default 1 MiB) —
byte-based credit paces 100-byte metadata ticks and multi-MiB content updates
equally well, where a fixed in-flight count would not. While blocked, the
server does not queue updates — dirt accumulates in its index, and the next
update simply covers more. A slow client gets fewer, larger diffs and never
falls behind; a stalled client costs retention memory until the server evicts
its cursor and restarts it with a `RESET … SYNC` series. A stale `update_id`
is ignored; acking beyond the highest sent id is a fatal watch error.

If the tree churns so fast that a `RESET … SYNC` series cannot reach a
coherent `SYNC` within bounded work, the server restarts the series at most
twice, then closes the sync with reason `3` rather than looping.

### `FS_FETCH` / `FS_FILE`

The one pull in the protocol, for `NO_CONTENT` files. `FS_FILE.status`: `0` ok,
`1` not found, `2` unreadable, `3` other. `data` is LZ4 full content,
fragmented as needed. A file larger than `MAX_DECOMPRESSED` (64 MiB, the
protocol-wide receiver cap) is refused with status `3` before any bytes are
read — no compliant client could decompress it, and reading it would spike
server memory. Clients that never call `FS_FETCH` are still fully correct.

### `FS_CLOSED`

`reason`: `0` client request (response to `FS_STOP`), `1` root deleted or
renamed away, `2` permission lost, `3` backend failure, `4` resource limit.
After it, the sync_id is dead; clients may simply re-`FS_SYNC`.

## Server implementation

The server does more so clients can do less. Three layers, all in a new
`blit-fssync` crate plus wiring in `blit-server`:

**Native hint backends** — inotify on Linux, FSEvents on macOS, overlapped
`ReadDirectoryChangesW` on Windows (v1 reaches all three through the
`notify` crate). They are demoted to producing exactly one thing: a
**dirty set** of paths. Rename cookies and action codes are used only as
locality hints; every native loss signal (`IN_Q_OVERFLOW`, `MustScanSubDirs`,
`ERROR_NOTIFY_ENUM_DIR`, internal channel overflow) degrades to "root is
dirty". No backend behavior is client-visible.

**Canonical index** — per synced root, shared and refcounted across clients:
a persistent (structurally shared) tree map `path → (type, size, mtime_ns,
mode, blake3)`. Each settle tick stats the dirty set, updates the index, and
publishes an immutable snapshot; snapshots share unchanged subtrees, so
holding several is cheap. Content lives in a content-addressed **blob store**
(BLAKE3 → bytes, LRU by total size) shared by all syncs — identical files and
unchanged-across-rename files cost one entry, and delta bases are found by
hash. "Efficient but not ultra-optimized" is the bar: a `BTreeMap` clone per
tick with a dirty-subtree copy is acceptable for v1; structural sharing is the
recommended implementation, not a wire requirement.

**Per-client differ** — each client cursor is one pointer to the snapshot it
last acked, plus its in-flight updates. An update is computed by walking two
snapshots' diff: trivial where subtrees are shared pointers (skip), records
where they differ. **Move detection is a diff-time join**, not event pairing:
entries that disappeared and appeared within one tick with the same file
identity (`(dev, ino)` on Unix, volume + file ID on Windows) or the same
content hash become `MOVE`; anything ambiguous decays to `DELETE` + `UPSERT`.
Snapshot retention per client is budgeted (default 32 MiB of unshared nodes);
over budget, the cursor is dropped and the client is restarted with a
`RESET … SYNC` series.

Nothing here runs under the session mutex; the differ and blob hashing run on
blocking-pool threads and deliver serialized updates through the normal
per-client writer, interleaved with terminal, surface, and audio traffic by
the existing scheduler and `S2C_FRAGMENT` fairness.

## Limits and defaults

| Knob                        | Default | Env                    |
| --------------------------- | ------- | ---------------------- |
| Settle / batching window    | 20 ms   | `BLIT_FS_LATENCY_MS`   |
| Inline content limit        | 16 MiB  | `BLIT_FS_INLINE_MAX`   |
| Blob store (process-wide)   | 256 MiB | `BLIT_FS_BLOB_MAX`     |
| Snapshot retention / client | 32 MiB  | `BLIT_FS_RETAIN_MAX`\* |
| Syncs per connection        | 64      | `BLIT_FS_MAX_SYNCS`    |
| Indexed entries per root    | 1 M     | `BLIT_FS_MAX_ENTRIES`  |
| Unacked bytes per sync      | 1 MiB   | `BLIT_FS_WINDOW`       |

\* Not needed in the implemented architecture: a sync engine holds at most
two whole-index references (its shadow and the latest published snapshot),
so per-client retention is bounded by design and this knob does not exist.

Watch-descriptor exhaustion fails `FS_SYNC` at arm time with status `3`
(`FS_STATUS_RESOURCE_LIMIT`), as do permission and not-found failures with
their matching statuses. The entry budget is enforced by the shared root's
reconciler, whose enumeration runs after `FS_SYNCED`, so exceeding it —
whether on the initial scan or as the tree grows later — closes the sync
with `FS_CLOSED` reason `4` rather than a synchronous refusal. Incremental
reconciliation enforces the same cap as the initial scan. On Linux the
server leaves headroom under `fs.inotify.max_user_watches`.

## Comparison with the rejected event-stream designs

|                    | invalidation streams                                             | verified events + deltas                       | state sync (this)                    |
| ------------------ | ---------------------------------------------------------------- | ---------------------------------------------- | ------------------------------------ |
| Wire model         | event / invalidation stream                                      | verified events + content deltas               | state diffs                          |
| Client must handle | sequences, DESYNC, resync, generations, rename pairing, re-reads | generations, sequences, hash-mismatch recovery | apply + ack                          |
| Loss recovery      | client-driven resync + barrier                                   | server rescan, synthetic events                | invisible (`RESET … SYNC` restaging) |
| Content            | out of scope                                                     | delta stream with ack bases                    | integral, content-addressed          |
| Non-UTF-8 names    | component encoding, client compares bytes                        | lossy or WTF-8                                 | server-side escaping                 |
| Server memory      | lowest (no index)                                                | index per watch                                | index + snapshots + blobs (highest)  |
| Server CPU         | lowest                                                           | stat verification                              | stat + hash + diff                   |
| Event fidelity     | highest (invalidations per change)                               | high                                           | state transitions only               |

Choose the event-stream designs if consumers need change _notifications_ with
minimal server cost. Choose this design if consumers need the _tree_ — which
is what agents tailing builds, browser file views, and sync features actually
consume — and thin clients matter more than server frugality.

## Security

The server already hands clients a shell, so syncing adds surface for
denial-of-service, not privilege. The mandatory mitigations are the budget
table above, request validation (unknown flags, NULs, oversized paths
rejected), prompt teardown on disconnect, and never logging escaped path
bytes as trusted text.

## Implementation

1. `blit-remote` (`crates/remote/src/fs.rs`): opcodes, record codecs,
   `FEATURE_FS`, and the `FsMirror` reference reducer; TypeScript
   counterparts in `@blit-sh/core` (`js/core/src/fs.ts`). Both pin the same
   byte fixtures, so codec drift fails on one side or the other.
2. `blit-fssync` (`crates/fssync`): a **shared root** per
   `(path, recursive, cross_filesystem)`, refcounted across every sync on
   every connection — one native watcher and one hint-driven reconciler
   owning the canonical index and publishing immutable snapshots — plus a
   **per-sync engine** holding only client state: shadow snapshot, held
   content map, ack window, staged `RESET … SYNC` assembly, byte-window
   pacing. Content is deduped through the process-wide content-addressed
   blob store (`BLIT_FS_BLOB_MAX`): the first sync to read a file teaches
   the reconciler its hash, and every other sync serves the bytes from
   memory. The blob store feeds the delta encoder — single-span (common
   prefix/suffix), which covers appends and contiguous edits and falls
   back to full content otherwise; identical rewrites send metadata only.
   The property test is the spec, now over two independently-acked
   clients of one shared root: for arbitrary mutation sequences and
   arbitrary ack timing, applying updates always yields the final tree.
3. Native backends via the `notify` crate (inotify / FSEvents /
   `ReadDirectoryChangesW`), demoted to dirty hints; semantics tests are
   backend-independent by construction — same suite, three CI targets.
4. Clients: `blit fs sync <path> [--content] [--once] [--json]`
   (`crates/cli/src/fs.rs`), and `syncFs(path)` on `BlitConnection` /
   `BlitWorkspace` in `@blit-sh/core` returning a live map plus per-record
   callbacks, with automatic acknowledgment and fetch-on-demand.
