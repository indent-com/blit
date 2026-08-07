# RFC: Git Introspection

- **Status:** Accepted, implemented
- **Date:** 2026-07-21, revised 2026-07-30 (second pass)
- **Companion to:** [fs-watch.md](fs-watch.md), [fs-write.md](fs-write.md)

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

**Second pass.** The first version shipped and a consumer built on it (the
Review panel in `indent-com/neo#3248`), which surfaced one recurring
failure: the server knew something and the wire did not carry it. A
bounded response could not say where it stopped, a rejection could not say
why, a rename that was not byte-identical read as delete + add, and a
binary file's patch record could not say whether it was added or deleted.
Pass one bounded the wire and got that right; what it under-delivered was
**legibility**. The sharpened rule, which the rest of this document
follows:

> Every bounded response says where it stopped, and every stopping point
> is resumable. Every rejection carries its code. Nothing the server
> computed is dropped between the engine and the consumer.

That pass reshaped message layouts in place rather than appending
compatibility tails: `FEATURE_GIT` remains the family's only feature bit,
and the handshake's `PROTOCOL_VERSION` is how a mismatched peer is turned
away. blit ships server, codecs, and clients from one version number, and
SSH remotes auto-install on first connection, so skew is bounded by
construction; where it is not, a refused handshake beats a diff whose
records are shifted by two bytes.

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

Each of these is refused for a reason, not merely unlisted. Where the
second pass took one on, it says so.

