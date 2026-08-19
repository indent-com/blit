//! `@muster` — the protocol half: one receive loop that services CLI
//! invocations, unit exits, filesystem changes, and readiness deadlines
//! together.
//!
//! The shape matters. Every blocking entry point in the SDK waits for *its own*
//! packet, so an extension parked in `CommandProvider::accept` cannot notice a
//! unit that died and cannot let a backoff deadline come due. So this owns the
//! loop — `wait_until(next deadline)`, then `recv`, then route by opcode — and
//! uses the SDK's non-blocking `offer` to hand channel packets over.

use blit_ext_muster::config::{
    self, ConfigError, InstanceFile, ReadyWhen, StackFile, TopLevel, UnitFile, UnitType,
};
use blit_ext_muster::envfile::{self, EnvFile, Origin};
use blit_ext_muster::journal::{Cause, Event, Journal, Record};
use blit_ext_muster::supervisor::{self, Phase, Run, Unit};
use blit_guest::command::{CommandProvider, ProviderEvent};
use blit_guest::remote::{self, ServerMsg};
use blit_guest::terminal::{CreateRequest, TerminalSubscriptions};
use blit_guest::{Client, EXIT_BOOTSTRAP_FAILURE, MonotonicInstant, WaitOutcome};
use serde_json::Value;
use std::collections::BTreeMap;
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
    {"path":["reload"],"summary":"Re-read the directory now",
     "usage":"blit @muster reload"},
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

// The panel channel `blit.muster.v1` is specified in docs/design/muster.md and
// not implemented yet: the supervisor and its CLI come first, and a panel with
// nothing to show is not worth the flow control.

/// Terminals are created at a fixed size: nothing subscribes to them here, and
/// a client that attaches resizes to its own pane.
const ROWS: u16 = 40;
const COLS: u16 = 120;

/// How often a `path`/`tcp`/`http` probe is retried while activating.
const PROBE_INTERVAL: Duration = Duration::from_millis(250);
/// `log` polls faster: it is racing a ring buffer, not a listening socket.
const LOG_PROBE_INTERVAL: Duration = Duration::from_millis(100);
/// An idle tick, so a directory that changed without an event is still noticed.
const IDLE_TICK: Duration = Duration::from_secs(30);

fn main() {}

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
    mirror: remote::fs::FsMirror,
    sync_id: Option<u16>,
    units: BTreeMap<String, Unit>,
    stacks: BTreeMap<String, StackFile>,
    instances: BTreeMap<String, Instance>,
    journal: Journal,
    /// Everything `doctor` should say, rebuilt on every load.
    findings: Vec<ConfigError>,
    /// `log:` readiness cursors, keyed by unit.
    log_cursor: BTreeMap<String, (u64, u16)>,
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
}

#[derive(Clone, Debug)]
struct Instance {
    stack: String,
    vars: BTreeMap<String, Value>,
    members: Vec<String>,
}

