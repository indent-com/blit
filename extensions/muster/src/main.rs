//! `@muster` — the protocol half: one receive loop that services CLI
//! invocations, unit exits, filesystem changes, and readiness deadlines
//! together.
//!
//! The shape matters. Every blocking entry point in the SDK waits for *its own*
//! packet, so an extension parked in `CommandProvider::accept` cannot notice a
//! unit that died and cannot let a backoff deadline come due. So this owns the
//! loop — `wait_until(next deadline)`, then `recv`, then route by opcode.
//!
//! The CLI channel is handled manually rather than through `CommandProvider::offer`,
//! because `offer` blocks in `Invocation::begin` waiting for INVOKE. That wait
//! can miss the DATA packet if it arrives while the loop is between `recv` and
//! `offer`, or it can stall the whole supervisor while a channel sits open. The
//! listener is still registered through the SDK so the descriptor is published;
//! after that, `CHANNEL_ACCEPTED`, `DATA`, `ACK` and `CLOSED` are routed here.

use blit_ext_muster::config::{
    self, ConfigError, InstanceFile, ReadyWhen, StackFile, TopLevel, UnitFile, UnitType,
    WorktreeSourceFile,
};
use blit_ext_muster::envfile::{self, EnvFile, Origin};
use blit_ext_muster::journal::{Cause, Event, Journal, Record};
use blit_ext_muster::supervisor::{self, DependentAction, Phase, Run, Unit};
use blit_ext_muster::worktrees::{self, PortLedger};
use blit_guest::remote::extension::{
    EXT_INFO, EXT_INFO_COMMAND_REGISTERED, ExtensionInfo, ExtensionMessage,
    msg_extension_command_register, parse_extension_message,
};
use blit_guest::remote::{self, ServerMsg};
use blit_guest::terminal::{CreateRequest, TerminalSubscriptions};
use blit_guest::{Client, EXIT_BOOTSTRAP_FAILURE, MonotonicInstant, WaitOutcome};
use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

const DESCRIPTOR: &str = r#"{
  "protocol":"blit.cli.v1",
  "summary":"Supervise units that run in terminals",
  "commands":[
    {"path":["list"],"summary":"Every unit and instance, and what it is doing",
     "usage":"blit @muster list [--json]"},
    {"path":["status"],"summary":"One unit or instance, with its retained runs",
     "usage":"blit @muster status <name> [--json]"},
    {"path":["start"],"summary":"Start a unit or an instance now",
     "usage":"blit @muster start <name>"},
    {"path":["stop"],"summary":"Stop a unit or an instance and hold it",
     "usage":"blit @muster stop <name>"},
    {"path":["restart"],"summary":"Stop and start, in a new terminal",
     "usage":"blit @muster restart <name>"},
    {"path":["instantiate"],"summary":"Write an instance of a stack, and start it",
     "usage":"blit @muster instantiate <stack> <name> [VAR=VALUE ...] [--no-start] [--force] [--json]"},
    {"path":["reload"],"summary":"Ask a unit to re-read its own configuration",
     "usage":"blit @muster reload <name>"},
    {"path":["rewatch"],"summary":"Retry the directories whose watch the server refused",
     "usage":"blit @muster rewatch"},
    {"path":["ready"],"summary":"Declare a readyWhen:manual unit ready",
     "usage":"blit @muster ready <unit>"},
    {"path":["log"],"summary":"The supervision journal",
     "usage":"blit @muster log [-n N] [-u NAME] [--since SEQ] [--json]"},
    {"path":["cat"],"summary":"The file behind a unit or instance",
     "usage":"blit @muster cat <name>"},
    {"path":["env"],"summary":"The environment a start would resolve",
     "usage":"blit @muster env <unit> [--values] [--json]"},
    {"path":["stacks"],"summary":"Stacks and their parameters",
     "usage":"blit @muster stacks [--json]"},
    {"path":["doctor"],"summary":"Everything wrong with the directory",
     "usage":"blit @muster doctor [--json]"},
    {"path":["schema"],"summary":"The JSON Schema for a unit file",
     "usage":"blit @muster schema"}
  ]
}"#;

/// Terminals are created at a fixed size: nothing subscribes to them here, and
/// a client that attaches resizes to its own pane.
const ROWS: u16 = 40;
const COLS: u16 = 120;

/// How long the filesystem watch coalesces changes before reporting them.
/// Enough that saving a file is one event rather than one per write.
const SETTLE_MS: u16 = 200;

/// How soon a directory that could not be watched is tried again, and the
/// ceiling that backoff climbs to.
///
/// The common cause is a pointer written before its target exists — a stack in
/// a worktree that has not been created yet — and the person who is about to
/// create it is standing right there, so the first retry is quick. A pointer at
/// a directory that will never exist should not cost a sync every five seconds
/// forever, hence the climb.
const REWATCH_MS: u64 = 5_000;
const REWATCH_MAX_MS: u64 = 60_000;

/// How often a `path`/`tcp`/`http` probe is retried while activating.
const PROBE_INTERVAL: Duration = Duration::from_millis(250);
/// `log` polls faster: it is racing a ring buffer, not a listening socket.
const LOG_PROBE_INTERVAL: Duration = Duration::from_millis(100);
/// An idle tick, so a directory that changed without an event is still noticed.
const IDLE_TICK: Duration = Duration::from_secs(30);

/// One durable record is enough: muster is its only writer, and a single CAS
/// keeps allocations across every configured repository consistent.
const PORT_LEDGER_KEY: &str = "ext/muster/worktree-ports/v1";

/// Mirror only Git's linked-worktree pointers. Watching all of `.git` would
/// index object storage and every ref merely to learn when one tiny `gitdir`
/// file appears or disappears.
const GIT_WORKTREES_EXCLUDE: &str = "*\n!worktrees/\n!worktrees/*/\n!worktrees/*/gitdir\n";

fn main() {}

fn register_descriptor(client: &mut Client, listener_id: u32) -> Result<(), String> {
    let nonce = 1;
    let request = msg_extension_command_register(nonce, listener_id, DESCRIPTOR)
        .ok_or("invalid command descriptor")?;
    client
        .send(&request)
        .map_err(|error| format!("command register: {error:?}"))?;
    let packet = client
        .recv_matching(|packet| {
            packet.first() == Some(&EXT_INFO)
                && packet.get(1) == Some(&EXT_INFO_COMMAND_REGISTERED)
                && matches!(
                    parse_extension_message(packet),
                    Ok(Some(ExtensionMessage::Info(
                        ExtensionInfo::CommandRegistered(registered)
                    ))) if registered.nonce == nonce
                )
        })
        .map_err(|error| format!("command register reply: {error:?}"))?
        .ok_or("endpoint closed during command registration")?;
    match parse_extension_message(&packet) {
        Ok(Some(ExtensionMessage::Info(ExtensionInfo::CommandRegistered(registered))))
            if registered.status == 0 =>
        {
            if registered.extension_id != client.context().extension_id
                || registered.definition_revision != client.context().definition_revision
            {
                Err(String::from("command registration identity mismatch"))
            } else {
                Ok(())
            }
        }
        Ok(Some(ExtensionMessage::Info(ExtensionInfo::CommandRegistered(registered)))) => {
            Err(format!(
                "command registration refused: status {} {}",
                registered.status, registered.detail
            ))
        }
        _ => Err(String::from("unexpected command registration reply")),
    }
}

pub(crate) fn ext_log(client: &mut Client, msg: &str) {
    if let Some(packet) =
        remote::extension::msg_extension_event(remote::extension::EXT_EVENT_LOG, msg.as_bytes())
    {
        let _ = client.send(&packet);
    }
}

// Not `blit_guest::entry!`: that bootstraps with `Client::bootstrap()`, which
// discards the initial burst. `S2C_LIST` arrives exactly once, before `READY`,
// and there is no request that asks for it again — it is the only way to find
// the terminals a previous supervisor left running.
blit_guest::register_getrandom!();

#[unsafe(export_name = "blit_main")]
pub extern "C" fn __blit_guest_main() -> i32 {
    let mut initial: Vec<Vec<u8>> = Vec::new();
    match Client::bootstrap_with_initial(|packet| initial.push(packet)) {
        Ok(client) => match run(client, &initial) {
            Ok(()) => 0,
            Err(err) => {
                // A bare exit code discards the message, so report through the
                // channel `blit ext run` and `ext attach` already render.
                eprintln!("muster: {err}");
                1
            }
        },
        Err(_) => EXIT_BOOTSTRAP_FAILURE,
    }
}

/// Everything the supervisor owns.
struct Muster {
    /// Absolute, `~` already expanded: the FS family does not expand it.
    dir: String,
    /// Derived from `dir` once, because `~` expansion happens per env file and
    /// per probe and the answer never changes.
    home: String,
    /// Watched directories. Root 0 is `dir`; pointer files in it name every
    /// external stack/include and every filtered Git worktree root.
    roots: Vec<Root>,
    /// Roots the server refused, kept so `doctor` can say so on every run
    /// rather than only in the journal at the moment it happened.
    unwatchable: BTreeMap<String, u8>,
    /// When to try the directories in `unwatchable` again, and how long to wait
    /// after that. A watch that was refused is the one failure the watch cannot
    /// report its way out of — nothing is watching a directory that is not
    /// being watched — so it is the only thing here that polls.
    rewatch_at_ms: u64,
    rewatch_delay_ms: u64,
    units: BTreeMap<String, Unit>,
    stacks: BTreeMap<String, StackFile>,
    instances: BTreeMap<String, Instance>,
    port_ledger: PortLedger,
    port_ledger_hash: u128,
    port_ledger_loaded: bool,
    journal: Journal,
    /// Everything `doctor` should say, rebuilt on every load.
    findings: Vec<ConfigError>,
    /// `log:` readiness cursors, keyed by unit.
    log_cursor: BTreeMap<String, (u64, u16)>,
    /// Listener id for the panel channel, zero when it could not be published.
    panel_listener: u32,
    panel_conns: Vec<panel::Conn>,
    /// Units whose row a panel has not been told about yet.
    dirty: BTreeSet<String>,
    /// When the oldest unflushed change arrived.
    dirty_since: Option<u64>,
    pending_events: Vec<Record>,
    /// Surface messages from the initial burst, held until the first load
    /// builds the owner map they are attributed through.
    initial_surfaces: Vec<Vec<u8>>,
    /// Surfaces this supervisor can account for, by surface id.
    surfaces: BTreeMap<u16, Surface>,
    /// Stamped `app_id` back to the unit that owns it, rebuilt on every load.
    /// A surface names its origin by `app_id`; this is the way back.
    surface_owners: BTreeMap<String, String>,
    /// Outstanding `C2S_TERM_WAIT`s, by nonce, so the reply finds its unit.
    log_waits: BTreeMap<u16, String>,
    /// When each activating unit is next probed.
    next_probe_ms: BTreeMap<String, u64>,
    terminals: TerminalSubscriptions,
    nonce: u16,
    net_stream: u16,
    /// Tags seen in the initial burst, pending adoption.
    adoptable: Vec<(u16, String)>,
    /// Exit statuses from the same burst, so an already-dead terminal is not
    /// adopted as the live run.
    exited: BTreeMap<u16, i32>,
    features: u32,
    /// Listener id for the CLI command channel, zero when not registered.
    cli_listener_id: u32,
    /// Accepted CLI command channels, keyed by channel id. Invocations are
    /// short, but accepts may overlap before their final CLOSED packets land.
    cli_conns: BTreeMap<u32, cli::CliConn>,
}

#[derive(Clone, Debug, PartialEq)]
struct Instance {
    stack: String,
    /// The port block this instance occupies, as `expand` resolved it.
    ports: Option<(i64, u32)>,
    members: Vec<String>,
}

