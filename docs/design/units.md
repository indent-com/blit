# RFC: Units — supervised sessions with lifecycle policy

- **Status:** Draft. Nothing implemented.
- **Date:** 2026-08-04
- **Companion to:** [../protocol.md](../protocol.md),
  [../server.md](../server.md), [net.md](net.md), [kv.md](kv.md)

## Summary

A **unit** is a named, declarative supervisor for a blit session: a
file on disk saying what to run, when to start it, when to restart it,
when it is ready, when it is healthy, and what it depends on — where a
dependency can be "after that unit answers its health check", not just
"after that process exists". The model is systemd's, down to the key
names; only the vocabulary is borrowed, not the parser.

A unit adds **no new stream object**. `C2S_RESTART` already respawns a
child in place, reusing the pty id and the terminal driver, so a
session identity that outlives a process already exists; a unit makes
it declarative, and a client subscribes once and follows a unit across
restarts with continuous scrollback. A unit does need a small registry
of its own, since it exists while inactive, dependency-blocked,
failed-to-load, or `RemainAfterExit=yes` past its process — none of
which have a PTY. The registry points at the current PTY rather than
duplicating it.

Units sit on three primitives that fix existing bugs and ship first:

- **Deadlines.** Every timeout is client-side, so a hung command
  outlives a disconnected orchestrator forever.
- **Group kill.** `C2S_KILL` signals the leader pid only; `C2S_CLOSE`'s
  SIGHUP misses anything that changed process group.
- **GC.** Nothing frees an exited PTY slot and `max_ptys` is hardcoded
  unlimited, so one session per tool call leaks without an explicit
  `C2S_CLOSE`.

## What exists today

Verified against `dc6a265`.

| Fact                                                                                                                                               | Evidence                                     |
| -------------------------------------------------------------------------------------------------------------------------------------------------- | -------------------------------------------- |
| PTY sessions are one flat `HashMap<u16, Pty>` on a single global `Session` — in blit, "session" means the whole server                             | `crates/server/src/lib.rs:1587`              |
| `Pty` carries no owner client, creation time, attached-client count, TTL, or deadline                                                              | `lib.rs:351-384`                             |
| Children are `setsid()` + `TIOCSCTTY` leaders, so `child_pid == pgid == sid`                                                                       | `pty/pty_unix.rs:445-446`                    |
| `C2S_KILL` → `libc::kill(child_pid, sig)`, positive pid                                                                                            | `lib.rs:8872`, `pty_unix.rs:233`             |
| `C2S_CLOSE` → `kill(child_pid, SIGHUP)` then `close(master_fd)`                                                                                    | `pty_unix.rs:239-244`                        |
| The code pgrp-signals already, but only for SIGWINCH                                                                                               | `pty_unix.rs:224-230`                        |
| Exit is inferred from EOF on the pty master, not SIGCHLD; `reap_zombies` is a 5 s backstop that drains `waitpid(-1)` and discards non-PTY statuses | `pty_unix.rs:369`, `:266-284`, `lib.rs:2614` |
| `cleanup_pty_internal` sets `exited: true` and keeps the entry                                                                                     | `lib.rs:2477-2491`                           |
| The only `ptys.remove` in the server is in the `C2S_CLOSE` arm                                                                                     | `lib.rs:8883`                                |
| `max_ptys: 0` (unlimited), hardcoded, no flag, no env var                                                                                          | `crates/cli/src/main.rs:802`                 |
| Hitting the cap is a bare `continue` — no reply, and there is no error opcode in the protocol at all, so a nonce-bearing create hangs forever      | `lib.rs:7815-7817` + 3 sites                 |
| Tags are client-chosen, optional, and not checked for uniqueness at creation                                                                       | `lib.rs:7777-7817`                           |
| The tick loop's `next_deadline` is `None` with no clients; it then sleeps purely on `delivery_notify`                                              | `lib.rs:2731`, `:4903-4911`                  |
| `C2S_RESTART` respawns in place, reusing pty id and driver                                                                                         | `lib.rs:8683`, `pty_unix.rs:570`             |

Machinery to reuse: the `audio.rs:608-690` heal loop (rate limit +
burst limiter + give-up) is already a restart policy;
`uplink.rs:54-104` is backoff with jitter; `crates/sd-notify/` is a
pure-libc `sd_notify(3)` **client**, so blit speaks the readiness
protocol without listening for it; `regex` is already a `blit-server`
dependency (`crates/server/Cargo.toml:35`); `config.rs:632` is the
live-reload watcher behind `blit.remotes`.

## Constraints

- **The wire is version-stable.** `PROTOCOL_VERSION = 1` is frozen —
  the JS client hard-closes on `version > 1`
  (`js/core/src/BlitConnection.ts:2909`). Compatibility rides on new
  opcodes plus a `FEATURE_*` bit, or append-only trailing fields
  length-gated on parse (`crates/remote/src/lib.rs:1598`).
- **Unknown opcodes are silently dropped both ways** (`lib.rs:8897`,
  `BlitConnection.ts:3663`), so a new client cannot tell "old server"
  from "processed". Everything is feature-gated, and nonce-bearing
  requests need a refusal path (`refuse_lsp_message`,
  `lib.rs:7431-7441`, defined at `:6928`).
