//! `@session` — the protocol half: one receive loop that services CLI
//! invocations, application exits, and backoff deadlines together.
//!
//! The shape matters. Every blocking entry point in the SDK waits for *its own*
//! packet, so an extension parked in `CommandProvider::accept` cannot notice a
//! child that died and cannot let a backoff deadline come due. So this owns the
//! loop — `wait_until(next deadline)`, then `recv`, then route by opcode — and
//! uses the SDK's non-blocking `offer` to hand channel packets over.

use blit_ext_session::desktop_entry::{self, DesktopEntry};
use blit_ext_session::icon;
use blit_ext_session::supervisor::{App, Phase, next_deadline_ns};
use blit_guest::command::{CommandProvider, Error, ProviderEvent};
use blit_guest::remote;
use blit_guest::{Client, WaitOutcome};
use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

const DESCRIPTOR: &str = r#"{
  "protocol":"blit.cli.v1",
  "summary":"Autostart and supervise GUI applications in this session",
  "commands":[
    {"path":["list"],"summary":"Installed applications and whether they are enabled",
     "usage":"blit @session list"},
    {"path":["enable"],"summary":"Start an application now and on every session start",
     "usage":"blit @session enable <app>"},
    {"path":["disable"],"summary":"Stop an application and stop starting it",
     "usage":"blit @session disable <app>"},
    {"path":["start"],"summary":"Start an application now, without remembering it",
     "usage":"blit @session start <app>"},
    {"path":["stop"],"summary":"Stop an application now, keeping it enabled",
     "usage":"blit @session stop <app>"},
    {"path":["forget"],"summary":"Stop an application and drop it from the list",
     "usage":"blit @session forget <app>"},
    {"path":["status"],"summary":"What one application is doing, and its windows",
     "usage":"blit @session status <app>"}
  ]
}"#;

/// kv key prefix. The store is flat and shared across every session on a
/// desktop server, so the prefix is what keeps two sessions from overwriting
/// each other's intent.
const KV_PREFIX: &str = "ext/session/app/";

/// Channel the browser panel reads.
///
/// Outbound is JSON, one object per message, so the mirror needs no parser of
/// its own. Inbound is a single line of plain text (`enable <id>`) because a
/// Wasm guest has no JSON parser and the command vocabulary is three verbs —
/// hand-rolling a parser for that would be more code than it saves.
const CHANNEL_NAME: &str = "blit.session.v1";

fn main() {}

blit_guest::entry!(run);

/// How long the installed catalog is trusted before it is read again.
///
/// It is not watched, so without this a package installed after the extension
/// started stays invisible to `list` and `enable` for the whole session. A
/// minute is short enough that installing something and reaching for it works,
/// and long enough that the read never lands on a hot path.
const CATALOG_TTL: Duration = Duration::from_secs(60);

/// Most icons a panel may ask for in one request.
///
/// Generous on purpose, and matched by the panel's own batch size: a request
/// costs one child whatever it asks for, so a bigger batch is strictly cheaper
/// per icon, and a scrolling list needs the throughput. It is still a bound —
/// the whole batch is resolved inside the receive loop, so an unbounded request
/// would be a way to stall the supervisor from the browser.
const MAX_ICON_REQUEST: usize = 48;

/// Resolved icon paths kept in the guest before the cache is dropped wholesale.
///
/// Measured in bytes rather than entries because the entries are not
/// comparable: a themed SVG is 3 KB and a 128px PNG can be [`icon::MAX_ICON_BYTES`],
/// so any count that is safe for the second is uselessly small for the first.
/// Base64 art is by far the largest thing this extension holds, and a session
/// whose operator scrolls a thousand-entry catalog would otherwise accumulate
/// all of it.
///
/// Clearing rather than evicting the oldest entry keeps the bookkeeping to a
/// comparison: a miss costs one shell round trip, and the panel has its own
/// cache, so nothing already on screen pays for it.
///
/// Large enough to hold one whole request — [`MAX_ICON_REQUEST`] files of
/// [`icon::MAX_ICON_BYTES`] come to about 8 MiB once base64 has grown them by a
/// third. Below that a single scroll of big artwork is guaranteed to clear the
/// cache it is still filling, which costs a re-read of everything it just did.
const MAX_CACHED_ICON_BYTES: usize = 12 * 1024 * 1024;

/// Icon messages a connection may have waiting on credit.
///
/// Icons are queued rather than dropped, because unlike state nothing provokes
/// a repeat: a dropped one leaves a placeholder until the panel asks again. Two
/// full requests' worth, so an ordinary scroll never reaches it, and a panel
/// that has stopped acking altogether still cannot grow the guest without
/// limit. What is dropped past here the panel re-asks for, once it stops
/// counting the id as outstanding.
const MAX_QUEUED_ICONS: usize = 128;

/// One browser connected to [`CHANNEL_NAME`].
struct Conn {
    id: u32,
    /// Send credit granted by the peer, and how much of it is spent. A panel
    /// that stops acking must not be allowed to grow the guest's memory.
    window: u64,
    sent: u64,
    acked: u64,
    /// Bytes received, for the cumulative ack owed back.
    received: u64,
    closed: bool,
    /// The last state this connection was actually *sent*.
    ///
    /// Per-connection, and recorded only after a successful send. A shared
    /// "last published" string updated before the send meant that a panel
    /// which was briefly out of credit missed the message and then had every
    /// repeat of it suppressed as a duplicate — it stayed stale until some
    /// unrelated change came along.
    last_sent: String,
    /// Icon messages waiting for credit, oldest first.
    ///
    /// State can be dropped when a panel is out of credit because the next
    /// publish carries it again. An icon reply cannot: it answers a request
    /// that will not be repeated, so dropping one leaves a row with a
    /// placeholder for the rest of the session.
    queued: Vec<String>,
}

/// One application's persisted intent.
struct Intent {
    enabled: bool,
    /// The boot generation `process_ref` was recorded under. A reference from
    /// a different server run names a different process, or nothing.
    boot_generation: u64,
    process_ref: Option<u64>,
}

struct State {
    /// Desired state, keyed by desktop-entry id.
    apps: BTreeMap<String, App>,
    /// Installed applications, refreshed on a TTL rather than watched.
    installed: BTreeMap<String, DesktopEntry>,
    /// When the catalog was last read, for [`CATALOG_TTL`].
    installed_at_ns: Option<i64>,
    /// Themed and flat icon directories, from the same environment read that
    /// found the catalog. Empty until that read happens.
    icon_theme_roots: Vec<String>,
    icon_flat_roots: Vec<String>,
    /// Every directory an icon could be in, best-first. Expanding the roots'
    /// globs is the one part of a lookup that does not depend on what is being
    /// looked up, so it is done once and reused; a catalog refresh clears it,
    /// because that is when a newly installed theme would show up.
    icon_dirs: Vec<String>,
    /// Resolved artwork, keyed by the `Icon=` value rather than by application
    /// id — a desktop and its `-nightly` twin share a key, and so do the dozens
    /// of entries that all say `application-x-executable`.
    ///
    /// `None` records "looked, found nothing", which is worth caching for the
    /// same reason the artwork is: it stops a panel that keeps redrawing an
    /// icon-less row from spawning a shell every time.
    icons: BTreeMap<String, Option<String>>,
    /// What [`State::icons`] holds, for [`MAX_CACHED_ICON_BYTES`].
    icon_bytes: usize,
    /// Stamped identity per surface, so `status` reports windows rather than
    /// guessing from a self-asserted app_id.
    surface_apps: BTreeMap<u16, String>,
    /// The server process this state describes. A different one means every
    /// recorded process_ref is meaningless.
    boot_generation: u64,
    nonce: u16,
    /// Endpoint-local process ids. Handed out rather than derived so a
    /// short-lived helper child can never collide with a supervised app's slot.
    next_process_id: u32,
    /// Listener id for [`CHANNEL_NAME`], zero when it could not be published.
    data_listener: u32,
    /// Browsers reading the panel.
    conns: Vec<Conn>,
}

impl State {
    fn next_nonce(&mut self) -> u16 {
        // Zero is a legal nonce but reserved here for unsolicited traffic.
        self.nonce = self.nonce.wrapping_add(1).max(1);
        self.nonce
    }

    fn next_process_id(&mut self) -> u32 {
        self.next_process_id = self.next_process_id.wrapping_add(1).max(1);
        self.next_process_id
    }

    /// Remember one lookup's answer, dropping the whole cache first if it has
    /// grown past [`MAX_CACHED_ICON_BYTES`].
    fn cache_icon(&mut self, key: String, data_url: Option<String>) {
        if self.icons.contains_key(&key) {
            return;
        }
        if self.icon_bytes >= MAX_CACHED_ICON_BYTES {
            self.icons.clear();
            self.icon_bytes = 0;
        }
        self.icon_bytes += key.len() + data_url.as_ref().map_or(0, String::len);
        self.icons.insert(key, data_url);
    }
}

