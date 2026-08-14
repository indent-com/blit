//! Live `@name` command discovery and `blit.cli.v1` invocation.

use super::Client;
use blit_remote::{
    STATUS_OK,
    channel::{
        self as channel_wire, CHANNEL_CLOSE_CANCELLED, CHANNEL_CLOSE_NORMAL, CHANNEL_MAX_PAYLOAD,
        CHANNEL_MAX_UNCONSUMED_MESSAGES, ChannelMessage, FEATURE_CHANNEL,
    },
    extension::{
        self as extension_wire, EXT_MAX_ARG, EXT_MAX_ARGS, EXT_MAX_ARGUMENT_BYTES, ExtensionInfo,
        ExtensionMessage, FEATURE_EXTENSION,
    },
    status_text,
};
use serde_json::Value;
use std::{
    collections::{BTreeMap, BTreeSet, HashSet, VecDeque},
    future::Future,
    io::IsTerminal,
};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

const CHANNEL_ID: u32 = 2;
const STDIN_CHUNK: usize = 64 * 1024;

const C2S_INVOKE: u8 = 1;
const C2S_STDIN: u8 = 2;
const C2S_STDIN_EOF: u8 = 3;
const C2S_CANCEL: u8 = 4;
const INVOKE_FLAG_STDIN: u8 = 1;

const S2C_STDOUT: u8 = 1;
const S2C_STDERR: u8 = 2;
const S2C_LOG: u8 = 3;
const S2C_RESULT: u8 = 4;
const S2C_EXIT: u8 = 5;