/// One watched directory and the mirror of its contents.
struct Root {
    path: String,
    kind: RootKind,
    /// Correlates `S2C_FS_SYNCED`, which is the only thing that carries the
    /// sync id back.
    nonce: u16,
    sync_id: Option<u16>,
    /// Whether the initial snapshot has landed. `sync_id` only says the sync
    /// was accepted; the contents arrive afterwards, and treating acceptance as
    /// arrival makes an empty mirror look like an empty directory.
    snapshot_done: bool,
    mirror: remote::fs::FsMirror,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RootKind {
    /// Configuration, stack, or include directory: recursive JSON content.
    Files,
    /// A filtered `.git` tree containing only worktree `gitdir` pointers.
    GitWorktrees,
}

fn run(mut client: Client, initial: &[Vec<u8>]) -> Result<(), String> {
    let features = client.context().hello.features;
    require(features, remote::fs::FEATURE_FS, "FS")?;
    require(features, remote::env::FEATURE_ENV, "ENV")?;

    let adoptable = adoptable_tags(initial);
    let dir = resolve_dir(&mut client)?;
    // The configuration directory is derived from HOME, so it carries it.
    let home = match dir.find("/.config/") {
        Some(at) => dir[..at].to_string(),
        None => String::from("/"),
    };
    let mut muster = Muster {
        dir,
        home,
        roots: Vec::new(),
        unwatchable: BTreeMap::new(),
        rewatch_at_ms: 0,
        rewatch_delay_ms: REWATCH_MS,
        units: BTreeMap::new(),
        stacks: BTreeMap::new(),
        instances: BTreeMap::new(),
        port_ledger: PortLedger::default(),
        port_ledger_hash: 0,
        port_ledger_loaded: false,
        journal: Journal::new(1),
        findings: Vec::new(),
        log_cursor: BTreeMap::new(),
        log_waits: BTreeMap::new(),
        panel_listener: 0,
        panel_conns: Vec::new(),
        dirty: BTreeSet::new(),
        dirty_since: None,
        pending_events: Vec::new(),
        initial_surfaces: initial
            .iter()
            .filter(|packet| is_surface_message(packet))
            .cloned()
            .collect(),
        surfaces: BTreeMap::new(),
        surface_owners: BTreeMap::new(),
        next_probe_ms: BTreeMap::new(),
        terminals: TerminalSubscriptions::new(),
        nonce: 1,
        net_stream: 1,
        adoptable: adoptable.0,
        exited: adoptable.1,
        features,
        cli_listener_id: 0,
        cli_conns: BTreeMap::new(),
    };

    // A supervisor that silently ran something other than the file asked for
    // would be worse than one that refuses: neither exec block is probeable,
    // and an older server reads the environment as command text.
    if features & remote::FEATURE_CREATE_EXEC == 0 {
        ext_log(
            &mut client,
            "muster: server does not advertise FEATURE_CREATE_EXEC; \
             units with a command or env cannot be started",
        );
    }

    let dir = muster.dir.clone();
    muster.watch(&mut client, &dir, RootKind::Files);
    muster.open_panel(&mut client);

    let listener_name = format!(
        "blit.cli.{:016x}.{}",
        client.context().extension_id,
        client.context().attempt
    );
    ext_log(
        &mut client,
        &format!(
            "muster: starting dir={} listener={listener_name}",
            muster.dir
        ),
    );
    let listener = client
        .listen_channel(&listener_name, b"")
        .map_err(|e| format!("cli listener: {e:?}"))?;
    let cli_listener_id = listener.id();
    muster.cli_listener_id = cli_listener_id;
    if let Err(err) = register_descriptor(&mut client, cli_listener_id) {
        ext_log(
            &mut client,
            &format!("muster: command registration failed: {err}"),
        );
    }

    loop {
        let now = muster.now_ms(&client);
        let deadline = muster.next_deadline(&client, now);
        match client.wait_until(deadline) {
            Ok(WaitOutcome::Closed) | Err(_) => break,
            Ok(WaitOutcome::Deadline) => {}
            Ok(WaitOutcome::Packet) => {
                let Ok(Some(packet)) = client.recv() else {
                    break;
                };
                if muster.route_cli(&mut client, &packet) {
                    continue;
                }
                muster.route(&mut client, &packet);
            }
        }
        muster.reconcile(&mut client);
        let now = muster.now_ms(&client);
        muster.flush_panel(&mut client, now);
    }
    Ok(())
}

fn require(features: u32, bit: u32, name: &str) -> Result<(), String> {
    if features & bit == 0 {
        return Err(format!("server does not support {name}"));
    }
    Ok(())
}

/// `~` is not expanded by the FS family, so resolve the directory here — the
/// same way `blit_config_dir()` does.
fn resolve_dir(client: &mut Client) -> Result<String, String> {
    let nonce = 1;
    client
        .send(&remote::env::msg_env_get(nonce))
        .map_err(|e| format!("env: {e:?}"))?;
    let reply = client
        .recv_matching(|p| p.first() == Some(&remote::env::S2C_ENV))
        .map_err(|e| format!("env: {e:?}"))?
        .ok_or("connection closed reading the environment")?;
    let env = remote::env::parse_env(&reply).map_err(|e| format!("env: {e:?}"))?;
    let get = |key: &str| {
        env.entries
            .get(key.as_bytes())
            .and_then(|v| String::from_utf8(v.clone()).ok())
    };
    resolve_dir_from(get)
}

fn resolve_dir_from(get: impl Fn(&str) -> Option<String>) -> Result<String, String> {
    if let Some(explicit) = get("BLIT_MUSTER_DIR") {
        return Ok(explicit);
    }
    let name = get("BLIT_SERVER_NAME")
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| "default".to_owned());
    let valid = name.len() <= 64
        && !name.ends_with('.')
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'));
    if !valid {
        return Err("BLIT_SERVER_NAME is not a portable server name".to_owned());
    }
    if let Some(xdg) = get("XDG_CONFIG_HOME").filter(|s| !s.is_empty()) {
        return Ok(format!("{xdg}/blit/instances/{name}/muster"));
    }
    let home = get("HOME")
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "/root".into());
    Ok(format!("{home}/.config/blit/instances/{name}/muster"))
}

/// PTYs a previous supervisor left running, from the one `S2C_LIST` that
/// arrives before `READY`.
/// The burst is `HELLO, LIST, TITLE*, EXITED*, READY`, so the exit status of a
/// terminal that died while nobody was supervising it is in there too. Without
/// it an exited terminal is adopted as the live run, sits in `activating` until
/// `timeoutStart`, and is then replaced — which is exactly the restart storm
/// adoption exists to avoid.
fn adoptable_tags(initial: &[Vec<u8>]) -> (Vec<(u16, String)>, BTreeMap<u16, i32>) {
    let mut tags = Vec::new();
    let mut exited = BTreeMap::new();
    for packet in initial {
        match remote::parse_server_msg(packet) {
            Some(ServerMsg::List { entries }) => {
                tags = entries
                    .iter()
                    .filter(|e| e.tag.starts_with(supervisor::TAG_PREFIX))
                    .map(|e| (e.pty_id, e.tag.to_string()))
                    .collect();
            }
            Some(ServerMsg::Exited {
                pty_id,
                exit_status,
                ..
            }) => {
                exited.insert(pty_id, exit_status);
            }
            _ => {}
        }
    }
    (tags, exited)
}

impl Muster {
    fn now_ms(&self, client: &Client) -> u64 {
        (client.realtime_now().unix_timestamp_nanos() / 1_000_000) as u64
    }

    fn next_nonce(&mut self) -> u16 {
        self.nonce = self.nonce.wrapping_add(1).max(1);
        self.nonce
    }

    /// Start watching one directory. Recursive, because a stack is a
    /// subdirectory; the second level is dropped when the mirror is read.
    ///
    /// The configuration directory is root 0 and is never dropped. Every other
    /// root is named by a pointer file in it — an instance whose `stack` is a
    /// path, an include, or an explicit worktree source — so discovery always
    /// begins somewhere the user deliberately wrote, never in a checkout that
    /// merely happens to be there.
    fn watch(&mut self, client: &mut Client, path: &str, kind: RootKind) {
        if self.roots.iter().any(|root| root.path == path) {
            return;
        }
        let nonce = self.next_nonce();
        // Zero takes the server's own inline ceiling. A 64 KiB cap here only
        // ever meant "a unit file larger than this silently becomes invalid",
        // which is a rule nobody wants and nobody would guess.
        let packet = match kind {
            RootKind::Files => remote::fs::msg_fs_sync(
                nonce,
                remote::fs::FS_SYNC_RECURSIVE | remote::fs::FS_SYNC_CONTENT,
                SETTLE_MS,
                0,
                path,
            ),
            RootKind::GitWorktrees => remote::fs::msg_fs_sync_excluding(
                nonce,
                remote::fs::FS_SYNC_RECURSIVE | remote::fs::FS_SYNC_CONTENT,
                SETTLE_MS,
                0,
                path,
                GIT_WORKTREES_EXCLUDE,
            ),
        };
        if client.send(&packet).is_ok() {
            self.roots.push(Root {
                path: path.to_string(),
                kind,
                nonce,
                sync_id: None,
                snapshot_done: false,
                mirror: remote::fs::FsMirror::new(),
            });
        }
    }

    /// Ask again for the directories whose `FS_SYNC` was refused.
    ///
    /// `watch` returns early for a path already in `roots`, and a refused sync
    /// leaves the root there with no `sync_id` — so without dropping it first,
    /// a directory that did not exist when its pointer was written stays
    /// unwatched for the life of the supervisor. Nothing else can catch this:
    /// the watch is how muster hears about the world, and there is no watch on
    /// a directory that is not being watched.
    ///
    /// `now` forces the retry from `@muster reload`, which is the only thing
    /// bare `reload` is for.
    pub(crate) fn retry_unwatchable(&mut self, client: &mut Client, now: u64, immediate: bool) {
        let stuck: BTreeSet<String> = self.unwatchable.keys().cloned().collect();
        if stuck.is_empty() {
            return;
        }
        self.roots.retain(|root| !stuck.contains(&root.path));
        self.unwatchable.clear();
        self.rewatch_delay_ms = if immediate {
            REWATCH_MS
        } else {
            (self.rewatch_delay_ms * 2).min(REWATCH_MAX_MS)
        };
        self.rewatch_at_ms = now + self.rewatch_delay_ms;
        // `load` is what re-issues the syncs, because it is what decides which
        // directories are wanted in the first place.
        self.load(client);
    }

    /// Stop watching the roots nothing names any more.
    ///
    /// `wanted` includes the configuration directory, so there is no positional
    /// "root 0 is special" rule to remember. An earlier version exempted the
    /// first root by index, and the exemption then had to be re-remembered
    /// everywhere the same set was reused — `unwatchable` promptly forgot it and
    /// discarded the configuration directory's own status on every load.
    fn prune_roots(&mut self, client: &mut Client, wanted: &BTreeMap<String, RootKind>) {
        let mut kept = Vec::with_capacity(self.roots.len());
        for root in std::mem::take(&mut self.roots) {
            if wanted.get(&root.path) == Some(&root.kind) {
                kept.push(root);
                continue;
            }
            if let Some(sync_id) = root.sync_id {
                let _ = client.send(&remote::fs::msg_fs_stop(sync_id));
            }
        }
        self.roots = kept;
    }

    /// A `stack`/`include`/`worktrees` value as an absolute directory.
    ///
    /// A bare word is a subdirectory of the configuration directory; anything
    /// else is a path, with `~` expanded here because the FS family does not
    /// expand it.
    pub(crate) fn resolve_path(&self, value: &str) -> String {
        if config::is_path(value) {
            let expanded = expand_tilde(value, &self.home);
            // Relative paths would resolve against the *server's* cwd, which is
            // never what a pointer file meant.
            if config::is_absolute_path(&expanded) {
                expanded
            } else {
                format!("{}/{expanded}", self.dir)
            }
        } else {
            format!("{}/{value}", self.dir)
        }
    }

    /// One file from one watched root, by its path relative to that root.
    pub(crate) fn file_at(&self, root: &str, relative: &str) -> Option<Vec<u8>> {
        self.roots
            .iter()
            .find(|r| r.path == root)?
            .mirror
            .live
            .get(relative)?
            .content
            .clone()
    }

    /// The configuration directory's own files, as `load` reads them.
    ///
    /// Nothing below the second level: a stack is a subdirectory, and anything
    /// deeper is yours.
    pub(crate) fn config_files(&self) -> BTreeMap<String, Vec<u8>> {
        self.files_in(&self.dir.clone())
            .into_iter()
            .filter(|(path, _)| path.matches('/').count() <= 1)
            .collect()
    }

    /// Write a file into the configuration directory.
    ///
    /// `exclusive` is a create-or-fail: a zero CAS base means "there must be
    /// nothing here", which is what keeps `instantiate` from silently
    /// replacing an instance somebody is running.
    ///
    /// Only the configuration directory is writable, and only at its top
    /// level. A stack directory outside it is a repository this supervisor was
    /// pointed at — cloning one must not let it be edited, and the same rule
    /// that keeps discovery inside the configuration directory keeps writes
    /// there too.
    pub(crate) fn write_config(
        &mut self,
        client: &mut Client,
        relative: &str,
        content: &[u8],
        exclusive: bool,
    ) -> Result<(), String> {
        if relative.contains('/') || relative.starts_with('.') {
            return Err(format!("{relative:?} is not a top-level file"));
        }
        let dir = self.dir.clone();
        let Some(sync_id) = self
            .roots
            .iter()
            .find(|root| root.path == dir)
            .and_then(|root| root.sync_id)
        else {
            return Err(format!("{dir} is not being watched yet"));
        };
        // The CAS base is the hash of what is there now, so a replacement has
        // to name the thing it replaces.
        let base = if exclusive {
            0
        } else {
            self.roots
                .iter()
                .find(|root| root.path == dir)
                .and_then(|root| root.mirror.live.get(relative))
                .map_or(0, |node| node.hash)
        };
        let nonce = self.next_nonce();
        let write = remote::fs::FsWrite {
            nonce,
            sync_id,
            flags: remote::fs::FS_WRITE_MKPARENTS,
            base,
            mode: 0o600,
            content_kind: remote::fs::FS_WRITE_CONTENT_FULL,
            path: relative.to_string(),
            content: content.to_vec(),
        };
        client
            .send(&remote::fs::msg_fs_write(&write))
            .map_err(|e| format!("write: {e:?}"))?;
        let reply = self
            .reply(client, remote::fs::S2C_FS_DONE, nonce)
            .ok_or("the server never answered the write")?;
        let (_, status, hash, mtime_ns) =
            remote::fs::parse_fs_done(&reply).ok_or("malformed write reply")?;
        match status {
            remote::fs::FS_DONE_OK => {}
            remote::fs::FS_DONE_CONFLICT => return Err(format!("{relative} already exists")),
            other => {
                return Err(format!(
                    "{relative}: {}",
                    remote::fs::fs_done_status_text(other)
                ));
            }
        }

        // Put the bytes in the mirror ourselves.
        //
        // The write does come back as an `FS_UPDATE`, but a *metadata-only*
        // one: the server primes the echo by marking this client as already
        // holding the content, so its own upsert carries no copy. `files_in`
        // reads content, so without this the file muster just wrote is a node
        // it can see and not a file it can parse — and since a metadata-only
        // upsert preserves whatever content is already there, seeding here is
        // also what makes the echo harmless when it lands.
        if let Some(root) = self.roots.iter_mut().find(|root| root.path == dir) {
            root.mirror.live.insert(
                relative.to_string(),
                remote::fs::FsNode {
                    entry_flags: remote::fs::FS_ENTRY_FILE,
                    size: content.len() as u64,
                    mtime_ns,
                    mode: 0o600,
                    hash,
                    content: Some(content.to_vec()),
                },
            );
        }
        Ok(())
    }

