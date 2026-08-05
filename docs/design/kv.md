# RFC: Server KV Store (CAS)

- **Status:** Draft
- **Date:** 2026-07-25
- **Companion to:** [fs-write.md](fs-write.md), [fs-watch.md](fs-watch.md),
  [../protocol.md](../protocol.md), [../ide.md](../ide.md)

## Summary

A small **host-local key→value store on the blit server**, with
compare-and-swap writes and prefix-watch subscriptions. Its first
consumer is the editor: the **list of opened files** and, for each, the
**modified buffer** when one exists — so editors become what terminals
already are: always-on server-backed state that a client _views_, not
state a client _owns_. Reload the tab, connect from another device, or
crash mid-edit: the open files and their unsaved edits are still there,
because they never lived only in the tab.

Today they live only in the tab. A dirty buffer exists solely in the
live CodeMirror document; there is no `beforeunload` handler anywhere
in `js/ui`, so a reload rides the fire-and-forget blur/hide autosave
and a crash loses the buffer outright. Worse, a buffer parked in the
save-**conflict** state (CAS refused — the file changed on disk under
it) has no fallback at all: teardown autosave fails the same CAS and
the user's version evaporates. The open-file list is no sturdier: pane
tiles survive same-tab reload via the URL hash, but the background dock
is a session-only signal and a second client sees none of it.

This is a step toward the buffer RFC [fs-write.md](fs-write.md)
anticipates (§ Forward compatibility), taken deliberately **below** it:
no shared editing, no server-side buffer engine, no LSP
`didOpen`-from-buffer. The store is a dumb, durable, CAS-guarded byte
map; the editor semantics live entirely in the client, expressed as
keys. That keeps the primitive general — layouts, tree-expansion state,
and future panel state have the same shape and get the store for free —
and keeps the hard collaborative-editing problems in the later RFC
where they belong. A second consumer ships in the same RFC and proves
the generality: the **workspace-roots registry** moves out of the
gateway config onto the server that owns the paths
(§ Second consumer).

The design reuses the family standards wholesale: BLAKE3-128
content-hash CAS with the zero-hash absent sentinel
([fs-write.md](fs-write.md) § Conflict model), the unified status table
([git.md](git.md)), nonce request/response with one reply per nonce,
`inline_max` + fetch for large values ([fs-watch.md](fs-watch.md)), and
echo attribution by hash, not token. A reader who knows the fs family
already knows this one.

## Wire

**Feature bit 9** (`FEATURE_KV`) — the first free `S2C_HELLO` bit
(fs=6, git=7, lsp=8; [../protocol.md](../protocol.md)). Opcodes take
the free `0x70` block in both directions. Gateway, proxy, and mux
forward them unmodified.

| Dir | Opcode | Name        | Layout                                                        |
| --- | ------ | ----------- | ------------------------------------------------------------- |
| C2S | `0x70` | `KV_OPEN`   | `[nonce:2][flags:1][inline_max:4][prefix_len:2][prefix:N]`    |
| C2S | `0x71` | `KV_STOP`   | `[kv_id:2]`                                                   |
| C2S | `0x72` | `KV_ACK`    | `[kv_id:2][update_id:4]` — cumulative                         |
| C2S | `0x73` | `KV_PUT`    | `[nonce:2][flags:1][base:16][key_len:2][key:N][value:LZ4]`    |
| C2S | `0x74` | `KV_FETCH`  | `[nonce:2][key_len:2][key:N]`                                 |
| S2C | `0x70` | `KV_OPENED` | `[nonce:2][kv_id:2][status:1][detail_len:2][detail:N]`        |
| S2C | `0x71` | `KV_UPDATE` | `[kv_id:2][update_id:4][flags:1][records:LZ4]`                |
| S2C | `0x72` | `KV_DONE`   | `[nonce:2][status:1][hash:16][mtime_ns:8]` — one per `KV_PUT` |
| S2C | `0x73` | `KV_VALUE`  | `[nonce:2][status:1][hash:16][data:LZ4]` — one per `KV_FETCH` |
| S2C | `0x74` | `KV_CLOSED` | `[kv_id:2][reason:1]` — server-initiated (§ Watch)            |

