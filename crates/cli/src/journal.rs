//! `blit terminal journal` / `output` / `history --since` — the per-command
//! journal and the sequence cursor (docs/design/term-journal.md).
//!
//! Three questions an agent driving a long-lived shell actually asks: what
//! commands has this terminal run, what did one of them print, and what is
//! new since I last looked. All three are answered by index or by cursor, so
//! nothing here pulls the whole scrollback by accident.
//!
//! Everything degrades to an error rather than a hang against a server
//! without `FEATURE_TERM_JOURNAL`: an old server drops the opcode silently
//! and the request would never be answered.

use blit_remote::journal::{
    CommandRecord, FEATURE_TERM_JOURNAL, JOURNAL_INDEX_LATEST, JOURNAL_TAIL, OUTPUT_ALT_SCREEN,
    OUTPUT_EVICTED, OUTPUT_TRUNCATED, OutputReply, RECORD_EVICTED, RECORD_HAS_EXIT,
    RECORD_INCOMPLETE, RECORD_NO_COMMAND, RECORD_PTY_EXITED, S2C_TERM_COMMAND, S2C_TERM_JOURNAL,
    S2C_TERM_OUTPUT, SINCE_PROBE, msg_term_journal, msg_term_journal_wait, msg_term_output,
    msg_term_since, parse_s2c_term_command, parse_s2c_term_journal, parse_s2c_term_output,
};
use blit_remote::{STATUS_NOT_FOUND, STATUS_OK, status_text};

use crate::agent::AgentConn;
use crate::transport::Transport;

/// Default cap on a single output read. Large enough that an ordinary
/// command comes back whole, small enough that a runaway one does not have
/// to be paged through in a thousand steps.
pub const OUTPUT_MAX_BYTES: u32 = 256 * 1024;

/// Default number of records `blit terminal journal` prints.
pub const JOURNAL_LIMIT: u16 = 20;

const NONCE: u16 = 1;

/// How long to wait for a reply that the server produces synchronously.
fn reply_deadline() -> tokio::time::Instant {
    tokio::time::Instant::now() + std::time::Duration::from_secs(10)
}

/// A cursor as the CLI spells it: `SEQ` or `SEQ:COL`, plus the two words
/// that save a caller from having to know a number at all.
pub(crate) enum Cursor {
    /// `now` — ask the server where the terminal currently is, and read
    /// nothing. This is how a caller starts following output.
    Now,
    At(u64, u16),
}

pub(crate) fn parse_cursor(text: &str) -> Result<Cursor, String> {
    match text {
        "now" | "end" => return Ok(Cursor::Now),
        "start" | "oldest" => return Ok(Cursor::At(0, 0)),
        _ => {}
    }
    let (seq, col) = match text.split_once(':') {
        Some((seq, col)) => (seq, Some(col)),
        None => (text, None),
    };
    let seq = seq
        .parse::<u64>()
        .map_err(|_| format!("not a cursor: {text} (want SEQ, SEQ:COL, now, or start)"))?;
    let col = match col {
        Some(col) => col
            .parse::<u16>()
            .map_err(|_| format!("not a column: {col}"))?,
        None => 0,
    };
    Ok(Cursor::At(seq, col))
}

/// The cursor as a client should print it and feed it back in.
pub(crate) fn format_cursor(seq: u64, col: u16) -> String {
    format!("{seq}:{col}")
}

fn require_feature(conn: &AgentConn, id: u16) -> Result<(), String> {
    if conn.features & FEATURE_TERM_JOURNAL == 0 {
        return Err(
            "server has no terminal journal (upgrade blit on the remote, or BLIT_TERM_JOURNAL=0 \
             is set)"
                .to_string(),
        );
    }
    if !conn.has_pty(id) {
        return Err(format!("pty {id} not found"));
    }
    Ok(())
}

/// Read replies until the one carrying our nonce arrives.
///
/// Anything else on the wire — updates for other subscribers, titles,
/// pings — is not ours to interpret here and is dropped.
async fn await_output(conn: &mut AgentConn, nonce: u16) -> Result<OutputReply, String> {
    let deadline = reply_deadline();
    loop {
        let data = conn.recv_deadline(deadline).await?;
        if data.first() != Some(&S2C_TERM_OUTPUT) {
            continue;
        }
        if let Some(reply) = parse_s2c_term_output(&data)
            && reply.nonce == nonce
        {
            return Ok(reply);
        }
    }
}

