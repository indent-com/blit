# `session`

A durable list of GUI applications the session brings up and keeps up. Enable an
application once and it starts with the session and comes back when it dies.

```bash
blit ext run --persist --restart always session extensions/dist/session.wasm
blit @session list
blit @session enable legcord
blit @session status legcord
blit @session disable legcord

# This session only, leaving intent alone
blit @session start legcord
blit @session stop legcord

# Off the list entirely
blit @session forget legcord
```

`enable`/`disable` are intent — what the next session start does.
`start`/`stop` are now. Trying an application is not the same as adopting it,
and stopping one for a minute is not the same as never wanting it again; one
button for both makes those indistinguishable.

`disable` keeps the row, because an application that just failed is worth
being able to look at and its failure count is the only record of that.
`forget` drops it: the stored intent is deleted rather than written "off", and
what is left is an installed application like any other.

`--persist` requires the operator to have started the server with
`--allow-persistent-extensions`, which is also what makes the intent outlive a
restart.

## How it starts an application

Three server features do the work, none of which the extension could fake:

- **`ENV_GET`** answers with the server's environment, which is the only way a
  Wasm guest can learn `XDG_DATA_DIRS` and so find installed applications at all.
- **`APP_SOCKET`** mints a Wayland socket dedicated to one application and tells
  the compositor that everything arriving on it belongs to that application. The
  socket is bound before the reply is sent, so the application can be spawned the
  moment it lands.
- **`PROCESS_SPAWN`** with `SESSION_ENV | DETACHABLE` supplies the desktop bus,
  audio sockets, and toolkit steering. An explicit `WAYLAND_DISPLAY` entry —
  which wins over the session's own — points the application at its stamped
  socket.

Desktop entries are read with a single shell child rather than the fs family,
which is built around sync sessions for an editor watching a tree; that is the
wrong shape for reading a fixed set of files once at startup.

## Why `status` can be trusted

`windows` is counted from the stamped identity the compositor reports
(`SURFACE_ORIGIN`), not from `xdg_toplevel.set_app_id`. The difference is not
academic: a Chromium launched under this extension reports a self-asserted
`app_id` of `claude-desktop` for a window showing `about:blank`. Anything built
on `app_id` matching files that window under the wrong application; the stamped
identity does not, because the application never gets to speak it.

## The browser panel

`blit.session.v1` publishes the whole state as JSON (one `{"type":"state"}`
object per change, with `catalog` on a greeting or a `resync`) and takes one
bare text line back: `enable`, `disable`, `start`, `stop`, `forget`,
`resync`, each followed by a desktop-entry id. `js/ui/src/session.ts` mirrors it, and the
**Applications** tab of an expanded remote is what a viewer sees.

## Restarting, and not restarting

Backoff mirrors the server's own extension supervisor — 250 ms base, 30 s cap,
full jitter, and a run that lasts 60 s forgives the failure history. Jitter
matters because a session starts several applications at once, so a shared cause
(a GPU reset, a compositor restart) would otherwise have them all retry in
lockstep forever.

Children are spawned `DETACHABLE`, so they outlive a restart of this extension.
A `process_ref` is only meaningful within the `boot_generation` it was recorded
under: a different one means the server restarted and every child went with it,
so the supervisor starts clean rather than adopting handles that no longer refer
to anything.

## Testing

The parts worth testing need no server: `cargo test --manifest-path
extensions/Cargo.toml` covers desktop-entry field codes and quoting, and the
backoff and failure-count rules. Everything else is protocol plumbing best
exercised against a real server.