fn run(mut client: Client) -> Result<(), Error> {
    let listener_name = format!(
        "blit.cli.{:016x}.{}",
        client.context().extension_id,
        client.context().attempt
    );
    let listener = client.listen_channel(&listener_name, b"")?;
    let mut provider = CommandProvider::register(&mut client, listener, DESCRIPTOR)?;

    // The browser panel's channel. Publishing it is not fatal if it fails —
    // another instance may already serve the name, and the CLI half is still
    // worth running.
    let data_listener = client
        .listen_channel(CHANNEL_NAME, b"")
        .map(|listener| listener.id())
        .unwrap_or(0);

    let mut state = State {
        apps: BTreeMap::new(),
        installed: BTreeMap::new(),
        installed_at_ns: None,
        icon_theme_roots: Vec::new(),
        icon_flat_roots: Vec::new(),
        icon_dirs: Vec::new(),
        icons: BTreeMap::new(),
        icon_bytes: 0,
        surface_apps: BTreeMap::new(),
        // Known before the first packet: the bootstrap HELLO carries it, and
        // re-adoption below cannot wait for a second one that never comes.
        boot_generation: client.context().hello.boot_generation.unwrap_or(0),
        nonce: 0,
        next_process_id: 100,
        data_listener,
        conns: Vec::new(),
    };

    // Intent outlives the server, so restore it before anything else. A failure
    // here is not fatal: serving `list` with no catalog is better than not
    // coming up at all.
    if let Err(error) = restore(&mut client, &mut state) {
        let _ = error;
    }
    // Anything already enabled starts now.
    reconcile(&mut client, &mut state);
    publish(&mut client, &mut state);

    loop {
        // MonotonicInstant cannot be built from raw nanos, so a deadline is
        // expressed as a delay from now.
        let now = client.monotonic_now();
        let pending = next_deadline_ns(state.apps.values());
        let delay = match pending {
            Some(ns) => Duration::from_nanos(ns.saturating_sub(now.raw_nanos()).max(0) as u64),
            // Nothing pending: wake periodically anyway, so a missed
            // notification cannot wedge the supervisor for the whole session.
            None => Duration::from_secs(30),
        };
        let outcome = client.wait_until(now + delay)?;
        match outcome {
            WaitOutcome::Closed => return Ok(()),
            WaitOutcome::Deadline => {
                reconcile(&mut client, &mut state);
                publish(&mut client, &mut state);
            }
            WaitOutcome::Packet => {
                let Some(packet) = client.recv()? else {
                    return Ok(());
                };
                // A CLI invocation is the provider's; everything else is ours.
                match provider.offer(&mut client, &packet)? {
                    Some(ProviderEvent::Invocation(invocation)) => {
                        serve(&mut client, &mut state, invocation)?;
                        reconcile(&mut client, &mut state);
                        publish(&mut client, &mut state);
                    }
                    Some(ProviderEvent::Closed(_)) => return Ok(()),
                    // Most packets change nothing a panel or the supervisor
                    // cares about — a supervised application's own stdout is
                    // by far the commonest — and reconciling and rebuilding
                    // the state JSON for each one turned a chatty child into
                    // a busy loop. `route` says when it is worth the work.
                    None => {
                        if route(&mut client, &mut state, &packet) {
                            reconcile(&mut client, &mut state);
                            publish(&mut client, &mut state);
                        }
                    }
                }
            }
        }
    }
}

/// JSON-escape a string into a buffer. Only the characters JSON requires, plus
/// the C0 range — an application name is arbitrary text from a `.desktop` file.
fn push_json_string(out: &mut String, value: &str) {
    out.push('"');
    // Copied in runs rather than character by character. This is the hot loop of
    // the whole extension: an icon is a 30 KB data URL of base64, none of which
    // needs escaping, and pushing that a `char` at a time cost seconds per
    // screenful in an interpreter.
    let mut rest = value;
    while let Some(at) = rest.find(|c: char| c == '"' || c == '\\' || (c as u32) < 0x20) {
        out.push_str(&rest[..at]);
        let mut chars = rest[at..].chars();
        let escaped = chars.next().unwrap_or(' ');
        match escaped {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            other => out.push_str(&format!("\\u{:04x}", other as u32)),
        }
        rest = chars.as_str();
    }
    out.push_str(rest);
    out.push('"');
}

/// The panel's whole view: every managed application, plus the installed
/// catalog so the panel can offer something to enable.
///
/// Sent complete on every change rather than as a delta. The managed set is
/// what an operator typed, so it is small, and a panel that can only ever be
/// correct is worth more here than one that avoids resending a few hundred
/// bytes. The catalog is the larger half and changes only when packages do, so
/// it is sent once per connection unless asked for again.
fn state_json(state: &State, with_catalog: bool) -> String {
    let mut out = String::from("{\"type\":\"state\",\"apps\":[");
    for (index, (id, app)) in state.apps.iter().enumerate() {
        if index > 0 {
            out.push(',');
        }
        let windows = state
            .surface_apps
            .values()
            .filter(|owner| owner.as_str() == id)
            .count();
        let name = state
            .installed
            .get(id)
            .map(|entry| entry.name.as_str())
            .unwrap_or(id.as_str());
        out.push_str("{\"id\":");
        push_json_string(&mut out, id);
        out.push_str(",\"name\":");
        push_json_string(&mut out, name);
        out.push_str(&format!(
            ",\"enabled\":{},\"phase\":\"{}\",\"failures\":{},\"windows\":{}",
            app.enabled,
            match app.phase {
                Phase::Running => "running",
                Phase::Backoff => "backoff",
                Phase::Idle => "starting",
                Phase::Stopped => "stopped",
            },
            app.failures,
            windows
        ));
        if let Some(exit) = app.last_exit {
            out.push_str(&format!(",\"lastExit\":{exit}"));
        }
        if let Some(display) = &app.wayland_display {
            out.push_str(",\"socket\":");
            push_json_string(&mut out, display);
        }
        out.push('}');
    }
    out.push(']');
    if with_catalog {
        out.push_str(",\"catalog\":[");
        for (index, (id, entry)) in state.installed.iter().enumerate() {
            if index > 0 {
                out.push(',');
            }
            out.push_str("{\"id\":");
            push_json_string(&mut out, id);
            out.push_str(",\"name\":");
            push_json_string(&mut out, &entry.name);
            out.push('}');
        }
        out.push(']');
    }
    out.push('}');
    out
}

/// One application's artwork, or the fact that it has none.
///
/// A missing `icon` field is the answer "there is nothing to draw", and the
/// panel records it so it stops asking. That is why this is a message per id
/// rather than a map of the ones that were found: a silent omission would be
/// indistinguishable from a reply still in flight.
fn icon_json(id: &str, path: Option<&str>) -> String {
    let mut out = String::from("{\"type\":\"icon\",\"id\":");
    push_json_string(&mut out, id);
    if let Some(path) = path {
        out.push_str(",\"path\":");
        push_json_string(&mut out, path);
    }
    out.push('}');
    out
}

/// Pair each requested id with what this batch resolved for its `Icon=` key.
///
/// An id whose key is absent from `resolved` is **omitted** rather than answered
/// "no artwork". The two are not interchangeable: the panel records "no artwork"
/// as final and never asks again, while an id it hears nothing about goes back
/// on the queue once it stops counting it as outstanding. So absence here means
/// "ask me later", and only a key that was genuinely looked for and not found
/// answers `None`.
fn batch_answers(
    keys: Vec<(String, Option<String>)>,
    resolved: &BTreeMap<String, Option<String>>,
) -> Vec<(String, Option<String>)> {
    keys.into_iter()
        .filter_map(|(id, key)| match key {
            // No usable `Icon=` at all: there is nothing to look for, and
            // nothing a later request could find either.
            None => Some((id, None)),
            Some(key) => resolved.get(&key).map(|found| (id, found.clone())),
        })
        .collect()
}