/// Where the terminal is right now, as a cursor. Nothing is read.
pub(crate) async fn probe_cursor(
    conn: &mut AgentConn,
    id: u16,
    nonce: u16,
) -> Result<(u64, u16), String> {
    conn.send(&msg_term_since(nonce, id, 0, 0, 0, SINCE_PROBE))
        .await?;
    let reply = await_output(conn, nonce).await?;
    check_status(reply.status, id)?;
    Ok((reply.next_seq, reply.next_col))
}

fn check_status(status: u8, id: u16) -> Result<(), String> {
    match status {
        STATUS_OK => Ok(()),
        STATUS_NOT_FOUND => Err(format!("pty {id}: no such terminal or command")),
        other => Err(format!("pty {id}: {}", status_text(other))),
    }
}

fn record_status(record: &CommandRecord) -> &'static str {
    if record.running() {
        "running"
    } else if record.flags & RECORD_INCOMPLETE != 0 {
        "incomplete"
    } else if record.flags & RECORD_HAS_EXIT != 0 {
        "exited"
    } else {
        "done"
    }
}

fn duration_ms(record: &CommandRecord) -> Option<u64> {
    (record.ended_ms >= record.started_ms && record.started_ms != 0 && record.ended_ms != 0)
        .then(|| record.ended_ms - record.started_ms)
}

fn record_json(record: &CommandRecord) -> serde_json::Value {
    serde_json::json!({
        "index": record.index,
        "command": record.command,
        "status": record_status(record),
        "exit": record.exit(),
        "running": record.running(),
        "start_seq": record.start_seq,
        "end_seq": record.end_seq,
        "cursor": format_cursor(record.start_seq, 0),
        "started_ms": record.started_ms,
        "ended_ms": record.ended_ms,
        "duration_ms": duration_ms(record),
        "command_known": record.flags & RECORD_NO_COMMAND == 0,
        "incomplete": record.flags & RECORD_INCOMPLETE != 0,
        "evicted": record.flags & RECORD_EVICTED != 0,
        "pty_exited": record.flags & RECORD_PTY_EXITED != 0,
    })
}

fn print_record_tsv(record: &CommandRecord) {
    let exit = record.exit().map(|c| c.to_string());
    let duration = duration_ms(record).map(|ms| ms.to_string());
    println!(
        "{}\t{}\t{}\t{}\t{}\t{}\t{}",
        record.index,
        record_status(record),
        exit.as_deref().unwrap_or("-"),
        duration.as_deref().unwrap_or("-"),
        record.start_seq,
        record.end_seq,
        // Commands can contain anything; keep the row one line.
        record.command.replace(['\t', '\n'], " "),
    );
}

/// `blit terminal journal ID` — the commands this terminal has run.
pub async fn cmd_journal(
    transport: Transport,
    id: u16,
    from: Option<u64>,
    limit: u16,
    json: bool,
) -> Result<i32, String> {
    let mut conn = AgentConn::connect(transport).await?;
    require_feature(&conn, id)?;

    // Without --from the interesting end is the newest, so the request
    // counts back from there rather than paging up from the oldest.
    let (from_index, flags) = match from {
        Some(index) => (index, 0),
        None => (0, JOURNAL_TAIL),
    };
    conn.send(&msg_term_journal(NONCE, id, from_index, limit, flags))
        .await?;

    let deadline = reply_deadline();
    loop {
        let data = conn.recv_deadline(deadline).await?;
        if data.first() != Some(&S2C_TERM_JOURNAL) {
            continue;
        }
        let Some(reply) = parse_s2c_term_journal(&data) else {
            continue;
        };
        if reply.nonce != NONCE {
            continue;
        }
        check_status(reply.status, id)?;
        if json {
            for record in &reply.records {
                println!("{}", record_json(record));
            }
        } else {
            println!("INDEX\tSTATUS\tEXIT\tMS\tSTART_SEQ\tEND_SEQ\tCOMMAND");
            for record in &reply.records {
                print_record_tsv(record);
            }
            if reply.records.is_empty() {
                // An empty journal is nearly always a missing shell hook
                // rather than an idle terminal, and that is not obvious.
                eprintln!(
                    "blit: no commands recorded — does the shell emit OSC 133? \
                     See docs/shell-integration.md"
                );
            }
        }
        return Ok(0);
    }
}

