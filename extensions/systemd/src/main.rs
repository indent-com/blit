//! A live view of the systemd system and user unit tables, published on a
//! native channel.
//!
//! ```text
//! blit ext run --persist --restart always systemd extensions/dist/systemd.wasm
//! blit @systemd list --scope user
//! blit @systemd watch ssh
//! ```
//!
//! A Wasm guest reaches nothing but Blit packets, so the host view comes from
//! `systemctl list-units` in a native child process (the process family). The
//! child is poked by D-Bus unit signals through `gdbus monitor` where that
//! tool exists and the manager is broadcasting, and polled otherwise; either
//! way the snapshot the extension diffs is `systemctl`'s own answer, so a
//! missed signal costs latency and never correctness.
//!
//! Channel `blit.systemd.v1` carries one JSON object per message from the
//! extension, and bare text commands (`resync`, `filter PREFIX`,
//! `scopes system,user`, `ping`) from the client.

use std::collections::{BTreeMap, VecDeque};
use std::time::Duration;

use blit_guest::Client;
use blit_guest::command::{exit_payload, result_payload, stdout_payload};
use blit_guest::remote::channel::{
    CHANNEL, CHANNEL_CLOSE_NORMAL, CHANNEL_MAX_PAYLOAD, ChannelMessage, FEATURE_CHANNEL,
    msg_channel_ack, msg_channel_close, msg_channel_data, parse_channel_message,
};
use blit_guest::remote::extension::{
    EXT_EVENT_STDERR, EXT_INFO, EXT_INFO_COMMAND_REGISTERED, ExtensionInfo, ExtensionMessage,
    FEATURE_EXTENSION, msg_extension_command_register, msg_extension_event,
    parse_extension_message,
};
use blit_guest::remote::process::{
    FEATURE_PROCESS, PROCESS_CONTROL_TERMINATE, PROCESS_CWD_DEFAULT, PROCESS_STREAM_STDERR,
    PROCESS_STREAM_STDOUT, ProcessControl, ProcessOutputAck, ProcessSpawnRequest, ProcessStdin,
    S2C_PROCESS_EXIT, S2C_PROCESS_STARTED, S2C_PROCESS_STDERR, S2C_PROCESS_STDOUT,
    msg_process_control, msg_process_output_ack, msg_process_spawn, msg_process_stdin,
    parse_process_exit, parse_process_started, parse_process_stderr, parse_process_stdout,
};

/// One `systemctl` view per scope, refreshed on request and on D-Bus pokes.
///
/// The record grammar on stdout is deliberately line-oriented and short:
/// `B scope` opens a snapshot, every line until `E scope` is one raw
/// `systemctl list-units` row, `R scope` reports that something changed, and
/// `X scope message…` is a diagnostic. Requests arrive on stdin as `s`
/// (snapshot now) and `q` (quit).
///
/// The floor is POSIX `sh` plus coreutils and systemd, because that is what a
/// server started from a systemd unit actually has: NixOS units carry a PATH
/// with neither `awk` nor `bash` on it, and an `awk` that silently is not
/// there drops every unit row while the snapshot still looks like it worked.
/// `gdbus` is genuinely optional — without it the scope polls.
const HELPER: &str = r#"
set -u
scope="${1:-system}"
signals="${2:-signals}"

# execve refuses any single argument or environment string over 128 KiB, and a
# server started from a development shell can carry one (DIRENV_DIFF is the
# usual culprit). Every child then dies with "Argument list too long" --
# including the `env -i` that would have cleaned up -- so the prune has to
# happen here, with builtins, before the first exec.
for assignment in $(export -p 2>/dev/null); do
  case "$assignment" in
    *=*) name=${assignment%%=*} ;;
    *) continue ;;
  esac
  case "$name" in
    PATH|HOME|LANG|LC_*|TZ|TERM|XDG_RUNTIME_DIR|DBUS_SESSION_BUS_ADDRESS) continue ;;
    [!A-Za-z_]*|*[!A-Za-z0-9_]*) continue ;;
  esac
  unset "$name" 2>/dev/null || true
done
if [ "$scope" = user ]; then
  if [ -z "${XDG_RUNTIME_DIR:-}" ] || [ ! -S "${XDG_RUNTIME_DIR}/bus" ]; then
    XDG_RUNTIME_DIR="/run/user/$(id -u)"
  fi
  export XDG_RUNTIME_DIR
  export DBUS_SESSION_BUS_ADDRESS="unix:path=${XDG_RUNTIME_DIR}/bus"
  ctl_scope=--user
  monitor_arg="--address ${DBUS_SESSION_BUS_ADDRESS}"
else
  ctl_scope=--system
  monitor_arg="--system"
fi

snapshot() {
  if ! out=$(systemctl $ctl_scope list-units --all --plain --no-legend --full --no-pager 2>&1); then
    printf 'X %s systemctl failed: %s\n' "$scope" "$(printf '%s' "$out" | head -1)"
    return
  fi
  # Rows go out verbatim; the reader splits fields, which costs no tool here.
  printf 'B %s\n' "$scope"
  printf '%s\n' "$out"
  printf 'E %s\n' "$scope"
}

if [ "$signals" = signals ] && command -v gdbus >/dev/null 2>&1; then
  printf 'X %s source gdbus\n' "$scope"
  gdbus monitor $monitor_arg --dest org.freedesktop.systemd1 2>/dev/null |
    while IFS= read -r signal; do
      case "$signal" in
        *ActiveState*|*SubState*|*UnitNew*|*UnitRemoved*|*JobNew*|*JobRemoved*|*Reloading*)
          printf 'R %s\n' "$scope" ;;
      esac
    done &
else
  printf 'X %s source poll\n' "$scope"
fi

snapshot
# `q` is not sent by this extension today; it is here so the helper can be
# driven by hand. The backgrounded `gdbus monitor` is deliberately not reaped
# on that path: `$!` names the tail of the pipeline rather than gdbus, and the
# server kills the whole process group when it terminates the helper, which is
# the only way this ever actually exits.
while IFS= read -r line; do
  case "$line" in
    s*) snapshot ;;
    q*) break ;;
  esac
done
"#;

const CHANNEL_NAME: &str = "blit.systemd.v1";
const SCOPES: [&str; 2] = ["system", "user"];
/// One channel message stays well under the 1 MiB cap and the peer's window.
const SNAPSHOT_CHUNK: usize = 64 * 1024;
const RESPAWN_BACKOFF: Duration = Duration::from_secs(5);
/// A scope with no signal source only learns about changes by asking.
const UNPOKED_INTERVAL: Duration = Duration::from_secs(1);

const DESCRIPTOR: &str = r#"{
  "protocol":"blit.cli.v1",
  "summary":"Live systemd system and user unit state",
  "commands":[
    {"path":[],"summary":"Show watcher health","usage":"status"},
    {"path":["status"],"summary":"Show watcher health","usage":"status"},
    {"path":["list"],"summary":"List units","usage":"list [--scope system|user] [--state STATE] [PREFIX]",
     "options":[{"name":"--scope","argument":true},{"name":"--state","argument":true}]},
    {"path":["get"],"summary":"Show one unit","usage":"get UNIT [--scope system|user]",
     "options":[{"name":"--scope","argument":true}]},
    {"path":["watch"],"summary":"Stream unit changes until interrupted","usage":"watch [--scope system|user] [PREFIX]",
     "options":[{"name":"--scope","argument":true}]},
    {"path":["logs"],"summary":"Read the journal, or follow it live",
     "usage":"logs [--scope system|user|all] [-u UNIT] [-n LINES] [--cursor C] [--priority P] [--grep RE] [-f]",
     "options":[{"name":"--scope","argument":true},{"name":"--unit","argument":true},
                {"name":"-u","argument":true},{"name":"--lines","argument":true},
                {"name":"-n","argument":true},{"name":"--cursor","argument":true},
                {"name":"--priority","argument":true},{"name":"--grep","argument":true},
                {"name":"--follow"},{"name":"-f"}]}
  ]
}"#;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct Unit {
    load: String,
    active: String,
    sub: String,
    description: String,
}

/// A `systemctl` view of one scope and the child process feeding it.
struct Scope {
    name: &'static str,
    signals: bool,
    process_id: u32,
    units: BTreeMap<String, Unit>,
    building: Option<BTreeMap<String, Unit>>,
    source: String,
    stdout_acked: u64,
    stderr_acked: u64,
    stdin_offset: u64,
    line: Vec<u8>,
    running: bool,
    ready: bool,
    snapshots: u64,
    detail: String,
    poke_deadline: Option<blit_guest::MonotonicInstant>,
    poll_deadline: Option<blit_guest::MonotonicInstant>,
    respawn_deadline: Option<blit_guest::MonotonicInstant>,
}

impl Scope {
    fn new(name: &'static str, signals: bool, process_id: u32) -> Self {
        Self {
            name,
            signals,
            process_id,
            units: BTreeMap::new(),
            building: None,
            source: String::from("unknown"),
            stdout_acked: 0,
            stderr_acked: 0,
            stdin_offset: 0,
            line: Vec::new(),
            running: false,
            ready: false,
            snapshots: 0,
            detail: String::new(),
            poke_deadline: None,
            poll_deadline: None,
            respawn_deadline: None,
        }
    }
}

/// How much a connection may have waiting for credit before it is hopeless.
///
/// Generous enough to hold a full unit table plus a journal page, so a peer
/// that is merely slow is waited for; small enough that a peer that has
/// stopped reading altogether is closed rather than grown into.
const MAX_QUEUED_BYTES: usize = 8 * 1024 * 1024;

/// One accepted channel: a data subscriber or a `blit.cli.v1` invocation.
struct Conn {
    id: u32,
    window: u64,
    sent: u64,
    acked: u64,
    received: u64,
    closed: bool,
    role: Role,
    /// Messages composed but not yet sent, oldest first.
    ///
    /// Snapshots and journal pages are multi-message answers whose total size
    /// routinely exceeds one window, and the peer's ACKs only arrive between
    /// packets. Dropping the remainder when credit ran out left the reader
    /// waiting forever for a `last` that was never coming, so what does not
    /// fit waits here and the ACK that frees credit drains it.
    queue: VecDeque<Vec<u8>>,
    queued_bytes: usize,
    /// An invocation has answered and the channel closes once `queue` empties.
    close_when_drained: bool,
}

enum Role {
    Data(Filter),
    /// A command invocation before its INVOKE arrives.
    Command,
    /// A `watch` invocation streaming changes until it is cancelled.
    Watch(Filter),
}

#[derive(Clone, Debug, Default)]
struct Filter {
    prefix: String,
    scopes: Vec<String>,
}

impl Filter {
    fn matches(&self, scope: &str, unit: &str) -> bool {
        (self.scopes.is_empty() || self.scopes.iter().any(|wanted| wanted == scope))
            && unit.starts_with(&self.prefix)
    }
}

impl Conn {
    fn credit(&self) -> u64 {
        self.acked
            .saturating_add(self.window)
            .saturating_sub(self.sent)
    }
}

struct Change {
    scope: &'static str,
    added: Vec<(String, Unit)>,
    changed: Vec<(String, Unit, Unit)>,
    removed: Vec<(String, Unit)>,
}