/// Answer a panel's icon request, reading whatever is not already cached.
///
/// One child for the whole batch, and it does the searching and the reading
/// together — see [`icon::fetch_script`]. Deliberately *not* preceded by a
/// catalog refresh: the ids came out of the catalog the panel already holds, so
/// the only thing rereading it could add is a random half-second stall in the
/// middle of a scroll. An id this does not recognise is answered "no artwork",
/// which is what it will be until the panel resyncs anyway.
///
/// What this batch resolved is held here and answered from here, never read back
/// out of [`State::icons`]: that cache is dropped wholesale when it grows past
/// [`MAX_CACHED_ICON_BYTES`], and at [`icon::MAX_ICON_BYTES`] per file it can
/// hit that limit *part way through one batch*. Answering from it afterwards
/// reported every icon cached before the drop as "no artwork" — which the panel
/// believes forever, so a scrolled list grew a permanent gap exactly one
/// cache-worth long, and the row after it was fine.
fn resolve_icons(
    client: &mut Client,
    state: &mut State,
    ids: &[&str],
) -> Vec<(String, Option<String>)> {
    // Ids the catalog knows nothing about, and entries with no `Icon=` at all,
    // are answered "nothing to draw" without touching the icon path.
    let keys: Vec<(String, Option<String>)> = ids
        .iter()
        .map(|id| {
            let key = state
                .installed
                .get(*id)
                .and_then(|entry| entry.icon.clone())
                .filter(|icon| icon.starts_with('/') || icon::is_lookup_name(icon));
            ((*id).to_string(), key)
        })
        .collect();

    // A name is looked up once however many applications name it.
    let mut resolved: BTreeMap<String, Option<String>> = BTreeMap::new();
    let mut lookups: Vec<String> = Vec::new();
    let mut absolute: Vec<String> = Vec::new();
    for key in keys.iter().filter_map(|(_, key)| key.as_ref()) {
        if let Some(cached) = state.icons.get(key) {
            resolved.insert(key.clone(), cached.clone());
            continue;
        }
        let bucket = if key.starts_with('/') {
            &mut absolute
        } else {
            &mut lookups
        };
        if !bucket.contains(key) {
            bucket.push(key.clone());
        }
    }

    // Names: one message for the whole batch. Each name is a group of ranked
    // candidates and `FS_READ_FIRST` answers each group with its own first hit,
    // so the batch costs one round trip rather than one per name — which is what
    // it cost when every name was its own request, and that was most of the time
    // a screenful of artwork took.
    if !lookups.is_empty() {
        refresh_icon_dirs(client, state);
        let dirs = state.icon_dirs.clone();
        let mut asked: Vec<&String> = Vec::new();
        let mut candidates: Vec<Vec<String>> = Vec::new();
        for name in &lookups {
            let paths = icon::candidates(&dirs, name);
            if paths.is_empty() {
                state.cache_icon(name.clone(), None);
                resolved.insert(name.clone(), None);
                continue;
            }
            asked.push(name);
            candidates.push(paths);
        }
        // The protocol bounds the paths one message may carry, so a long icon
        // path means fewer names per message rather than a refused request.
        let per_message = (remote::fs::FS_READ_MAX_PATHS / (dirs.len() * 2).max(1)).max(1);
        let mut at = 0;
        while at < candidates.len() {
            let end = (at + per_message).min(candidates.len());
            let groups: Vec<Vec<&str>> = candidates[at..end]
                .iter()
                .map(|paths| paths.iter().map(String::as_str).collect())
                .collect();
            let borrowed: Vec<&[&str]> = groups.iter().map(Vec::as_slice).collect();
            // No reply says nothing about these names, so they are neither
            // answered nor cached: the panel asks again and the next request
            // gets a fresh attempt rather than a remembered failure.
            // Which file, not what is in it: the panel reads the bytes itself,
            // so this asks for a stat per candidate and carries none of them.
            let Some(records) = fs_read(
                client,
                state,
                remote::fs::FS_READ_FIRST | remote::fs::FS_READ_NO_CONTENT,
                icon::MAX_ICON_BYTES,
                &borrowed,
            ) else {
                break;
            };
            // One record per group, in group order.
            for (name, (status, path, _)) in asked[at..end].iter().zip(records) {
                let found = (status == remote::fs::FS_FILE_OK && icon::is_drawable_path(&path))
                    .then_some(path);
                state.cache_icon((*name).clone(), found.clone());
                resolved.insert((*name).clone(), found);
            }
            at = end;
        }
    }

    // Absolute `Icon=` values need no search, only the read.
    if !absolute.is_empty() {
        for batch in absolute.chunks(remote::fs::FS_READ_MAX_PATHS) {
            let paths: Vec<&str> = batch.iter().map(String::as_str).collect();
            let Some(records) = fs_read(
                client,
                state,
                remote::fs::FS_READ_NO_CONTENT,
                icon::MAX_ICON_BYTES,
                &[paths.as_slice()],
            ) else {
                break;
            };
            for (status, path, _) in records {
                let found = (status == remote::fs::FS_FILE_OK && icon::is_drawable_path(&path))
                    .then(|| path.clone());
                state.cache_icon(path.clone(), found.clone());
                resolved.insert(path, found);
            }
        }
    }

    batch_answers(keys, &resolved)
}

/// Expand the icon path's globs into a ranked directory list, once.
///
/// The list is the same whatever is being looked up, and expanding it is the
/// expensive half of a lookup — a dozen roots, each with two globs — so a batch
/// that had to redo it would pay for the whole icon path to answer one name.
fn refresh_icon_dirs(client: &mut Client, state: &mut State) {
    if !state.icon_dirs.is_empty() {
        return;
    }
    let theme_roots = state.icon_theme_roots.clone();
    let mut dirs: Vec<String> = Vec::new();
    for root in &theme_roots {
        // Directories only: an icon theme is a few dozen directories holding
        // tens of thousands of files, and this wants somewhere to look.
        // Following links is what makes a Nix system tree visible: every
        // directory under /run/current-system/sw/share/icons is one, so a walk
        // that stops at them reports a theme name and nothing inside it.
        let flags = remote::fs::FS_INDEX_DIRS_ONLY | remote::fs::FS_INDEX_FOLLOW_LINKS;
        for rel in fs_index(client, state, root, flags) {
            if icon::is_icon_dir(&rel) {
                dirs.push(format!("{root}/{rel}"));
            }
        }
    }
    // Pixmaps is flat — the name sits directly in it — so it is a candidate
    // without a listing. One that does not exist costs a stat per lookup and
    // answers nothing, which is the same as not being there.
    dirs.extend(state.icon_flat_roots.iter().cloned());
    let borrowed: Vec<&str> = dirs.iter().map(String::as_str).collect();
    state.icon_dirs = icon::rank_directories(&borrowed);
}

/// Send one JSON message to one connection, respecting its credit.
///
/// Reports whether the bytes actually went out, because the caller's idea of
/// what this panel has seen must track what was sent and not what was tried.
fn send_json(client: &mut Client, conn: &mut Conn, payload: &str) -> bool {
    if conn.closed {
        return false;
    }
    let bytes = payload.as_bytes();
    if bytes.len() > remote::channel::CHANNEL_MAX_PAYLOAD {
        return false;
    }
    // A panel that stopped acking gets nothing rather than unbounded queueing
    // in the guest; it resyncs when its credit returns.
    let outstanding = conn.sent.saturating_sub(conn.acked);
    if outstanding.saturating_add(bytes.len() as u64) > conn.window {
        return false;
    }
    let Some(message) = remote::channel::msg_channel_data(conn.id, bytes) else {
        return false;
    };
    if client.send(&message).is_err() {
        conn.closed = true;
        return false;
    }
    conn.sent = conn.sent.saturating_add(bytes.len() as u64);
    true
}

/// Send the current state to every attached panel that has not already seen it.
///
/// The "not already seen" is load-bearing. This runs after every packet, and a
/// panel's own ACK is a packet: publishing unconditionally means the ACK for
/// one state message provokes the next, so two idle peers trade messages as
/// fast as the round trip allows and the panel rebuilds its rows the whole
/// time.
///
/// The comparison is per connection and is only updated once the send
/// succeeds, so a panel that was out of credit is caught up by the next
/// publish instead of having the message it missed suppressed as a duplicate.
/// Send what a connection has queued, oldest first, while its credit lasts.
///
/// Stops at the first refusal rather than skipping past it: the panel matches
/// icons to rows by id, but a viewer watching them appear should see them in
/// the order they were asked for.
fn flush_queued(client: &mut Client, conn: &mut Conn) {
    // Taken rather than cloned, and drained once at the end: a queued icon is
    // 30 KB, so copying each one to send it — and memmoving the rest of the
    // queue after every send — was pure overhead on the path that matters.
    let mut sent = 0;
    while sent < conn.queued.len() {
        let payload = core::mem::take(&mut conn.queued[sent]);
        if !send_json(client, conn, &payload) {
            conn.queued[sent] = payload;
            break;
        }
        sent += 1;
    }
    if sent > 0 {
        conn.queued.drain(..sent);
    }
}

fn publish(client: &mut Client, state: &mut State) {
    if state.conns.is_empty() {
        return;
    }
    // Credit freed by an ack is why this runs on every routed packet, so it is
    // also the moment anything held back gets another try.
    let mut conns = core::mem::take(&mut state.conns);
    for conn in &mut conns {
        flush_queued(client, conn);
    }
    conns.retain(|conn| !conn.closed);
    state.conns = conns;

    let payload = state_json(state, false);
    // Nothing to say if every panel already holds this exact state.
    if state.conns.iter().all(|conn| conn.last_sent == payload) {
        return;
    }
    let mut conns = core::mem::take(&mut state.conns);
    for conn in &mut conns {
        if conn.last_sent == payload {
            continue;
        }
        if send_json(client, conn, &payload) {
            conn.last_sent.clear();
            conn.last_sent.push_str(&payload);
        }
    }
    conns.retain(|conn| !conn.closed);
    state.conns = conns;
}