    fn files_in(&self, path: &str) -> BTreeMap<String, Vec<u8>> {
        let Some(root) = self.roots.iter().find(|root| root.path == path) else {
            return BTreeMap::new();
        };
        root.mirror
            .live
            .iter()
            .filter(|(path, _)| !path.starts_with('.') && !path.contains("/."))
            .filter(|(path, _)| path.ends_with(".json"))
            .filter_map(|(path, node)| node.content.as_ref().map(|c| (path.clone(), c.clone())))
            .collect()
    }

    fn content_in(&self, path: &str) -> BTreeMap<String, Vec<u8>> {
        let Some(root) = self.roots.iter().find(|root| root.path == path) else {
            return BTreeMap::new();
        };
        root.mirror
            .live
            .iter()
            .filter_map(|(path, node)| node.content.as_ref().map(|c| (path.clone(), c.clone())))
            .collect()
    }

    fn worktrees_for(&self, source: &WorktreeSourceFile) -> Vec<worktrees::Worktree> {
        let main = self.resolve_path(&source.worktrees);
        let git = worktrees::stack_path(&main, ".git");
        worktrees::discover(&main, &self.content_in(&git))
    }

    fn root_ready(&self, path: &str) -> bool {
        self.roots
            .iter()
            .find(|root| root.path == path)
            .is_some_and(|root| root.snapshot_done)
    }

    fn next_deadline(&self, client: &Client, now: u64) -> MonotonicInstant {
        let mut soonest: Option<u64> = None;
        for unit in self.units.values() {
            if let Some(at) = unit.next_deadline_ms() {
                soonest = Some(soonest.map_or(at, |s: u64| s.min(at)));
            }
        }
        for at in self.next_probe_ms.values() {
            soonest = Some(soonest.map_or(*at, |s: u64| s.min(*at)));
        }
        // A pending flush is a deadline like any other: without it the loop
        // would sleep through the coalescing window and a panel would see the
        // change only when something else happened to wake it.
        if let Some(at) = self.flush_due_ms(now) {
            soonest = Some(soonest.map_or(at, |s: u64| s.min(at)));
        }
        if !self.unwatchable.is_empty() {
            let at = self.rewatch_at_ms;
            soonest = Some(soonest.map_or(at, |s: u64| s.min(at)));
        }
        let idle = client.monotonic_now() + IDLE_TICK;
        match soonest {
            Some(at) => client.monotonic_now() + Duration::from_millis(at.saturating_sub(now)),
            None => idle,
        }
    }

    fn route(&mut self, client: &mut Client, packet: &[u8]) {
        let now = self.now_ms(client);
        match packet.first().copied() {
            // [0x40][nonce:2][sync_id:2][status:1] — the sync id is *after*
            // the nonce, and reading the nonce as the id silently rejects
            // every update that follows.
            Some(remote::fs::S2C_FS_SYNCED) => {
                if packet.len() >= 6 {
                    let nonce = u16::from_le_bytes([packet[1], packet[2]]);
                    let status = packet[5];
                    let Some(root) = self.roots.iter_mut().find(|root| root.nonce == nonce) else {
                        return;
                    };
                    if status == remote::fs::FS_STATUS_OK {
                        root.sync_id = Some(u16::from_le_bytes([packet[3], packet[4]]));
                        // Something worked, so whatever is still broken is
                        // broken for its own reason and deserves a fresh
                        // schedule rather than the backoff this one climbed.
                        self.rewatch_delay_ms = REWATCH_MS;
                    } else {
                        // A directory that cannot be watched is the difference
                        // between "no units" and "no configuration", and
                        // silence does not distinguish them.
                        let path = root.path.clone();
                        self.unwatchable.insert(path.clone(), status);
                        self.findings.push(ConfigError::new(
                            path,
                            format!("cannot watch this directory (status {status})"),
                        ));
                    }
                }
            }
            Some(remote::fs::S2C_FS_UPDATE) => {
                if packet.len() < 8 {
                    return;
                }
                let sync_id = u16::from_le_bytes([packet[1], packet[2]]);
                let Some(root) = self
                    .roots
                    .iter_mut()
                    .find(|root| root.sync_id == Some(sync_id))
                else {
                    return;
                };
                // `FS_UPDATE_SYNC` closes the staged initial snapshot: before
                // it, this root's mirror is a work in progress and its units do
                // not exist yet.
                let snapshot_done = packet[7] & remote::fs::FS_UPDATE_SYNC != 0;
                if let Some(update_id) = root.mirror.apply_update(packet) {
                    root.snapshot_done |= snapshot_done;
                    let _ = client.send(&remote::fs::msg_fs_ack(sync_id, update_id));
                    self.load(client);
                }
            }
            _ if self.route_panel(client, packet, now) => {}
            _ if self.note_log_wait(client, packet) => {}
            _ => match remote::parse_server_msg(packet) {
                Some(ServerMsg::Exited {
                    pty_id,
                    exit_status,
                    ..
                }) => self.note_exit(client, pty_id, exit_status),
                Some(other) => self.note_surface(other, now),
                None => {}
            },
        }
    }

    /// Route packets for the manually-tracked CLI command channel.
    ///
    /// `CommandProvider::offer` blocks waiting for INVOKE, which can stall the
    /// whole supervisor or miss the DATA packet entirely. After the descriptor
    /// is registered we own the listener id and accepted channel state directly,
    /// the same way the panel does.
    fn route_cli(&mut self, client: &mut Client, packet: &[u8]) -> bool {
        let message = match remote::channel::parse_channel_message(packet) {
            Ok(Some(message)) => message,
            _ => return false,
        };
        match message {
            remote::channel::ChannelMessage::Accepted {
                channel_id,
                listener_id,
                window,
                ..
            } => {
                if listener_id != self.cli_listener_id || self.cli_listener_id == 0 {
                    return false;
                }
                self.cli_conns.entry(channel_id).or_insert(cli::CliConn {
                    channel_id,
                    window,
                    sent: 0,
                    acked: 0,
                    received: 0,
                });
                true
            }
            remote::channel::ChannelMessage::Data {
                channel_id,
                payload,
            } => {
                if self.cli_conns.contains_key(&channel_id) {
                    self.dispatch_cli_data(client, channel_id, payload);
                    return true;
                }
                // An unknown channel may belong to the panel listener. The
                // server queues ACCEPTED before DATA on each endpoint, so a
                // CLI channel is always in `cli_conns` by this point.
                false
            }
            remote::channel::ChannelMessage::Ack { channel_id, bytes } => {
                let Some(conn) = self.cli_conns.get_mut(&channel_id) else {
                    return false;
                };
                conn.acked = conn.acked.max(bytes);
                true
            }
            remote::channel::ChannelMessage::Closed { channel_id, .. } => {
                if self.cli_conns.remove(&channel_id).is_some() {
                    return true;
                }
                if channel_id == self.cli_listener_id {
                    self.cli_listener_id = 0;
                    self.cli_conns.clear();
                    return true;
                }
                false
            }
            _ => false,
        }
    }

    fn dispatch_cli_data(&mut self, client: &mut Client, channel_id: u32, payload: &[u8]) {
        let Some(mut conn) = self.cli_conns.remove(&channel_id) else {
            return;
        };
        conn.received = conn.received.saturating_add(payload.len() as u64);
        let received = conn.received;
        let _ = client.send(&remote::channel::msg_channel_ack(channel_id, received));
        let mut tx = cli::CliTx::new(&mut conn);
        if let Some(invocation) = cli::decode_invoke(payload) {
            if let Err(err) = self.serve(client, &invocation, &mut tx) {
                ext_log(client, &format!("muster: command failed to respond: {err}"));
                let _ = tx.close_channel(client, remote::channel::CHANNEL_CLOSE_CANCELLED);
            }
        } else {
            let _ = tx.close_channel(client, remote::channel::CHANNEL_CLOSE_CANCELLED);
        }
        // Keep the connection record until the server's CHANNEL_CLOSED tells us
        // the channel is fully torn down. Without this the listener can stay
        // half-closed and the server will not offer the next invocation.
        self.cli_conns.insert(channel_id, conn);
    }

    // ---------------------------------------------------------------- loading

