# RFC: Units — supervised sessions with lifecycle policy

- **Status:** Draft. Nothing implemented.
- **Date:** 2026-08-04
- **Companion to:** [../protocol.md](../protocol.md),
  [../server.md](../server.md), [net.md](net.md), [kv.md](kv.md)

## Summary

A **unit** is a named, declarative supervisor for a blit session: a
file on disk that says what to run, when to start it, when to restart
it, when it is ready, when it is healthy, and what it depends on —
where a dependency can be "after that unit is answering its health
check", not merely "after that process exists". The
model is deliberately systemd's, down to the key names, because that
expressiveness is well understood and the vocabulary transfers.

A unit introduces **no new stream object**. `C2S_RESTART` already
respawns an exited child in place, reusing the same pty id and the
same terminal driver, so blit already has a session identity that
outlives a process. A unit is that identity made declarative, and a
client subscribes once and follows it across restarts with continuous
scrollback — the blit-native equivalent of a journal.

It does need a small registry of its own, because a unit exists while
inactive, blocked, or failed to load, and with `RemainAfterExit=yes`
after its process is gone — none of which have a PTY. That registry
points at the current PTY rather than duplicating it, so the PTY
remains the only terminal object and there is no second process
model bolted alongside the PTY map. See
[Identity and generations](#identity-and-generations).

Units sit on three primitives that are worth having on their own, and
which fix real bugs in today's PTY layer:

- **Deadlines.** Every timeout in blit is client-side today, so a hung
  command outlives a disconnected orchestrator forever.
- **Group kill.** `C2S_KILL` signals the leader pid only, and
  `C2S_CLOSE`'s SIGHUP misses anything that has changed process group.
- **GC.** Nothing frees an exited PTY slot, and `max_ptys` is
  hardcoded unlimited, so one session per tool call leaks without an
  explicit `C2S_CLOSE`.

Those ship first, as three small changes. Units then consume them.

## What exists today

Verified against `dc6a265`.

| Fact                                                                                                                                                   | Evidence                                      |
| ------------------------------------------------------------------------------------------------------------------------------------------------------ | --------------------------------------------- |
| PTY sessions live in one flat `HashMap<u16, Pty>` on a single global `Session` (in blit, "session" means the whole server)                             | `crates/server/src/lib.rs:1587`               |
| `Pty` carries no creation time, no owner client, no attached-client count, no TTL, no deadline                                                         | `crates/server/src/lib.rs:351-384`            |
| Children are `setsid()` + `TIOCSCTTY` session leaders, so `child_pid == pgid == sid`                                                                   | `crates/server/src/pty/pty_unix.rs:445-446`   |
| `C2S_KILL` calls `libc::kill(child_pid, sig)` with a positive pid                                                                                      | `lib.rs:8872`, `pty/pty_unix.rs:233`          |
| `C2S_CLOSE` calls `libc::kill(child_pid, SIGHUP)` then `close(master_fd)`                                                                              | `pty/pty_unix.rs:239-244`                     |
| The code already knows how to pgrp-signal, but only for SIGWINCH                                                                                       | `pty/pty_unix.rs:224-230`                     |
| Exit is detected by EOF on the pty master, not SIGCHLD; `reap_zombies` is a 5 s backstop                                                               | `pty/pty_unix.rs:369`, `lib.rs:4532`, `:2614` |
| `cleanup_pty_internal` sets `exited: true` and keeps the entry                                                                                         | `lib.rs:2477-2491`                            |
| The only `ptys.remove` in the whole server is inside the `C2S_CLOSE` arm                                                                               | `lib.rs:8883`                                 |
| `max_ptys: 0` (unlimited), hardcoded, with no flag and no env var                                                                                      | `crates/cli/src/main.rs:802`                  |
| Hitting `max_ptys` replies with nothing — a bare `continue`, so a nonce-bearing create hangs forever. There is no error opcode in the protocol at all. | `lib.rs:7815-7817` and three sibling sites    |
| The tick loop's `next_deadline` is `None` when no client is connected; it then sleeps purely on `delivery_notify`                                      | `lib.rs:2731`, `lib.rs:4903-4911`             |
| The only unconditional periodic timer is the 5 s `reap_zombies` task, which captures no state                                                          | `lib.rs:2614-2619`                            |
| `C2S_RESTART` respawns an exited child in place, reusing the pty id and driver                                                                         | `lib.rs:8683`, `pty/pty_unix.rs:570`          |

Existing machinery this design reuses rather than reinvents:

- `crates/server/src/audio.rs:608-690` — a heal loop with a
  minimum-interval rate limit, a burst limiter over a window, and a
  hard give-up. Already the shape of a restart policy.
- `crates/cli/src/uplink.rs:54-104` — exponential backoff with jitter,
  reset on success.
- `crates/sd-notify/` — a pure-`libc` `sd_notify(3)` **client**. blit
  already speaks the readiness protocol; it simply speaks the wrong
  half of it.
- `regex = "1"` is already a direct dependency of `blit-server`
  (`crates/server/Cargo.toml:35`), so `ReadyMatch=` costs no new
  dependency.
- `crates/webserver/src/config.rs:632` — the live-reload file watcher
  behind `blit.remotes`.

## Constraints

- **The wire is version-stable.** `PROTOCOL_VERSION = 1` is frozen: the
  JS client hard-closes the transport on `version > 1`
  (`js/core/src/BlitConnection.ts:2909`). Compatibility rides on new
  opcodes plus a `FEATURE_*` bit, or on append-only trailing fields
  that are length-gated on parse (`crates/remote/src/lib.rs:1598` is
  the canonical example).
- **Unknown opcodes are silently dropped in both directions**
  (`lib.rs:8897`, `BlitConnection.ts:3663`). A new client cannot
  distinguish "old server" from "processed", so every capability here
  is feature-gated, and anything nonce-bearing needs an explicit
  refusal path — the `refuse_lsp_message` pattern at
  `lib.rs:7431-7441`, with the function itself at `lib.rs:6928`.
- **Flat crate layout** ([../../CONTRIBUTING.md](../../CONTRIBUTING.md)):
  one new `crates/server/src/units.rs`, `mod`'d from the root.
- **The server is the stateful half**, and everything above the socket
  is stateless and restartable ([../../ARCHITECTURE.md](../../ARCHITECTURE.md)).
  Units belong in the server; nothing else is alive at boot to
  autostart them.
- **New families take the next `0x?0` opcode block** — 0x40 fs, 0x50
  git, 0x60 lsp, 0x70 kv, 0x80 net, so 0x90 here.
- **There is no `serde`, `toml`, or `ini` crate anywhere in the
  workspace.** The INI parser is roughly sixty hand-rolled lines,
  which is consistent with a repo that hand-rolls `openpty`, `fork`,
  and `sd_notify`.

## Primitives

### Group kill and escalation

Every blit child is a `setsid()` leader, so `kill(-pid, sig)` is
already valid and needs no new bookkeeping.

| `KillMode=`               | Behavior                                                                                        |
| ------------------------- | ----------------------------------------------------------------------------------------------- |
| `process`                 | `kill(pid, sig)` — today's behavior                                                             |
| `process-group` (default) | `kill(-pid, sig)`, plus `TIOCGPGRP` then `kill(-fg_pgid, sig)` when the foreground pgrp differs |
| `control-group`           | Linux only, opt-in: the child goes in a delegated cgroup v2 and is killed with `cgroup.kill`    |

The limit is worth stating plainly: `process-group` catches everything
except a process that has deliberately `setsid`'d itself into a new
session. Only a cgroup — or `PR_SET_CHILD_SUBREAPER` plus a descendant
walk — catches that, and both are Linux-only, so `control-group` stays
opt-in. blit is an unprivileged userspace multiplexer that also runs
on macOS and Windows.

Making `process-group` the default changes behavior for every existing
client. That is the fix, and it is intentional.

`C2S_CLOSE` gains the same escalation: SIGHUP to the group, wait
`TimeoutStopSec`, then SIGKILL to the group. Today `C2S_CLOSE` closes
the master fd immediately, which detaches the reader thread and leans
on the kernel hangup; a runaway grandchild holding the slave open
means no EOF and no `exited` transition. The timed second half of that
escalation depends on the supervisor loop below, so it lands with it.

**Wire:** append an optional `[flags:1]` to `C2S_KILL`. The arm is
`data.len() >= 7`, so old clients are unaffected and inherit the new
default.

### The supervisor loop

A hung command outliving a disconnected orchestrator is precisely the
case where today's tick loop stops scheduling. `blanket_frame_interval`
returns `None` when no client is connected (`lib.rs:2731`), so
`next_deadline` is `None` and the loop sleeps purely on
`delivery_notify`. A silent runaway produces no output and is
therefore never visited on any schedule.

That is an argument for a second loop, not for polling. The supervisor
is **fully reactive**, shaped exactly like the delivery loop at
`lib.rs:2597-2612`: a `tokio::select!` between `supervisor_notify` and
`sleep_until(next_deadline)`, where `next_deadline` is the minimum
over every armed timer and `None` means sleep indefinitely. It differs
from the delivery loop only in what it schedules — policy rather than
frame pacing — and in never returning `None` merely because no client
is watching.

Every timer here is a computable instant, so there is nothing to poll
for: deadline expiry, lease grace, restart backoff, `TimeoutStartSec`,
`TimeoutStopSec`, the next health probe, watchdog expiry, and
`exited-linger`. Arming or disarming any of them notifies the loop,
which recomputes the minimum. An idle server with no armed timers
wakes zero times, which is the property `lib.rs:4903-4907` is
protecting for the delivery loop and worth preserving here.

The one genuinely unreactive thing in the server today is child
death. There is no SIGCHLD handler anywhere; exit is inferred from
EOF on the pty master and backstopped by `reap_zombies` polling
`waitpid` every 5 s (`lib.rs:2614-2619`). So the supervisor adds a
`SignalKind::child()` handler — the same shape as the existing
SIGTERM/SIGINT handler at `lib.rs:2653-2662`, and `tokio` is already
built with `features = ["full"]`.

**The handler must not simply call today's `reap_zombies` more often.**
That function drains `waitpid(-1, WNOHANG)` in a loop and _discards_
the status of any pid not in `pty_pids()` (`pty_unix.rs:266-284`).
That discard is deliberate — a foreign child such as an LSP backend is
reaped by its own engine — but it means the drain steals statuses from
anything the server spawns through `Command` and waits on itself. The
audio pipeline already lives with that race at a 5 s cadence; running
the drain on every SIGCHLD would widen it sharply, and this RFC then
adds periodic `ExecHealthCheck=` children directly into its path. That
is a bug this design would have introduced.

So the signal handler only **wakes the supervisor**, which reaps
by targeted `waitpid(pid, WNOHANG)` over the pids it owns — PTY
children plus the helper children of `ExecStartPre=`, `ExecStop=`, and
`ExecHealthCheck=`, all registered through the existing
`register_pty_pid`/`pty_pids()` mechanism (`pty_unix.rs:545`,
`:296-304`) generalized to owned children. **The global
`waitpid(-1)` drain is deleted rather than rescheduled.** Children the
server spawns through `Command` keep being reaped by their own owners,
which is what `std`/`tokio` do anyway, and no status is ever collected
by a party that did not spawn it.

Windows keeps a poll: it has no SIGCHLD, and `reap_zombies` is already
a no-op there (`pty/pty_windows.rs:101`).

### Deadlines and leases

Four independent things can bound a session, and an earlier draft
claimed they "all resolve to one enforcement path" as though that
settled the interaction. It does not: with a single deadline field, a
lease disconnect would clobber an explicit `C2S_DEADLINE`, a reclaim
would clear a `RuntimeMaxSec` it never set, and a stale disconnect
could rearm a deadline on a session a newer connection had already
reclaimed.

So the model is **independent causes with an enforced minimum**:

```text
effective = min(explicit, lease[current_epoch], runtime_max, stop_escalation)
```

Each cause is armed and cleared only by itself. One enforcement path,
several constraints — which is what the original sentence should have
said.

The causes:

- **`C2S_DEADLINE [0x1D][pty_id:2][ms:4]`** — arm, refresh, or clear
  the _explicit_ cause (`ms = 0`). Refreshable, so it doubles as a
  dead-man switch: re-arm every 30 s and the session dies roughly 30 s
  after the orchestrator does.
- **`CREATE2_HAS_DEADLINE`**, a flag plus trailing `[ms:4]` on
  `C2S_CREATE2`. Closes the create-then-arm window in which an
  orchestrator can die leaking exactly the session it was about to
  protect.
- **`RuntimeMaxSec=`** — the unit-file cause.
- **Stop escalation** — the `TimeoutStopSec` timer between SIGTERM and
  SIGKILL.

#### Leases

`C2S_LEASE [0x1E][flags:1][grace_ms:4][lease_id:8]` →
`S2C_LEASE [lease_id:8][epoch:4]`. Sessions created while a connection
holds a lease are tagged with it. On disconnect each live leased
session gets a `grace_ms` deadline **in the lease cause only**; a
reclaim clears that cause and nothing else.

Ownership needs to be explicit, because a bare refreshable id has
none:

- **The server mints `lease_id`**, unguessably, and the client stores
  it. A client-chosen `u64` is a namespace anyone on the socket can
  collide with or guess, and the lease is a kill switch for other
  people's sessions.
- **A lease has an `epoch`**, incremented on every successful reclaim.
  A disconnect only arms the lease deadline if its epoch is still
  current, so an old connection dying after a newer one reclaimed
  cannot revoke the newer claim. This is the race that makes reconnect
  ownership more than bookkeeping.
- **One holder at a time.** A reclaim by a second connection
  supersedes the first, which is thereafter epoch-stale; there is no
  shared ownership and no holder count to get wrong.

#### Escalation and attribution

On expiry: SIGTERM to the group, wait `TimeoutStopSec` (default 5 s —
systemd's 90 s is wrong for agent workloads), then SIGKILL to the
group.

Append `[reason:1]` to `S2C_EXITED`
(`0=normal, 1=deadline, 2=lease, 3=gc, 4=unit-stop`). A deadline kill
currently arrives as `-9`, indistinguishable from a user kill. When
several causes expire together the reason is **the cause that produced
the minimum**, ties broken in the order listed above, so attribution
is deterministic rather than whichever timer the loop happened to see
first. The field is append-only and length-gated, the same trick the
boot generation used in `S2C_HELLO`.

### GC and `max_ptys`

There are two distinct leaks.

**Exited slots.** Add `exited_at: Instant` to `Pty` and reap in the
supervisor loop under two independent bounds: `max-exited`, a count
(default 1024, oldest evicted first), and `exited-linger`, a time
(default off). Eviction runs the same path as `C2S_CLOSE` and
broadcasts `S2C_CLOSED`, which every client already handles, so no
client change is required.

A count cap alone is the conservative default. Consumers routinely
create one session per tool call and read output back well after exit,
so any short linger silently breaks them, whereas a generous count cap
turns an unbounded leak into a bounded one without changing observable
behavior.

**Live but abandoned sessions** are deliberately not GC'd on a timer.
Detaching and coming back is the point of a multiplexer. Deadlines and
leases are the opt-in tools, and `max_ptys` is the backstop.

`max_ptys` itself gets a real default plus `BLIT_MAX_PTYS`, counting
only live sessions. The silent `continue` has to go with it: a new
`S2C_CREATE_FAILED [nonce:2][reason:1]` means a nonce-bearing client
gets an answer instead of hanging forever. That is a standalone bug,
worth fixing whether or not units happen.

**Unit sessions do not count against `max_ptys`.** The cap exists to
bound session creation by clients, which is where the unbounded leak
lives; unit sessions are bounded by the number of files an operator
put on disk. Counting them together would mean a cap tight enough to
be useful could be exhausted during autostart, failing units at boot
for a reason that has nothing to do with them — and worse,
nondeterministically, depending on how many clients happened to be
connected. Units get their own `max-units` instead, which exists only
to bound a pathological config directory.

## Units

### Identity and generations

A unit is **not** only policy attached to a `Pty`. An earlier draft
said it was, and that was wrong: a unit exists while inactive, while
blocked on a dependency, while failed to load, and — with
`RemainAfterExit=yes` — after its process is gone. None of those have
a PTY. Reload also has to retain definition and runtime state across
process generations. So there is a small explicit registry:

```rust
struct UnitRuntime {
    name: UnitName,               // validated, unique, server-owned
    definition: UnitDefinition,
    state: UnitState,
    current: Option<(PtyId, Generation)>,
    restarts: StartLimiter,
}
```

What survives from the original framing is narrower and still true:
**the PTY remains the only stream object.** The registry points at the
current PTY; it does not duplicate the terminal, the scrollback, or
the driver. This is orchestration state, not a second process model.

`Pty.tag` is not identity — tags are client-chosen, optional, and not
checked for uniqueness at creation (`lib.rs:7777-7817`), so any client
could `C2S_CREATE` a session tagged `api`. Unit names are validated
and unique in the registry, and a unit-owned PTY additionally carries
`unit: Option<UnitName>` set only by the supervisor and never settable
over the wire. A client PTY whose tag collides with a unit name is
left completely alone: it is not adopted, not refused, and never
appears in `S2C_UNIT_LIST`. The tag stays the cosmetic label it is
today.

#### Generations

Every unit-owned PTY carries a `Generation` that increments on each
restart, and **every asynchronous event carries `(pty_id, generation)`**.

This is load-bearing once restarts can be triggered by SIGCHLD rather
than by EOF. In-place respawn is safe today only because it happens
after the PTY has already reached the EOF/exited path, so there is no
live reader and no open master. Restarting on leader death removes
that precondition: the old reader thread can still be alive, old
descendants can still be writing, and `respawn_child` would open a new
master and start a second reader against the same pty id. Late output
or a late EOF from the retired reader would then mutate the new
generation's state.

So a restart is an ordered sequence, not a call:

1. Record the leader's exit status.
2. Terminate the remaining old process group (`KillMode=`, then the
   `TimeoutStopSec` escalation).
3. Close the old master fd.
4. Retire the old reader and join it.
5. Increment the generation.
6. Spawn the replacement.
7. Drop any event whose generation is not current.

Step 7 is the cheap backstop that makes the rest safe to get slightly
wrong.

### File format

Unit files live in `~/.config/blit/units/<name>.unit`, with a system
directory at `/etc/blit/units/` that the user directory shadows by
name. The format is INI with sections, using systemd's key names
wherever a systemd concept exists.

Going halfway — INI sections with blit-invented key names — would be
the worst of both, so the rule is: if systemd has the concept, use its
exact spelling. Where systemd has no analog, stay in its house style
and document the deviation.

**Only the vocabulary transfers, not systemd's parser.** Pointing at
`man systemd.service` as the reference, as an earlier draft did, is
not implementable: systemd's unit syntax carries specifiers (`%i`,
`%h`), `ExecStart=` prefixes (`-`, `@`, `+`, `!`), line continuations,
its own quoting and escaping rules, drop-in merge order, and
`EnvironmentFile=` expansion. Implementing a subset of that in sixty
hand-rolled lines while claiming compatibility is how a file format
that executes code acquires surprising behavior.

So the grammar is small, explicit, and deliberately stricter than
systemd's:

| Question                 | Answer                                                                                                                                   |
| ------------------------ | ---------------------------------------------------------------------------------------------------------------------------------------- |
| Line forms               | `[Section]`, `Key=Value`, `# comment`, blank. Nothing else.                                                                              |
| Continuations            | Not supported.                                                                                                                           |
| Inline comments          | Not supported — `#` only at the start of a line, so values need no escaping.                                                             |
| Value                    | Everything after the first `=`, trimmed of surrounding whitespace.                                                                       |
| Quoting / escaping       | None.                                                                                                                                    |
| `ExecStart=` and friends | `argv` split on unquoted whitespace, `execve`'d directly. **Never `/bin/sh -c`.** No globbing, no expansion, no prefixes.                |
| Specifiers (`%i`, `%h`)  | Not supported.                                                                                                                           |
| `Environment=`           | Repeatable, one `NAME=VALUE` each, no expansion. No `EnvironmentFile=`.                                                                  |
| Repeated scalar keys     | **Error**, not last-wins — a duplicate in a file that executes code is a mistake, not an override.                                       |
| Repeated list keys       | `Requires=`, `Wants=`, `After=`, `Environment=`, `ExecStartPre=` append.                                                                 |
| Dependency lists         | Whitespace-separated within one line, and repeatable across lines.                                                                       |
| Durations                | Bare integers are seconds; `ms`, `s`, `m`, `h` suffixes accepted; `infinity` accepted where a timeout is meaningful; `0` means disabled. |
| Booleans                 | `yes`/`no` only.                                                                                                                         |
| Unknown section or key   | **Error.** Not a warning.                                                                                                                |
| Malformed line           | Error, reported with file and line number.                                                                                               |
| Duplicate section        | Error.                                                                                                                                   |

Strict beats permissive here in every case: a unit file is executable
persistence, an error is visible at load, and a silently ignored
misspelled `Restart=` is a unit that never restarts and nobody
notices. It also means the grammar can be relaxed later, whereas
permissive parsing cannot be tightened.

```ini
# ~/.config/blit/units/example.unit
[Unit]
Description=Every supported key, for reference
Requires=postgres
After=postgres

[Service]
Type=notify              # simple | oneshot | notify | match | surface
ReadyMatch=^Listening on # Type=match only
ReadySurface=chromium    # Type=surface only
RemainAfterExit=no       # Type=oneshot only
ExecStartPre=/usr/bin/mkdir -p /var/run/api
ExecStart=cargo run -p api
ExecStop=/usr/bin/api-ctl drain
WorkingDirectory=/home/pierre/src/api
Environment=RUST_LOG=info
Backing=pty              # pty | pipe

Restart=on-failure       # no | on-failure | always
RestartSec=1
RestartMaxSec=30
StartLimitBurst=5
StartLimitIntervalSec=60

TimeoutStartSec=30
TimeoutStopSec=5
RuntimeMaxSec=0
WatchdogSec=30
KillMode=process-group

ExecHealthCheck=/usr/bin/curl -fsS localhost:8080/health
HealthCheckSec=10
HealthCheckTimeoutSec=3
ActiveWhenHealthy=yes    # default when ExecHealthCheck= is set

[Install]
Autostart=yes
```

Four deviations from systemd, each deliberate:

- `[Install] Autostart=` replaces `WantedBy=` plus `systemctl enable`
  symlinks into `.wants/` directories. Targets and symlink farms are
  the heaviest part of systemd's model and buy nothing without a boot
  ordering graph. Presence in the directory plus one boolean is the
  whole enable/disable story.
- `ExecHealthCheck=`, `HealthCheckSec=`, `HealthCheckTimeoutSec=`,
  and `ActiveWhenHealthy=` are new: systemd has no health-check
  concept at all, only `WatchdogSec`.
- `Type=match` with `ReadyMatch=` has no systemd analog.
- `Backing=` and `RestartMaxSec=` are blit-specific.

Unit files are live-reloaded by the same watcher machinery as
`blit.remotes`, so `blit unit reload` is a courtesy rather than a
requirement.

### State machine

```
inactive ──start──> activating ──ready+healthy──> active
   ^                    │                            │
   │                    │ fail                       │ exit / stop / health fail
   │                    v                            v
   └──stop/GC──── failed <──limit──── deactivating ──┘
```

The diagram is a summary; the contract is the table. Every transition
is `(state, event) → (state, actions)`, and the whole thing is a pure
function (see [Testability](#testability)).

| State          | Event                                           | Next                  | Actions                                              |
| -------------- | ----------------------------------------------- | --------------------- | ---------------------------------------------------- |
| `inactive`     | start, deps satisfied                           | `activating`          | spawn generation N, arm `TimeoutStartSec`            |
| `inactive`     | start, dep failed (`Requires=`)                 | `failed`              | —                                                    |
| `activating`   | spawn failed                                    | `failed`              | apply `Restart=`                                     |
| `activating`   | ready probe fired, no health gate               | `active`              | disarm `TimeoutStartSec`, arm `RuntimeMaxSec`        |
| `activating`   | ready probe fired, health gating                | `activating`          | run probe now                                        |
| `activating`   | first health probe passed                       | `active`              | disarm `TimeoutStartSec`, arm health interval        |
| `activating`   | health probe failed                             | `activating`          | retry until `TimeoutStartSec`                        |
| `activating`   | `TimeoutStartSec` expired                       | `failed`              | stop sequence, apply `Restart=`                      |
| `activating`   | child exited, `Type=oneshot`, rc=0              | `active`              | if `RemainAfterExit=no` → `inactive`                 |
| `activating`   | child exited, other                             | `failed`              | apply `Restart=`                                     |
| `active`       | child exited                                    | `failed` / `inactive` | by `Restart=` and exit status                        |
| `active`       | health failures ≥ `HealthCheckFailureThreshold` | `failed`              | stop sequence, apply `Restart=`                      |
| `active`       | watchdog expired                                | `failed`              | stop sequence, apply `Restart=`                      |
| `active`       | `RuntimeMaxSec` / deadline expired              | `deactivating`        | stop sequence, reason `deadline`                     |
| `active`       | explicit stop                                   | `deactivating`        | `ExecStop=`, then escalation; record operator intent |
| `active`       | dependency stopped (`Requires=`)                | `deactivating`        | stop sequence                                        |
| `deactivating` | stop completed                                  | `inactive`            | retire generation                                    |
| `deactivating` | `TimeoutStopSec` expired                        | `inactive`            | SIGKILL group, retire generation                     |
| any            | restart scheduled, backoff elapsed              | `activating`          | spawn generation N+1                                 |
| any            | start limit exhausted                           | `failed`              | stop scheduling restarts                             |
| any            | event from stale generation                     | unchanged             | **drop**                                             |
| any            | definition removed on reload                    | `deactivating`        | stop sequence, drop from registry when `inactive`    |

`Restart=` is evaluated identically wherever it appears in the
Actions column: `no` never restarts, `on-failure` restarts on a
non-zero or signalled exit and on any `failed` entry, `always`
restarts on every exit. Backoff is `RestartSec` doubling with jitter
up to `RestartMaxSec`; `StartLimitBurst` over `StartLimitIntervalSec`
exhausts the limiter. That is `audio.rs:650`'s burst limiter and
`uplink.rs:60-104`'s backoff, generalized.

Restarts respawn in place through `respawn_child`, keeping the pty id
and the driver, under the generation discipline above. A restarting
unit is not a new session.

### Readiness

`Type=` selects the probe, and carries systemd's meaning.

- `simple` — active as soon as the child is spawned. systemd's
  default; usually a lie, but the honest default when the program
  tells you nothing.
- `oneshot` — the unit is a command that runs to completion, not a
  daemon. It reaches `active` when the child exits successfully, and
  `RemainAfterExit=yes` keeps it `active` afterwards instead of
  returning it to `inactive`. `Restart=` is rejected for `oneshot` in
  anything but `no`. This is what makes "run this once, but only after
  its dependencies are genuinely up" expressible at all, and it is
  load-bearing for the worked example below.
- `notify` — blit sets `NOTIFY_SOCKET` in `build_child_env`, which
  already injects `BLIT_SOCK`, `WAYLAND_DISPLAY`, and `PULSE_SERVER`,
  and listens on an `AF_UNIX SOCK_DGRAM` socket for `READY=1`,
  `STATUS=`, and `WATCHDOG=1`. This is the strongest fit in the whole
  design: `crates/sd-notify/` already implements the client half in
  pure `libc`, and the listener is that same code mirrored, so blit
  ends up both speaking and understanding `sd_notify(3)`. Unix only;
  Windows falls back to `simple`.
- `match` — `ReadyMatch=<regex>` against the child's output stream.
  This is cheap only in blit, because the server is already handling
  every byte of that stream and already depends on `regex`. It is the
  pragmatic probe for the overwhelming majority of programs that will
  never call `sd_notify`, and arguably the most blit-native of the
  three.

  Which stream, precisely, since "output" is ambiguous in a terminal
  multiplexer: the **raw bytes off the reader thread, before terminal
  interpretation**, not the rendered grid. Matching the grid would
  make a redrawing progress bar nondeterministic — a line can be
  overwritten between two scans and never be observed. The raw stream
  is split on both `\n` and `\r`, since progress output updates a line
  with a bare carriage return, and the pattern is applied per segment.
  A segment is capped (16 KiB) and the accumulator is dropped at each
  boundary, so a program that never emits a newline cannot grow the
  buffer without bound. Scanning stops at readiness or
  `TimeoutStartSec`, whichever comes first, so a unit that never
  matches costs nothing after its start window.

- `surface` — `ReadySurface=<app_id pattern>`: ready when a matching
  Wayland surface exists in the compositor. For a GUI unit this is the
  only honest definition of started, since a browser process exists
  long before it has a window. `app_id` is already tracked per surface
  and already shipped on `S2C_SURFACE_CREATED`, so this needs no
  compositor change.

  The limitation is that matching is by `app_id`, not by process: two
  units running the same binary cannot be told apart. Surfaces are
  keyed by Wayland object id and the compositor does not record client
  credentials today (the `pid` bindings in
  `crates/compositor/src/imp.rs` are protocol ids, not process ids), so
  precise attribution means plumbing `SO_PEERCRED` from the Wayland
  client socket through to `Surface`. Worth doing if one-`app_id`-per-
  unit turns out to be too coarse; not worth doing pre-emptively.

`TimeoutStartSec` elapsing without readiness moves the unit to
`failed`, and `Restart=` applies.

### Health

Readiness is a one-shot edge; health is a continuous property.

`Type=` keeps its systemd meaning exactly. Health participates in
activation through a separate key, `ActiveWhenHealthy=`, which
**defaults to `yes` whenever `ExecHealthCheck=` is set** and is
otherwise inert: with it, a unit reaches `active` only once its
readiness probe has fired _and_ its first health check has passed,
the whole window bounded by `TimeoutStartSec`. Configure no health
check and every `Type=` behaves precisely as systemd documents it;
configure one and it obviously gates, which is the behavior almost
everyone wants from `After=`. `ActiveWhenHealthy=no` opts back out
for a probe that is pure monitoring.

Putting this behind its own key rather than folding it into `Type=`
matters more than it looks: the whole argument for borrowing systemd's
spellings is that muscle memory transfers, and silently redefining
`Type=simple` would break exactly that promise.

- **Watchdog.** For `Type=notify`, a missing `WATCHDOG=1` within
  `WatchdogSec` marks the unit failed and applies `Restart=`. It falls
  out for free once the notify listener exists, and blit sets
  `WATCHDOG_USEC` in the child environment exactly as systemd does.
- **`ExecHealthCheck=`** is spawned every `HealthCheckSec` and killed
  after `HealthCheckTimeoutSec`. Probes never overlap: one still
  running when the next is due is skipped, not stacked.

  Failure is **thresholded, not immediate**:
  `HealthCheckFailureThreshold` (default 3) consecutive failures fail
  the unit, and `HealthCheckSuccessThreshold` (default 1) consecutive
  successes recover it. Restarting a service because one `curl` lost a
  race is worse than the problem being solved, and every practical
  health-checking system landed on consecutive-count thresholds for
  the same reason.

**`After=` is an initial gate, not a maintained invariant.** It waits
for the dependency's first `active`, which with health gating means
its first passing probe. It does not keep the dependent stopped while
the dependency is unhealthy later, and `Requires=` propagates a
_stop_, not a health transition. Worth stating plainly because the
headline — "wait until the server passes its health check" — reads
like a continuous guarantee and is not one.

#### Helper children

`ExecStartPre=`, `ExecStop=`, and `ExecHealthCheck=` are processes the
server spawns that are **not** sessions, and they need their own
accounting rather than riding the PTY machinery.

They are spawned through the `Command` and `Stdio::piped` path the
audio pipeline already uses (`audio.rs:345` onward), not the PTY
spawner, each with `setpgid(0, 0)` so its own timeout can group-kill
it — a health probe that shells out must not leave the shell's
children behind. They are tracked in a `helpers: HashMap<pid, …>`
separate from `ptys`.

This is a hazard the SIGCHLD handler creates rather than solves: the
handler sees every child, so it must dispatch on pid. There is already
a registry for exactly this problem — `register_pty_pid` /
`pty_pids()` (`pty_unix.rs:545`, `:296-304`) exists so the backstop
reaper cannot mis-collect a foreign child's status — so helpers get
the same treatment: PTY pid, helper pid, or unknown-and-discarded.

`ExecStartPre=` runs to completion before `ExecStart=` and a non-zero
exit fails activation. `ExecStop=` runs before the SIGTERM escalation;
if it returns zero and the main process is gone, the unit stops
cleanly, and otherwise the escalation proceeds as normal, so a hung
`ExecStop=` cannot wedge a stop.

### Dependencies

systemd's separation of _requirement_ from _ordering_ is kept, because
collapsing the two is the thing homegrown supervisors reliably get
wrong.

- `Requires=a` pulls `a` in; this unit fails if `a` fails to start,
  **and stops if `a` later stops**. No ordering is implied.
- `Wants=a` pulls `a` in and ignores its outcome, then and later.
- `After=a` is ordering only: do not start until `a` reaches `active`.

`Requires=` carries systemd's full meaning here, including the
propagation half, because taking the spelling and quietly dropping
half its semantics is worse than not borrowing the word. A restart of
`a` is not a stop: as in systemd, `a` going through
`active → activating → active` leaves dependents alone, and only a
transition to `inactive` or `failed` that is not part of a restart
propagates.

`active` is doing the real work in the `After=` line. It means ready
by whichever probe `Type=` selects, and — when health gates activation
— first health check passed. So a single keyword covers "after the
process exists", "after it says it is ready", "after it has a window",
"after it answers its health endpoint", and "after that command
finished", depending only on how the dependency describes itself. The
dependent unit does not need to know which.

Cycles are refused at load with a diagnostic naming the cycle, rather
than silently broken by dropping ordering edges. That is systemd's
behavior, but it is confusing, and there is no compatibility
obligation here. Because unit files live-reload, a cycle introduced by
an edit rejects **that file** and keeps the last good graph, rather
than failing the whole set — a typo in one unit must not take down
everything already running.

### Pipe backing

`Backing=pipe` is the same fork with `pipe2()` on stdin, stdout, and
stderr instead of the pty slave, and no `setsid`/`TIOCSCTTY`. The
bytes still feed the alacritty driver, so `S2C_UPDATE`, scrollback,
search, `C2S_COPY_RANGE`, and browser rendering all work unchanged.
The difference is entirely on the child's side: no tty, so no job
control, no SIGWINCH, and tools correctly detect that they are not on
a terminal.

One interaction with `KillMode=` that is easy to miss: a `pipe` child
is not a session leader, so `process-group` is meaningless for it
unless the forked child does an explicit `setpgid(0, 0)`.

### Wire and CLI

A new family in the `0x90` block, gated on `FEATURE_UNITS`, bit 11
(bits 11 through 31 are free).

| Dir | Opcode | Name           | Layout                                                                                    |
| --- | ------ | -------------- | ----------------------------------------------------------------------------------------- |
| C2S | `0x90` | `UNIT_LIST`    | `[nonce:2]`                                                                               |
| C2S | `0x91` | `UNIT_START`   | `[nonce:2][name:N]`                                                                       |
| C2S | `0x92` | `UNIT_STOP`    | `[nonce:2][name:N]`                                                                       |
| C2S | `0x93` | `UNIT_RESTART` | `[nonce:2][name:N]`                                                                       |
| C2S | `0x94` | `UNIT_RELOAD`  | `[nonce:2]`                                                                               |
| S2C | `0x90` | `UNIT_LIST`    | `[nonce:2][count:2]` then per unit: name, state, pty id, restart count, last exit, health |
| S2C | `0x91` | `UNIT_STATE`   | `[name_len:2][name:N][state:1][pty_id:2]` — pushed on every transition                    |

Every request is nonce-bearing, so all of them need the refusal path
described under Constraints when the family is disabled.

On the CLI, `blit unit list|start|stop|restart|status|cat|reload`,
following the existing subcommand pattern, and added to
`crates/cli/src/learn.md` so agents discover it.

### Server restart

Units do not survive a server restart; unit runtime state is in
memory. On restart, autostart re-runs them from their files, and
clients detect the restart through the boot generation in
`S2C_HELLO`. This is coherent and crash-only, and is written down here
so that nobody expects a running unit's state to be durable.

There is exactly one exception, and it is not runtime state but
**operator intent**. `blit unit stop api` followed by a server restart
would otherwise re-autostart `api`, silently contradicting the person
who stopped it, and no amount of re-reading the unit files can
recover that decision — it is not in them. So the two facts that
represent a human decision, "this unit is disabled" and "this unit was
manually stopped", are persisted; everything else is recomputed.

They go in the KV store (`kv.md`), which is host-local, redb-backed
and already durable, under a reserved `blit/units/` prefix. That
implies one small addition to the KV family: **`blit/` becomes a
server-owned prefix**, rejected for client `KV_PUT` with
`KV_STATUS_INVALID` and readable like anything else. Without it any
client could forge operator intent, which is a strictly worse property
than the one being fixed. The prefix is worth reserving regardless of
this RFC, since it is the obvious namespace for any future
server-owned state.

### Support matrix

Most of the primitives are Unix facilities, so the honest summary is
that Windows gets the declarative layer and not the enforcement.

| Capability                               | Unix                 | Windows                                |
| ---------------------------------------- | -------------------- | -------------------------------------- |
| Unit files, autostart, dependencies      | yes                  | yes                                    |
| `Restart=`, backoff, start limits        | yes                  | yes                                    |
| `Type=simple` / `oneshot` / `match`      | yes                  | yes                                    |
| `ExecHealthCheck=`, `ActiveWhenHealthy=` | yes                  | yes                                    |
| Deadlines, leases, GC                    | yes                  | yes                                    |
| `Type=notify`, `WatchdogSec=`            | yes                  | no (`sd_notify` is a Unix socket)      |
| `KillMode=process-group`                 | yes                  | no (falls back to `process`)           |
| `KillMode=control-group`                 | Linux only           | no                                     |
| `Backing=pipe`                           | yes                  | yes (no `setsid`/`setpgid` either way) |
| Reactive child death (SIGCHLD)           | yes                  | no — keeps a poll                      |
| `Type=surface`                           | Linux only (Wayland) | no                                     |

**An unavailable capability is a load error, not a warning.** An
earlier draft had unsupported keys ignored so that a unit directory
shared across machines would not fail wholesale — but that trades a
loud failure for a silent one, and the silent one is worse in exactly
the cases that matter. `Type=notify` degrading to `simple` on Windows
does not merely lose a feature: it breaks the core promise that
`After=` waits for real readiness, and every dependent then starts
early, on a platform where nobody is watching. Same for `KillMode=`
quietly becoming `process`. Portability is achieved by writing
portable units, not by having the server pretend.

### Live reload

Unit files are watched, so reload semantics need to be part of the
contract rather than an emergent property of the watcher.

Reload **parses and validates a complete new registry, then swaps it
atomically**. A file that fails to parse, or an edit that introduces a
dependency cycle, rejects that file and keeps the last good
definition for it; the rest of the swap proceeds. An invalid edit
never partially mutates live policy, and a typo in one unit never
takes down everything already running.

What a swap does to a running unit:

| Change                                                                                                 | Effect                                                                                                            |
| ------------------------------------------------------------------------------------------------------ | ----------------------------------------------------------------------------------------------------------------- |
| `ExecStart=`, `Environment=`, `WorkingDirectory=`, `Backing=`, `Type=`, `ReadyMatch=`, `ReadySurface=` | Recorded; applies at the **next start**. A running process is never silently replaced.                            |
| `Restart=`, `RestartSec=`, `RestartMaxSec=`, `StartLimit*`                                             | Immediate; affects the next restart decision.                                                                     |
| `HealthCheckSec=`, thresholds, `ExecHealthCheck=`                                                      | Immediate; the interval timer is re-armed.                                                                        |
| `RuntimeMaxSec=`                                                                                       | Immediate; re-evaluated against the unit's current start time, so shortening it can expire a unit at once.        |
| `TimeoutStopSec=`, `KillMode=`                                                                         | Immediate; applies to the next stop.                                                                              |
| `Requires=`, `Wants=`, `After=`                                                                        | Immediate for future starts; does **not** retroactively stop a running unit whose new `Requires=` is unsatisfied. |
| `[Install] Autostart=`                                                                                 | Immediate; affects the next server start.                                                                         |
| File removed                                                                                           | Unit is stopped, then dropped from the registry.                                                                  |
| File added                                                                                             | Loaded; started only if `Autostart=yes`.                                                                          |

`blit unit restart <name>` is how a changed `ExecStart=` is applied,
which keeps "reload" purely about definitions and leaves process
lifetime under explicit control.

### Worked example

Open a URL in a browser, but only once the server behind that URL
actually answers and the browser is actually driveable. Three units,
each with a different honest definition of "up".

#### `~/.config/blit/units/api.unit`

```ini
[Unit]
Description=API server

[Service]
Type=simple
ExecStart=cargo run -p api
WorkingDirectory=/home/pierre/src/api
Environment=RUST_LOG=info
Environment=PORT=8080

ExecHealthCheck=/usr/bin/curl -fsS --max-time 2 http://localhost:8080/health
HealthCheckSec=5
HealthCheckTimeoutSec=3

TimeoutStartSec=180
TimeoutStopSec=5
Restart=on-failure
RestartSec=1
RestartMaxSec=30
StartLimitBurst=5
StartLimitIntervalSec=60
KillMode=process-group

[Install]
Autostart=yes
```

`Type=simple` with a health check is the shape most real services
want, and it is the clearest demonstration of health-gated
activation: `simple` on its own would call the unit active the
instant `cargo` is exec'd — before compilation has even started —
but because `ExecHealthCheck=` is set, `ActiveWhenHealthy=` defaults
to `yes` and `api` does not reach `active` until `/health` answers. No separate readiness probe is needed, and
the program needs no cooperation. `TimeoutStartSec=180` has to cover
a cold `cargo build`, and `KillMode=process-group` is what stops
`cargo`'s child compiler and the server binary from surviving a
restart.

Swap in `Type=notify` if the service calls `sd_notify`, or
`Type=match` with `ReadyMatch=^Listening on ` if it only prints. Both
compose with the health check: readiness fires first, the first probe
gates activation, and thereafter the probe is a liveness signal.

#### `~/.config/blit/units/chromium.unit`

```ini
[Unit]
Description=Chromium (Wayland, remote-debugging)

[Service]
Type=surface
ReadySurface=chromium
ExecStart=chromium --ozone-platform=wayland --remote-debugging-port=9222 --user-data-dir=/home/pierre/.cache/blit-chromium --no-first-run --no-default-browser-check
Backing=pipe

ExecHealthCheck=/usr/bin/curl -fsS --max-time 2 http://localhost:9222/json/version
HealthCheckSec=10
HealthCheckTimeoutSec=3

TimeoutStartSec=30
TimeoutStopSec=10
Restart=always
RestartSec=1
RestartMaxSec=15
KillMode=process-group

[Install]
Autostart=yes
```

Two conditions compose here, and both matter for a browser that is
going to be driven rather than watched. `Type=surface` says a window
exists; `ExecHealthCheck=` on the CDP endpoint says it is driveable.
Either alone is a lie — chromium answers CDP before it paints, and a
window can exist while the debugger port is not yet listening — so
`active` means both.

`Backing=pipe` because chromium has no use for a tty, which in turn
makes `KillMode=process-group` depend on the explicit `setpgid(0, 0)`
noted above: a pipe-backed child is not a session leader. That
combination is not incidental. Chromium is the motivating case for
group kill in the first place — it forks a zygote and a renderer per
tab, and today's `kill(child_pid)` leaves every one of them running.

#### `~/.config/blit/units/open-url.unit`

```ini
[Unit]
Description=Open the API in chromium
Requires=api chromium
After=api chromium

[Service]
Type=oneshot
RemainAfterExit=yes
ExecStart=<browser-driver> open http://localhost:8080
TimeoutStartSec=30

[Install]
Autostart=yes
```

Three different definitions of "up" — a passing HTTP health probe, a
Wayland surface plus a live CDP endpoint, and a command that exited
zero — and `open-url` expresses its requirement as
`After=api chromium` without knowing about any of them. That is the
payoff for keeping requirement separate from ordering and for letting
health participate in activation.

Note what `Requires=` adds over `After=` here: if `api` never becomes
healthy within `TimeoutStartSec`, `open-url` fails rather than
hanging, and it never opens a URL that was never going to load.

If a health check starts failing later, that unit leaves `active` and
its `Restart=` applies. `open-url` is not re-run — `Requires=`
propagates a _stop_, not a restart, exactly as in systemd, and
re-running dependents on restart is what `PartOf=` would express (see
[Deferred](#deferred)).

## Security

A unit file is arbitrary code executed at every server start.

An earlier draft proposed mirroring `blit.remotes` — refuse
group- or world-writable files and directories — and called it done.
**That control does not address the attacker it names.** A remote
client writing through the `fs` family writes _as the server's own
user_, producing a file owned by that user with ordinary `0644`
permissions in the user's own unit directory. It passes every mode
check. Mode bits distinguish a different Unix account from this one;
they say nothing about whether the bytes arrived from the person at
the keyboard or from a socket.

The distinction that actually matters is that `C2S_CREATE` is
_ephemeral_ code execution scoped to a connection, while a unit file
is _durable_ code execution that outlives the connection, the client,
and the server process, and re-runs at every boot with no one
attached. Treating those as the same capability because both "run
code" is how a session-scoped compromise becomes a permanent one.

So they are separated:

- **The unit directories are excluded from the `fs` family.** Reads
  and writes under `~/.config/blit/units/` and `/etc/blit/units/` are
  refused, the same way any other path outside a served root is. This
  is the single control that matters, and it is cheap because the fs
  family already has path validation to hang it on.
- **Installing a unit is a separate capability from creating a
  session.** It is a local operation — `blit unit install` writing
  through the filesystem directly, not over the wire — so remote
  access to a blit server never grants durable persistence by
  default. `C2S_UNIT_START`/`STOP`/`RESTART` remain available over the
  wire, because starting an _already-installed_ unit is the operator's
  own policy being exercised, not new code being introduced.
- **`/etc/blit/units/` autostarts by default; the user directory is
  gated.** A system directory is administrator-owned by construction.
  Autostart from `~/.config/blit/units/` requires an explicit server
  option, so the default posture of a blit server exposed to a network
  is that nothing on disk starts itself.
- **Load-time checks remain, as depth rather than as the control:**
  refuse group- or world-writable files and directories, refuse
  symlinked unit files, refuse a unit directory whose parent is
  writable by another user, and resolve and validate the path before
  opening rather than after.

And documented plainly, because a reader will otherwise assume
otherwise: **granting the `fs` family write access to a machine where
unit autostart is enabled for that directory is equivalent to granting
durable code execution**, and no permission bit mitigates it.

## Deferred

Everything here was considered and left out on purpose, rather than
missed.

- **`Before=`, `Conflicts=`, `PartOf=`, `BindsTo=`.** `Requires=`
  plus `After=` covers the cases this is being built for, and each of
  the rest adds a propagation edge that has to be reasoned about
  during live reload. `PartOf=` is the one most likely to be wanted
  next, for "restart the dependents when I restart".
- **Precise `Type=surface` attribution.** Matching is by `app_id`, so
  two units running the same binary cannot be told apart. Fixing it
  means plumbing `SO_PEERCRED` from the Wayland client socket through
  to `Surface`, which is real compositor work for a case that may
  never come up.
- **Drop-in directories** (`<name>.unit.d/*.conf`). Useful for
  overriding a system unit from the user directory, but the
  shadow-by-name rule already covers the common case.
- **Templated units** (`foo@bar.unit`). No motivating use yet.
- **Durable unit runtime state.** Restart counts and last-exit are
  deliberately lost on restart; only operator intent is persisted.
- **`EnvironmentFile=`, specifiers, and `ExecStart=` prefixes.** All
  are systemd syntax the strict grammar deliberately omits; adding any
  of them means owning a compatibility surface, not a vocabulary.
- **Continuous health-based ordering.** `After=` gates on first
  healthy and does not maintain the invariant afterwards.

## Testability

Worth stating as a constraint rather than an afterthought, because
this lands a lot of policy in `blit-server`, which sits at roughly 36%
line coverage — the least-covered crate that anyone actually tests.

Two rules, both following existing precedent:

- **Factor the wire family as a standalone
  `handle_unit_message(data, state, out, verbose)`**, the way fs, git,
  lsp, kv, and net already are. That factoring is exactly why those
  families are testable today: the server test pattern drives a
  handler over an `mpsc` outbox and polls for the expected opcode. An
  arm of the big `match data[0]` inside `handle_client` has no
  equivalent way to be tested.
- **Keep the state machine a pure function**,
  `fn step(state: UnitState, ev: UnitEvent, cfg: &Unit) -> (UnitState, Vec<Action>)`,
  with actions returned as data — `Spawn`, `Signal`, `ArmTimer`,
  `Notify` — rather than performed inline. Every interesting property
  here is then a table test over event sequences: backoff schedules,
  the start limiter tripping, health gating activation, `Requires=`
  propagation, deadline escalation ordering. None of it needs to spawn
  a process, and none of it is timing-dependent, because the clock is
  an input.

## Delivery

One axis per PR.

1. **Group kill.** `KillMode` as an appended flag byte on `C2S_KILL`,
   and `C2S_CLOSE`'s SIGHUP going to the group. Pure semantics, no new
   state.
2. **Supervisor loop, deadlines, leases.** A reactive loop with a
   computed `next_deadline`, plus a SIGCHLD handler replacing the 5 s
   `reap_zombies` poll; `C2S_DEADLINE`, `CREATE2_HAS_DEADLINE`,
   `C2S_LEASE`/`S2C_LEASE`, the `S2C_EXITED` reason byte, and the
   timed half of the `C2S_CLOSE` escalation.
3. **GC.** `exited_at`, the count and time bounds, a real `max_ptys`
   default with `BLIT_MAX_PTYS`, and `S2C_CREATE_FAILED`. Fixes the
   silent-hang bug on its own.
4. **Unit core.** The registry and generations, the strict INI
   parser, the state machine, autostart, restart policy, `Requires=`
   and `After=`, `Type=simple`/`oneshot`/`notify`, start and stop
   timeouts, helper children, the `0x90` wire family, and the CLI.
   PTY backing only.
5. **Unit policy.** Health checks with thresholds and
   `ActiveWhenHealthy=`, `WatchdogSec=`, `Type=match`,
   `Type=surface`, `Backing=pipe`, live reload, and the reserved KV
   prefix for operator intent.

The first three are independently valuable and are the
incident-shaped fixes; they should land and be exercised before the
unit layer commits to timer provenance, reconnect ownership, and
generation handling in production.

4 and 5 were one PR in an earlier draft. Splitting them is the
review's point that the first implementation should have a smaller
race surface, applied without dropping anything: everything still
ships, but the generation and ownership contracts get exercised by a
PTY-only, probe-free core before health probes, a second spawn path,
and live reload are layered onto them. `Backing=pipe` remains the
cleanest internal seam, since it touches only the spawner.
