# RFC: Language Intelligence

- **Status:** Implemented (`FEATURE_LSP`, protocol feature bit 8)
- **Date:** 2026-07-23
- **Companion to:** [fs-watch.md](fs-watch.md), [git.md](git.md)

Chosen over two sibling shapes — a raw JSON-RPC tunnel and an LSP-aware
passthrough multiplexer — evaluated against the same criteria; see
§ Alternatives for the comparison.

## Summary

Clients want what tools with a language server see: the errors in a tree
as they appear, and answers at a position — definition, references,
hover, symbols — without shipping an LSP client, JSON-RPC, or UTF-16
position math to every client, and without paying a language server's
multi-minute warmup per query.

The design terminates LSP at the blit server. The server hosts language
server processes the way it hosts PTYs — spawned lazily, shared, warm
across client connections — and is the **sole LSP client** of each: it
owns `initialize`, document synchronization, and every server→client
request. What clients see is a projection into blit-native records along
the family grain established by [git.md](git.md):

- **Mutable and small** — server phase, warmup progress, capabilities —
  is _pushed_ as whole-state snapshots under one-in-flight coalescing.
- **Diagnostics** — mutable, per-file, unbounded across a workspace —
  are _pushed_ as per-file replacement sets from a server-held cache,
  replayed in full to every fresh subscriber: a late joiner, a one-shot
  CLI, and a reconnect all just get current state.
- **Point-in-time answers** — definition, references, hover, symbols,
  rename plans — are _pulled_ by nonce request/response.

Every documented failure of LSP sharing — request-id rewriting,
capability intersection, dropped server→client requests, `didChange`
version corruption — exists only when N clients share one LSP stream.
Here N is 1 by construction: blit terminates the protocol, and blit
clients speak records. This is the shape every successful sharer
converged on (Zed, JetBrains Gateway, Live Share); every raw multiplexer
(lspmux, lspd, ra-multiplex) documents the wall this design never hits.

The contract is semantic ("definition at position"), not passthrough: as
`GIT_PATCH` rows are a presentation, not a contract with a diff
algorithm, a language could later be answered by tree-sitter or a SCIP
index with no protocol or client change.

## Goals

- One-shot CLI queries against warm shared sessions: every `blit lsp`
  invocation is a fresh connection, so language servers must be
  daemon-owned, keyed by workspace, surviving disconnects — rust-analyzer's
  warmup is paid once, amortized across every future query from every
  client.
- The thinnest possible correct client: apply records, ack. No JSON-RPC,
  no capability negotiation, no position-encoding awareness.
- Diagnostics as reliable state, not a stream: the edit → errors → fix
  loop is the highest-measured-value agent primitive, and it only works
  if a fresh subscriber always receives complete current state.
- Zero config: discover installed servers by root markers and PATH,
  degrade silently when absent, never download anything.
- Read-only by construction: no message mutates the worktree. Rename
  returns its edit plan as data; applying it is the client's business.
- Fit blit conventions: 1-byte opcodes, little-endian, `records:LZ4`,
  `S2C_FRAGMENT`, feature-bit gated, nonce request/response, budgets
  that degrade rather than surprise.

## Non-goals

- Mutation: applying workspace edits, code actions, formatting. All
  write-shaped features wait for the mutation/buffer RFC that
  [fs-watch.md](fs-watch.md) already anticipates.
- Editor-cursor UX: completion, semantic tokens, inlay hints, signature
  help, code lens. No surveyed agent bridge exposes them (the LLM is the
  completion engine), and no browser editor exists to consume them. They
  become worth revisiting only when one does.
- Client-driven document sync. No message carries document content from
  client to server — the version-ownership pitfall is closed by there
  being no opcode to misuse.
- Installing or updating language servers.
- Debuggers and non-LSP tools. A generic supervised-stdio family (DAP,
  REPLs) is a plausible later RFC and should reuse this family's process
  supervisor, but this protocol carries LSP projections only.

## Protocol