- **Flat crate layout:** one new `crates/server/src/units.rs`.
- **The server is the stateful half** and nothing above the socket is
  alive at boot, so units live in the server.
- **New families take the next `0x?0` block** — 0x40 fs, 0x50 git,
  0x60 lsp, 0x70 kv, 0x80 net, so 0x90 here.
- **No `serde`, `toml`, or `ini` crate exists in the workspace.**

## Primitives

### Group kill and escalation

Every blit child is a `setsid()` leader, so `kill(-pid, sig)` is
already valid with no new bookkeeping.

| `KillMode=`               | Behavior                                                                             |
| ------------------------- | ------------------------------------------------------------------------------------ |
| `process`                 | `kill(pid, sig)` — today's behavior                                                  |
| `process-group` (default) | `kill(-pid, sig)`, plus `TIOCGPGRP` → `kill(-fg_pgid, sig)` when the fg pgrp differs |
| `control-group`           | Linux only, opt-in: delegated cgroup v2 + `cgroup.kill`                              |

`process-group` catches everything except a process that deliberately
`setsid`'d into a new session. Only a cgroup (or
`PR_SET_CHILD_SUBREAPER` plus a descendant walk) catches that, both
Linux-only, so `control-group` stays opt-in — blit is an unprivileged
userspace multiplexer that also runs on macOS and Windows.

Making `process-group` the default changes behavior for existing
clients, intentionally. `C2S_CLOSE` gains the same escalation: SIGHUP
to the group, wait `TimeoutStopSec`, SIGKILL to the group. Today it
closes the master fd immediately and leans on the kernel hangup, so a
runaway grandchild holding the slave open yields no EOF and no
`exited` transition. The timed half needs the supervisor loop, so it
lands with it.

**Wire:** append an optional `[flags:1]` to `C2S_KILL`. The arm is
`data.len() >= 7`, so old clients inherit the new default unaffected.

### The supervisor loop

A hung command outliving a disconnected orchestrator is exactly where
the delivery loop stops scheduling: `blanket_frame_interval` returns
`None` with no clients (`lib.rs:2731`), so `next_deadline` is `None`
and it sleeps on `delivery_notify`. A silent runaway produces no
output and is never visited.

That argues for a second loop, not for polling. The supervisor is
**fully reactive**, shaped like the delivery loop at
`lib.rs:2597-2612`: `select!` between `supervisor_notify` and
`sleep_until(next_deadline)`, `next_deadline` the minimum over armed
timers, `None` meaning sleep indefinitely. Every timer here is a
computable instant — deadline expiry, lease grace, restart backoff,
`TimeoutStartSec`, `TimeoutStopSec`, next health probe, watchdog,
`exited-linger` — so arming or disarming notifies the loop to
recompute, and an idle server wakes zero times.

#### Child death

The one unreactive thing in the server: no SIGCHLD handler exists, and
exit is inferred from EOF plus the 5 s poll. The supervisor adds a
`SignalKind::child()` handler, same shape as the SIGTERM/SIGINT
handler at `lib.rs:2653-2662` (`tokio` is already
`features = ["full"]`).

**It must not call today's `reap_zombies` more often.** That function
drains `waitpid(-1, WNOHANG)` and _discards_ the status of any pid
outside `pty_pids()` (`pty_unix.rs:266-284`). The discard is
deliberate — a foreign child like an LSP backend is reaped by its own
engine — but it steals statuses from anything the server spawns via
`Command` and waits on itself. The audio pipeline lives with that race
at 5 s; per-SIGCHLD would widen it sharply, and this RFC adds periodic
`ExecHealthCheck=` children into its path.

So the handler only **wakes the supervisor**, which reaps by targeted
`waitpid(pid, WNOHANG)` over pids it owns — PTY children plus the
helpers of `ExecStartPre=`, `ExecStop=`, `ExecHealthCheck=` — all
registered through `register_pty_pid`/`pty_pids()`
(`pty_unix.rs:545`, `:296-304`) generalized to owned children. **The
global `waitpid(-1)` drain is deleted, not rescheduled.**
`Command`-owned children keep being reaped by their owners, and no
status is collected by a party that did not spawn it.

Windows keeps a poll: no SIGCHLD, and `reap_zombies` is already a
no-op (`pty/pty_windows.rs:101`).

### Deadlines and leases

Four independent causes, with an enforced minimum:

```text
effective = min(explicit, lease[current_epoch], runtime_max, stop_escalation)
```

Each cause is armed and cleared only by itself. One enforcement path,
several constraints — otherwise a lease disconnect clobbers an
explicit deadline, or a reclaim clears a `RuntimeMaxSec` it never set.

