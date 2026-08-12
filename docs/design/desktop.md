# RFC: Desktop Services — Tray Icons and Notifications (`DESKTOP_*`)

- **Status:** Draft
- **Date:** 2026-08-12
- **Companion to:** [../protocol.md](../protocol.md), [../frontend.md](../frontend.md),
  [fs-watch.md](fs-watch.md) (the state-not-events model this borrows),
  [git.md](git.md) (whole-snapshot state), [net.md](net.md) (the service worker)

## Summary

Slack running on a blit server today is a window with a title and nothing
else. Its tray icon — where the unread badge lives, where "Set yourself
away" lives, where closing the window leaves it — has nowhere to go,
because blit has no status area for it. Its notifications go nowhere at
all: no process on the private bus owns
`org.freedesktop.Notifications`, so the call fails and the message is
lost. The same is true of Element, Thunderbird, Steam, nm-applet, and
every `notify-send` in every script anyone runs in a blit pty.

Both gaps close against the same seam, and the seam already exists.
[`crates/server/src/desktop_bus.rs`](../../crates/server/src/desktop_bus.rs)
gives every compositor a **private D-Bus session** whose activation
environment points at blit's Wayland display. blit owns that bus. Nothing
else is on it. So blit can simply _be_ the desktop shell those apps are
looking for: claim `org.freedesktop.Notifications` and
`org.kde.StatusNotifierWatcher` before the first pty starts, implement
them, and publish what arrives to viewers.

This proposal adds one protocol family, `DESKTOP_*`, carrying three kinds
of thing:

- **Tray items** — StatusNotifierItem registrations: icon, status,
  tooltip, and a menu. Rendered as a cluster at the right of the status
  bar.
- **Notifications** — freedesktop notifications, plus a client-side
  posting API so agents and `blit notify` share the path. Rendered as an
  in-app toast and, when the tab is not looking, escalated to a real OS
  notification through the service worker.
- **Application icons** — for tray items, for notifications, and for
  ordinary toplevels, so that a surface chip in the status bar, the
  switcher, and a pane header can show what the window _is_ before
  reading what it is called.

**Everything here is state, not events**, which is the same bet
[fs-watch.md](fs-watch.md) made and for the same reason: viewers are
plural and come and go. A tray icon is obviously state. A notification
looks like an event, but the freedesktop protocol is create / replace by
id / close with a reason — a keyed map with expiry wearing an event's
clothes. Modelling it as state is what makes dismissing a Slack ping on
your phone clear it from the laptop, and what makes a tab reconnecting
after a nap see the four things it missed instead of nothing.

Unlike the filesystem, this state is _small_ — tens of records, hundreds
of bytes each — so it needs no diff engine, no ack window, and no
retention budget. The whole snapshot goes out on every change, coalesced,
the way [git.md](git.md) ships `GIT_STATE`. The only thing pulled on
demand is bulk: icon bytes (content-addressed, fetched once per hash) and
menu layouts (fetched when a menu is opened).

## Goals

- Slack, Element, Discord, Steam, Thunderbird, nm-applet and anything
  else built on libappindicator / `Gio.Notification` / KStatusNotifierItem
  work on a blit server with no per-app configuration.
- `notify-send` works in any pty, because the service it calls is
  finally there.
- One coherent state across every viewer of a session: same icons, same
  unread badge, same notification list, and a dismissal anywhere is a
  dismissal everywhere.
- Notifications reach the user when the blit tab is backgrounded — the
  case that makes the feature worth building — using the origin-scoped
  service worker that [net.md](net.md) already registered.
- Icons cross the wire once per distinct image, no matter how often the
  item republishes.
- Degrade in one direction only: no `dbus-daemon`, no bus, no tray, but
  notifications posted through blit's own API still work.

## Non-goals

- **Web Push.** Reaching a device with no blit tab open at all needs a
  VAPID keypair, a subscription store, and a push sender. Worth doing;
  not this RFC. Everything here is scoped to a connected client, awake
  or backgrounded.
- **Being a desktop.** No app menu export (`com.canonical.AppMenu`), no
  global shortcuts, no MPRIS, no power/session management, no
  `org.freedesktop.portal` implementation of blit's own (blit creates
  the bus, an out-of-process portal serves it — that stays true).