New `S2C_HELLO` feature bit:

```text
FEATURE_LSP = 1 << 8
```

Opcodes occupy the `0x60` block in both directions. Gateway, proxy, and
mux forward them unmodified. All integers little-endian; the 16 MiB
frame limit and [protocol.md](protocol.md) framing apply. Set
`BLIT_LSP=0` to disable the family (the feature bit is not advertised).

| Direction | Opcode | Name          | Layout                                                                                      |
| --------- | ------ | ------------- | ------------------------------------------------------------------------------------------- |
| C2S       | `0x60` | `LSP_OPEN`    | `[nonce:2][flags:1][diag_latency_ms:2][path_len:2][path:N]`                                 |
| C2S       | `0x61` | `LSP_CLOSE`   | `[lsp_id:2]`                                                                                |
| C2S       | `0x62` | `LSP_ACK`     | `[lsp_id:2][stream:1][update_id:4]`                                                         |
| C2S       | `0x63` | `LSP_QUERY`   | `[nonce:2][lsp_id:2][kind:1][flags:1][line:4][col:4][path_len:2][path:N][arg_len:2][arg:N]` |
| C2S       | `0x64` | `LSP_CANCEL`  | `[nonce:2]`                                                                                 |
| C2S       | `0x65` | `LSP_SERVERS` | `[nonce:2]`                                                                                 |
| C2S       | `0x66` | `LSP_STOP`    | `[nonce:2][server_ref:2]`                                                                   |
| S2C       | `0x60` | `LSP_OPENED`  | `[nonce:2][lsp_id:2][status:1][flags:1][root_len:2][root:N][detail_len:2][detail:N]`        |
| S2C       | `0x61` | `LSP_STATE`   | `[lsp_id:2][state_id:4][flags:1][records:LZ4]`                                              |
| S2C       | `0x62` | `LSP_DIAG`    | `[lsp_id:2][update_id:4][flags:1][records:LZ4]`                                             |
| S2C       | `0x63` | `LSP_QUERY`   | `[nonce:2][status:1][flags:1][records:LZ4]`                                                 |
| S2C       | `0x64` | `LSP_CLOSED`  | `[lsp_id:2][reason:1]`                                                                      |
| S2C       | `0x65` | `LSP_SERVERS` | `[nonce:2][status:1][flags:1][records:LZ4]`                                                 |
| S2C       | `0x66` | `LSP_STOPPED` | `[nonce:2][status:1]`                                                                       |

### Statuses

The [git.md](git.md) status table (codes 0–9, same numbers, same
semantics where they overlap) plus one addition:

```text
10 WARMING   the backing server has not finished initialize/indexing;
             retryable — the same request later will succeed
```

A matched-but-uninstalled server is named in the successful
`LSP_OPENED.detail` (`"gopls: not found on PATH"`), so an agent learns
what to install rather than guessing why a language is silent.

### Nonces and cancellation

Exactly [git.md](git.md)'s rules: one response per nonce in every
outcome, per-connection per-family namespaces, duplicates answered
`INVALID`. `LSP_CANCEL` maps to `$/cancelRequest` upstream — but
identical in-flight queries from different subscribers are coalesced
onto one upstream request, so cancels are refcounted: the upstream
cancel is sent only when every downstream waiter has cancelled.

### Positions, paths, and text

**Positions** are 0-based `line` plus `col` as a **UTF-8 byte offset
within the line**, in both directions, in every message. The server
transcodes to each backend's negotiated `positionEncoding` (UTF-16 for
gopls, pyright, typescript-language-server; UTF-8 where offered —
rust-analyzer, clangd) against the canonical file bytes it already
holds. No client ever learns UTF-16 exists. The CLI presents 1-based
`path:line:col` and converts at the edge.

