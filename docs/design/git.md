# RFC: Git Introspection

- **Status:** Accepted, implemented
- **Date:** 2026-07-21
- **Companion to:** [fs-watch.md](fs-watch.md)

## Summary

Clients want to see repositories the way tools see them: which refs exist and
where they point, what happened between two commits, what is staged, what
differs between any two of {commit, tree, index, worktree}, and the bytes of
any object — without shipping a Git implementation to every client or a
`.git` directory over the wire.

The design splits along Git's own grain:

- **Mutable and small** — HEAD, refs, in-progress operation, index/worktree
  status — is _pushed_ as whole-state snapshots, the same philosophy as
  [fs-watch.md](fs-watch.md): the server watches, settles, and streams; the
  client holds a map current by construction.
- **Immutable and large** — commits, trees, blobs, patches — is _pulled_ by
  content address. An oid names its bytes forever, so every response is
  cacheable client-side without invalidation, and nothing needs to stream.

A ref snapshot is a few KiB; the object store is unbounded. Pushing the
first and pulling the second is the only split that bounds both directions.

Two conveniences ride on that split. The server _resolves_ revision
expressions — `main`, `v1.0^`, `HEAD~3`, ranges like `dev..HEAD` — to the
commit oids a walk needs, so clients express intent in Git syntax without
parsing it. And a commit log can be _watched_: the server re-resolves and
re-walks a spec whenever the refs it names move, pushing the fresh page
under the same settle-and-coalesce pacing as state. Watching `main..HEAD`
updates live as either endpoint advances.

## Goals

- Traverse refs, walk commit ranges (`hide..tip`), enumerate trees and the
  index, and fetch blobs — with pagination that keeps the server stateless
  between requests.
- Resolve revision expressions (refs, oids, `HEAD~3`, `A..B`, `A...B`) to
  commit oids server-side, so clients never carry a rev-parser.
- Live-watch a commit log: subscribe to a spec and receive a fresh page
  whenever the refs it names move — no polling, no client-side rev-walk.
- Diff any two of commit / tree / index / worktree: file-level records
  first, render-ready hunk rows on demand — clients display diffs
  without carrying a diff parser.
- Live state: ref moves, HEAD changes, merge/rebase progress, and (opt-in)
  worktree status arrive without polling.
- Thin clients: apply records, cache by oid. No revwalk, no pack access, no
  rename detection client-side.
- Fit blit conventions: 1-byte opcodes, little-endian, LZ4,
  `S2C_FRAGMENT`, feature-bit gated, nonce request/response, budgets.

## Non-goals

- Mutation: staging, committing, checkout, branching, push. Read-only by
  design; a mutation family would be a separate RFC.
- Remote operations (fetch/push/ls-remote) and credentials.
- Blame and reflog traversal (natural later additions to the same opcode
  block; the record framing leaves room).
- Submodule recursion: submodules appear as entries with their commit oid;
  clients open them as separate repositories.
- Hook execution, config access, filter/smudge application: blobs are raw
  object bytes; worktree diffs use worktree bytes as they are on disk.

## Protocol

New `S2C_HELLO` feature bit:

```text
FEATURE_GIT = 1 << 7
```

Opcodes occupy the `0x50` block in both directions; request/response pairs
share the opcode value. Gateway, proxy, and mux forward them unmodified.
All integers little-endian; the 16 MiB frame limit and
[protocol.md](protocol.md) framing apply.

