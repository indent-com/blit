//! `blit ext` — Wasmi extension lifecycle client.
//!
//! The wire adapter in this module is intentionally narrow.  The remote crate
//! currently owns bounded C2S decoding and `EXT_INFO(INIT)`; until it also owns
//! lifecycle response codecs, this client keeps all S2C layout knowledge in
//! `wire` below.  Moving those builders/parsers into `blit-remote` later does
//! not change command behavior.

use crate::transport::{FragmentReassembly, Transport, read_message, write_frame};
use blit_remote::extension::{
    EXT_CONTROL_ATTACH, EXT_CONTROL_CANCEL, EXT_CONTROL_DISABLE, EXT_CONTROL_ENABLE,
    EXT_CONTROL_LIST, EXT_CONTROL_REMOVE, EXT_CONTROL_RESTART, EXT_CONTROL_STATUS,
    EXT_CONTROL_UNFOLLOW, EXT_FLAG_PERSIST, EXT_INFO, EXT_MAX_ARG, EXT_MAX_ARGS,
    EXT_MAX_ARGUMENT_BYTES, EXT_MAX_DETAIL, EXT_MAX_MODULE, EXT_OUTPUT_EVENT, EXT_PUT_BEGIN,
    EXT_PUT_FINAL, EXT_RESTART_ALWAYS, EXT_RESTART_NEVER, EXT_RESTART_ON_FAILURE, EXT_RUN_DETACH,
    EXT_RUN_PERSIST, EXT_RUN_UPDATE, EXT_STATUS, FEATURE_EXTENSION,
};
use blit_remote::{S2C_HELLO, S2C_QUIT, S2C_READY, STATUS_CONFLICT, STATUS_OK, status_text};
use clap::{Args, Subcommand, ValueEnum};
use std::ffi::OsString;
use std::fmt;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncWrite};

#[path = "extension_command.rs"]
mod command_cli;

const UPLOAD_CHUNK: usize = 1024 * 1024;
const PHASE_NEED_OBJECT: u8 = 1;
const PHASE_RUNNING: u8 = 4;
const PHASE_STOPPED: u8 = 6;
const PHASE_BLOCKED: u8 = 7;
const PUT_ALREADY_HAVE: u8 = 128;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, ValueEnum)]
pub enum RestartPolicy {
    #[default]
    Never,
    OnFailure,
    Always,
}

impl RestartPolicy {
    fn wire(self) -> u8 {
        match self {
            Self::Never => EXT_RESTART_NEVER,
            Self::OnFailure => EXT_RESTART_ON_FAILURE,
            Self::Always => EXT_RESTART_ALWAYS,
        }
    }
}

impl fmt::Display for RestartPolicy {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Never => "never",
            Self::OnFailure => "on-failure",
            Self::Always => "always",
        })
    }
}

#[derive(Args, Clone, Debug)]
pub struct RunArgs {
    /// Return once the extension has reached RUNNING
    #[arg(long)]
    pub detach: bool,

    /// Store an enabled, desired-running definition (implies --detach)
    #[arg(long)]
    pub persist: bool,

    /// Attempt restart policy
    #[arg(long, value_enum, default_value_t)]
    pub restart: RestartPolicy,

    /// Emit NDJSON lifecycle and output records
    #[arg(long)]
    pub json: bool,

    // Positional, in the same place `update` takes it, so the two commands read
    // alike. It was an optional `--name` flag, which meant `--persist` had to
    // declare `requires = "name"` and the help had to explain when a name
    // mattered; every extension has one worth printing, so requiring it is
    // simpler than describing the exception.
    /// A label, or the unique durable name under --persist
    pub name: String,

    /// WebAssembly module (a path or an https:// URL) followed by UTF-8
    /// arguments passed verbatim
    #[arg(
        value_names = ["MODULE", "ARGS"],
        num_args = 1..,
        trailing_var_arg = true,
        allow_hyphen_values = true
    )]
    pub invocation: Vec<OsString>,
}

#[derive(Args, Clone, Debug)]
pub struct UpdateArgs {
    /// Replace the stored restart policy (preserved when omitted)
    #[arg(long, value_enum)]
    pub restart: Option<RestartPolicy>,

    /// Emit an NDJSON lifecycle record
    #[arg(long)]
    pub json: bool,

    /// Exact persistent extension name
    pub name: String,

    /// Replacement WebAssembly module (a path or an https:// URL) followed
    /// by UTF-8 arguments passed verbatim
    #[arg(
        value_names = ["MODULE", "ARGS"],
        num_args = 1..,
        trailing_var_arg = true,
        allow_hyphen_values = true
    )]
    pub invocation: Vec<OsString>,
}

#[derive(Subcommand, Clone, Debug)]
pub enum ExtensionCommand {
    /// Execute a WebAssembly extension
    Run(RunArgs),

    /// List visible extensions
    #[command(alias = "ls")]
    List {
        /// Emit one NDJSON record per extension
        #[arg(long)]
        json: bool,
    },

    /// Show one extension's current lifecycle snapshot
    Status {
        selector: String,
        #[arg(long)]
        json: bool,
    },

    /// Follow retained and future output from an extension
    Attach {
        selector: String,
        #[arg(long)]
        json: bool,
    },

    /// Clear desired-running and cancel the current attempt
    Cancel {
        selector: String,
        #[arg(long)]
        json: bool,
    },

    /// Replace a persistent extension definition
    Update(UpdateArgs),

    /// Start a fresh attempt immediately
    Restart {
        selector: String,
        #[arg(long)]
        json: bool,
    },

    /// Durably enable a persistent extension
    Enable {
        selector: String,
        #[arg(long)]
        json: bool,
    },

    /// Durably disable a persistent extension
    Disable {
        selector: String,
        #[arg(long)]
        json: bool,
    },

    /// Remove a disabled, quiescent persistent extension
    Remove {
        selector: String,
        #[arg(long)]
        json: bool,
    },

    /// List live extension-provided command namespaces
    Commands,
}

pub async fn dispatch(transport: Transport, command: ExtensionCommand) -> Result<i32, String> {
    let mut client = Client::connect(transport).await?;
    match command {
        ExtensionCommand::Run(args) => run(&mut client, args).await,
        ExtensionCommand::List { json } => {
            let records = client.list().await?;
            print_list(&records, json);
            Ok(0)
        }
        ExtensionCommand::Status { selector, json } => {
            control_once(&mut client, &selector, EXT_CONTROL_STATUS, json).await
        }
        ExtensionCommand::Attach { selector, json } => {
            let id = resolve_selector(&mut client, &selector).await?;
            let status = client.control(id, EXT_CONTROL_ATTACH).await?;
            ensure_ok(&status, "attach")?;
            render_status(&status, json, "status");
            follow(&mut client, id, status, json, FollowMode::Attach, None).await
        }
        ExtensionCommand::Cancel { selector, json } => {
            control_once(&mut client, &selector, EXT_CONTROL_CANCEL, json).await
        }
        ExtensionCommand::Update(args) => update(&mut client, args).await,
        ExtensionCommand::Restart { selector, json } => {
            control_once(&mut client, &selector, EXT_CONTROL_RESTART, json).await
        }
        ExtensionCommand::Enable { selector, json } => {
            control_once(&mut client, &selector, EXT_CONTROL_ENABLE, json).await
        }
        ExtensionCommand::Disable { selector, json } => {
            control_once(&mut client, &selector, EXT_CONTROL_DISABLE, json).await
        }
        ExtensionCommand::Remove { selector, json } => {
            control_once(&mut client, &selector, EXT_CONTROL_REMOVE, json).await
        }
        ExtensionCommand::Commands => command_cli::list(&mut client).await,
    }
}

pub(crate) fn parse_advertised_command(
    tokens: Vec<String>,
) -> Result<(String, Vec<String>), String> {
    command_cli::parse_external(tokens)
}

pub(crate) async fn dispatch_advertised_command(
    transport: Transport,
    name: String,
    args: Vec<String>,
    json: bool,
) -> Result<i32, String> {
    let mut client = Client::connect(transport).await?;
    command_cli::invoke(&mut client, &name, args, json).await
}

pub(crate) async fn complete_advertised_commands(
    transport: Transport,
    words: &[String],
    current: &str,
) -> Result<Vec<String>, String> {
    let mut client = Client::connect(transport).await?;
    command_cli::complete(&mut client, words, current).await
}

async fn run(client: &mut Client, args: RunArgs) -> Result<i32, String> {
    validate_name(&args.name)?;
    let (file, extension_args) = split_invocation(args.invocation)?;
    validate_args(&extension_args)?;
    let mut module = ModuleCache::new(file);
    let hash = module.hash().await?;
    let detached = args.detach || args.persist;
    let mut flags = u8::from(detached) * EXT_RUN_DETACH;
    if args.persist {
        flags |= EXT_RUN_PERSIST;
    }
    let name = args.name.as_str();
    let status = client
        .run_request(RunRequest {
            flags,
            restart: args.restart.wire(),
            expected_id: 0,
            expected_revision: 0,
            hash,
            name,
            args: &extension_args,
        })
        .await?;
    ensure_ok(&status, "run")?;
    let baseline = status.last_running_attempt;
    let id = status.extension_id;
    if id == 0 {
        return Err("server returned a successful run without an extension ID".into());
    }
    if status.phase == PHASE_NEED_OBJECT {
        let _ = client.upload(module.get().await?).await?;
    }
    if args.json {
        render_status(&status, true, "status");
    }
    let mode = if detached {
        FollowMode::Detached { baseline }
    } else {
        FollowMode::Owned
    };
    follow(client, id, status, args.json, mode, Some(&mut module)).await
}