/// Handle one channel packet: a panel attaching, its acks, its commands.
fn on_channel(client: &mut Client, state: &mut State, packet: &[u8]) {
    let Ok(Some(message)) = remote::channel::parse_channel_message(packet) else {
        return;
    };
    match message {
        remote::channel::ChannelMessage::Accepted {
            channel_id,
            listener_id,
            window,
            ..
        } => {
            if listener_id != state.data_listener {
                return;
            }
            // A second Accepted for a live channel id would leave two records
            // sharing one peer's credit, and both would undercount it.
            if state.conns.iter().any(|conn| conn.id == channel_id) {
                return;
            }
            let mut conn = Conn {
                id: channel_id,
                window,
                sent: 0,
                acked: 0,
                received: 0,
                closed: false,
                last_sent: String::new(),
                queued: Vec::new(),
            };
            // A panel needs the catalog once to offer anything to enable; the
            // updates that follow carry only the managed set.
            refresh_installed_if_stale(client, state);
            let greeting = state_json(state, true);
            if send_json(client, &mut conn, &greeting) {
                // The greeting carries the managed set as well as the catalog,
                // so this panel is already up to date; without recording that,
                // the very next publish would resend it verbatim.
                conn.last_sent = state_json(state, false);
            }
            state.conns.push(conn);
        }
        remote::channel::ChannelMessage::Ack { channel_id, bytes } => {
            if let Some(conn) = state.conns.iter_mut().find(|c| c.id == channel_id) {
                conn.acked = conn.acked.max(bytes);
            }
        }
        remote::channel::ChannelMessage::Closed { channel_id, .. } => {
            state.conns.retain(|conn| conn.id != channel_id);
        }
        remote::channel::ChannelMessage::Data {
            channel_id,
            payload,
        } => {
            let Some(index) = state.conns.iter().position(|c| c.id == channel_id) else {
                return;
            };
            // Ack before acting: the command's effect arrives as a state
            // message, and withholding the ack until then would stall a panel
            // whose window is exactly one command deep.
            state.conns[index].received = state.conns[index]
                .received
                .saturating_add(payload.len() as u64);
            let received = state.conns[index].received;
            let _ = client.send(&remote::channel::msg_channel_ack(channel_id, received));
            let text = String::from_utf8_lossy(payload).trim().to_string();
            let (verb, id) = match text.split_once(' ') {
                Some((verb, id)) => (verb, id.trim()),
                None => (text.as_str(), ""),
            };
            match verb {
                "enable" if !id.is_empty() => {
                    refresh_installed_if_stale(client, state);
                    if let Some(entry) = state.installed.get(id).cloned() {
                        let app = state
                            .apps
                            .entry(id.to_string())
                            .or_insert_with(|| App::new(id.to_string(), entry.argv.clone()));
                        app.argv = entry.argv;
                        app.enabled = true;
                        if app.phase == Phase::Stopped {
                            app.phase = Phase::Idle;
                        }
                        persist(client, state, id);
                    }
                }
                "disable" if !id.is_empty() => stop_app(client, state, id),
                // start and stop move an application without touching what the
                // next session start will do. That difference is the whole
                // point: trying something is not the same as choosing it.
                "start" if !id.is_empty() => {
                    refresh_installed_if_stale(client, state);
                    if let Some(entry) = state.installed.get(id).cloned() {
                        let app = state
                            .apps
                            .entry(id.to_string())
                            .or_insert_with(|| App::new(id.to_string(), entry.argv.clone()));
                        app.argv = entry.argv;
                        app.failures = 0;
                        app.next_attempt_ns = None;
                        if app.phase != Phase::Running {
                            // Transient, not enabled: the supervisor only
                            // launches what it has been told to want, and
                            // this is how a one-off run says so.
                            app.transient = true;
                            app.phase = Phase::Idle;
                        }
                    }
                }
                "stop" if !id.is_empty() => halt_app(client, state, id),
                // Artwork for the rows a panel is about to draw. Requested
                // rather than pushed with the state: the managed set is a few
                // hundred bytes and the catalog a few thousand, but their icons
                // are megabytes, and a panel showing twelve search hits wants
                // twelve of them and not the other nine hundred.
                "icons" if !id.is_empty() => {
                    // Newline-separated, because a desktop-entry id is a
                    // filename: Steam alone installs hundreds with spaces in
                    // them, and splitting on whitespace would ask for six
                    // applications that do not exist instead of one that does.
                    let requested: Vec<&str> = id
                        .split('\n')
                        .map(str::trim)
                        .filter(|id| !id.is_empty())
                        .take(MAX_ICON_REQUEST)
                        .collect();
                    let resolved = resolve_icons(client, state, &requested);
                    let Some(conn) = state.conns.get_mut(index) else {
                        return;
                    };
                    for (id, data_url) in resolved {
                        if conn.queued.len() >= MAX_QUEUED_ICONS {
                            break;
                        }
                        conn.queued.push(icon_json(&id, data_url.as_deref()));
                    }
                }
                "forget" if !id.is_empty() => forget_app(client, state, id),
                // A panel that reconnects mid-session asks for the catalog it
                // missed rather than reopening the channel.
                // An explicit resync is the operator saying the catalog is
                // wrong, so it reads through the TTL rather than around it.
                "resync" => {
                    let _ = refresh_installed(client, state);
                    let payload = state_json(state, true);
                    let mut conns = core::mem::take(&mut state.conns);
                    if let Some(conn) = conns.get_mut(index) {
                        // The greeting carries the managed set too, so what
                        // the panel now holds is this state.
                        if send_json(client, conn, &payload) {
                            conn.last_sent = state_json(state, false);
                        }
                    }
                    conns.retain(|conn| !conn.closed);
                    state.conns = conns;
                    return;
                }
                _ => return,
            }
            publish(client, state);
        }
        _ => {}
    }
}

/// Stop an application now, leaving its intent alone.
///
/// The supervisor restarts what it believes should be running, so "stopped"
/// has to be a phase it respects rather than a signal it undoes.
fn halt_app(client: &mut Client, state: &mut State, id: &str) {
    let Some(app) = state.apps.get_mut(id) else {
        return;
    };
    app.phase = Phase::Stopped;
    // Calling off a run that was never adopted: the request goes with it, or
    // the next reconcile would start what was just stopped.
    app.transient = false;
    app.next_attempt_ns = None;
    app.wayland_display = None;
    app.started_at_ns = None;
    app.process_ref = None;
    let Some(process_id) = app.process_id.take() else {
        return;
    };
    terminate(client, state, process_id);
}

/// Ask the server to end one child.
fn terminate(client: &mut Client, state: &mut State, process_id: u32) {
    let nonce = state.next_nonce();
    if let Ok(message) = remote::process::msg_process_control(remote::process::ProcessControl {
        nonce,
        process_id,
        action: remote::process::PROCESS_CONTROL_TERMINATE,
        value: 0,
    }) {
        let _ = client.send(&message);
    }
}

/// Stop one application and record that it should stay stopped.
fn stop_app(client: &mut Client, state: &mut State, id: &str) {
    let Some(app) = state.apps.get_mut(id) else {
        return;
    };
    app.enabled = false;
    // The exit will arrive and clear the rest too, but a status read in
    // between must not name a socket nothing is listening on.
    halt_app(client, state, id);
    persist(client, state, id);
}

/// Stop one application and drop it from the managed set entirely.
///
/// Disabling keeps the row: an application that just failed is worth being
/// able to look at, and its failure count is the only record of that. But a
/// row that will never be wanted again is noise the operator cannot clear, so
/// this deletes the intent rather than writing "off" over it. What is left is
/// an installed application like any other, which the catalog already offers.
fn forget_app(client: &mut Client, state: &mut State, id: &str) {
    if !state.apps.contains_key(id) {
        return;
    }
    halt_app(client, state, id);
    state.apps.remove(id);
    let nonce = state.next_nonce();
    let put = remote::kv::KvPut {
        nonce,
        // NO_CAS because the value being deleted is one this extension wrote
        // and nobody else contends for; DELETE without it and with no base is
        // refused as delete-iff-absent.
        flags: remote::kv::KV_PUT_NO_CAS | remote::kv::KV_PUT_DELETE,
        base: 0,
        key: format!("{KV_PREFIX}{id}"),
        value: Vec::new(),
    };
    let _ = client.send(&remote::kv::msg_kv_put(&put));
}