    /// Rebuild the unit table from the mirror.
    ///
    /// A file that does not parse never displaces the one that did: the running
    /// unit keeps running, the failure is journaled, and `doctor` lists it.
    fn load(&mut self, client: &mut Client) {
        let now = self.now_ms(client);
        self.findings.clear();
        for (path, status) in &self.unwatchable {
            self.findings.push(ConfigError::new(
                path.clone(),
                format!("cannot watch this directory (status {status})"),
            ));
        }

        let dir = self.dir.clone();
        let files = self.files_in(&dir);
        // Nothing below the second level of the configuration directory is
        // read: a stack is a subdirectory, and anything deeper is yours.
        let files: BTreeMap<String, Vec<u8>> = files
            .into_iter()
            .filter(|(path, _)| path.matches('/').count() <= 1)
            .collect();

        // Pass one: sort the top level into units, instances, includes, and
        // worktree sources, and learn which directories are named. The
        // configuration directory is in the set from the start — it is a
        // watched root like any other, and making it one removes every "except
        // root 0" caveat downstream.
        let mut pointers: Vec<(String, TopLevel)> = Vec::new();
        let mut wanted_roots: BTreeMap<String, RootKind> =
            BTreeMap::from([(dir.clone(), RootKind::Files)]);
        for (path, bytes) in &files {
            if path.contains('/') {
                continue;
            }
            let name = path.trim_end_matches(".json").to_string();
            match config::parse_top_level(path, bytes) {
                Ok(top) => {
                    match &top {
                        TopLevel::Instance(instance) if config::is_path(&instance.stack) => {
                            wanted_roots
                                .insert(self.resolve_path(&instance.stack), RootKind::Files);
                        }
                        TopLevel::WorktreeSource(source) => {
                            let main = self.resolve_path(&source.worktrees);
                            wanted_roots.insert(
                                worktrees::stack_path(&main, ".git"),
                                RootKind::GitWorktrees,
                            );
                            for worktree in self.worktrees_for(source) {
                                wanted_roots.insert(
                                    worktrees::stack_path(&worktree.path, &source.stack),
                                    RootKind::Files,
                                );
                            }
                        }
                        TopLevel::Include(include) => {
                            wanted_roots
                                .insert(self.resolve_path(&include.include), RootKind::Files);
                        }
                        _ => {}
                    }
                    pointers.push((name, top));
                }
                Err(err) => {
                    // A file that does not parse never displaces the one that
                    // did, but the failure is a decision worth recording next
                    // to the ones it prevented.
                    self.record(
                        Record::new(name, Event::Invalid, "stopped")
                            .cause(Cause::File)
                            .detail(err.detail.clone()),
                        now,
                    );
                    self.findings.push(err);
                }
            }
        }

        // Adjust the watch set before reading anything from it. A root added
        // here is empty until its own updates arrive, which triggers another
        // load — so a new pointer costs one extra pass, not a missing stack.
        self.prune_roots(client, &wanted_roots);
        self.unwatchable
            .retain(|path, _| wanted_roots.contains_key(path));
        for (path, kind) in &wanted_roots {
            self.watch(client, path, *kind);
        }

        // Stacks declared inside the configuration directory. External ones are
        // resolved per instance, since their declarations live beside them.
        self.stacks.clear();
        for (path, bytes) in &files {
            let Some((sub, base)) = path.rsplit_once('/') else {
                continue;
            };
            if base == "stack.json" {
                match config::parse_json(path, bytes).and_then(|v| {
                    serde_json::from_value(v).map_err(|e| ConfigError::new(path, e.to_string()))
                }) {
                    Ok(stack) => {
                        self.stacks.insert(sub.to_string(), stack);
                    }
                    Err(err) => self.findings.push(err),
                }
            } else {
                // A stack directory with templates but no stack.json still
                // works: it simply declares no parameters.
                self.stacks.entry(sub.to_string()).or_default();
            }
        }

        let mut wanted: BTreeMap<String, Unit> = BTreeMap::new();
        let mut instances: BTreeMap<String, Instance> = BTreeMap::new();
        // Which pointer contributed each name, so a collision can name both.
        let mut provenance: BTreeMap<String, String> = BTreeMap::new();
        let mut worktree_sources: Vec<(String, String, WorktreeSourceFile)> = Vec::new();

        for (name, top) in pointers {
            let file = format!("{name}.json");
            match top {
                TopLevel::Unit(unit) => {
                    provenance.insert(name.clone(), file);
                    wanted.insert(name.clone(), Unit::new(name, None, *unit));
                }
                TopLevel::Instance(instance) => match self.expand(&name, &instance, &files) {
                    Ok(expansion) => {
                        for unit in expansion.units {
                            provenance.insert(unit.name.clone(), file.clone());
                            wanted.insert(unit.name.clone(), unit);
                        }
                        instances.insert(
                            name.clone(),
                            Instance {
                                stack: instance.stack.clone(),
                                ports: expansion.ports,
                                members: expansion.members,
                            },
                        );
                    }
                    Err(err) => self.findings.push(ConfigError::new(file, err)),
                },
                TopLevel::WorktreeSource(source) => {
                    worktree_sources.push((name, file, *source));
                }
                TopLevel::Include(include) => {
                    let root = self.resolve_path(&include.include);
                    for (template, bytes) in self.files_in(&root) {
                        // An include contributes ordinary units only. Its
                        // subdirectories are not stacks — an instance names a
                        // stack by path, which is a different pointer.
                        if template.contains('/') {
                            continue;
                        }
                        let unit_name = template.trim_end_matches(".json").to_string();
                        if include.omit.contains(&unit_name) {
                            continue;
                        }
                        let where_ = format!("{root}/{template}");
                        match config::parse_top_level(&where_, &bytes) {
                            Ok(TopLevel::Unit(mut unit)) => {
                                // An include adds no suffix, so two of them
                                // offering one name is ambiguous rather than
                                // mergeable. First writer wins, and both are
                                // named, so the fix is obvious.
                                if let Some(first) = provenance.get(&unit_name) {
                                    self.findings.push(ConfigError::new(
                                        file.clone(),
                                        format!(
                                            "{unit_name:?} is already provided by {first}; \
                                             omit it in one of them"
                                        ),
                                    ));
                                    continue;
                                }
                                if !include.autostart {
                                    unit.autostart = false;
                                }
                                // A relative path in an included unit means the
                                // directory it came from, exactly as it does in
                                // a stack template. Without this an included
                                // `"envFile": ".env"` silently resolves against
                                // the unit's cwd instead.
                                rebase_unit_paths(&mut unit, &root);
                                provenance.insert(unit_name.clone(), file.clone());
                                wanted.insert(unit_name.clone(), Unit::new(unit_name, None, *unit));
                            }
                            Ok(_) => self.findings.push(ConfigError::new(
                                where_,
                                "an included directory holds units, not stacks, instances, or worktree sources",
                            )),
                            Err(err) => self.findings.push(err),
                        }
                    }
                }
            }
        }

        for (name, file, source) in worktree_sources {
            self.expand_worktree_source(
                client,
                &name,
                &file,
                &source,
                &files,
                &mut wanted,
                &mut instances,
                &mut provenance,
            );
        }

        // Rebuilt every load so an adopted unit re-claims the surfaces its
        // previous run stamped: the id is derived from the name, and the
        // initial burst replays every live surface's origin.
        self.surface_owners = wanted
            .keys()
            .map(|name| (supervisor::app_id_for(name), name.clone()))
            .collect();

        // The burst arrived before the owner map existed, so attribute it now.
        for packet in std::mem::take(&mut self.initial_surfaces) {
            if let Some(msg) = remote::parse_server_msg(&packet) {
                self.note_surface(msg, now);
            }
        }

        self.check_ports(&instances);
        self.reconcile_table(client, wanted, instances, now);

        // Adoption has to wait for the first load: matching a tag needs the
        // unit it names. It cannot hang off `S2C_READY` either â bootstrap
        // consumes that, so the loop never sees it.
        if !self.adoptable.is_empty() {
            self.adopt(client);
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn expand_worktree_source(
        &mut self,
        client: &mut Client,
        source_name: &str,
        source_file: &str,
        source: &WorktreeSourceFile,
        files: &BTreeMap<String, Vec<u8>>,
        wanted: &mut BTreeMap<String, Unit>,
        instances: &mut BTreeMap<String, Instance>,
        provenance: &mut BTreeMap<String, String>,
    ) {
        let worktree_set: Vec<_> = self
            .worktrees_for(source)
            .into_iter()
            .filter(|worktree| {
                self.root_ready(&worktrees::stack_path(&worktree.path, &source.stack))
            })
            .collect();
        let Some(main) = worktree_set.iter().find(|worktree| worktree.is_main) else {
            // The main stack watch has been requested but its initial snapshot
            // has not landed yet. That update calls `load` again; reporting a
            // transient empty mirror as a broken source would be noise.
            return;
        };
        let main_stack = worktrees::stack_path(&main.path, &source.stack);
        let declarations = match self.declarations_of(&main_stack) {
            Ok(declarations) => declarations,
            Err(err) => {
                self.findings.push(ConfigError::new(source_file, err));
                return;
            }
        };

        let mut port_assignment: Option<(String, BTreeMap<String, i64>)> = None;
        if let Some((port_name, declaration)) = declarations
            .vars
            .iter()
            .find(|(_, declaration)| declaration.is_ports())
        {
            if source
                .vars
                .get(port_name)
                .and_then(serde_json::Value::as_str)
                != Some("auto")
            {
                self.findings.push(ConfigError::new(
                    source_file,
                    format!("worktree source must bind port parameter {port_name:?} to \"auto\""),
                ));
                return;
            }
            let Some(start) = declaration.start else {
                self.findings.push(ConfigError::new(
                    source_file,
                    format!("port parameter {port_name:?} needs start for a worktree source"),
                ));
                return;
            };
            if self.features & remote::kv::FEATURE_KV == 0 {
                self.findings.push(ConfigError::new(
                    source_file,
                    "worktree port allocation needs server KV support",
                ));
                return;
            }
            let explicit: Vec<(i64, u32)> = instances
                .values()
                .filter_map(|instance| instance.ports)
                .collect();
            let assigned = match self.assign_worktree_ports(
                client,
                source_name,
                &worktree_set,
                start,
                declaration.span,
                &explicit,
            ) {
                Ok(assigned) => assigned,
                Err(err) => {
                    self.findings.push(ConfigError::new(source_file, err));
                    return;
                }
            };
            port_assignment = Some((port_name.clone(), assigned));
        }

        for worktree in &worktree_set {
            let instance_name = worktrees::instance_name(source_name, worktree);
            if let Some(first) = provenance.get(&instance_name) {
                self.findings.push(ConfigError::new(
                    source_file,
                    format!("instance {instance_name:?} collides with a unit from {first}"),
                ));
                continue;
            }
            if let Some(first) = instances.get(&instance_name) {
                self.findings.push(ConfigError::new(
                    source_file,
                    format!(
                        "instance {instance_name:?} is already provided by stack {:?}",
                        first.stack
                    ),
                ));
                continue;
            }
            let mut vars = source.vars.clone();
            if let Some((port_name, assigned)) = &port_assignment
                && let Some(base) = assigned.get(&worktree.id)
            {
                vars.insert(port_name.clone(), serde_json::json!(*base));
            }
            let instance = InstanceFile {
                stack: worktrees::stack_path(&worktree.path, &source.stack),
                vars,
                omit: source.omit.clone(),
                autostart: source.autostart,
            };
            let expansion = match self.expand(&instance_name, &instance, files) {
                Ok(expansion) => expansion,
                Err(err) => {
                    self.findings.push(ConfigError::new(source_file, err));
                    continue;
                }
            };
            if let Some((unit, first)) = expansion
                .units
                .iter()
                .find_map(|unit| provenance.get(&unit.name).map(|first| (&unit.name, first)))
            {
                self.findings.push(ConfigError::new(
                    source_file,
                    format!("{unit:?} is already provided by {first}"),
                ));
                continue;
            }
            for unit in expansion.units {
                provenance.insert(unit.name.clone(), source_file.to_string());
                wanted.insert(unit.name.clone(), unit);
            }
            instances.insert(
                instance_name,
                Instance {
                    stack: instance.stack,
                    ports: expansion.ports,
                    members: expansion.members,
                },
            );
        }
    }

    fn assign_worktree_ports(
        &mut self,
        client: &mut Client,
        source_name: &str,
        worktrees: &[worktrees::Worktree],
        start: i64,
        span: u32,
        explicit: &[(i64, u32)],
    ) -> Result<BTreeMap<String, i64>, String> {
        // An extension update briefly overlaps the retiring and replacement
        // attempts. Both can read the same ledger revision, so the loser must
        // merge from the winner instead of leaving this source empty until an
        // unrelated filesystem event happens to reload it.
        const CAS_ATTEMPTS: usize = 3;
        for attempt in 0..CAS_ATTEMPTS {
            if !self.port_ledger_loaded {
                self.load_port_ledger(client)?;
            }
            let before = self.port_ledger.clone();
            let assigned = worktrees::assign_ports(
                source_name,
                worktrees,
                start,
                span,
                &mut self.port_ledger,
                explicit,
            )?;
            if self.port_ledger == before {
                return Ok(assigned);
            }
            match self.persist_port_ledger(client) {
                Ok(()) => return Ok(assigned),
                Err(_) if !self.port_ledger_loaded && attempt + 1 < CAS_ATTEMPTS => {
                    self.port_ledger = before;
                }
                Err(err) => {
                    self.port_ledger = before;
                    return Err(err);
                }
            }
        }
        unreachable!("the bounded port-ledger retry always returns")
    }

    fn load_port_ledger(&mut self, client: &mut Client) -> Result<(), String> {
        let nonce = self.next_nonce();
        client
            .send(&remote::kv::msg_kv_fetch(nonce, PORT_LEDGER_KEY))
            .map_err(|err| format!("read worktree port ledger: {err:?}"))?;
        let reply = self
            .reply(client, remote::kv::S2C_KV_VALUE, nonce)
            .ok_or("server never answered the worktree port ledger read")?;
        let Some((_, status, hash, value)) = remote::kv::parse_kv_value(&reply) else {
            return Err("malformed worktree port ledger reply".into());
        };
        match status {
            remote::kv::KV_STATUS_NOT_FOUND => {
                self.port_ledger = PortLedger::default();
                self.port_ledger_hash = 0;
            }
            remote::kv::KV_STATUS_OK => {
                self.port_ledger = serde_json::from_slice(&value)
                    .map_err(|err| format!("invalid worktree port ledger: {err}"))?;
                self.port_ledger_hash = hash;
            }
            other => {
                return Err(format!(
                    "cannot read worktree port ledger: {}",
                    remote::kv::kv_status_text(other)
                ));
            }
        }
        self.port_ledger_loaded = true;
        Ok(())
    }

    fn persist_port_ledger(&mut self, client: &mut Client) -> Result<(), String> {
        let value = serde_json::to_vec(&self.port_ledger)
            .map_err(|err| format!("serialize worktree port ledger: {err}"))?;
        let nonce = self.next_nonce();
        let put = remote::kv::KvPut {
            nonce,
            flags: remote::kv::KV_PUT_DURABLE,
            base: self.port_ledger_hash,
            key: PORT_LEDGER_KEY.into(),
            value,
        };
        client
            .send(&remote::kv::msg_kv_put(&put))
            .map_err(|err| format!("write worktree port ledger: {err:?}"))?;
        let reply = self
            .reply(client, remote::kv::S2C_KV_DONE, nonce)
            .ok_or("server never answered the worktree port ledger write")?;
        let Some((_, status, hash, _)) = remote::kv::parse_kv_done(&reply) else {
            return Err("malformed worktree port ledger write reply".into());
        };
        if status != remote::kv::KV_STATUS_OK {
            if status == remote::kv::KV_STATUS_CONFLICT {
                self.port_ledger_loaded = false;
            }
            return Err(format!(
                "cannot write worktree port ledger: {}",
                remote::kv::kv_status_text(status)
            ));
        }
        self.port_ledger_hash = hash;
        Ok(())
    }

    /// What a stack declares, without expanding anything.
    ///
    /// Split out of [`Self::expand`] because `instantiate` has to know which
    /// parameter is the port block before it has an instance to expand.
    pub(crate) fn declarations_of(&self, stack: &str) -> Result<StackFile, String> {
        if !config::is_path(stack) {
            return self
                .stacks
                .get(stack)
                .cloned()
                .ok_or_else(|| format!("no stack named {stack:?}"));
        }
        let dir = self.resolve_path(stack);
        if !self.roots.iter().any(|root| root.path == dir) {
            return Err(format!("{dir} is not being watched yet"));
        }
        match self.files_in(&dir).get("stack.json") {
            // A stack directory with templates but no `stack.json` works: it
            // simply declares no parameters.
            None => Ok(StackFile::default()),
            Some(bytes) => config::parse_json("stack.json", bytes)
                .and_then(|value| {
                    serde_json::from_value(value)
                        .map_err(|e| ConfigError::new("stack.json", e.to_string()))
                })
                .map_err(|e| e.detail),
        }
    }

    /// Turn one instance into its units.
    fn expand(
        &self,
        instance_name: &str,
        instance: &InstanceFile,
        files: &BTreeMap<String, Vec<u8>>,
    ) -> Result<Expansion, String> {
        let stack_dir = self.resolve_path(&instance.stack);
        // A stack in the configuration directory declares itself in a
        // subdirectory; one outside declares itself beside its templates. Both
        // reduce to a directory and a `stack.json` inside it.
        let (declarations, templates) = if config::is_path(&instance.stack) {
            let outside = self.files_in(&stack_dir);
            if outside.is_empty() && !self.roots.iter().any(|r| r.path == stack_dir) {
                return Err(format!("{stack_dir} is not being watched yet"));
            }
            let declared = outside
                .get("stack.json")
                .map(|bytes| {
                    config::parse_json("stack.json", bytes).and_then(|v| {
                        serde_json::from_value(v)
                            .map_err(|e| ConfigError::new("stack.json", e.to_string()))
                    })
                })
                .transpose()
                .map_err(|e| e.detail)?
                .unwrap_or_default();
            let templates: BTreeMap<String, Vec<u8>> = outside
                .into_iter()
                .filter(|(name, _)| !name.contains('/') && name != "stack.json")
                .collect();
            (declared, templates)
        } else {
            let declared = self
                .stacks
                .get(&instance.stack)
                .ok_or_else(|| format!("no stack named {:?}", instance.stack))?
                .clone();
            let prefix = format!("{}/", instance.stack);
            let templates: BTreeMap<String, Vec<u8>> = files
                .iter()
                .filter_map(|(path, bytes)| {
                    path.strip_prefix(&prefix)
                        .filter(|base| *base != "stack.json" && !base.contains('/'))
                        .map(|base| (base.to_string(), bytes.clone()))
                })
                .collect();
            (declared, templates)
        };

        let vars = config::bind_vars(
            instance_name,
            &instance.stack,
            &stack_dir,
            &declarations,
            &instance.vars,
        )?;

        let mut members = Vec::new();
        let mut units = Vec::new();
        for (base, bytes) in &templates {
            let template = base.trim_end_matches(".json");
            if instance.omit.iter().any(|o| o == template) {
                continue;
            }
            let path = format!("{stack_dir}/{base}");
            let mut value = config::parse_json(&path, bytes).map_err(|e| e.detail)?;
            config::substitute(&mut value, &vars)?;
            let mut file: UnitFile =
                serde_json::from_value(value).map_err(|e| format!("{path}: {e}"))?;
            rebase_unit_paths(&mut file, &stack_dir);
            file.validate(&path).map_err(|e| e.detail)?;
            let name = supervisor::qualified(instance_name, template);
            members.push(name.clone());

            // Inside a stack, dependencies name templates and always resolve
            // within the same instance.
            let qualify = |names: &mut Vec<String>| {
                for n in names.iter_mut() {
                    *n = supervisor::qualified(instance_name, n);
                }
            };
            qualify(&mut file.requires);
            qualify(&mut file.wants);
            qualify(&mut file.after);

            let mut unit = Unit::new(name, Some(instance_name.to_string()), file);
            if !instance.autostart {
                unit.file.autostart = false;
            }
            units.push(unit);
        }
        // A dependency on an omitted template is a mistake worth naming.
        for unit in &units {
            for dep in unit.file.requires.iter() {
                if !members.contains(dep) {
                    return Err(format!(
                        "{} requires {dep}, which this instance omits or does not have",
                        unit.name
                    ));
                }
            }
        }
        Ok(Expansion {
            members,
            units,
            ports: config::port_span(&declarations, &vars),
        })
    }

    /// Two instances whose port blocks overlap is the failure mode of running
    /// several dev stacks, and it presents as EADDRINUSE in whichever lost.
    ///
    /// Takes the freshly parsed map: reading `self.instances` here would
    /// inspect the *previous* generation, which is empty on the first load and
    /// stale on every one after it.
    fn check_ports(&mut self, instances: &BTreeMap<String, Instance>) {
        // The span is whatever `expand` resolved. Re-deriving it here from
        // `self.stacks` looked equivalent and was not: that map is keyed by
        // subdirectory name, so an instance naming a stack by path never
        // matched, and overlap detection was blind to exactly the case port
        // blocks exist for — one stack running once per worktree.
        let blocks: Vec<(String, i64, u32)> = instances
            .iter()
            .filter_map(|(name, instance)| {
                instance
                    .ports
                    .map(|(base, span)| (name.clone(), base, span))
            })
            .collect();
        for (i, (a, a_base, a_span)) in blocks.iter().enumerate() {
            for (b, b_base, b_span) in blocks.iter().skip(i + 1) {
                let overlap =
                    *a_base < b_base + i64::from(*b_span) && *b_base < a_base + i64::from(*a_span);
                if overlap {
                    self.findings.push(ConfigError::new(
                        format!("{a}.json"),
                        format!("port block {a_base}+{a_span} overlaps {b}'s {b_base}+{b_span}"),
                    ));
                }
            }
        }
    }

    /// Fold a freshly parsed table into the live one, keeping running units.
    fn reconcile_table(
        &mut self,
        client: &mut Client,
        wanted: BTreeMap<String, Unit>,
        instances: BTreeMap<String, Instance>,
        now: u64,
    ) {
        let mut restart: Vec<String> = Vec::new();
        for (name, fresh) in &wanted {
            match self.units.get_mut(name) {
                None => {
                    let unit = fresh.clone();
                    let instance = unit.instance.clone();
                    self.units.insert(name.clone(), unit);
                    self.record(
                        Record::new(name.clone(), Event::Loaded, "stopped").instance(instance),
                        now,
                    );
                }
                Some(existing) => {
                    if !same_spec(&existing.file, &fresh.file) {
                        existing.file = fresh.file.clone();
                        existing.stale = true;
                        let instance = existing.instance.clone();
                        let phase = existing.phase;
                        let restart_after_change = existing.restarts_after_change();
                        self.record(
                            Record::new(name.clone(), Event::Changed, phase.as_str())
                                .instance(instance),
                            now,
                        );
                        if restart_after_change {
                            restart.push(name.clone());
                        }
                    } else {
                        // Everything but the spec — descriptions, policies —
                        // applies without disturbing a running unit.
                        existing.file = fresh.file.clone();
                    }
                }
            }
        }

        let gone: Vec<String> = self
            .units
            .keys()
            .filter(|name| !wanted.contains_key(*name))
            .cloned()
            .collect();
        for name in gone {
            self.close_all(client, &name);
            if let Some(unit) = self.units.remove(&name) {
                self.record(
                    Record::new(name, Event::Unloaded, "stopped").instance(unit.instance),
                    now,
                );
            }
        }

        // A partial frame carries units, never the tree they hang under, so an
        // instance appearing or losing a member has to be a whole frame.
        if self.instances != instances {
            self.touch_all(now);
        }
        self.instances = instances;
        for name in restart {
            self.restart(client, &name, Cause::File);
        }
    }

    // -------------------------------------------------------------- lifecycle

    /// Whether every directory a pointer named has answered, one way or the
    /// other.
    ///
    /// Until then the unit table is incomplete by construction: a root added
    /// during a load is empty until its own updates arrive.
    fn roots_settled(&self) -> bool {
        self.roots
            .iter()
            .all(|root| root.snapshot_done || self.unwatchable.contains_key(&root.path))
    }

    /// Adopt the terminals a previous supervisor left running.
    ///
    /// Runs on every load, not just the first, because a unit whose definition
    /// lives outside the configuration directory does not exist yet on the load
    /// that discovers its pointer. A tag naming a unit that is not in the table
    /// *yet* stays pending; it is only closed once every root has reported, at
    /// which point "not in the table" really does mean gone. Closing eagerly
    /// killed and respawned exactly the units an external stack or an include
    /// contributed — the restart storm adoption exists to prevent, arriving by
    /// a different door.
    fn adopt(&mut self, client: &mut Client) {
        let now = self.now_ms(client);
        let tags = std::mem::take(&mut self.adoptable);
        if tags.is_empty() {
            return;
        }
        let settled = self.roots_settled();
        // Per unit, sort by sequence: the highest is the live run.
        let mut by_unit: BTreeMap<String, Vec<(u64, u16)>> = BTreeMap::new();
        for (pty, tag) in tags {
            let Some((name, seq)) = supervisor::parse_tag(&tag) else {
                continue;
            };
            by_unit
                .entry(name.to_string())
                .or_default()
                .push((seq, pty));
        }
        for (name, mut runs) in by_unit {
            runs.sort_unstable();
            if !self.units.contains_key(&name) {
                if !settled {
                    // Its definition may still be arriving from a root that has
                    // not reported. Keep the tags and try again next load.
                    self.adoptable.extend(
                        runs.into_iter()
                            .map(|(seq, pty)| (pty, supervisor::tag_for(&name, seq))),
                    );
                    continue;
                }
                // Every root has reported, so this really is gone. It takes its
                // history with it.
                for (_, pty) in runs {
                    let _ = client.send(&remote::msg_close(pty));
                }
                continue;
            }
            let unit = self.units.get_mut(&name).expect("checked");
            let highest = runs.last().expect("non-empty").0;
            unit.seq = highest + 1;

            // The highest sequence that has *not* exited is the live run.
            // Everything else is history, newest first.
            let live = runs
                .iter()
                .rev()
                .find(|(_, pty)| !self.exited.contains_key(pty))
                .copied();
            for (seq, pty) in runs.iter().rev() {
                if live.is_some_and(|(_, live_pty)| live_pty == *pty) {
                    continue;
                }
                unit.runs.push(Run {
                    pty: *pty,
                    seq: *seq,
                    exit_code: self.exited.get(pty).copied().unwrap_or(0),
                    started_ms: 0,
                    ended_ms: now,
                });
            }

            let phase = match live {
                Some((_, pty)) => {
                    unit.pty = Some(pty);
                    unit.started_ms = now;
                    unit.failures = 0;
                    // Only a probe that describes *current* state can be
                    // re-run against an adopted terminal. `path`, `tcp` and
                    // `http` ask the world a question and get today's answer.
                    // `log`, `delay` and `spawn` describe something that
                    // already happened, and the evidence may have scrolled out
                    // of the ring — re-running one stalls a healthy unit until
                    // `timeoutStart` and then replaces it, which is the restart
                    // storm adoption exists to prevent. A live terminal is the
                    // evidence for those.
                    if unit.file.ready_when.is_stateless() {
                        unit.deadline_ms = now + unit.file.timeout_start.ms();
                        Phase::Activating
                    } else {
                        unit.deadline_ms = 0;
                        Phase::Running
                    }
                }
                None => {
                    // Every terminal this unit left behind is dead. A oneshot
                    // that succeeded is still ready; anything else is stopped,
                    // and the next start takes a fresh sequence.
                    let succeeded = unit.runs.first().is_some_and(|r| r.exit_code == 0);
                    unit.last_exit = unit.runs.first().map(|r| r.exit_code);
                    if unit.file.unit_type == UnitType::Oneshot && succeeded {
                        Phase::Exited
                    } else {
                        Phase::Stopped
                    }
                }
            };
            unit.phase = phase;

            let stale = unit.reap();
            let instance = unit.instance.clone();
            for run in stale {
                let _ = client.send(&remote::msg_close(run.pty));
            }
            let mut record = Record::new(name, Event::Adopted, phase.as_str())
                .cause(Cause::Adopt)
                .instance(instance);
            if let Some((_, pty)) = live {
                record = record.pty(pty);
            }
            self.record(record, now);
        }
    }

    /// Start whatever is due, probe whatever is activating, kill whatever
    /// outstayed its stop grace.
    fn reconcile(&mut self, client: &mut Client) {
        let now = self.now_ms(client);

        if !self.unwatchable.is_empty() && now >= self.rewatch_at_ms {
            self.retry_unwatchable(client, now, false);
        }

        // Autostart: a unit that has never run and says so.
        let names: Vec<String> = self.units.keys().cloned().collect();
        // Resolve every readiness transition before trying waiting units.
        // Unit names are lexical, not topological: a dependent such as
        // `gateway` can sort before `server`. If gateway is checked first and
        // server becomes ready later in this same pass, gateway otherwise has
        // no deadline to wake the loop and may sit idle until unrelated I/O.
        for name in &names {
            let Some(unit) = self.units.get(name) else {
                continue;
            };
            if unit.phase == Phase::Stopped && unit.file.autostart && unit.pty.is_none() {
                self.want(client, name, Cause::Autostart);
            }
        }

        for name in &names {
            let Some(unit) = self.units.get(name) else {
                continue;
            };
            // A stop that did not take.
            if unit.kill_at_ms > 0 && now >= unit.kill_at_ms {
                if let Some(pty) = unit.pty {
                    let _ = client.send(&remote::msg_kill(pty, 9));
                }
                if let Some(unit) = self.units.get_mut(name) {
                    unit.kill_at_ms = 0;
                }
                continue;
            }
            if unit.phase == Phase::Activating {
                if now >= unit.deadline_ms && unit.deadline_ms > 0 {
                    self.fail_start(client, name, "timeout");
                } else if self.next_probe_ms.get(name).is_none_or(|at| now >= *at) {
                    self.probe(client, name, now);
                }
                continue;
            }
        }

        // Readiness is now settled for the whole table, so start everything
        // whose dependencies became ready in the pass above.
        for name in &names {
            let Some(unit) = self.units.get(name) else {
                continue;
            };
            if unit.pty.is_none() && unit.attempt_due(now) && self.deps_ready(name) {
                let cause = if unit.phase == Phase::Backoff {
                    Cause::Crash
                } else {
                    Cause::Policy
                };
                self.spawn(client, name, cause);
            }
        }
    }

    fn deps_ready(&self, name: &str) -> bool {
        let Some(unit) = self.units.get(name) else {
            return false;
        };
        unit.file.requires.iter().all(|dep| {
            self.units
                .get(dep)
                .is_some_and(Unit::is_ready_for_dependents)
        })
    }

    /// Record the intent to run something, pulling in what it needs.
    fn want(&mut self, client: &mut Client, name: &str, cause: Cause) {
        let reset_root = cause == Cause::Command;
        let closure = supervisor::start_closure(&self.units, name);
        let order = match supervisor::start_order(&self.units, &closure) {
            Ok(order) => order,
            Err(supervisor::Cycle(ring)) => {
                let now = self.now_ms(client);
                for member in &ring {
                    if let Some(unit) = self.units.get_mut(member) {
                        unit.phase = Phase::Failed;
                    }
                }
                self.record(
                    Record::new(name.to_string(), Event::Cycle, "failed").detail(ring.join(" -> ")),
                    now,
                );
                return;
            }
        };
        let now = self.now_ms(client);
        for member in order {
            // `start_order` walks `after` too, because ordering has to see it.
            // Only the closure says what to *start*: an `after` dependency
            // orders a unit that is already coming up and must not be brought
            // up by it.
            if !closure.contains(&member) {
                continue;
            }
            let is_root = member == name;
            let Some(unit) = self.units.get_mut(&member) else {
                continue;
            };
            if unit.phase.is_live() || unit.phase == Phase::Exited {
                continue;
            }
            if unit.phase == Phase::Held && !is_root {
                continue;
            }
            // A dependency that is already backing off from a previous failure
            // must keep its retry timer. Resetting it to Waiting would let an
            // autostarted dependent respawn the dependency immediately and
            // create a terminal storm.
            if unit.phase == Phase::Backoff && !is_root {
                continue;
            }
            // A dependency that has given up is also not to be pulled back by a
            // dependent. Leave it Failed until someone explicitly starts it.
            if unit.phase == Phase::Failed && !is_root {
                continue;
            }
            if is_root && reset_root {
                unit.reset_failure_budget();
            }
            unit.phase = Phase::Waiting;
            unit.next_attempt_ms = 0;
            let instance = unit.instance.clone();
            let cause = if is_root {
                cause.clone()
            } else {
                Cause::Dependency(name.to_string())
            };
            self.record(
                Record::new(member.clone(), Event::Start, "waiting")
                    .cause(cause)
                    .instance(instance),
                now,
            );
        }
    }

    /// Build the environment, then create the terminal.
    fn spawn(&mut self, client: &mut Client, name: &str, cause: Cause) {
        let now = self.now_ms(client);
        let needs_exec = self
            .units
            .get(name)
            .is_some_and(|u| u.file.command.is_some() || !u.file.env.is_empty());
        if needs_exec && self.features & remote::FEATURE_CREATE_EXEC == 0 {
            // Not probeable: an older server reads the environment block as
            // command text. Refuse rather than run something else.
            self.record(
                Record::new(name.to_string(), Event::Failed, "failed")
                    .detail("server does not advertise FEATURE_CREATE_EXEC"),
                now,
            );
            if let Some(unit) = self.units.get_mut(name) {
                unit.phase = Phase::Failed;
            }
            return;
        }
        let Some(unit) = self.units.get(name) else {
            return;
        };
        let file = unit.file.clone();
        let instance = unit.instance.clone();
        let seq = unit.seq;

        let cwd = expand_tilde(file.cwd.as_deref().unwrap_or("~"), &self.home);
        let resolved = self.resolve_env(client, name, &cwd);
        let ResolvedEnv { vars: env, sources } = match resolved {
            Ok(resolved) => resolved,
            Err(failure) => {
                let phase = self.note_failed_start(name, now);
                self.record(
                    Record::new(name.to_string(), Event::Failed, phase.as_str())
                        .detail(failure)
                        .instance(instance.clone()),
                    now,
                );
                return;
            }
        };

        let argv: Vec<String> = file.command.clone().unwrap_or_default();
        let argv_refs: Vec<&str> = argv.iter().map(String::as_str).collect();
        // A stamped Wayland socket for this run. Set in the terminal's
        // environment, so anything the unit launches — including a browser its
        // dev server spawns — arrives already attributed.
        let display = self.app_socket(client, name, seq);
        let mut env_refs: Vec<(&str, &str)> = env
            .iter()
            .map(|(k, v, _)| (k.as_str(), v.as_str()))
            .collect();
        if let Some(display) = &display {
            // The unit's own `env` wins: a unit pointed at another compositor
            // meant it.
            if !env_refs.iter().any(|(k, _)| *k == "WAYLAND_DISPLAY") {
                env_refs.push(("WAYLAND_DISPLAY", display));
            }
        }
        let shell = file.shell.clone().unwrap_or_default();
        let tag = supervisor::tag_for(name, seq);

        let request = CreateRequest {
            rows: ROWS,
            cols: COLS,
            tag: &tag,
            command: &shell,
            argv: file.command.is_some().then_some(argv_refs.as_slice()),
            cwd: Some(&cwd),
            env: &env_refs,
            deadline_ms: None,
        };
        let detail = if shell.is_empty() {
            argv.join(" ")
        } else {
            shell.clone()
        };
        match self.terminals.create(client, request) {
            Ok(pty) => {
                if let Some(unit) = self.units.get_mut(name) {
                    unit.pty = Some(pty);
                    unit.seq += 1;
                    unit.started_ms = now;
                    unit.stale = false;
                    unit.phase = Phase::Activating;
                    unit.deadline_ms = now + unit.file.timeout_start.ms();
                }
                self.log_cursor.remove(name);
                self.next_probe_ms.insert(name.to_string(), now);
                let event = if matches!(cause, Cause::Crash) {
                    Event::Restart
                } else {
                    Event::Spawn
                };
                self.record(
                    Record::new(name.to_string(), event, "activating")
                        .pty(pty)
                        .cause(cause)
                        .detail(detail)
                        .instance(instance)
                        .env(sources, env.len()),
                    now,
                );
            }
            Err(err) => {
                // A refused create is a failed start, never a running unit:
                // the server resolves the program before forking, so this is
                // where "no such binary" surfaces.
                let phase = self.note_failed_start(name, now);
                self.record(
                    Record::new(name.to_string(), Event::Exit, phase.as_str())
                        .detail(format!("create refused: {err:?}"))
                        .instance(instance),
                    now,
                );
            }
        }
    }

    fn note_failed_start(&mut self, name: &str, now: u64) -> Phase {
        let random = random();
        if let Some(unit) = self.units.get_mut(name) {
            unit.note_failed_start(now, random);
            unit.phase
        } else {
            Phase::Failed
        }
    }

    fn fail_start(&mut self, client: &mut Client, name: &str, why: &str) {
        let now = self.now_ms(client);
        let random = random();
        let (pty, instance, phase) = match self.units.get_mut(name) {
            Some(unit) => {
                unit.note_failed_activation(now, random);
                let pty = unit.pty;
                if pty.is_some() {
                    unit.kill_at_ms = now + unit.file.timeout_stop.ms();
                }
                (pty, unit.instance.clone(), unit.phase)
            }
            None => return,
        };
        self.record(
            Record::new(name.to_string(), Event::Failed, phase.as_str())
                .detail(why.to_string())
                .instance(instance),
            now,
        );
        if let Some(pty) = pty {
            let _ = client.send(&remote::msg_kill(pty, 15));
        }
        self.next_probe_ms.remove(name);
    }

    fn note_exit(&mut self, client: &mut Client, pty: u16, exit_status: i32) {
        let now = self.now_ms(client);
        let random = random();
        let Some(name) = self
            .units
            .iter()
            .find(|(_, u)| u.pty == Some(pty))
            .map(|(n, _)| n.clone())
        else {
            return;
        };
        let (stale, phase, instance, dependent_action) = {
            let unit = self.units.get_mut(&name).expect("just found");
            let completed_attempt = unit.phase.is_live();
            let stale = unit.note_exit(exit_status, now, random);
            let dependent_action = unit.dependent_action_after_exit(exit_status, completed_attempt);
            (stale, unit.phase, unit.instance.clone(), dependent_action)
        };
        for run in &stale {
            let _ = client.send(&remote::msg_close(run.pty));
            self.record(
                Record::new(name.clone(), Event::Reaped, phase.as_str())
                    .pty(run.pty)
                    .exit_code(run.exit_code)
                    .instance(instance.clone()),
                now,
            );
        }
        self.next_probe_ms.remove(&name);
        self.log_cursor.remove(&name);
        self.record(
            Record::new(name.clone(), Event::Exit, phase.as_str())
                .pty(pty)
                .exit_code(exit_status)
                .instance(instance),
            now,
        );

        // A normal dependency exit stops its dependents. Re-running a
        // successful oneshot is a staged replacement instead: a failure keeps
        // the old result in service, and success restarts dependents so they
        // consume the new one.
        match dependent_action {
            DependentAction::None => {}
            DependentAction::Stop | DependentAction::Restart => {
                self.stop_dependents(client, &name);
            }
        }

        // Dependency recovery asked for a new terminal once this one died.
        if let Some(unit) = self.units.get_mut(&name)
            && std::mem::take(&mut unit.restart_pending)
        {
            unit.phase = Phase::Waiting;
            unit.next_attempt_ms = 0;
        }
    }

    fn ready(&mut self, client: &mut Client, name: &str, how: &str) {
        let now = self.now_ms(client);
        let instance = self.units.get(name).and_then(|u| u.instance.clone());
        let pty = self.units.get(name).and_then(|u| u.pty);
        if let Some(unit) = self.units.get_mut(name) {
            unit.phase = Phase::Running;
            unit.deadline_ms = 0;
        }
        self.next_probe_ms.remove(name);
        let mut record = Record::new(name.to_string(), Event::Ready, "running")
            .detail(how.to_string())
            .instance(instance);
        if let Some(pty) = pty {
            record = record.pty(pty);
        }
        self.record(record, now);
    }

    /// Stop a unit and everything that requires it.
    ///
    /// `dependents` is already the transitive set, so this sweeps it flat.
    /// Recursing into it re-derives the same closure per member and stops a
    /// chain of depth *k* 2^(k-1) times, with a duplicate kill and a duplicate
    /// journal record each time.
    fn stop(&mut self, client: &mut Client, name: &str, cause: Cause, hold: bool) {
        self.stop_dependents(client, name);
        if let Some(unit) = self.units.get_mut(name) {
            unit.cancel_refresh();
        }
        self.stop_unit(client, name, cause, hold);
    }

    /// Stop dependents that are currently wanted, and leave them waiting for
    /// this dependency to become ready again. Held and idle dependents retain
    /// their intent rather than being accidentally autostarted.
    fn stop_dependents(&mut self, client: &mut Client, name: &str) {
        let dependents: Vec<String> = supervisor::dependents(&self.units, name)
            .into_iter()
            .filter(|dependent| {
                self.units
                    .get(dependent)
                    .is_some_and(Unit::wants_dependency_recovery)
            })
            .collect();
        for dependent in dependents {
            if let Some(unit) = self.units.get_mut(&dependent) {
                unit.cancel_refresh();
            }
            self.stop_unit(
                client,
                &dependent,
                Cause::Dependency(name.to_string()),
                false,
            );
            if let Some(unit) = self.units.get_mut(&dependent) {
                unit.resume_after_stop();
            }
        }
    }

    /// Stop exactly one unit. Cascading is [`Muster::stop`]'s job.
    fn stop_unit(&mut self, client: &mut Client, name: &str, cause: Cause, hold: bool) {
        let now = self.now_ms(client);
        let Some(unit) = self.units.get_mut(name) else {
            return;
        };
        let instance = unit.instance.clone();
        let pty = unit.pty;
        let stop_command = unit.file.stop_command.clone();
        unit.phase = if hold { Phase::Held } else { Phase::Stopped };
        unit.next_attempt_ms = 0;
        unit.deadline_ms = 0;
        if pty.is_some() {
            unit.kill_at_ms = now + unit.file.timeout_stop.ms();
        }
        let signal = signal_number(&unit.file.stop_signal);
        match (&stop_command, pty) {
            // A `stopCommand` replaces the signal, not the deadline: the
            // SIGKILL at `timeoutStop` still comes, because a stop command that
            // does not stop the unit is the case it exists to survive.
            (Some(argv), Some(_)) => self.run_side_command(client, name, "stop", argv.clone()),
            (_, Some(pty)) => {
                let _ = client.send(&remote::msg_kill(pty, signal));
            }
            (_, None) => {}
        }
        self.next_probe_ms.remove(name);
        self.record(
            Record::new(
                name.to_string(),
                Event::Stop,
                if hold { "held" } else { "stopped" },
            )
            .cause(cause)
            .instance(instance),
            now,
        );
    }

    /// Every restart is a new terminal: `C2S_RESTART` replays the spec the PTY
    /// was created with, so it cannot serve a restart caused by an edit.
    fn restart(&mut self, client: &mut Client, name: &str, cause: Cause) {
        let reset_failure_budget = matches!(cause, Cause::Command | Cause::File);
        let staged_refresh = self.units.get(name).is_some_and(Unit::can_stage_refresh);
        if staged_refresh {
            // A completed oneshot is still a usable dependency result. Keep
            // its consumers alive while producing a replacement; note_exit
            // commits the replacement only if it succeeds.
            if let Some(unit) = self.units.get_mut(name) {
                unit.begin_refresh();
            }
            self.stop_unit(client, name, cause.clone(), false);
        } else {
            self.stop(client, name, cause.clone(), false);
        }
        if let Some(unit) = self.units.get_mut(name)
            && reset_failure_budget
        {
            unit.reset_failure_budget();
        }
        // Starting through the ordinary path pulls in newly added dependencies
        // and checks the updated graph for cycles. The PTY guard in reconcile
        // keeps the replacement from spawning before the old terminal exits.
        self.want(client, name, cause);
    }

    /// Run a unit's `stopCommand` or `reloadCommand` in a terminal of its own.
    ///
    /// Not a run of the unit: it gets no sequence number, is never adopted, and
    /// is not retained. It is tagged all the same, so a supervisor that is
    /// replaced mid-stop can see what is still executing on its behalf — and so
    /// the terminal is identifiable rather than anonymous in `blit client
    /// list`.
    ///
    /// It inherits the unit's `cwd` and resolved environment, because a stop
    /// command that cannot see `DOCKER_HOST` or `.env` is a stop command that
    /// talks to a different machine than the one it is stopping.
    pub(crate) fn run_side_command(
        &mut self,
        client: &mut Client,
        name: &str,
        kind: &str,
        argv: Vec<String>,
    ) {
        let now = self.now_ms(client);
        let Some(unit) = self.units.get(name) else {
            return;
        };
        let instance = unit.instance.clone();
        let cwd = expand_tilde(unit.file.cwd.as_deref().unwrap_or("~"), &self.home);
        let env = match self.resolve_env(client, name, &cwd) {
            Ok(resolved) => resolved.vars,
            // The unit is being stopped, not started: a `.env` that has since
            // gone missing must not keep the stop from happening, so this falls
            // back to the bare environment and says so.
            Err(failure) => {
                self.record(
                    Record::new(name.to_string(), Event::Ran, "stopped")
                        .detail(format!("{kind}Command without its environment: {failure}"))
                        .instance(instance.clone()),
                    now,
                );
                Vec::new()
            }
        };
        let env_refs: Vec<(&str, &str)> = env
            .iter()
            .map(|(k, v, _)| (k.as_str(), v.as_str()))
            .collect();
        let argv_refs: Vec<&str> = argv.iter().map(String::as_str).collect();
        let tag = format!("{}{name}/{kind}", supervisor::TAG_PREFIX);
        let request = CreateRequest {
            rows: ROWS,
            cols: COLS,
            tag: &tag,
            command: "",
            argv: Some(argv_refs.as_slice()),
            cwd: Some(&cwd),
            env: &env_refs,
            deadline_ms: None,
        };
        let phase = self
            .units
            .get(name)
            .map_or("stopped", |unit| unit.phase.as_str());
        let detail = match self.terminals.create(client, request) {
            Ok(_) => format!("{kind}Command: {}", argv.join(" ")),
            Err(err) => format!("{kind}Command failed to start: {err:?}"),
        };
        self.record(
            Record::new(name.to_string(), Event::Ran, phase)
                .detail(detail)
                .instance(instance),
            now,
        );
    }

    fn close_all(&mut self, client: &mut Client, name: &str) {
        if let Some(unit) = self.units.get(name) {
            if let Some(pty) = unit.pty {
                let _ = client.send(&remote::msg_kill(pty, 15));
                let _ = client.send(&remote::msg_close(pty));
            }
            for run in &unit.runs {
                let _ = client.send(&remote::msg_close(run.pty));
            }
        }
    }

    // ------------------------------------------------------------- surfaces

    /// Fold one surface message into the table.
    ///
    /// Every client sees every surface, so most of these belong to something
    /// else entirely; only a stamped origin naming an `app_id` this supervisor
    /// minted attributes one to a unit.
    fn note_surface(&mut self, msg: ServerMsg<'_>, now: u64) {
        let touched = match msg {
            ServerMsg::SurfaceCreated {
                surface_id,
                width,
                height,
                title,
                ..
            } => {
                let entry = self.surfaces.entry(surface_id).or_default();
                entry.title = title.to_string();
                entry.width = width;
                entry.height = height;
                // Creation precedes the origin, so this is not yet ours to
                // claim; the origin below is what puts it in a panel.
                entry.unit.clone()
            }
            // The only trustworthy surface-to-unit link: `app_id` here is
            // stamped by the compositor from the socket the surface arrived
            // on, not asserted by the application.
            ServerMsg::SurfaceOrigin {
                surface_id,
                app_id,
                instance_id,
                ..
            } => {
                let owner = self.surface_owners.get(app_id).cloned();
                let entry = self.surfaces.entry(surface_id).or_default();
                entry.unit = owner.clone();
                entry.seq = instance_id.parse().ok();
                owner
            }
            ServerMsg::SurfaceTitle { surface_id, title } => {
                let entry = self.surfaces.entry(surface_id).or_default();
                entry.title = title.to_string();
                entry.unit.clone()
            }
            ServerMsg::SurfaceResized {
                surface_id,
                width,
                height,
                ..
            } => {
                let entry = self.surfaces.entry(surface_id).or_default();
                entry.width = width;
                entry.height = height;
                entry.unit.clone()
            }
            ServerMsg::SurfaceDestroyed { surface_id } => {
                self.surfaces.remove(&surface_id).and_then(|s| s.unit)
            }
            _ => None,
        };
        if let Some(unit) = touched {
            self.touch(&unit, now);
        }
    }

    /// The surfaces a unit's live run has open, newest id last.
    fn surfaces_of(&self, name: &str) -> Vec<(u16, &Surface)> {
        let seq = self.units.get(name).map(Unit::current_seq);
        self.surfaces
            .iter()
            .filter(|(_, surface)| surface.unit.as_deref() == Some(name))
            .filter(|(_, surface)| seq.is_none() || surface.seq.is_none() || surface.seq == seq)
            .map(|(id, surface)| (*id, surface))
            .collect()
    }

    /// Mint the stamped Wayland socket a run's windows will arrive on.
    ///
    /// Returns the basename to put in `WAYLAND_DISPLAY`. Everything the unit
    /// launches inherits it, so a browser a dev server spawns is attributed to
    /// the unit that spawned it — which is the whole point of doing this at the
    /// terminal's environment rather than by guessing at process trees.
    fn app_socket(&mut self, client: &mut Client, name: &str, seq: u64) -> Option<String> {
        if self.features & remote::process::FEATURE_APP_SOCKET == 0 {
            return None;
        }
        let app_id = supervisor::app_id_for(name);
        let nonce = self.next_nonce();
        let packet = remote::msg_app_socket_request(nonce, &app_id, &seq.to_string());
        client.send(&packet).ok()?;
        let reply = self.reply(client, remote::S2C_APP_SOCKET, nonce)?;
        let (_, status, display) = remote::parse_app_socket_reply(&reply)?;
        if status != 0 {
            return None;
        }
        self.surface_owners.insert(app_id, name.to_string());
        Some(display.to_string())
    }
    // ----------------------------------------------------------- environment

    /// Read every `envFile` and merge it with `env`.
    fn resolve_env(
        &mut self,
        client: &mut Client,
        name: &str,
        cwd: &str,
    ) -> Result<ResolvedEnv, String> {
        let Some(unit) = self.units.get(name) else {
            return Ok(ResolvedEnv::default());
        };
        // Only the two env fields are needed, and cloning the whole UnitFile
        // per spawn is the expensive way to borrow them.
        let entries = unit.file.env_file.clone();
        let inline = unit.file.env.clone();
        let mut loaded: Vec<(String, EnvFile)> = Vec::new();
        let mut sources = Vec::new();
        for entry in &entries {
            let path = rebase(&expand_tilde(&entry.path, &self.home), cwd);
            match self.read_file(client, &path) {
                Some(bytes) => {
                    let text = String::from_utf8_lossy(&bytes).into_owned();
                    loaded.push((path.clone(), envfile::parse(&text)));
                    sources.push(path);
                }
                None if entry.optional => {}
                None => return Err(format!("envFile {path} is missing")),
            }
        }
        Ok(ResolvedEnv {
            vars: envfile::merge(&loaded, &inline),
            sources,
        })
    }

    /// One-shot read of an absolute path. `FS_READ` needs no sync.
    fn read_file(&mut self, client: &mut Client, path: &str) -> Option<Vec<u8>> {
        let nonce = self.next_nonce();
        // Zero takes the server's per-file ceiling rather than inventing one.
        let packet = remote::fs::msg_fs_read_paths(nonce, 0, 0, &[path])?;
        client.send(&packet).ok()?;
        let reply = client
            .recv_matching(|p| {
                p.first() == Some(&remote::fs::S2C_FS_READ)
                    && p.len() >= 3
                    && u16::from_le_bytes([p[1], p[2]]) == nonce
            })
            .ok()??;
        let (_, _, records) = remote::fs::parse_fs_read_result(&reply)?;
        let (status, _, content) = records.into_iter().next()?;
        (status == 0).then_some(content)
    }

    // -------------------------------------------------------------- readiness

    fn probe(&mut self, client: &mut Client, name: &str, now: u64) {
        let Some(unit) = self.units.get(name) else {
            return;
        };
        let ready_when = unit.file.ready_when.clone();
        let unit_type = unit.file.unit_type;
        let pty = unit.pty;
        let started = unit.started_ms;

        // A oneshot is ready when it exits 0, which arrives as S2C_EXITED.
        if unit_type == UnitType::Oneshot {
            self.next_probe_ms
                .insert(name.to_string(), now + PROBE_INTERVAL.as_millis() as u64);
            return;
        }

        let satisfied = match &ready_when {
            ReadyWhen::Spawn => true,
            ReadyWhen::Manual => false,
            ReadyWhen::Delay(d) => now.saturating_sub(started) >= d.ms(),
            ReadyWhen::Path(path) => {
                let path = expand_tilde(path, &self.home);
                self.path_exists(client, &path)
            }
            ReadyWhen::Tcp(target) => self.tcp_connects(client, target),
            ReadyWhen::Http(url) => self.http_answers(client, url),
            ReadyWhen::Log(needle) => {
                // Not polled: the server holds the wait and answers through
                // the loop. Arm it once and take no further deadline.
                if let Some(pty) = pty {
                    self.arm_log_wait(client, name, pty, needle, now);
                }
                self.next_probe_ms.remove(name);
                return;
            }
        };
        if satisfied {
            let how = describe_ready(&ready_when);
            self.ready(client, name, &how);
        } else {
            let interval = if matches!(ready_when, ReadyWhen::Log(_)) {
                LOG_PROBE_INTERVAL
            } else {
                PROBE_INTERVAL
            };
            self.next_probe_ms
                .insert(name.to_string(), now + interval.as_millis() as u64);
        }
    }

    fn path_exists(&mut self, client: &mut Client, path: &str) -> bool {
        let nonce = self.next_nonce();
        let Some(packet) =
            remote::fs::msg_fs_read_paths(nonce, remote::fs::FS_READ_NO_CONTENT, 0, &[path])
        else {
            return false;
        };
        if client.send(&packet).is_err() {
            return false;
        }
        let deadline = client.monotonic_now() + PROBE_INTERVAL;
        let Some(reply) = self.reply_deadline(client, remote::fs::S2C_FS_READ, nonce, deadline)
        else {
            return false;
        };
        remote::fs::parse_fs_read_result(&reply)
            .and_then(|(_, _, records)| records.into_iter().next())
            .is_some_and(|(status, _, _)| status == 0)
    }

    fn tcp_connects(&mut self, client: &mut Client, target: &str) -> bool {
        let Some((host, port)) = split_host_port(target) else {
            return false;
        };
        self.net_stream = self.net_stream.wrapping_add(1).max(1);
        let stream = self.net_stream;
        let open = remote::net::NetOpen::tcp(stream, &host, port);
        if client.send(&remote::net::msg_net_open(&open)).is_err() {
            return false;
        }
        let deadline = client.monotonic_now() + PROBE_INTERVAL;
        let Some(reply) =
            self.reply_deadline(client, remote::net::S2C_NET_OPENED, stream, deadline)
        else {
            let _ = client.send(&remote::net::msg_net_close(stream, 0));
            return false;
        };
        let ok = remote::net::parse_net_opened(&reply)
            .is_some_and(|(_, status, _, _)| status == remote::net::NET_STATUS_OK);
        let _ = client.send(&remote::net::msg_net_close(stream, 0));
        ok
    }

    /// The dumbest possible HTTP: connect, GET, read the status line. No TLS,
    /// no redirects, no body — a probe, not a client.
    fn http_answers(&mut self, client: &mut Client, url: &str) -> bool {
        let rest = match url.strip_prefix("http://") {
            Some(rest) => rest,
            None => return false,
        };
        let (authority, path) = match rest.find('/') {
            Some(at) => (&rest[..at], &rest[at..]),
            None => (rest, "/"),
        };
        let Some((host, port)) = split_host_port_default(authority, 80) else {
            return false;
        };
        self.net_stream = self.net_stream.wrapping_add(1).max(1);
        let stream = self.net_stream;
        let open = remote::net::NetOpen::tcp(stream, &host, port);
        if client.send(&remote::net::msg_net_open(&open)).is_err() {
            return false;
        }
        let deadline = client.monotonic_now() + PROBE_INTERVAL;
        let opened = self.reply_deadline(client, remote::net::S2C_NET_OPENED, stream, deadline);
        let connected = opened
            .as_ref()
            .and_then(|reply| remote::net::parse_net_opened(reply))
            .is_some_and(|(_, status, _, _)| status == remote::net::NET_STATUS_OK);
        if !connected {
            let _ = client.send(&remote::net::msg_net_close(stream, 0));
            return false;
        }
        let request =
            format!("GET {path} HTTP/1.0\r\nHost: {authority}\r\nConnection: close\r\n\r\n");
        let _ = client.send(&remote::net::msg_net_data_c2s(stream, request.as_bytes()));
        let deadline = client.monotonic_now() + PROBE_INTERVAL;
        let answer = self.reply_deadline(client, remote::net::S2C_NET_DATA, stream, deadline);
        let _ = client.send(&remote::net::msg_net_close(stream, 0));
        let Some(data) = answer else { return false };
        // [op][stream:2][payload...]
        let body = &data[3.min(data.len())..];
        let head = String::from_utf8_lossy(&body[..body.len().min(64)]);
        head.split_whitespace()
            .nth(1)
            .and_then(|code| code.parse::<u16>().ok())
            .is_some_and(|code| code < 500)
    }

    /// Arm one `C2S_TERM_WAIT` and let the answer come back through the loop.
    ///
    /// The server blocks; muster does not. Waiting on the reply here would park
    /// the single receive loop for the whole of `timeoutStart` — every other
    /// unit's exit and every CLI invocation behind it — which is worse than the
    /// poll this replaces. So the wait is armed once, its nonce remembered, and
    /// `route` turns the reply into a readiness decision whenever it lands.
    ///
    /// The cursor comes from a `SINCE_PROBE` taken now, so the match is text
    /// that arrives *after* the unit started rather than whatever was already
    /// on screen. That one round trip is bounded and happens once per start.
    fn arm_log_wait(&mut self, client: &mut Client, name: &str, pty: u16, needle: &str, now: u64) {
        if self.log_waits.values().any(|waiting| waiting == name) {
            return;
        }
        let (from_seq, from_col) = match self.log_cursor.get(name) {
            Some(cursor) => *cursor,
            None => {
                let nonce = self.next_nonce();
                let packet = remote::journal::msg_term_since(
                    nonce,
                    pty,
                    0,
                    0,
                    0,
                    remote::journal::SINCE_PROBE,
                );
                if client.send(&packet).is_err() {
                    return;
                }
                let Some(reply) = self.term_output(client, nonce) else {
                    return;
                };
                let cursor = (reply.next_seq, reply.next_col);
                self.log_cursor.insert(name.to_string(), cursor);
                cursor
            }
        };
        let remaining = self
            .units
            .get(name)
            .map(|unit| unit.deadline_ms.saturating_sub(now))
            .unwrap_or(0);
        let nonce = self.next_nonce();
        let packet = remote::journal::msg_term_wait(
            nonce,
            pty,
            from_seq,
            from_col,
            0,
            remaining as u32,
            0,
            needle,
        );
        if client.send(&packet).is_ok() {
            self.log_waits.insert(nonce, name.to_string());
        }
    }

    /// A `C2S_TERM_WAIT` came back. Matched means ready; anything else means
    /// the wait timed out, and `timeoutStart` decides what that costs.
    fn note_log_wait(&mut self, client: &mut Client, packet: &[u8]) -> bool {
        if packet.first() != Some(&remote::journal::S2C_TERM_OUTPUT) || packet.len() < 3 {
            return false;
        }
        let nonce = u16::from_le_bytes([packet[1], packet[2]]);
        let Some(name) = self.log_waits.remove(&nonce) else {
            return false;
        };
        let Some(reply) = remote::journal::parse_s2c_term_output(packet) else {
            return true;
        };
        self.log_cursor
            .insert(name.clone(), (reply.next_seq, reply.next_col));
        // A wait armed for a run that has since been replaced must not declare
        // its successor ready: the reply describes a terminal that is gone.
        let current = self
            .units
            .get(&name)
            .is_some_and(|unit| unit.phase == Phase::Activating && unit.pty == Some(reply.pty_id));
        if current && reply.flags & remote::journal::OUTPUT_MATCHED != 0 {
            self.ready(client, &name, "log");
        }
        true
    }

    fn term_output(
        &mut self,
        client: &mut Client,
        nonce: u16,
    ) -> Option<remote::journal::OutputReply> {
        let reply = self.reply(client, remote::journal::S2C_TERM_OUTPUT, nonce)?;
        remote::journal::parse_s2c_term_output(&reply)
    }

    /// Wait for the answer to a correlated request.
    ///
    /// Every family muster asks a question of answers with the correlation id
    /// in the same two bytes, so this is the one place that offset is written
    /// down. `recv_matching` buffers everything else for the loop.
    fn reply(&mut self, client: &mut Client, opcode: u8, id: u16) -> Option<Vec<u8>> {
        self.reply_deadline(client, opcode, id, MonotonicInstant::MAX)
    }

    /// Like `reply`, but returns `None` if the deadline expires first.
    ///
    /// Probes use this so a single unresponsive check cannot park the whole
    /// supervisor loop: the packet they were waiting for is kept in `pending`
    /// and will be handled on the next loop iteration.
    fn reply_deadline(
        &mut self,
        client: &mut Client,
        opcode: u8,
        id: u16,
        deadline: MonotonicInstant,
    ) -> Option<Vec<u8>> {
        client
            .recv_matching_deadline(
                |packet| {
                    packet.first() == Some(&opcode)
                        && packet.len() >= 3
                        && u16::from_le_bytes([packet[1], packet[2]]) == id
                },
                deadline,
            )
            .ok()?
    }

    fn record(&mut self, record: Record, now: u64) {
        let stored = self.journal.push(record, now).clone();
        self.touch(&stored.unit, now);
        self.publish_event(&stored, now);
    }

    /// A verb a panel sent, with the same meaning the CLI gives it.
    fn panel_command(&mut self, client: &mut Client, verb: &str, name: &str, now: u64) {
        match verb {
            // Not "re-read the directory": the watch did that already. This is
            // the retry for a directory whose watch was refused, which is the
            // only thing a panel could ask for that it does not already have.
            "rewatch" => self.retry_unwatchable(client, now, true),
            "resync" => self.touch_all(now),
            "start" | "stop" | "restart" if !name.is_empty() => {
                for member in self.resolve_name(name) {
                    match verb {
                        "start" => self.want(client, &member, Cause::Command),
                        "stop" => self.stop(client, &member, Cause::Command, true),
                        _ => self.restart(client, &member, Cause::Command),
                    }
                }
            }
            _ => {}
        }
    }

    /// A name is a unit, or an instance standing for its members.
    pub(crate) fn resolve_name(&self, name: &str) -> Vec<String> {
        if self.units.contains_key(name) {
            return vec![name.to_string()];
        }
        self.instances
            .get(name)
            .map(|instance| instance.members.clone())
            .unwrap_or_default()
    }
}

// ------------------------------------------------------------------ helpers

fn same_spec(a: &UnitFile, b: &UnitFile) -> bool {
    a.requires == b.requires
        && a.wants == b.wants
        && a.after == b.after
        && a.command == b.command
        && a.shell == b.shell
        && a.cwd == b.cwd
        && a.env == b.env
        && a.env_file == b.env_file
        && a.unit_type == b.unit_type
        && a.ready_when == b.ready_when
}

fn expand_tilde(path: &str, home: &str) -> String {
    match path.strip_prefix('~') {
        Some(rest) => format!("{home}{rest}"),
        None => path.to_string(),
    }
}

/// Make a path absolute against a base. Used for `envFile` against `cwd` and
/// for a template's relative paths against its stack directory.
fn rebase(path: &str, cwd: &str) -> String {
    if path.starts_with('/') {
        path.to_string()
    } else {
        format!("{}/{}", cwd.trim_end_matches('/'), path)
    }
}

fn split_host_port(target: &str) -> Option<(String, u16)> {
    split_host_port_default(target, 0).filter(|(_, port)| *port != 0)
}

fn split_host_port_default(target: &str, default: u16) -> Option<(String, u16)> {
    match target.rsplit_once(':') {
        Some((host, port)) => Some((host.to_string(), port.parse().ok()?)),
        None => Some((target.to_string(), default)),
    }
}

fn describe_ready(ready: &ReadyWhen) -> String {
    match ready {
        ReadyWhen::Spawn => "spawn".into(),
        ReadyWhen::Manual => "manual".into(),
        ReadyWhen::Delay(d) => format!("delay:{}ms", d.ms()),
        ReadyWhen::Path(p) => format!("path:{p}"),
        ReadyWhen::Log(l) => format!("log:{l}"),
        ReadyWhen::Tcp(t) => format!("tcp:{t}"),
        // The scheme already names the probe. Prefixing it produced the
        // user-facing `http:http://…` in status and journal output.
        ReadyWhen::Http(u) => u.clone(),
    }
}

/// The signals worth naming. Anything else is taken as a number.
fn signal_number(name: &str) -> i32 {
    match name.trim().trim_start_matches("SIG") {
        "HUP" => 1,
        "INT" => 2,
        "QUIT" => 3,
        "KILL" => 9,
        "USR1" => 10,
        "USR2" => 12,
        "TERM" => 15,
        other => other.parse().unwrap_or(15),
    }
}

mod cli;
mod panel;

/// What a start resolved from `envFile` + `env`, and which files it read.
#[derive(Default)]
struct ResolvedEnv {
    vars: Vec<(String, String, Origin)>,
    sources: Vec<String>,
}

/// One uniform `u64` from the host, for backoff jitter.
fn random() -> u64 {
    let mut bytes = [0u8; 8];
    let _ = blit_guest::host::random(&mut bytes);
    u64::from_le_bytes(bytes)
}

/// What one instance resolved to.
struct Expansion {
    members: Vec<String>,
    units: Vec<Unit>,
    ports: Option<(i64, u32)>,
}

/// Resolve a unit's relative paths against the directory its file came from.
///
/// This is what lets a repository-resident stack say `"cwd": "../.."` and mean
/// its own checkout rather than the server's working directory. It applies to
/// included units too: where a file lives is what "relative" means, and a rule
/// that held only for templates would be a rule with an exception.
fn rebase_unit_paths(file: &mut UnitFile, base: &str) {
    let relative = |path: &str| !path.starts_with('/') && !path.starts_with('~');
    if let Some(cwd) = &file.cwd
        && relative(cwd)
    {
        file.cwd = Some(format!("{base}/{cwd}"));
    }
    for entry in &mut file.env_file {
        if relative(&entry.path) {
            entry.path = format!("{base}/{}", entry.path);
        }
    }
}

/// A Wayland surface, and the run it belongs to.
///
/// `unit` is `None` for a surface nothing here owns — every other client's
/// windows arrive on the same broadcast, and attributing them would be a lie.
#[derive(Clone, Debug, Default)]
struct Surface {
    unit: Option<String>,
    /// The run's sequence, from the socket's instance id, so a window is tied
    /// to the run that opened it rather than merely to the unit.
    seq: Option<u64>,
    title: String,
    width: u16,
    height: u16,
}

/// Whether a packet is one of the surface messages the table is built from.
///
/// Named rather than a numeric range: the surface opcodes are not contiguous —
/// origin sits at 0x32, well away from the 0x2x block the rest occupy.
fn is_surface_message(packet: &[u8]) -> bool {
    matches!(
        packet.first().copied(),
        Some(
            remote::S2C_SURFACE_CREATED
                | remote::S2C_SURFACE_DESTROYED
                | remote::S2C_SURFACE_TITLE
                | remote::S2C_SURFACE_RESIZED
                | remote::S2C_SURFACE_ORIGIN
        )
    )
}

#[cfg(test)]
mod spec_tests {
    use super::*;

    #[test]
    fn default_and_named_servers_get_separate_muster_directories() {
        let vars = BTreeMap::from([("HOME", "/home/me"), ("BLIT_SERVER_NAME", "work")]);
        assert_eq!(
            resolve_dir_from(|key| vars.get(key).map(|value| (*value).to_owned())).unwrap(),
            "/home/me/.config/blit/instances/work/muster"
        );
        assert_eq!(
            resolve_dir_from(|key| (key == "HOME").then(|| "/home/me".to_owned())).unwrap(),
            "/home/me/.config/blit/instances/default/muster"
        );
    }

    #[test]
    fn explicit_muster_directory_still_wins() {
        let vars = BTreeMap::from([
            ("BLIT_MUSTER_DIR", "/srv/muster"),
            ("BLIT_SERVER_NAME", "work"),
        ]);
        assert_eq!(
            resolve_dir_from(|key| vars.get(key).map(|value| (*value).to_owned())).unwrap(),
            "/srv/muster"
        );
    }

    fn file(json: &str) -> UnitFile {
        serde_json::from_str(json).unwrap()
    }

    #[test]
    fn execution_readiness_and_dependencies_are_spec() {
        let base = file(r#"{"command":["api"]}"#);
        for changed in [
            file(r#"{"command":["api","--debug"]}"#),
            file(r#"{"command":["api"],"requires":["db"]}"#),
            file(r#"{"command":["api"],"wants":["mail"]}"#),
            file(r#"{"command":["api"],"after":["migrate"]}"#),
            file(r#"{"command":["api"],"readyWhen":{"tcp":"127.0.0.1:80"}}"#),
        ] {
            assert!(!same_spec(&base, &changed));
        }
    }

    #[test]
    fn policy_changes_apply_without_replacing_the_process() {
        let base = file(r#"{"command":["api"]}"#);
        let changed =
            file(r#"{"command":["api"],"description":"new","restartOnFailure":false,"keep":4}"#);
        assert!(same_spec(&base, &changed));
    }
}
