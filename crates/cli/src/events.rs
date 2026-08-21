use std::io::ErrorKind;

use blit_remote::events::{
    Activation, C2S_CONFIG_GET, C2S_CONFIG_SET, C2S_DUMP, C2S_FILE_START, C2S_FILE_STOP,
    C2S_STREAM_START, C2S_STREAM_STOP, EVENT_NAMES, EVENT_RECORD_SIZE, EVENTS,
    EVENTS_DUMP_MAX_RECORDS, EVENTS_RING_MAX, EVENTS_RING_MIN, EventConfig, EventFileHeader,
    EventMessage, FEATURE_EVENTS, FILE_APPEND, FILE_SYNC, STREAM_FOLLOW, event_name,
    msg_config_get, msg_config_set, msg_dump, msg_file_start, msg_file_stop, msg_stream_start,
    msg_stream_stop, parse_event_message,
};
use blit_remote::{STATUS_BUDGET, STATUS_OK, status_text};
use tokio::io::AsyncWriteExt;

use crate::agent::AgentConn;
use crate::transport::Transport;

const REQUEST_CONFIG_GET: u32 = 1;
const REQUEST_CONFIG_SET: u32 = 2;
const REQUEST_DUMP: u32 = 3;
const REQUEST_STREAM_START: u32 = 4;
const REQUEST_STREAM_STOP: u32 = 5;
const REQUEST_FILE_START: u32 = 6;
const REQUEST_FILE_STOP: u32 = 7;

fn require_feature(conn: &AgentConn) -> Result<(), String> {
    if conn.features & FEATURE_EVENTS == 0 {
        return Err("server has no structured events support (upgrade blit on the remote)".into());
    }
    Ok(())
}

fn status_result(operation: &str, status: u8) -> Result<(), String> {
    if status == STATUS_OK {
        Ok(())
    } else {
        Err(format!("{operation}: {}", status_text(status)))
    }
}

async fn recv_message(conn: &mut AgentConn) -> Result<EventMessage, String> {
    loop {
        let packet = conn.recv().await?;
        if packet.first() != Some(&EVENTS) {
            continue;
        }
        return parse_event_message(&packet).map_err(|error| error.to_string());
    }
}

async fn recv_message_unbounded(conn: &mut AgentConn) -> Result<EventMessage, String> {
    loop {
        let packet = conn.recv_unbounded().await?;
        if packet.first() != Some(&EVENTS) {
            continue;
        }
        return parse_event_message(&packet).map_err(|error| error.to_string());
    }
}

async fn get_config(conn: &mut AgentConn, request_id: u32) -> Result<EventConfig, String> {
    conn.send(&msg_config_get(request_id)).await?;
    loop {
        match recv_message(conn).await? {
            EventMessage::Config {
                request_id: reply_id,
                status,
                config,
            } if reply_id == request_id => {
                status_result("events config", status)?;
                return Ok(config);
            }
            EventMessage::Status {
                request_id: reply_id,
                request_kind: C2S_CONFIG_GET,
                status,
            } if reply_id == request_id => {
                return Err(format!("events config: {}", status_text(status)));
            }
            _ => {}
        }
    }
}

async fn set_config(
    conn: &mut AgentConn,
    request_id: u32,
    config: EventConfig,
) -> Result<EventConfig, String> {
    let packet = msg_config_set(request_id, config).map_err(|error| error.to_string())?;
    conn.send(&packet).await?;
    loop {
        match recv_message(conn).await? {
            EventMessage::Config {
                request_id: reply_id,
                status,
                config,
            } if reply_id == request_id => {
                status_result("events config set", status)?;
                return Ok(config);
            }
            EventMessage::Status {
                request_id: reply_id,
                request_kind: C2S_CONFIG_SET,
                status,
            } if reply_id == request_id => {
                return Err(format!("events config set: {}", status_text(status)));
            }
            _ => {}
        }
    }
}