fn run(mut client: Client, initial: &[Vec<u8>]) -> Result<(), String> {
    let features = client.context().hello.features;
    require(features, remote::fs::FEATURE_FS, "FS")?;
    require(features, remote::env::FEATURE_ENV, "ENV")?;

    let adoptable = adoptable_tags(initial);
    let dir = resolve_dir(&mut client)?;
    let mut muster = Muster {
        dir,
        mirror: remote::fs::FsMirror::new(),
        sync_id: None,
        units: BTreeMap::new(),
        stacks: BTreeMap::new(),
        instances: BTreeMap::new(),
        journal: Journal::new(1),
        findings: Vec::new(),
        log_cursor: BTreeMap::new(),
        next_probe_ms: BTreeMap::new(),
        terminals: TerminalSubscriptions::new(),
        nonce: 1,
        net_stream: 1,
        adoptable: adoptable.0,
        exited: adoptable.1,
        features,
    };

    // A supervisor that silently ran something other than the file asked for
    // would be worse than one that refuses: neither exec block is probeable,
    // and an older server reads the environment as command text.
    if features & remote::FEATURE_CREATE_EXEC == 0 {
        eprintln!(
            "muster: server does not advertise FEATURE_CREATE_EXEC; \
             units with a command or env cannot be started"
        );
    }

    muster.start_watch(&mut client)?;

    let listener_name = format!(
        "blit.cli.{:016x}.{}",
        client.context().extension_id,
        client.context().attempt
    );
    let listener = client
        .listen_channel(&listener_name, b"")
        .map_err(|e| format!("cli listener: {e:?}"))?;
    let mut provider = match CommandProvider::register(&mut client, listener, DESCRIPTOR) {
        Ok(provider) => Some(provider),
        // A transient run registers nothing; supervising still works.
        Err(err) => {
            eprintln!("muster: no @muster commands ({err:?})");
            None
        }
    };

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
                if let Some(provider) = provider.as_mut()
                    && let Ok(Some(event)) = provider.offer(&mut client, &packet)
                {
                    if let ProviderEvent::Invocation(invocation) = event {
                        muster.serve(&mut client, invocation);
                    }
                    continue;
                }
                muster.route(&mut client, &packet);
            }
        }
        muster.reconcile(&mut client);
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
    if let Some(explicit) = get("BLIT_MUSTER_DIR") {
        return Ok(explicit);
    }
    if let Some(xdg) = get("XDG_CONFIG_HOME").filter(|s| !s.is_empty()) {
        return Ok(format!("{xdg}/blit/muster"));
    }
    let home = get("HOME")
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "/root".into());
    Ok(format!("{home}/.config/blit/muster"))
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
                    .filter(|e| e.tag.starts_with("muster/"))
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

    /// One sync over the whole directory. Not recursive would miss stacks; the
    /// second level is dropped when the mirror is read instead.
    fn start_watch(&mut self, client: &mut Client) -> Result<(), String> {
        let nonce = self.next_nonce();
        let packet = remote::fs::msg_fs_sync(
            nonce,
            remote::fs::FS_SYNC_RECURSIVE | remote::fs::FS_SYNC_CONTENT,
            200,
            64 * 1024,
            &self.dir,
        );
        client
            .send(&packet)
            .map_err(|e| format!("fs sync: {e:?}"))?;
        Ok(())
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
        let idle = client.monotonic_now() + IDLE_TICK;
        match soonest {
            Some(at) => client.monotonic_now() + Duration::from_millis(at.saturating_sub(now)),
            None => idle,
        }
    }

    fn route(&mut self, client: &mut Client, packet: &[u8]) {
        match packet.first().copied() {
            // [0x40][nonce:2][sync_id:2][status:1] — the sync id is *after*
            // the nonce, and reading the nonce as the id silently rejects
            // every update that follows.
            Some(remote::fs::S2C_FS_SYNCED) => {
                if packet.len() >= 6 {
                    let status = packet[5];
                    if status == remote::fs::FS_STATUS_OK {
                        self.sync_id = Some(u16::from_le_bytes([packet[3], packet[4]]));
                    } else {
                        // A directory that cannot be watched is the difference
                        // between "no units" and "no configuration", and
                        // silence does not distinguish them.
                        self.findings.push(ConfigError::new(
                            self.dir.clone(),
                            format!("cannot watch this directory (status {status})"),
                        ));
                    }
                }
            }
            Some(remote::fs::S2C_FS_UPDATE) => {
                let Some(sync_id) = self.sync_id else { return };
                if packet.len() < 8 || u16::from_le_bytes([packet[1], packet[2]]) != sync_id {
                    return;
                }
                if let Some(update_id) = self.mirror.apply_update(packet) {
                    let _ = client.send(&remote::fs::msg_fs_ack(sync_id, update_id));
                    self.load(client);
                }
            }
            _ => {
                if let Some(ServerMsg::Exited {
                    pty_id,
                    exit_status,
                    ..
                }) = remote::parse_server_msg(packet)
                {
                    self.note_exit(client, pty_id, exit_status)
                }
            }
        }
    }

    // ---------------------------------------------------------------- loading

    /// Rebuild the unit table from the mirror.
    ///
    /// A file that does not parse never displaces the one that did: the running
    /// unit keeps running, the failure is journaled, and `doctor` lists it.
    fn load(&mut self, client: &mut Client) {
        let now = self.now_ms(client);
        self.findings.clear();

        let mut files: BTreeMap<String, Vec<u8>> = BTreeMap::new();
        for (path, node) in &self.mirror.live {
            if path.starts_with('.') || path.contains("/.") {
                continue;
            }
            if !path.ends_with(".json") {
                continue;
            }
            // Nothing below the second level is read.
            if path.matches('/').count() > 1 {
                continue;
            }
            if let Some(content) = &node.content {
                files.insert(path.clone(), content.clone());
            }
        }

        // Stacks first: an instance cannot resolve without its declarations.
        self.stacks.clear();
        for (path, bytes) in &files {
            let Some((dir, base)) = path.rsplit_once('/') else {
                continue;
            };
            if base != "stack.json" {
                continue;
            }
            match config::parse_json(path, bytes).and_then(|v| {
                serde_json::from_value(v).map_err(|e| ConfigError::new(path, e.to_string()))
            }) {
                Ok(stack) => {
                    self.stacks.insert(dir.to_string(), stack);
                }
                Err(err) => self.findings.push(err),
            }
        }
        // A stack directory with templates but no stack.json still works: it
        // simply declares no parameters.
        for path in files.keys() {
            if let Some((dir, _)) = path.rsplit_once('/')
                && !self.stacks.contains_key(dir)
            {
                self.stacks.insert(dir.to_string(), StackFile::default());
            }
        }

        let mut wanted: BTreeMap<String, Unit> = BTreeMap::new();
        let mut instances: BTreeMap<String, Instance> = BTreeMap::new();

        for (path, bytes) in &files {
            if path.contains('/') {
                continue;
            }
            let name = path.trim_end_matches(".json").to_string();
            match config::parse_top_level(path, bytes) {
                Ok(TopLevel::Unit(file)) => {
                    wanted.insert(name.clone(), Unit::new(name, None, *file));
                }
                Ok(TopLevel::Instance(instance)) => match self.expand(&name, &instance, &files) {
                    Ok((members, units)) => {
                        for unit in units {
                            wanted.insert(unit.name.clone(), unit);
                        }
                        instances.insert(
                            name.clone(),
                            Instance {
                                stack: instance.stack.clone(),
                                vars: instance.vars.clone(),

                                members,
                            },
                        );
                    }
                    Err(err) => self.findings.push(ConfigError::new(path, err)),
                },
                Err(err) => self.findings.push(err),
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

    /// Turn one instance into its units.
    fn expand(
        &self,
        instance_name: &str,
        instance: &InstanceFile,
        files: &BTreeMap<String, Vec<u8>>,
    ) -> Result<(Vec<String>, Vec<Unit>), String> {
        let stack = self
            .stacks
            .get(&instance.stack)
            .ok_or_else(|| format!("no stack named {:?}", instance.stack))?;
        let vars = config::bind_vars(instance_name, &instance.stack, stack, &instance.vars)?;

        let mut members = Vec::new();
        let mut units = Vec::new();
        for (path, bytes) in files {
            let Some((dir, base)) = path.rsplit_once('/') else {
                continue;
            };
            if dir != instance.stack || base == "stack.json" {
                continue;
            }
            let template = base.trim_end_matches(".json");
            if instance.omit.iter().any(|o| o == template) {
                continue;
            }
            let mut value = config::parse_json(path, bytes).map_err(|e| e.detail)?;
            config::substitute(&mut value, &vars)?;
            let file: UnitFile =
                serde_json::from_value(value).map_err(|e| format!("{path}: {e}"))?;
            file.validate(path).map_err(|e| e.detail)?;
            let name = format!("{template}@{instance_name}");
            members.push(name.clone());

            // Inside a stack, dependencies name templates and always resolve
            // within the same instance.
            let mut file = file;
            let qualify = |names: &mut Vec<String>| {
                for n in names.iter_mut() {
                    *n = format!("{n}@{instance_name}");
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
        Ok((members, units))
    }

    /// Two instances whose port blocks overlap is the failure mode of running
    /// several dev stacks, and it presents as EADDRINUSE in whichever lost.
    ///
    /// Takes the freshly parsed map: reading `self.instances` here would
    /// inspect the *previous* generation, which is empty on the first load and
    /// stale on every one after it.
    fn check_ports(&mut self, instances: &BTreeMap<String, Instance>) {
        let mut blocks: Vec<(String, i64, u32)> = Vec::new();
        for (name, instance) in instances {
            if let Some(stack) = self.stacks.get(&instance.stack)
                && let Some((base, span)) = config::port_span(stack, &instance.vars)
            {
                blocks.push((name.clone(), base, span));
            }
        }
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
                        client,
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
                        self.record(
                            client,
                            Record::new(name.clone(), Event::Changed, phase.as_str())
                                .instance(instance),
                            now,
                        );
                        if phase.is_live() && fresh.file.restart_on_change {
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
                    client,
                    Record::new(name, Event::Unloaded, "stopped").instance(unit.instance),
                    now,
                );
            }
        }

        self.instances = instances;
        for name in restart {
            self.restart(client, &name, Cause::File);
        }
    }

    // -------------------------------------------------------------- lifecycle

    /// Adopt the terminals a previous supervisor left running.
    fn adopt(&mut self, client: &mut Client) {
        let now = self.now_ms(client);
        let tags = std::mem::take(&mut self.adoptable);
        if tags.is_empty() {
            return;
        }
        // Per unit, sort by sequence: the highest is the live run.
        let mut by_unit: BTreeMap<String, Vec<(u64, u16)>> = BTreeMap::new();
        for (pty, tag) in tags {
            let rest = tag.trim_start_matches("muster/");
            let Some((name, seq)) = rest.rsplit_once('/') else {
                continue;
            };
            let Ok(seq) = seq.parse::<u64>() else {
                continue;
            };
            by_unit
                .entry(name.to_string())
                .or_default()
                .push((seq, pty));
        }
        for (name, mut runs) in by_unit {
            runs.sort_unstable();
            let Some(unit) = self.units.get_mut(&name) else {
                // A unit or instance that no longer exists takes its history
                // with it.
                for (_, pty) in runs {
                    let _ = client.send(&remote::msg_close(pty));
                }
                continue;
            };
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
            self.record(client, record, now);
        }
    }

    /// Start whatever is due, probe whatever is activating, kill whatever
    /// outstayed its stop grace.
    fn reconcile(&mut self, client: &mut Client) {
        let now = self.now_ms(client);

        // Autostart: a unit that has never run and says so.
        let names: Vec<String> = self.units.keys().cloned().collect();
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
            if unit.attempt_due(now) && self.deps_ready(name) {
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
        unit.file
            .requires
            .iter()
            .all(|dep| self.units.get(dep).is_some_and(|d| d.phase.is_ready()))
    }

    /// Record the intent to run something, pulling in what it needs.
    fn want(&mut self, client: &mut Client, name: &str, cause: Cause) {
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
                    client,
                    Record::new(name.to_string(), Event::Cycle, "failed").detail(ring.join(" -> ")),
                    now,
                );
                return;
            }
        };
        let now = self.now_ms(client);
        for member in order {
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
            unit.phase = Phase::Waiting;
            unit.next_attempt_ms = 0;
            let instance = unit.instance.clone();
            let cause = if is_root {
                cause.clone()
            } else {
                Cause::Dependency(name.to_string())
            };
            self.record(
                client,
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
                client,
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

        let cwd = expand_tilde(file.cwd.as_deref().unwrap_or("~"), &self.home());
        let (env, sources, failure) = self.resolve_env(client, name, &cwd);
        if let Some(failure) = failure {
            self.record(
                client,
                Record::new(name.to_string(), Event::Failed, "backoff")
                    .detail(failure)
                    .instance(instance.clone()),
                now,
            );
            self.after_failed_spawn(client, name, now);
            return;
        }

        let argv: Vec<String> = file.command.clone().unwrap_or_default();
        let argv_refs: Vec<&str> = argv.iter().map(String::as_str).collect();
        let env_refs: Vec<(&str, &str)> = env
            .iter()
            .map(|(k, v, _)| (k.as_str(), v.as_str()))
            .collect();
        let shell = file.shell.clone().unwrap_or_default();
        let tag = format!("muster/{name}/{seq}");

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
                    client,
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
                self.record(
                    client,
                    Record::new(name.to_string(), Event::Exit, "backoff")
                        .detail(format!("create refused: {err:?}"))
                        .instance(instance),
                    now,
                );
                self.after_failed_spawn(client, name, now);
            }
        }
    }

    fn after_failed_spawn(&mut self, client: &mut Client, name: &str, now: u64) {
        let random = self.random(client);
        if let Some(unit) = self.units.get_mut(name) {
            unit.phase = Phase::Running;
            unit.note_exit(-1, now, random);
        }
    }

    fn fail_start(&mut self, client: &mut Client, name: &str, why: &str) {
        let now = self.now_ms(client);
        let (pty, instance) = match self.units.get(name) {
            Some(unit) => (unit.pty, unit.instance.clone()),
            None => return,
        };
        self.record(
            client,
            Record::new(name.to_string(), Event::Failed, "backoff")
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
        let random = self.random(client);
        let Some(name) = self
            .units
            .iter()
            .find(|(_, u)| u.pty == Some(pty))
            .map(|(n, _)| n.clone())
        else {
            return;
        };
        let (stale, phase, instance) = {
            let unit = self.units.get_mut(&name).expect("just found");
            let stale = unit.note_exit(exit_status, now, random);
            (stale, unit.phase, unit.instance.clone())
        };
        for run in &stale {
            let _ = client.send(&remote::msg_close(run.pty));
            self.record(
                client,
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
            client,
            Record::new(name.clone(), Event::Exit, phase.as_str())
                .pty(pty)
                .exit_code(exit_status)
                .instance(instance),
            now,
        );

        // A dependent may not outlive what it requires.
        for dependent in supervisor::dependents(&self.units, &name) {
            let leaves = self.units.get(&name).is_some_and(|u| !u.phase.is_ready());
            if leaves {
                self.stop_one(client, &dependent, Cause::Dependency(name.clone()), false);
            }
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
        self.record(client, record, now);
    }

    /// Stop a unit, and everything that requires it.
    fn stop_one(&mut self, client: &mut Client, name: &str, cause: Cause, hold: bool) {
        let now = self.now_ms(client);
        for dependent in supervisor::dependents(&self.units, name) {
            self.stop_one(
                client,
                &dependent,
                Cause::Dependency(name.to_string()),
                false,
            );
        }
        let Some(unit) = self.units.get_mut(name) else {
            return;
        };
        let instance = unit.instance.clone();
        let pty = unit.pty;
        unit.phase = if hold { Phase::Held } else { Phase::Stopped };
        unit.next_attempt_ms = 0;
        unit.deadline_ms = 0;
        if pty.is_some() {
            unit.kill_at_ms = now + unit.file.timeout_stop.ms();
        }
        let signal = signal_number(&unit.file.stop_signal);
        if let Some(pty) = pty {
            let _ = client.send(&remote::msg_kill(pty, signal));
        }
        self.next_probe_ms.remove(name);
        self.record(
            client,
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
        self.stop_one(client, name, cause.clone(), false);
        if let Some(unit) = self.units.get_mut(name) {
            if unit.pty.is_none() {
                unit.phase = Phase::Waiting;
            } else {
                // The exit will land in `note_exit`; autostart brings it back.
                unit.phase = Phase::Stopped;
                unit.file.autostart = true;
            }
        }
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

    // ----------------------------------------------------------- environment

    fn home(&self) -> String {
        // The config dir is derived from HOME, so it carries it.
        match self.dir.find("/.config/") {
            Some(at) => self.dir[..at].to_string(),
            None => String::from("/"),
        }
    }

    /// Read every `envFile`, merge with `env`, and report which files were
    /// read. `None` failure means the environment resolved.
    #[allow(clippy::type_complexity)]
    fn resolve_env(
        &mut self,
        client: &mut Client,
        name: &str,
        cwd: &str,
    ) -> (Vec<(String, String, Origin)>, Vec<String>, Option<String>) {
        let Some(unit) = self.units.get(name) else {
            return (Vec::new(), Vec::new(), None);
        };
        let file = unit.file.clone();
        let mut loaded: Vec<(String, EnvFile)> = Vec::new();
        let mut sources = Vec::new();
        for entry in &file.env_file {
            let path = absolute(&expand_tilde(&entry.path, &self.home()), cwd);
            match self.read_file(client, &path) {
                Some(bytes) => {
                    let text = String::from_utf8_lossy(&bytes).into_owned();
                    loaded.push((path.clone(), envfile::parse(&text)));
                    sources.push(path);
                }
                None if entry.optional => {}
                None => {
                    return (
                        Vec::new(),
                        Vec::new(),
                        Some(format!("envFile {path} is missing")),
                    );
                }
            }
        }
        (envfile::merge(&loaded, &file.env), sources, None)
    }

    /// One-shot read of an absolute path. `FS_READ` needs no sync.
    fn read_file(&mut self, client: &mut Client, path: &str) -> Option<Vec<u8>> {
        let nonce = self.next_nonce();
        let packet = remote::fs::msg_fs_read_paths(nonce, 0, 1024 * 1024, &[path])?;
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
                let path = expand_tilde(path, &self.home());
                self.path_exists(client, &path)
            }
            ReadyWhen::Tcp(target) => self.tcp_connects(client, target),
            ReadyWhen::Http(url) => self.http_answers(client, url),
            ReadyWhen::Log(needle) => match pty {
                Some(pty) => self.log_matches(client, name, pty, needle),
                None => false,
            },
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
        let Ok(Some(reply)) = client.recv_matching(|p| {
            p.first() == Some(&remote::fs::S2C_FS_READ)
                && p.len() >= 3
                && u16::from_le_bytes([p[1], p[2]]) == nonce
        }) else {
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
        let Ok(Some(reply)) = client.recv_matching(|p| {
            p.first() == Some(&remote::net::S2C_NET_OPENED)
                && p.len() >= 3
                && u16::from_le_bytes([p[1], p[2]]) == stream
        }) else {
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
        let opened = client.recv_matching(|p| {
            p.first() == Some(&remote::net::S2C_NET_OPENED)
                && p.len() >= 3
                && u16::from_le_bytes([p[1], p[2]]) == stream
        });
        let connected = matches!(&opened, Ok(Some(reply))
            if remote::net::parse_net_opened(reply)
                .is_some_and(|(_, status, _, _)| status == remote::net::NET_STATUS_OK));
        if !connected {
            let _ = client.send(&remote::net::msg_net_close(stream, 0));
            return false;
        }
        let request =
            format!("GET {path} HTTP/1.0\r\nHost: {authority}\r\nConnection: close\r\n\r\n");
        let _ = client.send(&remote::net::msg_net_data_c2s(stream, request.as_bytes()));
        let answer = client.recv_matching(|p| {
            p.first() == Some(&remote::net::S2C_NET_DATA)
                && p.len() >= 3
                && u16::from_le_bytes([p[1], p[2]]) == stream
        });
        let _ = client.send(&remote::net::msg_net_close(stream, 0));
        let Ok(Some(data)) = answer else { return false };
        // [op][stream:2][payload...]
        let body = &data[3.min(data.len())..];
        let head = String::from_utf8_lossy(&body[..body.len().min(64)]);
        head.split_whitespace()
            .nth(1)
            .and_then(|code| code.parse::<u16>().ok())
            .is_some_and(|code| code < 500)
    }

    /// Poll `TERM_SINCE` from a cursor taken at create, so the match is text
    /// that arrived *after* the unit started rather than whatever was on screen.
    fn log_matches(&mut self, client: &mut Client, name: &str, pty: u16, needle: &str) -> bool {
        let (from_seq, from_col) = match self.log_cursor.get(name) {
            Some(cursor) => *cursor,
            None => {
                // SINCE_PROBE returns the live cursor and no text.
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
                    return false;
                }
                let Some(reply) = self.term_output(client, nonce) else {
                    return false;
                };
                self.log_cursor
                    .insert(name.to_string(), (reply.next_seq, reply.next_col));
                return false;
            }
        };
        let nonce = self.next_nonce();
        let packet = remote::journal::msg_term_since(nonce, pty, from_seq, from_col, 64 * 1024, 0);
        if client.send(&packet).is_err() {
            return false;
        }
        let Some(reply) = self.term_output(client, nonce) else {
            return false;
        };
        self.log_cursor
            .insert(name.to_string(), (reply.next_seq, reply.next_col));
        // Eviction means the unit outran the poll; the start fails on its
        // timeout rather than hanging on a match that scrolled away.
        reply.text.contains(needle)
    }

    fn term_output(
        &mut self,
        client: &mut Client,
        nonce: u16,
    ) -> Option<remote::journal::OutputReply> {
        let reply = client
            .recv_matching(|p| {
                p.first() == Some(&remote::journal::S2C_TERM_OUTPUT)
                    && p.len() >= 3
                    && u16::from_le_bytes([p[1], p[2]]) == nonce
            })
            .ok()??;
        remote::journal::parse_s2c_term_output(&reply)
    }

    fn random(&self, client: &Client) -> u64 {
        let mut bytes = [0u8; 8];
        let _ = blit_guest::host::random(&mut bytes);
        let _ = client;
        u64::from_le_bytes(bytes)
    }

    fn record(&mut self, client: &mut Client, record: Record, now: u64) {
        let stored = self.journal.push(record, now);
        // The panel and `log -f` read the same bytes.
        let _ = client;
        let _ = stored;
    }
}

// ------------------------------------------------------------------ helpers

fn same_spec(a: &UnitFile, b: &UnitFile) -> bool {
    a.command == b.command
        && a.shell == b.shell
        && a.cwd == b.cwd
        && a.env == b.env
        && a.env_file == b.env_file
        && a.unit_type == b.unit_type
}

fn expand_tilde(path: &str, home: &str) -> String {
    match path.strip_prefix('~') {
        Some(rest) => format!("{home}{rest}"),
        None => path.to_string(),
    }
}

fn absolute(path: &str, cwd: &str) -> String {
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
        ReadyWhen::Http(u) => format!("http:{u}"),
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
