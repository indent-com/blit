# `muster`

Supervise units that run in terminals. `muster` reads
`~/.config/blit/muster/`, starts what it finds in dependency order, restarts
what crashes or what you edit, and journals every decision. A supervised unit
is an ordinary blit PTY, so *supervised* and *attachable* are the same thing.

```bash
blit ext run --persist --restart always muster extensions/dist/muster.wasm

blit @muster list                      # every unit and instance
blit @muster status gateway@epic       # one unit, with its retained runs
blit @muster start|stop|restart NAME   # a unit, or a whole instance
blit @muster log -n 20                 # why something is not running
blit @muster doctor                    # everything wrong with the directory
blit @muster env api --values          # which .env file won
blit @muster schema > ~/.config/blit/muster.schema.json
```

The design and its reasoning are in
[`docs/design/muster.md`](../../docs/design/muster.md). This file is what you
need to run it.

## The directory

An entry's name is its basename without `.json`, unique because the filesystem
says so. A top-level file is a unit unless it has a `"stack"` field, in which
case it is an instance of the subdirectory it names. Leading `.` is ignored,
and nothing below the second level is read.

```
~/.config/blit/muster/
  postgres.json          a unit
  blit/                  a stack of templates
    stack.json             its parameter declarations
    server.json
  main.json              an instance → server@main
  epic.json              another     → server@epic
```

A unit needs exactly one of `command` (an argv, exec'd directly) or `shell` (a
line for the server's **login** shell — fish, where `$SHELL` is fish). Both is
refused, and so is neither.

## What starts what

`requires` is hard and implies ordering: the dependency must be ready first,
and the dependent stops when it leaves ready. `wants` starts something without
waiting for it. `after` orders without starting anything. Cycles are refused at
load with every member named.

Ready means `readyWhen`: `spawn` (the fork worked), `{"delay":"2s"}`,
`{"path":"/tmp/x.sock"}`, `{"log":"listening on"}`, `{"tcp":"127.0.0.1:5432"}`,
`{"http":"http://127.0.0.1:10001/"}`, or `manual`. A `oneshot` is ready when it
exits 0, which is how one keyword covers what `process-compose` spells
`process_completed_successfully` and `process_healthy`.

## Restarting, and never in place

`restartOnFailure` and `restartOnChange` default on; `restartOnSuccess` does
not, because a process that exits 0 usually meant it — and the blit dev server
exits 0 on purpose when it is replaced, so retrying that is an infinite loop.

Every restart is a **new terminal**. `C2S_RESTART` would keep the pane, but it
replays the spec the PTY was created with, so it cannot serve a restart caused
by an edit; using it only for crashes would make the two kinds behave
differently. Instead `keep` (default 1) retains that many exited terminals per
unit, so a crash loop leaves its last runs addressable rather than concatenated
into one pane:

```
$ blit @muster status crasher
unit       crasher
phase      backoff
failures   7
run        31   exit 1   seq 7
run        30   exit 1   seq 6
```

`blit terminal journal 30` then reads that run with no scrollback archaeology.

## Environment, and the `PATH` that will bite you

Precedence ascends: what the server derives, then each `envFile` in order, then
`env`. Files are read at **every start**, so editing `.env` and restarting is
enough. The merged map travels in `CREATE2`'s environment block and reaches
`execve` as `envp` — never a command line, so an `envFile` secret is not in
`ps`, not in `/proc/<pid>/cmdline`, and not on disk.

**A `command` unit runs no rc file, so `PATH` is the server's.** Under a server
started from a systemd unit that is often coreutils, findutils, grep and sed —
no `cargo`, no `pnpm`, no `node`. The server resolves `command[0]` against the
child's *own* environment, so the fix is one shared env file:

```sh
# ~/.config/blit/muster/path.env
PATH=/home/you/.nix-profile/bin:/run/current-system/sw/bin:/usr/bin:/bin
```

listed by every unit. `blit @muster doctor` resolves `command[0]` against the
effective `PATH` rather than the server's, so it tells you before a start does.

**A binary that does not resolve fails silently**: the terminal exists, exits 1,
and prints nothing. `@muster status` shows the run and `doctor` names the
program; the terminal itself will not.

## Stacks, once per worktree

A stack's `stack.json` declares parameters; an instance binds them. Inside a
template, `${NAME}`, `${NAME+N}` and `${NAME-N}` substitute in any string value,
never a key. `${INSTANCE}` and `${STACK}` are always defined. An unbound name
fails **that instance** with the file, pointer and variable named — there is no
empty-string fallback, because a parameter you forgot should not quietly
produce `http://127.0.0.1:/`.

`${` is the only trigger, so `$BLIT_DEV_SOCK` in a `shell` template is still the
shell's variable.

```json
{ "stack": "blit", "vars": { "ROOT": "/src/blit", "PORTS": 10000 } }
```

Dependencies inside a stack name templates unqualified and always resolve within
the same instance. `"omit": ["website"]` drops one; anything requiring an
omitted template fails to load, by name. `"autostart": false` holds the whole
instance.

Declaring a parameter `{"kind":"ports","span":4}` lets `doctor` report two
instances whose blocks overlap — the failure mode of several dev stacks, which
otherwise presents as `EADDRINUSE` in whichever one lost.

## Surviving its own replacement

`blit ext update muster …` does not kill the stack. The supervisor keeps the
`S2C_LIST` burst from bootstrap (`bootstrap_with_initial`; plain `bootstrap()`
discards it, and there is no request that asks again), and re-adopts every PTY
tagged `muster/<unit>/<seq>`. Per unit the highest sequence that has **not**
exited is the live run — the burst carries `S2C_EXITED` for the rest — and the
others become retained history.

Adoption re-runs only a `readyWhen` that describes the present: `path`, `tcp`,
`http`. `log`, `delay` and `spawn` describe a past event whose evidence may have
been evicted, so a live terminal is taken as the evidence instead. Re-running
one of those stalls a healthy unit until `timeoutStart` and then replaces it,
which is the restart storm adoption exists to prevent.

## Testing

`cargo test -p blit-ext-muster --lib` covers the parts worth getting right on
the host: unit-file parsing, substitution, dotenv merging, backoff, retention
and dependency order. Nothing there needs a server.

For the rest, point a private server at a scratch directory — `BLIT_MUSTER_DIR`
is read from the **server's** environment, since that is whose filesystem is
being watched:

```bash
blit server --socket /tmp/mus.sock --allow-persistent-extensions   # with
                                    # BLIT_MUSTER_DIR=/tmp/mus set on it
blit --on socket:/tmp/mus.sock ext run --persist --restart always \
     muster extensions/dist/muster.wasm
blit --on socket:/tmp/mus.sock @muster list
```

Without `--on socket:…` the CLI talks to whatever server it finds, which is
usually not the one under test.

## Not here yet

- The `blit.muster.v1` channel, and the browser panel that would read it.
- The durable journal tail in kv: the ring is in memory, so `@muster log` starts
  empty after the supervisor restarts.
- `@muster instantiate` and `remove`, which need `FS_WRITE`.
- A restart caused by a file change is journaled with cause `crash` rather than
  `file`, because the retry runs through the backoff path.