/// Wait for a command to finish, returning the record the server settled on.
async fn wait_for_command(
    conn: &mut AgentConn,
    id: u16,
    index: u64,
    timeout_secs: u64,
) -> Result<CommandRecord, String> {
    let timeout_ms = timeout_secs.saturating_mul(1000).min(u32::MAX as u64) as u32;
    conn.send(&msg_term_journal_wait(NONCE, id, index, timeout_ms))
        .await?;
    // One second of slack over the server's own timeout: the server always
    // answers, and a client that gives up first would report a timeout that
    // did not happen.
    let deadline = tokio::time::Instant::now()
        + std::time::Duration::from_secs(timeout_secs.saturating_add(1));
    loop {
        let data = conn.recv_deadline(deadline).await.map_err(|e| {
            if e == "timeout" {
                format!("pty {id}: server did not answer the wait")
            } else {
                e
            }
        })?;
        if data.first() != Some(&S2C_TERM_COMMAND) {
            continue;
        }
        let Some((nonce, _pty, status, record)) = parse_s2c_term_command(&data) else {
            continue;
        };
        if nonce != NONCE {
            continue;
        }
        if status == STATUS_NOT_FOUND {
            return Err(format!(
                "pty {id}: no such command (nothing running, or it was evicted)"
            ));
        }
        check_status(status, id)?;
        return Ok(record);
    }
}

/// `blit terminal output ID [INDEX]` — one command's output.
///
/// With `--wait` this is the closest thing to "run it and give me the
/// result": block until the command finishes, print what it printed, and
/// exit with its status.
pub async fn cmd_output(
    transport: Transport,
    id: u16,
    index: Option<u64>,
    wait: Option<u64>,
    max_bytes: u32,
    json: bool,
) -> Result<i32, String> {
    let mut conn = AgentConn::connect(transport).await?;
    require_feature(&conn, id)?;

    let index = index.unwrap_or(JOURNAL_INDEX_LATEST);
    let record = match wait {
        Some(timeout) => Some(wait_for_command(&mut conn, id, index, timeout).await?),
        None => None,
    };
    // The wait latches whichever command it attached to, so fetch that one
    // rather than re-resolving "latest" against a shell that has moved on.
    let index = record.as_ref().map(|r| r.index).unwrap_or(index);

    conn.send(&msg_term_output(NONCE + 1, id, index, max_bytes, 0))
        .await?;
    let reply = await_output(&mut conn, NONCE + 1).await?;
    check_status(reply.status, id)?;

    let truncated = reply.flags & OUTPUT_TRUNCATED != 0;
    let evicted = reply.flags & OUTPUT_EVICTED != 0;
    if json {
        let mut value = match &record {
            Some(record) => record_json(record),
            None => serde_json::json!({ "index": index }),
        };
        let object = value.as_object_mut().expect("record json is an object");
        object.insert("text".into(), reply.text.clone().into());
        object.insert("truncated".into(), truncated.into());
        object.insert("output_evicted".into(), evicted.into());
        object.insert(
            "next_cursor".into(),
            format_cursor(reply.next_seq, reply.next_col).into(),
        );
        println!("{value}");
    } else {
        print!("{}", reply.text);
        if !reply.text.ends_with('\n') && !reply.text.is_empty() {
            println!();
        }
        if evicted {
            eprintln!("blit: output start had scrolled out of the backlog");
        }
        if truncated {
            eprintln!(
                "blit: truncated at {max_bytes} bytes; continue from cursor {}",
                format_cursor(reply.next_seq, reply.next_col)
            );
        }
    }

    // With --wait the command's own status is the useful exit code; a plain
    // fetch only reports whether the fetch worked.
    Ok(match record.as_ref().and_then(|r| r.exit()) {
        Some(code) if code >= 0 => code.min(255),
        Some(code) => 128 + (-code).min(127),
        // Timed out: the record came back still running.
        None if record.as_ref().is_some_and(|r| r.running()) => 124,
        None => 0,
    })
}

