# `muster`

Supervise units that run in terminals. `muster` reads
`~/.config/blit/instances/NAME/muster/` (`NAME` defaults to `default`), starts
what it finds in dependency order, restarts what crashes or what you edit, and
journals every decision. A supervised unit is an ordinary blit PTY, so
_supervised_ and _attachable_ are the same thing. `BLIT_MUSTER_DIR` remains an
explicit override.

```bash
blit ext run --persist --restart always muster extensions/dist/muster.wasm

blit @muster list                      # every unit and instance
blit @muster status epic/gateway       # one unit, with its retained runs
blit @muster start|stop|restart NAME   # a unit, or a whole instance
blit @muster instantiate blit epic \
     PORTS=auto                         # a stack instance, running
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
says so. A top-level file is a unit unless `stack`, `include`, or `worktrees`
selects an instance, an included unit directory, or a dynamic worktree source.
Leading `.` is ignored, and nothing below the second level is read.

```
~/.config/blit/instances/default/muster/
  postgres.json          a unit
  blit/                  a stack of templates
    stack.json             its parameter declarations
    server.json
  main.json              an instance → main/server, main/gateway
  epic.json              another     → epic/server, epic/gateway
```

A unit needs exactly one of `command` (an argv, exec'd directly) or `shell` (a
line for the server's **login** shell — fish, where `$SHELL` is fish). Both is
refused, and so is neither.

## Definitions that live somewhere else

A `stack` or an `include` may name a directory anywhere, so a stack can live in
the repository it starts and the configuration directory holds only a pointer.
A bare word is a subdirectory; anything with a `/` or a leading `~` is a path.

```json
// ~/.config/blit/instances/default/muster/epic.json — a worktree stack
{ "stack": "/src/blit/.claude/worktrees/epic/.blit/muster",
  "vars": { "PORTS": 10010 } }