async fn update(client: &mut Client, args: UpdateArgs) -> Result<i32, String> {
    let name = forced_name(&args.name)?;
    validate_name(name)?;
    let (file, extension_args) = split_invocation(args.invocation)?;
    validate_args(&extension_args)?;
    let mut module = ModuleCache::new(file);
    let hash = module.hash().await?;
    let records = client.list().await?;
    let record = find_named(&records, name)?;
    if record.flags & EXT_FLAG_PERSIST == 0 {
        return Err(format!("{name}: not a persistent extension"));
    }
    let expected_id = record.extension_id;
    let expected_revision = record.definition_revision;
    let restart = args.restart.map_or(record.restart, RestartPolicy::wire);

    loop {
        let status = client
            .run_request(RunRequest {
                flags: EXT_RUN_DETACH | EXT_RUN_PERSIST | EXT_RUN_UPDATE,
                restart,
                expected_id,
                expected_revision,
                hash,
                name,
                args: &extension_args,
            })
            .await?;
        ensure_ok(&status, "update")?;
        if status.phase != PHASE_NEED_OBJECT {
            render_status(&status, args.json, "status");
            return Ok(0);
        }

        match client.upload(module.get().await?).await? {
            UploadOutcome::Available => {
                // Uploading an update only primes the CAS.  Recheck the exact
                // tuple before retrying; never adopt a concurrent revision.
                let records = client.list().await?;
                let current = find_named(&records, name)?;
                if current.extension_id != expected_id
                    || current.definition_revision != expected_revision
                {
                    return Err(format!(
                        "{name}: definition changed during upload (expected id:{} revision {})",
                        format_id(expected_id),
                        expected_revision
                    ));
                }
            }
            UploadOutcome::Contended => {
                tokio::select! {
                    _ = tokio::time::sleep(Duration::from_secs(1)) => {}
                    _ = tokio::signal::ctrl_c() => return Ok(130),
                }
            }
        }
    }
}

async fn control_once(
    client: &mut Client,
    selector: &str,
    action: u8,
    json: bool,
) -> Result<i32, String> {
    let id = resolve_selector(client, selector).await?;
    let status = client.control(id, action).await?;
    ensure_ok(&status, action_name(action))?;
    render_status(&status, json, "status");
    Ok(0)
}

#[derive(Clone, Copy)]
enum FollowMode {
    Owned,
    Detached { baseline: u64 },
    Attach,
}

async fn follow(
    client: &mut Client,
    extension_id: u64,
    mut lifecycle: StatusRecord,
    json: bool,
    mode: FollowMode,
    mut module: Option<&mut ModuleCache>,
) -> Result<i32, String> {
    let mut last_exit: Option<ExitRecord> = None;
    let replay_snapshot_through = match mode {
        FollowMode::Attach => Some(lifecycle.output_sequence),
        FollowMode::Owned | FollowMode::Detached { .. } => None,
    };
    let mut replay_done = replay_snapshot_through.is_none();
    let mut expected_sequence = if lifecycle.replay_from_sequence != 0 {
        lifecycle.replay_from_sequence
    } else {
        lifecycle.output_sequence.saturating_add(1)
    };
    let mut pending_poll = None;
    let mut interrupted = false;
    let mut interrupt_deadline = None;
    let start = tokio::time::Instant::now() + Duration::from_secs(1);
    let mut poll = tokio::time::interval_at(start, Duration::from_secs(1));
    poll.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    loop {
        if replay_done {
            if let FollowMode::Detached { baseline } = mode
                && (lifecycle.phase == PHASE_RUNNING || lifecycle.last_running_attempt > baseline)
            {
                if !json {
                    println!("id:{}", format_id(extension_id));
                }
                return Ok(0);
            }
            if lifecycle.phase == PHASE_BLOCKED {
                if !lifecycle.detail.is_empty() {
                    eprintln!("blit: extension blocked: {}", lifecycle.detail);
                }
                return Ok(1);
            }
            if lifecycle.phase == PHASE_STOPPED {
                if let FollowMode::Detached { .. } = mode {
                    if !lifecycle.detail.is_empty() {
                        eprintln!(
                            "blit: extension stopped before running: {}",
                            lifecycle.detail
                        );
                    }
                    return Ok(1);
                }
                if interrupted {
                    return Ok(130);
                }
                return Ok(exit_code(last_exit.as_ref()));
            }
        }
        if interrupt_deadline.is_some_and(|deadline| tokio::time::Instant::now() >= deadline) {
            return Ok(130);
        }

        tokio::select! {
            _ = poll.tick(), if pending_poll.is_none() && !interrupted => {
                let nonce = client.send_control(extension_id, EXT_CONTROL_STATUS).await?;
                pending_poll = Some(nonce);
            }
            _ = tokio::signal::ctrl_c(), if !interrupted => {
                match mode {
                    FollowMode::Owned => {
                        client.send_control(extension_id, EXT_CONTROL_CANCEL).await?;
                        interrupted = true;
                        interrupt_deadline = Some(tokio::time::Instant::now() + Duration::from_secs(2));
                    }
                    FollowMode::Attach => {
                        let nonce = client.send_control(extension_id, EXT_CONTROL_UNFOLLOW).await?;
                        let _ = client.wait_status(nonce).await?;
                        return Ok(130);
                    }
                    FollowMode::Detached { .. } => return Ok(130),
                }
            }
            packet = client.next_packet() => {
                let packet = packet?;
                match wire::parse(&packet)? {
                    wire::Message::Status(status) if Some(status.nonce) == pending_poll => {
                        pending_poll = None;
                        ensure_ok(&status, "status")?;
                        if status.extension_id == extension_id {
                            lifecycle = status;
                            if json {
                                render_status(&lifecycle, true, "status");
                            }
                            if lifecycle.phase == PHASE_NEED_OBJECT
                                && let Some(module) = module.as_deref_mut()
                            {
                                let _ = client.upload(module.get().await?).await?;
                            }
                        }
                    }
                    wire::Message::Status(status) if status.extension_id == extension_id => {
                        ensure_ok(&status, "control")?;
                        lifecycle = status;
                        if json {
                            render_status(&lifecycle, true, "status");
                        }
                        if lifecycle.phase == PHASE_NEED_OBJECT
                            && let Some(module) = module.as_deref_mut()
                        {
                            let _ = client.upload(module.get().await?).await?;
                        }
                    }
                    wire::Message::InfoStatus(status) if status.extension_id == extension_id => {
                        report_gap(&mut expected_sequence, status.output_sequence, json);
                        let is_current = replay_snapshot_through
                            .is_none_or(|through| status.output_sequence > through);
                        if json {
                            render_status(&status, true, "status");
                        }
                        if is_current {
                            lifecycle = status;
                            if lifecycle.phase == PHASE_NEED_OBJECT
                                && let Some(module) = module.as_deref_mut()
                            {
                                let _ = client.upload(module.get().await?).await?;
                            }
                        }
                    }
                    wire::Message::Event(event) if event.extension_id == extension_id => {
                        report_gap(&mut expected_sequence, event.output_sequence, json);
                        render_event(&event, json)?;
                    }
                    wire::Message::Exit(exit) if exit.extension_id == extension_id => {
                        report_gap(&mut expected_sequence, exit.output_sequence, json);
                        render_exit(&exit, json);
                        if replay_snapshot_through
                            .is_none_or(|through| exit.output_sequence > through)
                        {
                            last_exit = Some(exit);
                        }
                    }
                    wire::Message::ReplayDone { extension_id: id, through_sequence }
                        if id == extension_id =>
                    {
                        if expected_sequence <= through_sequence {
                            let lost = through_sequence - expected_sequence + 1;
                            report_lost(lost, json);
                            expected_sequence = through_sequence.saturating_add(1);
                        }
                        if replay_snapshot_through
                            .is_some_and(|through| through_sequence >= through)
                        {
                            replay_done = true;
                        }
                    }
                    _ => {}
                }
            }
        }
    }
}

fn exit_code(exit: Option<&ExitRecord>) -> i32 {
    match exit {
        Some(exit) if exit.reason == 0 => exit.code,
        Some(_) => 1,
        None => 0,
    }
}

fn report_gap(expected: &mut u64, received: u64, json: bool) {
    if received == 0 {
        return;
    }
    if received > *expected {
        report_lost(received - *expected, json);
    }
    *expected = received.saturating_add(1);
}

fn report_lost(lost: u64, json: bool) {
    if json {
        println!("{}", serde_json::json!({"type": "gap", "lost": lost}));
    } else {
        eprintln!("blit: {lost} extension output record(s) were evicted");
    }
}

