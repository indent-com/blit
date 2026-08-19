# Running systemd user units inside a blit session

A blit session builds its own desktop from parts: a compositor, a private D-Bus
session ([`crates/server/src/desktop_bus.rs`](../crates/server/src/desktop_bus.rs)),
PipeWire, a portal frontend, and the `session` extension to keep applications up.
What it has no notion of is a **systemd user manager** — so `systemctl --user
start syncthing.service` inside a blit pane either reaches the host's manager
(on a desktop) or nothing at all (in a sandbox image, where no login ever
happened and `user@1000.service` was never started).

You can have one without patching anything: run `systemd --user` _as_ a
supervised session application. This document describes that setup using only
the shipped `session` extension, an ordinary desktop entry, and a wrapper
script. Nothing here requires logind, PAM, lingering, a system systemd, or
changes to blit.

What you get: user units start, stop, restart, socket-activate and order
themselves normally; `systemctl --user` and `busctl --user` work; anything a
unit launches lands on blit's compositor, blit's bus and blit's audio. What you
do not get: `loginctl`, seats, `enable-linger`, and journald (see
[Limits](#limits)).

## How the pieces fit

The `session` extension supervises **desktop entries**, not commands: its
catalog is every `*.desktop` directly under `$XDG_DATA_HOME/applications` and
each `$XDG_DATA_DIRS/*/applications`, read from _the server's_ environment.
Each enabled entry gets a Wayland socket stamped with its identity, and is
spawned with `PROCESS_SPAWN_SESSION_ENV | PROCESS_SPAWN_DETACHABLE` plus an
explicit `WAYLAND_DISPLAY` pointing at that stamped socket
([`extensions/session/src/main.rs`](../extensions/session/src/main.rs)).

So the user manager is described by a desktop entry, inherits the whole session
environment by exec, and every unit it starts inherits it in turn. No
`systemctl --user import-environment` step is needed — the manager already has
`WAYLAND_DISPLAY`, `DISPLAY`, `PULSE_SERVER` and the bus address when it starts.

## 1. The wrapper script

`systemd --user` cannot be started bare, for four reasons the script handles:
a private runtime directory, the stamped socket's name, the bus, and the units
blit already provides. Install it anywhere on the server's `PATH`, e.g.
`/usr/local/bin/blit-user-manager`:

```sh
#!/bin/sh
# Start a systemd user manager inside the blit session.
set -e
SYSTEMD=${SYSTEMD_BIN:-/usr/lib/systemd/systemd}
RT=/run/user/$(id -u)/blit-session          # short: AF_UNIX paths are 108 bytes
U=$RT/units
mkdir -p "$RT" "$U"; chmod 700 "$RT"

# The stamped Wayland socket is a bare name under the session runtime dir, so
# resolve it before we move XDG_RUNTIME_DIR out from under it.
case "$WAYLAND_DISPLAY" in
  /*) ;;
  "") ;;
  *) WAYLAND_DISPLAY="$XDG_RUNTIME_DIR/$WAYLAND_DISPLAY" ;;
esac
export WAYLAND_DISPLAY

# One bus, blit's: point $RT/bus at it and stop systemd starting a second one.
BUS=$DBUS_SESSION_BUS_ADDRESS
case "$BUS" in unix:path=*) p=${BUS#unix:path=}; ln -sf "${p%%,*}" "$RT/bus" ;; esac
ln -sf /dev/null "$U/dbus.socket"
ln -sf /dev/null "$U/dbus-broker.service"
# Anything blit already provides.
for m in pipewire.socket pipewire.service pipewire-pulse.socket pipewire-pulse.service \
         wireplumber.service xdg-desktop-portal.service xdg-desktop-portal-gtk.service; do
  ln -sf /dev/null "$U/$m"
done

export XDG_RUNTIME_DIR="$RT"
export SYSTEMD_UNIT_PATH="$U:"
unset DBUS_SESSION_BUS_ADDRESS
"$SYSTEMD" --user --unit=basic.target &
mgr=$!
trap 'kill -TERM $mgr 2>/dev/null' TERM INT
n=0
while [ ! -S "$RT/systemd/private" ] && [ $n -lt 100 ]; do n=$((n+1)); sleep 0.1; done
[ -n "$BUS" ] && systemctl --user set-environment DBUS_SESSION_BUS_ADDRESS="$BUS" || true
wait $mgr
```

Why each part is there:

- **A private `XDG_RUNTIME_DIR`.** On a host that already has a user manager,
  `/run/user/$UID/systemd` is taken; a second manager needs its own directory.
  Keep the path short: the manager's notify socket is `AF_UNIX`, so a long path
  fails with _"Notify socket … not valid for AF_UNIX socket address, refusing"_.
  In a sandbox with no other manager you may drop this and let it own
  `/run/user/$UID` directly, which makes `systemctl --user` work in every pane
  with no environment fiddling at all.
- **Resolving `WAYLAND_DISPLAY`.** The stamped socket is a _file_ named
  `blit-app-<id>-<instance>` in the session's runtime directory, and the
  extension passes its bare name. Once `XDG_RUNTIME_DIR` moves, that name no
  longer resolves — hence the rewrite to an absolute path, which libwayland
  accepts.
- **The bus.** Left alone, the manager starts `dbus.socket`/`dbus-broker` and
  exports `DBUS_SESSION_BUS_ADDRESS=$XDG_RUNTIME_DIR/bus`, giving the session a
  _second_ bus beside blit's. Masking those two units alone drops the variable
  from every unit's environment, so the address has to be handed back with
  `set-environment`. Note the address blit exports carries a `,guid=…` suffix
  that must be stripped before it is used as a path.
- **`--unit=basic.target`**, not `default.target`: the latter pulls in whatever
  the image's `default.target.wants` holds, including second copies of what blit
  runs itself.
- **`SYSTEMD_UNIT_PATH="$U:"`** — the trailing colon _appends_ the real search
  path. Without it nothing resolves, not even `basic.target`.
- **`wait $mgr`** propagates the manager's exit status, which is what the
  supervisor's restart policy reads (see [Supervision](#supervision)).

## 2. The desktop entry

Write `$XDG_DATA_HOME/applications/blit-user-manager.desktop` — where
`XDG_DATA_HOME` is read from _the server's_ environment, not your shell's:

```ini
[Desktop Entry]
Type=Application
Name=User service manager
Exec=/usr/local/bin/blit-user-manager
```

The parser
([`extensions/session/src/desktop_entry.rs`](../extensions/session/src/desktop_entry.rs))
is deliberately small, and its rules decide whether the entry is offered at all:

- The id is the **basename without `.desktop`** — `blit-user-manager` here — and
  that is what `@session enable` takes.
- `NoDisplay=true`, `Hidden=true` and `Terminal=true` entries are **skipped**.
  A user manager is none of those; do not add them to keep it out of a launcher.
- Only the `[Desktop Entry]` group is read, and only bare keys (`Name[fr]` is
  ignored).
- `Exec` is required, quoting follows the spec, and **field codes are dropped**:
  a literal `%` in the command line must be written `%%`. Prefer a script over
  an inline `sh -c` for anything with quoting in it.
- Directories are scanned non-recursively, earlier roots winning.

## 3. Enable it

```bash
blit ext run --name session --persist --restart always session.wasm
blit @session list                      # blit-user-manager  no  -  User service manager
blit @session enable blit-user-manager
blit @session status blit-user-manager
```

`--persist` requires a server that permits persistent extensions, which is the
default (`--no-persistent-extensions` turns it off); that is also what makes the
intent survive a restart. `enable` is durable — it is stored under `ext/session/app/<id>` in the
server's KV store and replayed at startup, so the manager comes up with the
session from then on.

`status` is the oracle that it worked:

```
app	blit-user-manager
enabled	yes
phase	running
failures	0
socket	blit-app-blit-user-manager-f6435af511bf9db8
windows	1
```

`windows` counts surfaces on the stamped socket, so a GUI application started by
one of your _units_ is counted here — which is the end-to-end proof that units
land on blit's compositor.

## 4. Day-to-day

Every `systemctl --user` invocation needs the manager's runtime directory:

```bash
alias uctl='XDG_RUNTIME_DIR=/run/user/$(id -u)/blit-session systemctl --user'
uctl start syncthing.service
uctl status
```

Drop unit files into `$XDG_RUNTIME_DIR/blit-session/units/` (the override
directory the script creates and masks into) or the usual
`~/.config/systemd/user`. Two things bite here:

- **`ExecStart` must be an absolute path.** A bare `alacritty` fails with
  `203/EXEC`.
- **The manager's `PATH` is the blit server's `PATH`.** A server started from a
  systemd unit has almost none; check with `systemctl show <unit> -p Environment`
  before assuming a tool is reachable.

## Supervision

The supervisor's rules
([`extensions/session/src/supervisor.rs`](../extensions/session/src/supervisor.rs))
apply to the manager like any other application:

- **A clean exit is never retried.** `systemctl --user exit` (or the `TERM` the
  extension sends on `@session disable`) exits 0, so the manager stops and stays
  stopped — no respawn loop. Bring it back with `@session enable`.
- **A crash backs off**: 250 ms base, doubling, 30 s cap, full jitter; a run that
  lasts 60 s forgives the failure history.
- **Each attempt mints a fresh stamped socket**, so `WAYLAND_DISPLAY` changes
  across a restart. Units inherit it at _their_ start, so units already running
  when the manager dies die with it, and units started after a restart get the
  new socket. Nothing caches a stale display.
- Children are `DETACHABLE`: they survive a restart of the `session` extension
  itself, and re-adoption is by `process_ref` within one `boot_generation`.

## Limits

- **No logind.** No `loginctl`, no seats, no `XDG_SESSION_ID`, no lingering, and
  no `systemd-run --user --scope` niceties that depend on a session. Units that
  hard-require them will not start. This needs a system systemd at PID 1, which
  is a different (VM-backed) design.
- **No journald in a sandbox image.** With no system journal to connect to,
  give units `StandardOutput=append:%t/<name>.log` or they log nowhere useful.
- **No cgroup boundary of its own.** The manager creates its unit cgroups inside
  whatever delegated subtree it happens to land in. That works, but resource
  limits set on the blit server apply to everything the manager starts.
- **System units are out of scope** — this manages `--user` units only.

## Verified

Measured on systemd 261, blit `main`, `session.wasm`
`10da0ab8c2490a99340dc7ec99c1aabf1e02f3d1eb7dc672b3758c84e818ae9a`: the manager
reaches `basic.target` in under a second, `systemctl --user is-system-running`
answers `running`, `busctl --user` lists blit's own bus connections, and an
`alacritty` started by a unit shows up as `windows 1` on the supervised
application's stamped socket.