#[derive(Clone, Debug, Eq, PartialEq)]
struct DirectoryRecord {
    name: String,
    listener_name: String,
    listener_token: [u8; 16],
    descriptor: Descriptor,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct Descriptor {
    summary: String,
    commands: Vec<DescriptorCommand>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct DescriptorCommand {
    path: Vec<String>,
    summary: Option<String>,
    usage: Option<String>,
    options: Vec<DescriptorOption>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct DescriptorOption {
    names: Vec<String>,
    takes_value: bool,
    help: Option<String>,
}

pub(super) fn parse_external(tokens: Vec<String>) -> Result<(String, Vec<String>), String> {
    let Some(namespace) = tokens.first() else {
        return Err("missing extension command namespace".into());
    };
    let Some(name) = namespace.strip_prefix('@') else {
        return Err(format!(
            "unknown command `{namespace}` (extension commands use @name)"
        ));
    };
    if name.is_empty()
        || name.len() > extension_wire::EXT_MAX_NAME
        || name.chars().any(char::is_control)
    {
        return Err(
            "extension command namespace must be @ followed by at most 255 UTF-8 bytes with no control characters"
                .into(),
        );
    }
    let name = name.to_string();
    let args = tokens.into_iter().skip(1).collect::<Vec<_>>();
    validate_invocation_args(&args)?;
    Ok((name, args))
}

pub(super) async fn list(client: &mut Client) -> Result<i32, String> {
    for record in discover(client).await? {
        println!("@{}\t{}", record.name, sanitize(&record.descriptor.summary));
    }
    Ok(0)
}

pub(super) async fn complete(
    client: &mut Client,
    words: &[String],
    current: &str,
) -> Result<Vec<String>, String> {
    let records = discover(client).await?;
    Ok(completion_candidates(&records, words, current))
}

pub(super) async fn invoke(
    client: &mut Client,
    name: &str,
    args: Vec<String>,
    json: bool,
) -> Result<i32, String> {
    let streams_stdin = !std::io::stdin().is_terminal();
    let stdin = tokio::io::stdin();
    let stdout = tokio::io::stdout();
    let stderr = tokio::io::stderr();
    let cancellation = async {
        tokio::signal::ctrl_c()
            .await
            .map_err(|error| format!("cannot listen for Ctrl-C: {error}"))
    };
    invoke_with_io(
        client,
        name,
        args,
        json,
        InvocationIo {
            streams_stdin,
            stdin,
            stdout,
            stderr,
            cancellation,
        },
    )
    .await
}

struct InvocationIo<R, O, E, C> {
    streams_stdin: bool,
    stdin: R,
    stdout: O,
    stderr: E,
    cancellation: C,
}

async fn invoke_with_io<R, O, E, C>(
    client: &mut Client,
    name: &str,
    args: Vec<String>,
    json: bool,
    mut io: InvocationIo<R, O, E, C>,
) -> Result<i32, String>
where
    R: AsyncRead + Unpin,
    O: AsyncWrite + Unpin,
    E: AsyncWrite + Unpin,
    C: Future<Output = Result<(), String>>,
{
    validate_invocation_args(&args)?;
    let record = discover(client)
        .await?
        .into_iter()
        .find(|record| record.name == name)
        .ok_or_else(|| format!("extension command namespace not found: @{name}"))?;

    if let Some(help) = local_help(&record, &args) {
        io.stdout
            .write_all(help.as_bytes())
            .await
            .map_err(|error| format!("cannot write command help: {error}"))?;
        io.stdout
            .flush()
            .await
            .map_err(|error| format!("cannot flush command help: {error}"))?;
        return Ok(0);
    }

    let invoke_payload = encode_invoke(&args, io.streams_stdin)?;
    run_invocation(client, &record, invoke_payload, json, io).await
}

async fn discover(client: &mut Client) -> Result<Vec<DirectoryRecord>, String> {
    require_command_features(client)?;
    let mut requested_revision = 0u64;
    let mut cursor = 0u64;
    let mut snapshot_revision = None;
    let mut seen_cursors = HashSet::from([0u64]);
    let mut records = BTreeMap::new();

    loop {
        let nonce = client.nonce();
        let request =
            extension_wire::msg_extension_command_discover(nonce, requested_revision, cursor)
                .ok_or_else(|| "invalid extension command discovery cursor".to_string())?;
        client.send(&request).await?;

        let (status, directory_revision, next_cursor, page) = loop {
            let packet = client.next_packet().await?;
            match extension_wire::parse_extension_message(&packet) {
                Ok(Some(ExtensionMessage::Info(ExtensionInfo::Commands {
                    nonce: reply_nonce,
                    status,
                    directory_revision,
                    next_cursor,
                    records,
                }))) if reply_nonce == nonce => {
                    let records = records
                        .into_iter()
                        .map(|record| {
                            (
                                record.name.to_string(),
                                record.listener_name.to_string(),
                                record.listener_token,
                                record.descriptor.to_string(),
                            )
                        })
                        .collect::<Vec<_>>();
                    break (status, directory_revision, next_cursor, records);
                }
                Ok(_) | Err(extension_wire::ExtensionDecodeError::NotExtension) => {}
                Err(error) => {
                    return Err(format!(
                        "invalid extension command directory response: {error}"
                    ));
                }
            }
        };

        if status != STATUS_OK {
            return Err(format!(
                "extension command discovery failed: {}",
                status_text(status)
            ));
        }
        if let Some(expected) = snapshot_revision {
            if directory_revision != expected {
                return Err("extension command directory revision changed during discovery".into());
            }
        } else {
            snapshot_revision = Some(directory_revision);
        }

        for (name, listener_name, listener_token, descriptor) in page {
            let owned = DirectoryRecord {
                name: name.clone(),
                listener_name,
                listener_token,
                descriptor: parse_descriptor(&descriptor).map_err(|error| {
                    format!("server advertised an invalid descriptor for @{name}: {error}")
                })?,
            };
            if records.insert(name.clone(), owned).is_some() {
                return Err(format!(
                    "server advertised duplicate extension command namespace @{name}"
                ));
            }
        }

        if next_cursor == 0 {
            return Ok(records.into_values().collect());
        }
        if directory_revision == 0 || !seen_cursors.insert(next_cursor) {
            return Err("server returned an invalid extension command discovery cursor".into());
        }
        requested_revision = directory_revision;
        cursor = next_cursor;
    }
}

fn completion_candidates(
    records: &[DirectoryRecord],
    words: &[String],
    current: &str,
) -> Vec<String> {
    let Some(command_words) = completion_command_words(words) else {
        return Vec::new();
    };
    let mut candidates = BTreeSet::new();
    let Some((namespace, arguments)) = command_words else {
        for record in records {
            if safe_namespace(&record.name) {
                let candidate = format!("@{}", record.name);
                if candidate.starts_with(current) {
                    candidates.insert(candidate);
                }
            }
        }
        return candidates.into_iter().collect();
    };
    let Some(record) = records
        .iter()
        .find(|record| record.name == namespace && safe_namespace(&record.name))
    else {
        return Vec::new();
    };
    command_completion_candidates(record, arguments, current, &mut candidates);
    candidates.into_iter().collect()
}

/// Remove only root extension-command options. This deliberately stops at
/// `@name`: later `--json`, `--on`, and `--hub` tokens are extension arguments.
fn completion_command_words(words: &[String]) -> Option<Option<(&str, &[String])>> {
    let mut index = 0usize;
    while index < words.len() {
        let word = &words[index];
        match word.as_str() {
            "--json" => index += 1,
            "--on" | "--hub" => index = index.checked_add(2)?,
            _ if word.starts_with("--on=") || word.starts_with("--hub=") => index += 1,
            _ => {
                let namespace = word.strip_prefix('@')?;
                return Some(Some((namespace, &words[index + 1..])));
            }
        }
    }
    Some(None)
}

fn command_completion_candidates(
    record: &DirectoryRecord,
    arguments: &[String],
    current: &str,
    candidates: &mut BTreeSet<String>,
) {
    let commands = record
        .descriptor
        .commands
        .iter()
        .filter(|command| command.path.iter().all(|part| safe_path_part(part)))
        .collect::<Vec<_>>();
    let mut path = Vec::<&str>::new();
    let mut path_open = true;
    let mut options_allowed = true;
    let mut used_options = HashSet::<&str>::new();
    let mut index = 0usize;

    while index < arguments.len() {
        let argument = arguments[index].as_str();
        if options_allowed && argument == "--" {
            options_allowed = false;
            path_open = false;
            index += 1;
            continue;
        }
        if options_allowed && argument.starts_with('-') {
            path_open = false;
            let (option_name, inline_value) = argument
                .split_once('=')
                .map_or((argument, false), |(name, _)| (name, true));
            if let Some(option) = exact_command(&commands, &path).and_then(|command| {
                command
                    .options
                    .iter()
                    .find(|option| option.names.iter().any(|name| name == option_name))
            }) {
                used_options.extend(option.names.iter().map(String::as_str));
                if option.takes_value && !inline_value {
                    index += 1;
                    if index == arguments.len() {
                        return;
                    }
                }
            }
            index += 1;
            continue;
        }
        if path_open
            && commands.iter().any(|command| {
                command.path.len() > path.len()
                    && command.path[..path.len()]
                        .iter()
                        .map(String::as_str)
                        .eq(path.iter().copied())
                    && command.path[path.len()] == argument
            })
        {
            path.push(argument);
        } else {
            path_open = false;
        }
        index += 1;
    }

    if path_open {
        for command in &commands {
            if command.path.len() > path.len()
                && command.path[..path.len()]
                    .iter()
                    .map(String::as_str)
                    .eq(path.iter().copied())
            {
                let candidate = &command.path[path.len()];
                if candidate.starts_with(current) {
                    candidates.insert(candidate.clone());
                }
            }
        }
    }
    if options_allowed && let Some(command) = exact_command(&commands, &path) {
        for option in &command.options {
            if option
                .names
                .iter()
                .any(|name| used_options.contains(name.as_str()))
            {
                continue;
            }
            for name in &option.names {
                if safe_option_name(name) && name.starts_with(current) {
                    candidates.insert(name.clone());
                }
            }
        }
    }
}

fn exact_command<'a>(
    commands: &[&'a DescriptorCommand],
    path: &[&str],
) -> Option<&'a DescriptorCommand> {
    commands.iter().copied().find(|command| {
        command
            .path
            .iter()
            .map(String::as_str)
            .eq(path.iter().copied())
    })
}

fn safe_namespace(value: &str) -> bool {
    safe_bare_token(value, extension_wire::EXT_MAX_NAME)
}

fn safe_path_part(value: &str) -> bool {
    safe_bare_token(value, 255)
}

fn safe_bare_token(value: &str, max: usize) -> bool {
    !value.is_empty()
        && value.len() <= max
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
        && !value.starts_with('-')
}

fn safe_option_name(value: &str) -> bool {
    let Some(body) = value.strip_prefix('-') else {
        return false;
    };
    let body = body.strip_prefix('-').unwrap_or(body);
    !body.is_empty()
        && value.len() <= 255
        && body
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
        && body.bytes().any(|byte| byte.is_ascii_alphanumeric())
}

fn require_command_features(client: &Client) -> Result<(), String> {
    let required = FEATURE_EXTENSION | FEATURE_CHANNEL;
    if client.features & required != required {
        Err(
            "server does not support extension commands (both extension and native-channel support are required)"
                .into(),
        )
    } else {
        Ok(())
    }
}

fn parse_descriptor(source: &str) -> Result<Descriptor, String> {
    let value: Value =
        serde_json::from_str(source).map_err(|error| format!("invalid JSON: {error}"))?;
    let object = value
        .as_object()
        .ok_or_else(|| "descriptor root is not an object".to_string())?;
    if object.get("protocol").and_then(Value::as_str) != Some("blit.cli.v1") {
        return Err("unsupported command protocol".into());
    }
    let summary = object
        .get("summary")
        .and_then(Value::as_str)
        .ok_or_else(|| "descriptor summary is missing".to_string())?
        .to_string();
    let command_values = object
        .get("commands")
        .and_then(Value::as_array)
        .ok_or_else(|| "descriptor commands array is missing".to_string())?;
    let commands = command_values
        .iter()
        .filter_map(parse_descriptor_command)
        .collect();
    Ok(Descriptor { summary, commands })
}

fn parse_descriptor_command(value: &Value) -> Option<DescriptorCommand> {
    let object = value.as_object()?;
    let path = object
        .get("path")?
        .as_array()?
        .iter()
        .map(|part| part.as_str().map(str::to_string))
        .collect::<Option<Vec<_>>>()?;
    let summary = object
        .get("summary")
        .and_then(Value::as_str)
        .map(str::to_string);
    let usage = object
        .get("usage")
        .and_then(Value::as_str)
        .map(str::to_string);
    let options = object
        .get("options")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(parse_descriptor_option)
        .collect();
    Some(DescriptorCommand {
        path,
        summary,
        usage,
        options,
    })
}

fn parse_descriptor_option(value: &Value) -> Option<DescriptorOption> {
    let object = value.as_object()?;
    let names = object
        .get("names")?
        .as_array()?
        .iter()
        .map(|name| name.as_str().map(str::to_string))
        .collect::<Option<Vec<_>>>()?;
    if names.is_empty() {
        return None;
    }
    Some(DescriptorOption {
        names,
        takes_value: object
            .get("takes_value")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        help: object
            .get("help")
            .and_then(Value::as_str)
            .map(str::to_string),
    })
}

fn local_help(record: &DirectoryRecord, args: &[String]) -> Option<String> {
    if args.last().map(String::as_str) != Some("--help") {
        return None;
    }
    let path = &args[..args.len() - 1];
    if path.is_empty() {
        return Some(render_root_help(record));
    }
    let command = record
        .descriptor
        .commands
        .iter()
        .find(|command| command.path == path)?;
    Some(render_command_help(record, command))
}

fn render_root_help(record: &DirectoryRecord) -> String {
    let mut output = format!(
        "@{} — {}\n",
        record.name,
        sanitize(&record.descriptor.summary)
    );
    if let Some(root) = record
        .descriptor
        .commands
        .iter()
        .find(|command| command.path.is_empty())
    {
        append_usage_options(&mut output, &record.name, root);
    }
    let commands = record
        .descriptor
        .commands
        .iter()
        .filter(|command| !command.path.is_empty())
        .collect::<Vec<_>>();
    if !commands.is_empty() {
        output.push_str("\nCommands:\n");
        for command in commands {
            output.push_str("  ");
            output.push_str(
                &command
                    .path
                    .iter()
                    .map(|part| sanitize(part))
                    .collect::<Vec<_>>()
                    .join(" "),
            );
            if let Some(summary) = &command.summary {
                output.push('\t');
                output.push_str(&sanitize(summary));
            }
            output.push('\n');
        }
    }
    output
}

fn render_command_help(record: &DirectoryRecord, command: &DescriptorCommand) -> String {
    let path = command
        .path
        .iter()
        .map(|part| sanitize(part))
        .collect::<Vec<_>>()
        .join(" ");
    let mut output = format!("@{} {}", record.name, path);
    if let Some(summary) = &command.summary {
        output.push_str(" — ");
        output.push_str(&sanitize(summary));
    }
    output.push('\n');
    append_usage_options(&mut output, &record.name, command);
    output
}

fn append_usage_options(output: &mut String, name: &str, command: &DescriptorCommand) {
    if let Some(usage) = &command.usage {
        output.push_str("Usage: @");
        output.push_str(name);
        output.push(' ');
        output.push_str(&sanitize(usage));
        output.push('\n');
    }
    if !command.options.is_empty() {
        output.push_str("\nOptions:\n");
        for option in &command.options {
            output.push_str("  ");
            output.push_str(
                &option
                    .names
                    .iter()
                    .map(|name| sanitize(name))
                    .collect::<Vec<_>>()
                    .join(", "),
            );
            if option.takes_value {
                output.push_str(" <VALUE>");
            }
            if let Some(help) = &option.help {
                output.push('\t');
                output.push_str(&sanitize(help));
            }
            output.push('\n');
        }
    }
}

fn sanitize(value: &str) -> String {
    value
        .chars()
        .map(|character| {
            if character.is_control() {
                ' '
            } else {
                character
            }
        })
        .collect()
}

fn validate_invocation_args(args: &[String]) -> Result<(), String> {
    if args.len() > EXT_MAX_ARGS {
        return Err(format!(
            "too many extension command arguments (maximum {EXT_MAX_ARGS})"
        ));
    }
    let mut argument_bytes = 0usize;
    let mut encoded_bytes = 4usize;
    for argument in args {
        if argument.len() > EXT_MAX_ARG {
            return Err(format!(
                "extension command argument exceeds {EXT_MAX_ARG} bytes"
            ));
        }
        argument_bytes = argument_bytes
            .checked_add(argument.len())
            .ok_or_else(|| "extension command arguments are too large".to_string())?;
        encoded_bytes = encoded_bytes
            .checked_add(4)
            .and_then(|total| total.checked_add(argument.len()))
            .ok_or_else(|| "extension command invocation is too large".to_string())?;
    }
    if argument_bytes > EXT_MAX_ARGUMENT_BYTES {
        return Err(format!(
            "extension command arguments exceed {EXT_MAX_ARGUMENT_BYTES} bytes"
        ));
    }
    if encoded_bytes > CHANNEL_MAX_PAYLOAD {
        return Err(format!(
            "encoded extension command invocation exceeds the {CHANNEL_MAX_PAYLOAD}-byte channel payload limit"
        ));
    }
    Ok(())
}

fn encode_invoke(args: &[String], streams_stdin: bool) -> Result<Vec<u8>, String> {
    validate_invocation_args(args)?;
    let encoded_len = 4 + args
        .iter()
        .map(|argument| 4 + argument.len())
        .sum::<usize>();
    let mut payload = Vec::with_capacity(encoded_len);
    payload.push(C2S_INVOKE);
    payload.push(u8::from(streams_stdin) * INVOKE_FLAG_STDIN);
    payload.extend_from_slice(&(args.len() as u16).to_le_bytes());
    for argument in args {
        payload.extend_from_slice(&(argument.len() as u32).to_le_bytes());
        payload.extend_from_slice(argument.as_bytes());
    }
    Ok(payload)
}

struct SendCredit {
    window: u64,
    sent: u64,
    acknowledged: u64,
    boundaries: VecDeque<u64>,
}

impl SendCredit {
    fn new(window: u64) -> Self {
        Self {
            window,
            sent: 0,
            acknowledged: 0,
            boundaries: VecDeque::new(),
        }
    }

    fn can_send(&self, length: usize) -> Result<bool, String> {
        if self.boundaries.len() >= CHANNEL_MAX_UNCONSUMED_MESSAGES {
            return Ok(false);
        }
        let length = u64::try_from(length)
            .map_err(|_| "channel payload byte count does not fit u64".to_string())?;
        let end = self
            .sent
            .checked_add(length)
            .ok_or_else(|| "channel sent-byte counter overflow".to_string())?;
        let limit = self
            .acknowledged
            .checked_add(self.window)
            .ok_or_else(|| "channel credit counter overflow".to_string())?;
        Ok(end <= limit)
    }

    fn record_send(&mut self, length: usize) -> Result<(), String> {
        let length = u64::try_from(length)
            .map_err(|_| "channel payload byte count does not fit u64".to_string())?;
        self.sent = self
            .sent
            .checked_add(length)
            .ok_or_else(|| "channel sent-byte counter overflow".to_string())?;
        self.boundaries.push_back(self.sent);
        Ok(())
    }

    fn apply_ack(&mut self, bytes: u64) -> Result<(), String> {
        if bytes < self.acknowledged || bytes > self.sent {
            return Err("channel ACK is outside the sent byte range".into());
        }
        if bytes != self.acknowledged && !self.boundaries.contains(&bytes) {
            return Err("channel ACK is not on a sent-message boundary".into());
        }
        self.acknowledged = bytes;
        while self
            .boundaries
            .front()
            .is_some_and(|boundary| *boundary <= bytes)
        {
            self.boundaries.pop_front();
        }
        Ok(())
    }
}

async fn run_invocation<R, O, E, C>(
    client: &mut Client,
    record: &DirectoryRecord,
    invoke_payload: Vec<u8>,
    json: bool,
    io: InvocationIo<R, O, E, C>,
) -> Result<i32, String>
where
    R: AsyncRead + Unpin,
    O: AsyncWrite + Unpin,
    E: AsyncWrite + Unpin,
    C: Future<Output = Result<(), String>>,
{
    let InvocationIo {
        streams_stdin,
        mut stdin,
        mut stdout,
        mut stderr,
        cancellation,
    } = io;
    let connect = channel_wire::msg_channel_connect(
        CHANNEL_ID,
        &record.listener_name,
        b"",
        Some(record.listener_token),
    )
    .ok_or_else(|| "server advertised invalid extension command listener data".to_string())?;
    client.send(&connect).await?;

    tokio::pin!(cancellation);
    let window = loop {
        tokio::select! {
            biased;
            packet = client.next_packet() => {
                let packet = packet?;
                let channel_id = match channel_wire::channel_header(&packet) {
                    Ok((_, channel_id, _)) => channel_id,
                    Err(channel_wire::ChannelDecodeError::NotChannel) => continue,
                    Err(error) => {
                        close_channel(client, CHANNEL_CLOSE_CANCELLED).await;
                        return Err(format!("invalid extension command channel envelope: {error}"));
                    }
                };
                if channel_id != CHANNEL_ID {
                    continue;
                }
                match channel_wire::parse_channel_message(&packet) {
                    Ok(Some(ChannelMessage::Opened { status, window, peer, detail, .. })) => {
                        if status != STATUS_OK {
                            return Err(format!(
                                "extension command listener rejected the connection: {}{}",
                                status_text(status),
                                if detail.is_empty() { String::new() } else { format!(": {}", sanitize(detail)) },
                            ));
                        }
                        if window == 0 || peer.is_empty() {
                            close_channel(client, CHANNEL_CLOSE_CANCELLED).await;
                            return Err("server returned an invalid successful command channel open".into());
                        }
                        break window;
                    }
                    Ok(Some(ChannelMessage::Closed { reason, detail, .. })) => {
                        return Err(closed_error(reason, detail));
                    }
                    Ok(None) => {}
                    Ok(Some(_)) => {
                        close_channel(client, CHANNEL_CLOSE_CANCELLED).await;
                        return Err("extension command channel sent data before OPENED".into());
                    }
                    Err(error) => {
                        close_channel(client, CHANNEL_CLOSE_CANCELLED).await;
                        return Err(format!("invalid extension command channel open: {error}"));
                    }
                }
            }
            cancelled = &mut cancellation => {
                close_channel(client, CHANNEL_CLOSE_CANCELLED).await;
                return match cancelled {
                    Ok(()) => Ok(130),
                    Err(error) => Err(error),
                };
            }
        }
    };

    let mut credit = SendCredit::new(window);
    if !credit.can_send(invoke_payload.len())? {
        close_channel(client, CHANNEL_CLOSE_CANCELLED).await;
        return Err("command channel window is too small for the encoded invocation".into());
    }
    send_data(client, &mut credit, &invoke_payload).await?;

    let mut input_buffer = vec![0u8; STDIN_CHUNK];
    let mut pending_input: Option<Vec<u8>> = None;
    let mut stdin_finished = !streams_stdin;
    let mut received = 0u64;
    let mut result_seen = false;

    loop {
        let pending_ready = match pending_input.as_ref() {
            Some(payload) => credit.can_send(payload.len())?,
            None => false,
        };
        if pending_ready {
            let payload = pending_input.take().expect("checked pending input");
            if let Err(error) = send_data(client, &mut credit, &payload).await {
                close_channel(client, CHANNEL_CLOSE_CANCELLED).await;
                return Err(error);
            }
        }

        tokio::select! {
            biased;
            packet = client.next_packet() => {
                let packet = packet?;
                let channel_id = match channel_wire::channel_header(&packet) {
                    Ok((_, channel_id, _)) => channel_id,
                    Err(channel_wire::ChannelDecodeError::NotChannel) => continue,
                    Err(error) => {
                        close_channel(client, CHANNEL_CLOSE_CANCELLED).await;
                        return Err(format!("invalid extension command channel envelope: {error}"));
                    }
                };
                if channel_id != CHANNEL_ID {
                    continue;
                }
                match channel_wire::parse_channel_message(&packet) {
                    Ok(None) => {}
                    Ok(Some(ChannelMessage::Ack { bytes, .. })) => {
                        if let Err(error) = credit.apply_ack(bytes) {
                            close_channel(client, CHANNEL_CLOSE_CANCELLED).await;
                            return Err(error);
                        }
                    }
                    Ok(Some(ChannelMessage::Data { payload, .. })) => {
                        let next_received = received
                            .checked_add(payload.len() as u64)
                            .ok_or_else(|| "channel received-byte counter overflow".to_string())?;
                        let output = match decode_output(payload, &mut result_seen) {
                            Ok(output) => output,
                            Err(error) => {
                                close_channel(client, CHANNEL_CLOSE_CANCELLED).await;
                                return Err(error);
                            }
                        };
                        let exit = match deliver_output(output, json, &mut stdout, &mut stderr).await {
                            Ok(exit) => exit,
                            Err(error) => {
                                close_channel(client, CHANNEL_CLOSE_CANCELLED).await;
                                return Err(error);
                            }
                        };
                        received = next_received;
                        client.send(&channel_wire::msg_channel_ack(CHANNEL_ID, received)).await?;
                        if let Some(code) = exit {
                            stdout.flush().await.map_err(|error| format!("cannot flush command stdout: {error}"))?;
                            stderr.flush().await.map_err(|error| format!("cannot flush command stderr: {error}"))?;
                            close_channel(client, CHANNEL_CLOSE_NORMAL).await;
                            return Ok(code);
                        }
                    }
                    Ok(Some(ChannelMessage::Closed { reason, detail, .. })) => {
                        return Err(closed_error(reason, detail));
                    }
                    Ok(Some(_)) => {
                        close_channel(client, CHANNEL_CLOSE_CANCELLED).await;
                        return Err("unexpected packet on extension command channel".into());
                    }
                    Err(error) => {
                        close_channel(client, CHANNEL_CLOSE_CANCELLED).await;
                        return Err(format!("invalid extension command channel packet: {error}"));
                    }
                }
            }
            cancelled = &mut cancellation => {
                if credit.can_send(1).unwrap_or(false) {
                    let _ = send_data(client, &mut credit, &[C2S_CANCEL]).await;
                }
                close_channel(client, CHANNEL_CLOSE_CANCELLED).await;
                return match cancelled {
                    Ok(()) => Ok(130),
                    Err(error) => Err(error),
                };
            }
            read = stdin.read(&mut input_buffer), if streams_stdin && !stdin_finished && pending_input.is_none() => {
                match read {
                    Ok(0) => {
                        stdin_finished = true;
                        pending_input = Some(vec![C2S_STDIN_EOF]);
                    }
                    Ok(count) => {
                        let mut payload = Vec::with_capacity(1 + count);
                        payload.push(C2S_STDIN);
                        payload.extend_from_slice(&input_buffer[..count]);
                        pending_input = Some(payload);
                    }
                    Err(error) => {
                        close_channel(client, CHANNEL_CLOSE_CANCELLED).await;
                        return Err(format!("cannot read command stdin: {error}"));
                    }
                }
            }
        }
    }
}

async fn send_data(
    client: &mut Client,
    credit: &mut SendCredit,
    payload: &[u8],
) -> Result<(), String> {
    if !credit.can_send(payload.len())? {
        return Err("extension command channel send credit is exhausted".into());
    }
    let packet = channel_wire::msg_channel_data(CHANNEL_ID, payload)
        .ok_or_else(|| "invalid extension command channel payload".to_string())?;
    client.send(&packet).await?;
    credit.record_send(payload.len())
}

async fn close_channel(client: &mut Client, reason: u8) {
    if let Some(packet) = channel_wire::msg_channel_close(CHANNEL_ID, reason) {
        let _ = client.send(&packet).await;
    }
}

fn closed_error(reason: u8, detail: &str) -> String {
    format!(
        "extension command channel closed before EXIT (reason {reason}){}",
        if detail.is_empty() {
            String::new()
        } else {
            format!(": {}", sanitize(detail))
        }
    )
}

enum CommandOutput<'a> {
    Stdout(&'a [u8]),
    Stderr(&'a [u8]),
    Log {
        level: u8,
        message: &'a str,
    },
    Result {
        content_type: &'a str,
        data: &'a [u8],
    },
    Exit {
        code: i32,
        detail: &'a str,
    },
}

fn decode_output<'a>(
    payload: &'a [u8],
    result_seen: &mut bool,
) -> Result<CommandOutput<'a>, String> {
    let Some((&kind, body)) = payload.split_first() else {
        return Err("extension command sent an empty DATA payload".into());
    };
    match kind {
        S2C_STDOUT => Ok(CommandOutput::Stdout(body)),
        S2C_STDERR => Ok(CommandOutput::Stderr(body)),
        S2C_LOG => {
            let Some((&level, message)) = body.split_first() else {
                return Err("extension command LOG payload is truncated".into());
            };
            if level > 4 {
                return Err("extension command LOG level is invalid".into());
            }
            let message = std::str::from_utf8(message)
                .map_err(|_| "extension command LOG is not UTF-8".to_string())?;
            Ok(CommandOutput::Log { level, message })
        }
        S2C_RESULT => {
            if *result_seen {
                return Err("extension command sent more than one RESULT".into());
            }
            if body.len() < 2 {
                return Err("extension command RESULT payload is truncated".into());
            }
            let content_type_len = u16::from_le_bytes([body[0], body[1]]) as usize;
            let content_type_end = 2usize
                .checked_add(content_type_len)
                .ok_or_else(|| "extension command RESULT length overflow".to_string())?;
            let content_type = body
                .get(2..content_type_end)
                .ok_or_else(|| "extension command RESULT content type is truncated".to_string())?;
            let content_type = std::str::from_utf8(content_type)
                .map_err(|_| "extension command RESULT content type is not UTF-8".to_string())?;
            if !valid_content_type(content_type) {
                return Err("extension command RESULT content type is invalid".into());
            }
            *result_seen = true;
            Ok(CommandOutput::Result {
                content_type,
                data: &body[content_type_end..],
            })
        }
        S2C_EXIT => {
            if body.len() < 4 {
                return Err("extension command EXIT payload is truncated".into());
            }
            let code = i32::from_le_bytes(body[..4].try_into().expect("checked exit code"));
            let detail = std::str::from_utf8(&body[4..])
                .map_err(|_| "extension command EXIT detail is not UTF-8".to_string())?;
            Ok(CommandOutput::Exit { code, detail })
        }
        _ => Err(format!(
            "extension command sent unknown blit.cli.v1 payload kind {kind}"
        )),
    }
}

async fn deliver_output<O, E>(
    output: CommandOutput<'_>,
    json: bool,
    stdout: &mut O,
    stderr: &mut E,
) -> Result<Option<i32>, String>
where
    O: AsyncWrite + Unpin,
    E: AsyncWrite + Unpin,
{
    if json {
        let (record, exit) = match output {
            CommandOutput::Stdout(data) => (
                serde_json::json!({
                    "type": "stdout",
                    "kind": S2C_STDOUT,
                    "data": data,
                }),
                None,
            ),
            CommandOutput::Stderr(data) => (
                serde_json::json!({
                    "type": "stderr",
                    "kind": S2C_STDERR,
                    "data": data,
                }),
                None,
            ),
            CommandOutput::Log { level, message } => (
                serde_json::json!({
                    "type": "log",
                    "kind": S2C_LOG,
                    "level": level,
                    "message": message,
                }),
                None,
            ),
            CommandOutput::Result { content_type, data } => (
                serde_json::json!({
                    "type": "result",
                    "kind": S2C_RESULT,
                    "content_type": content_type,
                    "data": data,
                }),
                None,
            ),
            CommandOutput::Exit { code, detail } => (
                serde_json::json!({
                    "type": "exit",
                    "kind": S2C_EXIT,
                    "code": code,
                    "detail": detail,
                }),
                Some(code),
            ),
        };
        let mut line = serde_json::to_vec(&record)
            .map_err(|error| format!("cannot encode command JSON output: {error}"))?;
        line.push(b'\n');
        stdout
            .write_all(&line)
            .await
            .map_err(|error| format!("cannot write command JSON output: {error}"))?;
        stdout
            .flush()
            .await
            .map_err(|error| format!("cannot flush command JSON output: {error}"))?;
        return Ok(exit);
    }

    match output {
        CommandOutput::Stdout(data) | CommandOutput::Result { data, .. } => stdout
            .write_all(data)
            .await
            .map_err(|error| format!("cannot write command stdout: {error}"))?,
        CommandOutput::Stderr(data) => stderr
            .write_all(data)
            .await
            .map_err(|error| format!("cannot write command stderr: {error}"))?,
        CommandOutput::Log { level, message } => {
            let prefix =
                ["[trace] ", "[debug] ", "[info] ", "[warning] ", "[error] "][level as usize];
            stderr
                .write_all(prefix.as_bytes())
                .await
                .map_err(|error| format!("cannot write command log: {error}"))?;
            stderr
                .write_all(message.as_bytes())
                .await
                .map_err(|error| format!("cannot write command log: {error}"))?;
            if !message.ends_with('\n') {
                stderr
                    .write_all(b"\n")
                    .await
                    .map_err(|error| format!("cannot write command log: {error}"))?;
            }
        }
        CommandOutput::Exit { code, detail } => {
            if !detail.is_empty() {
                stderr
                    .write_all(detail.as_bytes())
                    .await
                    .map_err(|error| format!("cannot write command exit detail: {error}"))?;
                if !detail.ends_with('\n') {
                    stderr
                        .write_all(b"\n")
                        .await
                        .map_err(|error| format!("cannot write command exit detail: {error}"))?;
                }
            }
            return Ok(Some(code));
        }
    }
    Ok(None)
}

fn valid_content_type(content_type: &str) -> bool {
    if content_type.is_empty() || content_type.len() > 255 {
        return false;
    }
    let mut components = content_type.split('/');
    let Some(left) = components.next() else {
        return false;
    };
    let Some(right) = components.next() else {
        return false;
    };
    components.next().is_none() && valid_media_component(left) && valid_media_component(right)
}

fn valid_media_component(component: &str) -> bool {
    let mut bytes = component.bytes();
    let Some(first) = bytes.next() else {
        return false;
    };
    (first.is_ascii_lowercase() || first.is_ascii_digit())
        && bytes.all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || b"!#$&^_.+-".contains(&byte)
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        cli::{Cli, Command},
        extension::ExtensionCommand,
        transport::{Transport, read_frame, write_frame},
    };
    use blit_remote::{
        S2C_HELLO, S2C_READY,
        channel::{ChannelRequest, msg_channel_opened, parse_channel_request},
        extension::{
            CommandRecord, ExtensionRequest, msg_extension_commands, parse_extension_request,
        },
    };
    use clap::Parser;
    use std::{
        pin::Pin,
        sync::{Arc, Mutex},
        task::{Context, Poll},
    };