fn render_event(event: &EventRecord, json: bool) -> Result<(), String> {
    if json {
        println!(
            "{}",
            serde_json::json!({
                "type": "event",
                "extension_id": format_id(event.extension_id),
                "definition_revision": event.definition_revision,
                "attempt": event.attempt,
                "task_id": event.task_id,
                "output_sequence": event.output_sequence,
                "kind": event.kind,
                "data": event.data,
            })
        );
        return Ok(());
    }
    match event.kind {
        1 => std::io::stdout()
            .write_all(&event.data)
            .map_err(|e| format!("writing extension stdout: {e}"))?,
        2 => std::io::stderr()
            .write_all(&event.data)
            .map_err(|e| format!("writing extension stderr: {e}"))?,
        3 => eprintln!("{}", String::from_utf8_lossy(&event.data)),
        other => eprintln!(
            "blit: extension event kind {other} ({} bytes)",
            event.data.len()
        ),
    }
    Ok(())
}

fn render_exit(exit: &ExitRecord, json: bool) {
    if json {
        println!(
            "{}",
            serde_json::json!({
                "type": "exit",
                "extension_id": format_id(exit.extension_id),
                "definition_revision": exit.definition_revision,
                "attempt": exit.attempt,
                "task_id": exit.task_id,
                "output_sequence": exit.output_sequence,
                "reason": exit.reason,
                "code": exit.code,
                "next_start_unix_ms": exit.next_start_unix_ms,
                "detail": exit.detail,
            })
        );
    } else if exit.reason != 0 || !exit.detail.is_empty() {
        eprintln!(
            "blit: extension attempt {} ended: reason {} code {}{}",
            exit.attempt,
            exit.reason,
            exit.code,
            if exit.detail.is_empty() {
                String::new()
            } else {
                format!(" ({})", exit.detail)
            }
        );
    }
}

fn render_status(status: &StatusRecord, json: bool, kind: &str) {
    if json {
        println!(
            "{}",
            serde_json::json!({
                "type": kind,
                "nonce": status.nonce,
                "status": status.status,
                "phase": status.phase,
                "phase_name": phase_name(status.phase),
                "flags": status.flags,
                "restart": status.restart,
                "extension_id": format_id(status.extension_id),
                "definition_revision": status.definition_revision,
                "attempt": status.attempt,
                "last_running_attempt": status.last_running_attempt,
                "task_id": status.task_id,
                "replay_from_sequence": status.replay_from_sequence,
                "output_sequence": status.output_sequence,
                "next_start_unix_ms": status.next_start_unix_ms,
                "hash": hex(&status.hash),
                "detail": status.detail,
            })
        );
    } else {
        println!(
            "id:{}\tphase={}\trevision={}\tattempt={}\trestart={}\thash={}{}",
            format_id(status.extension_id),
            phase_name(status.phase),
            status.definition_revision,
            status.attempt,
            restart_name(status.restart),
            hex(&status.hash),
            if status.detail.is_empty() {
                String::new()
            } else {
                format!("\tdetail={}", status.detail)
            }
        );
    }
}

fn print_list(records: &[ExtensionRecord], json: bool) {
    for record in records {
        if json {
            println!(
                "{}",
                serde_json::json!({
                    "type": "extension",
                    "extension_id": format_id(record.extension_id),
                    "definition_revision": record.definition_revision,
                    "name": record.name,
                    "phase": record.phase,
                    "phase_name": phase_name(record.phase),
                    "flags": record.flags,
                    "enabled": record.flags & 4 != 0,
                    "desired_running": record.flags & 8 != 0,
                    "restart": record.restart,
                    "attempt": record.attempt,
                    "last_running_attempt": record.last_running_attempt,
                    "task_id": record.task_id,
                    "output_sequence": record.output_sequence,
                    "next_start_unix_ms": record.next_start_unix_ms,
                    "hash": hex(&record.hash),
                })
            );
        } else {
            println!(
                "{}\t{}\t{}\t{}\t{}\t{}\t{}",
                format_id(record.extension_id),
                record.name,
                record.definition_revision,
                phase_name(record.phase),
                restart_name(record.restart),
                record.attempt,
                hex(&record.hash),
            );
        }
    }
}

fn ensure_ok(status: &StatusRecord, operation: &str) -> Result<(), String> {
    if status.status == STATUS_OK {
        return Ok(());
    }
    Err(format!(
        "{operation} failed: {}{}",
        status_text(status.status),
        if status.detail.is_empty() {
            String::new()
        } else {
            format!(": {}", status.detail)
        }
    ))
}

fn action_name(action: u8) -> &'static str {
    match action {
        EXT_CONTROL_CANCEL => "cancel",
        EXT_CONTROL_ATTACH => "attach",
        EXT_CONTROL_STATUS => "status",
        EXT_CONTROL_RESTART => "restart",
        EXT_CONTROL_ENABLE => "enable",
        EXT_CONTROL_DISABLE => "disable",
        EXT_CONTROL_REMOVE => "remove",
        _ => "control",
    }
}

fn phase_name(phase: u8) -> &'static str {
    match phase {
        0 => "none",
        1 => "need-object",
        2 => "validating",
        3 => "queued",
        4 => "running",
        5 => "backoff",
        6 => "stopped",
        7 => "blocked",
        8 => "stopping",
        _ => "unknown",
    }
}

fn restart_name(restart: u8) -> &'static str {
    match restart {
        EXT_RESTART_NEVER => "never",
        EXT_RESTART_ON_FAILURE => "on-failure",
        EXT_RESTART_ALWAYS => "always",
        _ => "unknown",
    }
}

fn format_id(id: u64) -> String {
    format!("{id:016x}")
}

fn hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for &byte in bytes {
        out.push(DIGITS[(byte >> 4) as usize] as char);
        out.push(DIGITS[(byte & 0xf) as usize] as char);
    }
    out
}

fn validate_name(name: &str) -> Result<(), String> {
    if name.len() > blit_remote::extension::EXT_MAX_NAME || name.chars().any(char::is_control) {
        return Err(
            "extension name must be at most 255 UTF-8 bytes with no control characters".into(),
        );
    }
    Ok(())
}

fn split_invocation(invocation: Vec<OsString>) -> Result<(ModuleSource, Vec<String>), String> {
    let mut invocation = invocation.into_iter();
    let file = invocation
        .next()
        .ok_or_else(|| "missing WebAssembly module MODULE".to_string())
        .and_then(ModuleSource::parse)?;
    let args = invocation
        .map(|arg| {
            arg.into_string()
                .map_err(|_| "extension arguments must be valid UTF-8".to_string())
        })
        .collect::<Result<Vec<_>, _>>()?;
    Ok((file, args))
}

fn validate_args(args: &[String]) -> Result<(), String> {
    if args.len() > EXT_MAX_ARGS {
        return Err(format!(
            "too many extension arguments (maximum {EXT_MAX_ARGS})"
        ));
    }
    let mut total = 0usize;
    for arg in args {
        if arg.len() > EXT_MAX_ARG {
            return Err(format!("extension argument exceeds {EXT_MAX_ARG} bytes"));
        }
        total = total
            .checked_add(arg.len())
            .ok_or_else(|| "extension arguments are too large".to_string())?;
    }
    if total > EXT_MAX_ARGUMENT_BYTES {
        return Err(format!(
            "extension arguments exceed {EXT_MAX_ARGUMENT_BYTES} bytes"
        ));
    }
    Ok(())
}

enum Selector<'a> {
    Id(u64),
    Name(&'a str),
}

fn selector(text: &str) -> Result<Selector<'_>, String> {
    if let Some(id) = text.strip_prefix("id:") {
        if id.len() != 16 || !id.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return Err("extension IDs use id:<16-hex-digits>".into());
        }
        return u64::from_str_radix(id, 16)
            .map(Selector::Id)
            .map_err(|_| "invalid extension ID".into());
    }
    let name = text.strip_prefix("name:").unwrap_or(text);
    if name.is_empty() {
        return Err("extension name cannot be empty".into());
    }
    Ok(Selector::Name(name))
}

fn forced_name(text: &str) -> Result<&str, String> {
    match selector(text)? {
        Selector::Name(name) => Ok(name),
        Selector::Id(_) => Err("update requires an exact persistent name, not an ID".into()),
    }
}

async fn resolve_selector(client: &mut Client, text: &str) -> Result<u64, String> {
    match selector(text)? {
        Selector::Id(id) => Ok(id),
        Selector::Name(name) => Ok(find_selectable(&client.list().await?, name)?.extension_id),
    }
}

/// The durable definition called `name`. Only a persistent extension has one:
/// there, the name is the identity that `update` and `remove` act on.
fn find_named<'a>(
    records: &'a [ExtensionRecord],
    name: &str,
) -> Result<&'a ExtensionRecord, String> {
    records
        .iter()
        .find(|record| record.name == name && record.flags & EXT_FLAG_PERSIST != 0)
        .ok_or_else(|| format!("extension name not found: {name}"))
}