- **Taking over a host session bus.** blit claims names on _its_ private
  bus. If a real notification daemon somehow owns the name, blit stands
  down (§ Claiming the names).
- **Notification history/persistence across compositor restarts.** Live
  notifications are session state; a restart is a fresh start.
- **Rendering SVG server-side.** The client is a browser (§ Icons).

## Where an icon comes from

Four sources, one representation. Every one of them ends as either PNG
bytes or SVG bytes in a content-addressed store, referenced everywhere
else by a 128-bit hash.

| Source                    | What the app gives us                                           | How it resolves                              |
| ------------------------- | --------------------------------------------------------------- | -------------------------------------------- |
| StatusNotifierItem        | `IconName` (+ `IconThemePath`), or `IconPixmap` (ARGB32)        | theme lookup, else pixmap → PNG              |
| Notification              | `app_icon` (name or `file://`), hints `image-path`/`image-data` | same lookup, else raw pixels → PNG           |
| `xdg-toplevel-icon-v1`    | an icon name, or `wl_buffer`s at several sizes                  | theme lookup, else largest buffer → PNG      |
| `xdg_toplevel.set_app_id` | an app id                                                       | `${app_id}.desktop` → `Icon=` → theme lookup |

The last row is the one that pays for the rest. Every toplevel already
sets an app id ([`imp.rs:6735`](../../crates/compositor/src/imp.rs)) and
blit already forwards it as `S2C_SURFACE_APP_ID`, so a `.desktop` lookup
alone puts a real icon on every surface chip, every switcher row, and
every pane header — for apps that will never own a tray icon and never
post a notification.

**Theme lookup** is the freedesktop Icon Theme spec, minus what a browser
makes unnecessary. Search `$XDG_DATA_HOME/icons`, each
`$XDG_DATA_DIRS/icons`, then `/usr/share/pixmaps`; consult
`$BLIT_ICON_THEME`, then `Adwaita`, then `hicolor`, following `Inherits=`
in each `index.theme`. Prefer `scalable/…svg` outright and otherwise take
the largest raster at or below 256×256. Preferring the SVG is not a
quality judgement, it is a work-avoidance one: shipping the SVG bytes
means the _browser_ rasterizes at whatever size and DPR the status bar
happens to want, and blit needs no SVG renderer at all. `resvg` would be
a large new dependency to do worse than `<img>`.

**The search path belongs to the app, not to blit.** On this box
`/run/current-system/sw/share/icons/hicolor` holds the system's icons and
nothing else; a Nix-installed Slack keeps its icons in its own store path,
reachable only through the `XDG_DATA_DIRS` of the process that _is_
Slack. So resolution reads the requesting process's environment:
`GetConnectionUnixProcessID` on the item's bus name (or the surface's pid
for a toplevel) → `/proc/<pid>/environ` → its `XDG_DATA_DIRS`,
`XDG_DATA_HOME`, `HOME`, falling back to the server's own. Resolving
against blit's environment instead would find nothing on any distribution
that isolates per-package data directories, which is exactly the case
where GUI apps are most likely to be running.

**Both formats are browser-native by construction.** Raw ARGB32 pixmaps
from SNI and from `image-data` hints are converted server-side with the
`png` crate — already a dependency, already doing exactly this for custom
cursors ([`lib.rs:6605`](../../crates/server/src/lib.rs), which the
client turns into a blob URL at `BlitConnection.ts:4598`). XPM is not
resolved. The client therefore handles two formats, both of which it can
hand to `<img>` untouched.

## Wire

New `S2C_HELLO` feature bit — bit 11, from the block the header comment
already reserves for per-family modules:

```text
FEATURE_DESKTOP = 1 << 11
```

Opcodes occupy the free `0x50` block in both directions. Gateway, proxy
and mux forward them unmodified. All integers little-endian; framing,
the 16 MiB frame limit, `S2C_FRAGMENT` reassembly and `MAX_DECOMPRESSED`
per [protocol.md](../protocol.md) apply.

