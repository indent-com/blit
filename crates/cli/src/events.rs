use std::path::Path;

use blit_remote::events::{
    EVENT_TYPE_CATALOG, EVENTS_STREAM_APPEND, EVENTS_STREAM_HISTORY, EVENTS_TARGET_CLIENT,
    EVENTS_TARGET_FILE, EventsMessage, FEATURE_EVENTS, msg_events_config_get,
    msg_events_config_set, msg_events_dump, msg_events_stream_list, msg_events_stream_start,
    msg_events_stream_stop, parse_activation_spec, parse_events_message,
};
use tokio::io::{AsyncWrite, AsyncWriteExt};

use crate::agent::AgentConn;
use crate::cli::{EventsCommand, EventsRecordCommand};
use crate::transport::Transport;

const NONCE: u16 = 0xE701;

async fn connect(transport: Transport) -> Result<AgentConn, String> {
    let connection = AgentConn::connect(transport).await?;
    if connection.features & FEATURE_EVENTS == 0 {
        return Err("server does not support blit.events.v1".into());
    }
    Ok(connection)
}

async fn get_config(
    conn: &mut AgentConn,
) -> Result<(u64, u64, u64, u64, u64, blit_remote::events::ActivationSet), String> {
    conn.send(&msg_events_config_get(NONCE)).await?;
    loop {
        let packet = conn.recv().await?;
        let Ok(message) = parse_events_message(&packet) else {
            continue;
        };
        match message {
            EventsMessage::Config {
                nonce: NONCE,
                size,
                used,
                records,
                dropped,
                next_sequence,
                activations,
            } => return Ok((size, used, records, dropped, next_sequence, activations)),
            EventsMessage::Result {
                nonce: NONCE,
                status,
                detail,
                ..
            } => return Err(format!("{}: {detail}", blit_remote::status_text(status))),
            _ => {}
        }
    }
}

fn print_config(
    size: u64,
    used: u64,
    records: u64,
    dropped: u64,
    next_sequence: u64,
    activations: blit_remote::events::ActivationSet,
) {
    println!("protocol\t{}", blit_remote::events::EVENTS_PROTOCOL);
    println!("size\t{size}");
    println!("used\t{used}");
    println!("records\t{records}");
    println!("dropped\t{dropped}");
    println!("next_sequence\t{next_sequence}");
    let enabled = EVENT_TYPE_CATALOG
        .iter()
        .filter_map(|&(kind, name)| activations.enabled(kind).then_some(name))
        .collect::<Vec<_>>()
        .join(",");
    println!("events\t{enabled}");
    println!(
        "bitset\t{}",
        activations
            .0
            .iter()
            .map(|word| format!("{word:016x}"))
            .collect::<Vec<_>>()
            .join(":")
    );
}

async fn wait_result(conn: &mut AgentConn, started: bool) -> Result<u32, String> {
    loop {
        let packet = conn.recv().await?;
        let Ok(message) = parse_events_message(&packet) else {
            continue;
        };
        match message {
            EventsMessage::Result {
                nonce: NONCE,
                status,
                stream_id,
                detail,
            } => {
                if status == blit_remote::STATUS_OK {
                    return Ok(stream_id);
                }
                return Err(format!("{}: {detail}", blit_remote::status_text(status)));
            }
            EventsMessage::StreamStarted {
                nonce: NONCE,
                status,
                stream_id,
                detail,
            } if started => {
                if status == blit_remote::STATUS_OK {
                    return Ok(stream_id);
                }
                return Err(format!("{}: {detail}", blit_remote::status_text(status)));
            }
            _ => {}
        }
    }
}

async fn open_output(
    path: &str,
    append: bool,
) -> Result<Box<dyn AsyncWrite + Unpin + Send>, String> {
    if path == "-" {
        return Ok(Box::new(tokio::io::stdout()));
    }
    let mut options = tokio::fs::OpenOptions::new();
    options
        .create(true)
        .write(true)
        .append(append)
        .truncate(!append);
    Ok(Box::new(
        options
            .open(Path::new(path))
            .await
            .map_err(|error| format!("cannot open {path}: {error}"))?,
    ))
}