/// Anything a selector may point at: the durable definition when there is one,
/// otherwise a transient attempt whose descriptive name is unambiguous.
///
/// A transient `--name` is a label rather than an identity, so it resolves
/// only while it happens to be unique. Refusing it outright reads as "no such
/// extension" for a name `ext list` is displaying right there.
fn find_selectable<'a>(
    records: &'a [ExtensionRecord],
    name: &str,
) -> Result<&'a ExtensionRecord, String> {
    if let Ok(persistent) = find_named(records, name) {
        return Ok(persistent);
    }
    let mut matches = records.iter().filter(|record| record.name == name);
    match (matches.next(), matches.next()) {
        (Some(only), None) => Ok(only),
        (Some(_), Some(_)) => Err(format!(
            "{name} is the descriptive name of more than one extension; select it by id:"
        )),
        _ => Err(format!("extension name not found: {name}")),
    }
}

/// Where the module bytes come from. The server never learns either form:
/// `EXT_RUN` carries the BLAKE3 digest, so a locator is purely a client-side
/// convenience that ends in the same content-addressed admission path.
///
/// A URL may pin that digest in its fragment —
/// `https://install.blit.sh/systemd#<64-hex>` — which names one exact object
/// in one argument. The fragment never reaches the origin server: it is a
/// client-side assertion about the bytes, not part of the request.
#[derive(Clone, Debug, Eq, PartialEq)]
enum ModuleSource {
    File(PathBuf),
    Url { url: String, pin: Option<[u8; 32]> },
}

/// How long a module download may take before the command gives up.
const FETCH_TIMEOUT: Duration = Duration::from_secs(30);

fn parse_digest(text: &str) -> Result<[u8; 32], String> {
    if text.len() != 64 || !text.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(format!(
            "expected a 64-hex-digit BLAKE3 digest, got {text:?}"
        ));
    }
    let mut digest = [0u8; 32];
    for (index, byte) in digest.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&text[index * 2..index * 2 + 2], 16)
            .map_err(|error| format!("invalid BLAKE3 digest: {error}"))?;
    }
    Ok(digest)
}

fn format_digest(digest: &[u8; 32]) -> String {
    digest.iter().fold(String::new(), |mut text, byte| {
        use fmt::Write as _;
        let _ = write!(text, "{byte:02x}");
        text
    })
}

impl ModuleSource {
    fn parse(token: OsString) -> Result<Self, String> {
        let text = token.to_string_lossy();
        if !text.starts_with("https://") && !text.starts_with("http://") {
            return Ok(Self::File(PathBuf::from(token)));
        }
        let mut url =
            reqwest::Url::parse(&text).map_err(|error| format!("cannot parse {text}: {error}"))?;
        let pin = match url.fragment() {
            Some(fragment) => Some(
                parse_digest(fragment)
                    .map_err(|error| format!("{text}: pinned digest is invalid: {error}"))?,
            ),
            None => None,
        };
        url.set_fragment(None);
        Ok(Self::Url {
            url: url.into(),
            pin,
        })
    }

    /// The digest this source is already known to have, if any.
    ///
    /// A pinned URL can be admitted before a single byte is fetched: the
    /// server answers `EXT_RUN` from its object cache, and the download only
    /// happens if it asks for one.
    fn pinned(&self) -> Option<[u8; 32]> {
        match self {
            Self::Url { pin, .. } => *pin,
            Self::File(_) => None,
        }
    }

    async fn load(&self) -> Result<ModuleObject, String> {
        let module = match self {
            Self::File(path) => ModuleObject::read(path)?,
            Self::Url { url, .. } => ModuleObject::fetch(url).await?,
        };
        if let Some(pin) = self.pinned()
            && module.hash != pin
        {
            return Err(format!(
                "digest mismatch: pinned {} but the bytes hash to {}",
                format_digest(&pin),
                format_digest(&module.hash)
            ));
        }
        Ok(module)
    }
}

/// One module, fetched at most once however many times it is needed.
///
/// The server can ask for the bytes again mid-run — an upload can expire, and
/// an unpinned object can be evicted — so a run that started from a pinned URL
/// still has to be able to produce them later.
struct ModuleCache {
    source: ModuleSource,
    loaded: Option<ModuleObject>,
}

impl ModuleCache {
    fn new(source: ModuleSource) -> Self {
        Self {
            source,
            loaded: None,
        }
    }

    fn pinned(&self) -> Option<[u8; 32]> {
        self.source.pinned()
    }

    async fn get(&mut self) -> Result<&ModuleObject, String> {
        if self.loaded.is_none() {
            self.loaded = Some(self.source.load().await?);
        }
        Ok(self.loaded.as_ref().expect("just loaded"))
    }

    /// The digest to put in `EXT_RUN`: the pin when there is one, otherwise
    /// whatever the bytes turn out to hash to.
    async fn hash(&mut self) -> Result<[u8; 32], String> {
        match self.pinned() {
            Some(pin) => Ok(pin),
            None => Ok(self.get().await?.hash),
        }
    }
}

/// Cleartext carries no integrity, and the digest this client computes comes
/// from the very bytes an attacker would have substituted, so plain HTTP is
/// only honest when it cannot leave the machine.
fn loopback_url(url: &reqwest::Url) -> bool {
    match url.host() {
        Some(url::Host::Domain(name)) => name == "localhost",
        Some(url::Host::Ipv4(address)) => address.is_loopback(),
        Some(url::Host::Ipv6(address)) => address.is_loopback(),
        None => false,
    }
}

struct ModuleObject {
    bytes: Vec<u8>,
    hash: [u8; 32],
}

impl ModuleObject {
    async fn fetch(url: &str) -> Result<Self, String> {
        let parsed =
            reqwest::Url::parse(url).map_err(|error| format!("cannot parse {url}: {error}"))?;
        if parsed.scheme() == "http" && !loopback_url(&parsed) {
            return Err(format!(
                "{url}: refusing plain HTTP to a non-loopback host; use https://"
            ));
        }
        let client = reqwest::Client::builder()
            .timeout(FETCH_TIMEOUT)
            .redirect(reqwest::redirect::Policy::limited(5))
            .user_agent(concat!("blit/", env!("CARGO_PKG_VERSION")))
            .build()
            .map_err(|error| format!("cannot build an HTTP client: {error}"))?;
        let mut response = client
            .get(parsed)
            .send()
            .await
            .map_err(|error| format!("cannot fetch {url}: {error}"))?
            .error_for_status()
            .map_err(|error| format!("cannot fetch {url}: {error}"))?;
        // A redirect chain must not walk out of TLS on the way to the bytes.
        if response.url().scheme() == "http" && !loopback_url(response.url()) {
            return Err(format!(
                "{url}: redirected to plain HTTP ({}); refusing",
                response.url()
            ));
        }
        if let Some(length) = response.content_length()
            && length > EXT_MAX_MODULE
        {
            return Err(format!(
                "{url} declares {length} bytes, over the {EXT_MAX_MODULE}-byte extension limit"
            ));
        }
        // Stream with the cap enforced per chunk: a declared length is a hint,
        // not a promise, and this runs against whatever the URL names.
        let mut bytes = Vec::new();
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|error| format!("cannot read {url}: {error}"))?
        {
            if bytes.len() as u64 + chunk.len() as u64 > EXT_MAX_MODULE {
                return Err(format!(
                    "{url} exceeds the {EXT_MAX_MODULE}-byte extension limit"
                ));
            }
            bytes.extend_from_slice(&chunk);
        }
        if bytes.is_empty() {
            return Err(format!("{url} returned no bytes"));
        }
        let hash = *blake3::hash(&bytes).as_bytes();
        Ok(Self { bytes, hash })
    }

    fn read(path: &Path) -> Result<Self, String> {
        let metadata = std::fs::metadata(path)
            .map_err(|e| format!("cannot inspect {}: {e}", path.display()))?;
        if !metadata.is_file() {
            return Err(format!("{} is not a regular file", path.display()));
        }
        if metadata.len() == 0 {
            return Err(format!("{} is empty", path.display()));
        }
        if metadata.len() > EXT_MAX_MODULE {
            return Err(format!(
                "{} exceeds the {}-byte extension limit",
                path.display(),
                EXT_MAX_MODULE
            ));
        }
        let bytes =
            std::fs::read(path).map_err(|e| format!("cannot read {}: {e}", path.display()))?;
        if bytes.is_empty() || bytes.len() as u64 > EXT_MAX_MODULE {
            return Err(format!("{} changed size while it was read", path.display()));
        }
        let hash = *blake3::hash(&bytes).as_bytes();
        Ok(Self { bytes, hash })
    }
}

enum UploadOutcome {
    Available,
    Contended,
}

struct RunRequest<'a> {
    flags: u8,
    restart: u8,
    expected_id: u64,
    expected_revision: u64,
    hash: [u8; 32],
    name: &'a str,
    args: &'a [String],
}

struct Client {
    reader: Box<dyn AsyncRead + Unpin + Send>,
    writer: Box<dyn AsyncWrite + Unpin + Send>,
    fragments: FragmentReassembly,
    features: u32,
    next_nonce: u16,
}