| Direction | Opcode | Name              | Layout                                                                                                            |
| --------- | ------ | ----------------- | ----------------------------------------------------------------------------------------------------------------- |
| C2S       | `0x50` | `GIT_OPEN`        | `[nonce:2][flags:1][refs_latency_ms:2][status_latency_ms:2][path_len:2][path:N]`                                  |
| C2S       | `0x51` | `GIT_CLOSE`       | `[repo_id:2]`                                                                                                     |
| C2S       | `0x52` | `GIT_ACK`         | `[repo_id:2][state_id:4]`                                                                                         |
| C2S       | `0x53` | `GIT_LOG`         | `[nonce:2][repo_id:2][flags:1][limit:2][path_len:2][path:N][n_tips:2][tips:32·N][n_hides:2][hides:32·N]`          |
| C2S       | `0x54` | `GIT_TREE`        | `[nonce:2][repo_id:2][oid:32][path_len:2][path:N]`                                                                |
| C2S       | `0x55` | `GIT_BLOB`        | `[nonce:2][repo_id:2][oid:32][path_len:2][path:N][max_len:4]`                                                     |
| C2S       | `0x56` | `GIT_DIFF`        | `[nonce:2][repo_id:2][flags:1][old_kind:1][old:32][new_kind:1][new:32][path_len:2][path:N]`                       |
| C2S       | `0x57` | `GIT_PATCH`       | `[nonce:2][repo_id:2][flags:1][context:1][old_kind:1][old:32][new_kind:1][new:32][path_len:2][path:N][max_len:4]` |
| C2S       | `0x58` | `GIT_INDEX`       | `[nonce:2][repo_id:2][path_len:2][path:N]`                                                                        |
| C2S       | `0x59` | `GIT_CANCEL`      | `[nonce:2]`                                                                                                       |
| C2S       | `0x5A` | `GIT_BASE`        | `[nonce:2][repo_id:2][n_oids:1][oids:32·N]`                                                                       |
| C2S       | `0x5B` | `GIT_RESOLVE`     | `[nonce:2][repo_id:2][spec_len:2][spec:N]`                                                                        |
| C2S       | `0x5C` | `GIT_LOG_WATCH`   | `[log_id:2][repo_id:2][flags:1][limit:2][spec_len:2][spec:N]`                                                     |
| C2S       | `0x5D` | `GIT_LOG_UNWATCH` | `[log_id:2][repo_id:2]`                                                                                           |
| C2S       | `0x5E` | `GIT_LOG_ACK`     | `[log_id:2][repo_id:2][update_id:4]`                                                                              |
| S2C       | `0x50` | `GIT_REPO`        | `[nonce:2][repo_id:2][status:1][oid_format:1][flags:1][workdir_len:2][workdir:N][gitdir_len:2][gitdir:N]`         |
| S2C       | `0x51` | `GIT_STATE`       | `[repo_id:2][state_id:4][flags:1][records:LZ4]`                                                                   |
| S2C       | `0x52` | `GIT_CLOSED`      | `[repo_id:2][reason:1]`                                                                                           |
| S2C       | `0x53` | `GIT_COMMITS`     | `[nonce:2][status:1][flags:1][n_frontier:2][frontier:32·N][records:LZ4]`                                          |
| S2C       | `0x54` | `GIT_TREE`        | `[nonce:2][status:1][flags:1][records:LZ4]`                                                                       |
| S2C       | `0x55` | `GIT_BLOB`        | `[nonce:2][status:1][size:8][data:LZ4]`                                                                           |
| S2C       | `0x56` | `GIT_DIFF`        | `[nonce:2][status:1][flags:1][records:LZ4]`                                                                       |
| S2C       | `0x57` | `GIT_PATCH`       | `[nonce:2][status:1][flags:1][data:LZ4]`                                                                          |
| S2C       | `0x58` | `GIT_INDEX`       | `[nonce:2][status:1][flags:1][records:LZ4]`                                                                       |
| S2C       | `0x5A` | `GIT_BASE`        | `[nonce:2][status:1][n_bases:1][bases:32·N]`                                                                      |
| S2C       | `0x5B` | `GIT_RESOLVE`     | `[nonce:2][status:1][n_tips:2][tips:32·N][n_hides:2][hides:32·N]`                                                 |
| S2C       | `0x5C` | `GIT_LOG_PAGE`    | `[log_id:2][update_id:4][status:1][flags:1][n_frontier:2][frontier:32·N][records:LZ4]`                            |

### Statuses

One table for every `status` byte in the family:

```text
0 OK
1 UNKNOWN_ID   repo_id unknown or already closed
2 NOT_FOUND    path or object does not exist
3 WRONG_TYPE   object is not what the request requires
               (not a repository / commit / tree / blob)
4 PERMISSION   permission denied
5 TOO_LARGE    over max_len or a size cap; size fields still carry truth
6 BUDGET       a budget was exhausted with no way to paginate or truncate
7 INVALID      malformed request (unknown flags, bad endpoint combination)
8 CANCELLED    ended by GIT_CANCEL
9 OTHER        diagnostic in the message's detail field where it has one
```

Codes 0–4 coincide with `FS_SYNCED`'s where the semantics overlap, so a
client's error mapping is one table, not one per message.

### Nonces and cancellation

Every nonce-bearing request yields **exactly one** response echoing the
nonce, in every outcome — success, error, cancellation, or the repo
closing mid-flight (then `UNKNOWN_ID`). Nonce namespaces are per
connection per family; correlation is `(family, nonce)`. Two in-flight
requests must not share a nonce — the server answers a duplicate
immediately with `INVALID` without executing it. A wrapping `u16` counter
suffices.

`GIT_CANCEL` is advisory: the server checks between walk steps and record
emissions; a cancelled request answers `CANCELLED` (or completes normally
if it already finished). Cancelling an unknown nonce is a no-op.