fn activation_hex(activation: Activation) -> String {
    activation
        .0
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn active_values(activation: Activation) -> Vec<serde_json::Value> {
    (0..128u8)
        .filter(|id| activation.contains(*id))
        .map(|id| match event_name(id) {
            Some(name) => serde_json::json!({ "id": id, "name": name }),
            None => serde_json::json!({ "id": id }),
        })
        .collect()
}

fn print_config(config: EventConfig, json: bool) {
    let bytes = config.ring_size as u64 * EVENT_RECORD_SIZE as u64;
    let hex = activation_hex(config.activation);
    if json {
        println!(
            "{}",
            serde_json::json!({
                "bytes": bytes,
                "records": config.ring_size,
                "activation_hex": hex,
                "active": active_values(config.activation),
            })
        );
        return;
    }
    let active = (0..128u8)
        .filter(|id| config.activation.contains(*id))
        .map(|id| match event_name(id) {
            Some(name) => format!("{id}:{name}"),
            None => id.to_string(),
        })
        .collect::<Vec<_>>()
        .join(",");
    println!("bytes\t{bytes}");
    println!("records\t{}", config.ring_size);
    println!("activation_hex\t{hex}");
    println!("active\t{active}");
}

pub async fn cmd_config(transport: Transport, json: bool) -> Result<(), String> {
    let mut conn = AgentConn::connect(transport).await?;
    require_feature(&conn)?;
    let config = get_config(&mut conn, REQUEST_CONFIG_GET).await?;
    print_config(config, json);
    Ok(())
}

pub async fn cmd_config_set(
    transport: Transport,
    bytes: Option<String>,
    active: Option<String>,
    json: bool,
) -> Result<(), String> {
    if bytes.is_none() && active.is_none() {
        return Err("events config set requires --bytes and/or --active".into());
    }
    let mut conn = AgentConn::connect(transport).await?;
    require_feature(&conn)?;
    let current = if bytes.is_none() || active.is_none() {
        Some(get_config(&mut conn, REQUEST_CONFIG_GET).await?)
    } else {
        None
    };
    let ring_size = match bytes {
        Some(value) => bytes_to_records(parse_bytes(&value)?)?,
        None => current.expect("missing field reads config").ring_size,
    };
    let activation = match active {
        Some(value) => parse_activation(&value)?,
        None => current.expect("missing field reads config").activation,
    };
    let config = set_config(
        &mut conn,
        REQUEST_CONFIG_SET,
        EventConfig::new(ring_size, activation).map_err(|error| error.to_string())?,
    )
    .await?;
    print_config(config, json);
    Ok(())
}

fn parse_bytes(input: &str) -> Result<u64, String> {
    let value = input.trim();
    let split = value
        .find(|character: char| !character.is_ascii_digit())
        .unwrap_or(value.len());
    let number = value[..split]
        .parse::<u64>()
        .map_err(|_| format!("invalid byte size {input:?}"))?;
    let multiplier = match value[split..].trim().to_ascii_lowercase().as_str() {
        "" | "b" => 1,
        "k" | "kb" | "kib" => 1024,
        "m" | "mb" | "mib" => 1024 * 1024,
        "g" | "gb" | "gib" => 1024 * 1024 * 1024,
        suffix => return Err(format!("invalid byte-size suffix {suffix:?}")),
    };
    number
        .checked_mul(multiplier)
        .ok_or_else(|| "byte size is too large".into())
}

fn bytes_to_records(bytes: u64) -> Result<u32, String> {
    if !bytes.is_multiple_of(EVENT_RECORD_SIZE as u64) {
        return Err(format!("--bytes must be a multiple of {EVENT_RECORD_SIZE}"));
    }
    let records = bytes / EVENT_RECORD_SIZE as u64;
    if !(EVENTS_RING_MIN as u64..=EVENTS_RING_MAX as u64).contains(&records) {
        return Err(format!(
            "--bytes must select {EVENTS_RING_MIN}..={EVENTS_RING_MAX} records"
        ));
    }
    Ok(records as u32)
}

fn parse_activation(input: &str) -> Result<Activation, String> {
    let value = input.trim();
    let hex = value.strip_prefix("0x").unwrap_or(value);
    if hex.len() == 32 && hex.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        let mut bytes = [0; 16];
        for (index, byte) in bytes.iter_mut().enumerate() {
            *byte = u8::from_str_radix(&hex[index * 2..index * 2 + 2], 16)
                .map_err(|_| "invalid activation hex".to_string())?;
        }
        return Ok(Activation(bytes));
    }

    let selectors = value
        .split([',', ' ', '\t', '\n'])
        .filter(|selector| !selector.is_empty())
        .collect::<Vec<_>>();
    if selectors.is_empty() {
        return Err("--active must contain a selector or 32 hexadecimal digits".into());
    }
    let mut activation = Activation::NONE;
    for raw in selectors {
        let (enabled, selector) = match raw.as_bytes()[0] {
            b'+' => (true, &raw[1..]),
            b'-' => (false, &raw[1..]),
            _ => (true, raw),
        };
        if selector.is_empty() {
            return Err("empty event selector".into());
        }
        match selector.replace('_', "-").to_ascii_lowercase().as_str() {
            "all" => {
                activation = if enabled {
                    Activation::ALL
                } else {
                    Activation::NONE
                }
            }
            "none" => {
                activation = if enabled {
                    Activation::NONE
                } else {
                    Activation::ALL
                }
            }
            selector => {
                let ids = selector_ids(selector)
                    .ok_or_else(|| format!("unknown event selector {selector:?}"))?;
                for id in ids {
                    activation.set(id, enabled);
                }
            }
        }
    }
    Ok(activation)
}

