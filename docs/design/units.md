# RFC: Units — supervised sessions with lifecycle policy

- **Status:** Draft. The three primitives shipped in
  [#204](https://github.com/indent-com/blit/pull/204); the unit layer
  itself is unimplemented.
- **Date:** 2026-08-01
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

Units sit on three primitives that fix existing bugs and ship first.
All three landed in [#204](https://github.com/indent-com/blit/pull/204),
so this RFC is now only the unit layer:

- **Deadlines.** Every timeout was client-side, so a hung command
  outlived a disconnected orchestrator forever.
- **Group kill.** `C2S_KILL` signalled the leader pid only; `C2S_CLOSE`'s
  SIGHUP missed anything that changed process group.
- **GC.** Nothing freed an exited PTY slot and `max_ptys` was hardcoded
  unlimited, so one session per tool call leaked without an explicit
  `C2S_CLOSE`.

## What exists today

Verified against `1919717`, i.e. after
[#204](https://github.com/indent-com/blit/pull/204) shipped the
primitives. Rows marked **#204** were false when this RFC was written
and are the parts of it that already landed.

| Fact                                                                                                                                                                     | Evidence                                                                                                     |
| ------------------------------------------------------------------------------------------------------------------------------------------------------------------------ | ------------------------------------------------------------------------------------------------------------ |
| PTY sessions are one flat `FxHashMap<u16, Pty>` on a single global `Session` — in blit, "session" means the whole server                                                 | `crates/server/src/lib.rs:4118-4119`                                                                         |
| `Pty` carries no owner client, no creation time and no attached-client count; **#204** added `deadline`, `stop_deadline`, `exit_reason`, `exited_at`, `generation`       | `lib.rs:543-601`                                                                                             |
| Children are `setsid()` + `TIOCSCTTY` leaders, so `child_pid == pgid == sid`                                                                                             | `pty/pty_unix.rs:657-658`                                                                                    |
| **#204** `C2S_KILL` signals the process group by default (`TIOCGPGRP` pgid, else `-child_pid`); leader-only is the opt-in `KILL_LEADER_ONLY` flag                        | `lib.rs:15603-15617`, `pty_unix.rs:285-298`                                                                  |
| **#204** `C2S_CLOSE` → group SIGHUP, then `close(master_fd)`, then the pid is abandoned with a SIGKILL deadline                                                          | `pty_unix.rs:306-311`, `lib.rs:15638`                                                                        |
| **#204** Exit is detected by a SIGCHLD-woken supervisor calling `poll_child_exited`, with pty-master EOF only as a deferred secondary path                               | `lib.rs:6214-6224`, `:6458-6473`, `pty_unix.rs:340`, `:572-576`                                              |
| **#204** `reap_zombies` waits only pids it owns; it drains `waitpid(-1)` only when running as PID 1                                                                      | `pty_unix.rs:430-478`, `:456-458`, sweep at `lib.rs:6237`                                                    |
| `cleanup_pty_internal` still sets `exited: true` and keeps the entry; **#204** `evict_exited` removes it later under the retention bounds                                | `lib.rs:6269-6289`, `:6142-6166`                                                                             |
| `ptys.remove` now has two sites: the `C2S_CLOSE` arm and retention eviction                                                                                              | `lib.rs:15620`, `:6152`                                                                                      |
| `max_ptys` still defaults to 0 (unlimited); **#204** made it settable by `--max-ptys` / `BLIT_MAX_PTYS`                                                                  | `crates/cli/src/main.rs:960`, `crates/cli/src/cli.rs:277-282`                                                |
| **The remaining refusal gap:** only `C2S_CREATE2` gets `S2C_CREATE_FAILED` on a cap hit. `C2S_CREATE`, `C2S_CREATE_AT` and `C2S_CREATE_N` are still a bare `continue`    | cap `lib.rs:4474-4485`, `refuse_create` `:5925`, silent sites `:13973-13975`, `:14078-14080`, `:14175-14177` |
| Tags are client-chosen, optional, and not checked for uniqueness at creation                                                                                             | `lib.rs:13935-13944`                                                                                         |
| The delivery loop's `next_deadline` is `None` with no clients; it then sleeps purely on `delivery_notify`. **#204** put lifecycle timers in a separate `supervisor_loop` | `lib.rs:10186-10189`, `:6425-6446`, supervisor `:6072-6093`                                                  |
| `C2S_RESTART` respawns in place, reusing pty id and driver; **#204** made it bump `Pty::generation`                                                                      | `lib.rs:15386`, `pty_unix.rs:790`                                                                            |

Machinery to reuse: the `audio.rs:753-834` heal loop (rate limit +
burst limiter + give-up) is already a restart policy;
`crates/cli/src/uplink.rs:60-104` is backoff with jitter; `crates/sd-notify/` is a
pure-libc `sd_notify(3)` **client**, so blit speaks the readiness
protocol without listening for it; `regex` is already a `blit-server`
dependency (`crates/server/Cargo.toml:46`); `crates/webserver/src/config.rs:779` is the
live-reload watcher behind `blit.remotes`.

## Constraints

- **The wire is version-stable.** `PROTOCOL_VERSION = 1` is frozen —
  the JS client hard-closes on `version > 1`
  (`js/core/src/BlitConnection.ts:4631-4659`). Compatibility rides on new
  opcodes plus a `FEATURE_*` bit, or append-only trailing fields
  length-gated on parse — `S2C_HELLO` has taken that route twice, for
  the boot generation and then the server version
  (`crates/remote/src/lib.rs:2753-2769`).
- **Unknown opcodes are silently dropped both ways** (`lib.rs:15649`,
  `BlitConnection.ts:5687-5688`), so a new client cannot tell "old server"
  from "processed". Everything is feature-gated, and nonce-bearing
  requests need a refusal path (`refuse_lsp_message`,
  `lib.rs:13538-13542`, defined at `:12833`).
- **Flat crate layout:** one new `crates/server/src/units.rs`.
- **The server is the stateful half** and nothing above the socket is
  alive at boot, so units live in the server.
- **New families take a free `0x?0` block** — 0x40 fs, 0x60 lsp, 0x70
  kv, 0x80 net, 0x90-0x95 extensions and channels, 0xA0-0xBF git,
  0xC0-0xC6 processes, so 0xD0 here. In the core range C2S `0x1F` and
  S2C `0x12`-`0x1F` are unallocated.
- **No `serde`, `toml`, or `ini` crate exists in the workspace.**

## Primitives

### Group kill and escalation

Every blit child is a `setsid()` leader, so `kill(-pid, sig)` is
already valid with no new bookkeeping. **Shipped in #204**, so the
table below is now the server's behavior rather than a proposal, minus
`control-group`.

| `KillMode=`               | Behavior                                                                             |
| ------------------------- | ------------------------------------------------------------------------------------ |
| `process`                 | `kill(pid, sig)` — the pre-#204 behavior, now opt-in                                 |
| `process-group` (default) | `kill(-pid, sig)`, plus `TIOCGPGRP` → `kill(-fg_pgid, sig)` when the fg pgrp differs |
| `control-group`           | Linux only, opt-in, **not implemented**: delegated cgroup v2 + `cgroup.kill`         |

`process-group` catches everything except a process that deliberately
`setsid`'d into a new session. Only a cgroup (or
`PR_SET_CHILD_SUBREAPER` plus a descendant walk) catches that, both
Linux-only, so `control-group` stays opt-in — blit is an unprivileged
userspace multiplexer that also runs on macOS and Windows.

**On Windows `process-group` is a Job Object**, also shipped in #204.
Windows has no process groups, but `CreateProcessW`
(`pty_windows.rs:422`) creates the child suspended,
`AssignProcessToJobObject`s it, and resumes; `TerminateJobObject`
(`pty_windows.rs:95`) is the group kill and
`JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE` gives `close_pty` the same
containment the Unix side gets from SIGHUP-to-the-group. Bare
`TerminateProcess` (`pty_windows.rs:97`), which orphans every
grandchild, is now only the leader-only and no-job fallback. That is
what makes the default portable — which matters, because a load error
on an unsupported capability would otherwise reject every unit that
simply omits `KillMode=`. The general rule: **defaults are resolved per
platform before the capability check, so a portable unit never fails
on a key it did not write.** Only keys present in the file can produce
a load error.

Making `process-group` the default changed behavior for existing
clients, intentionally. `C2S_CLOSE` gained the same escalation: SIGHUP
to the group, wait `TimeoutStopSec`, SIGKILL to the group. Before that
it closed the master fd immediately and leaned on the kernel hangup, so
a runaway grandchild holding the slave open yielded no EOF and no
`exited` transition. See [Delivery](#delivery) for how the timed half
actually landed.

**Wire:** append an optional `[flags:1]` to `C2S_KILL`. The arm is
`data.len() >= 8` — 7 is the existing message length, so arming there
would read a flag byte that is not present — and old clients inherit the
new default unaffected. **Shipped in #204** as `KILL_LEADER_ONLY`
(bit 0), advertised by `FEATURE_KILL_MODE` (HELLO bit 15).

### The supervisor loop

**Shipped in #204** as `supervisor_loop` (`lib.rs:6072-6093`).

A hung command outliving a disconnected orchestrator is exactly where
the delivery loop stops scheduling: `blanket_frame_interval` returns
`None` with no clients (`lib.rs:10186-10189`), so `next_deadline` is `None`
and it sleeps on `delivery_notify`. A silent runaway produces no
output and is never visited.

That argued for a second loop, not for polling. The supervisor is
**fully reactive**, shaped like the delivery loop at
`lib.rs:6425-6446`: `select!` between `supervisor_notify` and
`sleep_until(next_deadline)`, `next_deadline` the minimum over armed
timers (`earliest_armed_deadline`, `lib.rs:6097`), `None` meaning sleep
indefinitely. Every timer here is a computable instant — deadline
expiry, lease grace, restart backoff, `TimeoutStartSec`,
`TimeoutStopSec`, next health probe, watchdog, `exited-linger` — so
arming or disarming notifies the loop to recompute, and an idle server
wakes zero times. The unit-layer timers above are the ones still to be
added; the loop itself exists.

#### Child death

The one unreactive thing in the server used to be exit detection: no
SIGCHLD handler existed, and exit was inferred from EOF plus the 5 s
poll. The supervisor added a `SignalKind::child()` handler
(`lib.rs:6458-6473`), same shape as the SIGTERM/SIGINT handler at
`lib.rs:6508-6517` (`tokio` is already `features = ["full"]`).

**It must not call `reap_zombies` more often.** That function used to
drain `waitpid(-1, WNOHANG)` and _discard_ the status of any pid
outside `pty_pids()` (`pty_unix.rs:469-476`). The discard was
deliberate — a foreign child like an LSP backend is reaped by its own
engine — but it stole statuses from anything the server spawns via
`Command` and waits on itself. The audio pipeline lived with that race
at 5 s; per-SIGCHLD would have widened it sharply, and this RFC adds
periodic `ExecHealthCheck=` children into its path.

So the handler only **wakes the supervisor**, which reaps by targeted
`waitpid(pid, WNOHANG)` over pids it owns — PTY children plus the
helpers of `ExecStartPre=`, `ExecStop=`, `ExecHealthCheck=` — all
registered through `register_pty_pid`/`pty_pids()`
(`pty_unix.rs:499-507`), which the unit layer generalizes to owned
children. **The global `waitpid(-1)` drain is gone**, kept only for
the PID-1 case where nobody else can reap
(`adopts_orphans`, `pty_unix.rs:456-458`).
`Command`-owned children keep being reaped by their owners, and no
status is collected by a party that did not spawn it.

Windows keeps a poll: no SIGCHLD, and `reap_zombies` is already a
no-op (`pty/pty_windows.rs:177`).

### Deadlines and leases

Four independent causes, with an enforced minimum. **The explicit,
`runtime_max` and `stop_escalation` causes shipped in #204**
(`enforce_deadlines`, `lib.rs:6188-6205`); leases did not.

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

A lease is a reconnect token: sessions created while a connection
holds one are tagged with it, and on disconnect each live leased
session gets a `grace_ms` deadline **in the lease cause only**. A
reclaim clears that cause and nothing else.

`C2S_LEASE [0x1F][op:1][grace_ms:4][lease_id:8]` →
`S2C_LEASE [0x12][status:1][lease_id:8][epoch:4]`. Both moved twice
since this was written: #204 took S2C `0x10` for `S2C_CREATE_FAILED`,
and #260 took C2S `0x1E` / S2C `0x11` for `C2S_SCROLL_BY` /
`S2C_SCROLL_OFFSET`. `0x1F` is the last free core C2S opcode. One byte of
operation, not a flag set, because the three are mutually exclusive:

| `op` | Name      | `lease_id` in | Effect                                                                                                                   |
| ---- | --------- | ------------- | ------------------------------------------------------------------------------------------------------------------------ |
| 0    | `CREATE`  | must be 0     | Mint a lease, hold it on this connection, epoch 1. Replies with the new id.                                              |
| 1    | `RECLAIM` | the lease     | Take the holder slot, bump `epoch`, replace stored `grace_ms`, disarm the lease cause on every session tagged with it.   |
| 2    | `RELEASE` | the lease     | Drop the holder slot **without** arming grace, and disarm the lease cause on its sessions. The explicit "I'm done" path. |

`status`: `0=ok, 1=unknown-lease, 2=invalid-op, 3=permission`. That
last one is the refusal path [Constraints](#constraints) requires;
there is no nonce because the reply carries the `lease_id`.

- **The server mints `lease_id`**, unguessably. A client-chosen `u64`
  is a namespace anyone on the socket can guess, for what is a kill
  switch on other people's sessions.
- **`epoch` increments on every reclaim.** A disconnect arms the lease
  deadline only if its epoch is current, so an old connection dying
  after a newer one reclaimed cannot revoke the newer claim.
- **One holder at a time.** A second reclaim supersedes the first,
  which is thereafter epoch-stale. No shared ownership, no holder
  count.
- **A lease is dropped** once it has no holder and no tagged session.
  Otherwise `CREATE` in a reconnect loop leaks table entries.

#### Escalation and attribution

On expiry: SIGTERM to the group, wait `TimeoutStopSec` (default 5 s;
systemd's 90 s is wrong for agent workloads), SIGKILL to the group.
**Shipped in #204** (`enforce_deadlines`, `lib.rs:6188-6205`).

`[reason:1]` is appended to `S2C_EXITED`
(`0=normal, 1=deadline, 2=lease, 3=gc, 4=unit-stop`) — a deadline kill
otherwise arrives as `-9`, indistinguishable from a user kill. Also
shipped in #204, with only `0` and `1` ever sent: `2` and `4` are the
reservations this RFC still has to fill, and `3` turned out to be dead
because retention eviction only touches a terminal that has already
sent its `EXITED`, and signals itself with `CLOSED`. When causes expire
together the reason is the one that produced the minimum, ties broken
in table order, so attribution is deterministic rather than whichever
timer the loop saw first. Append-only and length-gated, like the boot
generation in `S2C_HELLO`.

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

**`max_ptys`** counts live sessions only, and the silent `continue`
goes. Standalone bug, worth fixing regardless.

**Shipped in #204**, with two changes from what this section proposed
and one gap left open. The refusal is
`S2C_CREATE_FAILED [0x10][nonce:2][status:1][detail:N]` —
`0x10` rather than `0x11`, and the common status registry rather than a
message-local `reason` byte, both to match what #167's `protocol.md` had
already allocated. And it is opt-in per request: a client sets
`CREATE2_WANT_STATUS` (bit 3) after seeing `FEATURE_CREATE_STATUS`
(HELLO bit 14), so a legacy client cannot mistake a refusal for PTY
zero. `max_ptys` kept its `0` default, since #188 had landed the env var
in the meantime and argued that unlimited is right — a client that can
open a terminal can already spend the machine from inside it. The gap:
`C2S_CREATE`, `C2S_CREATE_AT` and `C2S_CREATE_N` still hit a bare
`continue` (`lib.rs:13973-13975`, `:14078-14080`, `:14175-14177`), so
the silent drop survives on the three legacy opcodes. It is invisible
to a `CREATE2` client and does not block units, but it is the same bug.

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
uniqueness (`lib.rs:13935-13944`), so any client could create a session
tagged `api`. Unit names are validated and unique in the registry, and
a unit-owned PTY carries `unit: Option<UnitName>` set only by the
supervisor, never settable over the wire. A client PTY whose tag
collides with a unit name is left alone: not adopted, not refused,
never in `S2C_UNIT_LIST`.

A **`UnitName` is `[A-Za-z0-9_.-]{1,64}`**, ASCII, case-sensitive,
neither `.` nor `..`. No `/`, so a name can never traverse; the file
is exactly `<name>.unit` in a unit directory and the name is derived
from the filename, never from a key inside it. Two directories
offering the same name is a load error naming both paths, not a
silent shadow.

#### Generations

Every unit-owned PTY carries a `Generation` incremented on each
restart, and **every asynchronous event carries
`(pty_id, generation)`**. #204 added the `Pty::generation` field
(`lib.rs:588`) and bumps it on `C2S_RESTART`, so the counter exists;
what is missing is carrying it on events and dropping stale ones.

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
# ~/.config/blit/units/example.unit — every key, for reference.
# The grammar has no inline comments, so these sit on their own lines.
[Unit]
Description=Every supported key, for reference
Requires=postgres
After=postgres

[Service]
# simple | oneshot | notify | match | surface
Type=notify
# ReadyMatch= is Type=match only; the value runs to end of line
ReadyMatch=^Listening on
# ReadySurface= is Type=surface only
ReadySurface=chromium
# RemainAfterExit= is Type=oneshot only
RemainAfterExit=no
ExecStartPre=/usr/bin/mkdir -p /var/run/api
ExecStart=cargo run -p api
ExecStop=/usr/bin/api-ctl drain
WorkingDirectory=/home/pierre/src/api
Environment=RUST_LOG=info
# pty | pipe
Backing=pty
# NotifyAccess= is Type=notify only: main | all
NotifyAccess=main

# no | on-failure | always
Restart=on-failure
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
# defaults to yes when ExecHealthCheck= is set
ActiveWhenHealthy=yes

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
exhausts the limiter. That is `audio.rs:791-807`'s burst limiter and
`crates/cli/src/uplink.rs:60-104`'s backoff, generalized. Restarts respawn in place
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
- **`notify`** — blit sets `NOTIFY_SOCKET` to a fresh per-generation
  path in `build_child_env` (which already injects `BLIT_SOCK`,
  `WAYLAND_DISPLAY`, `PULSE_SERVER`) and listens on an
  `AF_UNIX SOCK_DGRAM` socket for `READY=1`, `STATUS=`, `WATCHDOG=1`.
  `crates/sd-notify/` already implements the client half in pure libc,
  so the listener is that code mirrored and blit ends up both speaking
  and understanding `sd_notify(3)`.

  A per-generation socket is not attribution: any process that can
  reach the path can send `READY=1`, and on this host that is anything
  running as the same user. So the socket sets `SO_PASSCRED` and every
  datagram is checked against the `SCM_CREDENTIALS` pid, with
  `NotifyAccess=` naming the policy — `main` (default) accepts only
  the unit's leader pid, `all` accepts any pid in the unit's process
  group. systemd's `exec` variant is omitted; blit has no
  `ExecStartPost` to scope it to. A datagram from outside the policy
  is dropped and logged, never silently honored.

  **`Type=notify` is Linux-only.** `crates/sd-notify` is already
  `#[cfg(target_os = "linux")]` with a no-op stub elsewhere, and
  `SCM_CREDENTIALS` has no portable equivalent — macOS gives peer
  credentials for `SOCK_STREAM` only, so a datagram readiness protocol
  there would be unauthenticated by construction. Better to reject the
  key than to ship readiness that anything on the box can forge.

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
  cannot be told apart: unit B's window would mark unit A ready.
  Precise attribution means plumbing `SO_PEERCRED` from the Wayland
  client socket into `Surface`, since surfaces are keyed by Wayland
  object id and no client credentials are recorded today (the `pid`
  bindings in `crates/compositor/src/imp.rs` are protocol ids). Until
  that exists, **two units declaring the same `ReadySurface=` is a
  load error**, which makes the ambiguity unrepresentable rather than
  best-effort. A surface from a non-unit client can still match; that
  residual is documented, not defended.

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
  (`audio.rs:461-472` onward), not the PTY spawner, each with
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

[#173](https://github.com/indent-com/blit/pull/173) has since merged a
fuller non-PTY process family (`PROCESS_*`, `0xC0`-`0xC6`) with byte
offsets, flow-control windows, `MERGE_STDERR`, and raw bytes that never
touch a terminal driver. `Backing=pipe` here is deliberately the
narrower thing — a unit whose output is still a scrollback a client can
render — but if the process family lands first, this key should be
built on it rather than on a second pipe spawner.

### Wire and CLI

New family in the `0xD0` block, gated on `FEATURE_UNITS`, bit 20.
Both moved: `0x90`-`0x94` and bit 11 went to #167's extensions, `0x95`
and bit 12 to its channels, `0xC0`-`0xC6` and bit 13 to #173's
processes. Bits 14-16 shipped with #204 (`CREATE_STATUS`, `KILL_MODE`,
`PTY_DEADLINE`), 17-19 with `SCROLL_BY`, `SURFACE_TOUCH` and
`SURFACE_TEXT_INPUT`. 20-31 are free; git holds `0xA0`-`0xBF`, so
`0xD0` is the first free block.

| Dir | Opcode | Name           | Layout                                              |
| --- | ------ | -------------- | --------------------------------------------------- |
| C2S | `0xD0` | `UNIT_LIST`    | `[nonce:2]`                                         |
| C2S | `0xD1` | `UNIT_START`   | `[nonce:2][name:N]`                                 |
| C2S | `0xD2` | `UNIT_STOP`    | `[nonce:2][name:N]`                                 |
| C2S | `0xD3` | `UNIT_RESTART` | `[nonce:2][name:N]`                                 |
| C2S | `0xD4` | `UNIT_RELOAD`  | `[nonce:2]`                                         |
| S2C | `0xD0` | `UNIT_LIST`    | `[nonce:2][count:2]`, then `count` **unit records** |
| S2C | `0xD1` | `UNIT_STATE`   | one **unit record**, pushed on every transition     |
| S2C | `0xD2` | `UNIT_DONE`    | `[nonce:2][status:1][detail_len:2][detail:N]`       |

`name` runs to end of frame, so `UNIT_START|STOP|RESTART` need no
length prefix. The wire is version-stable, so the record is pinned
now rather than described:

```
unit record:
  [name_len:1][name:name_len]   UnitName, ASCII
  [state:1]                     0 inactive 1 activating 2 active
                                3 deactivating 4 failed
  [health:1]                    0 not-configured 1 unknown 2 healthy 3 unhealthy
  [pty_id:2]                    0 = no current PTY (ids start at 1)
  [generation:8]                0 when pty_id is 0
  [restarts:4]                  restarts in the current StartLimitIntervalSec window
  [exit_kind:1]                 0 never exited, 1 exited, 2 signalled
  [exit_status:1]               exit code, or signal number, or 0
  [autostart:1]                 0 no, 1 yes
  [enabled:1]                   0 operator-stopped, 1 eligible
```

One record shape, one decoder, and every field a fixed width, so a
future field appends and old clients length-gate past it.

**Every request gets exactly one reply.** `UNIT_LIST` answers
`S2C_UNIT_LIST` on success; everything else answers `S2C_UNIT_DONE`,
including the [Constraints](#constraints) refusal when the family is
disabled:

| `status` | Meaning                                                  |
| -------- | -------------------------------------------------------- |
| 0        | `ok`                                                     |
| 1        | `not-found` — no such unit                               |
| 2        | `invalid` — malformed name or frame                      |
| 3        | `permission` — `BLIT_UNITS=0`, the refusal path          |
| 4        | `unloadable` — the unit's file exists but fails to parse |
| 5        | `dependency-failed` — a `Requires=` never became active  |
| 6        | `timeout` — `TimeoutStartSec`/`TimeoutStopSec` elapsed   |
| 7        | `conflict` — another operation on this unit is in flight |
| 8        | `internal`                                               |

`detail` is UTF-8, capped at 1 KiB, and is what the CLI prints: the
parse error with file and line, or the name of the dependency that
failed.

`UNIT_DONE` fires on the **terminal outcome of the command**, not on
acceptance — `blit unit start api` should block and exit nonzero when
`api` cannot come up, which is only knowable after readiness resolves.
It is bounded by the gating timeouts (`TimeoutStartSec` for the unit
plus each dependency it waits on), so it always arrives. Progress in
between is visible as `UNIT_STATE` pushes; `blit unit start --no-block`
returns after status 0 or a rejection and leaves the rest to those.

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

The supervisor reaches the store **in process**, through the
`OnceLock<Mutex<Store>>` at `crates/server/src/kv.rs:557-560`, not over
the wire. `BLIT_KV=0` withholds the feature bit and refuses every
`KV_*` message at dispatch (`lib.rs:13581-13586`); it is a control
on the client-facing surface and does not disable units, which have no
client-facing KV surface to withhold. What does defeat persistence is
having no resolvable state dir, where the store is memory-only
(`kv.rs:531`); then operator intent is lost across a restart, and
that is logged once at load rather than discovered when a stopped unit
comes back.

Because autostart is meaningless without it — a `blit unit stop` that
un-stops itself at the next restart is a broken contract, not a
missing feature — **operator intent persistence and the `blit/`
reservation ship in the same PR as autostart**, not later.

### Support matrix

| Capability                          | Unix                 | Windows                                    |
| ----------------------------------- | -------------------- | ------------------------------------------ |
| Unit files, autostart, dependencies | yes                  | yes                                        |
| `Restart=`, backoff, start limits   | yes                  | yes                                        |
| `Type=simple`/`oneshot`/`match`     | yes                  | yes                                        |
| Health checks, `ActiveWhenHealthy=` | yes                  | yes                                        |
| Deadlines, leases, GC               | yes                  | yes                                        |
| `KillMode=process` (explicit)       | yes                  | yes (`TerminateProcess`)                   |
| `KillMode=process-group` (default)  | yes                  | yes, as a Job Object                       |
| `KillMode=control-group`            | Linux only           | no                                         |
| `Type=notify`, `WatchdogSec=`       | Linux only           | no (`sd_notify` is a Unix datagram socket) |
| `Type=surface`                      | Linux only (Wayland) | no                                         |
| `Backing=pipe`                      | yes                  | yes (no `setsid`/`setpgid` either way)     |
| Reactive child death (SIGCHLD)      | yes                  | no — keeps a poll                          |

**An unavailable capability is a load error, not a warning** — but
only for a key actually written in the file. `Type=notify` degrading
to `simple` would not merely lose a feature: it breaks the promise
that `After=` waits for real readiness, so every dependent starts
early on the platform where nobody is watching. Same for an explicit
`KillMode=control-group` quietly becoming `process`. Portability comes
from writing portable units, not from the server pretending — which is
also why every default has to be honorable on every platform, and why
`process-group` gets a Job Object implementation rather than a
platform-specific default.

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

One axis per PR. **1-3 shipped as #204**, tracked as
[#181](https://github.com/indent-com/blit/issues/181) rather than
gated on this RFC — the argument that they are independently valuable
held up, and the unit layer below is unchanged by how they landed.

1. **Group kill.** ✅ #204. `KillMode` as an appended flag byte on
   `C2S_KILL`; `C2S_CLOSE`'s SIGHUP to the group; the Windows Job
   Object so the new default is honorable everywhere. Pure semantics,
   no new state.
2. **Supervisor loop, deadlines, leases.** Mostly #204: the reactive
   loop, the SIGCHLD handler replacing the `waitpid(-1)` drain,
   `C2S_DEADLINE`, `CREATE2_HAS_DEADLINE` (bit 4), the `S2C_EXITED`
   reason byte, and the timed half of the `C2S_CLOSE` escalation all
   shipped. That last one landed differently from what this RFC
   proposed: rather than holding the entry in a "closing" state, the
   pid is handed to `abandon_pty_pid` with a SIGKILL deadline and the
   slot is removed immediately, so `CLOSED` still means gone and
   `--max-ptys` and `evict_exited` do not have to reason about a third
   kind of entry. **Still open:** `C2S_LEASE`/`S2C_LEASE`.
3. **GC.** ✅ #204. `exited_at`, count and time bounds
   (`BLIT_MAX_EXITED`, default 1024; `BLIT_EXITED_LINGER`, off),
   `max_ptys` counting live sessions only, `S2C_CREATE_FAILED`. **Still
   open:** the refusal only reaches `C2S_CREATE2`; the three legacy
   create opcodes still drop a capped create on the floor.
4. **Unit core.** Registry and generations, the strict parser, the
   state machine, autostart, restart policy, `Requires=`/`After=`,
   `Type=simple`/`oneshot`/`notify`, start and stop timeouts, helper
   children, the `0xD0` family, the CLI, and — with autostart, not
   after it — operator-intent persistence plus the reserved `blit/`
   KV prefix. PTY backing only.
5. **Unit policy.** Health checks with thresholds and
   `ActiveWhenHealthy=`, `WatchdogSec=`, `Type=match`, `Type=surface`,
   `Backing=pipe`, live reload.

1-3 are independently valuable, are the incident-shaped fixes, and
should be exercised before the unit layer commits to timer provenance,
reconnect ownership, and generation handling. Splitting 4 from 5 keeps
the first unit implementation's race surface small: the generation and
ownership contracts get exercised by a PTY-only, probe-free core
before health probes, a second spawn path, and live reload land on
them.