| Direction | Opcode | Name                  | Layout                                                                                        |
| --------- | ------ | --------------------- | --------------------------------------------------------------------------------------------- |
| C2S       | `0x50` | `DESKTOP_SUBSCRIBE`   | `[flags:2]`                                                                                   |
| C2S       | `0x51` | `DESKTOP_UNSUBSCRIBE` | _(empty)_                                                                                     |
| C2S       | `0x52` | `DESKTOP_ICON_GET`    | `[nonce:2][hash:16]`                                                                          |
| C2S       | `0x53` | `DESKTOP_ITEM_ACTION` | `[item_id:2][action:1][x:2][y:2]`                                                             |
| C2S       | `0x54` | `DESKTOP_MENU_GET`    | `[nonce:2][item_id:2]`                                                                        |
| C2S       | `0x55` | `DESKTOP_MENU_EVENT`  | `[item_id:2][entry_id:4][event:1]`                                                            |
| C2S       | `0x56` | `DESKTOP_NOTE_ACTION` | `[note_id:4][key_len:1][key:N]`                                                               |
| C2S       | `0x57` | `DESKTOP_NOTE_CLOSE`  | `[note_id:4][reason:1]`                                                                       |
| C2S       | `0x58` | `DESKTOP_NOTIFY`      | `[nonce:2][replaces:4][urgency:1][expire_ms:4][summary_len:2][summary:N][body_len:2][body:N]` |
| S2C       | `0x50` | `DESKTOP_STATE`       | `[revision:4][flags:1][records:LZ4]`                                                          |
| S2C       | `0x51` | `DESKTOP_ICON`        | `[nonce:2][status:1][format:1][data:LZ4]`                                                     |
| S2C       | `0x52` | `DESKTOP_MENU`        | `[nonce:2][item_id:2][status:1][entries:LZ4]`                                                 |
| S2C       | `0x53` | `DESKTOP_NOTIFIED`    | `[nonce:2][note_id:4]`                                                                        |

### `DESKTOP_SUBSCRIBE` / `DESKTOP_STATE`

`flags` bit 0 `ITEMS`, bit 1 `NOTES`, bit 2 `SURFACE_ICONS`. A subscribe
is answered immediately with a full `DESKTOP_STATE` and then with another
on every change; it replaces any previous subscription, so narrowing is
a re-subscribe. There is no ack and no pacing window: a snapshot of the
whole desktop state is a few hundred bytes, and the only unbounded thing
in the family (icon bytes) is pulled separately.

`DESKTOP_STATE.flags` reports what is actually working: bit 0 `BUS` (the
private session bus is alive), bit 1 `TRAY` (the watcher name is held),
bit 2 `NOTIFY` (the notification name is held). A client with all three
clear renders no tray cluster and greys the bell rather than lying about
an empty desktop. `revision` increments per snapshot; it exists so a
client can tell a coalesced re-send from a no-op and so menu staleness
has something to compare against.

`records`, LZ4-compressed (`lz4_flex::compress_prepend_size`),
decompressed a sequence of length-prefixed records — `[record_len:4]`
first, exactly as the `FS_*` family does, so a client skips a kind it
does not know:

```text
ITEM    0x01: [record_len:4][kind:1][item_id:2][flags:1][status:1][pid:4]
              [icon:16][attention_icon:16][overlay_icon:16][menu_rev:4]
              [title_len:1][title:N][tooltip_len:2][tooltip:N]
NOTE    0x02: [record_len:4][kind:1][note_id:4][flags:1][urgency:1][source:1]
              [created_ms:8][expires_ms:8][icon:16]
              [app_len:1][app:N][summary_len:2][summary:N][body_len:2][body:N]
              [action_count:1] repeated{ [key_len:1][key:N][label_len:1][label:N] }
SURFICON 0x03: [record_len:4][kind:1][surface_id:2][icon:16]
```

An all-zero `icon` hash means "no icon" — a legal state for an item whose
`IconName` resolved to nothing, and the reason the field is fixed-width
rather than optional.

`ITEM.status` is SNI's: `0` passive, `1` active, `2` needs attention (the
state that makes Slack's icon the badged one). `flags` bit 0 `IS_MENU`
(the item asked that a primary click open its menu rather than activate),
bit 1 `HAS_MENU`. `menu_rev` bumps when the app's `dbusmenu` layout
changes, which is how an open menu knows to re-pull.