fn selector_ids(selector: &str) -> Option<Vec<u8>> {
    if let Ok(id) = selector.parse::<u8>() {
        return Some(vec![id]);
    }
    if let Some((id, _)) = EVENT_NAMES.iter().find(|(_, name)| *name == selector) {
        return Some(vec![*id]);
    }
    let range = match selector {
        "server" | "lifecycle" => 0..=7,
        "client" | "clients" => 8..=15,
        "request" | "requests" | "raw-request" | "raw-requests" => 16..=23,
        "writer" | "writers" => 24..=31,
        "pty" | "pty-create" => 32..=55,
        "process" | "processes" => 56..=63,
        "compositor" => 64..=67,
        "surface" | "surfaces" => 68..=71,
        "protocol" | "protocols" | "integration" | "integrations" => 72..=103,
        "task" | "tasks" => 104..=111,
        "recorder" | "config" | "ring" | "stream" => 112..=127,
        _ => return None,
    };
    Some(
        EVENT_NAMES
            .iter()
            .filter_map(|(id, _)| range.contains(id).then_some(*id))
            .collect(),
    )
}

enum EventOutput {
    Stdout(tokio::io::Stdout),
    File(tokio::fs::File),
}

impl EventOutput {
    async fn open(path: &str) -> Result<Self, String> {
        if path == "-" {
            Ok(Self::Stdout(tokio::io::stdout()))
        } else {
            tokio::fs::File::create(path)
                .await
                .map(Self::File)
                .map_err(|error| format!("{path}: {error}"))
        }
    }

    async fn write(&mut self, bytes: &[u8]) -> std::io::Result<()> {
        match self {
            Self::Stdout(output) => output.write_all(bytes).await,
            Self::File(output) => output.write_all(bytes).await,
        }
    }

    async fn flush(&mut self) -> std::io::Result<()> {
        match self {
            Self::Stdout(output) => output.flush().await,
            Self::File(output) => output.flush().await,
        }
    }
}

async fn write_records(
    output: &mut EventOutput,
    records: &[blit_remote::events::EventRecord],
) -> std::io::Result<()> {
    for record in records {
        output.write(&record.encode()).await?;
    }
    Ok(())
}

pub async fn cmd_dump(
    transport: Transport,
    since: u64,
    limit: u32,
    output_path: String,
) -> Result<(), String> {
    if limit == 0 || limit > EVENTS_DUMP_MAX_RECORDS {
        return Err(format!("--limit must be in 1..={EVENTS_DUMP_MAX_RECORDS}"));
    }
    let mut conn = AgentConn::connect(transport).await?;
    require_feature(&conn)?;
    let packet = msg_dump(REQUEST_DUMP, since, limit).map_err(|error| error.to_string())?;
    conn.send(&packet).await?;
    loop {
        match recv_message(&mut conn).await? {
            EventMessage::Dump {
                request_id: REQUEST_DUMP,
                status,
                records,
                ..
            } => {
                if status != STATUS_OK && status != STATUS_BUDGET {
                    return Err(format!("events dump: {}", status_text(status)));
                }
                if status == STATUS_BUDGET {
                    eprintln!("blit: events dump starts after records no longer retained");
                }
                let mut output = EventOutput::open(&output_path).await?;
                output
                    .write(&EventFileHeader::CANONICAL.encode())
                    .await
                    .map_err(|error| format!("{output_path}: {error}"))?;
                write_records(&mut output, &records)
                    .await
                    .map_err(|error| format!("{output_path}: {error}"))?;
                output
                    .flush()
                    .await
                    .map_err(|error| format!("{output_path}: {error}"))?;
                return Ok(());
            }
            EventMessage::Status {
                request_id: REQUEST_DUMP,
                request_kind: C2S_DUMP,
                status,
            } => return Err(format!("events dump: {}", status_text(status))),
            _ => {}
        }
    }
}