**Paths** follow the family split: `LSP_OPEN.path` is plain UTF-8
(a filesystem location the client chose, like `FS_SYNC.path` /
`GIT_OPEN.path`); every path the server _emits_ is workspace-root-
relative in [fs-watch.md](fs-watch.md) escaped form, and query paths
echo emitted form, exactly like `FS_FETCH`. `file://` URIs never
appear on the wire; the server constructs them in exactly one place.

**Hashes** are fs-watch's BLAKE3-128 of file content. A `LOCATION` or
`DIAG_FILE` record carries the hash of the content version it describes
(zero when unknown), so a client that knows current bytes — it just
wrote them, or mirrors the tree via `FS_SYNC` — detects a stale answer
by comparison. The families compose; no staleness protocol is needed.

**Records** inside every `records:LZ4` payload use the family framing:
`[record_len:4][kind:1][…]`, unknown kinds skipped, malformed records
end the payload, kinds namespaced per message type. Responses are
bounded by the entries/bytes budgets with `TRUNCATED` flags — adding a
query kind or a record field is a codec addition, never a new opcode.

### `LSP_OPEN` / `LSP_OPENED`

`flags`: bit 0 `WATCH` (stream `LSP_STATE`), bit 1 `DIAGS` (stream
`LSP_DIAG`; implies `WATCH`). `diag_latency_ms` is the diagnostics
settle window (`0` → default 500 ms, clamped 1–10000).

`path` names any location inside a workspace. The server walks upward
for root markers (§ Sessions and discovery); the canonical root comes
back escaped in `root`. One open may attach to **several** backends —
a repo with `Cargo.toml` and `go.mod` gets rust-analyzer and gopls —
and `lsp_id` names the attachment, not a server: queries route by the
queried path's language, diagnostics merge across backends (each `DIAG`
record's `source` attributes it). A language whose marker matched but
whose binary is absent is simply missing from `LSP_STATE`; the
successful `LSP_OPENED.detail` names those absent binaries
(`"gopls: not found on PATH"`), so a client learns what to install up
front, and querying such a language's files answers `NOT_FOUND`.

On failure `lsp_id` = `0xFFFF` and `detail` carries a diagnostic; on
success `detail` is the (possibly empty) absent-binary list.
`LSP_CLOSE` releases the attachment (backends stay warm; § Sessions);
`LSP_CLOSED` (`reason`: `0` client request, `1` root gone, `2`
permission lost, `3` backend failure, `4` resource limit) ends it from
the server side.

### `LSP_STATE` / `LSP_ACK`

A complete snapshot of the attachment's mutable state — one `SERVER`
record per live backend — sent once after `LSP_OPENED` when `WATCH`,
then after every settled change, paced by one-in-flight coalescing
under `LSP_ACK(stream=0)`, exactly `GIT_STATE`.

```text
SERVER 0x01: [kind:1][server_ref:2][phase:1][progress_pct:1][caps:4]
             [epoch:4][refused_edits:4][rss:8][id_len:2][id:N]
             [msg_len:2][msg:N]
             phase: 0 spawning, 1 initializing, 2 indexing, 3 ready,
             4 failed; progress_pct 0-100 (255 unknown), from
             $/progress; caps: coarse capability bits aligned with
             query kinds, so clients can gray out what a backend
             cannot answer; epoch increments on dynamic capability
             (re)registration; refused_edits counts workspace/applyEdit
             requests answered applied:false; rss is best-effort
             resident set size in bytes (0 = unknown), sampled at
             LSP_SERVERS enumeration and on settle ticks, never polled
             hot — "rust-analyzer 3.2 GiB" is the visibility the
             MAX_SERVERS defense depends on, including over remote
             targets where a shell is a different surface; msg = last
             progress or showMessage line, for surfacing warmup
             ("indexing 42%")
```