/// `blit terminal history ID --since CURSOR` — everything appended since a
/// cursor, with the cursor to use next time.
pub async fn cmd_since(
    transport: Transport,
    id: u16,
    cursor: Cursor,
    max_bytes: u32,
    json: bool,
) -> Result<i32, String> {
    let mut conn = AgentConn::connect(transport).await?;
    require_feature(&conn, id)?;

    let reply = match cursor {
        Cursor::Now => {
            let (seq, col) = probe_cursor(&mut conn, id, NONCE).await?;
            OutputReply {
                nonce: NONCE,
                pty_id: id,
                status: STATUS_OK,
                flags: 0,
                start_seq: seq,
                start_col: col,
                next_seq: seq,
                next_col: col,
                text: String::new(),
            }
        }
        Cursor::At(seq, col) => {
            conn.send(&msg_term_since(NONCE, id, seq, col, max_bytes, 0))
                .await?;
            let reply = await_output(&mut conn, NONCE).await?;
            check_status(reply.status, id)?;
            reply
        }
    };

    let next = format_cursor(reply.next_seq, reply.next_col);
    if json {
        println!(
            "{}",
            serde_json::json!({
                "pty": id,
                "text": reply.text,
                "cursor": format_cursor(reply.start_seq, reply.start_col),
                "next_cursor": next,
                "truncated": reply.flags & OUTPUT_TRUNCATED != 0,
                "evicted": reply.flags & OUTPUT_EVICTED != 0,
                "alt_screen": reply.flags & OUTPUT_ALT_SCREEN != 0,
            })
        );
    } else {
        print!("{}", reply.text);
        if !reply.text.is_empty() && !reply.text.ends_with('\n') {
            println!();
        }
        // stdout stays exactly the terminal's bytes, so the cursor — which
        // the caller needs for the next read — goes to stderr.
        eprintln!("cursor: {next}");
    }
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cursors_parse_in_every_spelling() {
        assert!(matches!(parse_cursor("now").unwrap(), Cursor::Now));
        assert!(matches!(parse_cursor("end").unwrap(), Cursor::Now));
        assert!(matches!(parse_cursor("start").unwrap(), Cursor::At(0, 0)));
        assert!(matches!(parse_cursor("42").unwrap(), Cursor::At(42, 0)));
        assert!(matches!(parse_cursor("42:7").unwrap(), Cursor::At(42, 7)));
    }

    #[test]
    fn a_printed_cursor_parses_back() {
        let text = format_cursor(1234, 56);
        assert!(matches!(parse_cursor(&text).unwrap(), Cursor::At(1234, 56)));
    }

    #[test]
    fn nonsense_cursors_are_rejected() {
        for text in ["", "-1", "abc", "1:2:3", "1:x", "1:70000"] {
            assert!(parse_cursor(text).is_err(), "{text} must not parse");
        }
    }

    #[test]
    fn status_names_follow_the_flags() {
        let running = CommandRecord {
            flags: blit_remote::journal::RECORD_RUNNING,
            ..CommandRecord::default()
        };
        assert_eq!(record_status(&running), "running");
        let exited = CommandRecord {
            flags: RECORD_HAS_EXIT,
            exit_code: 3,
            ..CommandRecord::default()
        };
        assert_eq!(record_status(&exited), "exited");
        assert_eq!(exited.exit(), Some(3));
        let interrupted = CommandRecord {
            flags: RECORD_INCOMPLETE,
            ..CommandRecord::default()
        };
        assert_eq!(record_status(&interrupted), "incomplete");
        assert_eq!(record_status(&CommandRecord::default()), "done");
    }

    #[test]
    fn duration_needs_both_timestamps() {
        let record = CommandRecord {
            started_ms: 1000,
            ended_ms: 1750,
            ..CommandRecord::default()
        };
        assert_eq!(duration_ms(&record), Some(750));
        assert_eq!(
            duration_ms(&CommandRecord {
                started_ms: 1000,
                ..CommandRecord::default()
            }),
            None
        );
    }

    #[test]
    fn json_carries_what_an_agent_branches_on() {
        let record = CommandRecord {
            index: 4,
            flags: RECORD_HAS_EXIT,
            exit_code: 1,
            start_seq: 100,
            end_seq: 120,
            started_ms: 10,
            ended_ms: 30,
            command: "false".into(),
        };
        let value = record_json(&record);
        assert_eq!(value["index"], 4);
        assert_eq!(value["exit"], 1);
        assert_eq!(value["running"], false);
        assert_eq!(value["status"], "exited");
        assert_eq!(value["duration_ms"], 20);
        assert_eq!(value["cursor"], "100:0");
    }
}