fn parse_stream_cursor(value: &str) -> Result<u64, String> {
    match value {
        "now" => Ok(u64::MAX),
        "oldest" => Ok(0),
        _ => value
            .parse()
            .map_err(|_| format!("invalid event cursor {value:?} (want now, oldest, or SEQ)")),
    }
}

async fn stop_stream(conn: &mut AgentConn, stream_id: u32) -> Result<(), String> {
    conn.send(&msg_stream_stop(REQUEST_STREAM_STOP, stream_id))
        .await?;
    loop {
        match recv_message(conn).await? {
            EventMessage::StreamStatus {
                request_id: REQUEST_STREAM_STOP,
                status,
                stream_id: reply_stream,
                ..
            } if reply_stream == stream_id => return status_result("events stream stop", status),
            EventMessage::Status {
                request_id: REQUEST_STREAM_STOP,
                request_kind: C2S_STREAM_STOP,
                status,
            } => return Err(format!("events stream stop: {}", status_text(status))),
            _ => {}
        }
    }
}

pub async fn cmd_stream(
    transport: Transport,
    since: String,
    output_path: String,
) -> Result<(), String> {
    let cursor = parse_stream_cursor(&since)?;
    let stream_id = random_id();
    let mut conn = AgentConn::connect(transport).await?;
    require_feature(&conn)?;
    let packet = msg_stream_start(REQUEST_STREAM_START, stream_id, cursor, STREAM_FOLLOW)
        .map_err(|error| error.to_string())?;
    conn.send(&packet).await?;
    loop {
        match recv_message(&mut conn).await? {
            EventMessage::StreamStatus {
                request_id: REQUEST_STREAM_START,
                status,
                stream_id: reply_stream,
                ..
            } if reply_stream == stream_id => {
                status_result("events stream", status)?;
                break;
            }
            EventMessage::Status {
                request_id: REQUEST_STREAM_START,
                request_kind: C2S_STREAM_START,
                status,
            } => return Err(format!("events stream: {}", status_text(status))),
            _ => {}
        }
    }

    let mut output = EventOutput::open(&output_path).await?;
    if let Err(error) = output.write(&EventFileHeader::CANONICAL.encode()).await {
        let _ = stop_stream(&mut conn, stream_id).await;
        if error.kind() == ErrorKind::BrokenPipe {
            return Ok(());
        }
        return Err(format!("{output_path}: {error}"));
    }

    loop {
        tokio::select! {
            result = recv_message_unbounded(&mut conn) => {
                match result? {
                    EventMessage::StreamData { stream_id: reply_stream, records }
                        if reply_stream == stream_id =>
                    {
                        if let Err(error) = write_records(&mut output, &records).await {
                            let _ = stop_stream(&mut conn, stream_id).await;
                            if error.kind() == ErrorKind::BrokenPipe {
                                return Ok(());
                            }
                            return Err(format!("{output_path}: {error}"));
                        }
                    }
                    EventMessage::StreamStatus { request_id: 0, stream_id: reply_stream, status, .. }
                        if reply_stream == stream_id && status == STATUS_BUDGET =>
                    {
                        eprintln!("blit: event stream gap: records were overwritten");
                    }
                    EventMessage::StreamStatus { request_id: 0, stream_id: reply_stream, status, .. }
                        if reply_stream == stream_id && status != STATUS_OK =>
                    {
                        return Err(format!("events stream: {}", status_text(status)));
                    }
                    _ => {}
                }
            }
            _ = tokio::signal::ctrl_c() => {
                let _ = stop_stream(&mut conn, stream_id).await;
                let _ = output.flush().await;
                return Ok(());
            }
        }
    }
}

fn random_id() -> u32 {
    loop {
        let id = rand::random();
        if id != 0 {
            return id;
        }
    }
}