/// Interpret a packet that was not a CLI invocation.
///
/// Returns whether it may have changed something a panel or the supervisor
/// would want to act on, so the caller can skip a reconcile and a full state
/// rebuild for the traffic that changes nothing.
fn route(client: &mut Client, state: &mut State, packet: &[u8]) -> bool {
    match packet.first().copied() {
        Some(remote::channel::CHANNEL) => {
            on_channel(client, state, packet);
            true
        }
        Some(remote::S2C_HELLO) => {
            if let Some(remote::ServerMsg::Hello {
                boot_generation, ..
            }) = remote::parse_server_msg(packet)
            {
                state.boot_generation = boot_generation.unwrap_or(0);
            }
            false
        }
        // Stamped identity: the only trustworthy surface-to-application link.
        Some(remote::S2C_SURFACE_ORIGIN) => {
            if let Some(remote::ServerMsg::SurfaceOrigin {
                surface_id, app_id, ..
            }) = remote::parse_server_msg(packet)
            {
                state.surface_apps.insert(surface_id, app_id.to_string());
            }
            true
        }
        Some(remote::S2C_SURFACE_DESTROYED) => {
            if let Some(remote::ServerMsg::SurfaceDestroyed { surface_id }) =
                remote::parse_server_msg(packet)
            {
                state.surface_apps.remove(&surface_id);
            }
            true
        }
        // The spawn was accepted, and this is where its server-global
        // reference arrives. Recording and persisting it is what lets the
        // next attempt of this extension adopt the child rather than start a
        // second one beside it.
        Some(remote::process::S2C_PROCESS_STARTED) => {
            let Ok(started) = remote::process::parse_process_started(packet) else {
                return false;
            };
            // A refused spawn is the only word this will ever get: there is no
            // child, so no exit is coming either. Dropping it left the
            // application in `Running` with nothing running — for the rest of
            // the session, since `attempt_due` does not reconsider a phase it
            // believes. An entry whose `Exec` names a binary that is not there
            // is the ordinary way to reach this.
            //
            // Counted as a failed run, exactly as `reconcile` treats a launch
            // that could not even be sent: an enabled application backs off and
            // tries again, a one-off run stops.
            if started.status != remote::STATUS_OK {
                let now = client.monotonic_now().raw_nanos();
                let mut random = [0u8; 8];
                let _ = client.random(&mut random);
                if let Some(app) = state
                    .apps
                    .values_mut()
                    .find(|app| app.process_id == Some(started.process_id))
                {
                    app.note_exit(-1, now, u64::from_le_bytes(random));
                }
                return true;
            }
            let Some(app) = state
                .apps
                .values_mut()
                .find(|app| app.process_id == Some(started.process_id))
            else {
                return false;
            };
            if !app.note_process_ref(started.process_id, started.process_ref) {
                return false;
            }
            let id = app.id.clone();
            persist(client, state, &id);
            false
        }
        // A supervised application's own output. Nothing here reads it, but
        // the server bounds what it will hold unacknowledged and disconnects
        // a reader that exceeds the window -- which would take the whole
        // supervisor down with it -- so it has to be acknowledged anyway.
        Some(remote::process::S2C_PROCESS_STDOUT) => {
            if let Ok(output) = remote::process::parse_process_stdout(packet) {
                ack_output(
                    client,
                    output.process_id,
                    remote::process::PROCESS_STREAM_STDOUT,
                    output.offset + output.data.len() as u64,
                );
            }
            false
        }
        Some(remote::process::S2C_PROCESS_STDERR) => {
            if let Ok(output) = remote::process::parse_process_stderr(packet) {
                ack_output(
                    client,
                    output.process_id,
                    remote::process::PROCESS_STREAM_STDERR,
                    output.offset + output.data.len() as u64,
                );
            }
            false
        }
        Some(remote::process::S2C_PROCESS_EXIT) => {
            let Ok(exit) = remote::process::parse_process_exit(packet) else {
                return false;
            };
            let now = client.monotonic_now().raw_nanos();
            let mut random = [0u8; 8];
            let _ = client.random(&mut random);
            let random = u64::from_le_bytes(random);
            // process_id is this connection's own handle, chosen when the app
            // was spawned or adopted: the id is the app's slot.
            let Some(app) = state
                .apps
                .values_mut()
                .find(|app| app.process_id == Some(exit.process_id))
            else {
                return false;
            };
            app.note_exit(exit.code as i32, now, random);
            let id = app.id.clone();
            // Windows die with the process; dropping them here keeps `status`
            // from counting corpses if a DESTROYED is missed.
            state.surface_apps.retain(|_, owner| *owner != id);
            // The reference just became a corpse, so it must not survive into
            // the next attempt as something to adopt.
            persist(client, state, &id);
            true
        }
        _ => false,
    }
}

/// Acknowledge process output up to `through`.
///
/// The server holds at most `PROCESS_DEFAULT_STREAM_WINDOW` bytes and
/// `PROCESS_MAX_UNACKED_PACKETS` frames per stream for a reader, and kicks the
/// endpoint rather than stalling once either is exceeded. Every stdout and
/// stderr frame this extension is sent therefore has to be acknowledged, read
/// or not.
fn ack_output(client: &mut Client, process_id: u32, stream: u8, through: u64) {
    if let Ok(message) =
        remote::process::msg_process_output_ack(remote::process::ProcessOutputAck {
            process_id,
            stream,
            bytes: through,
        })
    {
        let _ = client.send(&message);
    }
}

/// Start whatever is enabled and due.
fn reconcile(client: &mut Client, state: &mut State) {
    let now = client.monotonic_now().raw_nanos();
    let due: Vec<String> = state
        .apps
        .values()
        .filter(|app| app.attempt_due(now))
        .map(|app| app.id.clone())
        .collect();
    for id in due {
        if let Err(error) = start(client, state, &id) {
            let _ = error;
            // Treat a failed launch as a failed run so it backs off rather
            // than spinning on every wake-up.
            let mut random = [0u8; 8];
            let _ = client.random(&mut random);
            if let Some(app) = state.apps.get_mut(&id) {
                app.note_exit(-1, now, u64::from_le_bytes(random));
            }
        }
    }
}

/// Mint a stamped socket for one application and spawn it onto it.
fn start(client: &mut Client, state: &mut State, id: &str) -> Result<(), Error> {
    let argv = match state.apps.get(id) {
        Some(app) => app.argv.clone(),
        None => return Ok(()),
    };
    if argv.is_empty() {
        return Ok(());
    }
    // A fresh instance id per attempt, so a socket left behind by a crashed
    // predecessor is never mistaken for this one.
    let mut bytes = [0u8; 8];
    client.random(&mut bytes).map_err(Error::from)?;
    let instance: String = bytes.iter().map(|b| format!("{b:02x}")).collect();

    let nonce = state.next_nonce();
    client
        .send(&remote::msg_app_socket_request(nonce, id, &instance))
        .map_err(Error::from)?;
    let reply = client
        .recv_matching(|packet| {
            remote::parse_app_socket_reply(packet).is_some_and(|(n, ..)| n == nonce)
        })
        .map_err(Error::from)?
        .ok_or(Error::Client(blit_guest::Error::EndpointClosed))?;
    let (_, status, display) = remote::parse_app_socket_reply(&reply)
        .ok_or(Error::InvalidInvocation("malformed app socket reply"))?;
    if status != remote::STATUS_OK {
        return Err(Error::InvalidInvocation("app socket refused"));
    }
    let display = display.to_string();

    // The process id is this connection's handle for the child; use a stable
    // per-app value so an exit can be attributed without a side table.
    // A fresh id per attempt, never derived from the app's position in the
    // catalog: enabling one application reorders the map, which would silently
    // renumber every application sorted after it while their children keep
    // running under the old id. An exit would then be attributed to the wrong
    // app, and a disable would terminate whoever had inherited the slot.
    let process_id = state.next_process_id();
    let argv_bytes: Vec<&[u8]> = argv.iter().map(|arg| arg.as_bytes()).collect();
    let display_bytes = display.as_bytes();
    let request = remote::process::ProcessSpawnRequest {
        nonce: state.next_nonce(),
        process_id,
        // SESSION_ENV supplies the bus, audio and toolkit steering;
        // DETACHABLE keeps the app alive across a restart of this extension,
        // which is why re-adoption below has to exist.
        flags: remote::process::PROCESS_SPAWN_SESSION_ENV
            | remote::process::PROCESS_SPAWN_DETACHABLE,
        cwd_kind: remote::process::PROCESS_CWD_DEFAULT,
        src_pty_id: 0,
        cwd: b"",
        argv: argv_bytes,
        // Wins over the session's shared socket — this is what puts the app on
        // its own stamped one.
        env: vec![(b"WAYLAND_DISPLAY".as_slice(), display_bytes)],
    };
    let message = remote::process::msg_process_spawn(&request)
        .map_err(|_| Error::InvalidInvocation("invalid spawn request"))?;
    client.send(&message).map_err(Error::from)?;

    let now = client.monotonic_now().raw_nanos();
    if let Some(app) = state.apps.get_mut(id) {
        // The server-global reference is not known yet; `PROCESS_STARTED`
        // brings it, and `route` persists it when it lands.
        app.note_started(process_id, display, now);
    }
    Ok(())
}