impl Client {
    async fn connect(transport: Transport) -> Result<Self, String> {
        let (mut reader, writer) = transport.split();
        let mut fragments = FragmentReassembly::default();
        let mut features = 0u32;
        loop {
            let packet = tokio::time::timeout(
                Duration::from_secs(10),
                read_message(&mut reader, &mut fragments),
            )
            .await
            .map_err(|_| "timeout waiting for server".to_string())?
            .ok_or_else(|| "server closed connection".to_string())?;
            match packet.first().copied() {
                Some(S2C_HELLO) if packet.len() >= 7 => {
                    features = u32::from_le_bytes(packet[3..7].try_into().expect("fixed length"));
                }
                Some(S2C_READY) => break,
                Some(S2C_QUIT) => return Err("server is shutting down".into()),
                _ => {}
            }
        }
        if features & FEATURE_EXTENSION == 0 {
            return Err(
                "server does not support extensions (upgrade blit or enable extensions on the remote)"
                    .into(),
            );
        }
        Ok(Self {
            reader,
            writer,
            fragments,
            features,
            next_nonce: 1,
        })
    }

    fn nonce(&mut self) -> u16 {
        let nonce = self.next_nonce;
        self.next_nonce = self.next_nonce.wrapping_add(1);
        if self.next_nonce == 0 {
            self.next_nonce = 1;
        }
        nonce
    }

    async fn send(&mut self, packet: &[u8]) -> Result<(), String> {
        if write_frame(&mut self.writer, packet).await {
            Ok(())
        } else {
            Err("connection closed".into())
        }
    }

    async fn next_packet(&mut self) -> Result<Vec<u8>, String> {
        loop {
            let packet = read_message(&mut self.reader, &mut self.fragments)
                .await
                .ok_or_else(|| "connection closed".to_string())?;
            match packet.first().copied() {
                Some(S2C_QUIT) => return Err("server is shutting down".into()),
                None => continue,
                _ => return Ok(packet),
            }
        }
    }

    async fn run_request(&mut self, request: RunRequest<'_>) -> Result<StatusRecord, String> {
        let nonce = self.nonce();
        let packet = wire::run(nonce, &request)?;
        self.send(&packet).await?;
        self.wait_status(nonce).await
    }

    async fn send_control(&mut self, id: u64, action: u8) -> Result<u16, String> {
        let nonce = self.nonce();
        self.send(&wire::control(nonce, id, action)).await?;
        Ok(nonce)
    }

    async fn control(&mut self, id: u64, action: u8) -> Result<StatusRecord, String> {
        let nonce = self.send_control(id, action).await?;
        self.wait_status(nonce).await
    }

    async fn wait_status(&mut self, nonce: u16) -> Result<StatusRecord, String> {
        loop {
            let packet = self.next_packet().await?;
            if let wire::Message::Status(status) = wire::parse(&packet)?
                && status.nonce == nonce
            {
                return Ok(status);
            }
        }
    }

    async fn list(&mut self) -> Result<Vec<ExtensionRecord>, String> {
        let nonce = self.nonce();
        self.send(&wire::control(nonce, 0, EXT_CONTROL_LIST))
            .await?;
        loop {
            let packet = self.next_packet().await?;
            if let wire::Message::List {
                nonce: reply_nonce,
                status,
                records,
            } = wire::parse(&packet)?
                && reply_nonce == nonce
            {
                if status != STATUS_OK {
                    return Err(format!("list failed: {}", status_text(status)));
                }
                return Ok(records);
            }
        }
    }