**Keys** are raw UTF-8, length-prefixed on the wire (no escaping — the
fs family escapes because paths embed in records; KV keys are always
length-delimited). ≤ 256 bytes, no NUL, non-empty. Namespacing is by
convention with `/` separators (`editor/buf/…`); the store itself is
flat — a prefix is a filter, not a directory.

**`KV_PUT.flags`:** bit 0 `NO_CAS` (ignore `base`, unconditional), bit
1 `DELETE` (value must be empty; the entry is removed — a delete is a
put of absence, folded rather than given its own opcode, the
[fs-write.md](fs-write.md) `FS_OP` economy), bit 2 `DURABLE` (fsync
before replying; default trades durability for latency, as fs-write).

**CAS** is [fs-write.md](fs-write.md) § Conflict model verbatim, over
the **value bytes**: `base` non-zero → put/delete iff the current value
hashes to `base`, else `CONFLICT`; `base` zero → create-exclusive
(`CONFLICT` if the key exists; for `DELETE`, zero base means
"delete iff absent" and is `INVALID` — meaningless); `NO_CAS` →
unconditional. On `CONFLICT`, `KV_DONE.hash` carries the current hash
so the client rebases without a round trip. On success it carries the
new value hash (zero for a delete). The client never hashes; it chains
CAS off returned hashes (`lastWrittenHash` discipline, including the
echo-suppression rule below).