`ready` means **quiescent**, not merely initialized: servers answer
`initialize` in milliseconds and start indexing after, with the first
`$/progress` trailing the handshake — reporting `ready` in that gap
would let `lsp wait` return into a wall of `WARMING`. A session is
promoted to `ready` only after it stays progress-idle through a grace
window (`BLIT_LSP_READY_GRACE_MS`) following `initialized` or the last
progress `end` — idle alone is not enough, since servers pause between
warmup stages (rust-analyzer's metadata → crate graph → indexing). A
server that reports quiescence explicitly overrides the heuristic in
both directions: blit advertises rust-analyzer's experimental
`serverStatus` notification, and from its first arrival `quiescent`
alone decides `ready`/`indexing`, with no grace window.

### `LSP_DIAG` / `LSP_ACK`

Diagnostics do not satisfy `GIT_STATE`'s smallness precondition (a
workspace mid-refactor can carry thousands), so they get their own
paced stream against a server-held **last-per-file cache**. Records:

```text
DIAG_FILE 0x01: [kind:1][hash:16][n:2][path_len:2][path:N]
                replaces the file's entire diagnostic set; n=0 clears;
                hash names the content version the set describes
DIAG      0x02: [kind:1][severity:1][flags:1][line:4][col:4]
                [end_line:4][end_col:4][code_len:2][code:N]
                [src_len:2][source:N][msg_len:4][msg:N]
                severity: LSP values (1 error … 4 hint); flags: bit 0
                UNNECESSARY, bit 1 DEPRECATED (LSP diagnostic tags)
```

`flags` bit 0 `FULL`: this update carries the complete diagnostic state
of the workspace — drop everything, then apply. Every `DIAGS` subscribe
begins with a `FULL` update (the cache replay: a fresh CLI invocation
or a late-joining tab never sees a blank gutter), and the server may
send `FULL` at any time instead of an incremental update — recovery and
normal operation are indistinguishable, the fs-watch principle in
one-message form. Otherwise updates carry replacement sets only for
files whose diagnostics changed, coalesced latest-per-file while the
client is unacked (`LSP_ACK(stream=1)`, cumulative): a slow client gets
fewer, larger updates covering more files, and never falls behind.

There is deliberately no wire-level quiescence barrier. LSP cannot
promise one (rust-analyzer interleaves flycheck cycles; gopls has no
reliable signal), and a protocol op that can lie is worse than none.
The honest primitive is correlation: `DIAG_FILE.hash` tells the client
_which_ content the diagnostics describe, and phase `ready` plus
hash-match is the CLI's `--wait` condition — heuristic settle windows
are a fallback, not a contract.

### `LSP_QUERY`

One request opcode; `kind` selects the operation, response records are
skippable, so the query surface grows without spending opcodes:

```text
1 DEFINITION   → LOCATION records
2 REFERENCES   → LOCATION records; flags bit 0 INCLUDE_DECLARATION
3 HOVER        → MARKUP record (+ optional LOCATION for the range)
4 DOC_SYMBOLS  → SYMBOL records, pre-order; line/col ignored
5 WS_SYMBOLS   → SYMBOL records; path empty, arg = query string
6 RENAME       → EDIT records; arg = new name; never applied
```

Reserved next kinds: implementation, type definition, call hierarchy.

Symbol answers are normalized server-side: hierarchical
`DocumentSymbol[]` and flat `SymbolInformation[]` responses both become
`SYMBOL` records (`depth` carries the hierarchy), and lazily-located
`WS_SYMBOLS` results are resolved before encoding (3.17
`workspaceSymbol/resolve`), so records always carry full positions —
clients never see a half-answer. For kind 5 the ignored `line` field is
reserved as a future SymbolKind bitmask filter (26 kinds fit 32 bits;
`0` = all) — a codec addition under the existing layout, not a new
opcode; until then kind filtering ("classes only") is a client concern.