    #[derive(Clone, Default)]
    struct Capture(Arc<Mutex<Vec<u8>>>);

    impl Capture {
        fn bytes(&self) -> Vec<u8> {
            self.0.lock().unwrap().clone()
        }
    }

    impl AsyncWrite for Capture {
        fn poll_write(
            self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            buffer: &[u8],
        ) -> Poll<std::io::Result<usize>> {
            self.0.lock().unwrap().extend_from_slice(buffer);
            Poll::Ready(Ok(buffer.len()))
        }

        fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
            Poll::Ready(Ok(()))
        }

        fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
            Poll::Ready(Ok(()))
        }
    }

    const DESCRIPTOR: &str = r#"{
        "protocol":"blit.cli.v1",
        "summary":"Build and publish",
        "commands":[{
            "path":["build"],
            "summary":"Build one target",
            "usage":"build [--release] TARGET",
            "options":[{"names":["-r","--release"],"takes_value":false,"help":"optimized"}]
        }]
    }"#;

    #[test]
    fn clap_preserves_every_token_after_namespace() {
        let cli = Cli::try_parse_from([
            "blit",
            "--on",
            "prod",
            "--json",
            "@builder",
            "build",
            "--release",
            "app",
            "--json",
            "--on",
            "guest-value",
        ])
        .unwrap();
        assert_eq!(cli.connect.on.as_deref(), Some("prod"));
        assert!(cli.advertised_command_json);
        let Command::External(tokens) = cli.command else {
            panic!("not an external command");
        };
        assert_eq!(
            tokens,
            [
                "@builder",
                "build",
                "--release",
                "app",
                "--json",
                "--on",
                "guest-value"
            ]
        );

        let cli = Cli::try_parse_from(["blit", "@builder", "--json"]).unwrap();
        assert!(!cli.advertised_command_json);
        let Command::External(tokens) = cli.command else {
            panic!("not an external command");
        };
        assert_eq!(tokens, ["@builder", "--json"]);
    }

    #[test]
    fn commands_subcommand_is_discoverable() {
        let cli = Cli::try_parse_from(["blit", "ext", "commands"]).unwrap();
        assert!(matches!(
            cli.command,
            Command::Extension {
                command: ExtensionCommand::Commands
            }
        ));
    }

    #[test]
    fn completion_candidates_are_contextual_sorted_and_sanitized() {
        let descriptor = parse_descriptor(
            r#"{
                "protocol":"blit.cli.v1",
                "summary":"completion test",
                "commands":[
                    {"path":[],"options":[
                        {"names":["--verbose"],"takes_value":false}
                    ]},
                    {"path":["deploy"],"options":[
                        {"names":["-e","--environment"],"takes_value":true},
                        {"names":["--force"],"takes_value":false},
                        {"names":["--bad\nname"],"takes_value":false}
                    ]},
                    {"path":["project"]},
                    {"path":["project","build"],"options":[
                        {"names":["--release"],"takes_value":false}
                    ]},
                    {"path":["bad\npath"]}
                ]
            }"#,
        )
        .unwrap();
        let records = vec![
            DirectoryRecord {
                name: "builder".into(),
                listener_name: "listener".into(),
                listener_token: [1; 16],
                descriptor: descriptor.clone(),
            },
            DirectoryRecord {
                name: "evil\nname".into(),
                listener_name: "listener".into(),
                listener_token: [2; 16],
                descriptor,
            },
        ];

        assert_eq!(completion_candidates(&records, &[], "@"), ["@builder"]);
        assert_eq!(
            completion_candidates(&records, &["--json".into()], "@"),
            ["@builder"]
        );
        assert_eq!(
            completion_candidates(&records, &["@builder".into()], ""),
            ["--verbose", "deploy", "project"]
        );
        assert_eq!(
            completion_candidates(&records, &["@builder".into(), "project".into()], "b"),
            ["build"]
        );
        assert_eq!(
            completion_candidates(&records, &["@builder".into(), "deploy".into()], "--"),
            ["--environment", "--force"]
        );
        assert!(
            completion_candidates(
                &records,
                &["@builder".into(), "deploy".into(), "--environment".into()],
                ""
            )
            .is_empty()
        );
        assert_eq!(
            completion_candidates(
                &records,
                &[
                    "@builder".into(),
                    "deploy".into(),
                    "--environment".into(),
                    "prod".into()
                ],
                "--"
            ),
            ["--force"]
        );
        assert!(
            completion_candidates(
                &records,
                &["@builder".into(), "deploy".into(), "--".into()],
                "--"
            )
            .is_empty()
        );
    }

    #[test]
    fn help_requires_a_final_marker_after_an_exact_path() {
        let record = DirectoryRecord {
            name: "builder".into(),
            listener_name: "listener".into(),
            listener_token: [1; 16],
            descriptor: parse_descriptor(DESCRIPTOR).unwrap(),
        };
        let root = local_help(&record, &["--help".into()]).unwrap();
        assert!(root.contains("Build and publish"));
        assert!(root.contains("build\tBuild one target"));
        let command = local_help(&record, &["build".into(), "--help".into()]).unwrap();
        assert!(command.contains("Usage: @builder build [--release] TARGET"));
        assert!(command.contains("-r, --release"));
        assert!(local_help(&record, &["unknown".into(), "--help".into()]).is_none());
        assert!(local_help(&record, &["--help".into(), "build".into()]).is_none());
    }

    #[test]
    fn invocation_encoding_is_exact_and_bounded() {
        let args = vec!["build".to_string(), "--json".to_string()];
        let payload = encode_invoke(&args, true).unwrap();
        assert_eq!(&payload[..4], &[C2S_INVOKE, INVOKE_FLAG_STDIN, 2, 0]);
        assert_eq!(
            &payload[4..],
            &[
                5, 0, 0, 0, b'b', b'u', b'i', b'l', b'd', 6, 0, 0, 0, b'-', b'-', b'j', b's', b'o',
                b'n'
            ]
        );

        let oversized = vec!["x".repeat(EXT_MAX_ARG + 1)];
        assert!(validate_invocation_args(&oversized).is_err());
        let encoded_overhead = vec!["x".repeat(EXT_MAX_ARG); EXT_MAX_ARGS];
        assert!(validate_invocation_args(&encoded_overhead).is_err());
    }

    #[test]
    fn ack_must_be_monotonic_and_land_on_a_message_boundary() {
        let mut credit = SendCredit::new(100);
        credit.record_send(4).unwrap();
        credit.record_send(3).unwrap();
        assert!(credit.apply_ack(5).is_err());
        credit.apply_ack(4).unwrap();
        assert!(credit.apply_ack(3).is_err());
        credit.apply_ack(7).unwrap();
        assert!(credit.boundaries.is_empty());
    }

    #[test]
    fn output_decoder_validates_result_and_exit() {
        let mut seen = false;
        let mut result = vec![S2C_RESULT];
        result.extend_from_slice(&16u16.to_le_bytes());
        result.extend_from_slice(b"application/json");
        result.extend_from_slice(b"{}");
        assert!(matches!(
            decode_output(&result, &mut seen).unwrap(),
            CommandOutput::Result {
                content_type: "application/json",
                data: b"{}",
            }
        ));
        assert!(decode_output(&result, &mut seen).is_err());

        let mut exit = vec![S2C_EXIT];
        exit.extend_from_slice(&(-7i32).to_le_bytes());
        exit.extend_from_slice(b"done");
        assert!(matches!(
            decode_output(&exit, &mut false).unwrap(),
            CommandOutput::Exit {
                code: -7,
                detail: "done"
            }
        ));
    }

    #[tokio::test]
    async fn json_output_preserves_frame_fields_and_binary_bytes() {
        let stdout = Capture::default();
        let stderr = Capture::default();
        let mut output = stdout.clone();
        let mut errors = stderr.clone();

        assert_eq!(
            deliver_output(
                CommandOutput::Stdout(&[0, 255]),
                true,
                &mut output,
                &mut errors,
            )
            .await
            .unwrap(),
            None
        );
        assert_eq!(
            deliver_output(
                CommandOutput::Stderr(&[128, 10]),
                true,
                &mut output,
                &mut errors,
            )
            .await
            .unwrap(),
            None
        );
        assert_eq!(
            deliver_output(
                CommandOutput::Log {
                    level: 4,
                    message: "failed",
                },
                true,
                &mut output,
                &mut errors,
            )
            .await
            .unwrap(),
            None
        );
        assert_eq!(
            deliver_output(
                CommandOutput::Result {
                    content_type: "application/octet-stream",
                    data: &[1, 2, 254],
                },
                true,
                &mut output,
                &mut errors,
            )
            .await
            .unwrap(),
            None
        );
        assert_eq!(
            deliver_output(
                CommandOutput::Exit {
                    code: i32::MIN,
                    detail: "complete",
                },
                true,
                &mut output,
                &mut errors,
            )
            .await
            .unwrap(),
            Some(i32::MIN)
        );

        let records = stdout
            .bytes()
            .split(|byte| *byte == b'\n')
            .filter(|line| !line.is_empty())
            .map(|line| serde_json::from_slice::<Value>(line).unwrap())
            .collect::<Vec<_>>();
        assert_eq!(
            records,
            [
                serde_json::json!({"type":"stdout", "kind":1, "data":[0,255]}),
                serde_json::json!({"type":"stderr", "kind":2, "data":[128,10]}),
                serde_json::json!({"type":"log", "kind":3, "level":4, "message":"failed"}),
                serde_json::json!({
                    "type":"result",
                    "kind":4,
                    "content_type":"application/octet-stream",
                    "data":[1,2,254],
                }),
                serde_json::json!({
                    "type":"exit",
                    "kind":5,
                    "code":i32::MIN,
                    "detail":"complete",
                }),
            ]
        );
        assert!(stderr.bytes().is_empty());
    }

    #[tokio::test]
    async fn discovery_connect_invoke_output_ack_and_close_round_trip() {
        let (client_io, mut server_io) = tokio::io::duplex(64 * 1024);
        let server = tokio::spawn(async move {
            let mut hello = vec![S2C_HELLO, 0, 0];
            hello.extend_from_slice(&(FEATURE_EXTENSION | FEATURE_CHANNEL).to_le_bytes());
            assert!(write_frame(&mut server_io, &hello).await);
            assert!(write_frame(&mut server_io, &[S2C_READY]).await);

            let discovery = read_frame(&mut server_io).await.unwrap();
            let Some(ExtensionRequest::CommandDiscover {
                nonce,
                directory_revision: 0,
                cursor: 0,
            }) = parse_extension_request(&discovery).unwrap()
            else {
                panic!("not an initial command discovery");
            };
            let token = [9; 16];
            let record = CommandRecord {
                extension_id: 7,
                definition_revision: 3,
                hash: [5; 32],
                name: "builder",
                listener_name: "blit.cli.7.1",
                listener_token: token,
                descriptor: DESCRIPTOR,
            };
            let response = msg_extension_commands(nonce, STATUS_OK, 12, 0, &[record]).unwrap();
            assert!(write_frame(&mut server_io, &response).await);

            let connect = read_frame(&mut server_io).await.unwrap();
            let Some(ChannelRequest::Connect {
                channel_id,
                name,
                metadata,
                listener_token,
            }) = parse_channel_request(&connect).unwrap()
            else {
                panic!("not a channel connect");
            };
            assert_eq!(channel_id, CHANNEL_ID);
            assert_eq!(name, "blit.cli.7.1");
            assert!(metadata.is_empty());
            assert_eq!(listener_token, Some(token));
            assert!(
                write_frame(
                    &mut server_io,
                    &msg_channel_opened(
                        CHANNEL_ID,
                        STATUS_OK,
                        channel_wire::CHANNEL_WINDOW_BYTES,
                        "extension:7",
                        b"",
                        "",
                    )
                    .unwrap(),
                )
                .await
            );

            let invoke = read_frame(&mut server_io).await.unwrap();
            let Some(ChannelRequest::Data { payload, .. }) =
                parse_channel_request(&invoke).unwrap()
            else {
                panic!("not invocation data");
            };
            assert_eq!(
                payload,
                encode_invoke(&["build".into(), "--json".into()], false).unwrap()
            );
            assert!(
                write_frame(
                    &mut server_io,
                    &channel_wire::msg_channel_ack(CHANNEL_ID, payload.len() as u64),
                )
                .await
            );

            let stdout_payload = [vec![S2C_STDOUT], b"hello".to_vec()].concat();
            assert!(
                write_frame(
                    &mut server_io,
                    &channel_wire::msg_channel_data(CHANNEL_ID, &stdout_payload).unwrap(),
                )
                .await
            );
            let ack = read_frame(&mut server_io).await.unwrap();
            assert!(matches!(
                parse_channel_request(&ack).unwrap(),
                Some(ChannelRequest::Ack { bytes, .. }) if bytes == stdout_payload.len() as u64
            ));

            let mut result = vec![S2C_RESULT];
            result.extend_from_slice(&16u16.to_le_bytes());
            result.extend_from_slice(b"application/json");
            result.extend_from_slice(b"{}");
            assert!(
                write_frame(
                    &mut server_io,
                    &channel_wire::msg_channel_data(CHANNEL_ID, &result).unwrap(),
                )
                .await
            );
            let ack = read_frame(&mut server_io).await.unwrap();
            assert!(matches!(
                parse_channel_request(&ack).unwrap(),
                Some(ChannelRequest::Ack { bytes, .. })
                    if bytes == (stdout_payload.len() + result.len()) as u64
            ));

            let mut exit = vec![S2C_EXIT];
            exit.extend_from_slice(&(-7i32).to_le_bytes());
            exit.extend_from_slice(b"done");
            assert!(
                write_frame(
                    &mut server_io,
                    &channel_wire::msg_channel_data(CHANNEL_ID, &exit).unwrap(),
                )
                .await
            );
            let ack = read_frame(&mut server_io).await.unwrap();
            assert!(matches!(
                parse_channel_request(&ack).unwrap(),
                Some(ChannelRequest::Ack { bytes, .. })
                    if bytes == (stdout_payload.len() + result.len() + exit.len()) as u64
            ));
            let close = read_frame(&mut server_io).await.unwrap();
            assert!(matches!(
                parse_channel_request(&close).unwrap(),
                Some(ChannelRequest::Close {
                    reason: CHANNEL_CLOSE_NORMAL,
                    ..
                })
            ));
        });

        let mut client = Client::connect(Transport::Duplex(client_io)).await.unwrap();
        let stdout = Capture::default();
        let stderr = Capture::default();
        let code = invoke_with_io(
            &mut client,
            "builder",
            vec!["build".into(), "--json".into()],
            false,
            InvocationIo {
                streams_stdin: false,
                stdin: tokio::io::empty(),
                stdout: stdout.clone(),
                stderr: stderr.clone(),
                cancellation: std::future::pending::<Result<(), String>>(),
            },
        )
        .await
        .unwrap();
        assert_eq!(code, -7);
        assert_eq!(stdout.bytes(), b"hello{}".to_vec());
        assert_eq!(stderr.bytes(), b"done\n".to_vec());
        server.await.unwrap();
    }

    #[tokio::test]
    async fn discovery_pages_hold_one_revision_and_sort_names() {
        let (client_io, mut server_io) = tokio::io::duplex(64 * 1024);
        let server = tokio::spawn(async move {
            let mut hello = vec![S2C_HELLO, 0, 0];
            hello.extend_from_slice(&(FEATURE_EXTENSION | FEATURE_CHANNEL).to_le_bytes());
            assert!(write_frame(&mut server_io, &hello).await);
            assert!(write_frame(&mut server_io, &[S2C_READY]).await);

            let first = read_frame(&mut server_io).await.unwrap();
            let Some(ExtensionRequest::CommandDiscover {
                nonce,
                directory_revision: 0,
                cursor: 0,
            }) = parse_extension_request(&first).unwrap()
            else {
                panic!("not an initial discovery page");
            };
            let zebra = CommandRecord {
                extension_id: 1,
                definition_revision: 1,
                hash: [1; 32],
                name: "zebra",
                listener_name: "listener.zebra",
                listener_token: [1; 16],
                descriptor: DESCRIPTOR,
            };
            assert!(
                write_frame(
                    &mut server_io,
                    &msg_extension_commands(nonce, STATUS_OK, 44, 91, &[zebra]).unwrap(),
                )
                .await
            );

            let second = read_frame(&mut server_io).await.unwrap();
            let Some(ExtensionRequest::CommandDiscover {
                nonce,
                directory_revision: 44,
                cursor: 91,
            }) = parse_extension_request(&second).unwrap()
            else {
                panic!("discovery did not retain the snapshot revision");
            };
            let alpha = CommandRecord {
                extension_id: 2,
                definition_revision: 1,
                hash: [2; 32],
                name: "alpha",
                listener_name: "listener.alpha",
                listener_token: [2; 16],
                descriptor: DESCRIPTOR,
            };
            assert!(
                write_frame(
                    &mut server_io,
                    &msg_extension_commands(nonce, STATUS_OK, 44, 0, &[alpha]).unwrap(),
                )
                .await
            );
        });

        let mut client = Client::connect(Transport::Duplex(client_io)).await.unwrap();
        let records = discover(&mut client).await.unwrap();
        assert_eq!(
            records
                .iter()
                .map(|record| record.name.as_str())
                .collect::<Vec<_>>(),
            ["alpha", "zebra"]
        );
        server.await.unwrap();
    }

    #[tokio::test]
    async fn redirected_stdin_waits_for_credit_and_ends_with_eof() {
        let (client_io, mut server_io) = tokio::io::duplex(64 * 1024);
        let server = tokio::spawn(async move {
            let mut hello = vec![S2C_HELLO, 0, 0];
            hello.extend_from_slice(&(FEATURE_EXTENSION | FEATURE_CHANNEL).to_le_bytes());
            assert!(write_frame(&mut server_io, &hello).await);
            assert!(write_frame(&mut server_io, &[S2C_READY]).await);

            let discovery = read_frame(&mut server_io).await.unwrap();
            let Some(ExtensionRequest::CommandDiscover { nonce, .. }) =
                parse_extension_request(&discovery).unwrap()
            else {
                panic!("not command discovery");
            };
            let token = [4; 16];
            let record = CommandRecord {
                extension_id: 7,
                definition_revision: 3,
                hash: [5; 32],
                name: "builder",
                listener_name: "blit.cli.7.1",
                listener_token: token,
                descriptor: DESCRIPTOR,
            };
            assert!(
                write_frame(
                    &mut server_io,
                    &msg_extension_commands(nonce, STATUS_OK, 12, 0, &[record]).unwrap(),
                )
                .await
            );

            let connect = read_frame(&mut server_io).await.unwrap();
            assert!(matches!(
                parse_channel_request(&connect).unwrap(),
                Some(ChannelRequest::Connect {
                    listener_token: Some(value),
                    ..
                }) if value == token
            ));
            // INVOKE consumes the entire initial window. No STDIN message may
            // cross the wire until its cumulative ACK releases credit.
            assert!(
                write_frame(
                    &mut server_io,
                    &msg_channel_opened(CHANNEL_ID, STATUS_OK, 4, "extension:7", b"", "").unwrap(),
                )
                .await
            );

            let invoke = read_frame(&mut server_io).await.unwrap();
            let Some(ChannelRequest::Data { payload, .. }) =
                parse_channel_request(&invoke).unwrap()
            else {
                panic!("not invocation data");
            };
            assert_eq!(payload, &[C2S_INVOKE, INVOKE_FLAG_STDIN, 0, 0]);
            assert!(
                write_frame(
                    &mut server_io,
                    &channel_wire::msg_channel_ack(CHANNEL_ID, 4),
                )
                .await
            );

            let stdin = read_frame(&mut server_io).await.unwrap();
            let Some(ChannelRequest::Data { payload, .. }) = parse_channel_request(&stdin).unwrap()
            else {
                panic!("not stdin data");
            };
            assert_eq!(payload, &[C2S_STDIN, b'a', b'b', b'c']);
            assert!(
                write_frame(
                    &mut server_io,
                    &channel_wire::msg_channel_ack(CHANNEL_ID, 8),
                )
                .await
            );

            let eof = read_frame(&mut server_io).await.unwrap();
            let Some(ChannelRequest::Data { payload, .. }) = parse_channel_request(&eof).unwrap()
            else {
                panic!("not stdin EOF");
            };
            assert_eq!(payload, &[C2S_STDIN_EOF]);
            assert!(
                write_frame(
                    &mut server_io,
                    &channel_wire::msg_channel_ack(CHANNEL_ID, 9),
                )
                .await
            );

            let mut exit = vec![S2C_EXIT];
            exit.extend_from_slice(&0i32.to_le_bytes());
            assert!(
                write_frame(
                    &mut server_io,
                    &channel_wire::msg_channel_data(CHANNEL_ID, &exit).unwrap(),
                )
                .await
            );
            let ack = read_frame(&mut server_io).await.unwrap();
            assert!(matches!(
                parse_channel_request(&ack).unwrap(),
                Some(ChannelRequest::Ack { bytes: 5, .. })
            ));
            let close = read_frame(&mut server_io).await.unwrap();
            assert!(matches!(
                parse_channel_request(&close).unwrap(),
                Some(ChannelRequest::Close {
                    reason: CHANNEL_CLOSE_NORMAL,
                    ..
                })
            ));
        });

        let mut client = Client::connect(Transport::Duplex(client_io)).await.unwrap();
        let code = invoke_with_io(
            &mut client,
            "builder",
            Vec::new(),
            false,
            InvocationIo {
                streams_stdin: true,
                stdin: std::io::Cursor::new(b"abc".to_vec()),
                stdout: Capture::default(),
                stderr: Capture::default(),
                cancellation: std::future::pending::<Result<(), String>>(),
            },
        )
        .await
        .unwrap();
        assert_eq!(code, 0);
        server.await.unwrap();
    }

    #[tokio::test]
    async fn cancellation_sends_typed_cancel_then_closes_the_channel() {
        let (client_io, mut server_io) = tokio::io::duplex(64 * 1024);
        let (cancel_tx, cancel_rx) = tokio::sync::oneshot::channel();
        let server = tokio::spawn(async move {
            let mut hello = vec![S2C_HELLO, 0, 0];
            hello.extend_from_slice(&(FEATURE_EXTENSION | FEATURE_CHANNEL).to_le_bytes());
            assert!(write_frame(&mut server_io, &hello).await);
            assert!(write_frame(&mut server_io, &[S2C_READY]).await);

            let discovery = read_frame(&mut server_io).await.unwrap();
            let Some(ExtensionRequest::CommandDiscover { nonce, .. }) =
                parse_extension_request(&discovery).unwrap()
            else {
                panic!("not command discovery");
            };
            let token = [8; 16];
            let record = CommandRecord {
                extension_id: 7,
                definition_revision: 3,
                hash: [5; 32],
                name: "builder",
                listener_name: "blit.cli.7.1",
                listener_token: token,
                descriptor: DESCRIPTOR,
            };
            assert!(
                write_frame(
                    &mut server_io,
                    &msg_extension_commands(nonce, STATUS_OK, 12, 0, &[record]).unwrap(),
                )
                .await
            );
            let connect = read_frame(&mut server_io).await.unwrap();
            assert!(matches!(
                parse_channel_request(&connect).unwrap(),
                Some(ChannelRequest::Connect {
                    listener_token: Some(value),
                    ..
                }) if value == token
            ));
            assert!(
                write_frame(
                    &mut server_io,
                    &msg_channel_opened(CHANNEL_ID, STATUS_OK, 16, "extension:7", b"", "").unwrap(),
                )
                .await
            );

            let invoke = read_frame(&mut server_io).await.unwrap();
            assert!(matches!(
                parse_channel_request(&invoke).unwrap(),
                Some(ChannelRequest::Data {
                    payload: [C2S_INVOKE, 0, 0, 0],
                    ..
                })
            ));
            cancel_tx.send(()).unwrap();

            let cancel = read_frame(&mut server_io).await.unwrap();
            assert!(matches!(
                parse_channel_request(&cancel).unwrap(),
                Some(ChannelRequest::Data {
                    payload: [C2S_CANCEL],
                    ..
                })
            ));
            let close = read_frame(&mut server_io).await.unwrap();
            assert!(matches!(
                parse_channel_request(&close).unwrap(),
                Some(ChannelRequest::Close {
                    reason: CHANNEL_CLOSE_CANCELLED,
                    ..
                })
            ));
        });

        let mut client = Client::connect(Transport::Duplex(client_io)).await.unwrap();
        let code = invoke_with_io(
            &mut client,
            "builder",
            Vec::new(),
            false,
            InvocationIo {
                streams_stdin: false,
                stdin: tokio::io::empty(),
                stdout: Capture::default(),
                stderr: Capture::default(),
                cancellation: async {
                    cancel_rx
                        .await
                        .map_err(|_| "test cancellation source closed".to_string())
                },
            },
        )
        .await
        .unwrap();
        assert_eq!(code, 130);
        server.await.unwrap();
    }
}