Log subscriptions correlate differently: `GIT_LOG_WATCH`/`GIT_LOG_PAGE`/
`GIT_LOG_UNWATCH`/`GIT_LOG_ACK` carry a client-assigned `log_id` (its own
per-connection namespace) rather than a nonce, and a single subscription
yields **many** `GIT_LOG_PAGE`s over its lifetime, each tagged with a
monotonic `update_id` the client acks. `GIT_RESOLVE` is an ordinary
nonce-bearing request.

### Oids, paths, and text

**Oids** are always 32 bytes on the wire, zero-padded past the repository's
hash width. `GIT_REPO.oid_format` announces the width: `0` SHA-1 (20 bytes
used), `1` SHA-256 (32). The all-zero oid means "absent" (unborn branch,
unhashed worktree side, deleted side of a diff).

**Paths and ref names** follow the fs family's split. Strings the server
_emits_ (repo-relative paths, ref names, tree entry names) use the
[fs-watch.md](fs-watch.md) escaping scheme — valid UTF-8 on the wire,
`%XX` for non-UTF-8 bytes, `%uXXXX` for unpaired surrogates, `%25` for
literal `%`, `/` separators — and request fields that _echo_ emitted
strings (`GIT_TREE`/`GIT_BLOB`/`GIT_DIFF`/`GIT_PATCH`/`GIT_INDEX` paths,
the `GIT_LOG` filter) use the same form, exactly like `FS_FETCH`.
`GIT_OPEN.path` alone is plain UTF-8, like `FS_SYNC.path`: it names a
filesystem location the client chose, not a name the server minted.