pub(crate) async fn run(transport: Transport, command: EventsCommand) -> Result<(), String> {
    let mut conn = connect(transport).await?;
    match command {
        EventsCommand::Config => {
            let (size, used, records, dropped, next_sequence, activations) =
                get_config(&mut conn).await?;
            print_config(size, used, records, dropped, next_sequence, activations);
        }
        EventsCommand::Set { size, events } => {
            let (current_size, _, _, _, _, current_activations) = get_config(&mut conn).await?;
            let size = size.unwrap_or(current_size);
            let activations = events
                .as_deref()
                .map(parse_activation_spec)
                .transpose()?
                .unwrap_or(current_activations);
            conn.send(&msg_events_config_set(NONCE, size, activations))
                .await?;
            loop {
                let packet = conn.recv().await?;
                let Ok(message) = parse_events_message(&packet) else {
                    continue;
                };
                match message {
                    EventsMessage::Config {
                        nonce: NONCE,
                        size,
                        used,
                        records,
                        dropped,
                        next_sequence,
                        activations,
                    } => {
                        print_config(size, used, records, dropped, next_sequence, activations);
                        break;
                    }
                    EventsMessage::Result {
                        nonce: NONCE,
                        status,
                        detail,
                        ..
                    } => return Err(format!("{}: {detail}", blit_remote::status_text(status))),
                    _ => {}
                }
            }
        }
        EventsCommand::Dump { output } => {
            conn.send(&msg_events_dump(NONCE)).await?;
            loop {
                let packet = conn.recv().await?;
                if let Ok(EventsMessage::Dump {
                    nonce: NONCE,
                    bytes,
                }) = parse_events_message(&packet)
                {
                    let path = output.as_deref().unwrap_or("-");
                    let mut writer = open_output(path, false).await?;
                    writer
                        .write_all(bytes)
                        .await
                        .map_err(|error| format!("cannot write {path}: {error}"))?;
                    writer
                        .flush()
                        .await
                        .map_err(|error| format!("cannot flush {path}: {error}"))?;
                    break;
                }
            }
        }
        EventsCommand::Tail {
            output,
            append,
            from_now,
        } => {
            let mut flags = 0;
            if !from_now {
                flags |= EVENTS_STREAM_HISTORY;
            }
            conn.send(&msg_events_stream_start(
                NONCE,
                EVENTS_TARGET_CLIENT,
                flags,
                "",
            ))
            .await?;
            let stream_id = wait_result(&mut conn, true).await?;
            let path = output.as_deref().unwrap_or("-");
            let mut writer = open_output(path, append).await?;
            loop {
                tokio::select! {
                    result = conn.recv_unbounded() => {
                        let packet = result?;
                        let Ok(message) = parse_events_message(&packet) else {
                            continue;
                        };
                        match message {
                            EventsMessage::Dump { nonce: NONCE, bytes } => {
                                writer.write_all(bytes).await.map_err(|e| e.to_string())?;
                            }
                            EventsMessage::Record { stream_id: id, record } if id == stream_id => {
                                writer.write_all(record).await.map_err(|e| e.to_string())?;
                            }
                            EventsMessage::StreamGap { stream_id: id, lost } if id == stream_id => {
                                eprintln!("blit: event stream lost {lost} records");
                            }
                            EventsMessage::StreamStopped { stream_id: id, status, detail }
                                if id == stream_id => {
                                    if status != blit_remote::STATUS_OK {
                                        return Err(format!("{}: {detail}", blit_remote::status_text(status)));
                                    }
                                    break;
                                }
                            _ => {}
                        }
                    }
                    _ = tokio::signal::ctrl_c() => {
                        conn.send(&msg_events_stream_stop(NONCE, stream_id)).await?;
                        let _ = wait_result(&mut conn, false).await?;
                        break;
                    }
                }
            }
            writer.flush().await.map_err(|error| error.to_string())?;
        }
        EventsCommand::Record { command } => match command {
            EventsRecordCommand::Start {
                path,
                append,
                from_now,
            } => {
                let mut flags = 0;
                if !from_now {
                    flags |= EVENTS_STREAM_HISTORY;
                }
                if append {
                    flags |= EVENTS_STREAM_APPEND;
                }
                conn.send(&msg_events_stream_start(
                    NONCE,
                    EVENTS_TARGET_FILE,
                    flags,
                    &path,
                ))
                .await?;
                println!("{}", wait_result(&mut conn, true).await?);
            }
            EventsRecordCommand::List => {
                conn.send(&msg_events_stream_list(NONCE)).await?;
                loop {
                    let packet = conn.recv().await?;
                    let Ok(message) = parse_events_message(&packet) else {
                        continue;
                    };
                    match message {
                        EventsMessage::Streams {
                            nonce: NONCE,
                            streams,
                        } => {
                            for stream in streams {
                                let state = if stream.running {
                                    "running"
                                } else {
                                    "finished"
                                };
                                let history = if stream.flags & EVENTS_STREAM_HISTORY != 0 {
                                    "history"
                                } else {
                                    "from-now"
                                };
                                let mode = if stream.flags & EVENTS_STREAM_APPEND != 0 {
                                    "append"
                                } else {
                                    "truncate"
                                };
                                println!(
                                    "{}\t{state}\t{history}\t{mode}\t{}",
                                    stream.stream_id, stream.path
                                );
                            }
                            break;
                        }
                        EventsMessage::Result {
                            nonce: NONCE,
                            status,
                            detail,
                            ..
                        } => {
                            return Err(format!("{}: {detail}", blit_remote::status_text(status)));
                        }
                        _ => {}
                    }
                }
            }
            EventsRecordCommand::Stop { id } => {
                conn.send(&msg_events_stream_stop(NONCE, id)).await?;
                let _ = wait_result(&mut conn, false).await?;
            }
        },
    }
    Ok(())
}