/// Reload intent from kv and re-adopt anything still running.
fn restore(client: &mut Client, state: &mut State) -> Result<(), Error> {
    refresh_installed(client, state)?;
    let stored = read_intent(client, state)?;
    for (id, intent) in &stored {
        // An application that has since been uninstalled keeps its row: the
        // intent is the operator's, and losing it silently because a package
        // was upgraded out from under the session is worse than a row whose
        // argv is empty until the package comes back.
        let argv = state
            .installed
            .get(id)
            .map(|entry| entry.argv.clone())
            .unwrap_or_default();
        let mut app = App::new(id.clone(), argv);
        app.enabled = intent.enabled;
        app.phase = if app.enabled {
            Phase::Idle
        } else {
            Phase::Stopped
        };
        state.apps.insert(id.clone(), app);
    }
    adopt(client, state, &stored)?;
    Ok(())
}

/// Read every persisted intent under [`KV_PREFIX`] in one exchange.
///
/// A subscription rather than a fetch per application: the previous shape cost
/// one blocking round trip for each installed `.desktop` file — hundreds on an
/// ordinary desktop — and could only ever find intent for applications that
/// were still installed, because the catalog was what it iterated.
fn read_intent(client: &mut Client, state: &mut State) -> Result<BTreeMap<String, Intent>, Error> {
    let nonce = state.next_nonce();
    // Every value here is a handful of bytes, so ask for them inline and the
    // snapshot is the whole answer.
    client
        .send(&remote::kv::msg_kv_open(nonce, 0, 4096, KV_PREFIX))
        .map_err(Error::from)?;
    let opened = client
        .recv_matching(|packet| {
            remote::kv::parse_kv_opened(packet).is_some_and(|(n, ..)| n == nonce)
        })
        .map_err(Error::from)?
        .ok_or(Error::Client(blit_guest::Error::EndpointClosed))?;
    let Some((_, kv_id, status, _)) = remote::kv::parse_kv_opened(&opened) else {
        return Err(Error::InvalidInvocation("malformed kv open reply"));
    };
    if status != remote::kv::KV_STATUS_OK {
        return Err(Error::InvalidInvocation("kv subscription refused"));
    }

    let mut mirror = remote::kv::KvMirror::new();
    while !mirror.snapshot_done {
        let update = client
            .recv_matching(|packet| {
                matches!(
                    packet.first().copied(),
                    Some(remote::kv::S2C_KV_UPDATE) | Some(remote::kv::S2C_KV_CLOSED)
                ) && packet.get(1..3) == Some(&kv_id.to_le_bytes()[..])
            })
            .map_err(Error::from)?
            .ok_or(Error::Client(blit_guest::Error::EndpointClosed))?;
        if update.first() == Some(&remote::kv::S2C_KV_CLOSED) {
            return Err(Error::InvalidInvocation("kv subscription closed"));
        }
        let Some(update_id) = mirror.apply_update(&update) else {
            continue;
        };
        // Ack is not optional: the server bounds unacked updates.
        let _ = client.send(&remote::kv::msg_kv_ack(kv_id, update_id));
    }
    // Nothing watches this prefix afterwards -- the extension is the only
    // writer -- so the subscription is released rather than left to idle.
    let _ = client.send(&remote::kv::msg_kv_stop(kv_id));

    let mut stored = BTreeMap::new();
    for (key, entry) in &mirror.live {
        let Some(id) = key.strip_prefix(KV_PREFIX) else {
            continue;
        };
        if id.is_empty() {
            continue;
        }
        let Some(value) = entry.value.as_ref() else {
            continue;
        };
        if let Some(intent) = parse_intent(value) {
            stored.insert(id.to_string(), intent);
        }
    }
    Ok(stored)
}

/// Re-adopt the children this extension's previous attempt left running.
///
/// `PROCESS_SPAWN_DETACHABLE` keeps a supervised application alive across a
/// restart of the extension, so without this every restart would start a
/// second copy of everything enabled and leave the first orphaned. A recorded
/// reference is only trusted when the server still lists it as live *and* it
/// was recorded under this boot generation — the numbers are reused across
/// server runs, so a stale one would adopt an unrelated process.
fn adopt(
    client: &mut Client,
    state: &mut State,
    stored: &BTreeMap<String, Intent>,
) -> Result<(), Error> {
    let wanted: BTreeMap<u64, String> = stored
        .iter()
        .filter(|(_, intent)| intent.boot_generation == state.boot_generation)
        .filter_map(|(id, intent)| intent.process_ref.map(|reference| (reference, id.clone())))
        .collect();
    if wanted.is_empty() {
        return Ok(());
    }

    let nonce = state.next_nonce();
    client
        .send(&remote::process::msg_process_list(
            remote::process::ProcessList { nonce },
        ))
        .map_err(Error::from)?;
    let listed = client
        .recv_matching(|packet| {
            remote::process::parse_process_listed(packet).is_ok_and(|listed| listed.nonce == nonce)
        })
        .map_err(Error::from)?
        .ok_or(Error::Client(blit_guest::Error::EndpointClosed))?;
    let listed = remote::process::parse_process_listed(&listed)
        .map_err(|_| Error::InvalidInvocation("malformed process list"))?;
    if listed.status != remote::STATUS_OK {
        return Ok(());
    }

    let live: Vec<u64> = listed
        .entries
        .iter()
        .filter(|entry| entry.state == remote::process::PROCESS_STATE_RUNNING)
        .map(|entry| entry.process_ref)
        .filter(|reference| wanted.contains_key(reference))
        .collect();
    let now = client.monotonic_now().raw_nanos();
    for process_ref in live {
        let Some(id) = wanted.get(&process_ref) else {
            continue;
        };
        let process_id = state.next_process_id();
        let nonce = state.next_nonce();
        let watch = remote::process::msg_process_watch(remote::process::ProcessWatch {
            nonce,
            process_id,
            process_ref,
            flags: 0,
        })
        .map_err(|_| Error::InvalidInvocation("invalid watch request"))?;
        client.send(&watch).map_err(Error::from)?;
        let watched = client
            .recv_matching(|packet| {
                remote::process::parse_process_watched(packet)
                    .is_ok_and(|watched| watched.nonce == nonce)
            })
            .map_err(Error::from)?
            .ok_or(Error::Client(blit_guest::Error::EndpointClosed))?;
        let Ok(watched) = remote::process::parse_process_watched(&watched) else {
            continue;
        };
        // A refused or already-exited watch means there is nothing to adopt,
        // and the app stays Idle so `reconcile` starts it normally.
        if watched.status != remote::STATUS_OK
            || watched.state != remote::process::PROCESS_STATE_RUNNING
        {
            continue;
        }
        if let Some(app) = state.apps.get_mut(id) {
            // The socket this instance was given is not recorded, so `status`
            // reports the app as running without naming one rather than
            // naming one it cannot vouch for.
            app.note_adopted(process_id, process_ref, None, now);
        }
    }
    Ok(())
}

/// Read the catalog again if it has aged past [`CATALOG_TTL`].
///
/// The catalog is not watched, so the only alternatives are re-reading it on
/// every request — a shell child and a directory walk per keystroke — or the
/// previous rule of reading it once and never again, which made an
/// application installed mid-session permanently invisible to `enable`.
fn refresh_installed_if_stale(client: &mut Client, state: &mut State) {
    let now = client.monotonic_now().raw_nanos();
    let fresh = state.installed_at_ns.is_some_and(|read_at| {
        now.saturating_sub(read_at) < CATALOG_TTL.as_nanos() as i64 && !state.installed.is_empty()
    });
    if fresh {
        return;
    }
    let _ = refresh_installed(client, state);
}

/// Read the installed applications: `XDG_DATA_DIRS` from the server's
/// environment, then the `.desktop` files under each.
fn refresh_installed(client: &mut Client, state: &mut State) -> Result<(), Error> {
    let nonce = state.next_nonce();
    client
        .send(&remote::env::msg_env_get(nonce))
        .map_err(Error::from)?;
    let reply = client
        .recv_matching(|packet| {
            remote::env::parse_env(packet).is_ok_and(|reply| reply.nonce == nonce)
        })
        .map_err(Error::from)?
        .ok_or(Error::Client(blit_guest::Error::EndpointClosed))?;
    let reply = remote::env::parse_env(&reply)
        .map_err(|_| Error::InvalidInvocation("malformed env reply"))?;
    let get = |key: &str| {
        reply
            .entries
            .get(key.as_bytes())
            .and_then(|value| core::str::from_utf8(value).ok())
            .map(str::to_string)
    };
    // The spec's defaults, so a session with these unset still finds apps.
    let home = get("XDG_DATA_HOME").unwrap_or_else(|| {
        let base = get("HOME").unwrap_or_default();
        format!("{base}/.local/share")
    });
    let dirs = get("XDG_DATA_DIRS").unwrap_or_else(|| "/usr/local/share:/usr/share".to_string());
    // The icon path is the data path, so it is settled by the read that already
    // happened rather than by one of its own.
    let (theme_roots, flat_roots) = icon::roots(&home, &get("HOME").unwrap_or_default(), &dirs);
    state.icon_theme_roots = theme_roots;
    state.icon_flat_roots = flat_roots;
    // A package installed mid-session can bring a theme with it, and this read
    // is the moment that becomes visible.
    state.icon_dirs.clear();

    state.installed.clear();
    let roots: Vec<String> = core::iter::once(home.as_str())
        .chain(dirs.split(':'))
        .filter(|base| !base.is_empty())
        .map(|base| format!("{base}/applications"))
        .collect();
    {
        for (path, contents) in read_desktop_files(client, state, &roots) {
            let Some(id) = path
                .rsplit('/')
                .next()
                .and_then(|name| name.strip_suffix(".desktop"))
            else {
                continue;
            };
            let Some(entry) = desktop_entry::parse(id, &contents) else {
                continue;
            };
            if entry.hidden || entry.terminal {
                continue;
            }
            // Earlier directories win, per the spec's precedence.
            state.installed.entry(entry.id.clone()).or_insert(entry);
        }
    }
    state.installed_at_ns = Some(client.monotonic_now().raw_nanos());
    Ok(())
}