impl Change {
    fn is_empty(&self) -> bool {
        self.added.is_empty() && self.changed.is_empty() && self.removed.is_empty()
    }
}

/// Fields worth carrying out of `journalctl -o json`. Asking for a short list
/// keeps the parse small and the payload honest about what a log line is.
const JOURNAL_FIELDS: &str =
    "__CURSOR,__REALTIME_TIMESTAMP,PRIORITY,_SYSTEMD_UNIT,_COMM,_PID,MESSAGE";
/// A page nobody can read is a page nobody should have to receive.
///
/// This bounds one page, not the history: scrollback is unbounded because each
/// page is addressed by the cursor the last one ended on, so a reader walks as
/// far back as the journal goes one page at a time.
const JOURNAL_MAX_LIMIT: u64 = 1000;
const JOURNAL_DEFAULT_LIMIT: u64 = 200;
/// How long a follow batches entries before sending them.
///
/// Long enough that a burst becomes one message, short enough to read as live.
const FOLLOW_FLUSH: Duration = Duration::from_millis(200);
/// Entries a follow will hold before flushing regardless of the timer.
const FOLLOW_MAX_BATCH: usize = 256;
/// Filters go into an argv, so they are bounded before they get there.
const JOURNAL_MAX_ARG: usize = 512;
/// The longest single line of `journalctl` output worth reassembling.
///
/// A boot list is one array on one line, so this is not as tight as the
/// per-entry limit would be.
const MAX_JOB_LINE: usize = 4 * 1024 * 1024;

/// One `journalctl` run on behalf of one channel.
///
/// A `Logs` or `Boots` query is request/response: the answer is whatever the
/// journal held at the moment it was asked, addressed by cursor so the next
/// page continues exactly where this one stopped. A `Follow` is the opposite —
/// a `journalctl --follow` that lives until it is cancelled and emits entries
/// as the journal grows.
struct Job {
    channel_id: u32,
    request_id: String,
    process_id: u32,
    kind: JobKind,
    /// Newest-first on the wire; the reader flips it back for display.
    reverse: bool,
    line: Vec<u8>,
    entries: Vec<String>,
    detail: String,
    limit: u64,
    /// Entries absorbed, including any a page dropped for want of a cursor.
    ///
    /// `more` is whether the journal had at least as many entries as the page
    /// asked for, which is a question about what `journalctl` produced and not
    /// about what survived the parse. Counting the survivors instead ended
    /// scrollback early whenever a page contained an unaddressable entry.
    absorbed: u64,
    /// A follow's entries since the last flush, and when they go out.
    ///
    /// `journalctl --follow` emits a line per entry and a busy unit emits
    /// bursts of them; one channel message each would be mostly framing.
    flush_deadline: Option<blit_guest::MonotonicInstant>,
}

/// One journal query, as its caller describes it.
struct JobSpec<'a> {
    channel_id: u32,
    request_id: &'a str,
    kind: JobKind,
    reverse: bool,
    limit: u64,
    argv: &'a [String],
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum JobKind {
    Logs,
    Boots,
    /// A live `journalctl --follow`, streaming until cancelled.
    Follow,
}

struct Watcher {
    scopes: Vec<Scope>,
    conns: Vec<Conn>,
    jobs: Vec<Job>,
    data_listener: u32,
    command_listener: u32,
    next_process_id: u32,
    poll_interval: Duration,
    idle_interval: Duration,
    debounce: Duration,
    /// Passed to journalctl as --directory when the operator named one.
    journal_dir: String,
}

fn main() {}

blit_guest::entry!(extension);

/// Say something a human can read with `blit ext run` or `blit ext attach`.
///
/// The SDK turns a failed entry into a bare exit code and drops the message,
/// so anything worth knowing has to be said before returning: an extension
/// that just stops is indistinguishable from one that was never installed.
fn say(client: &mut Client, message: &str) {
    if let Some(packet) = msg_extension_event(EXT_EVENT_STDERR, format!("{message}\n").as_bytes()) {
        let _ = client.send(&packet);
    }
}

fn extension(mut client: Client) -> Result<(), String> {
    match run(&mut client) {
        Ok(()) => Ok(()),
        Err(error) => {
            say(&mut client, &format!("fatal: {error}"));
            Err(format!("systemd watcher: {error}"))
        }
    }
}

fn run(client: &mut Client) -> Result<(), String> {
    let features = client.context().hello.features;
    for (bit, name) in [(FEATURE_PROCESS, "process"), (FEATURE_CHANNEL, "channel")] {
        if features & bit == 0 {
            return Err(format!("server does not advertise the {name} family"));
        }
    }

    let mut wanted: Vec<&'static str> = SCOPES.to_vec();
    let mut poll_interval = Duration::from_secs(5);
    let mut idle_interval = Duration::from_secs(30);
    let mut debounce = Duration::from_millis(250);
    let mut signals = true;
    let mut journal_dir = String::new();
    let args = client.context().args.clone();
    let mut index = 0;
    while index < args.len() {
        let value = args.get(index + 1).cloned();
        match args[index].as_str() {
            "--scopes" => {
                let value = value.ok_or("--scopes needs a value")?;
                wanted = SCOPES
                    .into_iter()
                    .filter(|scope| value.split(',').any(|name| name.trim() == *scope))
                    .collect();
                if wanted.is_empty() {
                    return Err(String::from("--scopes selected no known scope"));
                }
                index += 2;
            }
            "--interval-ms" => {
                poll_interval = Duration::from_millis(parse_millis(value.as_deref())?);
                index += 2;
            }
            "--idle-interval-ms" => {
                idle_interval = Duration::from_millis(parse_millis(value.as_deref())?);
                index += 2;
            }
            "--debounce-ms" => {
                debounce = Duration::from_millis(parse_millis(value.as_deref())?);
                index += 2;
            }
            "--no-signals" => {
                signals = false;
                index += 1;
            }
            // Whose journal to read, when it is not this machine's own: an
            // archived copy, a mounted host, a directory the server user can
            // actually open. The operator picks it, never a channel client.
            "--journal-dir" => {
                journal_dir = value.ok_or("--journal-dir needs a path")?;
                index += 2;
            }
            other => return Err(format!("unknown argument {other:?}")),
        }
    }

    // Channel names are process-global, so a second copy of this extension is
    // not a second watcher -- it is a crash loop. Name the winner.
    let data_listener = client
        .listen_channel(CHANNEL_NAME, b"")
        .map_err(|error| {
            format!("cannot publish {CHANNEL_NAME} ({error}); another instance already serves it")
        })?
        .id();

    // Command registration needs a named, persistent attempt; without one the
    // channel still works and only `@systemd` is missing.
    let named = client.context().name.is_some() && client.context().persistent;
    let identity = (client.context().extension_id, client.context().attempt);
    let command_listener = if features & FEATURE_EXTENSION != 0 && named {
        let name = format!("blit.cli.systemd.{:016x}.{}", identity.0, identity.1);
        let listener = client
            .listen_channel(&name, b"")
            .map_err(|error| format!("cannot publish {name}: {error}"))?;
        register_commands(client, listener.id())?;
        say(client, "serving blit.systemd.v1 and @systemd");
        listener.id()
    } else {
        // The commonest way to be confused by this extension: no @systemd, no
        // `ext commands` entry, and nothing anywhere saying why.
        say(
            client,
            "serving blit.systemd.v1; @systemd needs `ext run --persist systemd` \
             on a server started with --allow-persistent-extensions",
        );
        0
    };

    let mut watcher = Watcher {
        scopes: Vec::new(),
        conns: Vec::new(),
        jobs: Vec::new(),
        data_listener,
        command_listener,
        next_process_id: 1,
        poll_interval,
        idle_interval,
        debounce,
        journal_dir,
    };
    for name in wanted {
        let process_id = watcher.next_process_id;
        watcher.next_process_id += 1;
        watcher.scopes.push(Scope::new(name, signals, process_id));
        spawn_helper(client, watcher.scopes.last_mut().expect("just pushed"))?;
    }

    loop {
        let deadline = watcher.next_deadline(client);
        match client
            .wait_until(deadline)
            .map_err(|error| format!("wait: {error}"))?
        {
            blit_guest::WaitOutcome::Closed => return Ok(()),
            blit_guest::WaitOutcome::Deadline => watcher.on_deadline(client)?,
            blit_guest::WaitOutcome::Packet => {
                match client.recv().map_err(|error| format!("recv: {error}"))? {
                    Some(packet) => watcher.on_packet(client, &packet)?,
                    None => return Ok(()),
                }
            }
        }
    }
}

fn parse_millis(value: Option<&str>) -> Result<u64, String> {
    value
        .ok_or_else(|| String::from("interval needs a value"))?
        .parse::<u64>()
        .map_err(|_| String::from("interval must be whole milliseconds"))
        .map(|millis| millis.clamp(100, 3_600_000))
}