// ~/.config/blit/instances/default/muster/work.json — ordinary units
{ "include": "~/work/units" }
```

The two differ in naming, which is the whole reason both exist. An **instance**
qualifies its templates as `<instance>/<template>` — `epic/server` — which is
what you want when the same stack runs once per worktree, and which sorts every
unit of an instance together. An **include** does not: its units keep their own
names, as though the files had been dropped in the configuration directory. Two includes offering
one name is therefore an error rather than a merge — `doctor` names both files,
and `omit` resolves it. An included directory holds units only; its
subdirectories are not stacks, because an instance names a stack by path.

Inside a stack, `${STACK_DIR}` is the stack's own directory, and a relative
`cwd` or `envFile` resolves against it. So a stack at `<repo>/.blit/muster/`
reaches its checkout with `"cwd": "../.."`; the stack location is the checkout
identity.

**Discovery never leaves the configuration directory.** Muster does not look for
`.blit/muster` in a repository, a cwd, or any ancestor of one: cloning a
repository and starting a server must not run its code. The pointer file is a
deliberate act — and the same act already grants arbitrary execution, so this
adds reach rather than privilege. What it does add is that **a branch switch can
change what a template says**, since the file is written by `git checkout`
rather than by you. `restartOnChange` is on by default, so that edit takes
effect the way any other does.

Each distinct external directory costs one `FS_SYNC`, shared between pointers
that name the same one, and dropped when the last pointer goes away.

### One stack per Git worktree

A worktree source is one deliberate pointer to a repository's main checkout
and a stack path inside it:

```json
{
  "worktrees": "/src/blit",
  "stack": ".blit/muster",
  "vars": { "PORTS": "auto" }
}
```

The source file's name is the main instance name. If this is `blit.json`, the
main units are `blit/server` and so on; a linked worktree whose Git
administrative id is `epic` becomes `blit-epic/server`. Muster watches only
Git's `worktrees/*/gitdir` pointers and each selected stack directory, not the
repository or object database.

A port parameter used this way declares both its span and its first block:

```json
{ "kind": "ports", "span": 4, "start": 10000 }
```

The main worktree receives `start` exactly. Linked worktrees receive the first
free block above it; the concrete leases are durable in kv, so inserting or
removing another worktree does not move them. A removed worktree keeps its
reservation and receives it again if it returns.

### Nothing needs telling — except a watch that was refused

There is no "re-read the directory" verb, because there is nothing to ask for:
muster watches every directory it reads, and an edit is loaded before you could
type a command about it.

The one exception is a directory whose `FS_SYNC` the server refused — usually a
pointer written before its target exists, which is the ordinary order of events
when a stack lives in a worktree you are about to create. That cannot arrive by
itself, because nothing watches a directory that is not being watched, so it is
the only thing here that polls: retried after 5s, doubling to a minute, reset
whenever any watch succeeds. `blit @muster rewatch` forces it now, and says what
happened:

```
$ blit @muster doctor
/src/blit/wt/epic/.blit/muster   cannot watch this directory (status 1)
$ git worktree add /src/blit/wt/epic
$ blit @muster rewatch
/src/blit/wt/epic/.blit/muster   watched
```

With nothing broken it says so, rather than reporting work it did not need to
do. It is its own verb rather than an argument-less `reload` because retrying a
watch and reloading a unit share no subject and no effect, and one word for both
would put the wrong one a forgotten argument away.

## What starts what

`requires` is hard and implies ordering: the dependency must be ready first,
and the dependent stops when it leaves ready. `wants` starts something without
waiting for it. `after` orders without starting anything. Cycles are refused at
load with every member named.

Ready means `readyWhen`: `spawn` (the fork worked), `{"delay":"2s"}`,
`{"path":"/tmp/x.sock"}`, `{"log":"listening on"}`, `{"tcp":"127.0.0.1:5432"}`,
`{"http":"http://127.0.0.1:10001/"}`, or `manual`. A `oneshot` is ready when it
exits 0, so successful completion is both its health and its readiness signal.

Re-running a successful `oneshot` is staged. Its dependents keep using the last
successful result while the new run is in progress or failed; they restart only
when a later run exits 0. An initial run still gates dependents normally, and an
explicit `stop` still takes the dependency tree down immediately.

## Restarting, and never in place

`restartOnFailure`, `restartOnAbnormal` and `restartOnChange` default on;
`restartOnSuccess` does not, because a process that exits 0 usually meant it —
and the blit dev server exits 0 on purpose when it is replaced, so retrying that
is an infinite loop.

Retries start with a one-second jittered backoff, capped at 30 seconds. After
five consecutive failures a unit stays `failed`; set `startLimit` to `0` for no
limit. A failed `oneshot` is never retried automatically: it is a task with a
result, and can be started again explicitly after the failure is fixed.

`restartOnAbnormal` is separate from `restartOnFailure` because they answer
different questions. A process that returns 1 has decided something; a process
the OOM killer took has not. `"restartOnFailure": false` means "obey what it
says", and it should not also mean "and stay down when it is shot" — so the two
are independent, and either is enough. Blit's exit status negates the
terminating signal, which is how a kill is told from a return.

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

## Stopping and reloading with a command

Some programs are a handle on something else — `docker compose up`, a tunnel, a
device — and signalling the handle leaves the thing it opened running.
`stopCommand` runs instead of the signal:

```json
{
  "command": ["docker", "compose", "up"],
  "stopCommand": ["docker", "compose", "down"],
  "timeoutStop": "60s"
}
```

It replaces the signal, not the deadline: `SIGKILL` still arrives at
`timeoutStop`, because a stop command that does not stop the unit is exactly the
case it exists to survive.

`reloadCommand` is the same shape for `blit @muster reload <name>`. A unit with
no `reloadCommand` is **restarted** instead, and the answer says which happened
per unit:

```
$ blit @muster reload epic
epic/gateway   reloaded
epic/ui        restarted
```

Both run in a terminal of their own, tagged `muster/<unit>/stop|reload`, with
the unit's `cwd` and resolved environment — a stop command that cannot see
`DOCKER_HOST` talks to a different machine than the one it is stopping. Neither
is a run of the unit: no sequence number, never adopted, not retained.

## Environment, and the `PATH` that will bite you

Precedence ascends: what the server derives, then each `envFile` in order, then
`env`. Files are read at **every start**, so editing `.env` and restarting is
enough. The merged map travels in `CREATE2`'s environment block and reaches
`execve` as `envp` — never a command line, so an `envFile` secret is not in
`ps`, not in `/proc/<pid>/cmdline`, and not on disk.

**A `command` unit runs no rc file, so `PATH` is the server's.** Under a server
started from a systemd unit that is often coreutils, findutils, grep and sed —
no `cargo`, no `pnpm`, no `node`. The server resolves `command[0]` against the
child's _own_ environment, so the fix is one shared env file:

```sh
# ~/.config/blit/instances/default/muster/path.env
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
{ "stack": "blit", "vars": { "PORTS": 10000 } }
```

Dependencies inside a stack name templates unqualified and always resolve within
the same instance. `"omit": ["website"]` drops one; anything requiring an
omitted template fails to load, by name. `"autostart": false` holds the whole
instance.

Declaring a parameter `{"kind":"ports","span":4,"start":10000}` lets `auto`
allocate the first instance and lets `doctor` report two instances whose blocks
overlap — the failure mode of several dev stacks, which otherwise presents as
`EADDRINUSE` in whichever one lost.

### One command for a manual instance

```bash
blit @muster instantiate blit epic PORTS=auto
```

That writes the instance file and starts it, in one command, because those were
never two decisions: an instance file with `autostart` — the default — _is_ the
start. What comes back is the units and the phase each is actually in, not what
they were asked to do, because the write goes into the mirror and the load and
reconcile happen before the answer is sent.

`VALUE` is typed the way a JSON file would type it: a number is a number, `true`
is a boolean, and everything else is the text you typed, so paths need no
quoting. `PORTS=auto` takes the lowest free block for a parameter declared
`{"kind":"ports"}` — free against **every** instance's block, not just this
stack's. Its first instance uses the declaration's `start`; without one, the
first instance still has to say a number rather than guessing a machine-wide
range.

Nothing is written if the instance would not load — the expansion runs first,
against the same code that will run it for real, so a forgotten parameter is an
error on your terminal rather than a finding in `doctor` about a file you never
typed. An existing name is refused unless `--force`; `--no-start` writes
`"autostart": false` for someone who wants to read it before it runs.

Only the configuration directory is writable, and only at its top level. A stack
directory outside it is a repository muster was pointed at, and the rule that
keeps discovery out of it keeps writes out of it too.

## The panel, and the channel under it

`muster` publishes `blit.muster.v1`, and the browser's Manage pane grows a
**Muster** tab while something is listening on it (`js/ui/src/MusterPanel.tsx`,
mirrored by `js/ui/src/muster.ts`). The tab shows the tree the CLI cannot:
instance ▸ unit ▸ (terminal, windows). A unit's windows are there because a run
is spawned onto its own stamped Wayland socket, so the compositor — not the
supervisor guessing at process trees — is what says which window is whose.

The wire is JSON, one object per message.

| server → panel                                         | meaning                                              |
| ------------------------------------------------------ | ---------------------------------------------------- |
| `{"type":"hello","version":1,"dir":…}`                 | which directory is being watched                     |
| `{"type":"state","units":[…],"gone":[…]}`              | these units, **whole**; those names no longer exist  |
| `{"type":"state","full":true,"units":…,"instances":…}` | the entire table, and the tree it hangs under        |
| `{"type":"events","records":[…]}`                      | journal records, as `@muster log --json` prints them |

The panel sends the CLI's verbs as bare lines: `start NAME`, `stop NAME`,
`restart NAME`, `rewatch`, `resync`. A name is a unit or an instance.

Two properties are worth stating, because they are what the design is for:

- **A unit arrives whole, never as a patch.** So a reader that missed a frame is
  correct after the next one, and no reconciliation code exists on either side.
- **A frame carries only what changed.** Transitions mark units dirty and a
  flush 80 ms later sends that set; a hundred-unit directory does not ship a
  hundred rows because one of them restarted. `full` marks the frames that
  redefine the whole table — a new reader's first, and after `resync` — and
  those are the only ones where an absent unit means a deleted unit.

Env-file **values** never appear on the channel, as they never appear in
`@muster status` or the journal. `@muster env --values` remains the only way to
read them.

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

For the rest, use a named private server, which gives the socket, extension
catalog, KV database, cache, and muster directory independent defaults:

```bash
blit server --name mus
blit --on local:mus ext run --persist --restart always \
     muster extensions/dist/muster.wasm
blit --on local:mus @muster list
```

Set `BLIT_MUSTER_DIR` on that server when the automatically namespaced
configuration directory is not the scratch location you want.

Without `--on local:mus` the CLI talks to its default target, which is usually
not the server under test.

The browser half is exercised by `e2e/tests/muster-panel.spec.ts`, which brings
its own units: `start-servers.sh` points the supervisor at an empty
`BLIT_MUSTER_DIR` of its own and publishes the path, so the spec never reads —
or starts — whatever you actually supervise.

## Not here yet

- A panel row names a unit's terminal and windows; it does not open them. That
  is frontend-only work — the pty is already in the client's session list, and
  what is missing is a way for a BSP tile to hand the workspace something to
  place, which is `ide/activeEditor.ts`'s registry pointed the other way.
- The durable journal tail in kv: the ring is in memory, so `@muster log` starts
  empty after the supervisor restarts.
- `@muster remove`, the other half of `instantiate`. Deleting an instance file
  needs `FS_OP`, and unlike a write it destroys something.
- Dependencies across stack boundaries. A self-contained stack has no ambiguity
  about which instance a name means; a shared database between per-worktree
  stacks wants a decision about migrations before it wants a `requires`.
- A restart caused by a file change is journaled with cause `crash` rather than
  `file`, because the retry runs through the backoff path.

## Deliberately not

- **`envFile` key subsets, or a `passEnv` list forwarding named server
  variables.** Both are ways to make what a unit sees depend on something other
  than its own file, and the current answer — the file says everything, and
  `@muster env --values` shows what it resolved to — is worth more than the
  convenience.
- **A stack fetched from a repository, pinned by digest like an extension
  module.** Discovery never leaves the configuration directory, on purpose:
  cloning a repository must not be able to start running its code. A stack you
  point at by path is you naming it; a stack muster fetches is not.