/// Largest `.desktop` file worth reading. The spec's entries are a few hundred
/// bytes; anything at this size is not one, and reading it would only crowd out
/// the rest of the batch.
const MAX_DESKTOP_BYTES: u32 = 64 * 1024;

/// Read every `*.desktop` under a set of directories.
///
/// Two messages per root and no child process: `FS_INDEX` for what is there,
/// then `FS_READ` for the entries themselves. This used to be a shell loop over
/// a glob with `cat`, because the fs family could only read inside an established
/// sync session — a watched tree, the wrong shape for reading a fixed set of
/// files once at startup. A root that does not exist answers an empty listing,
/// which matters because most of `XDG_DATA_DIRS` is absent on any given machine.
fn read_desktop_files(
    client: &mut Client,
    state: &mut State,
    roots: &[String],
) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for root in roots {
        // Root-relative, so a nested `kde4/foo.desktop` keeps its subdirectory.
        let entries: Vec<String> = fs_index(client, state, root, 0)
            .into_iter()
            .filter(|rel| rel.ends_with(".desktop"))
            .map(|rel| format!("{root}/{rel}"))
            .collect();
        for batch in entries.chunks(remote::fs::FS_READ_MAX_PATHS) {
            let paths: Vec<&str> = batch.iter().map(String::as_str).collect();
            let Some(records) = fs_read(client, state, 0, MAX_DESKTOP_BYTES, &[paths.as_slice()])
            else {
                return out;
            };
            for (status, path, body) in records {
                if status != remote::fs::FS_FILE_OK {
                    continue;
                }
                // Lossy on purpose: one entry with a stray byte in it must not
                // discard the file, let alone the catalog.
                out.push((path, String::from_utf8_lossy(&body).into_owned()));
            }
        }
    }
    out
}

/// Read a batch of paths in one round trip (`docs/design/fs-read.md`).
///
/// Every record comes back in request order with its own status, so a missing or
/// oversized file is an answer about that path rather than a failure of the
/// batch. `None` means the request never got a reply — the connection is going
/// away — which is the only case a caller has to treat as "ask again later".
fn fs_read(
    client: &mut Client,
    state: &mut State,
    flags: u8,
    max_bytes: u32,
    groups: &[&[&str]],
) -> Option<Vec<(u8, String, Vec<u8>)>> {
    let nonce = fs_read_send(client, state, flags, max_bytes, groups)?;
    fs_read_collect(client, nonce)
}

/// Ask for a batch and return the nonce its answer will carry.
///
/// Split from the collection so a caller with many independent questions can put
/// them all on the wire before waiting for any: the icon search is one request
/// per name, and asking them one at a time made a screenful of artwork a
/// screenful of round trips.
fn fs_read_send(
    client: &mut Client,
    state: &mut State,
    flags: u8,
    max_bytes: u32,
    groups: &[&[&str]],
) -> Option<u16> {
    let nonce = state.next_nonce();
    let request = remote::fs::msg_fs_read(nonce, flags, max_bytes, groups)?;
    client.send(&request).ok()?;
    Some(nonce)
}

/// Collect one outstanding [`fs_read_send`]. Answers to the others wait in the
/// client's pending queue, so they can be collected in any order.
fn fs_read_collect(client: &mut Client, nonce: u16) -> Option<Vec<(u8, String, Vec<u8>)>> {
    // The predicate reads the nonce out of the header instead of parsing the
    // packet. `recv_matching` runs it over every reply already queued, and
    // parsing decompresses a whole batch of file content to look at two bytes:
    // with one request per name and a screenful in flight, that alone made a
    // batch quadratic — 48 icons spent 8.8s decompressing answers that had
    // already arrived.
    let wanted = nonce.to_le_bytes();
    let reply = client
        .recv_matching(|packet| {
            packet.first().copied() == Some(remote::fs::S2C_FS_READ)
                && packet.get(1..3) == Some(&wanted[..])
        })
        .ok()??;
    let (_, status, records) = remote::fs::parse_fs_read_result(&reply)?;
    if status != remote::fs::FS_DONE_OK {
        return Some(Vec::new());
    }
    Some(records)
}

/// Everything under `root`, root-relative: its files, or its directories.
///
/// The walk is the server's (`FS_INDEX`), which is what makes a directory
/// listing a message rather than a glob. A truncated answer is used as far as it
/// goes: for both callers here that means some applications or some themes, not
/// a wrong answer.
fn fs_index(client: &mut Client, state: &mut State, root: &str, flags: u8) -> Vec<String> {
    let nonce = state.next_nonce();
    let mut request = remote::fs::msg_fs_index(nonce, root);
    // The flags byte sits after the opcode and nonce; the builder writes a zero
    // there because the search-and-index callers want plain files.
    request[3] = flags;
    if client.send(&request).is_err() {
        return Vec::new();
    }
    let wanted = nonce.to_le_bytes();
    let Ok(Some(reply)) = client.recv_matching(|packet| {
        packet.first().copied() == Some(remote::fs::S2C_FS_INDEX)
            && packet.get(1..3) == Some(&wanted[..])
    }) else {
        return Vec::new();
    };
    match remote::fs::parse_fs_index_result(&reply) {
        Some((_, status, _, paths)) if status == remote::fs::FS_DONE_OK => paths,
        _ => Vec::new(),
    }
}

/// Persist one application's intent.
///
/// The record is `<enabled> [<boot-generation> <process-ref>]`: the operator's
/// choice, plus the handle a restarted extension needs to re-adopt the child
/// instead of spawning a second one. A bare `0`/`1` — everything written
/// before the reference existed — still parses.
fn persist(client: &mut Client, state: &mut State, id: &str) {
    let Some(app) = state.apps.get(id) else {
        return;
    };
    let mut value = String::from(if app.enabled { "1" } else { "0" });
    if let Some(process_ref) = app.process_ref {
        value.push_str(&format!(" {} {process_ref}", state.boot_generation));
    }
    let nonce = state.next_nonce();
    let put = remote::kv::KvPut {
        nonce,
        flags: remote::kv::KV_PUT_NO_CAS,
        base: 0,
        key: format!("{KV_PREFIX}{id}"),
        value: value.into_bytes(),
    };
    let _ = client.send(&remote::kv::msg_kv_put(&put));
}

/// Read one persisted intent record back.
fn parse_intent(value: &[u8]) -> Option<Intent> {
    let text = core::str::from_utf8(value).ok()?;
    let mut fields = text.split_whitespace();
    let enabled = match fields.next()? {
        "1" => true,
        "0" => false,
        _ => return None,
    };
    // The two halves are written together and are meaningless apart, so a
    // record carrying only one of them carries neither. A tail that does not
    // parse costs the reference and nothing else: the enabled bit in front of
    // it is the operator's choice, and dropping that would silently
    // un-autostart the application.
    let (boot_generation, process_ref) = match (fields.next(), fields.next()) {
        (Some(generation), Some(reference)) => {
            match (generation.parse::<u64>(), reference.parse::<u64>()) {
                (Ok(generation), Ok(reference)) => (generation, Some(reference)),
                _ => (0, None),
            }
        }
        _ => (0, None),
    };
    Some(Intent {
        enabled,
        boot_generation,
        process_ref,
    })
}