| Cause           | Set by                                                                 |
| --------------- | ---------------------------------------------------------------------- |
| explicit        | `C2S_DEADLINE [0x1D][pty_id:2][ms:4]` — arm, refresh, clear (`ms = 0`) |
| explicit        | `CREATE2_HAS_DEADLINE` flag + trailing `[ms:4]` on `C2S_CREATE2`       |
| runtime_max     | `RuntimeMaxSec=`                                                       |
| stop_escalation | the `TimeoutStopSec` timer between SIGTERM and SIGKILL                 |
| lease           | lease disconnect grace (below)                                         |

`C2S_DEADLINE` is refreshable, so it doubles as a dead-man switch:
re-arm every 30 s and the session dies ~30 s after the orchestrator
does. The `CREATE2` flag closes the create-then-arm window in which an
orchestrator can die leaking exactly the session it meant to protect.

#### Leases

`C2S_LEASE [0x1E][flags:1][grace_ms:4][lease_id:8]` →
`S2C_LEASE [lease_id:8][epoch:4]`. Sessions created while a connection
holds a lease are tagged with it; on disconnect each live leased
session gets a `grace_ms` deadline **in the lease cause only**, and a
reclaim clears that cause and nothing else.

- **The server mints `lease_id`**, unguessably. A client-chosen `u64`
  is a namespace anyone on the socket can guess, for what is a kill
  switch on other people's sessions.
- **`epoch` increments on every reclaim.** A disconnect arms the lease
  deadline only if its epoch is current, so an old connection dying
  after a newer one reclaimed cannot revoke the newer claim.
- **One holder at a time.** A second reclaim supersedes the first,
  which is thereafter epoch-stale. No shared ownership, no holder
  count.

#### Escalation and attribution