async fn recv_file_status(
    conn: &mut AgentConn,
    request_id: u32,
    request_kind: u8,
    stream_id: u32,
) -> Result<(u64, u64, String), String> {
    loop {
        match recv_message(conn).await? {
            EventMessage::FileStatus {
                request_id: reply_id,
                status,
                stream_id: reply_stream,
                records_written,
                bytes_written,
                detail,
            } if reply_id == request_id && reply_stream == stream_id => {
                status_result("events file", status)?;
                return Ok((records_written, bytes_written, detail));
            }
            EventMessage::Status {
                request_id: reply_id,
                request_kind: reply_kind,
                status,
            } if reply_id == request_id && reply_kind == request_kind => {
                return Err(format!("events file: {}", status_text(status)));
            }
            _ => {}
        }
    }
}

fn print_file_status(id: u32, records_written: u64, bytes_written: u64, detail: &str, json: bool) {
    if json {
        println!(
            "{}",
            serde_json::json!({
                "id": id,
                "records_written": records_written,
                "bytes_written": bytes_written,
                "detail": detail,
            })
        );
    } else {
        println!("id\t{id}");
        println!("records_written\t{records_written}");
        println!("bytes_written\t{bytes_written}");
        if !detail.is_empty() {
            println!("detail\t{detail}");
        }
    }
}

pub async fn cmd_file_start(
    transport: Transport,
    path: String,
    append: bool,
    sync: bool,
    id: Option<u32>,
    json: bool,
) -> Result<(), String> {
    let stream_id = id.unwrap_or_else(random_id);
    if stream_id == 0 {
        return Err("event file id must not be zero".into());
    }
    let mut conn = AgentConn::connect(transport).await?;
    require_feature(&conn)?;
    let flags = (if append { FILE_APPEND } else { 0 }) | (if sync { FILE_SYNC } else { 0 });
    let packet = msg_file_start(REQUEST_FILE_START, stream_id, flags, &path)
        .map_err(|error| error.to_string())?;
    conn.send(&packet).await?;
    let (records, bytes, detail) =
        recv_file_status(&mut conn, REQUEST_FILE_START, C2S_FILE_START, stream_id).await?;
    print_file_status(stream_id, records, bytes, &detail, json);
    Ok(())
}

pub async fn cmd_file_stop(transport: Transport, stream_id: u32, json: bool) -> Result<(), String> {
    let mut conn = AgentConn::connect(transport).await?;
    require_feature(&conn)?;
    conn.send(&msg_file_stop(REQUEST_FILE_STOP, stream_id))
        .await?;
    let (records, bytes, detail) =
        recv_file_status(&mut conn, REQUEST_FILE_STOP, C2S_FILE_STOP, stream_id).await?;
    print_file_status(stream_id, records, bytes, &detail, json);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use blit_remote::events::{EventRequest, parse_event_request};

    #[test]
    fn parses_bytes_and_activation_forms() {
        assert_eq!(bytes_to_records(parse_bytes("1MiB").unwrap()), Ok(16_384));
        assert!(bytes_to_records(65).is_err());

        let active = parse_activation("none,pty,+task-failed").unwrap();
        assert!(active.contains(32));
        assert!(active.contains(107));
        assert!(!active.contains(8));
        let hex = activation_hex(active);
        assert_eq!(parse_activation(&hex), Ok(active));
        assert_eq!(parse_activation(&format!("0x{hex}")), Ok(active));
        assert!(parse_activation("unknown-event").is_err());
    }

    #[test]
    fn cli_values_round_trip_through_protocol_codec() {
        let activation = parse_activation("client,pty-exit,117").unwrap();
        let config = EventConfig::new(
            bytes_to_records(parse_bytes("64KiB").unwrap()).unwrap(),
            activation,
        )
        .unwrap();
        let packet = msg_config_set(REQUEST_CONFIG_SET, config).unwrap();
        assert_eq!(
            parse_event_request(&packet),
            Ok(EventRequest::ConfigSet {
                request_id: REQUEST_CONFIG_SET,
                config,
            })
        );

        let cursor = parse_stream_cursor("now").unwrap();
        let packet = msg_stream_start(91, 92, cursor, STREAM_FOLLOW).unwrap();
        assert!(matches!(
            parse_event_request(&packet),
            Ok(EventRequest::StreamStart {
                request_id: 91,
                stream_id: 92,
                from_sequence: u64::MAX,
                flags: STREAM_FOLLOW,
            })
        ));
    }
}