fn serve(
    client: &mut Client,
    state: &mut State,
    mut invocation: blit_guest::command::Invocation,
) -> Result<(), Error> {
    let args = invocation.request().args.clone();
    let (command, target) = (
        args.first().map(String::as_str).unwrap_or("list"),
        args.get(1).map(String::as_str),
    );
    let mut out = String::new();
    let mut code = 0;

    match (command, target) {
        ("list", _) => {
            refresh_installed_if_stale(client, state);
            out.push_str("APP\tENABLED\tPHASE\tNAME\n");
            // A managed application that is no longer installed still has a
            // row: it is the only way to see that something enabled has gone
            // missing, and the only way to `forget` it.
            let ids: BTreeSet<&String> = state.installed.keys().chain(state.apps.keys()).collect();
            for id in ids {
                let app = state.apps.get(id);
                let enabled = app.is_some_and(|app| app.enabled);
                let phase = match app.map(|app| app.phase) {
                    Some(Phase::Running) => "running",
                    Some(Phase::Backoff) => "backoff",
                    Some(Phase::Idle) => "starting",
                    _ => "-",
                };
                let name = state
                    .installed
                    .get(id)
                    .map(|entry| entry.name.as_str())
                    .unwrap_or("(not installed)");
                out.push_str(&format!(
                    "{id}\t{}\t{phase}\t{name}\n",
                    if enabled { "yes" } else { "no" },
                ));
            }
        }
        ("enable", Some(id)) => {
            refresh_installed_if_stale(client, state);
            match state.installed.get(id) {
                Some(entry) => {
                    let app = state
                        .apps
                        .entry(id.to_string())
                        .or_insert_with(|| App::new(id.to_string(), entry.argv.clone()));
                    app.argv = entry.argv.clone();
                    app.enabled = true;
                    if app.phase == Phase::Stopped {
                        app.phase = Phase::Idle;
                    }
                    persist(client, state, id);
                    out.push_str(&format!("enabled {id}\n"));
                }
                None => {
                    out.push_str(&format!("no application called {id}\n"));
                    code = 1;
                }
            }
        }
        ("disable", Some(id)) => {
            if state.apps.contains_key(id) {
                stop_app(client, state, id);
                out.push_str(&format!("disabled {id}\n"));
            } else {
                out.push_str(&format!("{id} was not enabled\n"));
                code = 1;
            }
        }
        // start and stop are this session only. Intent is untouched, so
        // `stop` on an enabled application stays stopped until something asks
        // otherwise -- and the next session start still brings it up.
        ("start", Some(id)) => {
            refresh_installed_if_stale(client, state);
            match state.installed.get(id) {
                Some(entry) => {
                    let argv = entry.argv.clone();
                    let app = state
                        .apps
                        .entry(id.to_string())
                        .or_insert_with(|| App::new(id.to_string(), argv.clone()));
                    app.argv = argv;
                    app.failures = 0;
                    app.next_attempt_ns = None;
                    if app.phase != Phase::Running {
                        // See the channel's `start`: without this the
                        // supervisor has been told nothing it acts on.
                        app.transient = true;
                        app.phase = Phase::Idle;
                    }
                    out.push_str(&format!("starting {id}\n"));
                }
                None => {
                    out.push_str(&format!("no application called {id}\n"));
                    code = 1;
                }
            }
        }
        ("stop", Some(id)) => {
            if state.apps.contains_key(id) {
                halt_app(client, state, id);
                out.push_str(&format!("stopped {id}\n"));
            } else {
                out.push_str(&format!("{id} is not running\n"));
                code = 1;
            }
        }
        // Forgetting is not disabling: the row goes away, and with it the
        // failure history that made keeping a disabled one worth it.
        ("forget", Some(id)) => {
            if state.apps.contains_key(id) {
                forget_app(client, state, id);
                out.push_str(&format!("forgot {id}\n"));
            } else {
                out.push_str(&format!("{id} was not managed\n"));
                code = 1;
            }
        }
        ("status", Some(id)) => match state.apps.get(id) {
            Some(app) => {
                let windows = state
                    .surface_apps
                    .values()
                    .filter(|owner| owner.as_str() == id)
                    .count();
                out.push_str(&format!("app\t{id}\n"));
                out.push_str(&format!(
                    "enabled\t{}\n",
                    if app.enabled { "yes" } else { "no" }
                ));
                out.push_str(&format!(
                    "phase\t{}\n",
                    match app.phase {
                        Phase::Running => "running",
                        Phase::Backoff => "backoff",
                        Phase::Idle => "starting",
                        Phase::Stopped => "stopped",
                    }
                ));
                out.push_str(&format!("failures\t{}\n", app.failures));
                if let Some(exit) = app.last_exit {
                    out.push_str(&format!("last-exit\t{exit}\n"));
                }
                if let Some(display) = &app.wayland_display {
                    out.push_str(&format!("socket\t{display}\n"));
                }
                // Counted from stamped identity, not from a self-asserted
                // app_id — the whole reason this number can be trusted.
                out.push_str(&format!("windows\t{windows}\n"));
            }
            None => {
                out.push_str(&format!("{id} is not managed\n"));
                code = 1;
            }
        },
        (other, None)
            if matches!(
                other,
                "enable" | "disable" | "start" | "stop" | "forget" | "status"
            ) =>
        {
            out.push_str(&format!("{other} needs an application name\n"));
            code = 2;
        }
        (other, _) => {
            out.push_str(&format!("unknown command {other}\n"));
            code = 2;
        }
    }

    invocation.stdout(client, out.as_bytes())?;
    invocation.exit(client, code, "")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Records written before the process reference existed are still out
    /// there, and reading one as "not enabled" would silently un-autostart
    /// every application on the first upgrade.
    #[test]
    fn a_bare_enabled_bit_still_parses() {
        let intent = parse_intent(b"1").expect("parses");
        assert!(intent.enabled);
        assert_eq!(intent.process_ref, None);
        assert!(!parse_intent(b"0").expect("parses").enabled);
        assert!(parse_intent(b"").is_none());
        assert!(parse_intent(b"yes").is_none());
    }

    #[test]
    fn a_full_record_round_trips() {
        let intent = parse_intent(b"1 7 12345").expect("parses");
        assert!(intent.enabled);
        assert_eq!(intent.boot_generation, 7);
        assert_eq!(intent.process_ref, Some(12345));
    }

    /// The generation and the reference are only meaningful together: a
    /// reference without the generation it was recorded under could be
    /// matched against a different server's process of the same number.
    #[test]
    fn half_a_reference_is_no_reference() {
        let intent = parse_intent(b"1 7").expect("parses");
        assert!(intent.enabled, "the intent itself still counts");
        assert_eq!(intent.process_ref, None);
        assert_eq!(intent.boot_generation, 0);

        // Unparseable halves are dropped rather than guessed at.
        let intent = parse_intent(b"1 seven 12345").expect("parses");
        assert_eq!(intent.process_ref, None);
        let intent = parse_intent(b"1 7 many").expect("parses");
        assert_eq!(intent.process_ref, None);
    }

    /// The bug that put a permanent gap in a scrolled list: the icon cache is
    /// dropped wholesale, and it can happen part way through the batch that is
    /// filling it. Whatever the batch resolved before the drop must still be
    /// answered, because "no artwork" is a final answer to the panel.
    #[test]
    fn a_batch_answers_what_it_resolved_even_if_the_cache_was_dropped() {
        // What the batch resolved, held apart from the cache the drop emptied.
        let resolved = BTreeMap::from([
            (
                "early".to_string(),
                Some("data:image/png;base64,AA".to_string()),
            ),
            (
                "late".to_string(),
                Some("data:image/png;base64,BB".to_string()),
            ),
        ]);
        let keys = vec![
            ("Portal 2.desktop".to_string(), Some("early".to_string())),
            ("Shatter.desktop".to_string(), Some("late".to_string())),
        ];
        assert_eq!(
            batch_answers(keys, &resolved),
            vec![
                (
                    "Portal 2.desktop".to_string(),
                    Some("data:image/png;base64,AA".to_string())
                ),
                (
                    "Shatter.desktop".to_string(),
                    Some("data:image/png;base64,BB".to_string())
                ),
            ]
        );
    }

    /// "No artwork" and "ask me later" are different answers, and only one of
    /// them is safe to give for a read that never happened: the panel treats a
    /// null as final, so a failed child must leave the id unanswered.
    #[test]
    fn an_unresolved_key_is_omitted_but_a_missing_icon_field_is_answered() {
        let resolved = BTreeMap::from([("found".to_string(), None)]);
        let answers = batch_answers(
            vec![
                ("no-icon-key.desktop".to_string(), None),
                ("looked-for.desktop".to_string(), Some("found".to_string())),
                (
                    "child-failed.desktop".to_string(),
                    Some("never".to_string()),
                ),
            ],
            &resolved,
        );
        assert_eq!(
            answers,
            vec![
                // Nothing to look for: final.
                ("no-icon-key.desktop".to_string(), None),
                // Looked for and not found: also final.
                ("looked-for.desktop".to_string(), None),
            ],
            "the id whose lookup never ran is left for the panel to re-ask"
        );
    }

    /// A disabled application can still have a live child -- `disable` stops
    /// it, but the exit arrives later -- so the two halves are independent.
    #[test]
    fn a_disabled_record_can_still_carry_a_reference() {
        let intent = parse_intent(b"0 3 99").expect("parses");
        assert!(!intent.enabled);
        assert_eq!(intent.process_ref, Some(99));
    }
}
