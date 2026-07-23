# RFC: Filesystem Writes

- **Status:** Implemented (rides `FEATURE_FS`, protocol feature bit 6;
  no new bit) — the disk-write side; the Monaco pane (§ Rollout step 6) is
  separate `js/ui` work.
- **Date:** 2026-07-23
- **Companion to:** [fs-watch.md](fs-watch.md), [git.md](git.md), [lsp.md](lsp.md)

## Summary

This is the RFC [fs-watch.md](fs-watch.md) defers ("Bidirectional sync
(client writes) … is a separate RFC") and [lsp.md](lsp.md) gates its
write-shaped features on ("all write-shaped features wait for the
mutation/buffer RFC"). It supersedes both lines. Its goal is a
credible browser IDE: enough to build a Monaco editor and a file
explorer on top of blit — content writes with conflict detection, plus
the directory operations a file tree needs.

It deliberately **narrows** fs-watch's suggested shape. Writes are _not_
client-pushed `UPSERT` records into the state stream — that reintroduces
the N-writer version-ownership hazard [lsp.md](lsp.md) closes by never
having it. Writes are nonce request/response side-band operations
against **disk**: the pull side of `FS_FETCH` inverted. The server stays
the sole author of every mirror. A write lands on disk, the reconciler
re-indexes, and the change re-enters _all_ mirrors — including the
writer's own — through the existing echo path (fs-watch's per-client
differ). The thin-client invariant holds: a client that only applies
and acks needs zero new code; only a client that _writes_ learns the new
messages.

The model is last-writer-wins on disk, guarded by compare-and-swap on
the content hash fs-sync already maintains. No operational transform, no
CRDT, no client-side buffers, no multi-file transaction — each has an
explicit trigger for a later RFC (§ Out of scope).

## Wire

**No new feature bit.** The write family rides `FEATURE_FS`
(bit 6): the whole `FS_*` family, reads and writes, ships together, and
`S2C_HELLO` bits are scarce — 0–8 are taken (fs=6, git=7, lsp=8;
[protocol.md](../protocol.md)). `BLIT_FS_WRITE=0` still offers read-only
sync (§ Security), but as a **dispatch gate** rather than an
unadvertised bit: the server answers `FS_WRITE`/`FS_OP` with `FS_DONE`
`PERMISSION` without executing, so every nonce keeps its one response.
The conceded cost: a client cannot see read-only-ness in the handshake —
it learns on the first refused write and grays out editing then.
Opcodes take the free `0x44`/`0x45` slots in the fs `0x40` block (git
owns `0x50`). Gateway, proxy, and mux forward them unmodified.

| Dir | Opcode | Name       | Layout                                                                                            |
| --- | ------ | ---------- | ------------------------------------------------------------------------------------------------- |
| C2S | `0x44` | `FS_WRITE` | `[nonce:2][sync_id:2][flags:1][base:16][mode:4][content_kind:1][path_len:2][path:N][content:LZ4]` |
| C2S | `0x45` | `FS_OP`    | `[nonce:2][sync_id:2][op:1][flags:1][base:16][mode:4][a_len:2][a:N][b_len:2][b:N]`                |
| S2C | `0x44` | `FS_DONE`  | `[nonce:2][status:1][hash:16][mtime_ns:8]`                                                        |

One response per nonce in every outcome; per-connection-per-family nonce
namespace; a duplicate answers `INVALID` without executing (git's
rule). Handled **inline on the engine thread**, exactly as `FS_FETCH`
(`handle_fetch`, no spawn): a new `Command::Write` / `Command::Op`
replying with one `FS_DONE`. That inline serialization on the shared
root's engine is what makes blit-vs-blit CAS race-free (§ Conflict).

**A dedicated `FS_WRITE` opcode, a folded `FS_OP`.** Content-bearing
writes get their own fat opcode (like `GIT_BLOB` / `FS_FILE`, whose
layout is genuinely distinct); the homogeneous path operations fold
under an `op` selector (like `LSP_QUERY`'s `kind`). `FS_OP` carries a
`base` and a `mode` that some ops ignore — `base` is used by `REMOVE`
(conditional delete) and unused by `MKDIR`/`RENAME`; `mode` is used by
`MKDIR` and unused by `REMOVE`/`RENAME` — exactly as `LSP_QUERY`'s
`line`/`col` are ignored by the symbol kinds. The fold conserves the
scarce `0x44`–`0x4F` block for the growth axis a browser IDE actually
pushes on (metadata ops), and a new op is a codec addition, not a new
opcode.

`FS_WRITE.flags`: bit 0 `NO_CAS` (ignore `base`, unconditional
upsert), bit 1 `MKPARENTS` (create missing parent dirs), bit 2
`DURABLE` (fsync the file and parent — `F_FULLFSYNC` on macOS — before
returning; default trades durability for latency), bit 3
`FOLLOW_SYMLINK` (write through a final-component symlink whose resolved
target stays under the root; default refuses one — § Path validation).
`content_kind`: `0`/`1` full bytes (v1); `2` reserved for a
delta-against-`base` write (v2, mirroring fs-watch's content deltas) —
a client may always send full, so the encoder is a server-side
addition later.
`FS_OP.op`: `1` `MKDIR`(a), `2` `REMOVE`(a, subtree), `3`
`RENAME`(a → b), `4` `SYMLINK`(a = target string → link at b), `5`
`HARDLINK`(a = source file → link at b) (§ Links). `flags` bit 1
`MKPARENTS`, bit 0 `NO_CAS` (for `REMOVE` and the link ops).
`mode` `0` means "preserve existing, else umask default"
(`NodeMeta.mode`).

**Paths** are the fs family's **escaped form** (the `FS_FETCH` rule): a
client-minted name is valid UTF-8, so escaped ≡ plain except a literal
`%` → `%25` — the one rule a writer carries, and the form the echo
`UPSERT` will carry back, so self-echo suppression matches byte for
byte (§ Echo). The resolver validates the _decoded_ component
(traversal fix, § Path validation).

`FS_DONE.status`: the unified git/lsp status table, **not** `FS_SYNCED`'s
grandfathered `0`–`4` — writes need `WRONG_TYPE` / `TOO_LARGE` /
`BUDGET` / `INVALID`, and the unified table is already the family
standard for git and lsp. One code is added, in lsp's `10 WARMING`
extension style:

```text
11 CONFLICT   a precondition failed (CAS mismatch, create-exclusive on
              an existing path, conditional remove on a changed file)
```

On success `hash`/`mtime_ns` are the post-op stat: the new content hash
and mtime for `FS_WRITE`; the directory's for `MKDIR`; the target-bytes
hash for `SYMLINK` and the source content hash for `HARDLINK` (both feed
self-echo suppression, § Echo); zero for `REMOVE`/`RENAME`. **On
`CONFLICT`, `hash` carries the current on-disk hash**, so the client
rebases (3-way diff, retry) without a round trip — the analog of git's
"size fields still carry truth" on `TOO_LARGE`.

### Links

Symlinks follow git's grain: **a symlink's content is its target
bytes**, its hash BLAKE3-128 over them — landed on the read side too
([fs-watch.md](fs-watch.md)): symlink `UPSERT`s carry the target as
inline content, `size` is the target length, and `FS_FETCH` of a
symlink returns the target bytes, never what it points at. That one
identity makes the write side free: `SYMLINK`'s `base` CASes on the
entry at `b` exactly as a write's does on its path — zero =
create-exclusive (`symlink()` fails `EEXIST` natively, race-free),
non-zero = retarget iff the current target hashes to `base`, `NO_CAS` =
unconditional. Retargeting is atomic: the new link lands at a sibling
temp path and renames over `b`, so a reader sees the old target or the
new, never neither. The target string `a` is stored **verbatim** —
relative, absolute, or dangling targets are all legitimate symlinks
(exactly what `ln -s` accepts), and none of it changes the confinement
posture: the sync reports links and never follows them, `FS_FETCH`
returns target bytes, and a content write through a symlink still
re-confirms the resolved path under the root (§ Path validation). A
directory at `b` refuses with `WRONG_TYPE`; a symlink at `b` — even one
pointing at a directory — is an entry like any other and may be
replaced.

`HARDLINK` requires `a` to be a regular file under the root
(`WRONG_TYPE` otherwise — aliasing directories is impossible and
aliasing symlinks is platform-divergent), creates the link at `b` under
the same `base` discipline, and returns the source's content hash. The
echo `UPSERT` at `b` is an ordinary file entry: fs-watch disclaims
hardlink identity, so the mirror sees an independent file with
identical content. Cross-filesystem sources fail with the mapped I/O
error (`link(2)` cannot span devices); Windows honors hard links for
files and may refuse symlink creation without the developer privilege —
per-platform divergence stays server-side, the wire statuses identical
(§ Atomicity).

## Conflict model

Compare-and-swap on BLAKE3-128, the hash fs-sync already maintains and
the client already holds for every synced file (`FsNode.hash`) — so a
save carries its precondition for free, and the client never computes a
hash (there is no BLAKE3 in `js/core`; the server does more). Three
modes on the one `base` field:

- **`base` non-zero** → write iff the current on-disk content hash
  equals `base`, else `CONFLICT`. The lost-update guard is that the
  comparison is against a **freshly re-stat'd live hash** taken under the
  engine lock immediately before the rename — never a settle-lagging
  index snapshot and never a blob-cache entry, either of which could be
  stale and clobber a concurrent edit.
- **`base` all-zero** → **create-exclusive** (`O_EXCL` on the
  destination): write iff the path does not exist, else `CONFLICT`. The
  zero-hash sentinel mirrors git's zero-oid = "absent"; no real content,
  not even an empty file, hashes to zero. This is the natural "New File"
  precondition, and `O_EXCL` on the destination makes it race-free even
  against an external creator.
- **`NO_CAS`** → unconditional overwrite/create — the escape hatch for
  "Save As, replace" and VS Code's `overwriteFileOnDisk`.

mtime+size etags (VS Code's on-disk-change scheme) miss a same-size edit
inside timestamp granularity (a documented VS Code bug class);
content-hash CAS is self-verifying and costs one 16-byte field.

The **blit-vs-external-writer** window is irreducible: no OS offers an
atomic compare-hash-and-rename, so an external process writing between
blit's hash check and blit's rename can be clobbered. The blit-vs-blit
window is closed by serializing the compare-hash-and-write section on a
process-global lock keyed by the **canonical target path** — not the
per-sync engine or the shared root, since two writers can reach the same
file through different roots (a recursive and a non-recursive sync of the
same directory, or a root and a nested root), each with its own engine
thread and `SharedRootHandle`. Distinct files still lock independently.
The cross-writer window is disclosed, not solved (no design can).

## Atomicity and durability

A **server implementation detail, best-effort per platform, not a wire
guarantee.** This RFC upgrades fs-watch's durability disclaimer to
"atomic-replace best-effort": the wire promises only that a reader sees
the old bytes or the new, never a torn write. A `write_atomic(path,
bytes, mode)` helper lands beside the read primitives in `crates/fssync`
(pure platform code, composing with `resolve_wire_path` as `handle_fetch`
does): temp file in the **same directory** (same filesystem ⇒ atomic
`rename`), write, then rename over the target.

- **Unix:** `O_EXCL` temp, `write`, `rename`; with `DURABLE`,
  `sync_all` then fsync the parent directory, and `F_FULLFSYNC` on macOS
  (plain `fsync` does not flush the drive cache).
- **Windows:** `ReplaceFileW`, or `MoveFileExW(REPLACE_EXISTING |
WRITE_THROUGH)`, same-dir temp, **retrying on sharing violations**
  (indexers and AV hold handles without `FILE_SHARE_DELETE`), falling
  back to in-place truncate only as a last resort.

Conceded cost: rename swaps the inode and breaks hardlinks on every
platform. Acceptable — fs-watch disclaims hardlink identity, and the
watcher watches by path. This is the one place fs-watch's "identical
semantics on three platforms" is genuinely hard; it is kept at the
**wire** level (identical statuses) while the server absorbs the
per-platform divergence.

## Echo and attribution

The write opcode **echoes nothing itself.** It lands the bytes on disk;
the existing per-client differ emits `UPSERT`/`MOVE`/`DELETE` to every
mirror, the writer's included. Two server-side moves make the echo
prompt and cheap:

- **Synchronous dirty hint.** After the rename, the engine injects
  `Hint::Dirty` into the shared root's reconciler, so the change
  publishes in one settle window (~20 ms) instead of awaiting the
  native watcher — which also fires and reconciles to a no-op (the hint
  is idempotent). This requires retaining the `Arc<SharedRootHandle>`
  (or a cloned `HintSender`) in `FsSyncEntry`, which today is dropped
  after `start_sync`.
- **Metadata-only echo.** The verified bytes are fed to the blob store
  and a `HashLearned` is sent, so the writer's echo `UPSERT` degrades to
  metadata + hash, not a redundant copy of the bytes it just uploaded
  (fs-watch already sends metadata alone for identical content).

**Attribution is by hash, not a token.** `FS_DONE` returns the new
hash; the client records it as `lastWrittenHash`. When the echo `UPSERT`
arrives:

- `record.hash == lastWrittenHash` → my own echo: do not re-apply to the
  editor model.
- `!=` and the model is clean → a genuine external change: apply a
  computed diff via Monaco's `applyEdits`, **never `setValue`** (which
  destroys the undo stack and cursor).
- `!=` and the model is dirty → surface a conflict (Reload / Overwrite /
  Compare).

Content-addressing already provides identity, so an attribution token
would be redundant state. **No stale flash** rests on two rules the
client must follow: never `setValue` its own echo (the hash-match skip),
and chain consecutive rapid saves' CAS off the **returned** hash, not
the mirror — `live` lags the echo by a settle window and is
wholesale-swapped on `SYNC`, so node references must never be retained
across it.

Honest weakness: if an external writer commits **byte-identical**
content between the write and its echo, blit mis-attributes it as the
self-echo and suppresses the "changed on disk" notice. But the bytes are
identical, so the model is already correct; only a benign notification
is lost — consistent with fs-watch, where a state-identical change is
invisible by design.

## Operation set (scope, both directions)

| Op                    | Verdict         | Why                                                                                                                                                                                   |
| --------------------- | --------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| write (CAS)           | **in**          | the core primitive                                                                                                                                                                    |
| mkdir (+ mode)        | **in**          | empty folders are real fs-sync entries; explorer "New Folder"; mode for `0700`                                                                                                        |
| remove (subtree, CAS) | **in**          | explorer delete; mirrors the `DELETE` record; optional `base` = "delete iff unchanged"                                                                                                |
| rename (subtree)      | **in**          | rename _and_ drag-move are one op; surfaces as a `MOVE` record                                                                                                                        |
| symlink (create/retarget, CAS) | **in** | dotfile/workspace layouts are symlink-shaped; content = target bytes makes retarget an ordinary CAS (§ Links)                                                                  |
| hardlink              | **in**          | `link(2)` is one op the client cannot compose from writes; source must be a regular file (§ Links)                                                                                    |
| create-parents        | **in** (flag)   | drag-move into a fresh path                                                                                                                                                           |
| delete-to-trash       | **out → shell** | XDG/Recycle/`~/.Trash` semantics diverge; a synced trash dir churns. Compose via rename                                                                                               |
| copy / duplicate      | **out (v1)**    | the weakest cut; subtree copy can't compose client-side without shipping bytes both ways. Server-side `FS_OP` `COPY` is cheap (it holds the blobs) — trigger: duplicate latency hurts |
| touch                 | **out**         | create-empty is a zero-byte write with a zero base; mtime-touch has no IDE use                                                                                                        |
| save-all / txn        | **out**         | N independent nonces, per-file `CONFLICT`, no rollback                                                                                                                                |

**Multi-file operations get no wire transaction** — the deliberate
stance. Save-all, and applying an [lsp.md](lsp.md) rename plan's `EDIT`
records, are orchestrated client-side as N writes, each CAS'd on its own
base; a mid-batch `CONFLICT` stops and reports which files were applied.
No filesystem offers multi-file atomicity on any of the three platforms,
so a fake commit-or-rollback we cannot honor is worse than the honest
partial-failure UX every editor already shows. A half-applied refactor
is recoverable (re-run or undo per file); a fake transaction is a lie.

## Path validation and security posture

Writes tighten the confinement that reads already need. The **decode-
order traversal bug is fixed as a prerequisite** (landed separately):
`resolve_wire_path` now validates the _decoded_ component — rejecting
empty, `.`, `..`, absolute/prefix pieces, and embedded separators — so
`%2E%2E` and `%2F` can no longer climb out of the root on `FS_FETCH` or
a write.

A write additionally **resolves the parent, canonicalizes it, and
re-confirms `starts_with(root)`** before the rename (defeating a synced
symlink whose target is outside the root — `resolve_wire_path` does no
symlink resolution). A final component that is itself a symlink is
**refused by default**; the `FOLLOW_SYMLINK` flag opts into writing
through it, and only after the resolved target is re-confirmed under the
root (so an in-tree symlink editor workflow works, an escaping one does
not). Inbound content is bounded by `MAX_DECOMPRESSED` (64 MiB) before
decompression.

**Posture shift, stated honestly.** fs-watch's "the server already hands
clients a shell, so this adds DoS surface, not privilege" carries for
_privilege_ — a PTY can already write anywhere the process can — but
**not for blast radius.** A traversal or symlink bug turns a
root-scoped API into an arbitrary-path write _structurally_, where the
shell gates the same power behind the user's own typed commands. Same
ceiling, higher and un-audited blast radius, plus a new **confinement
obligation** read-only sync never carried. And a relay or proxy may
grant read-only sync to a party not trusted with a shell —
`BLIT_FS_WRITE=0` keeps that deployment: `FS_WRITE`/`FS_OP` are refused
at dispatch with `FS_DONE` `PERMISSION`, before any parsing or engine
work. (An earlier draft spent feature bit 9 on advertising this; the
gate needs only refusal, not advertisement, so the family shares
`FEATURE_FS` and the bit stays free.)

## Budgets

| Knob                    | Default | Env                      |
| ----------------------- | ------- | ------------------------ |
| Per-write content       | 16 MiB  | `BLIT_FS_WRITE_MAX`      |
| Decompressed inbound    | 64 MiB  | (protocol-wide)          |
| Writes in flight / conn | 16      | `BLIT_FS_WRITE_INFLIGHT` |

C2S has no `S2C_FRAGMENT` and no credit window — nonce request/response
_is_ the backpressure, bounded by the in-flight cap. A file over
`BLIT_FS_WRITE_MAX` is refused (`TOO_LARGE`); chunked and append writes
are a v2 non-goal. The S2C window/retention budgets are untouched: the
echo rides the already-bounded update path.

## Client surface

Handle-level on `FsSyncHandle`, matching `fetch` and `openRepo`:

```ts
writeFile(path, data, { ifHash?, mode?, createParents?, durable? })
  : Promise<{ hash, mtimeNs }>
mkdir(path, { mode?, createParents? }): Promise<{ hash, mtimeNs }>
remove(path, { ifHash? }): Promise<void>
rename(from, to, { createParents? }): Promise<void>
symlink(target, path, { ifHash?, force?, createParents? })
  : Promise<{ hash, mtimeNs }>
hardlink(source, path, { ifHash?, force?, createParents? })
  : Promise<{ hash, mtimeNs }>
```

The link methods default to create-exclusive (the common "New Link"
case); `ifHash` retargets under CAS, `force` replaces unconditionally —
inverted from `writeFile`, whose default matches a shell `>` redirect.

**Monaco save flow, end to end:**

1. `node = live.get(path)` — the mirror already holds `node.hash`.
2. `writeFile(path, bytes, { ifHash: node.hash })`.
3. On `{ hash }` → set `lastWrittenHash = hash`; the matching echo
   `UPSERT` is recognized and _not_ re-applied to the model.
4. On `CONFLICT` → `FS_DONE.hash` is the current disk hash; `fetch()`
   the current bytes and present Overwrite (retry `NO_CAS`) / Compare
   (3-way) / Revert (`applyEdits` the disk version), no extra round
   trip.

The client never hashes. CLI: `blit fs write PATH [--if-hash H |
--create | --force] [--durable]` from stdin, plus `blit fs mkdir | rm |
mv` and `blit fs ln [-s] TARGET LINK [--if-hash H | --force]` (like
`ln(1)`: hard link by default, symlink with `-s`).

## Forward compatibility: the buffer/collab RFC

This RFC is disk-only, and it is designed so the buffer/collab RFC
[lsp.md](lsp.md) anticipates composes _above_ it rather than fighting
its wire — the three contracts that keep that seam open:

1. **Buffer identity survives rename.** `RENAME` carries no buffer
   identity, so a future `(sync_id, buffer_id)` key is left
   unforeclosed — a buffer can outlive the path it is saved under.
2. **Disk-content CAS is a separate namespace from buffer versions.**
   `base`/`hash` here are content hashes of bytes on disk; a future
   buffer stream's engine-minted monotonic versions are a distinct
   space and must never be overloaded onto this field. Disk truth and
   buffer truth stay orthogonal.
3. **A buffer "save" composes as a `FS_WRITE` with `ifHash`**, not a new
   replacement path. Buffers layer over single-writer disk I/O exactly
   as Zed and Live Share layer a CRDT above the filesystem — LSP
   `didOpen`-from-buffer, real-time co-edit, and OT/CRDT all live in
   that later feature bit, never in this one.

## Out of scope (with triggers)

- **Client buffers / `didOpen`-from-buffer** — disk-truth only. Trigger:
  a browser editor wanting unsaved-buffer diagnostics ([lsp.md](lsp.md)
  names the buffer as an alternate byte source into its single-writer
  projection).
- **OT/CRDT collaborative editing** — last-writer-wins via CAS here; a
  separate buffer-sync bit layered above. Trigger: a real-time co-edit
  product.
- **LSP completion / `workspace/applyEdit`** — stays [lsp.md](lsp.md)'s;
  this RFC supplies the write primitive its rename-apply composes on.
- **Chunked/append write, subtree copy** — triggers in § Operation set.

## Rollout

1. `crates/remote/src/fs.rs`: opcodes, codecs; TypeScript mirror in
   `js/core/src/fs.ts`; byte fixtures both sides.
2. **Path-validation prerequisite** — the `resolve_wire_path` decode-
   order fix (done) plus the write-time parent-canonicalize /
   `starts_with(root)` / symlink guard, with the compiled traversal
   test, before any write opcode dispatches.
3. `crates/fssync`: `write_atomic` + synchronous hint injection +
   `HashLearned`; CAS against the live re-stat'd hash; retain the
   `SharedRootHandle` in `FsSyncEntry`.
4. `crates/server`: `Command::Write` / `Command::Op` inline dispatch,
   e2e; `blit fs write|mkdir|rm|mv|ln`.
5. `js/core`: handle methods and `lastWrittenHash` echo suppression.
6. Monaco pane (separate `js/ui` work — a new BSP assignment kind and
   component). Ship write + CAS first, mkdir/remove/rename second,
   defer copy.

## Top risks

1. **Path confinement.** The decode-order/symlink gap would let a write
   API weaponize a pre-existing _read_ traversal into arbitrary-path
   write. The decode-order half is fixed; the symlink/parent-canonical
   half must land before ship. Highest.
2. **Echo ordering under rapid saves.** A wrong `lastWrittenHash` chain
   yields `CONFLICT` storms or cursor/undo flashes — the IDE's whole
   feel rides on it. The mitigations (chain CAS off the reply, never
   `setValue`) are load-bearing, not decorative.
3. **Windows atomic-replace.** No documented atomic rename-replace;
   sharing violations from AV/indexers; inode/hardlink break. The one
   place three-platform parity is genuinely hard — degrade to documented
   best-effort, keep the wire statuses identical.