fn register_commands(client: &mut Client, listener_id: u32) -> Result<(), String> {
    let nonce = 1;
    let request = msg_extension_command_register(nonce, listener_id, DESCRIPTOR)
        .ok_or("invalid command descriptor")?;
    client
        .send(&request)
        .map_err(|error| format!("command register: {error}"))?;
    let packet = client
        .recv_matching(|packet| {
            packet.first() == Some(&EXT_INFO) && packet.get(1) == Some(&EXT_INFO_COMMAND_REGISTERED)
        })
        .map_err(|error| format!("command register reply: {error}"))?
        .ok_or("endpoint closed during command registration")?;
    match parse_extension_message(&packet) {
        Ok(Some(ExtensionMessage::Info(ExtensionInfo::CommandRegistered(registered))))
            if registered.status == 0 =>
        {
            Ok(())
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

fn spawn_helper(client: &mut Client, scope: &mut Scope) -> Result<(), String> {
    let request = ProcessSpawnRequest {
        nonce: scope.process_id as u16,
        process_id: scope.process_id,
        flags: 0,
        cwd_kind: PROCESS_CWD_DEFAULT,
        src_pty_id: 0,
        cwd: b"",
        argv: vec![
            b"/bin/sh",
            b"-c",
            HELPER.as_bytes(),
            b"blit-systemd",
            scope.name.as_bytes(),
            if scope.signals {
                b"signals"
            } else {
                b"nosignals"
            },
        ],
        env: Vec::new(),
    };
    let packet = msg_process_spawn(&request).map_err(|error| format!("spawn encode: {error:?}"))?;
    client
        .send(&packet)
        .map_err(|error| format!("spawn {}: {error}", scope.name))?;
    scope.stdout_acked = 0;
    scope.stderr_acked = 0;
    scope.stdin_offset = 0;
    scope.line.clear();
    scope.building = None;
    scope.respawn_deadline = None;
    Ok(())
}

impl Watcher {
    fn scope_index(&self, process_id: u32) -> Option<usize> {
        self.scopes
            .iter()
            .position(|scope| scope.process_id == process_id)
    }

    fn subscribers(&self) -> usize {
        self.conns
            .iter()
            .filter(|conn| matches!(conn.role, Role::Data(_) | Role::Watch(_)))
            .count()
    }

    fn next_deadline(&self, client: &Client) -> blit_guest::MonotonicInstant {
        let mut earliest = blit_guest::MonotonicInstant::MAX;
        for scope in &self.scopes {
            for deadline in [
                scope.poke_deadline,
                scope.poll_deadline,
                scope.respawn_deadline,
            ]
            .into_iter()
            .flatten()
            {
                if deadline < earliest {
                    earliest = deadline;
                }
            }
        }
        // A follow holding a part-full batch is waiting on its flush; without
        // it here the loop parks until the next unit change and the tail
        // stalls behind an unrelated event.
        for job in &self.jobs {
            if let Some(deadline) = job.flush_deadline
                && deadline < earliest
            {
                earliest = deadline;
            }
        }
        let _ = client;
        earliest
    }

    fn on_deadline(&mut self, client: &mut Client) -> Result<(), String> {
        let now = client.monotonic_now();
        for index in 0..self.scopes.len() {
            let (poke, poll, respawn) = {
                let scope = &self.scopes[index];
                (
                    scope.poke_deadline.is_some_and(|deadline| deadline <= now),
                    scope.poll_deadline.is_some_and(|deadline| deadline <= now),
                    scope
                        .respawn_deadline
                        .is_some_and(|deadline| deadline <= now),
                )
            };
            if respawn {
                let scope = &mut self.scopes[index];
                scope.respawn_deadline = None;
                let process_id = self.next_process_id;
                self.next_process_id += 1;
                scope.process_id = process_id;
                let scope = &mut self.scopes[index];
                spawn_helper(client, scope)?;
                continue;
            }
            if poke || poll {
                self.scopes[index].poke_deadline = None;
                self.request_snapshot(client, index)?;
            }
        }
        for job in 0..self.jobs.len() {
            if self.jobs[job]
                .flush_deadline
                .is_some_and(|deadline| deadline <= now)
            {
                self.flush_follow(client, job)?;
            }
        }
        Ok(())
    }

    /// How often a scope is re-read when nobody pokes it.
    ///
    /// A scope whose helper found no signal source is the only one that must
    /// poll to stay live, so it keeps its fast pace whether or not the D-Bus
    /// accelerator is available elsewhere.
    fn interval_for(&self, index: usize) -> Duration {
        if self.subscribers() == 0 {
            return self.idle_interval;
        }
        if self.scopes[index].source == "poll" {
            return UNPOKED_INTERVAL.min(self.poll_interval);
        }
        self.poll_interval
    }

    /// Ask one helper for a fresh snapshot and re-arm its periodic refresh.
    fn request_snapshot(&mut self, client: &mut Client, index: usize) -> Result<(), String> {
        let interval = self.interval_for(index);
        let now = client.monotonic_now();
        let scope = &mut self.scopes[index];
        scope.poll_deadline = Some(now + interval);
        if !scope.running {
            return Ok(());
        }
        let packet = msg_process_stdin(ProcessStdin {
            process_id: scope.process_id,
            offset: scope.stdin_offset,
            data: b"s\n",
        })
        .map_err(|error| format!("stdin encode: {error:?}"))?;
        scope.stdin_offset += 2;
        client
            .send(&packet)
            .map_err(|error| format!("stdin {}: {error}", scope.name))
    }

    fn on_packet(&mut self, client: &mut Client, packet: &[u8]) -> Result<(), String> {
        match packet.first().copied() {
            Some(S2C_PROCESS_STARTED) => {
                let started = parse_process_started(packet)
                    .map_err(|error| format!("started parse: {error:?}"))?;
                if let Some(index) = self.scope_index(started.process_id) {
                    let now = client.monotonic_now();
                    let scope = &mut self.scopes[index];
                    if started.status == 0 {
                        scope.running = true;
                        scope.detail = String::new();
                        scope.poll_deadline = Some(now + self.idle_interval);
                    } else {
                        scope.running = false;
                        scope.detail = format!(
                            "spawn refused with status {}: {}",
                            started.status, started.detail
                        );
                        scope.respawn_deadline = Some(now + RESPAWN_BACKOFF);
                    }
                }
                Ok(())
            }
            Some(S2C_PROCESS_STDOUT) => {
                let output = parse_process_stdout(packet)
                    .map_err(|error| format!("stdout parse: {error:?}"))?;
                let through = output.offset + output.data.len() as u64;
                let ack = msg_process_output_ack(ProcessOutputAck {
                    process_id: output.process_id,
                    stream: PROCESS_STREAM_STDOUT,
                    bytes: through,
                })
                .map_err(|error| format!("ack encode: {error:?}"))?;
                if let Some(job) = self.job_index(output.process_id) {
                    client
                        .send(&ack)
                        .map_err(|error| format!("journal ack: {error}"))?;
                    self.jobs[job].line.extend_from_slice(output.data);
                    self.drain_job_lines(job);
                    if self.jobs[job].kind == JobKind::Follow {
                        // A big enough batch goes now; anything smaller waits
                        // for the timer so a trickle still arrives promptly.
                        if self.jobs[job].entries.len() >= FOLLOW_MAX_BATCH {
                            self.flush_follow(client, job)?;
                        } else if self.jobs[job].flush_deadline.is_none() {
                            self.jobs[job].flush_deadline =
                                Some(client.monotonic_now() + FOLLOW_FLUSH);
                        }
                    }
                    return Ok(());
                }
                let Some(index) = self.scope_index(output.process_id) else {
                    return Ok(());
                };
                let through = output.offset + output.data.len() as u64;
                self.scopes[index].line.extend_from_slice(output.data);
                self.scopes[index].stdout_acked = through;
                let ack = msg_process_output_ack(ProcessOutputAck {
                    process_id: output.process_id,
                    stream: PROCESS_STREAM_STDOUT,
                    bytes: through,
                })
                .map_err(|error| format!("ack encode: {error:?}"))?;
                client
                    .send(&ack)
                    .map_err(|error| format!("stdout ack: {error}"))?;
                self.drain_lines(client, index)
            }
            Some(S2C_PROCESS_STDERR) => {
                let output = parse_process_stderr(packet)
                    .map_err(|error| format!("stderr parse: {error:?}"))?;
                if let Some(job) = self.job_index(output.process_id) {
                    let text = String::from_utf8_lossy(output.data).trim().to_string();
                    if !text.is_empty() {
                        self.jobs[job].detail = text;
                    }
                    let ack = msg_process_output_ack(ProcessOutputAck {
                        process_id: output.process_id,
                        stream: PROCESS_STREAM_STDERR,
                        bytes: output.offset + output.data.len() as u64,
                    })
                    .map_err(|error| format!("ack encode: {error:?}"))?;
                    client
                        .send(&ack)
                        .map_err(|error| format!("journal stderr ack: {error}"))?;
                    return Ok(());
                }
                if let Some(index) = self.scope_index(output.process_id) {
                    let scope = &mut self.scopes[index];
                    scope.detail = String::from_utf8_lossy(output.data).trim().to_string();
                    scope.stderr_acked = output.offset + output.data.len() as u64;
                    let ack = msg_process_output_ack(ProcessOutputAck {
                        process_id: output.process_id,
                        stream: PROCESS_STREAM_STDERR,
                        bytes: scope.stderr_acked,
                    })
                    .map_err(|error| format!("ack encode: {error:?}"))?;
                    client
                        .send(&ack)
                        .map_err(|error| format!("stderr ack: {error}"))?;
                }
                Ok(())
            }
            Some(S2C_PROCESS_EXIT) => {
                let exit =
                    parse_process_exit(packet).map_err(|error| format!("exit parse: {error:?}"))?;
                if let Some(job) = self.job_index(exit.process_id) {
                    // A trailing line without its newline is still an entry.
                    self.drain_job_lines(job);
                    let leftover = std::mem::take(&mut self.jobs[job].line);
                    if !leftover.is_empty() {
                        let line = String::from_utf8_lossy(&leftover).to_string();
                        self.absorb_job_line(job, &line);
                    }
                    return self.finish_job(client, job);
                }
                if let Some(index) = self.scope_index(exit.process_id) {
                    let now = client.monotonic_now();
                    let scope = &mut self.scopes[index];
                    scope.running = false;
                    // The unit map survives the helper, so the snapshot after
                    // a respawn still reports what changed during the gap.
                    scope.detail = format!(
                        "helper exited (reason {} code {}) {}",
                        exit.reason, exit.code, exit.detail
                    );
                    scope.respawn_deadline = Some(now + RESPAWN_BACKOFF);
                }
                Ok(())
            }
            Some(opcode) if opcode == CHANNEL => self.on_channel(client, packet),
            _ => Ok(()),
        }
    }

    /// Turn complete helper lines into snapshots, pokes, and diagnostics.
    fn drain_lines(&mut self, client: &mut Client, index: usize) -> Result<(), String> {
        loop {
            let Some(end) = self.scopes[index]
                .line
                .iter()
                .position(|byte| *byte == b'\n')
            else {
                // A helper line is short; an unterminated megabyte is noise.
                if self.scopes[index].line.len() > 1024 * 1024 {
                    self.scopes[index].line.clear();
                }
                return Ok(());
            };
            let raw: Vec<u8> = self.scopes[index].line.drain(..=end).collect();
            let line = String::from_utf8_lossy(&raw[..raw.len() - 1])
                .trim()
                .to_string();
            if line.is_empty() {
                continue;
            }
            // Control lines are one letter and a scope; everything else inside
            // a snapshot is a `systemctl` row, which no unit name can imitate
            // because a row carries at least four fields.
            let (tag, rest) = match line.split_once(' ') {
                Some((tag, rest)) if matches!(tag, "B" | "E" | "R" | "X") => {
                    (tag, rest.trim_start())
                }
                _ => ("", line.as_str()),
            };
            match tag {
                "B" => self.scopes[index].building = Some(BTreeMap::new()),
                "" => {
                    if let Some(units) = self.scopes[index].building.as_mut()
                        && let Some(unit) = parse_unit_row(rest)
                    {
                        units.insert(unit.0, unit.1);
                    }
                }
                "E" => {
                    let Some(units) = self.scopes[index].building.take() else {
                        continue;
                    };
                    let first = !self.scopes[index].ready;
                    let change = diff(self.scopes[index].name, &self.scopes[index].units, &units);
                    self.scopes[index].units = units;
                    self.scopes[index].ready = true;
                    self.scopes[index].snapshots += 1;
                    if !first && !change.is_empty() {
                        self.publish(client, &change)?;
                    }
                }
                "R" => {
                    let now = client.monotonic_now();
                    let debounce = self.debounce;
                    let scope = &mut self.scopes[index];
                    if scope.poke_deadline.is_none() {
                        scope.poke_deadline = Some(now + debounce);
                    }
                }
                "X" => {
                    let scope = &mut self.scopes[index];
                    let message = rest
                        .strip_prefix(scope.name)
                        .unwrap_or(rest)
                        .trim()
                        .to_string();
                    match message.strip_prefix("source ") {
                        Some(source) => scope.source = String::from(source),
                        None => scope.detail = message,
                    }
                }
                _ => {}
            }
        }
    }

    // --- channels -------------------------------------------------------

    fn conn_index(&self, id: u32) -> Option<usize> {
        self.conns.iter().position(|conn| conn.id == id)
    }

    fn on_channel(&mut self, client: &mut Client, packet: &[u8]) -> Result<(), String> {
        let message = match parse_channel_message(packet) {
            Ok(Some(message)) => message,
            _ => return Ok(()),
        };
        match message {
            ChannelMessage::Accepted {
                channel_id,
                listener_id,
                window,
                ..
            } => {
                let role = if listener_id == self.data_listener {
                    Role::Data(Filter::default())
                } else if listener_id == self.command_listener {
                    Role::Command
                } else {
                    return Ok(());
                };
                let greet = matches!(role, Role::Data(_));
                self.conns.push(Conn {
                    id: channel_id,
                    window,
                    sent: 0,
                    acked: 0,
                    received: 0,
                    closed: false,
                    role,
                    queue: VecDeque::new(),
                    queued_bytes: 0,
                    close_when_drained: false,
                });
                if greet {
                    // `hello` and nothing else: a client that connected only
                    // to find out whether a watcher is here (the UI decides
                    // whether to offer the panel at all) should not be handed
                    // a hundred kilobytes of unit table. Snapshots follow the
                    // first `resync`, `filter`, or `scopes` request.
                    let index = self.conns.len() - 1;
                    let hello = self.hello_json(client);
                    self.send_json(client, index, &hello)?;
                    // The first subscriber pulls every scope off its idle pace.
                    for scope_index in 0..self.scopes.len() {
                        let now = client.monotonic_now();
                        let interval = self.interval_for(scope_index);
                        self.scopes[scope_index].poll_deadline = Some(now + interval);
                    }
                }
                Ok(())
            }
            ChannelMessage::Ack { channel_id, bytes } => {
                if let Some(index) = self.conn_index(channel_id) {
                    self.conns[index].acked = self.conns[index].acked.max(bytes);
                    // Credit just came back, so anything the window would not
                    // hold when it was composed can go now.
                    self.drain_queue(client, index)?;
                }
                Ok(())
            }
            ChannelMessage::Closed { channel_id, .. } => {
                if channel_id == self.data_listener || channel_id == self.command_listener {
                    return Err(String::from("a listener closed"));
                }
                // The reader is gone, so nothing it asked for has anywhere to
                // go. Without this a closed tab left its `journalctl` running
                // -- forever, for a follow -- with its output arriving for a
                // channel that no longer exists.
                self.cancel_jobs(client, channel_id, None, None);
                self.conns.retain(|conn| conn.id != channel_id);
                Ok(())
            }
            ChannelMessage::Data {
                channel_id,
                payload,
            } => {
                let Some(index) = self.conn_index(channel_id) else {
                    return Ok(());
                };
                let through = self.conns[index].received + payload.len() as u64;
                self.conns[index].received = through;
                client
                    .send(&msg_channel_ack(channel_id, through))
                    .map_err(|error| format!("channel ack: {error}"))?;
                let payload = payload.to_vec();
                match self.conns[index].role {
                    Role::Data(_) => self.on_data_request(client, index, &payload),
                    Role::Command | Role::Watch(_) => {
                        self.on_command_payload(client, index, &payload)
                    }
                }
            }
            ChannelMessage::Opened { .. } => Ok(()),
        }
    }

    /// Hand one message to a connection, sending it now or queueing it.
    ///
    /// Reports `false` only when the message will never be delivered — the
    /// connection is closed, the payload is unsendable, or the peer has
    /// stopped reading for long enough to hit [`MAX_QUEUED_BYTES`]. A `true`
    /// means the reader will see it, which is what lets a chunked answer
    /// commit to finishing.
    fn send_bytes(
        &mut self,
        client: &mut Client,
        index: usize,
        payload: &[u8],
    ) -> Result<bool, String> {
        let conn = &mut self.conns[index];
        if conn.closed || payload.len() > CHANNEL_MAX_PAYLOAD {
            return Ok(false);
        }
        // Order is the whole point of a queue: once something is waiting,
        // everything after it waits too, however much credit turns up.
        if !conn.queue.is_empty() || (payload.len() as u64) > conn.credit() {
            if conn.queued_bytes.saturating_add(payload.len()) > MAX_QUEUED_BYTES {
                // A peer that will not read does not get to grow our memory,
                // and is told so rather than left with a half-sent answer.
                conn.closed = true;
                let close =
                    msg_channel_close(conn.id, CHANNEL_CLOSE_NORMAL).ok_or("invalid close")?;
                client
                    .send(&close)
                    .map_err(|error| format!("channel close: {error}"))?;
                return Ok(false);
            }
            conn.queued_bytes += payload.len();
            conn.queue.push_back(payload.to_vec());
            return Ok(true);
        }
        let packet = msg_channel_data(conn.id, payload).ok_or("invalid channel payload")?;
        conn.sent += payload.len() as u64;
        client
            .send(&packet)
            .map_err(|error| format!("channel data: {error}"))?;
        Ok(true)
    }

    /// Send whatever the returning credit now covers, oldest first.
    fn drain_queue(&mut self, client: &mut Client, index: usize) -> Result<(), String> {
        loop {
            let conn = &mut self.conns[index];
            if conn.closed {
                conn.queue.clear();
                conn.queued_bytes = 0;
                return Ok(());
            }
            let Some(next) = conn.queue.front() else {
                return Ok(());
            };
            if (next.len() as u64) > conn.credit() {
                return Ok(());
            }
            let payload = conn.queue.pop_front().expect("just inspected");
            conn.queued_bytes = conn.queued_bytes.saturating_sub(payload.len());
            let packet = msg_channel_data(conn.id, &payload).ok_or("invalid channel payload")?;
            conn.sent += payload.len() as u64;
            client
                .send(&packet)
                .map_err(|error| format!("channel data: {error}"))?;
            if conn.queue.is_empty() {
                // The last of a finished invocation's output just went out.
                return self.close_if_drained(client, index);
            }
        }
    }

    fn send_json(&mut self, client: &mut Client, index: usize, json: &str) -> Result<bool, String> {
        self.send_bytes(client, index, json.as_bytes())
    }

    /// Send one JSON message in whatever framing this connection speaks.
    ///
    /// A data subscriber reads raw JSON off the channel; an invocation reads a
    /// `blit.cli.v1` stream, where the same object has to arrive as a STDOUT
    /// payload or the CLI prints nothing at all.
    fn deliver(&mut self, client: &mut Client, index: usize, json: &str) -> Result<bool, String> {
        match self.conns[index].role {
            Role::Data(_) => self.send_json(client, index, json),
            Role::Command | Role::Watch(_) => {
                let payload =
                    stdout_payload(format!("{json}\n").as_bytes()).map_err(|_| "logs payload")?;
                self.send_bytes(client, index, &payload)
            }
        }
    }

    fn hello_json(&self, client: &Client) -> String {
        let mut json = format!(
            "{{\"type\":\"hello\",\"protocol\":\"blit.systemd.v1\",\"ts\":{},\"scopes\":[",
            unix_millis(client)
        );
        for (position, scope) in self.scopes.iter().enumerate() {
            if position > 0 {
                json.push(',');
            }
            json.push_str("{\"scope\":");
            push_json_string(&mut json, scope.name);
            json.push_str(",\"source\":");
            push_json_string(&mut json, &scope.source);
            json.push_str(",\"units\":");
            json.push_str(&scope.units.len().to_string());
            json.push('}');
        }
        json.push_str("]}");
        json
    }

    /// Send every matching unit as chunked `snapshot` messages.
    fn send_snapshots(&mut self, client: &mut Client, index: usize) -> Result<(), String> {
        for scope_index in 0..self.scopes.len() {
            let (name, units): (&'static str, Vec<(String, Unit)>) = {
                let scope = &self.scopes[scope_index];
                let filter = match &self.conns[index].role {
                    Role::Data(filter) | Role::Watch(filter) => filter.clone(),
                    Role::Command => Filter::default(),
                };
                (
                    scope.name,
                    scope
                        .units
                        .iter()
                        .filter(|(unit, _)| filter.matches(scope.name, unit))
                        .map(|(unit, state)| (unit.clone(), state.clone()))
                        .collect(),
                )
            };

            let mut chunk = 0;
            let mut position = 0;
            loop {
                let mut json = format!(
                    "{{\"type\":\"snapshot\",\"scope\":\"{name}\",\"ts\":{},\"chunk\":{chunk},\"units\":[",
                    unix_millis(client)
                );
                let mut count = 0;
                while position < units.len() {
                    let mut entry = String::new();
                    if count > 0 {
                        entry.push(',');
                    }
                    push_unit_json(&mut entry, &units[position].0, &units[position].1);
                    if json.len() + entry.len() > SNAPSHOT_CHUNK && count > 0 {
                        break;
                    }
                    json.push_str(&entry);
                    position += 1;
                    count += 1;
                }
                let last = position >= units.len();
                json.push_str(&format!("],\"last\":{last}}}"));
                if !self.send_json(client, index, &json)? {
                    return Ok(());
                }
                chunk += 1;
                if last {
                    break;
                }
            }
        }
        Ok(())
    }

    /// Fan one scope's diff out to every data subscriber and `watch` client.
    fn publish(&mut self, client: &mut Client, change: &Change) -> Result<(), String> {
        let ts = unix_millis(client);
        for index in 0..self.conns.len() {
            let filter = match &self.conns[index].role {
                Role::Data(filter) | Role::Watch(filter) => filter.clone(),
                Role::Command => continue,
            };
            let Some(json) = change_json(change, &filter, ts) else {
                continue;
            };
            let watching = matches!(self.conns[index].role, Role::Watch(_));
            if watching {
                let payload =
                    stdout_payload(format!("{json}\n").as_bytes()).map_err(|_| "watch payload")?;
                self.send_bytes(client, index, &payload)?;
            } else {
                self.send_json(client, index, &json)?;
            }
        }
        Ok(())
    }

    fn on_data_request(
        &mut self,
        client: &mut Client,
        index: usize,
        payload: &[u8],
    ) -> Result<(), String> {
        let request = String::from_utf8_lossy(payload).trim().to_string();
        // Filters and cursors do not fit in a bare verb line, so a request may
        // also be a flat JSON object. The text verbs stay: they are what a
        // person types into a channel by hand.
        if request.starts_with('{') {
            return self.on_json_request(client, index, &request);
        }
        let (verb, rest) = match request.split_once(' ') {
            Some((verb, rest)) => (verb, rest.trim()),
            None => (request.as_str(), ""),
        };
        match verb {
            "resync" => self.send_snapshots(client, index),
            "ping" => {
                self.send_json(client, index, "{\"type\":\"pong\"}")?;
                Ok(())
            }
            // A filter box sends one of these per keystroke, and each answer is
            // the whole matching unit table for every scope. Re-sending it for
            // a filter that did not actually change is pure cost -- and the
            // fastest way to exhaust the peer's window.
            "filter" => {
                let mut changed = false;
                if let Role::Data(filter) = &mut self.conns[index].role
                    && filter.prefix != rest
                {
                    filter.prefix = String::from(rest);
                    changed = true;
                }
                if !changed {
                    return Ok(());
                }
                self.send_snapshots(client, index)
            }
            "scopes" => {
                let wanted: Vec<String> = rest
                    .split(',')
                    .map(|name| name.trim().to_string())
                    .filter(|name| !name.is_empty())
                    .collect();
                let mut changed = false;
                if let Role::Data(filter) = &mut self.conns[index].role
                    && filter.scopes != wanted
                {
                    filter.scopes = wanted;
                    changed = true;
                }
                if !changed {
                    return Ok(());
                }
                self.send_snapshots(client, index)
            }
            other => {
                let mut json = String::from("{\"type\":\"error\",\"message\":");
                push_json_string(&mut json, &format!("unknown request {other:?}"));
                json.push('}');
                self.send_json(client, index, &json)?;
                Ok(())
            }
        }
    }

    // --- journal queries -------------------------------------------------

    /// Consume whole lines of `journalctl` output into entries.
    fn drain_job_lines(&mut self, job: usize) {
        loop {
            let Some(end) = self.jobs[job].line.iter().position(|byte| *byte == b'\n') else {
                // One journal entry is a JSON object on one line, and a boot
                // list is one array on one line; neither is a megabyte. An
                // unterminated one that big is a journalctl this cannot read,
                // and holding it only grows the guest.
                if self.jobs[job].line.len() > MAX_JOB_LINE {
                    self.jobs[job].line.clear();
                    self.jobs[job].detail = String::from("journal output line was too long");
                }
                return;
            };
            let raw: Vec<u8> = self.jobs[job].line.drain(..=end).collect();
            let line = String::from_utf8_lossy(&raw[..raw.len() - 1]).to_string();
            self.absorb_job_line(job, &line);
        }
    }

    fn absorb_job_line(&mut self, job: usize, line: &str) {
        let line = line.trim();
        if line.is_empty() {
            return;
        }
        match self.jobs[job].kind {
            JobKind::Logs | JobKind::Follow => {
                self.jobs[job].absorbed += 1;
                if let Some(entry) = journal_entry_json(line) {
                    self.jobs[job].entries.push(entry);
                }
            }
            // `--list-boots -o json` answers with one array, not one object
            // per line, so the whole reply arrives as a single "line".
            JobKind::Boots => {
                for object in split_json_array(line) {
                    if let Some(entry) = boot_entry_json(&object) {
                        self.jobs[job].entries.push(entry);
                    }
                }
            }
        }
    }

    /// `{"type":"logs"|"boots"|"cancel", …}` — the request half of paging.
    fn on_json_request(
        &mut self,
        client: &mut Client,
        index: usize,
        request: &str,
    ) -> Result<(), String> {
        let fields = parse_flat_json(request);
        let field = |key: &str| -> String {
            fields
                .iter()
                .find(|(name, _)| name == key)
                .map(|(_, value)| value.clone())
                .unwrap_or_default()
        };
        let channel_id = self.conns[index].id;
        let request_id = field("id");
        match field("type").as_str() {
            "logs" => {
                // `all` is not a scope systemd has; it is the absence of the
                // filter, which is what a caller wants when the journal being
                // read is not this machine's own live one.
                let scope = match field("scope").as_str() {
                    "user" => "user",
                    "all" => "all",
                    _ => "system",
                };
                let limit = field("limit")
                    .parse::<u64>()
                    .unwrap_or(JOURNAL_DEFAULT_LIMIT)
                    .clamp(1, JOURNAL_MAX_LIMIT);
                // Older pages walk backwards from a cursor, which journalctl
                // spells `--after-cursor … --reverse`; newer pages walk
                // forward from the same cursor with the same anchor.
                let backward = field("direction") != "forward";
                let cursor = field("cursor");
                let mut argv = vec![
                    String::from("--no-pager"),
                    String::from("-o"),
                    String::from("json"),
                    format!("--output-fields={JOURNAL_FIELDS}"),
                    format!("--lines={limit}"),
                ];
                match scope {
                    "user" => argv.push(String::from("--user")),
                    "system" => argv.push(String::from("--system")),
                    _ => {}
                }
                if !self.journal_dir.is_empty() {
                    argv.push(format!("--directory={}", self.journal_dir));
                }
                // Long options only: `-u=x` is not `-u x`, getopt reads the
                // whole `=x` as the value and the filter silently matches
                // nothing. A user-scope unit lives in a different field, so
                // it needs the other flag entirely.
                let unit_flag = if scope == "user" {
                    "--user-unit"
                } else {
                    "--unit"
                };
                for (flag, value) in [
                    (unit_flag, field("unit")),
                    ("--boot", field("boot")),
                    ("--priority", field("priority")),
                    ("--grep", field("grep")),
                    ("--after-cursor", cursor.clone()),
                ] {
                    if value.is_empty() {
                        continue;
                    }
                    if value.len() > JOURNAL_MAX_ARG {
                        return self.fail_job(client, index, &request_id, "filter is too long");
                    }
                    argv.push(format!("{flag}={value}"));
                }
                if backward {
                    argv.push(String::from("--reverse"));
                }
                self.start_job(
                    client,
                    JobSpec {
                        channel_id,
                        request_id: &request_id,
                        kind: JobKind::Logs,
                        reverse: backward,
                        limit,
                        argv: &argv,
                    },
                )
            }
            // Live tail. The same filters as `logs`, plus the cursor the
            // loaded page ended on: starting the stream from there is what
            // makes the join seamless -- no gap while the page was rendering,
            // and no entry delivered twice.
            "follow" => {
                let scope = match field("scope").as_str() {
                    "user" => "user",
                    "all" => "all",
                    _ => "system",
                };
                let cursor = field("cursor");
                let mut argv = vec![
                    String::from("--no-pager"),
                    String::from("-o"),
                    String::from("json"),
                    format!("--output-fields={JOURNAL_FIELDS}"),
                    String::from("--follow"),
                ];
                match scope {
                    "user" => argv.push(String::from("--user")),
                    "system" => argv.push(String::from("--system")),
                    _ => {}
                }
                if !self.journal_dir.is_empty() {
                    argv.push(format!("--directory={}", self.journal_dir));
                }
                // Without a cursor there is no anchor, so start at the end
                // rather than replaying the whole journal into the stream.
                if cursor.is_empty() {
                    argv.push(String::from("--lines=0"));
                }
                let unit_flag = if scope == "user" {
                    "--user-unit"
                } else {
                    "--unit"
                };
                for (flag, value) in [
                    (unit_flag, field("unit")),
                    ("--priority", field("priority")),
                    ("--grep", field("grep")),
                    ("--after-cursor", cursor),
                ] {
                    if value.is_empty() {
                        continue;
                    }
                    if value.len() > JOURNAL_MAX_ARG {
                        return self.fail_job(client, index, &request_id, "filter is too long");
                    }
                    argv.push(format!("{flag}={value}"));
                }
                self.start_job(
                    client,
                    JobSpec {
                        channel_id,
                        request_id: &request_id,
                        kind: JobKind::Follow,
                        reverse: false,
                        limit: 0,
                        argv: &argv,
                    },
                )
            }
            // Stop the live tail, leaving any page query alone.
            "unfollow" => {
                self.cancel_jobs(client, channel_id, Some(JobKind::Follow), None);
                Ok(())
            }
            "boots" => {
                let mut argv = vec![
                    String::from("--no-pager"),
                    String::from("--list-boots"),
                    String::from("-o"),
                    String::from("json"),
                ];
                if !self.journal_dir.is_empty() {
                    argv.push(format!("--directory={}", self.journal_dir));
                }
                self.start_job(
                    client,
                    JobSpec {
                        channel_id,
                        request_id: &request_id,
                        kind: JobKind::Boots,
                        reverse: false,
                        limit: 0,
                        argv: &argv,
                    },
                )
            }
            "cancel" => {
                self.cancel_jobs(client, channel_id, None, Some(&request_id));
                Ok(())
            }
            other => {
                let mut json = String::from("{\"type\":\"error\",\"id\":");
                push_json_string(&mut json, &request_id);
                json.push_str(",\"message\":");
                push_json_string(&mut json, &format!("unknown request {other:?}"));
                json.push('}');
                self.send_json(client, index, &json)?;
                Ok(())
            }
        }
    }

    fn fail_job(
        &mut self,
        client: &mut Client,
        index: usize,
        request_id: &str,
        message: &str,
    ) -> Result<(), String> {
        let mut json = String::from("{\"type\":\"error\",\"id\":");
        push_json_string(&mut json, request_id);
        json.push_str(",\"message\":");
        push_json_string(&mut json, message);
        json.push('}');
        self.deliver(client, index, &json)?;
        if matches!(self.conns[index].role, Role::Command | Role::Watch(_)) {
            return self.finish(client, index, 1);
        }
        Ok(())
    }

    /// What a caller has to say to start a `journalctl`.
    fn start_job(&mut self, client: &mut Client, spec: JobSpec<'_>) -> Result<(), String> {
        let JobSpec {
            channel_id,
            request_id,
            kind,
            reverse,
            limit,
            argv,
        } = spec;
        // One query per channel: a viewer scrolling fast should replace its
        // own in-flight page rather than queue a hundred journalctl runs.
        self.cancel_jobs(client, channel_id, Some(kind), None);
        let process_id = self.next_process_id;
        self.next_process_id += 1;
        let mut owned: Vec<&[u8]> = vec![b"journalctl"];
        owned.extend(argv.iter().map(|argument| argument.as_bytes()));
        let request = ProcessSpawnRequest {
            nonce: process_id as u16,
            process_id,
            flags: 0,
            cwd_kind: PROCESS_CWD_DEFAULT,
            src_pty_id: 0,
            cwd: b"",
            argv: owned,
            env: Vec::new(),
        };
        let packet =
            msg_process_spawn(&request).map_err(|error| format!("journal spawn: {error:?}"))?;
        client
            .send(&packet)
            .map_err(|error| format!("journal spawn: {error}"))?;
        self.jobs.push(Job {
            channel_id,
            request_id: String::from(request_id),
            process_id,
            kind,
            reverse,
            line: Vec::new(),
            entries: Vec::new(),
            detail: String::new(),
            limit,
            absorbed: 0,
            flush_deadline: None,
        });
        Ok(())
    }

    /// Send a follow's batched entries, if it has any.
    fn flush_follow(&mut self, client: &mut Client, job: usize) -> Result<(), String> {
        self.jobs[job].flush_deadline = None;
        if self.jobs[job].entries.is_empty() {
            return Ok(());
        }
        let Some(index) = self.conn_index(self.jobs[job].channel_id) else {
            return Ok(());
        };
        let entries = std::mem::take(&mut self.jobs[job].entries);
        let mut position = 0;
        // Chunked on the same budget as a page: a burst after a resume can be
        // far more than one message will carry.
        while position < entries.len() {
            let mut json = String::from("{\"type\":\"logs\",\"follow\":true,\"id\":");
            push_json_string(&mut json, &self.jobs[job].request_id);
            json.push_str(",\"entries\":[");
            let mut count = 0;
            while position < entries.len() {
                let entry = &entries[position];
                if json.len() + entry.len() + 2 > SNAPSHOT_CHUNK && count > 0 {
                    break;
                }
                if count > 0 {
                    json.push(',');
                }
                json.push_str(entry);
                position += 1;
                count += 1;
            }
            json.push_str("],\"last\":true}");
            if !self.deliver(client, index, &json)? {
                return Ok(());
            }
        }
        Ok(())
    }

    /// Drop this channel's in-flight queries, optionally narrowing to one.
    ///
    /// Scoped by kind: a viewer scrolling fast should replace its own page,
    /// but the boot list it asked for in the same breath is a different
    /// question and must not be cancelled by the answer to another.
    fn cancel_jobs(
        &mut self,
        client: &mut Client,
        channel_id: u32,
        kind: Option<JobKind>,
        request_id: Option<&str>,
    ) {
        let mut index = 0;
        while index < self.jobs.len() {
            let job = &self.jobs[index];
            let matches = job.channel_id == channel_id
                && kind.is_none_or(|wanted| job.kind == wanted)
                && request_id.is_none_or(|wanted| wanted.is_empty() || job.request_id == wanted);
            if !matches {
                index += 1;
                continue;
            }
            let control = msg_process_control(ProcessControl {
                nonce: job.process_id as u16,
                process_id: job.process_id,
                action: PROCESS_CONTROL_TERMINATE,
                value: 0,
            });
            if let Ok(packet) = control {
                let _ = client.send(&packet);
            }
            self.jobs.remove(index);
        }
    }

    fn job_index(&self, process_id: u32) -> Option<usize> {
        self.jobs
            .iter()
            .position(|job| job.process_id == process_id)
    }

    /// Turn one finished `journalctl` into chunked messages on its channel.
    fn finish_job(&mut self, client: &mut Client, job_index: usize) -> Result<(), String> {
        // A follow is not a page: whatever it had batched goes out, and the
        // reader is told the stream ended so it can offer to resume rather
        // than sit in front of a tail that silently stopped.
        if self.jobs[job_index].kind == JobKind::Follow {
            self.flush_follow(client, job_index)?;
            let job = self.jobs.remove(job_index);
            let Some(index) = self.conn_index(job.channel_id) else {
                return Ok(());
            };
            let mut json = String::from("{\"type\":\"followEnd\",\"id\":");
            push_json_string(&mut json, &job.request_id);
            json.push_str(",\"message\":");
            push_json_string(
                &mut json,
                if job.detail.is_empty() {
                    "journal follow ended"
                } else {
                    &job.detail
                },
            );
            json.push('}');
            self.deliver(client, index, &json)?;
            // A `logs --follow` invocation ends when its stream does.
            if matches!(self.conns[index].role, Role::Command | Role::Watch(_)) {
                return self.finish(client, index, 0);
            }
            return Ok(());
        }
        let job = self.jobs.remove(job_index);
        let Some(index) = self.conn_index(job.channel_id) else {
            return Ok(());
        };
        if job.entries.is_empty() && !job.detail.is_empty() {
            // journalctl's own words: "No journal files were opened due to
            // insufficient permissions" is the answer, not an empty page.
            return self.fail_job(client, index, &job.request_id, &job.detail);
        }
        // The wire order is always oldest-first, whichever way the page walked.
        let mut entries = job.entries;
        if job.reverse {
            entries.reverse();
        }
        let kind = match job.kind {
            // A follow returned above; it shares the `logs` payload type.
            JobKind::Logs | JobKind::Follow => "logs",
            JobKind::Boots => "boots",
        };
        // Counted from what journalctl produced, not from what survived the
        // parse: an entry dropped for want of a cursor still proves the
        // journal had more to give, and counting survivors ended scrollback
        // early on any page that contained one.
        let more = job.kind == JobKind::Logs && job.absorbed >= job.limit;
        let mut chunk = 0;
        let mut position = 0;
        loop {
            let mut json = format!("{{\"type\":\"{kind}\",\"id\":");
            push_json_string(&mut json, &job.request_id);
            json.push_str(&format!(",\"chunk\":{chunk},\"entries\":["));
            let mut count = 0;
            while position < entries.len() {
                let entry = &entries[position];
                if json.len() + entry.len() + 2 > SNAPSHOT_CHUNK && count > 0 {
                    break;
                }
                if count > 0 {
                    json.push(',');
                }
                json.push_str(entry);
                position += 1;
                count += 1;
            }
            let last = position >= entries.len();
            json.push_str(&format!("],\"last\":{last}"));
            if last {
                json.push_str(&format!(",\"more\":{more}"));
            }
            json.push('}');
            if !self.deliver(client, index, &json)? {
                return Ok(());
            }
            chunk += 1;
            if last {
                break;
            }
        }
        // A one-shot page answers an invocation completely.
        if matches!(self.conns[index].role, Role::Command | Role::Watch(_)) {
            return self.finish(client, index, 0);
        }
        Ok(())
    }

    // --- @systemd commands ----------------------------------------------

    fn on_command_payload(
        &mut self,
        client: &mut Client,
        index: usize,
        payload: &[u8],
    ) -> Result<(), String> {
        const INVOKE: u8 = 1;
        const CANCEL: u8 = 4;
        match payload.first().copied() {
            Some(INVOKE) => {
                let args = parse_invoke(payload)?;
                self.run_command(client, index, &args)
            }
            Some(CANCEL) => self.finish(client, index, 0),
            _ => Ok(()),
        }
    }

    /// Send the exit status and close, once everything before it has gone out.
    ///
    /// Closing while output is still queued would truncate the answer the
    /// invocation just produced, so the close waits for the queue to drain and
    /// [`drain_queue`](Self::drain_queue) performs it.
    fn finish(&mut self, client: &mut Client, index: usize, code: i32) -> Result<(), String> {
        let payload = exit_payload(code, "").map_err(|_| "exit payload")?;
        self.send_bytes(client, index, &payload)?;
        self.conns[index].close_when_drained = true;
        self.close_if_drained(client, index)
    }

    /// Close a finished connection if nothing is still waiting to be sent.
    fn close_if_drained(&mut self, client: &mut Client, index: usize) -> Result<(), String> {
        let conn = &mut self.conns[index];
        if conn.closed || !conn.close_when_drained || !conn.queue.is_empty() {
            return Ok(());
        }
        conn.closed = true;
        let close = msg_channel_close(conn.id, CHANNEL_CLOSE_NORMAL).ok_or("invalid close")?;
        client
            .send(&close)
            .map_err(|error| format!("channel close: {error}"))?;
        Ok(())
    }

    fn out(&mut self, client: &mut Client, index: usize, text: &str) -> Result<(), String> {
        let payload = stdout_payload(text.as_bytes()).map_err(|_| "stdout payload")?;
        self.send_bytes(client, index, &payload)?;
        Ok(())
    }

    /// Answer one invocation as text or as a structured `RESULT`, never both:
    /// the CLI writes a plain-mode RESULT straight to stdout, so sending the
    /// pair would print every answer twice.
    fn answer(
        &mut self,
        client: &mut Client,
        index: usize,
        json_mode: bool,
        text: &str,
        json: &str,
    ) -> Result<(), String> {
        if json_mode {
            let payload =
                result_payload("application/json", json.as_bytes()).map_err(|_| "result")?;
            self.send_bytes(client, index, &payload)?;
        } else {
            self.out(client, index, text)?;
        }
        self.finish(client, index, 0)
    }

    fn run_command(
        &mut self,
        client: &mut Client,
        index: usize,
        args: &[String],
    ) -> Result<(), String> {
        let mut scope_filter: Vec<String> = Vec::new();
        let mut state_filter = String::new();
        let mut json_mode = false;
        let mut follow = false;
        let mut unit = String::new();
        let mut priority = String::new();
        let mut grep = String::new();
        let mut cursor = String::new();
        let mut lines = JOURNAL_DEFAULT_LIMIT;
        let mut positional: Vec<String> = Vec::new();
        let mut position = 0;
        while position < args.len() {
            match args[position].as_str() {
                "--scope" => {
                    if let Some(value) = args.get(position + 1) {
                        scope_filter.push(value.clone());
                    }
                    position += 2;
                }
                "--state" => {
                    state_filter = args.get(position + 1).cloned().unwrap_or_default();
                    position += 2;
                }
                "--unit" | "-u" => {
                    unit = args.get(position + 1).cloned().unwrap_or_default();
                    position += 2;
                }
                "--priority" => {
                    priority = args.get(position + 1).cloned().unwrap_or_default();
                    position += 2;
                }
                "--grep" => {
                    grep = args.get(position + 1).cloned().unwrap_or_default();
                    position += 2;
                }
                // Where the previous page stopped: this is what makes
                // scrollback continuous rather than a repeated first page.
                "--cursor" => {
                    cursor = args.get(position + 1).cloned().unwrap_or_default();
                    position += 2;
                }
                "--lines" | "-n" => {
                    lines = args
                        .get(position + 1)
                        .and_then(|value| value.parse::<u64>().ok())
                        .unwrap_or(JOURNAL_DEFAULT_LIMIT)
                        .clamp(1, JOURNAL_MAX_LIMIT);
                    position += 2;
                }
                "--follow" | "-f" => {
                    follow = true;
                    position += 1;
                }
                "--json" => {
                    json_mode = true;
                    position += 1;
                }
                other => {
                    positional.push(String::from(other));
                    position += 1;
                }
            }
        }
        let verb = positional
            .first()
            .cloned()
            .unwrap_or_else(|| String::from("status"));
        let prefix = positional.get(1).cloned().unwrap_or_default();
        let filter = Filter {
            prefix: prefix.clone(),
            scopes: scope_filter,
        };

        match verb.as_str() {
            "status" => {
                let mut text = String::new();
                let mut json = String::from("{\"scopes\":[");
                for (position, scope) in self.scopes.iter().enumerate() {
                    text.push_str(&format!(
                        "{:<7} units={:<5} source={:<7} snapshots={:<5} {}{}\n",
                        scope.name,
                        scope.units.len(),
                        scope.source,
                        scope.snapshots,
                        if scope.running { "running" } else { "stopped" },
                        if scope.detail.is_empty() {
                            String::new()
                        } else {
                            format!(" ({})", scope.detail)
                        }
                    ));
                    if position > 0 {
                        json.push(',');
                    }
                    json.push_str(&format!(
                        "{{\"scope\":\"{}\",\"units\":{},\"snapshots\":{},\"running\":{},\"source\":",
                        scope.name,
                        scope.units.len(),
                        scope.snapshots,
                        scope.running
                    ));
                    push_json_string(&mut json, &scope.source);
                    json.push_str(",\"detail\":");
                    push_json_string(&mut json, &scope.detail);
                    json.push('}');
                }
                json.push_str(&format!(
                    "],\"subscribers\":{},\"channel\":\"{CHANNEL_NAME}\"}}",
                    self.subscribers()
                ));
                self.answer(client, index, json_mode, &text, &json)
            }
            "list" => {
                let mut text = String::new();
                let mut json = String::from("{\"units\":[");
                let mut matched = 0;
                for scope_index in 0..self.scopes.len() {
                    let name = self.scopes[scope_index].name;
                    let units: Vec<(String, Unit)> = self.scopes[scope_index]
                        .units
                        .iter()
                        .filter(|(unit, state)| {
                            filter.matches(name, unit)
                                && (state_filter.is_empty()
                                    || state.active == state_filter
                                    || state.sub == state_filter)
                        })
                        .map(|(unit, state)| (unit.clone(), state.clone()))
                        .collect();
                    for (unit, state) in units {
                        text.push_str(&format!(
                            "{name:<7} {unit:<52} {:<8} {:<10} {}\n",
                            state.active, state.sub, state.description
                        ));
                        if matched > 0 {
                            json.push(',');
                        }
                        json.push_str(&format!("{{\"scope\":\"{name}\","));
                        push_unit_json_body(&mut json, &unit, &state);
                        json.push('}');
                        matched += 1;
                    }
                }
                json.push_str(&format!("],\"count\":{matched}}}"));
                self.answer(client, index, json_mode, &text, &json)
            }
            "get" => {
                let Some(name) = positional.get(1).cloned() else {
                    self.out(client, index, "usage: get UNIT [--scope system|user]\n")?;
                    return self.finish(client, index, 2);
                };
                let mut found = None;
                for scope in &self.scopes {
                    if !filter.scopes.is_empty()
                        && !filter.scopes.iter().any(|wanted| wanted == scope.name)
                    {
                        continue;
                    }
                    if let Some(state) = scope.units.get(&name) {
                        found = Some((scope.name, state.clone()));
                        break;
                    }
                }
                match found {
                    Some((scope, state)) => {
                        let text = format!(
                            "{scope} {name}\n  load        {}\n  active      {}\n  sub         {}\n  description {}\n",
                            state.load, state.active, state.sub, state.description
                        );
                        let mut json = format!("{{\"scope\":\"{scope}\",");
                        push_unit_json_body(&mut json, &name, &state);
                        json.push('}');
                        self.answer(client, index, json_mode, &text, &json)
                    }
                    None => {
                        self.out(client, index, &format!("no such unit: {name}\n"))?;
                        self.finish(client, index, 1)
                    }
                }
            }
            // The channel's journal reader, reachable from a shell. `--follow`
            // is the same live stream the panel gets, and without `--follow`
            // it is one page of scrollback addressed by `--cursor`.
            "logs" => {
                let channel_id = self.conns[index].id;
                let scope = match filter.scopes.first().map(String::as_str) {
                    Some("user") => "user",
                    Some("all") => "all",
                    _ => "system",
                };
                let mut argv = vec![
                    String::from("--no-pager"),
                    String::from("-o"),
                    String::from("json"),
                    format!("--output-fields={JOURNAL_FIELDS}"),
                ];
                match scope {
                    "user" => argv.push(String::from("--user")),
                    "system" => argv.push(String::from("--system")),
                    _ => {}
                }
                if !self.journal_dir.is_empty() {
                    argv.push(format!("--directory={}", self.journal_dir));
                }
                let unit_flag = if scope == "user" {
                    "--user-unit"
                } else {
                    "--unit"
                };
                if follow {
                    argv.push(String::from("--follow"));
                    if cursor.is_empty() {
                        argv.push(String::from("--lines=0"));
                    }
                } else {
                    argv.push(format!("--lines={lines}"));
                    argv.push(String::from("--reverse"));
                }
                for (flag, value) in [
                    (unit_flag, unit.clone()),
                    ("--priority", priority.clone()),
                    ("--grep", grep.clone()),
                    ("--after-cursor", cursor.clone()),
                ] {
                    if value.is_empty() {
                        continue;
                    }
                    if value.len() > JOURNAL_MAX_ARG {
                        return self.fail_job(client, index, "", "filter is too long");
                    }
                    argv.push(format!("{flag}={value}"));
                }
                self.start_job(
                    client,
                    JobSpec {
                        channel_id,
                        request_id: "",
                        kind: if follow {
                            JobKind::Follow
                        } else {
                            JobKind::Logs
                        },
                        reverse: !follow,
                        limit: lines,
                        argv: &argv,
                    },
                )
            }
            "watch" => {
                self.conns[index].role = Role::Watch(filter);
                self.out(
                    client,
                    index,
                    &format!(
                        "watching {} units; interrupt to stop\n",
                        self.scopes
                            .iter()
                            .map(|scope| scope.units.len())
                            .sum::<usize>()
                    ),
                )?;
                // A watcher makes the poll interval the fast one.
                for scope_index in 0..self.scopes.len() {
                    self.request_snapshot(client, scope_index)?;
                }
                Ok(())
            }
            other => {
                self.out(
                    client,
                    index,
                    &format!("unknown command {other:?}; try status, list, get, watch\n"),
                )?;
                self.finish(client, index, 2)
            }
        }
    }
}

/// One `systemctl list-units --plain` row: four fields and a free-text tail.
///
/// The columns are padded to the widest value, so the split has to be on runs
/// of whitespace rather than single spaces.
fn parse_unit_row(row: &str) -> Option<(String, Unit)> {
    let mut fields = row.split_whitespace();
    let name = fields.next()?;
    let load = fields.next()?;
    let active = fields.next()?;
    let sub = fields.next()?;
    let consumed = [name, load, active, sub]
        .iter()
        .fold(0usize, |offset, field| {
            let start = row[offset..].find(field).map_or(offset, |at| offset + at);
            start + field.len()
        });
    Some((
        String::from(name),
        Unit {
            load: String::from(load),
            active: String::from(active),
            sub: String::from(sub),
            description: String::from(row[consumed..].trim()),
        },
    ))
}

/// Read one field out of a flat JSON object.
///
/// Not a JSON parser: it walks the object's top level, which is all these
/// payloads are — `journalctl -o json` writes one flat object per entry, and
/// a request is a handful of strings. A value that is an array (journald's
/// encoding for a field that is not valid UTF-8) reads as absent.
fn json_field(object: &str, key: &str) -> Option<String> {
    let bytes = object.as_bytes();
    let mut index = 0;
    let mut depth = 0i32;
    while index < bytes.len() {
        match bytes[index] {
            b'{' | b'[' => {
                depth += 1;
                index += 1;
            }
            b'}' | b']' => {
                depth -= 1;
                index += 1;
            }
            b'"' => {
                let (text, next) = json_string_at(object, index)?;
                index = next;
                // Only a key at the object's own level counts.
                if depth != 1 || text != key {
                    continue;
                }
                while index < bytes.len() && (bytes[index] as char).is_whitespace() {
                    index += 1;
                }
                if bytes.get(index) != Some(&b':') {
                    continue;
                }
                index += 1;
                while index < bytes.len() && (bytes[index] as char).is_whitespace() {
                    index += 1;
                }
                return match bytes.get(index)? {
                    b'"' => json_string_at(object, index).map(|(value, _)| value),
                    b'[' | b'{' => None,
                    _ => {
                        let start = index;
                        while index < bytes.len()
                            && !matches!(bytes[index], b',' | b'}' | b']' | b' ')
                        {
                            index += 1;
                        }
                        Some(String::from(&object[start..index]))
                    }
                };
            }
            _ => index += 1,
        }
    }
    None
}

/// Decode the JSON string starting at `start`, returning it and the offset
/// just past its closing quote.
fn json_string_at(text: &str, start: usize) -> Option<(String, usize)> {
    let bytes = text.as_bytes();
    if bytes.get(start) != Some(&b'"') {
        return None;
    }
    let mut out = String::new();
    let mut index = start + 1;
    while index < bytes.len() {
        match bytes[index] {
            b'"' => return Some((out, index + 1)),
            b'\\' => {
                let escape = *bytes.get(index + 1)?;
                index += 2;
                match escape {
                    b'n' => out.push('\n'),
                    b't' => out.push('\t'),
                    b'r' => out.push('\r'),
                    b'b' => out.push('\u{8}'),
                    b'f' => out.push('\u{c}'),
                    b'u' => {
                        let hex = text.get(index..index + 4)?;
                        index += 4;
                        let code = u32::from_str_radix(hex, 16).ok()?;
                        out.push(char::from_u32(code).unwrap_or('\u{fffd}'));
                    }
                    other => out.push(other as char),
                }
            }
            _ => {
                let rest = &text[index..];
                let character = rest.chars().next()?;
                out.push(character);
                index += character.len_utf8();
            }
        }
    }
    None
}

/// Split a JSON array of objects into its top-level objects, textually.
fn split_json_array(text: &str) -> Vec<String> {
    let bytes = text.as_bytes();
    let mut objects = Vec::new();
    let mut depth = 0i32;
    let mut start = 0usize;
    let mut index = 0usize;
    while index < bytes.len() {
        match bytes[index] {
            b'"' => {
                let Some((_, next)) = json_string_at(text, index) else {
                    break;
                };
                index = next;
                continue;
            }
            b'{' => {
                if depth == 0 {
                    start = index;
                }
                depth += 1;
            }
            b'}' => {
                depth -= 1;
                if depth == 0 {
                    objects.push(String::from(&text[start..=index]));
                }
            }
            _ => {}
        }
        index += 1;
    }
    objects
}

/// Flat `{"key":"value"}` request objects, as key/value pairs.
fn parse_flat_json(object: &str) -> Vec<(String, String)> {
    let mut pairs = Vec::new();
    for key in [
        "type",
        "id",
        "scope",
        "unit",
        "boot",
        "priority",
        "grep",
        "cursor",
        "direction",
        "limit",
    ] {
        if let Some(value) = json_field(object, key) {
            pairs.push((String::from(key), value));
        }
    }
    pairs
}

/// One `journalctl -o json` entry, reduced to what a log view shows.
fn journal_entry_json(line: &str) -> Option<String> {
    let cursor = json_field(line, "__CURSOR")?;
    // A MESSAGE that is not UTF-8 arrives as an array of bytes; say so rather
    // than dropping the entry, which would silently shorten the page.
    let message = json_field(line, "MESSAGE").unwrap_or_else(|| String::from("(binary)"));
    let mut json = String::from("{\"cursor\":");
    push_json_string(&mut json, &cursor);
    json.push_str(",\"realtime\":");
    push_json_string(
        &mut json,
        &json_field(line, "__REALTIME_TIMESTAMP").unwrap_or_default(),
    );
    json.push_str(",\"priority\":");
    push_json_string(&mut json, &json_field(line, "PRIORITY").unwrap_or_default());
    json.push_str(",\"unit\":");
    push_json_string(
        &mut json,
        &json_field(line, "_SYSTEMD_UNIT")
            .or_else(|| json_field(line, "_COMM"))
            .unwrap_or_default(),
    );
    json.push_str(",\"pid\":");
    push_json_string(&mut json, &json_field(line, "_PID").unwrap_or_default());
    json.push_str(",\"message\":");
    push_json_string(&mut json, &message);
    json.push('}');
    Some(json)
}

/// One `--list-boots -o json` record.
fn boot_entry_json(object: &str) -> Option<String> {
    let boot = json_field(object, "boot_id")?;
    let mut json = String::from("{\"boot\":");
    push_json_string(&mut json, &boot);
    json.push_str(",\"index\":");
    push_json_string(&mut json, &json_field(object, "index").unwrap_or_default());
    json.push_str(",\"first\":");
    push_json_string(
        &mut json,
        &json_field(object, "first_entry").unwrap_or_default(),
    );
    json.push_str(",\"last\":");
    push_json_string(
        &mut json,
        &json_field(object, "last_entry").unwrap_or_default(),
    );
    json.push('}');
    Some(json)
}

fn parse_invoke(payload: &[u8]) -> Result<Vec<String>, String> {
    if payload.len() < 4 {
        return Err(String::from("truncated INVOKE"));
    }
    let count = u16::from_le_bytes([payload[2], payload[3]]) as usize;
    let mut offset = 4usize;
    // `count` is attacker-supplied and `usize` is 32-bit on wasm32, so every
    // step of the walk is checked rather than trusted to stay in range.
    let mut args = Vec::new();
    for _ in 0..count {
        let end = offset.checked_add(4).ok_or("INVOKE argument overflows")?;
        let length_bytes = payload
            .get(offset..end)
            .ok_or("truncated INVOKE argument length")?;
        let length = u32::from_le_bytes(length_bytes.try_into().expect("four bytes")) as usize;
        offset = end;
        let argument_end = offset
            .checked_add(length)
            .ok_or("INVOKE argument overflows")?;
        let argument = payload
            .get(offset..argument_end)
            .ok_or("truncated INVOKE argument")?;
        args.push(String::from_utf8_lossy(argument).to_string());
        offset = argument_end;
    }
    Ok(args)
}

fn diff(
    scope: &'static str,
    before: &BTreeMap<String, Unit>,
    after: &BTreeMap<String, Unit>,
) -> Change {
    let mut change = Change {
        scope,
        added: Vec::new(),
        changed: Vec::new(),
        removed: Vec::new(),
    };
    for (unit, state) in after {
        match before.get(unit) {
            None => change.added.push((unit.clone(), state.clone())),
            Some(previous) if previous != state => {
                change
                    .changed
                    .push((unit.clone(), state.clone(), previous.clone()));
            }
            Some(_) => {}
        }
    }
    for (unit, state) in before {
        if !after.contains_key(unit) {
            change.removed.push((unit.clone(), state.clone()));
        }
    }
    change
}

fn unix_millis(client: &Client) -> i64 {
    client.realtime_now().unix_timestamp_nanos() / 1_000_000
}

fn change_json(change: &Change, filter: &Filter, ts: i64) -> Option<String> {
    if !filter.scopes.is_empty() && !filter.scopes.iter().any(|name| name == change.scope) {
        return None;
    }
    let keep = |unit: &String| unit.starts_with(&filter.prefix);
    let added: Vec<_> = change.added.iter().filter(|(unit, _)| keep(unit)).collect();
    let changed: Vec<_> = change
        .changed
        .iter()
        .filter(|(unit, _, _)| keep(unit))
        .collect();
    let removed: Vec<_> = change
        .removed
        .iter()
        .filter(|(unit, _)| keep(unit))
        .collect();
    if added.is_empty() && changed.is_empty() && removed.is_empty() {
        return None;
    }

    let mut json = format!(
        "{{\"type\":\"change\",\"scope\":\"{}\",\"ts\":{ts},\"added\":[",
        change.scope
    );
    for (position, (unit, state)) in added.iter().enumerate() {
        if position > 0 {
            json.push(',');
        }
        push_unit_json(&mut json, unit, state);
    }
    json.push_str("],\"changed\":[");
    for (position, (unit, state, previous)) in changed.iter().enumerate() {
        if position > 0 {
            json.push(',');
        }
        json.push('{');
        push_unit_json_body(&mut json, unit, state);
        json.push_str(",\"previous\":{\"load\":");
        push_json_string(&mut json, &previous.load);
        json.push_str(",\"active\":");
        push_json_string(&mut json, &previous.active);
        json.push_str(",\"sub\":");
        push_json_string(&mut json, &previous.sub);
        json.push_str("}}");
    }
    json.push_str("],\"removed\":[");
    for (position, (unit, _)) in removed.iter().enumerate() {
        if position > 0 {
            json.push(',');
        }
        push_json_string(&mut json, unit);
    }
    json.push_str("]}");
    Some(json)
}

fn push_unit_json(json: &mut String, unit: &str, state: &Unit) {
    json.push('{');
    push_unit_json_body(json, unit, state);
    json.push('}');
}

fn push_unit_json_body(json: &mut String, unit: &str, state: &Unit) {
    json.push_str("\"name\":");
    push_json_string(json, unit);
    json.push_str(",\"load\":");
    push_json_string(json, &state.load);
    json.push_str(",\"active\":");
    push_json_string(json, &state.active);
    json.push_str(",\"sub\":");
    push_json_string(json, &state.sub);
    json.push_str(",\"description\":");
    push_json_string(json, &state.description);
}

/// Unit names carry `\x2d` escapes verbatim, so JSON escaping is not optional.
fn push_json_string(json: &mut String, value: &str) {
    json.push('"');
    for character in value.chars() {
        match character {
            '"' => json.push_str("\\\""),
            '\\' => json.push_str("\\\\"),
            '\n' => json.push_str("\\n"),
            '\r' => json.push_str("\\r"),
            '\t' => json.push_str("\\t"),
            control if (control as u32) < 0x20 => {
                json.push_str(&format!("\\u{:04x}", control as u32));
            }
            other => json.push(other),
        }
    }
    json.push('"');
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One real `journalctl -o json` line, captured on a live system.
    const ENTRY: &str = r#"{"_COMM":"sudo","__MONOTONIC_TIMESTAMP":"50233826838","__REALTIME_TIMESTAMP":"1787014673311286","MESSAGE":"pam_unix(sudo:session): session opened for user root(uid=0) by pcarrier(uid=1000)","__CURSOR":"s=8d3d672963ec49398326cba2c610f117;i=6dee6eb;b=60bbffd90f0a41a1a470bc44cfdfa2a3;m=bb22b5e16;t=65947c6cfa636;x=3b11c1341ffdd2","PRIORITY":"6","_SYSTEMD_UNIT":"user@1000.service","_PID":"931716","_BOOT_ID":"60bbffd90f0a41a1a470bc44cfdfa2a3"}"#;

    #[test]
    fn journal_fields_are_read_out_of_a_real_entry() {
        assert_eq!(json_field(ENTRY, "PRIORITY").as_deref(), Some("6"));
        assert_eq!(
            json_field(ENTRY, "_SYSTEMD_UNIT").as_deref(),
            Some("user@1000.service")
        );
        assert_eq!(json_field(ENTRY, "_PID").as_deref(), Some("931716"));
        assert!(
            json_field(ENTRY, "__CURSOR")
                .expect("cursor")
                .starts_with("s=8d3d672963ec4939")
        );
        assert_eq!(json_field(ENTRY, "NOT_THERE"), None);
    }

    #[test]
    fn message_escapes_survive_the_round_trip() {
        // journald escapes quotes in MESSAGE; the entry we emit has to escape
        // them again rather than hand a reader broken JSON.
        let line =
            r#"{"__CURSOR":"s=1","MESSAGE":"[\"90.38.173.240\"] said \\ hi","PRIORITY":"5"}"#;
        assert_eq!(
            json_field(line, "MESSAGE").as_deref(),
            Some(r#"["90.38.173.240"] said \ hi"#)
        );
        let entry = journal_entry_json(line).expect("entry");
        assert!(entry.contains(r#""message":"[\"90.38.173.240\"] said \\ hi""#));
    }

    #[test]
    fn a_binary_message_is_named_rather_than_dropped() {
        // A MESSAGE that is not UTF-8 arrives as an array of byte values.
        let line = r#"{"__CURSOR":"s=2","MESSAGE":[104,105],"PRIORITY":"3"}"#;
        let entry = journal_entry_json(line).expect("entry");
        assert!(entry.contains(r#""message":"(binary)""#));
        // An entry with no cursor cannot be paged from, so it is not an entry.
        assert!(journal_entry_json(r#"{"MESSAGE":"x"}"#).is_none());
    }

    #[test]
    fn nested_objects_do_not_shadow_top_level_keys() {
        let line = r#"{"a":{"__CURSOR":"inner"},"__CURSOR":"outer"}"#;
        assert_eq!(json_field(line, "__CURSOR").as_deref(), Some("outer"));
    }

    #[test]
    fn boots_come_out_of_one_json_array() {
        let boots = r#"[{"index":-7,"boot_id":"7ec05d4e13de4feb9aff26849d0eab95","first_entry":1784597241992512,"last_entry":1785874782673604},{"index":0,"boot_id":"60bbffd90f0a41a1a470bc44cfdfa2a3","first_entry":1786964477000000,"last_entry":1787014673311286}]"#;
        let objects = split_json_array(boots);
        assert_eq!(objects.len(), 2);
        let entry = boot_entry_json(&objects[1]).expect("boot");
        assert!(entry.contains(r#""boot":"60bbffd90f0a41a1a470bc44cfdfa2a3""#));
        assert!(entry.contains(r#""index":"0""#));
    }

    #[test]
    fn requests_are_read_as_flat_objects() {
        let request = r#"{"type":"logs","id":"7","unit":"sshd.service","grep":"a\"b","limit":50}"#;
        let fields = parse_flat_json(request);
        let get = |key: &str| {
            fields
                .iter()
                .find(|(name, _)| name == key)
                .map(|(_, value)| value.clone())
        };
        assert_eq!(get("type").as_deref(), Some("logs"));
        assert_eq!(get("id").as_deref(), Some("7"));
        assert_eq!(get("unit").as_deref(), Some("sshd.service"));
        assert_eq!(get("grep").as_deref(), Some("a\"b"));
        assert_eq!(get("limit").as_deref(), Some("50"));
        assert_eq!(get("cursor"), None);
    }

    /// The count and every length come off the wire, and `usize` is 32-bit on
    /// wasm32, so a hostile INVOKE must be refused rather than wrap.
    #[test]
    fn a_truncated_or_overflowing_invoke_is_refused() {
        // One argument, "hi".
        let mut good = vec![1u8, 0, 1, 0];
        good.extend_from_slice(&2u32.to_le_bytes());
        good.extend_from_slice(b"hi");
        assert_eq!(
            parse_invoke(&good).expect("parses"),
            vec![String::from("hi")]
        );

        // A length that runs off the end of the payload.
        let mut lying = vec![1u8, 0, 1, 0];
        lying.extend_from_slice(&u32::MAX.to_le_bytes());
        lying.extend_from_slice(b"hi");
        assert!(parse_invoke(&lying).is_err());

        // A count that promises more arguments than there are bytes.
        let mut greedy = vec![1u8, 0, 9, 0];
        greedy.extend_from_slice(&2u32.to_le_bytes());
        greedy.extend_from_slice(b"hi");
        assert!(parse_invoke(&greedy).is_err());

        assert!(parse_invoke(&[1, 0, 1]).is_err());
    }

    #[test]
    fn unit_rows_split_on_padding_not_single_spaces() {
        let row = "sshd.service     loaded    active   running   SSH Daemon";
        let (name, unit) = parse_unit_row(row).expect("row");
        assert_eq!(name, "sshd.service");
        assert_eq!(unit.load, "loaded");
        assert_eq!(unit.active, "active");
        assert_eq!(unit.sub, "running");
        assert_eq!(unit.description, "SSH Daemon");
        assert!(parse_unit_row("only two").is_none());
    }
}