`NOTE.urgency` is `0` low, `1` normal, `2` critical; `source` is `0` bus,
`1` client API, `2` terminal bell. `flags` bit 0 `TRANSIENT` (the app
asked that it not be kept in a list), bit 1 `RESIDENT` (it survives its
own actions being invoked), bit 2 `HAS_DEFAULT` (a plain click means
something). `created_ms` and `expires_ms` are Unix milliseconds;
`expires_ms` 0 means it never expires on its own, which is what critical
urgency gets. Actions are the freedesktop pairs — key then human label,
with `default` conventionally unlabelled.

`SURFICON` joins onto the existing `BlitSurface` by `surface_id` rather
than extending `S2C_SURFACE_CREATED`. Two reasons: an icon resolves
asynchronously (a `.desktop` scan, a file read) and must not delay the
surface announcement that a pane is waiting on, and gating it on
`FEATURE_DESKTOP` keeps a compositor-only client from having to know what
a hash is.

### `DESKTOP_ICON_GET` / `DESKTOP_ICON`

The one pull for pixels, and the reason state snapshots are cheap. A tray
item republishes its whole property set every time its unread count
changes — Slack swaps between two images all day — so inlining icon bytes
in the snapshot would resend the same pixels hundreds of times per hour.
Content addressing makes an item record ~60 bytes and each distinct image
cross once.

`hash` is BLAKE3 truncated to 128 bits, over the stored bytes: the same
choice, for the same reason, as the fs family's content hashes, and
`blake3` is already a dependency of `blit-fssync`, `blit-lsp` and
`blit-webserver`. `status`: `0` ok, `1` unknown hash, `2` too large.
`format`: `0` PNG, `1` SVG. A client caches by hash for the life of the
tab and never re-fetches; the server pins every hash referenced by the
current state, so a fetch that follows a snapshot cannot miss.

There is deliberately no HTTP route for icons. The gateway has exactly
one fallback handler and no `.route()` calls, everything else it serves
is compiled in, and — decisively — a blit connection over ssh or a relay
transport has no HTTP origin at all. The wire is the only channel every
transport has.

### `DESKTOP_ITEM_ACTION`

`action`: `0` activate (primary click), `1` secondary activate (middle
click), `2` context menu. `x`/`y` are the coordinates SNI's methods take;
blit passes the icon's on-screen position in the viewer, which is
meaningless to a headless compositor but is what the interface signature
demands and what some apps echo back. Scroll (`SNI.Scroll`) is not
mapped in v1; nothing common needs it.

### `DESKTOP_MENU_GET` / `DESKTOP_MENU` / `DESKTOP_MENU_EVENT`

A tray icon that can only be clicked is half a feature: `nm-applet` and
Steam are menu-only (`ItemIsMenu`), and Slack's "Set yourself as away"
and "Quit" live nowhere else. The menu is pulled when it is opened,
never pushed, because it is bulk and because most items' menus are never
opened at all.

`entries`, LZ4-compressed, length-prefixed records mirroring the state
family:

```text
ENTRY 0x01: [record_len:4][kind:1][entry_id:4][parent_id:4][flags:1][toggle:1]
            [icon:16][label_len:1][label:N][shortcut_len:1][shortcut:N]
```

`parent_id` 0 is the root, so the tree arrives flat and the client
rebuilds it. `flags` bit 0 `ENABLED`, bit 1 `VISIBLE`, bit 2 `SEPARATOR`,
bit 3 `HAS_CHILDREN`. `toggle` low nibble: `0` none, `1` checkmark, `2`
radio; high nibble: `0` off, `1` on, `2` indeterminate. Labels arrive
with `dbusmenu`'s `_` mnemonics stripped. `DESKTOP_MENU.status`: `0` ok,
`1` no such item, `2` the item exports no menu, `3` the app did not
answer in time.

`DESKTOP_MENU_EVENT.event`: `0` clicked, `1` opened, `2` closed. The
`opened` event is not decoration — `dbusmenu`'s `AboutToShow` is how apps
with dynamic menus (a workspace list, a recent-files list) fill them in,
and an app that gets a click on a submenu it was never told was opening
is entitled to do nothing.