```text
LOCATION 0x01: [kind:1][flags:1][hash:16][line:4][col:4]
               [end_line:4][end_col:4][path_len:2][path:N]
MARKUP   0x02: [kind:1][format:1][text_len:4][text:N]
               format: 0 plaintext, 1 markdown (as the server gave it)
SYMBOL   0x03: [kind:1][sym_kind:1][flags:1][depth:1][line:4][col:4]
               [end_line:4][end_col:4][name_len:2][name:N]
               [path_len:2][path:N]
               sym_kind: LSP SymbolKind; depth nests document outlines
               without a container field; flags bit 0 DEPRECATED
EDIT     0x04: [kind:1][flags:1][hash:16][line:4][col:4]
               [end_line:4][end_col:4][new_len:4][new_text:N]
               [path_len:2][path:N]
               the rename plan: ordered edits against the content
               version named by hash. Data, never applied — applying
               is the client's business until the mutation RFC.
```

Response `flags`: bit 0 `TRUNCATED` (an entries/bytes budget was hit),
bit 1 `INCOMPLETE` (a `RENAME` `WorkspaceEdit` carried whole-file
create/rename/delete operations, which v1 does not project — the `EDIT`
records are the text edits only, so the plan is partial). A client
must treat an `INCOMPLETE` rename as advisory, not a complete edit set.

Query timeout (`BLIT_LSP_TIMEOUT_MS`) answers `OTHER` with detail — a
hung backend pins a nonce for seconds, never indefinitely. Queries
before a backend finishes initialize answer `WARMING`.

### `LSP_SERVERS` / `LSP_STOP`

Daemon-scoped visibility and control, the `terminal list`/`close`
ethos: `LSP_SERVERS` enumerates every live backend (SERVER records as
in `LSP_STATE`, plus the escaped root), across all roots, regardless of
attachments. `LSP_STOP` shuts one down by `server_ref` (subscribers see
`LSP_STATE` lose the record; a later query respawns it). Observability
before force: agents are told to look before they kill.

## Sessions and discovery

Backends are **daemon-owned, keyed by `(canonical_root, server_id)`** —
the PTY model, not the fs/git model. Connection-scoped sessions are
absurd against multi-minute warmup; the entire point is that a fresh
one-shot CLI connection attaches to a warm backend in milliseconds. The
registry lives beside the PTY table; attachments hold strong refs, and
a backend with zero attachments starts an idle timer
(`BLIT_LSP_IDLE_SECS`) before `shutdown`/`exit` (escalating to kill) —
a deliberate third lifecycle, between fssync's drop-on-last-ref (too
eager for warmth) and the PTY's explicit-close (leak-prone for
processes this heavy).

Discovery is a compiled-in table (~10 entries: `Cargo.toml` →
`rust-analyzer`, `go.mod` → `gopls`, `tsconfig.json`/`package.json` →
`typescript-language-server --stdio`, `pyproject.toml` →
`pyright-langserver --stdio`, `compile_commands.json` → `clangd`, …).
Each entry declares its **root policy** — how the upward marker walk
chooses among nested matches, always bounded above by the git root
(existing gix discovery), which is also the fallback when no marker
matches:

- `outermost` — rust-analyzer: the outermost `Cargo.toml` is the cargo
  workspace; nearest would spawn a backend per member and lose
  cross-crate analysis (Zed's manifest providers make the same call).
- `nearest` — clangd (`compile_commands.json` is per build tree),
  typescript-language-server, pyright.
- gopls — outermost `go.work`, else nearest `go.mod`.

The policy decides `canonical_root`, and therefore backend identity and
sharing. Binaries are probed on PATH at open; absent means absent,
silently — the PipeWire/GPU-dlopen precedent. blit never downloads a
server. Escape hatch: `lsp.<id>.command` / `.args` / `.roots` /
`.root_policy` / `.init` / `.settings` keys in `blit.conf` shadow or
extend the table. `.init` and `.settings` hold **verbatim JSON**,
handed unread to `initializationOptions` and `workspace/configuration`
respectively — blit never validates, interprets, or documents
individual server settings, so per-server schema churn stays outside
blit forever (helix's `config` pass-through is the precedent). The
zero-config default remains empty configuration, which every server in
the table accepts.

**Commands come only from the compiled table or the user's config.**
Repository contents select which entry applies; they never define what
runs. `initializationOptions` and `workspace/configuration` answers
come from config alone.

## Document truth

Disk is the only truth, and document sync is **server-driven, never
client-driven** — no C2S message carries file content, so N-writer
version corruption is impossible rather than forbidden.

The engine reuses the fssync shared-root watcher (one native watcher
per tree, [fs-watch.md](fs-watch.md)) to feed
`workspace/didChangeWatchedFiles`, honoring dynamic watcher
registrations. Because several major servers diagnose only open
documents (typescript-language-server, pyright, clangd), the engine
maintains an **open set** from day one, admitted by three signals:
files recently changed on disk (watcher-dirty), files a subscriber
requested diagnostics for, and files recently queried — LRU-capped
(`BLIT_LSP_MAX_DOCS`), `didOpen`ed with disk bytes and re-`didChange`d
(full text) on settled watcher hints, versions minted by the engine.
Dirty-driven admission makes the primary loop work by construction: the
file an agent just saved is opened and diagnosed without ceremony. What
remains partial on open-doc-only servers is cold coverage — a file
never touched in the daemon's lifetime carries no diagnostics, and the
absence of a `DIAG_FILE` record means unknown, not clean. The engine
may later fill cold files opportunistically through 3.17
`workspace/diagnostic` pull where servers implement it — an engine
enhancement, no wire change. (Cycling the whole workspace through
didOpen/didClose to force coverage was considered and rejected:
open-doc-only servers typecheck per open, so it is unbounded thrash on
large trees.)

Intelligence therefore reflects **saved state** — exactly what agents
(who write disk) and every read-only viewer see. When the buffer/editor
RFC lands, buffers become an alternate byte source into the same
single-writer projection: versions stay engine-minted, the wire keeps
carrying `(path, line, col)`, and clients do not change.

Every server→client LSP request terminates in blit:
`workspace/configuration` from config (empty defaults);
`client/registerCapability`/`unregister` into an internal table, epoch
bumped in `SERVER`; `window/workDoneProgress/create` + `$/progress`
into phase/percent; `workspace/workspaceFolders` from the root;
`window/showMessage`\* into the `SERVER` msg field and server log;
`workspace/applyEdit` answered `applied:false` and counted — read-only
by construction, [git.md](git.md)'s stance.

## Limits and defaults

| Knob                             | Default        | Env                                           |
| -------------------------------- | -------------- | --------------------------------------------- |
| Backends per daemon              | 4              | `BLIT_LSP_MAX_SERVERS`                        |
| Attachments per connection       | 16             | `BLIT_LSP_MAX_OPENS`                          |
| Queries in flight per connection | 16             | `BLIT_LSP_MAX_INFLIGHT`                       |
| Open documents per backend       | 128            | `BLIT_LSP_MAX_DOCS`                           |
| Diagnostics settle window        | 500 ms         | `BLIT_LSP_DIAG_LATENCY_MS`                    |
| Query timeout                    | 30 s           | `BLIT_LSP_TIMEOUT_MS`                         |
| Initialize timeout               | 60 s           | `BLIT_LSP_INIT_TIMEOUT`                       |
| Ready quiescence grace           | 1 s            | `BLIT_LSP_READY_GRACE_MS`                     |
| Idle shutdown                    | 900 s          | `BLIT_LSP_IDLE_SECS`                          |
| Records / bytes per response     | 10 000 / 8 MiB | `BLIT_LSP_ENTRIES_MAX` / `BLIT_LSP_BYTES_MAX` |
| Restarts per backend             | 3/hour         | `BLIT_LSP_MAX_RESTARTS`                       |
| Spawns per daemon                | 30/minute      | `BLIT_LSP_SPAWN_RATE`                         |

Exhaustion degrades: truncation flags, `WARMING`, `BUDGET`, never a
hang. RSS is honestly uncappable portably; `MAX_SERVERS=4` plus
`lsp list`/`stop` visibility is the real defense on shared boxes —
rust-analyzer alone can hold multiple GiB.

## Server implementation

A new `blit-lsp` crate wired into `blit-server`, on **async-lsp** (the
one maintained crate designed for the client role; tower-based, typed
via `lsp-types`) over stdio pipes — no PTY. Per backend, one engine
(thread + inbox, the family shape) owns the LSP session, the open set,
the diagnostics cache, and per-attachment subscriber cursors with their
own outboxes and ack pacing (fssync's reconciler/subscriber split, with
strong refs). Queries route through the engine — LSP sessions are
ordered — but transcoding and record encoding run off the session
mutex; responses interleave with terminal, surface, audio, fs, and git
traffic through the existing per-client writer and `S2C_FRAGMENT`
fairness.

Two implementation traps, named now:

- **Reaping.** The daemon's 5-second `waitpid(-1)` backstop reaps any
  child, which _races_ a supervisor doing its own `wait()` — exit
  statuses get stolen (`ECHILD`). The engine `wait()`s (and kills, on
  timeout) its own child on every path so it usually wins; the backstop
  was made **selective**, reaping every child to avoid zombies but only
  _parking_ statuses for PTY-owned pids (`register_pty_pid` in
  `blit-server`). An LSP child therefore leaves no parked status to
  collide with a later PTY child that recycles its pid. Windows needs
  kill-on-drop job objects — the one platform shim.
- **Non-blocking child I/O.** Stdin writes go through a dedicated writer
  thread fed by a channel, so a language server that stops draining
  stdin blocks only that thread — the engine loop keeps expiring
  queries, honoring `LSP_STOP`, and restarting on the init timeout,
  which kills the wedged child.
- **The quirk matrix is the product.** Terminating LSP means every
  server's spec deviation — open-doc-only diagnostics, encoding
  preferences, dynamic-registration timing, nonstandard progress — is
  blit's bug, per server, forever. The mitigations are structural: a
  per-server adapter table in `blit-lsp`, and a scripted fake-LSP-server
  harness so quirk handling is tested deterministically, not against
  whatever rust-analyzer does today. The small projected surface keeps
  the tax bounded; it never disappears.

Platform story: full parity. Pipes, spawn, and the `notify` watcher
work on Linux/macOS/Windows; language servers are cross-platform
binaries the user already has; nothing touches the compositor.

## Alternatives

|                       | raw tunnel (byte channels)             | LSP-aware passthrough                 | projection (this)              |
| --------------------- | -------------------------------------- | ------------------------------------- | ------------------------------ |
| Wire payload          | raw JSON-RPC                           | LSP JSON in blit frames               | blit records                   |
| Client carries        | full LSP client (×2: TS and Rust)      | JSON-RPC + UTF-16 math + URI building | apply records, ack             |
| Sharing               | by convention; one raw attach corrupts | id rewriting, capability intersection | N=1 by construction            |
| One-shot `lsp diag`   | missed-forever notifications           | cache replay works                    | cache replay, `FULL`           |
| Positions             | client's problem                       | UTF-16 on the wire                    | UTF-8 bytes, server transcodes |
| LSP spec churn lands  | in every client                        | in the wire contract                  | in the server engine           |
| Non-LSP backend later | impossible                             | must forge LSP JSON                   | invisible                      |

The tunnel re-runs the documented multiplexer graveyard and breaks the
single most valuable primitive (diagnostics for a client that was not
attached at publish time). The passthrough terminates the right session
layer but makes LSP JSON the wire contract on a protocol whose identity
is no-JSON, and taxes the CLI with UTF-16. Both were rejected; the
passthrough's diagnostics cache and hash-correlation ideas were kept.

## Relation to fs and git

Complementary, composing on hashes and roots: an agent or pane fs-syncs
the worktree for bytes, git-watches for decorations, lsp-subscribes for
squiggles; `LOCATION.hash` and `DIAG_FILE.hash` join against fs-sync
content hashes for staleness; root discovery reuses gix; the LSP engine
reuses the fssync shared-root watcher rather than arming a second one.
No family carries another's data.

## Security

Read-only by construction: no message mutates the worktree, applies
edits, or runs repo-defined commands — executables come from the
compiled table or the user's own config, never from repository
contents. The authority model is [fs-watch.md](fs-watch.md)'s: the
server already hands clients a shell, so this family adds
denial-of-service surface, not privilege. Mitigations are the budget
table (including spawn-rate and restart caps against respawn storms),
request validation, prompt teardown of attachments on disconnect, idle
shutdown of backends, and never logging escaped paths or server-supplied
text as trusted.

## Implementation notes

Landed across `crates/remote/src/lsp.rs` (codecs + the `LspStateMirror`
/ `LspDiagMirror` reference reducers), `crates/lsp` (discovery,
supervisor, JSON-RPC client, projection engines, the scripted
fake-server test harness), `crates/server` (dispatch + refusal tests),
`crates/cli/src/lsp.rs` (`blit lsp
def|refs|hover|symbols|diagnostics|rename|wait|list|stop`), and
`js/core/src/lsp.ts` + `openLsp` on `BlitConnection`, with byte
fixtures pinned across both codec implementations. Deviations, all
invisible to the wire contract and upgradable server-side:

- Lazily-located `workspace/symbol` results emit the zero range instead
  of a `workspaceSymbol/resolve` round trip; rust-analyzer, gopls, and
  pyright return full locations, so the gap is narrow.
- Identical concurrent queries are not coalesced onto one upstream
  request — each gets its own, so cancel refcounting is trivially
  correct. Coalescing remains an engine optimization for later.
- `workspace/symbol` routes to the first backend advertising the
  capability rather than merging results across a workspace's backends.
- Dynamic `didChangeWatchedFiles` registrations bump the capability
  epoch but do not narrow the event stream: the engine reports every
  settled change under the root (servers tolerate extra events; glob
  filtering is a wire-invisible refinement).
- Each backend arms its own `notify` watcher; sharing fssync's
  shared-root watcher (§ Relation to fs and git) is deferred until an
  `FS_SYNC` on the same root actually coexists — a full fssync
  reconciler hashes the tree, which a watch-only consumer should not
  pay for.
- A `FULL` diagnostics replay is one complete update regardless of the
  entries budget (fragmentation carries the size); incremental updates
  chunk under the budget with one in flight.
- `rss` is sampled when records are built (attachment snapshot or
  `LSP_SERVERS`), not continuously.

## Rollout

1. `blit-remote`: `lsp` module (opcodes, record codecs, builders,
   `FEATURE_LSP`), TypeScript mirror in `@blit-sh/core`, byte fixtures
   both directions.
2. `blit-lsp`: supervisor (reaper-integrated), discovery table,
   async-lsp client, transcoding, projection engine — tested against
   the scripted fake server (quirks as fixtures), then e2e against real
   rust-analyzer and gopls.
3. Server wiring; CLI: `blit lsp def|refs|hover PATH:LINE:COL`,
   `symbols [QUERY]`, `diagnostics [PATH] [--watch|--wait]`,
   `rename PATH:LINE:COL NAME` (prints the plan), `wait`, `list`,
   `stop` — NDJSON `--json`, exit codes 0/1/2, 1-based positions,
   stderr for warmup progress; learn.md section with the
   `blit lsp wait && blit lsp …` idiom. **Ship, then measure agent
   adoption for a release cycle. If agents do not call it, stop here.**
4. `connection.openLsp(path, opts)` in `@blit-sh/core`: promise
   methods per query kind, `LspMirror` diagnostics map with auto-ack,
   `openRepo`'s shape exactly.
5. Next query kinds (implementation, call hierarchy) as adoption
   warrants.

Gated on the mutation/buffer RFC: buffer-sourced `didOpen`, applying
workspace edits, code actions, formatting. Gated on a browser editor
existing: completion, semantic tokens, inlay hints.