    async fn upload(&mut self, module: &ModuleObject) -> Result<UploadOutcome, String> {
        let total = module.bytes.len() as u64;
        for (index, chunk) in module.bytes.chunks(UPLOAD_CHUNK).enumerate() {
            let offset = (index * UPLOAD_CHUNK) as u64;
            let end = offset + chunk.len() as u64;
            let mut flags = 0;
            if index == 0 {
                flags |= EXT_PUT_BEGIN;
            }
            if end == total {
                flags |= EXT_PUT_FINAL;
            }
            let nonce = self.nonce();
            self.send(&wire::put(nonce, flags, module.hash, offset, total, chunk))
                .await?;
            let reply = loop {
                let packet = self.next_packet().await?;
                if let wire::Message::PutStatus(reply) = wire::parse(&packet)?
                    && reply.nonce == nonce
                {
                    break reply;
                }
            };
            if reply.hash != module.hash {
                return Err("server replied to upload with a different hash".into());
            }
            match reply.status {
                STATUS_OK if reply.received == end => {}
                PUT_ALREADY_HAVE => return Ok(UploadOutcome::Available),
                STATUS_CONFLICT if offset == 0 => return Ok(UploadOutcome::Contended),
                STATUS_OK => {
                    return Err(format!(
                        "server acknowledged {} upload bytes, expected {end}",
                        reply.received
                    ));
                }
                status => {
                    return Err(format!(
                        "upload failed: {}{}",
                        status_text(status),
                        if reply.detail.is_empty() {
                            String::new()
                        } else {
                            format!(": {}", reply.detail)
                        }
                    ));
                }
            }
        }
        Ok(UploadOutcome::Available)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct StatusRecord {
    nonce: u16,
    status: u8,
    phase: u8,
    flags: u8,
    restart: u8,
    extension_id: u64,
    definition_revision: u64,
    attempt: u64,
    last_running_attempt: u64,
    task_id: u32,
    replay_from_sequence: u64,
    output_sequence: u64,
    next_start_unix_ms: u64,
    hash: [u8; 32],
    detail: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct ExtensionRecord {
    extension_id: u64,
    definition_revision: u64,
    phase: u8,
    flags: u8,
    restart: u8,
    attempt: u64,
    last_running_attempt: u64,
    task_id: u32,
    output_sequence: u64,
    next_start_unix_ms: u64,
    hash: [u8; 32],
    name: String,
}

#[derive(Debug)]
struct PutStatus {
    nonce: u16,
    status: u8,
    hash: [u8; 32],
    received: u64,
    detail: String,
}

#[derive(Debug)]
struct EventRecord {
    extension_id: u64,
    definition_revision: u64,
    attempt: u64,
    task_id: u32,
    output_sequence: u64,
    kind: u8,
    data: Vec<u8>,
}

#[derive(Debug)]
struct ExitRecord {
    extension_id: u64,
    definition_revision: u64,
    attempt: u64,
    task_id: u32,
    output_sequence: u64,
    reason: u8,
    code: i32,
    next_start_unix_ms: u64,
    detail: String,
}

mod wire {
    use super::*;
    use blit_remote::extension::{EXT_EXIT, EXT_INFO_INIT, EXT_PUT_STATUS};

    const INFO_LIST: u8 = 2;
    const INFO_STATUS: u8 = 3;
    const INFO_REPLAY_DONE: u8 = 6;

    pub(super) enum Message {
        Status(StatusRecord),
        PutStatus(PutStatus),
        List {
            nonce: u16,
            status: u8,
            records: Vec<ExtensionRecord>,
        },
        InfoStatus(StatusRecord),
        ReplayDone {
            extension_id: u64,
            through_sequence: u64,
        },
        Event(EventRecord),
        Exit(ExitRecord),
        Unknown,
    }

    pub(super) fn run(nonce: u16, request: &RunRequest<'_>) -> Result<Vec<u8>, String> {
        let args = request
            .args
            .iter()
            .map(|arg| arg.as_bytes())
            .collect::<Vec<_>>();
        blit_remote::extension::msg_extension_run(&blit_remote::extension::ExtensionRunRequest {
            nonce,
            flags: request.flags,
            restart: request.restart,
            expected_extension_id: request.expected_id,
            expected_definition_revision: request.expected_revision,
            hash: request.hash,
            name: request.name,
            args,
        })
        .ok_or_else(|| "invalid or oversized extension run request".to_string())
    }

    pub(super) fn put(
        nonce: u16,
        flags: u8,
        hash: [u8; 32],
        offset: u64,
        total: u64,
        data: &[u8],
    ) -> Vec<u8> {
        blit_remote::extension::msg_extension_put(&blit_remote::extension::ExtensionPutRequest {
            nonce,
            flags,
            hash,
            offset,
            total_size: total,
            data,
        })
        .expect("validated extension upload chunk")
    }

    pub(super) fn control(nonce: u16, extension_id: u64, action: u8) -> Vec<u8> {
        blit_remote::extension::msg_extension_control(nonce, extension_id, action)
            .expect("validated extension control request")
    }

    pub(super) fn parse(packet: &[u8]) -> Result<Message, String> {
        // The shared codec is authoritative for bounds and scalar invariants.
        // The small match below only converts its borrowed result into the
        // CLI's owned queue-friendly records.
        match blit_remote::extension::parse_extension_message(packet) {
            Ok(Some(_)) => {}
            Ok(None) => return Ok(Message::Unknown),
            Err(blit_remote::extension::ExtensionDecodeError::NotExtension) => {
                return Ok(Message::Unknown);
            }
            Err(error) => return Err(format!("invalid extension response: {error}")),
        }
        match packet.first().copied() {
            Some(EXT_STATUS) => parse_status(packet).map(Message::Status),
            Some(EXT_PUT_STATUS) => parse_put_status(packet).map(Message::PutStatus),
            Some(EXT_INFO) => parse_info(packet),
            Some(EXT_OUTPUT_EVENT) => parse_event(packet).map(Message::Event),
            Some(EXT_EXIT) => parse_exit(packet).map(Message::Exit),
            _ => Ok(Message::Unknown),
        }
    }

    fn parse_status(packet: &[u8]) -> Result<StatusRecord, String> {
        let mut d = Decoder::new(packet, 1);
        let record = StatusRecord {
            nonce: d.u16()?,
            status: d.u8()?,
            phase: d.u8()?,
            flags: d.u8()?,
            restart: d.u8()?,
            extension_id: d.u64()?,
            definition_revision: d.u64()?,
            attempt: d.u64()?,
            last_running_attempt: d.u64()?,
            task_id: d.u32()?,
            replay_from_sequence: d.u64()?,
            output_sequence: d.u64()?,
            next_start_unix_ms: d.u64()?,
            hash: d.hash()?,
            detail: d.detail()?,
        };
        Ok(record)
    }

    fn parse_put_status(packet: &[u8]) -> Result<PutStatus, String> {
        let mut d = Decoder::new(packet, 1);
        Ok(PutStatus {
            nonce: d.u16()?,
            status: d.u8()?,
            hash: d.hash()?,
            received: d.u64()?,
            detail: d.detail()?,
        })
    }

    fn parse_info(packet: &[u8]) -> Result<Message, String> {
        let Some(&kind) = packet.get(1) else {
            return Err("truncated EXT_INFO packet".into());
        };
        match kind {
            EXT_INFO_INIT => Ok(Message::Unknown),
            INFO_LIST => parse_list(packet),
            INFO_STATUS => parse_info_status(packet).map(Message::InfoStatus),
            INFO_REPLAY_DONE => {
                let mut d = Decoder::new(packet, 2);
                let extension_id = d.u64()?;
                let through_sequence = d.u64()?;
                d.finish()?;
                Ok(Message::ReplayDone {
                    extension_id,
                    through_sequence,
                })
            }
            _ => Ok(Message::Unknown),
        }
    }

    fn parse_list(packet: &[u8]) -> Result<Message, String> {
        let mut d = Decoder::new(packet, 2);
        let nonce = d.u16()?;
        let status = d.u8()?;
        let count = d.u16()? as usize;
        let mut records = Vec::with_capacity(count);
        for _ in 0..count {
            let extension_id = d.u64()?;
            let definition_revision = d.u64()?;
            let phase = d.u8()?;
            let flags = d.u8()?;
            let restart = d.u8()?;
            let attempt = d.u64()?;
            let last_running_attempt = d.u64()?;
            let task_id = d.u32()?;
            let output_sequence = d.u64()?;
            let next_start_unix_ms = d.u64()?;
            let hash = d.hash()?;
            let name_len = d.u16()? as usize;
            if name_len > blit_remote::extension::EXT_MAX_NAME {
                return Err("extension list name exceeds the protocol limit".into());
            }
            let name = d.text(name_len)?.to_string();
            if name.chars().any(char::is_control) {
                return Err("extension list name contains a control character".into());
            }
            records.push(ExtensionRecord {
                extension_id,
                definition_revision,
                phase,
                flags,
                restart,
                attempt,
                last_running_attempt,
                task_id,
                output_sequence,
                next_start_unix_ms,
                hash,
                name,
            });
        }
        d.finish()?;
        Ok(Message::List {
            nonce,
            status,
            records,
        })
    }

    fn parse_info_status(packet: &[u8]) -> Result<StatusRecord, String> {
        let mut d = Decoder::new(packet, 2);
        Ok(StatusRecord {
            nonce: 0,
            status: STATUS_OK,
            extension_id: d.u64()?,
            definition_revision: d.u64()?,
            phase: d.u8()?,
            flags: d.u8()?,
            restart: d.u8()?,
            attempt: d.u64()?,
            last_running_attempt: d.u64()?,
            task_id: d.u32()?,
            replay_from_sequence: 0,
            output_sequence: d.u64()?,
            next_start_unix_ms: d.u64()?,
            hash: d.hash()?,
            detail: d.detail()?,
        })
    }

    fn parse_event(packet: &[u8]) -> Result<EventRecord, String> {
        let mut d = Decoder::new(packet, 1);
        let event = EventRecord {
            extension_id: d.u64()?,
            definition_revision: d.u64()?,
            attempt: d.u64()?,
            task_id: d.u32()?,
            output_sequence: d.u64()?,
            kind: d.u8()?,
            data: d.rest().to_vec(),
        };
        if event.data.len() > blit_remote::extension::EXT_MAX_EVENT {
            return Err("extension event exceeds the protocol limit".into());
        }
        Ok(event)
    }

    fn parse_exit(packet: &[u8]) -> Result<ExitRecord, String> {
        let mut d = Decoder::new(packet, 1);
        Ok(ExitRecord {
            extension_id: d.u64()?,
            definition_revision: d.u64()?,
            attempt: d.u64()?,
            task_id: d.u32()?,
            output_sequence: d.u64()?,
            reason: d.u8()?,
            code: d.i32()?,
            next_start_unix_ms: d.u64()?,
            detail: d.detail()?,
        })
    }

    struct Decoder<'a> {
        bytes: &'a [u8],
        offset: usize,
    }

    impl<'a> Decoder<'a> {
        fn new(bytes: &'a [u8], offset: usize) -> Self {
            Self { bytes, offset }
        }

        fn take(&mut self, len: usize) -> Result<&'a [u8], String> {
            let end = self
                .offset
                .checked_add(len)
                .ok_or_else(|| "extension response length overflow".to_string())?;
            let value = self
                .bytes
                .get(self.offset..end)
                .ok_or_else(|| "truncated extension response".to_string())?;
            self.offset = end;
            Ok(value)
        }

        fn u8(&mut self) -> Result<u8, String> {
            Ok(self.take(1)?[0])
        }

        fn u16(&mut self) -> Result<u16, String> {
            Ok(u16::from_le_bytes(
                self.take(2)?.try_into().expect("fixed length"),
            ))
        }

        fn u32(&mut self) -> Result<u32, String> {
            Ok(u32::from_le_bytes(
                self.take(4)?.try_into().expect("fixed length"),
            ))
        }

        fn u64(&mut self) -> Result<u64, String> {
            Ok(u64::from_le_bytes(
                self.take(8)?.try_into().expect("fixed length"),
            ))
        }

        fn i32(&mut self) -> Result<i32, String> {
            Ok(i32::from_le_bytes(
                self.take(4)?.try_into().expect("fixed length"),
            ))
        }

        fn hash(&mut self) -> Result<[u8; 32], String> {
            Ok(self.take(32)?.try_into().expect("fixed length"))
        }

        fn text(&mut self, len: usize) -> Result<&'a str, String> {
            std::str::from_utf8(self.take(len)?)
                .map_err(|_| "extension response contains invalid UTF-8".to_string())
        }

        fn detail(&mut self) -> Result<String, String> {
            let len = self.bytes.len() - self.offset;
            if len > EXT_MAX_DETAIL {
                return Err("extension response detail exceeds the protocol limit".into());
            }
            let detail = self.text(len)?.to_string();
            self.finish()?;
            Ok(detail)
        }

        fn rest(&mut self) -> &'a [u8] {
            let rest = &self.bytes[self.offset..];
            self.offset = self.bytes.len();
            rest
        }