### `DESKTOP_NOTE_ACTION` / `DESKTOP_NOTE_CLOSE` / `DESKTOP_NOTIFY`

`DESKTOP_NOTE_ACTION` invokes one of the note's action keys — `default`
for a plain click — which becomes an `ActionInvoked` signal to the app.
`DESKTOP_NOTE_CLOSE.reason` is the freedesktop set: `1` expired, `2`
dismissed by the user, `3` closed by a call, `4` undefined. Both are
idempotent against a note that has already gone: two viewers clicking the
same toast fire the action once, and the second `close` is dropped
silently rather than answered with an error nobody could act on.

Invoking an action does not itself close the note unless the app said
`RESIDENT` — the same rule the freedesktop spec gives, and it matters:
a chat app's "Reply" is resident, a build script's "Open log" is not.

`DESKTOP_NOTIFY` posts a notification from the client side, answered with
`DESKTOP_NOTIFIED` carrying the assigned `note_id` (which `replaces`
reuses, exactly as `Notify`'s `replaces_id` does). It is what makes
`blit notify` and agent-driven notifications work identically to an app's,
and — because it needs no D-Bus at all — it is what keeps the feature
alive on a server with no `dbus-daemon`, on macOS, and in every unit test
of this family.

## Server: claiming the names

`DesktopBus` grows a companion, `blit-desktop`, that connects to the
private bus as soon as it is up and claims, with `DO_NOT_QUEUE`:

- `org.freedesktop.Notifications`
- `org.kde.StatusNotifierWatcher`, plus a host registration under
  `org.kde.StatusNotifierHost-<pid>`

**Timing is load-bearing.** `dbus-daemon` activates services from the
_host's_ service files, so a host-installed notification daemon can be
activated onto blit's private bus by the first app that calls it. Claim
before any pty is spawned — the bus is created in the same block that
creates the compositor, well before `handle_client` reaches a spawn —
and the race does not exist. If a claim nevertheless fails, blit does not
fight for it: the corresponding `DESKTOP_STATE` flag stays clear, and the
other service still runs.

**The host registration is not optional.** Electron and every
libappindicator app check `IsStatusNotifierHostRegistered` and hide their
tray icon when it is false — the failure mode is not an error, it is
Slack looking like it has no tray icon at all. Both the `org.kde` and
`org.freedesktop` spellings of the watcher name are claimed, since apps
disagree about which one is real.

**A dead bus must come back.** Today the supervisor drops a dead
`DesktopBus` and never respawns it
([`lib.rs:5954`](../../crates/server/src/lib.rs)), which already means
every pty spawned afterwards has no `DBUS_SESSION_BUS_ADDRESS`; with
services on that bus it would also mean the tray and every notification
silently stop for the life of the compositor. Respawn-with-backoff, and
re-claim the names on the new bus, is a prerequisite of this RFC rather
than a part of it. Apps that were on the old bus are gone from the tray
until they re-register, which is correct: their process lost its bus too.

### Notification service

`org.freedesktop.Notifications` with the four methods (`Notify`,
`CloseNotification`, `GetCapabilities`, `GetServerInformation`) and both
signals (`NotificationClosed`, `ActionInvoked`).

Capabilities advertised: `body`, `actions`, `icon-static`, `persistence`.
Deliberately **not** `body-markup`. The spec's markup is a small HTML
subset, and advertising it would mean accepting app-controlled markup and
rendering it in blit's own origin — a sanitizer between an untrusted
process and the UI, forever. Bodies are plain text, the client renders
text nodes, and the whole class of injection goes away for the cost of
Slack's bold sender name. Also not advertised: `sound` (no output path
from a headless bus), `body-images` (the same argument as markup).

`expire_timeout` follows the spec: `-1` means "server decides" — 8 s for
low, 20 s for normal, never for critical — and `0` means never. Expiry is
server-side and emits `NotificationClosed(1)`, so an app's own bookkeeping
stays correct no matter how many viewers there are or whether any is
connected. `desktop-entry`, `image-path` and `image-data` hints resolve
the icon; `urgency` maps straight through; `transient` and `resident`
become record flags; unknown hints are dropped.