- **Mutation**: staging, committing, checkout, branching. Still out; the
  shape a mutation family would take is sketched under
  [Mutation](#mutation-proposed), which is the one part of this document
  that is a proposal rather than a contract.
- **Push and `ls-remote`.** Fetch is in (see [`GIT_FETCH`](#git_fetch)) —
  it was the one remote operation whose absence pushed real work back on
  every consumer, in the form of `git fetch` in a PTY with its exit codes
  screen-scraped off the terminal grid. Push and `ls-remote` are genuinely
  lower value for a read-oriented client.
- **Credentials.** blit stores, parses, and transmits no secret. A fetch
  runs the box's own `git`, which picks up whatever `credential.helper`
  the box's config names — the same thing the PTY workaround relied on.
  Remote URLs go out as configured; see Security.
- **Filter/smudge execution.** Running a `filter.<driver>.clean` program
  means spawning an arbitrary configured binary as a side effect of a
  read, and the read side is deliberately a pure function of the object
  store and the worktree. The two sides of a filtered path are instead
  _flagged_ incomparable (`FILTERED`), which removes the actively
  misleading whole-file rewrite without crossing that line. `text`/`eol`
  normalization, which needs no external program, is applied.
- **Hook execution** and general config access. Two specific config
  values are exposed because nothing else can answer the questions they
  answer: remote names/URLs (`STATE_REMOTE`) and a symbolic ref's target
  (`STATE_REF.target`). Neither is a key/value surface.
- **Submodule recursion.** Submodules are still separate repositories —
  but a client no longer has to guess where one lives:
  `GIT_OPEN.parent_repo_id` names it by `(parent, path)` and the server
  resolves the gitdir.

## Protocol

New `S2C_HELLO` feature bit:

```text
FEATURE_GIT = 1 << 7
```

Opcodes occupy the `0xA0` block in both directions; request/response pairs
share the opcode value. Gateway, proxy, and mux forward them unmodified.
All integers little-endian; the 16 MiB frame limit and
[protocol.md](protocol.md) framing apply.

| Direction | Opcode | Name              | Layout                                                                                                                                                         |
| --------- | ------ | ----------------- | -------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| C2S       | `0xA0` | `GIT_OPEN`        | `[nonce:2][flags:2][refs_latency_ms:2][status_latency_ms:2][src_pty_id:2][parent_repo_id:2][n_prefixes:2][(prefix_len:2, prefix:N)·N][path_len:2][path:N]`     |
| C2S       | `0xA1` | `GIT_CLOSE`       | `[repo_id:2]`                                                                                                                                                  |
| C2S       | `0xA2` | `GIT_ACK`         | `[repo_id:2][state_id:4]`                                                                                                                                      |
| C2S       | `0xA7` | `GIT_LOG`         | `[nonce:2][repo_id:2][flags:1][limit:2][path_len:2][path:N][n_tips:2][tips:32·N][n_hides:2][hides:32·N]`                                                       |
| C2S       | `0xAB` | `GIT_TREE`        | `[nonce:2][repo_id:2][flags:1][oid:32][path_len:2][path:N][after_len:2][after:N]`                                                                              |
| C2S       | `0xAC` | `GIT_BLOB`        | `[nonce:2][repo_id:2][flags:1][oid:32][path_len:2][path:N][offset:8][max_len:4]`                                                                               |
| C2S       | `0xAD` | `GIT_DIFF`        | `[nonce:2][repo_id:2][flags:1][rename:1][old_kind:1][old:32][new_kind:1][new:32][path_len:2][path:N][after_len:2][after:N]`                                    |
| C2S       | `0xAE` | `GIT_PATCH`       | `[nonce:2][repo_id:2][flags:2][context:1][rename:1][old_kind:1][old:32][new_kind:1][new:32][path_len:2][path:N][max_len:4][after_len:2][after:N][after_pos:8]` |
| C2S       | `0xAF` | `GIT_INDEX`       | `[nonce:2][repo_id:2][flags:1][path_len:2][path:N][after_len:2][after:N]`                                                                                      |
| C2S       | `0xA3` | `GIT_CANCEL`      | `[nonce:2]`                                                                                                                                                    |
| C2S       | `0xB0` | `GIT_BASE`        | `[nonce:2][repo_id:2][n_oids:1][oids:32·N]`                                                                                                                    |
| C2S       | `0xA6` | `GIT_RESOLVE`     | `[nonce:2][repo_id:2][spec_len:2][spec:N]`                                                                                                                     |
| C2S       | `0xA8` | `GIT_LOG_WATCH`   | `[log_id:2][repo_id:2][flags:1][limit:2][spec_len:2][spec:N]`                                                                                                  |
| C2S       | `0xA9` | `GIT_LOG_UNWATCH` | `[log_id:2][repo_id:2]`                                                                                                                                        |
| C2S       | `0xAA` | `GIT_LOG_ACK`     | `[log_id:2][repo_id:2][update_id:4]`                                                                                                                           |
| C2S       | `0xB1` | `GIT_DISCOVER`    | `[nonce:2][flags:1][depth:1][path_len:2][path:N][after_len:2][after:N]`                                                                                        |
| C2S       | `0xB2` | `GIT_BLAME`       | `[nonce:2][repo_id:2][flags:1][oid:32][start_line:4][line_count:4][path_len:2][path:N]`                                                                        |
| C2S       | `0xB3` | `GIT_REFLOG`      | `[nonce:2][repo_id:2][flags:1][limit:2][after_pos:8][ref_len:2][ref:N]`                                                                                        |
| C2S       | `0xB4` | `GIT_FETCH`       | `[nonce:2][repo_id:2][flags:1][timeout_ms:4][remote_len:2][remote:N][n_refspecs:2][(len:2, refspec:N)·N]`                                                      |
| S2C       | `0xA0` | `GIT_REPO`        | `[nonce:2][repo_id:2][status:1][oid_format:1][flags:1][workdir_len:2][workdir:N][gitdir_len:2][gitdir:N]`                                                      |
| S2C       | `0xA4` | `GIT_STATE`       | `[repo_id:2][state_id:4][flags:1][records:LZ4]`                                                                                                                |
| S2C       | `0xA5` | `GIT_CLOSED`      | `[repo_id:2][reason:1]`                                                                                                                                        |
| S2C       | `0xA7` | `GIT_COMMITS`     | `[nonce:2][status:1][flags:1][n_frontier:2][frontier:32·N][records:LZ4]`                                                                                       |
| S2C       | `0xAB` | `GIT_TREE`        | `[nonce:2][status:1][flags:1][records:LZ4]`                                                                                                                    |
| S2C       | `0xAC` | `GIT_BLOB`        | `[nonce:2][status:1][size:8][data:LZ4]`                                                                                                                        |
| S2C       | `0xAD` | `GIT_DIFF`        | `[nonce:2][status:1][flags:1][records:LZ4]`                                                                                                                    |
| S2C       | `0xAE` | `GIT_PATCH`       | `[nonce:2][status:1][flags:1][data:LZ4]`                                                                                                                       |
| S2C       | `0xAF` | `GIT_INDEX`       | `[nonce:2][status:1][flags:1][records:LZ4]`                                                                                                                    |
| S2C       | `0xB0` | `GIT_BASE`        | `[nonce:2][status:1][n_bases:1][bases:32·N]`                                                                                                                   |
| S2C       | `0xA6` | `GIT_RESOLVE`     | `[nonce:2][status:1][n_tips:2][tips:32·N][n_hides:2][hides:32·N]`                                                                                              |
| S2C       | `0xA8` | `GIT_LOG_PAGE`    | `[log_id:2][update_id:4][status:1][flags:1][n_frontier:2][frontier:32·N][records:LZ4]`                                                                         |
| S2C       | `0xB1` | `GIT_DISCOVER`    | `[nonce:2][status:1][flags:1][records:LZ4]`                                                                                                                    |
| S2C       | `0xB2` | `GIT_BLAME`       | `[nonce:2][status:1][flags:1][records:LZ4]`                                                                                                                    |
| S2C       | `0xB3` | `GIT_REFLOG`      | `[nonce:2][status:1][flags:1][records:LZ4]`                                                                                                                    |
| S2C       | `0xB4` | `GIT_FETCH`       | `[nonce:2][status:1][flags:1][records:LZ4]`                                                                                                                    |

One contiguous block, `0xA0`–`0xB4`, grouped by role — lifecycle, pushed
state, revision and log, object reads, then the repository-wide
operations — with `0xB5`–`0xBF` reserved for what comes next. The family
was renumbered out of `0x50` when the second pass broke the wire anyway:
there was no reason to carry a split allocation forward, and the freed
block is available to a future family.

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
9 OTHER        an unclassified backend failure, diagnostic in the
              message's detail field where it has one
11 CONFLICT    a precondition failed (a lock was held, or the repository
              moved under a request)
12 NO_MERGE_BASE
              a MERGE_BASE endpoint over histories with no common
              ancestor: the request is well-formed, the repository has
              no such base
```

Codes 0–4 coincide with `FS_SYNCED`'s where the semantics overlap, so a
client's error mapping is one table, not one per message. `10` is lsp's
`WARMING`; `11` is the code [fs-write.md](fs-write.md) already assigned,
reused rather than given a synonym.

`OTHER` means _only_ "unclassified backend failure" — anything with a
classifiable cause (an invalid path, a wrong object type) returns the
specific code, because a consumer's whole reason for reading the status
byte is to tell a recoverable condition from a real error. `INVALID` is
held to the same rule from the other side: it means the request itself is
wrong, so a condition of the repository — disjoint histories, an unborn
HEAD — gets its own code rather than telling a correct client it made a
malformed request. The human-readable mapping is total on both sides:
`OTHER` and an unrecognized code render differently, so a log never
conflates "the backend failed" with "this build does not know that code".

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

Every emitted string carries a `u16` length, and one longer than that is
**clipped on a character boundary** rather than cast. A repository is
attacker-supplied: a tree entry or ref name has no length cap, and the
escaping above expands a non-UTF-8 byte roughly sixfold, so about 11 KB of
raw bytes escapes past 64 KiB. A wrapped prefix does not corrupt one field,
it desynchronizes every field after it in the response — a visibly
shortened name costs one unhelpful row instead.

### Continuation

Six responses can be cut short by a budget, and all six say where they
stopped. One record, reserved family-wide, carries the resume point:

```text
CURSOR 0x7F: [kind:1][after_len:2][after:N][pos:8]
             emitted last when a budget cut the response short. `after`
             is the escaped path of the last emitted item; `pos` is a
             position within it (rows delivered, for GIT_PATCH; 0
             elsewhere).
```

`GIT_TREE`, `GIT_INDEX`, `GIT_DIFF` and `GIT_PATCH` take a matching
`after` (and, for `GIT_PATCH`, `after_pos`): re-issue the same request
with the cursor's values and receive the remainder. Empty means "from the
beginning", so each message has one parse shape. The `*_TRUNCATED`
response flags keep their meaning and gain a companion rule:
**`TRUNCATED` with no `CURSOR` record means the response is genuinely
unresumable**, which after this pass is only the pathological state case
below.

Continuation is **stateless**, exactly as `GIT_LOG`'s frontier is — the
server holds nothing between requests. That requires a deterministic total
order, so one is now normative: tree and index entries in git's own path
order, diff and patch entries by new path, falling back to the old path
for deletions. For `GIT_TREE` that order is git's tree order literally,
which sorts a subtree as if its name ended in `/` — so a subtree's cursor
`after` carries the trailing slash. It has to: `lib.rs` precedes `lib/` in
a tree and follows `lib` bytewise, and a cursor compared on bare names
drops or repeats one of them wherever a page breaks between the two. Unlike a commit walk, a tree/index/worktree enumeration is
not immutable, so a continuation can straddle a change; the contract is
[fs-watch.md](fs-watch.md)'s — per-item coherent, whole-response
best-effort. For COMMIT × COMMIT diffs, which are immutable, continuation
is exact.

A per-request budget override was considered instead and declined. A
client-settable ceiling the server clamps is the clamp with extra steps:
it does not let a client get a whole 40 MiB patch, it only lets one client
consume more of a shared box before hitting the same wall, and it makes
per-request memory a function of untrusted input. The env knobs stay
operator-facing.

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

`flags` (`u16`): bit 0 `WATCH` (stream `GIT_STATE`), bit 1 `STATUS`
(include index/worktree status records in state; implies `WATCH`), bit 2
`UNTRACKED` (status includes untracked files), bit 3 `IGNORED` (status
includes ignored files; implies `UNTRACKED`), bit 4 `TRACKING` (include
per-branch upstream records in state; implies `WATCH`), bit 5 `REMOTES`
(include one `STATE_REMOTE` record per configured remote; implies
`WATCH`). Bits 6–15 are reserved and a set bit is `INVALID`.

`src_pty_id` and `parent_repo_id` are plain fields with an `0xFFFF`
"none" sentinel rather than flag-gated tails, so the message has one parse
shape however it is used:

- `src_pty_id` names a pty whose live cwd `path` is joined onto before
  discovery ([ide.md](../ide.md) Decision 3).
- `parent_repo_id` makes `path` a **submodule path relative to that
  repo's worktree**: the server resolves the submodule's own gitdir and
  worktree (a `.git` file, `.git/modules/<name>`, or a relocated
  worktree) and opens it. `WRONG_TYPE` when the path is not a submodule
  of that parent, `NOT_FOUND` when it is not initialized _or_ initialized
  with no checkout yet (an empty directory — discovery from it would walk
  up and find the parent, which reads as "not a submodule" when the
  honest answer is "not checked out"), and `INVALID` when the path leaves
  the parent's worktree. That last one is checked after resolution, not
  just lexically: refusing `..` and absolute paths says nothing about a
  symlink inside the worktree pointing anywhere on the box. Either way
  the answer is one a UI needs, and one that guess-and-open reports as an
  indistinguishable failure. With the diff entry already carrying the
  old and new commit oids, a submodule bump becomes "here are the 12
  commits that came in" (`GIT_LOG old..new` on the child) instead of two
  hex strings.

Setting both is `INVALID`.

`ref_prefixes` bounds what the state stream watches: empty means every
ref, `refs/heads/` means branches only. A UI that renders branches and
never renders tags stops paying for tags at every settle — on a large
monorepo the difference between a 3 KiB snapshot and a 4 MiB one,
recomputed on every ref move.

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
(sparse-checkout active), bit 3 `LINKED` (linked worktree), bit 4
`WRITABLE`, bit 5 `FETCHABLE`. The last two answer capability **per
repository** rather than per connection, which is strictly more accurate —
a checkout can be read-only, or a box can have no `git` binary for
`GIT_FETCH` to run — and costs two bits of a byte that had four free.
`WRITABLE` is clear in this build (no mutation family); `FETCHABLE` is
clear when `BLIT_GIT_FETCH=0` or `git` is absent.

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
budget hit; counts still accurate up to the cap), bit 2 `PARTIAL`.

A snapshot too large for one message spans several `GIT_STATE` messages
sharing one `state_id`, each but the last carrying `PARTIAL`. The client
accumulates and replaces its map on the final chunk, so "complete
snapshot, replace the map" still holds and it never observes a half-built
state; only the final chunk is acked, so the one-in-flight pacing is
unchanged.

Records are emitted **most load-bearing first**, so a budget sheds what
nobody decorates with: HEAD and the operation, then HEAD's branch and its
upstream, then `refs/remotes/*/HEAD`, then branches, then remote branches,
then upstream/stash/remote records, then tags, then status. A repository
with 200 000 tags truncates its tags and keeps its branches. This matters
more than the chunking: a dropped `refs/remotes/origin/HEAD` reads as
"this branch has no base", which is silently wrong rather than visibly
partial, and ordering is what makes that unreachable.

Records:

```text
HEAD   0x01: [kind:1][flags:1][oid:32][name_len:2][name:N]
             flags: bit 0 DETACHED, bit 1 UNBORN; name = symbolic target
STATE_REF 0x02: [kind:1][flags:1][oid:32][peeled:32][name_len:2][name:N]
             [target_len:2][target:N]
             flags: bit 0 PEELED_VALID (annotated tag), bit 1 SYMBOLIC.
             target = the symbolic target's full ref name when SYMBOLIC,
             else empty. This is what turns refs/remotes/origin/HEAD from
             an oid into "the default branch is <this>", replacing a
             client-side HEAD → main → master ladder that gives a
             repository with any other default branch no answer at all.
             Besides refs/*, an in-progress operation streams its gitdir
             pseudo-refs — MERGE_HEAD (one record per line; octopus
             extras named MERGE_HEAD#2…), CHERRY_PICK_HEAD, REVERT_HEAD,
             REBASE_HEAD, and ORIG_HEAD (only while an op is live) —
             names with no refs/ prefix, so clients can style them apart
OP     0x03: [kind:1][op:1][oid:32][detail_len:2][detail:N]
             op: 1 merge, 2 rebase, 3 cherry-pick, 4 revert, 5 bisect;
             oid = the operation head (first MERGE_HEAD line for an
             octopus); detail = "step/total" for rebases, else "";
             absent record = no operation
STATUS 0x04: [kind:1][staged:1][unstaged:1][flags:1][oid:32]
             [old_len:2][old_path:N][path_len:2][path:N]
             staged/unstaged: ASCII ' ' A M D R T U, '?' untracked,
             '!' ignored (porcelain letters); flags: bit 0 CONFLICTED;
             old_path non-empty only for renames.
             oid is the worktree content's hash when the status walk read
             the file, else zero — see below
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
STATE_REMOTE 0x07: [kind:1][flags:1][name_len:2][name:N]
             [fetch_len:2][fetch_url:N][push_len:2][push_url:N]
             one per configured remote, with the REMOTES open flag;
             flags bit 0 DEFAULT; push_url empty when it equals fetch_url.
             URLs go out as configured, userinfo included — the caller
             already has a shell and can read .git/config, so stripping
             it would cost them the ability to reproduce the remote and
             buy nothing. Three named fields, no key/value access, no
             writes: enough to answer "is this checkout the repository
             this pull request belongs to" without parsing owner/name out
             of the worktree path and hoping the clone followed a
             convention.
```

**Pseudo-refs share the `STATE_REF` stream, and that is a migration
hazard.** Before this pass every key in a client's ref map began with
`refs/`; now `MERGE_HEAD`, `ORIG_HEAD` and their siblings appear there
too, while an operation is live. A consumer that inverts the map into
oid → names to decorate a log will render `ORIG_HEAD` as though it were a
branch unless it filters. They are not given a synthetic `refs/` prefix
precisely because they are not refs — git resolves them from the gitdir,
they vanish when the operation ends, and a prefix would make them
indistinguishable from a branch a client could check out. The rule is the
whole test: **a name with no `refs/` prefix is a pseudo-ref**, which the
TypeScript client exports as `isGitPseudoRef` so no consumer has to
re-derive it.

**Why `STATUS` carries an oid.** The engine drops a snapshot byte-identical
to the last one it sent — the stream's contract is "latest state", and a
repeated snapshot carries none. Without the oid that suppression swallowed
a real event: writing a file that is already `M` and stays `M` changes the
worktree and not the record, so no frame went out, no `state_id` moved, and
a consumer diffing the worktree had nothing to key a refetch on. That is
exactly what an agent editing one file over and over does. The status walk
has already read the file to decide the letter, so hashing what is in hand
costs nothing and makes the existing dedupe do the right thing with no new
concept. Zero when the walk short-circuited without reading (an over-cap
file, two content-addressed sides), which is also when there is nothing new
to say. The same hash lands on `DIFF_ENTRY.new_oid`, so a worktree-side oid
is real whenever the content was read rather than only when the index
happened to know it.

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
between, so the client never parses git syntax. `spec` is one or more
whitespace-separated git revision expressions — tips and hides merge
across tokens, exactly like `git rev-list` arguments, so `base..a b ^c`
logs from a base to multiple heads in one spec. Each token is any single
expression: a ref (`main`, `origin/main`, `v1.0`), a (short) oid,
a relative form (`HEAD~3`, `main^2`), or a range — `A..B` (`B` reachable but
not `A`), `A...B` (symmetric difference, bounded by the merge base), and the
`^A` / `A --not B` exclusion forms. The reply lists the resolved commit oids
as `tips` and `hides`; feed them straight into `GIT_LOG` (or `GIT_LOG_WATCH`,
which accepts the same multi-token specs). A bare ref or oid yields one tip
and no hides.

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
`flags`: bit 0 `TRUNCATED` (entry budget), paired with a `CURSOR` record;
`after` continues the listing. Request `flags` is reserved and a set bit
is `INVALID`.

```text
TREE_ENTRY 0x02: [kind:1][otype:1][mode:4][oid:32][name_len:2][name:N]
                 otype: 1 commit (submodule), 2 tree, 3 blob
                 mode: raw git mode (100644, 100755, 120000, 40000, 160000)
```

### `GIT_BLOB`

The pull for object content. `oid` names a blob directly, or a
commit/tag/tree resolved through `path`. The effective cap is
`min(max_len, BLIT_GIT_BLOB_MAX, MAX_DECOMPRESSED)`, with `max_len` `0`
meaning the server default — the numbers can never disagree.

**A read is a window, not all-or-nothing.** The server returns bytes
`[offset, offset + cap)` and `size` is always the true object size, so a
client walks a large object and knows it is done when
`offset + data.len() == size`. `offset > size` is `INVALID`;
`offset == size` is `OK` with no data. `(oid, offset, len)` is as
content-addressed as `oid` was, so the client cache generalizes with no
invalidation story.

`flags` bit 0 `WHOLE` asks for the old behavior explicitly — the entire
object or `TOO_LARGE` with the true size. That case is real: a caller that
must hash or parse a whole file gains nothing from a prefix and should not
pay for one. But it is a request the caller makes, not a default imposed
on the viewer that would happily render the first 500 lines of a 20 MiB
generated file and used to get nothing at all.

`status`: `UNKNOWN_ID`, `NOT_FOUND`, `WRONG_TYPE`, `INVALID`,
`TOO_LARGE` (only under `WHOLE`), `OTHER`. `data` is raw object bytes,
LZ4, fragmented as needed.

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
| All work since it forked  | MERGE_BASE (upstream) | WORKTREE       |

A MERGE_BASE old side pairs with COMMIT, INDEX or WORKTREE. INDEX and
WORKTREE carry no oid, so the base is taken against HEAD — the commit
their contents are staged and edited on top of. That is the review view of
a branch that is still being worked on: everything since the fork,
committed or not, in one request. The other kinds stay `INVALID`: EMPTY
names nothing to fork from and TREE has no ancestry, so neither has a
merge base to compute.

With a MERGE_BASE endpoint the response opens with a `BASE` record naming
the chosen base (what `git merge-base` would pick), so per-file follow-ups
become oid-addressed and cacheable forever by `(base, topic, path)`.

**Rename detection** is `RENAMES` plus a `rename` similarity threshold:
`0` is the exact-oid join (byte-identical moves only, reported at
similarity 100), `1..=100` a percentage — git's own default is 50 — and
anything above is `INVALID`, on `GIT_PATCH` exactly as on `GIT_DIFF` —
the two share the field, so they share the rejection rather than one of
them quietly falling back to the exact-oid join. Threshold 0 finds nothing in a real pull
request, because a rename with one character changed reads as delete +
add; scoring is what makes the flag mean something. It runs after the
exact join, over the unmatched add/delete candidates only, and is bounded
by `BLIT_GIT_RENAME_LIMIT` (git's `diff.renameLimit`, same default);
past the limit the response falls back to the exact join and sets
response `flags` bit 1 `RENAME_LIMIT`, so a client can say "rename
detection skipped" rather than showing pairs it cannot explain.

`flags`: bit 0 `RENAMES` (rename/copy detection), bit 1 `UNTRACKED`
(worktree endpoint reports untracked files as additions), bit 2 `IGNORED`,
bit 3 `IGNORE_SPACE_CHANGE` (runs of whitespace compare equal and
trailing whitespace is ignored — git's `-b`), bit 4 `IGNORE_ALL_SPACE`
(whitespace ignored entirely — git's `-w`). With either ignore bit set,
entries whose changes vanish under the normalization are omitted and `st`
reflects the normalized comparison; oids still name the true blobs.
`path` filters to a subtree. `status` as `GIT_TREE` plus `INVALID`
(e.g. INDEX/WORKTREE on a bare repo, MERGE_BASE on the new side or
against an EMPTY or TREE new side), `NO_MERGE_BASE` (the histories share
no ancestor), `WRONG_TYPE` (a MERGE_BASE operand that does not peel to a
commit) and `NOT_FOUND` (an absent oid, or an unborn HEAD standing in for
an oid-less new side: that request is well-formed, the repository simply
holds no commit to take the base against, so a client can degrade to
another view rather than read it as a request it built wrong). A
MERGE_BASE operand peels like a revision spec, so an annotated tag names
its commit. Response `flags`: bit 0 `TRUNCATED`.

```text
DIFF_ENTRY 0x03: [kind:1][st:1][similarity:1][dflags:1]
                 [old_mode:4][new_mode:4][old_oid:32][new_oid:32]
                 [old_len:2][old_path:N][new_len:2][new_path:N]
                 st: ASCII A M D R C T U; similarity 0-100 (renames/copies)
                 dflags: bit 0 BINARY, bit 1 SUBMODULE, bit 2 FILTERED
BASE       0x04: [kind:1][oid:32]
                 first record when a MERGE_BASE endpoint was used: the
                 base the server chose
```

**Attributes.** The worktree side is normalized per the path's `text`/`eol`
gitattributes before comparison, so a CRLF checkout of an LF-normalized
object is not reported as every line changed. `RAW` (`flags` bit 5) opts
out, for a caller that genuinely wants on-disk bytes compared as they are.
Normalization is the default rather than opt-in because the un-normalized
answer is wrong, and a flag everyone is expected to set is a default with
extra steps.

A path whose attributes name a `filter` driver is a different case: with
`filter=lfs` the object store holds a ~130-byte pointer and the worktree
holds the asset, so the two sides are not comparable at all and every
LFS-tracked file would read as a total rewrite whether or not the user
touched it. blit does not run the filter — that would mean spawning a
configured program as a side effect of a read — it sets `FILTERED` and
emits no rows, the way a binary file behaves, so a client renders
"filtered file changed" instead of a wrong 4000-line diff.

Worktree-side oids are zero unless the file's content hash is known — from
the index, or because the diff itself read and hashed the file. Neither case
means the object database holds that blob, so a worktree side's content is
always read from disk. Worktree reads use the torn-read discipline of
[fs-watch.md](fs-watch.md): per-file coherent, tree-wide best-effort — no
filesystem offers more.

**Submodules.** A gitlink's path is a directory on disk, so a worktree side
cannot read it as a file: it takes its oid from the HEAD of the repository
checked out there — the oid its gitlink would get if it were staged — and
the `SUBMODULE` dflag says the two sides name commits, not blobs. A
submodule that is registered but not checked out (a clone that never ran
`submodule update`, leaving an empty directory) has no HEAD to read and
reads as unchanged rather than deleted; git says nothing about it either.
The untracked walk stops at a gitlink for the same reason: what is inside
belongs to the submodule's own status, not the superproject's.

### `GIT_PATCH`

Same endpoint spec as `GIT_DIFF` (including MERGE_BASE, with the same
leading `BASE` record — kind `0x04` here too) plus `context` (context
lines, `0` → 3) and a `path`: non-empty for one file's patch, empty for
the whole diff (subject to `max_len`, `TOO_LARGE` when over — distinct
from `INVALID` for bad endpoints). File-level records first (`GIT_DIFF`),
hunks on demand keeps the common case (status pane) cheap and the
expensive case (full patch) explicit.

Request `flags` is a `u16` whose low six bits are **exactly** `GIT_DIFF`'s
(including the ignore-whitespace bits and `RAW`), so there is one shared
prefix rather than two overlapping numberings, and adds: bit 6 `TEXT` —
return a classic unified diff (UTF-8, escaped paths in headers) as raw
`data`, for consumers that feed `git apply` or archive patches; bit 7
`CHAR_SPANS` — character-granularity spans instead of the default word
granularity; bit 8 `NO_SPANS` — skip intraline refinement entirely, for
whole-line renderers; bit 9 `BINARY` — git's `--binary`, emitting binary
content as a `GIT binary patch` block instead of the `Binary files … differ`
sentence. `rename` is `GIT_DIFF`'s similarity threshold.
Response `flags`: bit 0 `STRUCTURED` (`data` is records, the
default), bit 1 `TRUNCATED`.

Continuation is per **row**, not per file, which is what makes a diff with
one enormous file finishable: the `CURSOR` names the file the budget
stopped inside and `pos` counts the row and gap records of it already
delivered, and the next request re-emits that file's `PATCH_FILE` header —
so a page always says which file its rows belong to — and continues from
`after_pos`. A cut _between_ files is the same record with `pos` `0`,
meaning "past this file entirely"; `pos` is therefore not monotone across
pages, and a client resumes by echoing the pair rather than by comparing
it. The budget is a stopping threshold rather than a hard ceiling: the
record that crosses it has been written, so a response can exceed
`max_len` by one row.

**`TEXT` mode truncates only at a file boundary**, and carries no `CURSOR`:
a unified diff cut between a hunk header and its rows is not a patch, and
the payload is text with nowhere to put a record. `TRUNCATED` there means
"re-issue with `after` set to the last `+++` path you were given", which
the client is already holding. A single file whose unified text alone
exceeds the budget is `TOO_LARGE` — a status, not a truncation, so the
"`TRUNCATED` carries a `CURSOR`" rule stands. Structured mode never
refuses for size.

**The default response is structured**: aligned row records, so clients
render side-by-side or inline with a loop, never a unified-diff parser. A
row pairs an old line with a new line; change _spans_ mark the byte
ranges within each side that differ (intraline refinement of modified
pairs). A context row has no spans:

```text
PATCH_FILE 0x01: [kind:1][st:1][similarity:1][flags:1]
                 [old_len:2][old_path:N][new_len:2][new_path:N]
                 begins a file section. st/similarity mirror DIFF_ENTRY
                 field for field — one status alphabet and one field
                 order across both views — because a binary or empty
                 added file emits no rows at all and would otherwise be
                 unable to say whether it was added, deleted or
                 modified, which is the one thing text mode could
                 express that records could not. old_path carries the
                 old path whenever there is an old side, not only for
                 renames; st disambiguates.
                 flags: bit 0 BINARY, bit 1 FILTERED (both: no rows)
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

### `GIT_PATCH_TEXT` output

Text mode emits **git's own patch format**, not a subset, so a parser
written against `git diff` works unchanged:

```text
diff --git a/<old> b/<new>
[old mode <m>] [new mode <m>]        mode changed, content also changed
[deleted file mode <m>]              st == D
[new file mode <m>]                  st == A
[similarity index <n>%]              st == R or C
[rename from <old>] [rename to <new>]
[copy from <old>] [copy to <new>]
index <old_oid>..<new_oid>[ <mode>]
--- a/<old> | /dev/null
+++ b/<new> | /dev/null
@@ hunks, with "\ No newline at end of file" where needed
```

A binary file reads `Binary files a/<old> and b/<new> differ`, git's exact
sentence. A pure rename with no content change is a well-formed git rename
patch rather than a lone `diff --git` line.

A pure mode change emits the `diff --git` line and the two mode lines and
stops, as git does — there is no content to describe. Hunk ranges follow
git's spelling exactly: a zero-length side starts at `0`, a one-line side
omits its count (`-1`, not `-1,1`), and the closing `@@` carries the
section heading xdiff's default picks (the nearest preceding line starting
with an alphabetic character, `_`, or `$`).

Two deliberate deviations remain, documented rather than discovered — a
third, binary content, is closed below:

- **`index` carries full-length oids**, not `core.abbrev` abbreviations.
  A unique short oid costs an object-database probe per side per file, and
  every consumer of an `index` line either ignores it or wants the full
  oid. `git apply` accepts either. (The mode suffix follows git's rule: on
  the `index` line only when unchanged, since an add or delete already
  stated it.)
- **`similarity index` reports blit's score**, not git's, because the
  rename scorer is blit's own (see Implementation notes).
- ~~**Binary content is not emitted**~~ — closed. `BINARY` (git's
  `--binary`) emits git's `GIT binary patch` block, and without it the
  response carries `Binary files … differ`, which is `git diff`'s own
  behaviour without the flag. Both bodies go out, forward and reverse, as
  `emit_binary_diff` writes them — the second is what `git apply -R`
  replays, and a block with only the first is not the format. Literals
  only, never deltas: `git apply` reads both, and producing a delta means
  carrying git's delta encoder for bytes on the wire rather than
  correctness. Verified by _applying_, not by matching bytes:
  `git apply --binary` and `git apply --binary -R` against the pre-change
  tree, with the results compared byte for byte
  (`binary_patch_applies_with_git_apply`). The block does come out
  identical to `git diff --binary`'s today, deflate stream included, but
  what is promised is that git accepts it — two zlib implementations at
  the same level may compress the same bytes differently and both be
  right.

Verification is differential, not an assertion of intent
(`text_patch_matches_git_diff`): a fixture repository covering add /
delete / modify / rename-with-edit / mode change / binary /
no-trailing-newline is diffed with the system `git` and compared line for
line, with only those deviations normalized away. It was worth
writing — it caught four format differences the eye had passed over,
including the hunk-range spelling and the section heading.

### `GIT_DISCOVER`

"What repositories are under this path", so a client stops probing a
ladder of candidate paths and walking directories itself with a
non-recursive `FS_SYNC` per level — an fs sync per directory level, on a
family whose purpose is watching, purely to enumerate names.

```text
REPO_FOUND 0x01: [kind:1][flags:1][workdir_len:2][workdir:N]
                 [gitdir_len:2][gitdir:N]
                 flags: bit 0 BARE, bit 1 LINKED, bit 2 SUBMODULE
```

`depth` `0` → the default (4), clamped to `BLIT_GIT_DISCOVER_DEPTH_MAX`.
Request `flags`: bit 0 `NESTED` (descend into a repository once one is
found — off by default, so a tree full of vendored checkouts costs
nothing), bit 1 `BARE`. Results dedupe by canonical gitdir, which is the
identity `GIT_REPO` reports and the one that survives several paths
resolving to one repository. Bounded by `BLIT_GIT_DISCOVER_MAX` results
and `BLIT_GIT_DISCOVER_SCAN_MAX` scanned entries, with a `CURSOR` for the
remainder.

Because the walk is stateless, a resume replays it to reach the cursor —
and while it is replaying, neither budget is charged. Both bound the _new_
work a request does, or a page counted from the start of the walk would
stop at the same repository on every call and the cursor would never move.

The replay also needs the walk to be reproducible, so **sibling
directories are visited in path order**: `read_dir` guarantees no ordering
at all, and a page that reached the cursor's repository at a different
point would skip or repeat its neighbours. That is the same requirement the
Continuation section makes normative, met the way a filesystem walk can
meet it. What it cannot promise is a tree that changes between pages —
per-item coherent, whole-response best-effort, as everywhere else.

**It allocates no repo ids**: an enumeration, not an open, so it cannot
exhaust the per-connection repo budget. Discovery is a filesystem walk and
inherits the fs family's authority unchanged — it finds nothing the caller
could not have found with `FS_SYNC` and a loop — and does not follow
symlinks out of the tree.

### `GIT_BLAME`

Line attribution, the question a review surface asks right after "what
changed". Client-side blame is not viable: it means a blob plus a diff per
commit per file, which is the round-trip pattern this family exists to
avoid, and it exhausts `entries_max` long before producing an answer.

```text
BLAME_RANGE 0x01: [kind:1][flags:1][commit:32]
                  [start_line:4][line_count:4][orig_start:4]
                  [orig_path_len:2][orig_path:N]
                  one per contiguous attributed range; orig_path empty
                  unless the range came from a different path
```

`oid` names the commit to blame from (zero = HEAD; the worktree is not
blameable — `INVALID`). `line_count` `0` means to end of file. Request
`flags`: bit 0 `FOLLOW_RENAMES` (git's `-M`), bit 1 `FOLLOW_COPIES`
(`-C`, materially more expensive, off by default).

**Author and message are deliberately absent.** The response carries
commit oids; the client resolves the distinct set with one `GIT_LOG`, or
finds them already in its oid-keyed cache. That keeps a viewport blame to
a few hundred bytes and keeps the "oid-addressed, cache forever"
discipline intact — the same reason `PATH_AT` carries an oid and not a
blob. Line ranges are the budget story: blaming a viewport is cheap,
blaming a 20 000-line file is not, so `line_count` scopes the walk,
`BLIT_GIT_BLAME_LINES_MAX` caps it, and `CURSOR` resumes it.

The requested range is **clamped to the file** rather than refused: a
viewport that reaches the last line, and `line_count` `0` from an offset,
are the ordinary cases, and gix rejects an inclusive range longer than the
file instead of clamping — so the file's length is read first. A
`start_line` past the end is `OK` with no records. `TRUNCATED` describes
the answer, not the request: it is set when the cap stopped the walk short
of the lines asked for, and the `CURSOR`'s `pos` is the last line
attributed. Blame resumes through `start_line` — one past that `pos` —
rather than through an `after` of its own, since the request already has a
field that says where to begin.

### `GIT_REFLOG`

```text
REFLOG_ENTRY 0x01: [kind:1][flags:1][old:32][new:32]
                   [time:8 i64 s][tz:2 i16 min][msg_len:2][msg:N]
```

`ref` empty means `HEAD`. `flags` bit 0 `OLDEST_FIRST` (default
newest-first, matching `git reflog`). Entry signatures are omitted: the
message carries the operation, which is what a caller reads. A ref that
exists but has never moved answers `OK` with no entries; only a ref that
does not exist is `NOT_FOUND` — the ref is resolved before the reflog is
read, because the reader itself cannot tell the two apart. That is `git
reflog show`'s own split: empty and exit zero for a tag, fatal for a name
it cannot resolve.

A reflog has no path to name a resume point with, so its continuation is
positional: `after_pos` is the number of entries already delivered from
whichever end `OLDEST_FIRST` selected, and a page cut short by `limit`
ends with a `CURSOR` whose `pos` is what to pass next. The file is
append-only, so the position is stable in the direction that matters;
entries landing between pages fall under the family's per-item-coherent,
whole-response-best-effort contract.

Two things this makes possible that nothing else does. An agent working in
a sandbox checkout for a session switches branches, resets, rebases and
amends; "what did this session do to the repository" is only answerable
from `HEAD`'s reflog, and the alternative is `git reflog` in a PTY with
the terminal grid scraped — that tax, for a purely local read. And it is
the only way to name an oid no longer reachable from any ref: `resolve`
cannot see an amended-away commit and `log` cannot reach it, but the
reflog has it, and once named the object works normally everywhere else.

### `GIT_FETCH`

The one remote operation on the wire. Without it a client cannot see a
pull request from a fork, a retarget, or a force-push, because blit only
sees objects already in the local store — so the workaround is `git fetch`
in a PTY, and it costs: exit codes that lie (a remote can refuse one
refspec of several and still exit zero), a two-step refspec fallback,
`GIT_TERMINAL_PROMPT=0` or a hang at a username prompt, fetched objects
anchored by hand so an unlucky `gc` cannot prune them, failure diagnosis
by reading git's last output line off the terminal grid, and a session, a
shell, a `PATH`, and a terminal-UI filter that has to know to hide it.

```text
FETCH_REF 0x01: [kind:1][flags:1][status:1][old:32][new:32]
                [name_len:2][name:N][detail_len:2][detail:N]
                flags: bit 0 FORCED, bit 1 PRUNED, bit 2 NEW,
                bit 3 TAG_UPDATE
```

git's flag alphabet is covered in full — ` ` fast-forward, `=` up to date,
`+` forced, `-` pruned, `*` new, `t` tag update, `!` rejected — because a
letter this parser does not handle is a ref the reply never mentions, which
is the failure the response exists to prevent. `t` keeps its own bit rather
than folding into `FORCED`: git distinguishes the two, and "the tag you
pinned now points elsewhere" is a different sentence from "this branch was
rewritten".

Request `flags`: bit 0 `PRUNE`, bit 1 `NO_TAGS`, bit 2 `ANCHOR` (write
each fetched tip under `refs/blit/fetch/<remote>/<n>` so a concurrent `gc`
cannot prune it before the client diffs it). `timeout_ms` `0` → the
default, clamped. One fetch per repo at a time; a second answers
`CONFLICT`. `GIT_CANCEL` applies. `PERMISSION` when `BLIT_GIT_FETCH=0` or
the open cleared `FETCHABLE`.

The reply's shape is the point: **"did I actually get these commits" is
answerable from it** — per-refspec status plus resulting oids — instead of
needing a `resolve` per commit afterwards to re-establish a truth the exit
code obscured.

**Implementation: a subprocess, not an in-process TLS stack.** This is the
one place the family contradicts its own "never shell out" stance, so the
reasoning is explicit. That stance is about _reads_: a spawn is real
overhead against a 2 ms tree listing, and porcelain parsing is fragile
against a format meant for humans. Neither transfers. A fetch is a network
operation measured in seconds, so a 2 ms spawn is noise, and
`git fetch --porcelain --atomic` emits a stable machine format
(`<flag> <old> <new> <ref>`) that is not porcelain-for-humans at all.
Against that, `gix` with `blocking-network-client` links a full TLS stack
into every blit binary — including the static musl build — for a feature
most deployments never use, and reimplements the parts of git's
configuration that make fetches work in practice: `url.<base>.insteadOf`,
`http.proxy`, `credential.helper` chains, `core.sshCommand`, per-host SSH
config, corporate CA bundles. Divergence there is invisible until a user's
fetch fails in a way their `git` does not.

So the server runs `git fetch --porcelain --atomic --no-write-fetch-head`
as a plain subprocess — no PTY, no shell, argv only — in the repo's
directory with the environment pinned (`GIT_TERMINAL_PROMPT=0`,
`GIT_ASKPASS`/`SSH_ASKPASS` disabled, `GIT_CONFIG_PARAMETERS` empty,
`stdin` closed), parses the porcelain lines into `FETCH_REF` records, and
reports git's stderr tail as `detail` on failure. A timeout kills the
process group. If `git` is not on `PATH`, `FETCHABLE` is clear on every
open and the opcode answers `OTHER`.

This keeps the credential boundary where it belongs: blit never stores,
parses, or transmits a secret; the fetch picks up whatever
`credential.helper` the box's config names, which is what the PTY
workaround already relied on. The difference is that the result comes back
structured instead of scraped.

## Mutation (proposed)

The one part of this document that is a proposal rather than a contract.
Everything read-side is first-class, so a review surface can show a
reviewer exactly what changed and act on none of it: stage a hunk, discard
a file, commit what is staged, each leaves the family for `git` in a PTY —
shell quoting, screen-scraping, a visible terminal — for operations that
are purely local and touch no network and no credential. It also splits
the mental model: state arrives on a watched stream, but a change the
client makes lands invisibly until the watcher notices.

Sketch, deliberately narrow. In: stage / unstage / discard by path;
commit; branch create, switch, delete — one opcode discriminated by an
`op` byte, following `LSP_QUERY`'s precedent. Out: push, rebase / merge /
cherry-pick as server-side operations, hooks, and hunk-level staging in a
first cut.

The interesting questions are ordering and observability, not the git
calls:

- **Serialization.** Mutations run on the repo's state engine thread, so
  two cannot interleave and none can race the snapshot describing it;
  reads stay on the stateless pool and stay concurrent.
- **Observability.** The reply carries the `state_id` it produced — the
  engine re-snapshots immediately rather than waiting out the settle
  window — so a client awaits the snapshot it was promised instead of
  guessing when the watcher will notice.
- **Preconditions.** A CAS on `state_id`, mirroring
  [fs-write.md](fs-write.md): `0` is unconditional, otherwise the mutation
  applies only if the current `state_id` matches, else `CONFLICT` with the
  current id, so the client rebases without a round trip.
- **In-flight reads.** Oid-addressed reads are unaffected; a read against
  `INDEX`/`WORKTREE` would carry the `state_id` it was computed at so a
  client can discard a diff of a tree that no longer exists.
- **Hooks** are never run, and the reply would say so rather than staying
  quiet — silently skipping a `pre-commit` hook the repository defines is
  exactly the quiet wrongness this pass is trying to remove.

## Limits and defaults

| Knob                            | Default        | Env                           |
| ------------------------------- | -------------- | ----------------------------- |
| Open repos per connection       | 16             | `BLIT_GIT_MAX_REPOS`          |
| Requests in flight per conn     | 16             | `BLIT_GIT_MAX_INFLIGHT`       |
| Log subscriptions per repo      | 64             | `BLIT_GIT_MAX_LOG_SUBS`       |
| Ref settle window               | 50 ms          | `BLIT_GIT_REFS_LATENCY_MS`    |
| Status settle window            | 500 ms         | `BLIT_GIT_STATUS_LATENCY_MS`  |
| Blob / patch size cap           | 16 MiB         | `BLIT_GIT_BLOB_MAX`           |
| Commits per `GIT_LOG`           | 256 (max 4096) | `BLIT_GIT_LOG_MAX`            |
| Records per response            | 10 000         | `BLIT_GIT_ENTRIES_MAX`        |
| Commits visited per walk        | 100 000        | `BLIT_GIT_WALK_MAX`           |
| Uncompressed bytes per response | 8 MiB          | `BLIT_GIT_BYTES_MAX`          |
| Rename candidate pairs          | 1 000          | `BLIT_GIT_RENAME_LIMIT`       |
| Blame lines per request         | 50 000         | `BLIT_GIT_BLAME_LINES_MAX`    |
| Discovery depth (max)           | 4 (16)         | `BLIT_GIT_DISCOVER_DEPTH_MAX` |
| Discovery results               | 256            | `BLIT_GIT_DISCOVER_MAX`       |
| Discovery entries scanned       | 100 000        | `BLIT_GIT_DISCOVER_SCAN_MAX`  |
| Fetch timeout                   | 120 s          | `BLIT_GIT_FETCH_TIMEOUT_MS`   |
| Fetch enabled                   | on             | `BLIT_GIT_FETCH=0`            |

`BLIT_GIT_MAX_REPOS` is **per connection**: `GitRepos` is constructed per
connection and the cap is checked against that map, so a leaky client
starves only itself. (Only the env read is process-wide, which is a
caching detail.) Repo _handles_ dedupe by canonical gitdir across opens,
so N opens of one repository cost N ids but one engine.

Budget exhaustion degrades, never surprises — and, since the second pass,
always continues: `GIT_LOG` paginates (`MORE` + frontier), enumerations
truncate with a `CURSOR` naming where they stopped, sized pulls window
(or refuse with the true size under `WHOLE`), and unpaginatable walks
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
`rebase-merge/`, `sequencer/`, `info/`, and the linked worktree's private
dir) drives ref/op/upstream/stash snapshots; with `STATUS`, a watch on the
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

Every ignore source the status walk reads is watched, wherever it lives —
what counts as untracked is decided by rules, and a rule change that raises
no event leaves the view showing the old answer with nothing to correct it.
In-tree `.gitignore` files ride the worktree watch; `$GIT_DIR/info/exclude`
rides the gitdir watch (`info/` is armed for it, and is redundant only
while the worktree watch already covers a `.git` inside the tree); and the
user's global ignore file — `core.excludesFile`, defaulting to
`$XDG_CONFIG_HOME/git/ignore` — is outside every root, so its _parent
directory_ is armed on its own (a watch on a file follows its inode past
the rename-over an editor performs, the same reason
[fs-watch.md](fs-watch.md) watches parents). That directory is armed for
one file: its siblings are ignored rather than falling into the
"unclassifiable, recompute anyway" case. A `config` change re-resolves the
path and moves the watch with it.

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

Read-only by construction with one named exception (`GIT_FETCH`): no other
message mutates the repository, runs a program, or reaches the network.
Discovery honors standard Git layout only; the authority model is
[fs-watch.md](fs-watch.md)'s — the server already hands clients a shell,
so this adds denial-of-service surface, not privilege, and the mitigations
are the budget table, request validation (unknown flags/kinds, NULs,
oversized paths, bad oids rejected), prompt teardown on disconnect, and
never logging escaped names as trusted text.

Four specifics worth naming:

- **Remote URLs are emitted as configured**, userinfo included. This is
  deliberate and follows the family's authority model rather than
  defecting from it: the server already hands this caller a shell, so a
  value they can `cat .git/config` for is not a secret the wire is
  keeping, and stripping it would only stop them reproducing the remote.
  The place to be careful is server-side logging, which the rule below
  already covers.
- **Cursors are untrusted paths.** `after` goes through the same
  validation as every other request path — escaping, NUL rejection, length
  caps, traversal refusal — because a resume token that is really a path
  is a path. It carries no server state, so a forged cursor can at worst
  name a different valid starting point.
- **Discovery** reveals only what `FS_SYNC` plus a loop already reveals,
  bounded by depth, result, and scan caps, and does not follow symlinks
  out of the tree.
- **Fetch** reaches the network and may execute a credential helper — the
  one the box's git config already names, run by a subprocess that is
  exactly the `git fetch` the user's own shell would run, with prompting
  disabled so a missing credential fails reportably instead of hanging.
  Operators who do not want server-initiated egress set
  `BLIT_GIT_FETCH=0`, which clears `FETCHABLE` on every open and refuses
  the opcode. `ANCHOR` writes refs under `refs/blit/fetch/`, a namespace
  no other tool uses; nothing else in the repository is modified.

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
**Second-pass status.** Implemented across `crates/remote` (codecs and
fixtures), `crates/git` (engine, plus `reads.rs` for the repository-wide
operations), `crates/server` (dispatch), and `js/core` (codec, mirror,
and the `GitRepoHandle` surface), with byte fixtures pinned identically on
both sides.

Still outstanding: a CLI surface for the second-pass reads
(`blit git blame|reflog|discover|fetch`), and the mutation family below,
which remains a proposal.

Deviations, all invisible to the wire contract and upgradable server-side:

- Rename similarity is blit's own scorer — weighted hashed-line overlap,
  git's `2·common/(a+b)` shape — rather than
  `gix_diff::rewrites::Tracker`. The tracker consumes gix tree-diff
  changes, and this pipeline diffs flattened `path → Side` maps so a
  single code path can span index and worktree endpoints that no tree
  diff sees. Copy detection (`C`) is not implemented; only renames.
- `GIT_LOG`'s path filter compares the entry against the first parent
  only, and `FOLLOW` adopts the parent-side name of an identical blob —
  exact-rename following, not similarity-based.
- Topological order is applied within each delivered page (the walk
  itself is commit-time ordered), so cross-page topology can interleave
  under extreme clock skew.
- The `OP` record's `detail` is populated for rebases only
  (`rebase-merge/msgnum`+`end`, `rebase-apply/next`+`last`); sequencer
  progress for multi-commit cherry-picks/reverts is not surfaced.
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