        fn finish(&self) -> Result<(), String> {
            if self.offset == self.bytes.len() {
                Ok(())
            } else {
                Err("extension response has trailing bytes".into())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transport::{read_frame, write_frame};
    use blit_remote::extension::{
        EXT_FLAG_DESIRED_RUNNING, EXT_FLAG_DETACH, EXT_FLAG_ENABLED, EXT_PHASE_NEED_OBJECT,
        EXT_PHASE_RUNNING, EXT_PHASE_STOPPED, ExtensionExit, ExtensionInfoStatus,
        ExtensionPutStatus, ExtensionRequest, ExtensionStatus, msg_extension_exit,
        msg_extension_info_status, msg_extension_put_status, msg_extension_replay_done,
        msg_extension_status, parse_extension_request,
    };
    use clap::Parser;

    fn follow_status(phase: u8, replay_from_sequence: u64, output_sequence: u64) -> StatusRecord {
        StatusRecord {
            nonce: 1,
            status: STATUS_OK,
            phase,
            flags: EXT_FLAG_DETACH | EXT_FLAG_ENABLED | EXT_FLAG_DESIRED_RUNNING,
            restart: EXT_RESTART_NEVER,
            extension_id: 9,
            definition_revision: 1,
            attempt: 2,
            last_running_attempt: 2,
            task_id: if phase == EXT_PHASE_RUNNING { 17 } else { 0 },
            replay_from_sequence,
            output_sequence,
            next_start_unix_ms: 0,
            hash: [7; 32],
            detail: String::new(),
        }
    }

    fn info_status(phase: u8, output_sequence: u64) -> Vec<u8> {
        msg_extension_info_status(&ExtensionInfoStatus {
            extension_id: 9,
            definition_revision: 1,
            phase,
            flags: EXT_FLAG_DETACH | EXT_FLAG_ENABLED | EXT_FLAG_DESIRED_RUNNING,
            restart: EXT_RESTART_NEVER,
            attempt: 2,
            last_running_attempt: 2,
            task_id: if phase == EXT_PHASE_RUNNING { 17 } else { 0 },
            output_sequence,
            next_start_unix_ms: 0,
            hash: [7; 32],
            detail: "",
        })
        .unwrap()
    }

    fn exit(output_sequence: u64, code: i32) -> Vec<u8> {
        msg_extension_exit(&ExtensionExit {
            extension_id: 9,
            definition_revision: 1,
            attempt: 2,
            task_id: 17,
            output_sequence,
            reason: 0,
            code,
            next_start_unix_ms: 0,
            detail: "",
        })
        .unwrap()
    }

    async fn connected_test_client() -> (Client, tokio::io::DuplexStream) {
        let (client_io, mut server_io) = tokio::io::duplex(4096);
        let mut hello = vec![S2C_HELLO, 0, 0];
        hello.extend_from_slice(&FEATURE_EXTENSION.to_le_bytes());
        assert!(write_frame(&mut server_io, &hello).await);
        assert!(write_frame(&mut server_io, &[S2C_READY]).await);
        let client = Client::connect(Transport::Duplex(client_io)).await.unwrap();
        (client, server_io)
    }

    #[test]
    fn run_wire_matches_remote_request_decoder() {
        let args = vec!["one".to_string(), "--flag".to_string()];
        let request = RunRequest {
            flags: EXT_RUN_DETACH,
            restart: EXT_RESTART_ON_FAILURE,
            expected_id: 0,
            expected_revision: 0,
            hash: [7; 32],
            name: "worker",
            args: &args,
        };
        let packet = wire::run(42, &request).unwrap();
        let ExtensionRequest::Run {
            nonce,
            flags,
            restart,
            hash,
            name,
            args: decoded_args,
            ..
        } = parse_extension_request(&packet).unwrap().unwrap()
        else {
            panic!("not a run request");
        };
        assert_eq!(nonce, 42);
        assert_eq!(flags, EXT_RUN_DETACH);
        assert_eq!(restart, EXT_RESTART_ON_FAILURE);
        assert_eq!(hash, [7; 32]);
        assert_eq!(name, "worker");
        assert_eq!(decoded_args, vec![b"one".as_slice(), b"--flag".as_slice()]);
    }

    #[test]
    fn run_and_alias_treat_every_token_after_file_as_guest_arguments() {
        let namespaced = crate::cli::Cli::try_parse_from([
            "blit",
            "--on",
            "tcp:server:1",
            "--hub",
            "https://server-hub.example",
            "ext",
            "run",
            "--detach",
            "--restart",
            "on-failure",
            "labelled",
            "module.wasm",
            "--on",
            "guest-target",
            "--hub",
            "guest-hub",
            "--json",
            "--restart",
            "always",
        ])
        .unwrap();
        let alias = crate::cli::Cli::try_parse_from([
            "blit",
            "--on",
            "tcp:server:1",
            "--hub",
            "https://server-hub.example",
            "run",
            "--detach",
            "--restart",
            "on-failure",
            "labelled",
            "module.wasm",
            "--on",
            "guest-target",
            "--hub",
            "guest-hub",
            "--json",
            "--restart",
            "always",
        ])
        .unwrap();
        let extract = |command| match command {
            crate::cli::Command::Extension {
                command: ExtensionCommand::Run(args),
            }
            | crate::cli::Command::Run(args) => args,
            _ => panic!("wrong command"),
        };
        assert_eq!(namespaced.connect.on.as_deref(), Some("tcp:server:1"));
        assert_eq!(alias.connect.on, namespaced.connect.on);
        assert_eq!(
            namespaced.connect.hub,
            "https://server-hub.example".to_string()
        );
        assert_eq!(alias.connect.hub, namespaced.connect.hub);
        let first = extract(namespaced.command);
        let second = extract(alias.command);
        assert_eq!(second.detach, first.detach);
        assert_eq!(first.restart, RestartPolicy::OnFailure);
        assert!(!first.json);
        let first = split_invocation(first.invocation).unwrap();
        let second = split_invocation(second.invocation).unwrap();
        assert_eq!(second, first);
        assert_eq!(first.0, ModuleSource::File(PathBuf::from("module.wasm")));
        assert_eq!(
            first.1,
            [
                "--on",
                "guest-target",
                "--hub",
                "guest-hub",
                "--json",
                "--restart",
                "always"
            ]
        );
    }

    #[test]
    fn update_treats_options_after_file_as_guest_arguments() {
        let cli = crate::cli::Cli::try_parse_from([
            "blit",
            "ext",
            "update",
            "worker",
            "--on",
            "tcp:server:1",
            "--json",
            "--restart",
            "on-failure",
            "module.wasm",
            "--on",
            "guest-target",
            "--hub",
            "guest-hub",
            "--json",
            "--restart",
            "always",
        ])
        .unwrap();
        assert_eq!(cli.connect.on.as_deref(), Some("tcp:server:1"));
        let crate::cli::Command::Extension {
            command: ExtensionCommand::Update(args),
        } = cli.command
        else {
            panic!("wrong command");
        };
        assert_eq!(args.name, "worker");
        assert!(args.json);
        assert_eq!(args.restart, Some(RestartPolicy::OnFailure));
        let (file, args) = split_invocation(args.invocation).unwrap();
        assert_eq!(file, ModuleSource::File(PathBuf::from("module.wasm")));
        assert_eq!(
            args,
            [
                "--on",
                "guest-target",
                "--hub",
                "guest-hub",
                "--json",
                "--restart",
                "always"
            ]
        );
    }

    #[test]
    fn module_sources_split_urls_from_paths() {
        assert_eq!(
            ModuleSource::parse(OsString::from("https://install.blit.sh/systemd")).unwrap(),
            ModuleSource::Url {
                url: "https://install.blit.sh/systemd".into(),
                pin: None
            }
        );
        // Only these two schemes are locators; everything else is a path, so a
        // Windows drive letter and a relative name keep working.
        for path in ["module.wasm", "./module.wasm", "C:\\modules\\a.wasm"] {
            assert_eq!(
                ModuleSource::parse(OsString::from(path)).unwrap(),
                ModuleSource::File(PathBuf::from(path))
            );
        }
    }

    #[test]
    fn url_fragments_pin_one_exact_object() {
        let digest = "1a3baedf416f2b0f9b6cd683a01d8408a1c6928ba698c0533fdd81aca8fc7e2c";
        let source = ModuleSource::parse(OsString::from(format!(
            "https://install.blit.sh/systemd#{digest}"
        )))
        .unwrap();
        // The fragment is a client-side assertion, so it must not survive into
        // the request the origin server sees.
        assert_eq!(
            source,
            ModuleSource::Url {
                url: "https://install.blit.sh/systemd".into(),
                pin: Some(parse_digest(digest).unwrap()),
            }
        );
        assert_eq!(format_digest(&source.pinned().unwrap()), digest);
        assert_eq!(
            ModuleSource::parse(OsString::from("https://install.blit.sh/systemd"))
                .unwrap()
                .pinned(),
            None
        );
        for bad in [
            "#",
            "#deadbeef",
            "#zz",
            "#1a3baedf416f2b0f9b6cd683a01d8408a1c6928ba6",
        ] {
            assert!(
                ModuleSource::parse(OsString::from(format!("https://install.blit.sh/x{bad}")))
                    .is_err()
            );
        }
        // A path is never reinterpreted: '#' is a legal filename byte.
        assert_eq!(
            ModuleSource::parse(OsString::from("./mod#1.wasm")).unwrap(),
            ModuleSource::File(PathBuf::from("./mod#1.wasm"))
        );
    }

    #[test]
    fn plain_http_is_only_trusted_on_loopback() {
        let loopback = ["http://localhost:8080/a.wasm", "http://127.0.0.1/a.wasm"];
        for url in loopback {
            assert!(loopback_url(&reqwest::Url::parse(url).unwrap()));
        }
        for url in ["http://install.blit.sh/a.wasm", "http://10.0.0.1/a.wasm"] {
            assert!(!loopback_url(&reqwest::Url::parse(url).unwrap()));
        }
    }

    #[test]
    fn selectors_reach_transient_attempts_by_their_label() {
        let record = |extension_id: u64, name: &str, flags: u8| ExtensionRecord {
            extension_id,
            definition_revision: 1,
            phase: PHASE_RUNNING,
            flags,
            restart: EXT_RESTART_NEVER,
            attempt: 1,
            last_running_attempt: 1,
            task_id: 0,
            output_sequence: 0,
            next_start_unix_ms: 0,
            hash: [0; 32],
            name: String::from(name),
        };
        // A durable definition owns its name even while it is stopped.
        let mixed = [
            record(1, "systemd", EXT_FLAG_DETACH),
            record(2, "systemd", EXT_FLAG_PERSIST),
        ];
        assert_eq!(find_selectable(&mixed, "systemd").unwrap().extension_id, 2);

        // With no definition, an unambiguous label still resolves: `ext list`
        // prints that name, so refusing it reads as "no such extension".
        let transient = [record(7, "systemd", EXT_FLAG_DETACH)];
        assert_eq!(
            find_selectable(&transient, "systemd").unwrap().extension_id,
            7
        );
        assert!(find_named(&transient, "systemd").is_err());

        let twins = [
            record(7, "systemd", EXT_FLAG_DETACH),
            record(8, "systemd", EXT_FLAG_DETACH),
        ];
        assert!(
            find_selectable(&twins, "systemd")
                .unwrap_err()
                .contains("more than one")
        );
        assert!(
            find_selectable(&twins, "other")
                .unwrap_err()
                .contains("not found")
        );
    }

    #[test]
    fn selectors_never_guess_bare_hex_as_an_id() {
        assert!(matches!(
            selector("0123456789abcdef").unwrap(),
            Selector::Name(_)
        ));
        assert!(matches!(
            selector("id:0123456789abcdef").unwrap(),
            Selector::Id(0x0123_4567_89ab_cdef)
        ));
        assert!(matches!(
            selector("name:id:foo").unwrap(),
            Selector::Name("id:foo")
        ));
        assert!(selector("id:12").is_err());
    }

    #[test]
    fn list_response_decoder_is_bounded_and_exact() {
        let mut packet = vec![EXT_INFO, 2];
        packet.extend_from_slice(&9u16.to_le_bytes());
        packet.push(STATUS_OK);
        packet.extend_from_slice(&1u16.to_le_bytes());
        packet.extend_from_slice(&4u64.to_le_bytes());
        packet.extend_from_slice(&8u64.to_le_bytes());
        packet.extend_from_slice(&[PHASE_RUNNING, 0x0f, EXT_RESTART_ALWAYS]);
        packet.extend_from_slice(&3u64.to_le_bytes());
        packet.extend_from_slice(&2u64.to_le_bytes());
        packet.extend_from_slice(&7u32.to_le_bytes());
        packet.extend_from_slice(&11u64.to_le_bytes());
        packet.extend_from_slice(&0u64.to_le_bytes());
        packet.extend_from_slice(&[5; 32]);
        packet.extend_from_slice(&6u16.to_le_bytes());
        packet.extend_from_slice(b"worker");
        let wire::Message::List {
            nonce,
            status,
            records,
        } = wire::parse(&packet).unwrap()
        else {
            panic!("not a list response");
        };
        assert_eq!(nonce, 9);
        assert_eq!(status, STATUS_OK);
        assert_eq!(records[0].name, "worker");
        assert_eq!(records[0].extension_id, 4);
        packet.push(0);
        assert!(wire::parse(&packet).is_err());
    }

    #[test]
    fn status_decoder_preserves_signed_exit_code_and_full_hash() {
        let mut packet = vec![blit_remote::extension::EXT_EXIT];
        packet.extend_from_slice(&4u64.to_le_bytes());
        packet.extend_from_slice(&8u64.to_le_bytes());
        packet.extend_from_slice(&3u64.to_le_bytes());
        packet.extend_from_slice(&7u32.to_le_bytes());
        packet.extend_from_slice(&11u64.to_le_bytes());
        packet.push(0);
        packet.extend_from_slice(&(-1234i32).to_le_bytes());
        packet.extend_from_slice(&0u64.to_le_bytes());
        let wire::Message::Exit(exit) = wire::parse(&packet).unwrap() else {
            panic!("not an exit response");
        };
        assert_eq!(exit.code, -1234);
        assert_eq!(exit.output_sequence, 11);
    }

    #[tokio::test]
    async fn attach_stopped_snapshot_waits_for_replay_done() {
        let (mut client, mut server_io) = connected_test_client().await;
        let status = follow_status(EXT_PHASE_STOPPED, 1, 2);
        let task = tokio::spawn(async move {
            follow(&mut client, 9, status, false, FollowMode::Attach, None).await
        });

        tokio::time::sleep(Duration::from_millis(20)).await;
        assert!(!task.is_finished(), "attach returned before replay started");
        assert!(write_frame(&mut server_io, &exit(1, 23)).await);
        assert!(write_frame(&mut server_io, &info_status(EXT_PHASE_STOPPED, 2)).await);
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert!(!task.is_finished(), "attach returned before REPLAY_DONE");

        assert!(write_frame(&mut server_io, &msg_extension_replay_done(9, 2).unwrap()).await);
        let code = tokio::time::timeout(Duration::from_secs(1), task)
            .await
            .expect("attach completed after REPLAY_DONE")
            .unwrap()
            .unwrap();
        assert_eq!(code, 0, "historical EXIT replaced current exit state");
    }

    #[tokio::test]
    async fn attach_running_snapshot_ignores_historical_stopped_status() {
        let (mut client, mut server_io) = connected_test_client().await;
        let status = follow_status(EXT_PHASE_RUNNING, 1, 2);
        let task = tokio::spawn(async move {
            follow(&mut client, 9, status, false, FollowMode::Attach, None).await
        });

        assert!(write_frame(&mut server_io, &exit(1, 23)).await);
        assert!(write_frame(&mut server_io, &info_status(EXT_PHASE_STOPPED, 2)).await);
        assert!(write_frame(&mut server_io, &msg_extension_replay_done(9, 2).unwrap()).await);
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert!(
            !task.is_finished(),
            "historical STOPPED regressed the running snapshot"
        );

        assert!(write_frame(&mut server_io, &exit(3, 29)).await);
        assert!(write_frame(&mut server_io, &info_status(EXT_PHASE_STOPPED, 4)).await);
        let code = tokio::time::timeout(Duration::from_secs(1), task)
            .await
            .expect("live STOPPED completed attach")
            .unwrap()
            .unwrap();
        assert_eq!(code, 29);
    }

    #[tokio::test]
    async fn request_reply_flow_uploads_only_after_need_object() {
        let (client_io, mut server_io) = tokio::io::duplex(4096);
        let server = tokio::spawn(async move {
            let mut hello = vec![S2C_HELLO, 0, 0];
            hello.extend_from_slice(&FEATURE_EXTENSION.to_le_bytes());
            assert!(write_frame(&mut server_io, &hello).await);
            assert!(write_frame(&mut server_io, &[S2C_READY]).await);

            let run = read_frame(&mut server_io).await.unwrap();
            let ExtensionRequest::Run {
                nonce, hash, flags, ..
            } = parse_extension_request(&run).unwrap().unwrap()
            else {
                panic!("not a run request");
            };
            assert_eq!(flags, EXT_RUN_DETACH);
            let response = msg_extension_status(&ExtensionStatus {
                nonce,
                status: STATUS_OK,
                phase: EXT_PHASE_NEED_OBJECT,
                flags: EXT_FLAG_DETACH | EXT_FLAG_ENABLED | EXT_FLAG_DESIRED_RUNNING,
                restart: EXT_RESTART_NEVER,
                extension_id: 9,
                definition_revision: 1,
                attempt: 0,
                last_running_attempt: 0,
                task_id: 0,
                replay_from_sequence: 0,
                output_sequence: 0,
                next_start_unix_ms: 0,
                hash,
                detail: "",
            })
            .unwrap();
            assert!(write_frame(&mut server_io, &response).await);

            let put = read_frame(&mut server_io).await.unwrap();
            let ExtensionRequest::Put {
                nonce,
                flags,
                hash,
                offset,
                total_size,
                data,
            } = parse_extension_request(&put).unwrap().unwrap()
            else {
                panic!("not an upload request");
            };
            assert_eq!(flags, EXT_PUT_BEGIN | EXT_PUT_FINAL);
            assert_eq!(offset, 0);
            assert_eq!(total_size, 3);
            assert_eq!(data, b"was");
            let response = msg_extension_put_status(&ExtensionPutStatus {
                nonce,
                status: STATUS_OK,
                hash,
                received: 3,
                detail: "",
            })
            .unwrap();
            assert!(write_frame(&mut server_io, &response).await);
        });

        let mut client = Client::connect(Transport::Duplex(client_io)).await.unwrap();
        let module = ModuleObject {
            bytes: b"was".to_vec(),
            hash: *blake3::hash(b"was").as_bytes(),
        };
        let status = client
            .run_request(RunRequest {
                flags: EXT_RUN_DETACH,
                restart: EXT_RESTART_NEVER,
                expected_id: 0,
                expected_revision: 0,
                hash: module.hash,
                name: "",
                args: &[],
            })
            .await
            .unwrap();
        assert_eq!(status.phase, PHASE_NEED_OBJECT);
        assert!(matches!(
            client.upload(&module).await.unwrap(),
            UploadOutcome::Available
        ));
        server.await.unwrap();
    }
}