**Names, emails, and commit messages** are re-encoded to UTF-8 server-side
(honoring the commit's `encoding` header, lossy otherwise); the commit
record carries a `LOSSY` flag when bytes were replaced. Clients never see a
charset.

**Records** inside every `records:LZ4` payload use the
[fs-watch.md](fs-watch.md) framing: `[record_len:4][kind:1][…]`, unknown
kinds skipped via `record_len`, malformed records end the payload. Record
kinds are namespaced per message type. Compression is
`lz4_flex::compress_prepend_size`, subject to the protocol-wide
`MAX_DECOMPRESSED` receiver guard ([protocol.md](protocol.md)); the
server additionally closes any records payload at the uncompressed byte
bound (`BLIT_GIT_BYTES_MAX`) with `MORE`/`TRUNCATED` semantics, so the
guard can never be what a well-behaved response trips.

### `GIT_OPEN` / `GIT_REPO`

`flags`: bit 0 `WATCH` (stream `GIT_STATE`), bit 1 `STATUS` (include
index/worktree status records in state; implies `WATCH`), bit 2 `UNTRACKED`
(status includes untracked files), bit 3 `IGNORED` (status includes ignored
files; implies `UNTRACKED`), bit 4 `TRACKING` (include per-branch upstream
records in state; implies `WATCH`).

`refs_latency_ms` and `status_latency_ms` are per-open settle windows
(`0` → server defaults 50 / 500 ms, clamped to 1–1000 / 1–10000); the env
vars in the limits table set the defaults, exactly like
`BLIT_FS_LATENCY_MS`. When several opens share one engine, it runs at the
minimum requested window and coalesces for slower clients.

`path` is plain UTF-8, absolute or server-cwd-relative; the server runs
standard upward discovery from it (stopping at filesystem boundaries), so
opening any path inside a worktree works. Linked worktrees resolve to their
own worktree with the shared gitdir.

`GIT_REPO.status`: `NOT_FOUND`, `WRONG_TYPE` (exists but no repository
found from it), `PERMISSION`, `BUDGET` (repo limit reached), `INVALID`,
`OTHER`. On failure `repo_id` = `0xFFFF` and `workdir` carries a
diagnostic. On success `workdir` is the canonical worktree root (empty for
bare) and `gitdir` the canonical git directory, both escaped.
`GIT_REPO.flags`: bit 0 `BARE`, bit 1 `SHALLOW`, bit 2 `SPARSE`
(sparse-checkout active), bit 3 `LINKED` (linked worktree).

`repo_id` scopes every other message. `GIT_CLOSE` releases it;
`GIT_CLOSED` (`reason`: `0` client request, `1` repository gone, `2`
permission lost, `3` backend failure, `4` resource limit) ends it from the
server side. Reopening after `GIT_CLOSED` is always just `GIT_OPEN`.

### `GIT_STATE` / `GIT_ACK`

Each `GIT_STATE` is a **complete snapshot** of the mutable state — not a
diff. Ref sets are small enough that diffing buys little, and whole
snapshots make the client obligation "replace the map", with no staging
protocol at all. Sent once immediately after `GIT_REPO` when `WATCH`, then
after every settled change.

Pacing is coalescing: at most one snapshot is in flight; the client acks
`state_id`, and the server then sends the _latest_ state if it changed
while unacked. A slow client skips intermediate states and never falls
behind. `flags`: bit 0 `REFS_TRUNCATED`, bit 1 `STATUS_TRUNCATED` (entry
budget hit; counts still accurate up to the cap).

Records:

```text
HEAD   0x01: [kind:1][flags:1][oid:32][name_len:2][name:N]
             flags: bit 0 DETACHED, bit 1 UNBORN; name = symbolic target
STATE_REF 0x02: [kind:1][flags:1][oid:32][peeled:32][name_len:2][name:N]
             flags: bit 0 PEELED_VALID (annotated tag), bit 1 SYMBOLIC
OP     0x03: [kind:1][op:1][oid:32][detail_len:2][detail:N]
             op: 1 merge, 2 rebase, 3 cherry-pick, 4 revert, 5 bisect;
             oid = the operation head; absent record = no operation
STATUS 0x04: [kind:1][staged:1][unstaged:1][flags:1]
             [old_len:2][old_path:N][path_len:2][path:N]
             staged/unstaged: ASCII ' ' A M D R T U, '?' untracked,
             '!' ignored (porcelain letters); flags: bit 0 CONFLICTED;
             old_path non-empty only for renames
UPSTREAM 0x05: [kind:1][flags:1][ahead:4][behind:4]
             [name_len:2][name:N][upstream_len:2][upstream:N]
             one per local branch with a configured upstream; name joins
             STATE_REF by ref name; flags: bit 0 GONE (upstream configured
             but its ref is missing; counts zero), bit 1 COUNTS_VALID
             (unset when the walk budget was hit; names still valid)
STASH  0x06: [kind:1][index:2][oid:32][time:8 i64 s][tz:2 i16 min]
             [msg_len:2][msg:N]
             index is the N of stash@{N}, oid the stash commit, msg the
             reflog message under the family's text rules
```

`STATUS` records appear only with the `STATUS` open flag; `UPSTREAM` only
with `TRACKING`. `main ↑2 ↓3` — the most-rendered piece of git chrome —
is thus a pushed-state lookup, not a walk: the server derives the
branch→upstream mapping from config (never exposed raw) and memoizes
counts by the immutable `(tip, upstream)` oid pair, so steady state costs
nothing and a ref move recomputes only the pairs it touched. Stash
contents need no opcodes: a stash entry is a commit, so its diff is
`GIT_DIFF` COMMIT(`stash^1`)×COMMIT(`stash`) and untracked bytes hang off
its third parent via `GIT_TREE`/`GIT_BLOB`.

Ref snapshots are re-read after a settle window on gitdir hints; they are
eventually consistent, never torn beyond what loose-ref updates themselves
allow.

### `GIT_LOG` / `GIT_COMMITS`

Commits reachable from `tips` and not from `hides` — the `hides..tips`
range. Empty `tips` means HEAD. `limit` `0` means the server default (256);
requests are clamped to the maximum (4096). `path` non-empty restricts to
commits touching that subtree. `flags`: bit 0 `FIRST_PARENT`, bit 1 `TOPO`
(topological order; default committer-date), bit 2 `FULL_MESSAGE` (default
first line only), bit 3 `FOLLOW` (`path` must name a single file —
`WRONG_TYPE` on a directory; the walk tracks it across renames), bit 4
`PATH_OIDS` (after each commit, emit the object at the rename-adjusted
`path` in that commit).

`GIT_COMMITS.status`: `UNKNOWN_ID`, `NOT_FOUND` / `WRONG_TYPE` (bad or
non-commit oid), `OTHER`. `flags`: bit 0 `MORE`. **Continuation is
stateless:** when `MORE` is set, `frontier` holds the walk's pending
boundary; the client re-issues `GIT_LOG` with `tips = frontier` and the
same `hides` to continue exactly where the walk stopped. Commits are
immutable, so a continuation is correct no matter how much time passed.
Budget exhaustion is pagination, not failure: hitting the walk or byte
budget returns the partial page with `MORE` set, never an error.

```text
COMMIT  0x01: [kind:1][flags:1][oid:32][tree:32][n_parents:1][parents:32·N]
              [author_time:8 i64 s][author_tz:2 i16 min]
              [committer_time:8][committer_tz:2]
              [author_name_len:2][author_name:N][author_email_len:2][email:N]
              [committer_name_len:2][…][committer_email_len:2][…]
              [msg_len:4][message:N]
              flags: bit 0 LOSSY_ENCODING
PATH_AT 0x02: [kind:1][otype:1][mode:4][oid:32][path_len:2][path:N]
              with PATH_OIDS: the object at the followed path as of the
              preceding COMMIT record; zero oid when that commit deletes
              it; the path field reveals renames as it changes
```

`FOLLOW` + `PATH_OIDS` make a file-history scrubber one request: each
step's content is then oid-addressed (`GIT_BLOB`), cacheable forever.

### `GIT_RESOLVE`

Turns a human revision spec into the `tips`/`hides` that `GIT_LOG` walks
between, so the client never parses git syntax. `spec` is any single git
revision expression: a ref (`main`, `origin/main`, `v1.0`), a (short) oid,
a relative form (`HEAD~3`, `main^2`), or a range — `A..B` (`B` reachable but
not `A`), `A...B` (symmetric difference, bounded by the merge base), and the
`^A` / `A --not B` exclusion forms. The reply lists the resolved commit oids
as `tips` and `hides`; feed them straight into `GIT_LOG` (or `GIT_LOG_WATCH`).
A bare ref or oid yields one tip and no hides.

`GIT_RESOLVE.status`: `NOT_FOUND` (no such ref/revision, or an unparsable
spec), `WRONG_TYPE` (the spec names a non-commit that will not peel to one),
`BUDGET` (a range whose merge base needed more work than the budget allows),
`OTHER`. The resolution is a point-in-time snapshot: refs move, so a spec
resolved once can drift — `GIT_LOG_WATCH` exists to track that.

### `GIT_LOG_WATCH` / `GIT_LOG_PAGE` / `GIT_LOG_UNWATCH` / `GIT_LOG_ACK`

A server-pushed live log. `GIT_LOG_WATCH` names a `spec` (as in `GIT_RESOLVE`)
and the same `GIT_LOG` `flags`/`limit`; `log_id` is a client-assigned
subscription id, unique per connection. The server resolves the spec, sends
one `GIT_LOG_PAGE`, and re-sends whenever the resolved endpoints move — a ref
the spec names is created, deleted, or repointed. Because the endpoints are
watched, `main..HEAD` updates when either `main` or `HEAD` changes; a spec
over immutable oids only ever sends its initial page. Subscriptions share the
repo's gitdir watch (`GIT_OPEN` need not request state) and cost nothing while
refs are quiet.

`GIT_LOG_PAGE` carries the same records as `GIT_COMMITS` plus a monotonic
`update_id`. Pacing mirrors `GIT_STATE`: the server holds the next update
until the client returns a `GIT_LOG_ACK` for the last `update_id`, and
coalesces bursts so a flurry of ref changes collapses to the latest state.
`status` reports resolution failures (`NOT_FOUND`, `WRONG_TYPE`, `BUDGET`)
per update without ending the subscription — a spec naming a not-yet-created
branch reports `NOT_FOUND` now and delivers commits once the branch appears.
`flags` bit 0 `MORE` marks a truncated head page; pull older history
statelessly with `GIT_LOG` from `frontier`. `GIT_LOG_UNWATCH` ends the
subscription; the server frees it and stops sending. Subscriptions do not
survive reconnects — re-issue `GIT_LOG_WATCH` after re-`GIT_OPEN`.

### `GIT_TREE`

`oid` may name a tree, or a commit/tag (peeled server-side); `path`
descends from it. Lists one level — clients walk by issuing further
requests (entries carry the child oids) or skip levels with `path`.
`status`: `UNKNOWN_ID`, `NOT_FOUND`, `WRONG_TYPE`, `OTHER`. Response
`flags`: bit 0 `TRUNCATED` (entry budget).

```text
TREE_ENTRY 0x02: [kind:1][otype:1][mode:4][oid:32][name_len:2][name:N]
                 otype: 1 commit (submodule), 2 tree, 3 blob
                 mode: raw git mode (100644, 100755, 120000, 40000, 160000)
```

### `GIT_BLOB`

The pull for object content. `oid` names a blob directly, or a
commit/tag/tree resolved through `path`. The effective cap is
`min(max_len, BLIT_GIT_BLOB_MAX, MAX_DECOMPRESSED)`, with `max_len` `0`
meaning the server default — the numbers can never disagree. `status`:
`UNKNOWN_ID`, `NOT_FOUND`, `WRONG_TYPE`, `TOO_LARGE` (`size` is always
the true object size, so the client knows what it declined), `OTHER`.
`data` is raw object bytes, LZ4, fragmented as needed. Content-addressed
⇒ cache forever, never refetch.

### `GIT_DIFF`

Endpoints are `(kind, oid)` pairs; `kind`: `0` EMPTY, `1` COMMIT (oid), `2`
TREE (oid), `3` INDEX, `4` WORKTREE, `5` MERGE_BASE (old side only: the
server substitutes `merge-base(oid, new)` — the PR-style triple-dot view
in one endpoint, no round trip to learn the base first). The classic
views compose from them:

| View                      | old                   | new            |
| ------------------------- | --------------------- | -------------- |
| Between commits           | COMMIT                | COMMIT         |
| Staged                    | COMMIT (HEAD)         | INDEX          |
| Unstaged                  | INDEX                 | WORKTREE       |
| Working tree vs HEAD      | COMMIT (HEAD)         | WORKTREE       |
| Branch vs where it forked | MERGE_BASE (upstream) | COMMIT (topic) |

With a MERGE_BASE endpoint the response opens with a `BASE` record naming
the chosen base (what `git merge-base` would pick), so per-file follow-ups
become oid-addressed and cacheable forever by `(base, topic, path)`.

`flags`: bit 0 `RENAMES` (rename/copy detection), bit 1 `UNTRACKED`
(worktree endpoint reports untracked files as additions), bit 2 `IGNORED`,
bit 3 `IGNORE_SPACE_CHANGE` (runs of whitespace compare equal and
trailing whitespace is ignored — git's `-b`), bit 4 `IGNORE_ALL_SPACE`
(whitespace ignored entirely — git's `-w`). With either ignore bit set,
entries whose changes vanish under the normalization are omitted and `st`
reflects the normalized comparison; oids still name the true blobs.
`path` filters to a subtree. `status` as `GIT_TREE` plus `INVALID`
(e.g. INDEX/WORKTREE on a bare repo, MERGE_BASE on the new side or over
non-commits, no common ancestor). Response `flags`: bit 0 `TRUNCATED`.

```text
DIFF_ENTRY 0x03: [kind:1][st:1][similarity:1][dflags:1]
                 [old_mode:4][new_mode:4][old_oid:32][new_oid:32]
                 [old_len:2][old_path:N][new_len:2][new_path:N]
                 st: ASCII A M D R C T U; similarity 0-100 (renames/copies)
                 dflags: bit 0 BINARY, bit 1 SUBMODULE
BASE       0x04: [kind:1][oid:32]
                 first record when a MERGE_BASE endpoint was used: the
                 base the server chose
```

Worktree-side oids are zero unless the file's hash was already known (from
the index). Worktree reads use the torn-read discipline of
[fs-watch.md](fs-watch.md): per-file coherent, tree-wide best-effort — no
filesystem offers more.

### `GIT_PATCH`

Same endpoint spec as `GIT_DIFF` (including MERGE_BASE, with the same
leading `BASE` record — kind `0x04` here too) plus `context` (context
lines, `0` → 3) and a `path`: non-empty for one file's patch, empty for
the whole diff (subject to `max_len`, `TOO_LARGE` when over — distinct
from `INVALID` for bad endpoints). File-level records first (`GIT_DIFF`),
hunks on demand keeps the common case (status pane) cheap and the
expensive case (full patch) explicit.

Request `flags` shares bits 0–4 with `GIT_DIFF` (including the
ignore-whitespace bits) and adds: bit 5 `TEXT` — return a classic unified
diff (UTF-8, escaped paths in headers) as raw `data`, for consumers that
feed `git apply` or archive patches; bit 6 `CHAR_SPANS` — character-
granularity spans instead of the default word granularity; bit 7
`NO_SPANS` — skip intraline refinement entirely, for whole-line
renderers. Response `flags`: bit 0 `STRUCTURED` (`data` is records, the
default), bit 1 `TRUNCATED`.

**The default response is structured**: aligned row records, so clients
render side-by-side or inline with a loop, never a unified-diff parser. A
row pairs an old line with a new line; change _spans_ mark the byte
ranges within each side that differ (intraline refinement of modified
pairs). A context row has no spans:

```text
PATCH_FILE 0x01: [kind:1][flags:1]
                 [old_len:2][old_path:N][new_len:2][new_path:N]
                 begins a file section; flags: bit 0 BINARY (no rows)
PATCH_ROW  0x02: [kind:1][old_line:4][new_line:4]
                 [old_text_len:4][old_text:N][new_text_len:4][new_text:N]
                 [n_old_spans:2][spans:(start:4,len:4)·N]
                 [n_new_spans:2][spans:(start:4,len:4)·N]
                 line numbers are 1-based; 0 = side absent (pure
                 addition/deletion)
PATCH_GAP  0x03: [kind:1][old_line:4][new_line:4]
                 elision between hunks (the "@@" of a unified diff)
```

**Granularity and whitespace.** Spans default to word granularity — text
tokenized into runs of word characters, runs of whitespace, and single
punctuation, which reads best in review UIs; `CHAR_SPANS` requests
minimal character ranges instead. With an ignore-whitespace bit set,
alignment and change detection run on normalized text, but rows always
carry the **true bytes** of both sides and spans map back to true byte
ranges; a modification that vanishes under normalization becomes a
span-less row — it renders as unchanged even though its sides differ in
ignored whitespace. Clients get every view (word, char, `-b`, `-w`) by
flipping request bits, never by reprocessing.

Rows are a _presentation_ computed server-side, not a contract with any
particular diff algorithm: the tokenization may improve, a smarter
engine (e.g. syntax-aware alignment) can replace the alignment later
with no protocol or client change, and both sides' true bytes are always
recoverable via `GIT_BLOB` by the oids in the `DIFF_ENTRY`.

### `GIT_INDEX`

Enumerates index entries under a `path` prefix (empty = all). Conflicted
paths appear as their stage-1/2/3 entries. Response `flags`: bit 0
`TRUNCATED`.

```text
INDEX_ENTRY 0x04: [kind:1][stage:1][iflags:1][mode:4][size:8][mtime_ns:8]
                  [oid:32][path_len:2][path:N]
                  iflags: bit 0 INTENT_TO_ADD, bit 1 SKIP_WORKTREE
```

### `GIT_BASE`

Merge bases as a first-class pull, for when the client needs the ancestor
oid itself — fetching the base side of a 3-way conflict view, or choosing
a diff base across several tips (`n_oids` ≥ 2; octopus allowed). `bases`
comes best-first (what `git merge-base` would print first); `n_bases` `0`
with `OK` means disjoint histories. The answer is immutable per oid set,
so it caches forever like every other pull.

## Limits and defaults

| Knob                            | Default        | Env                          |
| ------------------------------- | -------------- | ---------------------------- |
| Open repos per connection       | 16             | `BLIT_GIT_MAX_REPOS`         |
| Log subscriptions per repo      | 64             | `BLIT_GIT_MAX_LOG_SUBS`      |
| Ref settle window               | 50 ms          | `BLIT_GIT_REFS_LATENCY_MS`   |
| Status settle window            | 500 ms         | `BLIT_GIT_STATUS_LATENCY_MS` |
| Blob / patch size cap           | 16 MiB         | `BLIT_GIT_BLOB_MAX`          |
| Commits per `GIT_LOG`           | 256 (max 4096) | `BLIT_GIT_LOG_MAX`           |
| Records per response            | 10 000         | `BLIT_GIT_ENTRIES_MAX`       |
| Commits visited per walk        | 100 000        | `BLIT_GIT_WALK_MAX`          |
| Uncompressed bytes per response | 8 MiB          | `BLIT_GIT_BYTES_MAX`         |

Budget exhaustion degrades, never surprises: `GIT_LOG` paginates
(`MORE` + frontier), enumerations truncate (`TRUNCATED`), sized pulls
refuse with the true size (`TOO_LARGE`), and unpaginatable walks
(`GIT_BASE`, `UPSTREAM` counting) answer `BUDGET` or clear
`COUNTS_VALID`. Only repo-level failures close the repo (`GIT_CLOSED`
reason `4`). Two settle windows because ref moves are cheap to re-read
and users feel their latency, while status recomputation walks the
worktree.

## Server implementation

A new `blit-git` crate wired into `blit-server`, on **gitoxide** (`gix`):
pure Rust, no C dependency, fits the static and Nix builds; pack access is
mmap-based and fast enough that requests are served directly from
blocking-pool threads. `git2`/libgit2 would work but drags a C toolchain
into every target; shelling out to `git` costs a spawn per request and a
porcelain-parsing layer that this protocol exists to avoid.

Per opened repo, one engine (thread + inbox, the [fs-watch.md](fs-watch.md)
engine shape) owns the `GIT_STATE` stream. It reuses `blit-fssync`'s
backend hints: a watch on the gitdir (HEAD, `refs/`, `packed-refs`,
`index`, `logs/refs/stash`, `config` (upstream mapping), `MERGE_HEAD`,
`rebase-merge/`, `sequencer/`, and the linked worktree's private dir)
drives ref/op/upstream/stash snapshots; with `STATUS`, a watch on the
worktree drives status recomputation through gix's stat-cache-aware
status. Ahead/behind counts memoize by `(tip, upstream)` oid pair,
accelerated by commit-graph generation numbers, bounded by
`BLIT_GIT_WALK_MAX` (over budget: `COUNTS_VALID` cleared, never a stall).
Requests (`GIT_LOG`, `GIT_TREE`, `GIT_BLOB`, `GIT_DIFF`, `GIT_PATCH`,
`GIT_INDEX`, `GIT_BASE`, `GIT_RESOLVE`) do not go through the engine — they
are stateless reads against the object store and index, answered
concurrently. `GIT_LOG_WATCH` is the exception: it registers a subscription
on the engine, which re-resolves the spec and re-walks on each settled ref
change (sharing the gitdir watch above) and pushes `GIT_LOG_PAGE` under the
same one-in-flight coalescing pacing as `GIT_STATE`. A repo opened for
watched logs alone starts a log-only engine — the same thread, with the
`GIT_STATE` snapshot suppressed.

`GIT_PATCH` rows come from a plain line diff (`imara-diff`, already in
the tree via gix) with intraline span refinement on modified line pairs —
word- or character-granular, over raw or whitespace-normalized text, per
request flags; binary detection short-circuits to `BINARY`. The row
records are engine-agnostic by design, so a syntax-aware engine can
replace the alignment later, purely server-side.

Nothing runs under the session mutex; responses interleave with terminal,
surface, audio, and fs traffic through the existing per-client writer and
`S2C_FRAGMENT` fairness.

## Relation to filesystem sync

Complementary, and designed to compose: an IDE pane fs-syncs the worktree
for bytes-on-screen, git-watches the repo for decorations, `GIT_DIFF`
INDEX×WORKTREE names the dirty files, `GIT_BLOB` fetches the base for a
3-way view — each layer answering the question it is authoritative for.
Neither includes the other's data: git state never carries file content;
fs sync never interprets `.git`. The one lockstep piece is on the fs
side: `FS_SYNC`'s `EXCLUDE_GIT` flag ([fs-watch.md](fs-watch.md), landing
with `FEATURE_GIT`), so a worktree sync doesn't mirror object-store
churn. It is a pure name filter — fs sync still never reads git data.

## Security

Read-only by construction: no message mutates the repository, touches
remotes, runs hooks, or reads credentials. Discovery honors standard Git
layout only; the authority model is [fs-watch.md](fs-watch.md)'s — the
server already hands clients a shell, so this adds denial-of-service
surface, not privilege, and the mitigations are the budget table,
request validation (unknown flags/kinds, NULs, oversized paths, bad oids
rejected), prompt teardown on disconnect, and never logging escaped names
as trusted text.

## Implementation notes

Landed across `crates/remote/src/git.rs` (codecs + `GitStateMirror`),
`crates/git` (gitoxide engine + `GIT_STATE`/log-watch engines),
`crates/server` (dispatch + e2e), `crates/cli/src/git.rs`
(`blit git status|log|diff` — `status` prints once, or streams with
`--watch`, reprinting only when the view changes; `log` takes a positional
revision or range, `-- <path>`,
`--follow`/`--first-parent`/`--full-message`/`--topo`, `--watch` for a
live-repainting log, and a full `--json`; `diff` takes git-style
endpoints — none, one, or two revisions, or an `A..B` / `A...B`
range — with `--staged`, a `-- <path>` filter, and `-p/--patch` for unified
hunks), and `js/core/src/git.ts` + `openRepo` on
`BlitConnection`/`BlitWorkspace` (whose handle adds `resolve` and
`watchLog`), with byte fixtures pinned across both codec implementations.
Deviations, all invisible to the wire contract and upgradable server-side:

- Rename/copy detection is an exact-oid join reported at similarity 100;
  content-similarity scoring can land later.
- `GIT_LOG`'s path filter compares the entry against the first parent
  only, and `FOLLOW` adopts the parent-side name of an identical blob —
  exact-rename following, not similarity-based.
- Topological order is applied within each delivered page (the walk
  itself is commit-time ordered), so cross-page topology can interleave
  under extreme clock skew.
- The `OP` record's `detail` field is not yet populated.
- `blit git diff` against the worktree passes `UNTRACKED`, so untracked
  files show as additions — unlike `git diff`, closer to `git status`. A
  CLI choice, not a protocol one: the flag is opt-in per request.
- SHA-256 repositories are wire-ready but blocked on gitoxide support.

## Rollout

1. `blit-remote`: `git` module (opcodes, record codecs, builders,
   `FEATURE_GIT`), TypeScript mirror in `@blit-sh/core`, byte fixtures
   both directions.
2. `blit-git`: engine + request handlers over `gix`, tested against
   fixture repositories (including SHA-256, linked worktrees, conflicts,
   renames, non-UTF-8 paths).
3. Server wiring, e2e; CLI (`blit git status|log|diff [--json]`), with
   `log` accepting revisions/ranges, path filters, and `--watch`, and
   `diff` accepting revisions/ranges (`A..B`, `A...B`), `--staged`, a path
   filter, and `-p` for unified hunks.
4. `workspace.openRepo(path)` in `@blit-sh/core`: live state map plus
   promise-returning `log`/`tree`/`blob`/`diff`/`patch`/`index`/`resolve`
   and a pushed `watchLog(spec, opts, onUpdate)`, all with an oid-keyed
   cache.
5. Revision resolution (`GIT_RESOLVE`) and server-pushed watched logs
   (`GIT_LOG_WATCH`) on a per-repo log engine, capped by
   `BLIT_GIT_MAX_LOG_SUBS`.