**The click completes a loop that already exists.** An Electron app
handling `ActionInvoked` typically raises its window through
`xdg_activation_v1` — which blit already binds and already forwards as
`S2C_SURFACE_ACTIVATED`, whose own doc comment names this exact case
("an Electron app reacting to a notification click"). Clicking a Slack
toast in a browser tab therefore raises the Slack pane, with no new code
between the two ends.

### StatusNotifier watcher and host

`RegisterStatusNotifierItem` accepts either a bus name or an object path,
tolerates both `/StatusNotifierItem` and `/org/ayatana/NotificationItem/…`
(libayatana's spelling), and starts a property mirror per item: `Id`,
`Title`, `Status`, `IconName`, `IconThemePath`, `IconPixmap`,
`AttentionIconName`, `OverlayIconName`, `ToolTip`, `ItemIsMenu`, `Menu`.
Items are followed with `PropertiesChanged` plus the legacy `NewIcon`,
`NewStatus`, `NewToolTip`, `NewAttentionIcon` signals — legacy because
several toolkits emit only those and never `PropertiesChanged`, so a
mirror that trusts the modern signal alone shows a permanently stale
icon. A `NameOwnerChanged` to nothing removes the item.

`item_id` is a small server-assigned integer, stable for as long as the
item's bus name owns it, so a client's DOM keys and open menus survive an
icon change.

### Icon store

Process-wide, hash → bytes, LRU by total size (`BLIT_DESKTOP_ICON_MAX`,
8 MiB), with every hash referenced by live state pinned. Resolution is
memoized on `(source-identity, name, theme search path)` so a tray item
flipping between two icons hits the memo rather than the filesystem.
Everything — the `.desktop` scan, the theme walk, the file read, the PNG
encode — runs on the blocking pool and never under the session mutex, per
the same rule the fs family follows.

## Client

**Tray cluster.** A new segment in `StatusBar.tsx`, to the left of the
connection dots: one button per item, icon at the status bar's glyph size
(doubled in touch mode, like the existing icon buttons), a dot overlay for
`needs attention`, the overlay icon composited when present, and the
tooltip as the accessible name. Primary click sends `activate` — or opens
the menu when `IS_MENU` — and right click sends `context menu`, both of
which pull the layout first if `menu_rev` moved. Following
[ide.md](../ide.md)'s pattern, that is one new prop wired at the
`Workspace.tsx` call site and one inline block.

**Bell chip and list.** A bell with an unread count opens a panel listing
live notifications, newest first, each with icon, app, summary, body,
action buttons and a dismiss. The panel is the whole notification UI on a
phone, where a toast overlay competes with the keyboard for the only
screen there is.

**Surface icons.** `SurfaceStore` gains an `icon` per surface from the
`SURFICON` records, and the four places that already render
`surfaceName()` — status bar identity, switcher rows, pane headers,
`documentTitle` — render it before the name. The switcher is where this
matters most: a list of five Chromium windows distinguished only by title
prefix is exactly the list an icon fixes.

**Escalating to the OS.** In-app rendering is unconditional and needs no
permission. Escalation to a real notification is per-client policy:
default is _the document is hidden_ and permission is granted, via
`registration.showNotification()` on the service worker that
[net.md](net.md) already registers at origin scope (`Notification` alone
is not enough — Chrome on Android refuses it). Permission is requested
from a settings toggle, never on load, because a permission prompt fired
by a page the user just opened is the prompt everyone denies reflexively.

The escalation rule needs one guard the in-app path does not. Because
notifications are _state_, a client that reconnects after an hour
receives every still-live note in one snapshot — correct for the list,
catastrophic as a burst of OS notifications. So a client escalates a note
only if it has not seen its `note_id` before _and_ `created_ms` is within
the last 60 s. The list shows everything; the OS hears about what is
actually new.

**`notificationclick` closes the loop**: the worker focuses the blit
client and posts the `(note_id, action)` pair to it, which sends
`DESKTOP_NOTE_ACTION`, which reaches the app, which activates its
toplevel, which raises the pane.

**Icons are rendered only as `<img src="blob:…">`.** Never inlined into
the DOM, never `innerHTML`, never a background image built from
app-controlled text. An SVG loaded through `<img>` cannot run scripts and
cannot fetch subresources; the same bytes inlined can do both, in blit's
origin, on behalf of whatever is running in a pty. This is the single
most important implementation constraint in this document.

### Terminal bell

`Event::Bell` is dropped today at
[`alacritty-driver/src/lib.rs:343`](../../crates/alacritty-driver/src/lib.rs).
It becomes an **attention flag on the pty's chip**, not a notification:
a bell is how `less` says "no more matches", and promoting that to a
desktop notification would train everyone to turn the feature off.
`BLIT_BELL_NOTIFY=1` promotes it to a low-urgency note for people who
use `; echo -e '\a'` as a build-finished signal. OSC 9 and OSC 777 stay
unhandled: the private bus makes `notify-send` work, and a second,
worse notification API is not worth a vendored-parser change.

## CLI

```bash
blit notify send "Build finished" "3 warnings" --urgency low
blit notify list --json          # live notifications, machine-readable
blit notify close <id>
blit tray list --json            # id, app, status, tooltip, has-menu
blit tray activate <id>
blit tray menu <id>              # dump the layout
blit tray click <id> <entry-id>
```

These are not conveniences bolted on afterwards; they are how the family
gets tested without a browser, which is the same reason `blit surface`
exists.

## Limits and defaults

| Knob                                | Default  | Env                        |
| ----------------------------------- | -------- | -------------------------- |
| Tray items                          | 32       | `BLIT_DESKTOP_MAX_ITEMS`   |
| Live notifications                  | 64       | `BLIT_DESKTOP_MAX_NOTES`   |
| Notifications per bus name / minute | 60       | `BLIT_DESKTOP_NOTE_RATE`   |
| Icon bytes (one image)              | 256 KiB  | `BLIT_DESKTOP_ICON_BYTES`  |
| Icon store (process-wide)           | 8 MiB    | `BLIT_DESKTOP_ICON_MAX`    |
| Menu entries per item               | 512      | —                          |
| State snapshot coalescing           | 100 ms   | `BLIT_DESKTOP_COALESCE_MS` |
| Default expiry (low / normal)       | 8s / 20s | —                          |
| Icon theme                          | Adwaita  | `BLIT_ICON_THEME`          |
| Whole family off                    | on       | `BLIT_DESKTOP=0`           |

Over the note limit, the oldest non-critical note is closed with reason
`4` and its `NotificationClosed` is emitted, so an app that tracks its own
ids stays consistent with what the user can see. Over the item limit, new
registrations are refused — an app that registers 33 tray icons is
broken, and the 33rd is not the one to show. Coalescing at 100 ms is what
keeps an app animating its tooltip from turning into a snapshot storm.

## Security

The trust boundary does not move: blit already hands a client a shell,
and the private bus is reachable only by processes blit itself spawned
into that compositor. Nothing here grants a pty a capability it lacked.
What it does is give untrusted local processes a new path to _bytes and
text rendered in the viewer's origin_, and that is where the care goes.

- **SVG only through `<img>`** (§ Client). The one rule that, broken,
  turns a tray icon into script execution in blit's origin.
- **No `body-markup`** (§ Notification service). Untrusted text is
  rendered as text.
- **Untrusted display strings** — titles, tooltips, summaries, app names
  — are length-clamped and stripped of controls on the way out, the same
  discipline `documentTitle.ts` already applies to terminal titles.
- **Icon reads are bounded**, 256 KiB per image, and a decoded pixmap
  over 512×512 is rejected before it is re-encoded, so a hostile
  `IconPixmap` cannot turn into a gigabyte of PNG. Absolute paths from
  `IconThemePath` and `image-path` are honoured rather than sandboxed:
  the app could have handed us the same bytes inline, so refusing them
  buys nothing.
- **Rate limits are per bus name**, not global, so one chatty app cannot
  drown another's notifications or the wire.
- **The private bus stays private.** Its address goes to pty children
  and nowhere else; nothing in this family exposes the address to a
  client, and `DESKTOP_NOTIFY` is a protocol message rather than a bus
  proxy precisely so that a browser can never make an arbitrary D-Bus
  call.

## Testing

Every layer has an oracle that needs no GUI and no browser:

1. **No bus at all** — `DESKTOP_NOTIFY` + `blit notify list --json`
   exercises the state model, expiry, dismissal, multi-client fan-out and
   the codecs on any platform. This is the unit-test path.
2. **Real D-Bus, no app** — `notify-send -u critical "hi" "body"` from a
   pty is the shortest end-to-end test that exists; `gdbus call` from a
   pty (the env is already right) drives `Notify`, `CloseNotification`
   and `GetCapabilities` directly.
3. **Synthetic tray item** — a small script registering a
   StatusNotifierItem with an `IconName`, then an `IconPixmap`, then a
   `dbusmenu` layout, covers all three icon paths and the menu pull
   without installing anything.
4. **Real apps** — Electron with libayatana-appindicator for the tray
   (`ItemIsMenu`, badge swaps), and the compositor repro path for a real
   Chromium/Slack window's `app_id` → `.desktop` → icon resolution.
5. **Browser** — a Playwright spec granting the `notifications`
   permission, asserting the toast renders, that a hidden document
   escalates and a visible one does not, and that a reconnect replaying
   an hour-old note escalates nothing.

Codec fixtures are pinned on both sides — `crates/remote/src/desktop.rs`
and `js/core/src/desktop.ts` — as the other families do, so drift fails
on one side or the other rather than in a browser.

## Dependencies

This is the first thing in blit to speak D-Bus. Two ways:

**`zbus`** (recommended) — pure Rust, no `libdbus`, tokio-native, and it
implements both halves (client for the SNI property mirror, server for
the two services blit owns). It brings `zvariant` and `serde`; `serde` is
already in the lock file. The cost is real but bounded, and it is
Linux-only: `blit-desktop` is `#[cfg(target_os = "linux")]` like
`desktop_bus` and `audio` before it.

**A hand-rolled D-Bus client** — blit has form here (a hand-written
Wayland compositor on raw `wayland-server`, a hand-rolled wire protocol
with no serde), and the D-Bus wire is not large: SASL `EXTERNAL`
handshake, a marshaller for the type codes these interfaces actually use,
name claiming, signal matching. Perhaps 1,500 lines, and it would keep
the dependency graph flat.

Take `zbus` first. If the dependency proves objectionable, the interface
boundary (`blit-desktop` exposing state and actions, nothing D-Bus-shaped
crossing it) means it can be replaced later without touching the wire,
the server, or the client.

The Wayland side needs no new dependency at all: `xdg-toplevel-icon-v1`
is already in the pinned `wayland-protocols` 0.32 under the `staging`
feature the compositor already enables — a `create_global` and a
`Dispatch` impl, using the same shm-read path the compositor already has
for custom cursors.

## Implementation

1. **`blit-desktop` crate** (`crates/desktop`), Linux-only: the state
   model (items, notes, icon store), the D-Bus services, and a
   platform-independent core that `DESKTOP_NOTIFY` alone can drive.
   Wire codecs in `crates/remote/src/desktop.rs` and
   `js/core/src/desktop.ts` with shared fixtures.
2. **Notifications end to end, no icons**: the fdo service, state
   snapshots, the bell chip and list panel in `StatusBar.tsx`,
   `blit notify`. Icon hashes are all-zero throughout, which the client
   already has to handle. Prerequisite: respawn the dead `DesktopBus`.
3. **Icons**: the store, the theme/`.desktop` resolver reading the app's
   own environment, `DESKTOP_ICON_GET`, and `SURFICON` records driven
   by `app_id` alone. This is the step that puts an icon on every
   surface chip and switcher row.
4. **Tray**: watcher, host registration, property mirror, the status bar
   cluster, `DESKTOP_ITEM_ACTION`, `blit tray`.
5. **Menus**: `dbusmenu` pull, the popup, `DESKTOP_MENU_EVENT`.
6. **The rest**: `xdg-toplevel-icon-v1`, bell attention flags, the
   service-worker escalation policy and its settings toggle.

Steps 2 and 3 are each independently shippable and independently useful;
4 and 5 are one feature split in two, and shipping 4 without 5 is
acceptable only briefly, since a menu-only item has nothing else to
offer.