**Statuses** are the unified table ([git.md](git.md)) plus lsp's `10
WARMING` convention and fs-write's `11 CONFLICT`. `KV_PUT` can answer
`OK`, `CONFLICT`, `TOO_LARGE`, `BUDGET`, `PERMISSION`, `INVALID`.
`KV_FETCH` of an absent key is `NOT_FOUND`.

### Watch

`KV_OPEN` subscribes to a **prefix** (empty = whole store) and returns
a `kv_id`. The prefix is a literal byte prefix — no glob syntax:
subscribe to `roots` for that one document, `editor/buf/` for the
buffer family, `editor/` for both editor families at once. The server
replies `KV_OPENED`, then pushes an initial snapshot followed by live
changes as `KV_UPDATE` batches of records:

```text
UPSERT  key, hash:16, size:4, mtime_ns:8, content?   (content iff size ≤ inline_max)
DELETE  key
```

`flags` bit 0 marks the batch that completes the initial snapshot.
Values over the subscriber's `inline_max` arrive as metadata + hash
only; the client fetches on demand (`KV_FETCH`) — fs-watch's
`inline_max` contract, so a watcher of `editor/open/` (tiny values)
inlines everything while a watcher of `editor/buf/` (whole buffers)
pulls only what it restores. Multiple subscriptions per connection are
fine, and `KV_STOP` closes one.

**Retention.** `KV_ACK` is cumulative: it advances the subscription's
acked floor, and the server retains each queued update's wire size
until the floor passes it. Queued-unacked bytes per subscription are
budgeted (`BLIT_KV_UNACKED_MAX`, § Budgets); a subscription that
breaches it — a stalled client whose updates pile up in the outbox —
is dropped, with `KV_CLOSED` reason `4` (`RESOURCE_LIMIT`) emitted
through the same outbox so a client that eventually drains learns of
it in order. Snapshot chunks count toward the same budget: a client
that never drains its snapshot is the same failure mode as one that
never acks. After `KV_CLOSED` the `kv_id` is dead — a late `KV_ACK`
for it is ignored, the close/ack race being benign — and a client that
still wants the prefix re-opens with `KV_OPEN`. The drop is lossless
because updates carry state, not events: the fresh snapshot _is_ the
recovery, nothing needs replaying. Reasons follow the fs numbering
([fs-watch.md](fs-watch.md) § `FS_CLOSED`); only `4` is produced
today. Compatibility rides [../protocol.md](../protocol.md)'s rule —
new message types take new opcodes, and clients ignore opcodes they do
not know (the `js/core` dispatch drops unknown types) — so a client
predating `KV_CLOSED` sees only a watch gone silent, exactly what its
stall was already producing.

**Echo attribution is by hash** ([fs-write.md](fs-write.md) § Echo): a
writer records the hash `KV_DONE` returned; when its own `UPSERT`
arrives on a subscription it matches and is not re-applied. A
byte-identical concurrent write mis-attributes exactly as in fs-write,
and is benign for the same reason.

Puts and fetches are store-global — no `kv_id` field. The fs family
scopes them to a `sync_id` because a sync resolves a root; the KV store
is one flat map per server, so subscription identity and operation
addressing are simply unrelated. A client may put without ever opening
a watch.

## Storage

The server persists **nothing** today — terminals survive client
disconnects only because the server process holds the PTYs, and die
with it. The KV store is blit's first at-rest state:

- **One [redb](https://github.com/cberner/redb) database** at the
  platform state path (`$XDG_STATE_HOME/blit/kv.redb`,
  `~/Library/Application Support/blit/kv.redb` on macOS;
  `BLIT_KV_PATH` overrides): a single table, key bytes →
  `[mtime_ns:8][value…]`. redb is the tree's first embedded-storage
  dependency, taken deliberately: pure Rust, actively maintained, a
  stable file format, and a copy-on-write B-tree whose atomic commits
  mean a crash sees the old store or the new, never a torn one. An
  earlier draft persisted value-per-file with percent-escaped-key
  filenames and zero dependencies; it died on arithmetic — a legal
  256-byte wire key whose escaped form exceeds `NAME_MAX` is an
  illegal filename, and a storage layer that can refuse a legal key is
  a bug wearing a design's clothes. redb has no key-encoding problem
  to solve.
- **`DURABLE`** maps to an immediate (fsynced) commit; the default
  commit is eventual-durability — the same latency-over-durability
  default as fs-write, here as a per-commit redb knob rather than a
  temp-file fsync.
- **An in-memory map** (key → value, hash, mtime) is loaded once at
  startup — hashes are not persisted; BLAKE3-128 recomputes at memory
  speed over a ≤ 256 MiB store — and is the source of truth for CAS
  and watches; redb is its write-behind, commits riding a dedicated
  writer thread fed in mutation order (queued mutations batch into one
  transaction, so a `DURABLE` commit's fsync also hardens everything
  ordered before it, and a non-`DURABLE` put is acked as soon as the
  in-memory mutation lands). All mutations serialize on one store
  lock, so the compare-hash-and-write section is trivially race-free
  server-side (the [fs-write.md](fs-write.md) blit-vs-external window
  does not exist: the server is the only writer of its own database,
  and an external mutator of it is out of contract).

Conceded cost of the engine: `ls`/`cat` no longer debug the store —
`blit kv ls|get` is the inspection tool — and the state is one file,
one basket. The copy-on-write commit discipline is what makes the
basket acceptable; a backup is one `cp` of a crash-consistent file.

Entries persist across server restarts — deliberately _more_ durable
than terminals. A parked buffer surviving reboot is the feature; a PTY
surviving reboot is impossible. There is no TTL and no eviction:
over-budget writes are **refused** (`BUDGET`), never silently evicted —
an evicted "unsaved buffer" is data loss wearing a cache's clothes
(§ Budgets; the honest-refusal stance of
[fs-write.md](fs-write.md) § Operation set).

## First consumer: editor state

Two key families, all values minted by the client. The store neither
parses nor validates them — these shapes are a `js/ui` convention
documented here, not wire schema.

**Keys embed the absolute path and nothing else.** The client's own
maps key on `(connectionId, path)`, but the connectionId is a
client-local remote _name_ — two clients reach the same host under
different names, and the store is already per-host. Embedding a
connection name in a key would silently shard the state this RFC
exists to share; the connection identity is implicit in which server
you asked.

**`editor/open/<abs-path>`** — presence = this file is open somewhere.
Value: small JSON `{ "at": mtime, "cursor": [line, col], "scroll":
top }`. Written `NO_CAS` — two tabs updating cursor metadata may race
and last-writer-wins is correct (both agree the file is open, and a
cursor is advisory). Deleted when the last view of the file closes
(dock ✕, tile close without background).

**`editor/buf/<abs-path>`** — present iff the buffer has unsaved edits.
Value: `[ver:1][base:16][content…]` — `ver` = 0, `base` = the **disk
content hash** (`FsNode.hash`) the buffer diverged from, `content` =
the full buffer bytes. Written with **CAS chained off the previous
put** (zero on first divergence — create-exclusive), debounced ~1 s
after the last edit and flushed on the autosave triggers (blur, tab
hide, teardown). Deleted (CAS'd on the last written hash) when a save
lands on disk or the user discards.

The `base` field is what makes restore honest. On editor mount, the
client fetches `editor/buf/<path>`:

- absent → load disk, clean editor (today's path).
- present, `base` == current disk hash → restore the buffer as the
  dirty content; the user is exactly where the crash/reload left them.
- present, `base` ≠ current disk hash → the disk moved under the parked
  buffer. Surface the existing conflict UI (Reload / Overwrite /
  Compare) — the same three-way the CAS save path already owns.

This closes the worst hole in the current model for free: a buffer
whose disk save keeps refusing `CONFLICT` still parks in the store
(the KV put chains on KV hashes, not disk hashes), so "the file
changed under me" stops being a countdown to data loss. The remaining
exposure is honest and small: the debounced put rides the same
fire-and-forget triggers autosave does, so a crash can lose at most
the final debounce window (~1 s) of typing.

**Two namespaces, kept orthogonal** ([fs-write.md](fs-write.md)
§ Forward compatibility, contract 2): the KV layer CASes on hashes of
**KV value bytes**; the `base` _inside_ a buffer value references
**disk** content space. They are never compared to each other. A save
is still `FS_WRITE ifHash` against disk (contract 3) — the KV store
never writes files.

**Cross-client behavior falls out.** A second client watching
`editor/open/` sees the first's open files and shows them (the
background dock is the natural landing — they arrive as parked
editors). Two clients editing the same file both put `editor/buf/<p>`;
the CAS chain makes the second put `CONFLICT`, and the client surfaces
it — crude, disclosed, and correct: this RFC parks buffers, it does not
merge them. Real co-editing is the buffer RFC's problem.

**Honest weakness — rename.** Buffer keys embed paths, so an external
`mv` orphans a parked buffer (contract 1's buffer-identity question,
still unforeclosed). A client that observes the fs `MOVE` record may
migrate the key (get → put → delete, each CAS'd); one that doesn't
leaves an orphan that restore never finds. Bounded loss, listed, and
the reason keys are a client convention: the migration needs no server
feature.

## Second consumer: workspace roots

Workspace roots today live in the **gateway** config
(`blit.roots`, the `name = remote:path` line format; `js/ui/src/storage.ts`
parses it and mutates via `roots-add/remove/toggle/reorder` over the
config WebSocket) — a server-backed KV in all but name, minus every
property this RFC adds: string-only, last-writer-wins, no CAS, no
watch semantics beyond a whole-file broadcast, and scoped to the
gateway rather than to the server that actually owns the paths. Roots
move into the store; the gateway copy becomes a legacy fallback.

**One key, `roots`, holding the whole ordered list** — the same
`name = value` line format minus the remote prefix (a root stored _on_
a server names a path on that server; the remote name was only ever
the client's routing label, the connectionId argument again). Roots
are an _ordered_, human-edited list: per-entry keys would trade one
rare CAS retry for a rank-maintenance scheme, the wrong trade at human
edit rates. Every mutation is read-modify-write CAS'd on the previous
hash; the Roots overlay retries on `CONFLICT` by re-reading — at human
rates the retry is invisible, and two clients editing simultaneously
converge instead of silently dropping one side's edit (the
last-writer-wins hazard the gateway scheme has today).

**Re-scoping is the feature and the conceded cost in one.** Stored
per-server, a root travels with the host: every client that connects
sees the same roots, with no gateway in common required. The cost: the
picker's list becomes the union over _connected_ servers, so an
offline server's roots are invisible until it connects — defensible
(a root you cannot reach is not actionable) but a real behavior
change, stated.

**Migration:** on first connect to a `supportsKv` server whose `roots`
key is absent, the client seeds it from the gateway list's entries
whose remote resolves to this connection, then treats the store as
authoritative for that server. Servers without `FEATURE_KV` keep the
gateway path indefinitely; the two lists union in the picker during
the transition. No server-side migration code — the seed is three KV
calls from the client that already knows both worlds.

## Budgets

| Knob                  | Default | Env                   |
| --------------------- | ------- | --------------------- |
| Key length            | 256 B   | (fixed)               |
| Per-value size        | 4 MiB   | `BLIT_KV_VALUE_MAX`   |
| Store total bytes     | 256 MiB | `BLIT_KV_TOTAL_MAX`   |
| Entries               | 16384   | `BLIT_KV_MAX_ENTRIES` |
| Puts in flight / conn | 16      | `BLIT_KV_INFLIGHT`    |
| Subscriptions / conn  | 16      | `BLIT_KV_MAX_SUBS`    |
| Unacked bytes / sub   | 16 MiB  | `BLIT_KV_UNACKED_MAX` |

Over-limit puts refuse with `TOO_LARGE`/`BUDGET`; nonce
request/response is the C2S backpressure (no credit window), and the
S2C subscription path is bounded per subscription: queued-unacked
bytes past `BLIT_KV_UNACKED_MAX` drop the subscription with
`KV_CLOSED` (§ Watch). Dropping a watcher is safe where evicting
stored state is not — a re-open rebuilds it from a fresh snapshot, so
the honest-refusal stance costs nothing here; the 16 MiB default holds
four maximum-size values in flight before declaring a client stalled.
A 4 MiB value cap deliberately undercuts fs-write's 16 MiB write cap:
a buffer larger than that should raise eyebrows, and the cap is an env
knob, not an architecture.

## Security posture

The store carries file contents (buffers), so it inherits the fs
family's read posture, and it accepts writes, so it inherits the
write-side gate: **`BLIT_KV=0`** refuses every `KV_*` at dispatch
(`PERMISSION`, one reply per nonce) — no advertisement, the
[fs-write.md](fs-write.md) dispatch-gate precedent, feature bit spent
anyway since the family is new. No path resolution exists to confine:
keys never touch the filesystem API (they are table entries in one
database, not filenames), so the traversal class fs-write § Path
validation fights cannot arise — the one structural safety advantage
of a flat map. The database and its parent directory are created
`0600`/`0700`. Multi-client visibility is the _point_, and the ceiling
is unchanged: every client that can open the store can already open a
PTY.

**The store is flat across sessions, and that is visible where a server
is shared.** A cloud sandbox runs one server per session, so the
question does not arise there. A local desktop server is one per
`computer.id` and serves every session on the machine, so session B can
watch or fetch session A's keys — including `editor/buf/<abs-path>`,
whose value is an unsaved buffer's contents and whose key is an absolute
path. The access boundary is the same one the already-global PTY table
draws, and both are reachable by anyone holding the connection secret;
what is new is that buffer text now sits **at rest** in a file rather
than only in a live PTY. Accepted deliberately: a per-session namespace
would take away the cross-client visibility this family exists to
provide, and the disclosure is to sessions that could already read the
same files through the fs family. An operator who needs the separation
runs separate servers, or `BLIT_KV=0`.

## Client surface

`BlitConnection`, the `syncFs`/`openRepo`/`openLsp` pattern: gated on
`FEATURE_KV`, snapshot capability `supportsKv`.

```ts
kvPut(key, value, { ifHash?, create?, durable? }): Promise<{ hash, mtimeNs }>
kvDelete(key, { ifHash? }): Promise<void>
kvFetch(key): Promise<{ hash, value } | null>
watchKv(prefix, { inlineMax?, onRecord, onClosed }): Promise<KvWatchHandle>
```

Option mapping is `writeFile`'s exactly: `ifHash` → CAS, `create` →
zero-base create-exclusive, neither → `NO_CAS` (the shell-`>` default).
A KV conflict rejects with the same `FsConflictError` shape — `hash`
carrying the current value hash — so the editor's existing
rebase-and-retry reflexes transfer unchanged.

`js/ui` builds the editor consumer on top: a small `ide/serverState.ts`
owning the debounced buffer puts (hooking the existing autosave
triggers), the open-markers, and the mount-time restore; it follows the
`connGen`/retry discipline every other handle already follows for
re-establish. On `S2C_HELLO` re-establish, watches are reset with the
other syncs and re-opened by the same effects; puts in flight reject
and the debouncer simply re-fires — the store's CAS makes the retry
safe. A server-initiated `KV_CLOSED` fires the same `onClosed` (with
the reason) and takes the same recovery: the handle is dead, the
consumer re-`watchKv`s, and the fresh snapshot replaces the mirror.

CLI: `blit kv get|put|rm|ls [--prefix P] [--if-hash H] [--watch]` —
the store is also a handy host-local scratch space for scripts, which
is not a goal but falls out free.

## Out of scope (with triggers)

- **Server-side buffer engine / co-editing / OT-CRDT** — the store
  parks bytes; it never merges. Trigger: real-time co-edit product
  ([fs-write.md](fs-write.md) § Forward compatibility).
- **LSP `didOpen`-from-buffer** — diagnostics on unsaved parked
  buffers. Trigger: [lsp.md](lsp.md)'s buffer-as-byte-source line.
- **Cross-host sync** — the store is per-server by design; roaming
  state between hosts is a hub/gateway product question. Trigger: a
  multi-host workspace product.
- **TTL / eviction / compaction** — refusal over eviction in v1.
  Trigger: real deployments hitting `BUDGET` on legitimate state.
- **Value deltas** — full values only; buffers are small and LZ4'd.
  Trigger: measured put bandwidth pain (then fs-watch's
  `content_kind 2` delta shape is the template).
- **Layouts / tree state / further consumers** — roots prove the
  pattern; layouts and tree-expansion state compose the same way with
  zero server work, each as its own `js/ui` change. Not scheduled
  here.
- **Multi-key transactions** — every consumer so far is one key per
  logical unit (roots deliberately so). Trigger: a consumer whose
  invariant genuinely spans keys; redb has native transactions
  waiting, so the cost then is wire design, not storage.

## Rollout

1. `crates/remote`: `pub mod kv` — `KV_*` opcodes + codecs, the
   fs/git/lsp module pattern; TS mirror in `js/core/src/kv.ts`; byte
   fixtures both sides.
2. `crates/server` (or a small `crates/kv`): the store — the `redb`
   dependency, open/load at first use, write-behind commits, store
   lock, budgets, dispatch gate; a `0x70`–`0x74` opcode-range dispatch
   before the session mutex, the fs/git/lsp shape.
3. `js/core`: `kvPut`/`kvDelete`/`kvFetch`/`watchKv` +
   `supportsKv` + echo suppression.
4. `js/ui`: `ide/serverState.ts` — buffer parking on the autosave
   triggers + mount-time restore (`editor/buf/`), then open-markers +
   cross-client dock landing (`editor/open/`).

   _Shipped on `tabs/` instead_ (`ide/openTabs.ts`): the tab registry
   already records every opened tab — diffs, commits and web panes
   included, not just editor files — keyed by a deterministic id, so
   the dock is derived by watching that prefix and subtracting what the
   client currently displays. `editor/open/` remains the right home for
   per-file cursor/scroll metadata; it is no longer needed to answer
   "what is open".

5. `js/ui`: roots on KV — `roots` key read/watch/CAS mutations in
   `storage.ts`, gateway seed-and-fallback (§ Second consumer).
6. `blit kv` CLI.

## Top risks

1. **Restore correctness.** A wrong `base` comparison on mount silently
   resurrects a stale buffer over newer disk content — data loss with a
   UI that looks intentional. The mount flow must treat "base ≠ disk"
   as conflict, never auto-apply. Highest.
2. **Debounce vs. teardown races.** A buffer put in flight while the
   editor tears down and deletes the key (save landed) can interleave;
   the CAS chain makes the outcome safe but the client must not retry a
   `CONFLICT` delete blindly. The `lastWrittenHash` discipline is
   load-bearing here exactly as in fs-write.
3. **First at-rest state, first storage dependency.** The server has
   never owned persistent data; one `kv.redb` file now carries user
   file contents and the roots registry. Deployments that treat blit
   servers as stateless (containers, ephemeral hosts) silently lose
   the durability story — worth a line in server docs, not a design
   change. And the store's health now rides a third-party format:
   mitigated by redb's stable file format and the store's small size
   (a full re-seed from clients is cheap), but a dependency bug is now
   a data bug, which zero-dependency blit has never had before.