On expiry: SIGTERM to the group, wait `TimeoutStopSec` (default 5 s;
systemd's 90 s is wrong for agent workloads), SIGKILL to the group.

Append `[reason:1]` to `S2C_EXITED`
(`0=normal, 1=deadline, 2=lease, 3=gc, 4=unit-stop`) — a deadline kill
currently arrives as `-9`, indistinguishable from a user kill. When
causes expire together the reason is the one that produced the
minimum, ties broken in table order, so attribution is deterministic
rather than whichever timer the loop saw first. Append-only and
length-gated, like the boot generation in `S2C_HELLO`.

### GC and `max_ptys`

**Exited slots.** Add `exited_at: Instant` to `Pty`; reap in the
supervisor loop under two bounds — `max-exited` (count, default 1024,
oldest first) and `exited-linger` (time, default off). Eviction runs
the `C2S_CLOSE` path and broadcasts `S2C_CLOSED`, which every client
already handles, so no client change is needed. Count-cap-only is the
conservative default: consumers create one session per tool call and
read output back well after exit, so a short linger silently breaks
them while a generous count cap turns an unbounded leak into a bounded
one.

**Live but abandoned sessions** are not GC'd on a timer — detaching
and returning is the point of a multiplexer. Deadlines and leases are
the opt-in tools.

**`max_ptys`** gets a real default plus `BLIT_MAX_PTYS`, counting live
sessions only, and the silent `continue` goes:
`S2C_CREATE_FAILED [nonce:2][reason:1]` answers a nonce-bearing client
instead of hanging it forever. Standalone bug, worth fixing
regardless.

**Unit sessions do not count against `max_ptys`.** The cap bounds
client-driven creation, which is where the leak is; unit sessions are
bounded by files an operator wrote. Counting them together means a
useful cap can be exhausted during autostart, failing units at boot
for an unrelated reason — nondeterministically, depending on how many
clients happen to be connected. `max-units` bounds units instead.

## Units

### Identity and generations

```rust
struct UnitRuntime {
    name: UnitName,               // validated, unique, server-owned
    definition: UnitDefinition,
    state: UnitState,
    current: Option<(PtyId, Generation)>,
    restarts: StartLimiter,
}
```

The PTY remains the only stream object; the registry points at the
current one and never duplicates the terminal, scrollback, or driver.
This is orchestration state, not a second process model.

`Pty.tag` is not identity — tags are client-chosen and unchecked for
uniqueness (`lib.rs:7777-7817`), so any client could create a session
tagged `api`. Unit names are validated and unique in the registry, and
a unit-owned PTY carries `unit: Option<UnitName>` set only by the
supervisor, never settable over the wire. A client PTY whose tag
collides with a unit name is left alone: not adopted, not refused,
never in `S2C_UNIT_LIST`.

#### Generations

Every unit-owned PTY carries a `Generation` incremented on each
restart, and **every asynchronous event carries
`(pty_id, generation)`**.

In-place respawn is safe today only because it happens after the PTY
reached the EOF/exited path, so there is no live reader and no open
master. Restarting on leader death removes that precondition: the old
reader can still be alive, old descendants can still be writing, and
`respawn_child` would open a second master against the same pty id, so
a late EOF would mutate the new generation.

A restart is therefore an ordered sequence:

1. Record the leader's exit status.
2. Terminate the remaining old process group (`KillMode=`, then the
   `TimeoutStopSec` escalation).
3. Close the old master fd.
4. Retire and join the old reader.
5. Increment the generation.
6. Spawn the replacement.
7. Drop any event whose generation is not current.

Step 7 is the cheap backstop that makes the rest safe to get slightly
wrong.

### File format

`~/.config/blit/units/<name>.unit`, with `/etc/blit/units/` shadowed
by name from the user directory. INI with sections and systemd's key
names wherever a systemd concept exists; blit-specific keys stay in
its house style.

**Only the vocabulary transfers, not systemd's parser.** systemd's
syntax carries specifiers (`%i`, `%h`), `ExecStart=` prefixes (`-`,
`@`, `+`, `!`), continuations, its own quoting and escaping, drop-in
merge order, and `EnvironmentFile=` expansion. Implementing a subset
of that in sixty hand-rolled lines while implying compatibility is how
a code-executing file format acquires surprises. So the grammar is
small, explicit, and stricter than systemd's:

| Question                      | Answer                                                                                                        |
| ----------------------------- | ------------------------------------------------------------------------------------------------------------- |
| Line forms                    | `[Section]`, `Key=Value`, `# comment`, blank. Nothing else.                                                   |
| Continuations                 | Unsupported.                                                                                                  |
| Inline comments               | Unsupported — `#` only at line start, so values need no escaping.                                             |
| Value                         | Everything after the first `=`, trimmed.                                                                      |
| Quoting / escaping            | None.                                                                                                         |
| `Exec*=`                      | `argv` split on whitespace, `execve`'d directly. **Never `/bin/sh -c`.** No globbing, expansion, or prefixes. |
| Specifiers                    | Unsupported.                                                                                                  |
| `Environment=`                | Repeatable, one `NAME=VALUE` each, no expansion. No `EnvironmentFile=`.                                       |
| Repeated scalar key           | **Error**, not last-wins.                                                                                     |
| Repeated list key             | `Requires=`, `Wants=`, `After=`, `Environment=`, `ExecStartPre=` append.                                      |
| Dependency lists              | Whitespace-separated, repeatable across lines.                                                                |
| Durations                     | Bare integer = seconds; `ms`/`s`/`m`/`h` suffixes; `infinity` where meaningful; `0` disables.                 |
| Booleans                      | `yes`/`no`.                                                                                                   |
| Unknown section or key        | **Error.**                                                                                                    |
| Malformed / duplicate section | Error, with file and line number.                                                                             |

Strict beats permissive: a silently ignored misspelled `Restart=` is a
unit that never restarts and nobody notices, and a strict grammar can
be relaxed later where a permissive one cannot be tightened.

```ini
# ~/.config/blit/units/example.unit — every key, for reference
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
HealthCheckFailureThreshold=3
ActiveWhenHealthy=yes    # default when ExecHealthCheck= is set

[Install]
Autostart=yes
```

Deviations from systemd, each deliberate:

- **`[Install] Autostart=`** replaces `WantedBy=` plus `enable`
  symlinks into `.wants/`. Targets and symlink farms are the heaviest
  part of systemd's model and buy nothing without a boot-ordering
  graph; presence in the directory plus one boolean is the whole
  enable/disable story.
- **`ExecHealthCheck=`, `HealthCheckSec=`, `HealthCheckTimeoutSec=`,
  `HealthCheck*Threshold=`, `ActiveWhenHealthy=`** are new — systemd
  has no health-check concept, only `WatchdogSec`.
- **`Type=match`/`surface`, `ReadyMatch=`, `ReadySurface=`** have no
  analog.
- **`Backing=`, `RestartMaxSec=`** are blit-specific.

### State machine

```
inactive ──start──> activating ──ready+healthy──> active
   ^                    │                            │
   │                    │ fail                       │ exit / stop / health fail
   │                    v                            v
   └──stop/GC──── failed <──limit──── deactivating ──┘
```

The diagram is a summary; the table is the contract. Every transition
is `(state, event) → (state, actions)`, and the whole thing is a pure
function (see [Testability](#testability)).

| State          | Event                                           | Next                  | Actions                                         |
| -------------- | ----------------------------------------------- | --------------------- | ----------------------------------------------- |
| `inactive`     | start, deps satisfied                           | `activating`          | spawn gen N, arm `TimeoutStartSec`              |
| `inactive`     | start, `Requires=` dep failed                   | `failed`              | —                                               |
| `activating`   | spawn failed                                    | `failed`              | apply `Restart=`                                |
| `activating`   | ready probe fired, no health gate               | `active`              | disarm start timer, arm `RuntimeMaxSec`         |
| `activating`   | ready probe fired, health gating                | `activating`          | run probe now                                   |
| `activating`   | first health probe passed                       | `active`              | disarm start timer, arm health interval         |
| `activating`   | health probe failed                             | `activating`          | retry until `TimeoutStartSec`                   |
| `activating`   | `TimeoutStartSec` expired                       | `failed`              | stop sequence, apply `Restart=`                 |
| `activating`   | child exited, `Type=oneshot`, rc=0              | `active`              | `RemainAfterExit=no` → `inactive`               |
| `activating`   | child exited, otherwise                         | `failed`              | apply `Restart=`                                |
| `active`       | child exited                                    | `failed` / `inactive` | by `Restart=` and exit status                   |
| `active`       | health failures ≥ `HealthCheckFailureThreshold` | `failed`              | stop sequence, apply `Restart=`                 |
| `active`       | watchdog expired                                | `failed`              | stop sequence, apply `Restart=`                 |
| `active`       | `RuntimeMaxSec` / deadline expired              | `deactivating`        | stop sequence, reason `deadline`                |
| `active`       | explicit stop                                   | `deactivating`        | `ExecStop=`, escalation, record operator intent |
| `active`       | `Requires=` dependency stopped                  | `deactivating`        | stop sequence                                   |
| `deactivating` | stop completed                                  | `inactive`            | retire generation                               |
| `deactivating` | `TimeoutStopSec` expired                        | `inactive`            | SIGKILL group, retire generation                |
| any            | restart scheduled, backoff elapsed              | `activating`          | spawn gen N+1                                   |
| any            | start limit exhausted                           | `failed`              | stop scheduling restarts                        |
| any            | event from a stale generation                   | unchanged             | **drop**                                        |
| any            | definition removed on reload                    | `deactivating`        | stop, drop from registry when `inactive`        |

`Restart=` evaluates identically everywhere: `no` never, `on-failure`
on a non-zero or signalled exit and on any `failed` entry, `always` on
every exit. Backoff is `RestartSec` doubling with jitter to
`RestartMaxSec`; `StartLimitBurst` over `StartLimitIntervalSec`
exhausts the limiter. That is `audio.rs:650`'s burst limiter and
`uplink.rs:60-104`'s backoff, generalized. Restarts respawn in place
under the generation discipline above — a restarting unit is not a new
session.

### Readiness

`Type=` keeps its systemd meaning exactly.

| `Type=`   | Active when                                                       |
| --------- | ----------------------------------------------------------------- |
| `simple`  | the child is spawned                                              |
| `oneshot` | the child exits zero; `RemainAfterExit=yes` keeps it active after |
| `notify`  | the child sends `READY=1`                                         |
| `match`   | `ReadyMatch=<regex>` matches the output stream                    |
| `surface` | a compositor surface matching `ReadySurface=<app_id>` exists      |

- **`oneshot`** is what makes "run this once, but only after its
  dependencies are genuinely up" expressible. `Restart=` is rejected
  for it in anything but `no`.
- **`notify`** — blit sets `NOTIFY_SOCKET` in `build_child_env` (which
  already injects `BLIT_SOCK`, `WAYLAND_DISPLAY`, `PULSE_SERVER`) and
  listens on an `AF_UNIX SOCK_DGRAM` socket for `READY=1`, `STATUS=`,
  `WATCHDOG=1`. The strongest fit here: `crates/sd-notify/` already
  implements the client half in pure libc, so the listener is that
  code mirrored, and blit ends up both speaking and understanding
  `sd_notify(3)`.
- **`match`** is cheap only in blit, since the server already handles
  every byte and already depends on `regex`. It matches the **raw
  bytes off the reader thread, before terminal interpretation** — not
  the rendered grid, where a redrawing progress bar could be
  overwritten between scans and never observed. Split on `\n` **and**
  `\r`, since progress output updates a line with a bare CR; segments
  capped at 16 KiB so a program that never emits a newline cannot grow
  the buffer; scanning stops at readiness or `TimeoutStartSec`.
- **`surface`** is the only honest definition of started for a GUI
  unit, since a browser process exists long before it has a window.
  `app_id` is already tracked per surface and shipped on
  `S2C_SURFACE_CREATED`, so no compositor change is needed. Matching
  is by `app_id`, not process, so two units running the same binary
  cannot be told apart; precise attribution means plumbing
  `SO_PEERCRED` from the Wayland client socket into `Surface`, since
  surfaces are keyed by Wayland object id and no client credentials
  are recorded today (the `pid` bindings in
  `crates/compositor/src/imp.rs` are protocol ids).

`TimeoutStartSec` elapsing without readiness → `failed`, and
`Restart=` applies.

### Health

Readiness is a one-shot edge; health is a continuous property.

Health participates in activation through `ActiveWhenHealthy=`, which
**defaults to `yes` whenever `ExecHealthCheck=` is set** and is
otherwise inert: a unit then reaches `active` only once its readiness
probe has fired _and_ its first health check passed, the window
bounded by `TimeoutStartSec`. With no health check every `Type=`
behaves precisely as systemd documents it. Keeping this out of `Type=`
matters: the argument for borrowing systemd's spellings is that muscle
memory transfers, and silently redefining `Type=simple` would break
exactly that.

- **Watchdog.** For `Type=notify`, a missing `WATCHDOG=1` within
  `WatchdogSec` fails the unit and applies `Restart=`. Free once the
  notify listener exists; blit sets `WATCHDOG_USEC` as systemd does.
- **`ExecHealthCheck=`** runs every `HealthCheckSec`, killed after
  `HealthCheckTimeoutSec`, never overlapping — one still running when
  the next is due is skipped, not stacked. Failure is **thresholded**:
  `HealthCheckFailureThreshold` (default 3) consecutive failures fail
  the unit, `HealthCheckSuccessThreshold` (default 1) recovers it.
  Restarting because one `curl` lost a race is worse than the problem.

**`After=` is an initial gate, not a maintained invariant.** It waits
for the dependency's first `active`, which under health gating means
its first passing probe. It does not hold the dependent stopped while
the dependency is unhealthy later, and `Requires=` propagates a stop,
not a health transition. Worth stating because "wait until the server
passes its health check" reads like a continuous guarantee.

#### Helper children

`ExecStartPre=`, `ExecStop=`, and `ExecHealthCheck=` are processes the
server spawns that are **not** sessions. They go through the `Command`

- `Stdio::piped` path the audio pipeline already uses
  (`audio.rs:345` onward), not the PTY spawner, each with
  `setpgid(0, 0)` so its own timeout can group-kill it — a probe that
  shells out must not orphan the shell's children — and are tracked in a
  `helpers` map separate from `ptys`.

The SIGCHLD handler must dispatch on pid: PTY child, helper, or
unknown-and-ignored. `register_pty_pid`/`pty_pids()` exists for
exactly this problem and generalizes to owned children.

`ExecStartPre=` runs to completion before `ExecStart=`; non-zero fails
activation. `ExecStop=` runs before the SIGTERM escalation, and if it
returns zero with the main process gone the unit stops cleanly —
otherwise escalation proceeds, so a hung `ExecStop=` cannot wedge a
stop.

### Dependencies

- **`Requires=a`** pulls `a` in; this unit fails if `a` fails to
  start, **and stops if `a` later stops**. No ordering implied.
- **`Wants=a`** pulls `a` in and ignores its outcome, then and later.
- **`After=a`** is ordering only: do not start until `a` is `active`.

`Requires=` carries systemd's full meaning including propagation,
because taking the spelling and dropping half the semantics is worse
than not borrowing the word. A restart is not a stop: as in systemd,
`active → activating → active` leaves dependents alone, and only a
transition to `inactive`/`failed` outside a restart propagates.

`active` is doing the work in the `After=` line — it means ready by
whichever probe `Type=` selects, and under health gating first probe
passed. One keyword therefore covers "after the process exists",
"after it says it is ready", "after it has a window", "after it
answers its health endpoint", and "after that command finished",
depending only on how the dependency describes itself.

Cycles are refused at load with a diagnostic naming the cycle rather
than silently broken by dropping ordering edges. Because files
live-reload, a cycle introduced by an edit rejects **that file** and
keeps the last good graph.

### Pipe backing

`Backing=pipe` is the same fork with `pipe2()` on stdin/stdout/stderr
instead of the pty slave, and no `setsid`/`TIOCSCTTY`. Bytes still
feed the alacritty driver, so `S2C_UPDATE`, scrollback, search,
`C2S_COPY_RANGE`, and browser rendering work unchanged. The difference
is on the child's side: no tty, so no job control, no SIGWINCH, and
tools correctly detect they are not on a terminal.

A pipe child is not a session leader, so `KillMode=process-group`
requires the forked child to `setpgid(0, 0)` explicitly.

### Wire and CLI

New family in the `0x90` block, gated on `FEATURE_UNITS`, bit 11
(bits 11-31 free).

| Dir | Opcode | Name           | Layout                                                                                                 |
| --- | ------ | -------------- | ------------------------------------------------------------------------------------------------------ |
| C2S | `0x90` | `UNIT_LIST`    | `[nonce:2]`                                                                                            |
| C2S | `0x91` | `UNIT_START`   | `[nonce:2][name:N]`                                                                                    |
| C2S | `0x92` | `UNIT_STOP`    | `[nonce:2][name:N]`                                                                                    |
| C2S | `0x93` | `UNIT_RESTART` | `[nonce:2][name:N]`                                                                                    |
| C2S | `0x94` | `UNIT_RELOAD`  | `[nonce:2]`                                                                                            |
| S2C | `0x90` | `UNIT_LIST`    | `[nonce:2][count:2]`, then per unit: name, state, pty id, generation, restart count, last exit, health |
| S2C | `0x91` | `UNIT_STATE`   | `[name_len:2][name:N][state:1][pty_id:2][generation:8]` — pushed on every transition                   |

Every request is nonce-bearing, so all need the refusal path from
[Constraints](#constraints) when the family is disabled.

CLI: `blit unit list|start|stop|restart|status|cat|reload`, following
the existing subcommand pattern, added to `crates/cli/src/learn.md` so
agents discover it.

### Server restart

Unit runtime state is in memory and does not survive a server restart.
Autostart re-runs units from their files, and clients detect the
restart through the boot generation in `S2C_HELLO`. Crash-only, and
written down so nobody expects a running unit's state to be durable.

One exception, and it is not runtime state but **operator intent**.
`blit unit stop api` followed by a server restart would otherwise
re-autostart `api`, silently contradicting the person who stopped it,
and no amount of re-reading the files recovers that decision — it is
not in them. So "disabled" and "manually stopped" are persisted;
everything else is recomputed.

They go in the KV store ([kv.md](kv.md)) — host-local, redb-backed,
already durable — under a reserved `blit/units/` prefix. That implies
one addition to the KV family: **`blit/` becomes a server-owned
prefix**, rejected for client `KV_PUT` with `KV_STATUS_INVALID` and
readable like anything else. Without it a client could forge operator
intent, which is worse than the property being fixed. Worth reserving
regardless, as the namespace for any future server-owned state.

### Support matrix

| Capability                          | Unix                 | Windows                                |
| ----------------------------------- | -------------------- | -------------------------------------- |
| Unit files, autostart, dependencies | yes                  | yes                                    |
| `Restart=`, backoff, start limits   | yes                  | yes                                    |
| `Type=simple`/`oneshot`/`match`     | yes                  | yes                                    |
| Health checks, `ActiveWhenHealthy=` | yes                  | yes                                    |
| Deadlines, leases, GC               | yes                  | yes                                    |
| `Type=notify`, `WatchdogSec=`       | yes                  | no (`sd_notify` is a Unix socket)      |
| `KillMode=process-group`            | yes                  | no                                     |
| `KillMode=control-group`            | Linux only           | no                                     |
| `Backing=pipe`                      | yes                  | yes (no `setsid`/`setpgid` either way) |
| Reactive child death (SIGCHLD)      | yes                  | no — keeps a poll                      |
| `Type=surface`                      | Linux only (Wayland) | no                                     |

**An unavailable capability is a load error, not a warning.**
`Type=notify` degrading to `simple` would not merely lose a feature:
it breaks the promise that `After=` waits for real readiness, so every
dependent starts early on the platform where nobody is watching. Same
for `KillMode=` quietly becoming `process`. Portability comes from
writing portable units, not from the server pretending.

### Live reload

Reload **parses and validates a complete new registry, then swaps it
atomically**. A file that fails to parse, or an edit introducing a
cycle, rejects that file and keeps its last good definition; the rest
of the swap proceeds. An invalid edit never partially mutates live
policy, and a typo in one unit never takes down what is running.

| Change                                                                                                 | Effect                                                                                         |
| ------------------------------------------------------------------------------------------------------ | ---------------------------------------------------------------------------------------------- |
| `ExecStart=`, `Environment=`, `WorkingDirectory=`, `Backing=`, `Type=`, `ReadyMatch=`, `ReadySurface=` | Applies at the **next start**; a running process is never silently replaced.                   |
| `Restart=`, `RestartSec=`, `RestartMaxSec=`, `StartLimit*`                                             | Immediate; affects the next restart decision.                                                  |
| `ExecHealthCheck=`, `HealthCheckSec=`, thresholds                                                      | Immediate; the interval timer is re-armed.                                                     |
| `RuntimeMaxSec=`                                                                                       | Immediate, re-evaluated against the current start time — shortening can expire a unit at once. |
| `TimeoutStopSec=`, `KillMode=`                                                                         | Immediate; applies to the next stop.                                                           |
| `Requires=`, `Wants=`, `After=`                                                                        | Immediate for future starts; does **not** retroactively stop a running unit.                   |
| `[Install] Autostart=`                                                                                 | Immediate; affects the next server start.                                                      |
| File removed                                                                                           | Unit stopped, then dropped from the registry.                                                  |
| File added                                                                                             | Loaded; started only if `Autostart=yes`.                                                       |

`blit unit restart <name>` applies a changed `ExecStart=`, keeping
reload about definitions and process lifetime under explicit control.

### Worked example

Open a URL in a browser, but only once the server behind it answers
and the browser is actually driveable. Three units, three different
honest definitions of "up".

```ini
# api.unit
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

`Type=simple` plus a health check is the shape most services want, and
the clearest demonstration of health-gated activation: `simple` alone
would call this active the instant `cargo` is exec'd, before
compilation starts, but with `ExecHealthCheck=` set
`ActiveWhenHealthy=` defaults to `yes` and `api` is not `active` until
`/health` answers — no readiness probe, no cooperation from the
program. `TimeoutStartSec=180` covers a cold `cargo build`, and
`KillMode=process-group` stops cargo's child compiler and the server
binary surviving a restart. Swap in `Type=notify` if the service calls
`sd_notify`, or `Type=match` with `ReadyMatch=^Listening on ` if it
only prints; both compose with the health check, which then becomes a
liveness signal.

```ini
# chromium.unit
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

Two conditions compose, and both matter for a browser that will be
driven rather than watched: `Type=surface` says a window exists, the
CDP health check says it is driveable. Either alone is a lie —
chromium answers CDP before it paints, and a window can exist before
the debugger port listens. `Backing=pipe` because chromium has no use
for a tty, which makes `KillMode=process-group` depend on the explicit
`setpgid(0, 0)`. Chromium is also the motivating case for group kill:
it forks a zygote and a renderer per tab, and today's
`kill(child_pid)` leaves every one of them running.

```ini
# open-url.unit
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

Three definitions of "up" — a passing HTTP probe, a Wayland surface
plus a live CDP endpoint, a command that exited zero — and `open-url`
says only `After=api chromium`. `Requires=` earns its place beside it:
if `api` never becomes healthy within `TimeoutStartSec`, `open-url`
fails rather than hanging, and never opens a URL that was never going
to load.

If a health check starts failing later, that unit leaves `active` and
its `Restart=` applies. `open-url` is not re-run — `Requires=`
propagates a stop, not a restart, and re-running dependents is what
`PartOf=` would express (see [Deferred](#deferred)).

## Security

**The blit socket is already an arbitrary-execution capability:** any
client can `C2S_CREATE` any command. Units add **durability**, not
execution — a unit file outlives the connection, the client, and the
server process, and runs unattended at boot.

Installing units through the `fs` family is intended, so the honest
move is to state the equivalence rather than pretend to mitigate it:
**write access to a unit directory is durable code execution on that
host.** There is no privilege boundary inside the protocol to defend
here; the boundary is at the socket — gateway passphrase, SSH, and
what the listener is reachable from.

Permission bits in particular do not provide one. A remote `fs` client
writes as the server's own user, `0644`, in that user's own directory:
it passes every mode check. Mode bits distinguish a different Unix
account from this one; they say nothing about whether bytes arrived
from a keyboard or a socket.

What is worth having:

- **`BLIT_UNITS=0` disables unit loading and autostart entirely**, so
  a deployment that exposes the socket widely can have no durable
  surface at all.
- **Load-time hygiene, not security:** refuse symlinked unit files,
  refuse group- or world-writable files and directories, resolve and
  validate the path before opening.
- **Every load, start, and restart logs the unit name and resolved
  path**, so persistence is auditable after the fact — the realistic
  control when the capability itself is deliberately granted.

## Deferred

Considered and left out on purpose.

- **`Before=`, `Conflicts=`, `PartOf=`, `BindsTo=`.** `Requires=` plus
  `After=` covers the target cases, and each of the rest adds a
  propagation edge to reason about during live reload. `PartOf=` is
  the most likely next want.
- **Precise `Type=surface` attribution** via `SO_PEERCRED` — real
  compositor work for a case that may not arise.
- **Drop-in directories** (`<name>.unit.d/*.conf`) — shadow-by-name
  already covers the common case.
- **Templated units** (`foo@bar.unit`) — no motivating use yet.
- **Durable unit runtime state** — restart counts and last-exit are
  deliberately lost; only operator intent persists.
- **`EnvironmentFile=`, specifiers, `Exec*=` prefixes** — systemd
  syntax the strict grammar omits; adding any means owning a
  compatibility surface rather than a vocabulary.
- **Continuous health-based ordering** — `After=` gates on first
  healthy and does not maintain the invariant.

## Testability

This lands policy in `blit-server`, at roughly 36% line coverage. Two
rules, both following existing precedent:

- **Factor the wire family as a standalone
  `handle_unit_message(data, state, out, verbose)`**, as fs, git, lsp,
  kv, and net already are. That factoring is why those families are
  testable: the server test pattern drives a handler over an `mpsc`
  outbox and polls for the expected opcode. An arm of the big
  `match data[0]` inside `handle_client` has no equivalent.
- **Keep the state machine pure** —
  `fn step(state: UnitState, ev: UnitEvent, cfg: &Unit) -> (UnitState, Vec<Action>)`
  with actions returned as data (`Spawn`, `Signal`, `ArmTimer`,
  `Notify`) rather than performed inline. Backoff schedules, the start
  limiter, health gating, `Requires=` propagation, and deadline
  escalation ordering then become table tests with the clock as an
  input — no spawning, no timing flakes.

## Delivery

One axis per PR.

1. **Group kill.** `KillMode` as an appended flag byte on `C2S_KILL`;
   `C2S_CLOSE`'s SIGHUP to the group. Pure semantics, no new state.
2. **Supervisor loop, deadlines, leases.** The reactive loop, the
   SIGCHLD handler replacing the `waitpid(-1)` drain, `C2S_DEADLINE`,
   `CREATE2_HAS_DEADLINE`, `C2S_LEASE`/`S2C_LEASE`, the `S2C_EXITED`
   reason byte, and the timed half of the `C2S_CLOSE` escalation.
3. **GC.** `exited_at`, count and time bounds, a real `max_ptys`
   default with `BLIT_MAX_PTYS`, `S2C_CREATE_FAILED`. Fixes the
   silent-hang bug on its own.
4. **Unit core.** Registry and generations, the strict parser, the
   state machine, autostart, restart policy, `Requires=`/`After=`,
   `Type=simple`/`oneshot`/`notify`, start and stop timeouts, helper
   children, the `0x90` family, the CLI. PTY backing only.
5. **Unit policy.** Health checks with thresholds and
   `ActiveWhenHealthy=`, `WatchdogSec=`, `Type=match`, `Type=surface`,
   `Backing=pipe`, live reload, the reserved KV prefix.

1-3 are independently valuable, are the incident-shaped fixes, and
should be exercised before the unit layer commits to timer provenance,
reconnect ownership, and generation handling. Splitting 4 from 5 keeps
the first unit implementation's race surface small: the generation and
ownership contracts get exercised by a PTY-only, probe-free core
before health probes, a second spawn path, and live reload land on
them.
