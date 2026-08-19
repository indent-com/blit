//! Per-command terminal journal and sequence-addressed output
//! (docs/design/term-journal.md).
//!
//! A shell that emits OSC 133 semantic-prompt markers tells the server where
//! each command begins, where its output starts, and how it ended. The server
//! turns that into a bounded ring of [`CommandRecord`]s per PTY and hands a
//! client the record list, one command's output, or everything appended since
//! a cursor — the three questions an agent driving a long-lived shell actually
//! asks.
//!
//! Output is addressed by **sequence**, not by grid coordinate. A sequence is
//! the absolute index of a grid line since the PTY was created
//! (`scrolled_lines + row`), so it stays valid as scrollback evicts underneath
//! it: an evicted region is detectable (`oldest_seq`) rather than silently
//! misread. `(seq, col)` together form a byte-exact cursor.
//!
//! All integers little-endian, tightly packed, as everywhere in the protocol.

/// List journal records: [0x50][nonce:2][pty_id:2][from_index:8][limit:2][flags:1]
/// `from_index` is the first record wanted; with [`JOURNAL_TAIL`] it instead
/// counts back from the newest, so `from_index = 0` means "the last `limit`".
pub const C2S_TERM_JOURNAL: u8 = 0x50;
/// Fetch one command's output: [0x51][nonce:2][pty_id:2][index:8][max_bytes:4][flags:1]
/// `index` = [`JOURNAL_INDEX_LATEST`] selects the newest record.
pub const C2S_TERM_OUTPUT: u8 = 0x51;
/// Read everything appended since a cursor:
/// [0x52][nonce:2][pty_id:2][from_seq:8][from_col:2][max_bytes:4][flags:1]
/// Answered with `S2C_TERM_OUTPUT`, whose `next_seq`/`next_col` are the
/// cursor to send back next time.
pub const C2S_TERM_SINCE: u8 = 0x52;
/// Block until a command finishes: [0x53][nonce:2][pty_id:2][index:8][timeout_ms:4]
/// `index` = [`JOURNAL_INDEX_LATEST`] waits on whatever is running now, or on
/// the next command to start if the shell is at a prompt. Exactly one
/// `S2C_TERM_COMMAND` comes back per nonce; on timeout it carries the record
/// as it stands, still flagged [`RECORD_RUNNING`].
pub const C2S_TERM_JOURNAL_WAIT: u8 = 0x53;
/// Block until output appears after a cursor:
/// [0x54][nonce:2][pty_id:2][from_seq:8][from_col:2][max_bytes:4][timeout_ms:4][flags:1][needle_len:2][needle:N]
///
/// The output counterpart of [`C2S_TERM_JOURNAL_WAIT`], which waits on a
/// *command record* and so only ever fires for a PTY whose shell emits OSC 133.
/// A process exec'd directly emits no markers and has no records, so "wait
/// until this program says it is listening" needs a wait on text instead.
///
/// A non-empty `needle` waits for that substring; an empty one waits for any
/// output at all. Exactly one `S2C_TERM_OUTPUT` comes back per nonce. On a
/// match it is flagged [`OUTPUT_MATCHED`] and `next_seq` names the line after
/// the one the needle completed on, so re-arming from it neither repeats the
/// match nor skips what followed it. On timeout the flag is absent and the
/// reply carries whatever did arrive, so a caller keeps its cursor moving
/// rather than re-reading from the start.
pub const C2S_TERM_WAIT: u8 = 0x54;

/// Journal listing: [0x50][nonce:2][pty_id:2][status:1][oldest_index:8][next_index:8][count:2][records…]
/// `oldest_index` is the lowest index still retained (records below it were
/// evicted by the ring bound); `next_index` is the index the next command
/// will take, so `next_index - oldest_index` is what the journal holds.
pub const S2C_TERM_JOURNAL: u8 = 0x50;
/// Output: [0x51][nonce:2][pty_id:2][status:1][flags:1][start_seq:8][start_col:2][next_seq:8][next_col:2][text_len:4][text:N]
/// Answers both `C2S_TERM_OUTPUT` and `C2S_TERM_SINCE`; `start_*` is where
/// the returned text actually begins after clamping, `next_*` is the cursor
/// to resume from.
pub const S2C_TERM_OUTPUT: u8 = 0x51;
/// One command record: [0x52][nonce:2][pty_id:2][status:1][record]
/// Answer to `C2S_TERM_JOURNAL_WAIT`.
pub const S2C_TERM_COMMAND: u8 = 0x52;

/// `S2C_HELLO` feature bit: the server keeps a per-command journal driven by
/// OSC 133 and serves sequence-addressed output (docs/design/term-journal.md).
/// `BLIT_TERM_JOURNAL=0` withholds the bit *and* refuses every nonce-bearing
/// request with `PERMISSION`, so a client that ignores feature bits still
/// gets its one reply.
pub const FEATURE_TERM_JOURNAL: u32 = 1 << 28;

/// `from_index` counts back from the newest record rather than up from the
/// oldest. `C2S_TERM_JOURNAL` flags bit 0.
pub const JOURNAL_TAIL: u8 = 1 << 0;

/// `C2S_TERM_SINCE` flags bit 0: report the current cursor and return no
/// text. This is how a client establishes a starting cursor without first
/// pulling everything already on screen.
pub const SINCE_PROBE: u8 = 1 << 0;

/// `index` sentinel meaning "the newest record" (`C2S_TERM_OUTPUT`) or "the
/// running command, else the next one to start" (`C2S_TERM_JOURNAL_WAIT`).
pub const JOURNAL_INDEX_LATEST: u64 = u64::MAX;

// S2C_TERM_OUTPUT flags.
/// `max_bytes` cut the response short. `next_seq`/`next_col` name the first
/// line not included, so the client pages forward by re-asking from there.
pub const OUTPUT_TRUNCATED: u8 = 1 << 0;
/// The requested start had already scrolled out of the scrollback; the text
/// begins at the oldest line still retained.
pub const OUTPUT_EVICTED: u8 = 1 << 1;
/// The PTY is on the alternate screen, where sequences do not advance
/// (docs/design/term-journal.md § Alternate screen).
pub const OUTPUT_ALT_SCREEN: u8 = 1 << 2;
/// A [`C2S_TERM_WAIT`] found its needle. Absent on the reply a timeout
/// produces, which is otherwise the same shape — so this bit is how a caller
/// tells "it said it was ready" from "it has not said so yet".
pub const OUTPUT_MATCHED: u8 = 1 << 3;

// CommandRecord flags.
/// The command has not finished.
pub const RECORD_RUNNING: u8 = 1 << 0;
/// `exit_code` is meaningful. Absent when the shell's `D` marker carried no
/// status, which is common in the wild.
pub const RECORD_HAS_EXIT: u8 = 1 << 1;
/// The command line could not be recovered — an `OSC 133 ; C` arrived with no
/// preceding `B` to delimit the input region.
pub const RECORD_NO_COMMAND: u8 = 1 << 2;
/// Closed by a new prompt rather than by a `D` marker: the shell was
/// interrupted, reset, or emits only a subset of the markers.
pub const RECORD_INCOMPLETE: u8 = 1 << 3;
/// The start of this command's output has been evicted from the scrollback,
/// so fetching it returns only the surviving tail.
pub const RECORD_EVICTED: u8 = 1 << 4;
/// The PTY's process exited while this command was still running.
pub const RECORD_PTY_EXITED: u8 = 1 << 5;

/// Fixed-size head of an encoded record, before the command text.
const RECORD_HEAD: usize = 8 + 1 + 4 + 8 + 8 + 8 + 8 + 2;

/// One command as the server observed it between OSC 133 markers.
///
/// The output region is a half-open sequence range `[start_seq, end_seq)` of
/// absolute grid lines. While the command runs, `end_seq` is the live bottom
/// and moves; once it completes it is frozen.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CommandRecord {
    /// Monotonic per PTY, stable across reads, never reused.
    pub index: u64,
    pub flags: u8,
    /// Meaningful only under [`RECORD_HAS_EXIT`].
    pub exit_code: i32,
    /// First grid line of the command's output.
    pub start_seq: u64,
    /// One past the last grid line of the output.
    pub end_seq: u64,
    /// Unix epoch milliseconds; 0 when unknown.
    pub started_ms: u64,
    /// Unix epoch milliseconds; 0 while running.
    pub ended_ms: u64,
    /// The command line, recovered from the region between the `B` and `C`
    /// markers (or given verbatim by `OSC 633 ; E`). Empty under
    /// [`RECORD_NO_COMMAND`].
    pub command: String,
}

impl CommandRecord {
    pub fn running(&self) -> bool {
        self.flags & RECORD_RUNNING != 0
    }

    pub fn exit(&self) -> Option<i32> {
        (self.flags & RECORD_HAS_EXIT != 0).then_some(self.exit_code)
    }

    pub fn encode(&self, out: &mut Vec<u8>) {
        let cmd = self.command.as_bytes();
        let cmd_len = cmd.len().min(u16::MAX as usize);
        out.reserve(RECORD_HEAD + cmd_len);
        out.extend_from_slice(&self.index.to_le_bytes());
        out.push(self.flags);
        out.extend_from_slice(&self.exit_code.to_le_bytes());
        out.extend_from_slice(&self.start_seq.to_le_bytes());
        out.extend_from_slice(&self.end_seq.to_le_bytes());
        out.extend_from_slice(&self.started_ms.to_le_bytes());
        out.extend_from_slice(&self.ended_ms.to_le_bytes());
        out.extend_from_slice(&(cmd_len as u16).to_le_bytes());
        out.extend_from_slice(&cmd[..cmd_len]);
    }

    /// Decode one record, returning it with the number of bytes consumed.
    pub fn decode(buf: &[u8]) -> Option<(Self, usize)> {
        if buf.len() < RECORD_HEAD {
            return None;
        }
        let u64_at = |o: usize| u64::from_le_bytes(buf[o..o + 8].try_into().unwrap());
        let cmd_len = u16::from_le_bytes([buf[RECORD_HEAD - 2], buf[RECORD_HEAD - 1]]) as usize;
        let end = RECORD_HEAD + cmd_len;
        if buf.len() < end {
            return None;
        }
        let record = Self {
            index: u64_at(0),
            flags: buf[8],
            exit_code: i32::from_le_bytes(buf[9..13].try_into().unwrap()),
            start_seq: u64_at(13),
            end_seq: u64_at(21),
            started_ms: u64_at(29),
            ended_ms: u64_at(37),
            command: String::from_utf8_lossy(&buf[RECORD_HEAD..end]).into_owned(),
        };
        Some((record, end))
    }
}

/// A parsed `C2S_TERM_JOURNAL`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct JournalRequest {
    pub nonce: u16,
    pub pty_id: u16,
    pub from_index: u64,
    pub limit: u16,
    pub flags: u8,
}

/// A parsed `C2S_TERM_OUTPUT`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct OutputRequest {
    pub nonce: u16,
    pub pty_id: u16,
    pub index: u64,
    pub max_bytes: u32,
    pub flags: u8,
}

/// A parsed `C2S_TERM_SINCE`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SinceRequest {
    pub nonce: u16,
    pub pty_id: u16,
    pub from_seq: u64,
    pub from_col: u16,
    pub max_bytes: u32,
    pub flags: u8,
}

/// A parsed `C2S_TERM_JOURNAL_WAIT`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WaitRequest {
    pub nonce: u16,
    pub pty_id: u16,
    pub index: u64,
    pub timeout_ms: u32,
}

/// A decoded `S2C_TERM_OUTPUT`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OutputReply {
    pub nonce: u16,
    pub pty_id: u16,
    pub status: u8,
    pub flags: u8,
    pub start_seq: u64,
    pub start_col: u16,
    pub next_seq: u64,
    pub next_col: u16,
    pub text: String,
}

/// A decoded `S2C_TERM_JOURNAL`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct JournalReply {
    pub nonce: u16,
    pub pty_id: u16,
    pub status: u8,
    pub oldest_index: u64,
    pub next_index: u64,
    pub records: Vec<CommandRecord>,
}

fn u16_at(buf: &[u8], o: usize) -> u16 {
    u16::from_le_bytes([buf[o], buf[o + 1]])
}

fn u32_at(buf: &[u8], o: usize) -> u32 {
    u32::from_le_bytes(buf[o..o + 4].try_into().unwrap())
}

fn u64_at(buf: &[u8], o: usize) -> u64 {
    u64::from_le_bytes(buf[o..o + 8].try_into().unwrap())
}

pub fn msg_term_journal(
    nonce: u16,
    pty_id: u16,
    from_index: u64,
    limit: u16,
    flags: u8,
) -> Vec<u8> {
    let mut msg = Vec::with_capacity(16);
    msg.push(C2S_TERM_JOURNAL);
    msg.extend_from_slice(&nonce.to_le_bytes());
    msg.extend_from_slice(&pty_id.to_le_bytes());
    msg.extend_from_slice(&from_index.to_le_bytes());
    msg.extend_from_slice(&limit.to_le_bytes());
    msg.push(flags);
    msg
}

pub fn parse_term_journal(data: &[u8]) -> Option<JournalRequest> {
    if data.len() < 16 || data[0] != C2S_TERM_JOURNAL {
        return None;
    }
    Some(JournalRequest {
        nonce: u16_at(data, 1),
        pty_id: u16_at(data, 3),
        from_index: u64_at(data, 5),
        limit: u16_at(data, 13),
        flags: data[15],
    })
}

pub fn msg_term_output(nonce: u16, pty_id: u16, index: u64, max_bytes: u32, flags: u8) -> Vec<u8> {
    let mut msg = Vec::with_capacity(18);
    msg.push(C2S_TERM_OUTPUT);
    msg.extend_from_slice(&nonce.to_le_bytes());
    msg.extend_from_slice(&pty_id.to_le_bytes());
    msg.extend_from_slice(&index.to_le_bytes());
    msg.extend_from_slice(&max_bytes.to_le_bytes());
    msg.push(flags);
    msg
}

pub fn parse_term_output(data: &[u8]) -> Option<OutputRequest> {
    if data.len() < 18 || data[0] != C2S_TERM_OUTPUT {
        return None;
    }
    Some(OutputRequest {
        nonce: u16_at(data, 1),
        pty_id: u16_at(data, 3),
        index: u64_at(data, 5),
        max_bytes: u32_at(data, 13),
        flags: data[17],
    })
}

pub fn msg_term_since(
    nonce: u16,
    pty_id: u16,
    from_seq: u64,
    from_col: u16,
    max_bytes: u32,
    flags: u8,
) -> Vec<u8> {
    let mut msg = Vec::with_capacity(20);
    msg.push(C2S_TERM_SINCE);
    msg.extend_from_slice(&nonce.to_le_bytes());
    msg.extend_from_slice(&pty_id.to_le_bytes());
    msg.extend_from_slice(&from_seq.to_le_bytes());
    msg.extend_from_slice(&from_col.to_le_bytes());
    msg.extend_from_slice(&max_bytes.to_le_bytes());
    msg.push(flags);
    msg
}

pub fn parse_term_since(data: &[u8]) -> Option<SinceRequest> {
    if data.len() < 20 || data[0] != C2S_TERM_SINCE {
        return None;
    }
    Some(SinceRequest {
        nonce: u16_at(data, 1),
        pty_id: u16_at(data, 3),
        from_seq: u64_at(data, 5),
        from_col: u16_at(data, 13),
        max_bytes: u32_at(data, 15),
        flags: data[19],
    })
}

/// A decoded [`C2S_TERM_WAIT`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OutputWaitRequest<'a> {
    pub nonce: u16,
    pub pty_id: u16,
    pub from_seq: u64,
    pub from_col: u16,
    pub max_bytes: u32,
    pub timeout_ms: u32,
    pub flags: u8,
    /// Empty waits for any output at all.
    pub needle: &'a str,
}

/// Header bytes before the needle.
pub const TERM_WAIT_HEADER: usize = 26;

/// Longest needle a wait may carry. A readiness marker is a phrase, not a
/// document, and the string is compared against every read until it matches.
pub const TERM_WAIT_NEEDLE_MAX: usize = 4096;

#[allow(clippy::too_many_arguments)]
pub fn msg_term_wait(
    nonce: u16,
    pty_id: u16,
    from_seq: u64,
    from_col: u16,
    max_bytes: u32,
    timeout_ms: u32,
    flags: u8,
    needle: &str,
) -> Vec<u8> {
    let needle = &needle[..needle.len().min(TERM_WAIT_NEEDLE_MAX)];
    let mut msg = Vec::with_capacity(TERM_WAIT_HEADER + needle.len());
    msg.push(C2S_TERM_WAIT);
    msg.extend_from_slice(&nonce.to_le_bytes());
    msg.extend_from_slice(&pty_id.to_le_bytes());
    msg.extend_from_slice(&from_seq.to_le_bytes());
    msg.extend_from_slice(&from_col.to_le_bytes());
    msg.extend_from_slice(&max_bytes.to_le_bytes());
    msg.extend_from_slice(&timeout_ms.to_le_bytes());
    msg.push(flags);
    msg.extend_from_slice(&(needle.len() as u16).to_le_bytes());
    msg.extend_from_slice(needle.as_bytes());
    msg
}

pub fn parse_term_wait(data: &[u8]) -> Option<OutputWaitRequest<'_>> {
    if data.len() < TERM_WAIT_HEADER || data[0] != C2S_TERM_WAIT {
        return None;
    }
    let needle_len = u16_at(data, 24) as usize;
    if needle_len > TERM_WAIT_NEEDLE_MAX || data.len() < TERM_WAIT_HEADER + needle_len {
        return None;
    }
    Some(OutputWaitRequest {
        nonce: u16_at(data, 1),
        pty_id: u16_at(data, 3),
        from_seq: u64_at(data, 5),
        from_col: u16_at(data, 13),
        max_bytes: u32_at(data, 15),
        timeout_ms: u32_at(data, 19),
        flags: data[23],
        needle: core::str::from_utf8(&data[TERM_WAIT_HEADER..TERM_WAIT_HEADER + needle_len])
            .ok()?,
    })
}

pub fn msg_term_journal_wait(nonce: u16, pty_id: u16, index: u64, timeout_ms: u32) -> Vec<u8> {
    let mut msg = Vec::with_capacity(17);
    msg.push(C2S_TERM_JOURNAL_WAIT);
    msg.extend_from_slice(&nonce.to_le_bytes());
    msg.extend_from_slice(&pty_id.to_le_bytes());
    msg.extend_from_slice(&index.to_le_bytes());
    msg.extend_from_slice(&timeout_ms.to_le_bytes());
    msg
}

pub fn parse_term_journal_wait(data: &[u8]) -> Option<WaitRequest> {
    if data.len() < 17 || data[0] != C2S_TERM_JOURNAL_WAIT {
        return None;
    }
    Some(WaitRequest {
        nonce: u16_at(data, 1),
        pty_id: u16_at(data, 3),
        index: u64_at(data, 5),
        timeout_ms: u32_at(data, 13),
    })
}

pub fn msg_s2c_term_journal(
    nonce: u16,
    pty_id: u16,
    status: u8,
    oldest_index: u64,
    next_index: u64,
    records: &[CommandRecord],
) -> Vec<u8> {
    let count = records.len().min(u16::MAX as usize);
    let mut msg = Vec::with_capacity(24 + count * RECORD_HEAD);
    msg.push(S2C_TERM_JOURNAL);
    msg.extend_from_slice(&nonce.to_le_bytes());
    msg.extend_from_slice(&pty_id.to_le_bytes());
    msg.push(status);
    msg.extend_from_slice(&oldest_index.to_le_bytes());
    msg.extend_from_slice(&next_index.to_le_bytes());
    msg.extend_from_slice(&(count as u16).to_le_bytes());
    for record in &records[..count] {
        record.encode(&mut msg);
    }
    msg
}

pub fn parse_s2c_term_journal(data: &[u8]) -> Option<JournalReply> {
    if data.len() < 24 || data[0] != S2C_TERM_JOURNAL {
        return None;
    }
    let count = u16_at(data, 22) as usize;
    let mut records = Vec::with_capacity(count);
    let mut at = 24;
    for _ in 0..count {
        let (record, used) = CommandRecord::decode(&data[at..])?;
        records.push(record);
        at += used;
    }
    Some(JournalReply {
        nonce: u16_at(data, 1),
        pty_id: u16_at(data, 3),
        status: data[5],
        oldest_index: u64_at(data, 6),
        next_index: u64_at(data, 14),
        records,
    })
}

#[allow(clippy::too_many_arguments)]
pub fn msg_s2c_term_output(
    nonce: u16,
    pty_id: u16,
    status: u8,
    flags: u8,
    start_seq: u64,
    start_col: u16,
    next_seq: u64,
    next_col: u16,
    text: &str,
) -> Vec<u8> {
    let mut msg = Vec::with_capacity(31 + text.len());
    msg.push(S2C_TERM_OUTPUT);
    msg.extend_from_slice(&nonce.to_le_bytes());
    msg.extend_from_slice(&pty_id.to_le_bytes());
    msg.push(status);
    msg.push(flags);
    msg.extend_from_slice(&start_seq.to_le_bytes());
    msg.extend_from_slice(&start_col.to_le_bytes());
    msg.extend_from_slice(&next_seq.to_le_bytes());
    msg.extend_from_slice(&next_col.to_le_bytes());
    msg.extend_from_slice(&(text.len() as u32).to_le_bytes());
    msg.extend_from_slice(text.as_bytes());
    msg
}

pub fn parse_s2c_term_output(data: &[u8]) -> Option<OutputReply> {
    if data.len() < 31 || data[0] != S2C_TERM_OUTPUT {
        return None;
    }
    let text_len = u32_at(data, 27) as usize;
    if data.len() < 31 + text_len {
        return None;
    }
    Some(OutputReply {
        nonce: u16_at(data, 1),
        pty_id: u16_at(data, 3),
        status: data[5],
        flags: data[6],
        start_seq: u64_at(data, 7),
        start_col: u16_at(data, 15),
        next_seq: u64_at(data, 17),
        next_col: u16_at(data, 25),
        text: String::from_utf8_lossy(&data[31..31 + text_len]).into_owned(),
    })
}

pub fn msg_s2c_term_command(
    nonce: u16,
    pty_id: u16,
    status: u8,
    record: &CommandRecord,
) -> Vec<u8> {
    let mut msg = Vec::with_capacity(6 + RECORD_HEAD);
    msg.push(S2C_TERM_COMMAND);
    msg.extend_from_slice(&nonce.to_le_bytes());
    msg.extend_from_slice(&pty_id.to_le_bytes());
    msg.push(status);
    record.encode(&mut msg);
    msg
}

/// Returns `(nonce, pty_id, status, record)`.
pub fn parse_s2c_term_command(data: &[u8]) -> Option<(u16, u16, u8, CommandRecord)> {
    if data.len() < 6 || data[0] != S2C_TERM_COMMAND {
        return None;
    }
    let (record, _) = CommandRecord::decode(&data[6..])?;
    Some((u16_at(data, 1), u16_at(data, 3), data[5], record))
}

/// The nonce of any journal-family C2S message, for the blanket refusal a
/// disabled family owes every request (`BLIT_TERM_JOURNAL=0`).
pub fn journal_nonce(data: &[u8]) -> Option<u16> {
    if data.len() < 5 {
        return None;
    }
    matches!(
        data[0],
        C2S_TERM_JOURNAL | C2S_TERM_OUTPUT | C2S_TERM_SINCE | C2S_TERM_JOURNAL_WAIT
    )
    .then(|| u16_at(data, 1))
}

/// The refusal a disabled family sends for `data`, shaped like the reply the
/// caller was waiting for so one nonce still gets one answer.
pub fn refusal(data: &[u8], status: u8) -> Option<Vec<u8>> {
    if data.len() < 5 {
        return None;
    }
    let nonce = u16_at(data, 1);
    let pty_id = u16_at(data, 3);
    match data[0] {
        C2S_TERM_JOURNAL => Some(msg_s2c_term_journal(nonce, pty_id, status, 0, 0, &[])),
        C2S_TERM_OUTPUT | C2S_TERM_SINCE => Some(msg_s2c_term_output(
            nonce, pty_id, status, 0, 0, 0, 0, 0, "",
        )),
        C2S_TERM_JOURNAL_WAIT => Some(msg_s2c_term_command(
            nonce,
            pty_id,
            status,
            &CommandRecord::default(),
        )),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> CommandRecord {
        CommandRecord {
            index: 7,
            flags: RECORD_HAS_EXIT | RECORD_EVICTED,
            exit_code: -1,
            start_seq: 1200,
            end_seq: 1234,
            started_ms: 1_700_000_000_000,
            ended_ms: 1_700_000_001_500,
            command: "cargo test --workspace".into(),
        }
    }

    #[test]
    fn record_round_trips() {
        let record = sample();
        let mut buf = Vec::new();
        record.encode(&mut buf);
        let (back, used) = CommandRecord::decode(&buf).expect("decode");
        assert_eq!(back, record);
        assert_eq!(used, buf.len());
        assert_eq!(back.exit(), Some(-1));
        assert!(!back.running());
    }

    #[test]
    fn record_decode_refuses_short_buffers() {
        let mut buf = Vec::new();
        sample().encode(&mut buf);
        for cut in 0..buf.len() {
            assert!(
                CommandRecord::decode(&buf[..cut]).is_none(),
                "a {cut}-byte prefix must not decode"
            );
        }
    }

    #[test]
    fn journal_request_round_trips() {
        let msg = msg_term_journal(9, 3, 42, 20, JOURNAL_TAIL);
        let req = parse_term_journal(&msg).expect("parse");
        assert_eq!(
            req,
            JournalRequest {
                nonce: 9,
                pty_id: 3,
                from_index: 42,
                limit: 20,
                flags: JOURNAL_TAIL,
            }
        );
    }

    #[test]
    fn since_request_round_trips() {
        let msg = msg_term_since(1, 2, u64::MAX, 17, 65536, SINCE_PROBE);
        let req = parse_term_since(&msg).expect("parse");
        assert_eq!(req.from_seq, u64::MAX);
        assert_eq!(req.from_col, 17);
        assert_eq!(req.max_bytes, 65536);
        assert_eq!(req.flags, SINCE_PROBE);
    }

    #[test]
    fn output_and_wait_requests_round_trip() {
        let out = msg_term_output(4, 5, JOURNAL_INDEX_LATEST, 4096, 0);
        let req = parse_term_output(&out).expect("parse");
        assert_eq!(req.index, JOURNAL_INDEX_LATEST);
        assert_eq!(req.max_bytes, 4096);

        let wait = msg_term_journal_wait(6, 7, 11, 30_000);
        let req = parse_term_journal_wait(&wait).expect("parse");
        assert_eq!(
            req,
            WaitRequest {
                nonce: 6,
                pty_id: 7,
                index: 11,
                timeout_ms: 30_000,
            }
        );
    }

    #[test]
    fn parsers_reject_the_wrong_opcode() {
        let msg = msg_term_journal(1, 1, 0, 1, 0);
        assert!(parse_term_output(&msg).is_none());
        assert!(parse_term_since(&msg).is_none());
        assert!(parse_term_journal_wait(&msg).is_none());
    }

    #[test]
    fn journal_reply_round_trips_with_records() {
        let records = vec![
            sample(),
            CommandRecord {
                index: 8,
                flags: RECORD_RUNNING | RECORD_NO_COMMAND,
                ..CommandRecord::default()
            },
        ];
        let msg = msg_s2c_term_journal(3, 1, crate::STATUS_OK, 5, 9, &records);
        let reply = parse_s2c_term_journal(&msg).expect("parse");
        assert_eq!(reply.nonce, 3);
        assert_eq!(reply.oldest_index, 5);
        assert_eq!(reply.next_index, 9);
        assert_eq!(reply.records, records);
        assert!(reply.records[1].running());
        assert_eq!(reply.records[1].exit(), None);
    }

    #[test]
    fn output_reply_round_trips() {
        let msg = msg_s2c_term_output(
            2,
            4,
            crate::STATUS_OK,
            OUTPUT_TRUNCATED | OUTPUT_EVICTED,
            100,
            0,
            140,
            13,
            "hello\nworld",
        );
        let reply = parse_s2c_term_output(&msg).expect("parse");
        assert_eq!(reply.start_seq, 100);
        assert_eq!(reply.next_seq, 140);
        assert_eq!(reply.next_col, 13);
        assert_eq!(reply.text, "hello\nworld");
        assert_eq!(reply.flags, OUTPUT_TRUNCATED | OUTPUT_EVICTED);
    }

    #[test]
    fn command_reply_round_trips() {
        let record = sample();
        let msg = msg_s2c_term_command(5, 6, crate::STATUS_OK, &record);
        let (nonce, pty_id, status, back) = parse_s2c_term_command(&msg).expect("parse");
        assert_eq!((nonce, pty_id, status), (5, 6, crate::STATUS_OK));
        assert_eq!(back, record);
    }

    #[test]
    fn refusals_answer_every_request_shape() {
        for msg in [
            msg_term_journal(1, 2, 0, 10, 0),
            msg_term_output(1, 2, 0, 10, 0),
            msg_term_since(1, 2, 0, 0, 10, 0),
            msg_term_journal_wait(1, 2, 0, 10),
        ] {
            assert_eq!(journal_nonce(&msg), Some(1));
            let reply = refusal(&msg, crate::STATUS_PERMISSION).expect("refusal");
            let status = match reply[0] {
                S2C_TERM_JOURNAL => parse_s2c_term_journal(&reply).map(|r| r.status),
                S2C_TERM_OUTPUT => parse_s2c_term_output(&reply).map(|r| r.status),
                S2C_TERM_COMMAND => parse_s2c_term_command(&reply).map(|r| r.2),
                other => panic!("unexpected refusal opcode {other:#04x}"),
            };
            assert_eq!(status, Some(crate::STATUS_PERMISSION));
        }
    }

    #[test]
    fn journal_nonce_ignores_foreign_opcodes() {
        assert_eq!(journal_nonce(&[crate::C2S_READ, 1, 0, 2, 0]), None);
        assert_eq!(journal_nonce(&[C2S_TERM_JOURNAL, 1]), None);
    }

    #[test]
    fn feature_bit_is_free() {
        // Bits 0-27 were taken when this family landed; the mask below is
        // every bit the tree defines, and the new one must not collide.
        let taken = crate::FEATURE_CREATE_NONCE
            | crate::FEATURE_RESTART
            | crate::FEATURE_RESIZE_BATCH
            | crate::FEATURE_COPY_RANGE
            | crate::FEATURE_COMPOSITOR
            | crate::FEATURE_AUDIO
            | crate::fs::FEATURE_FS
            | crate::git::FEATURE_GIT
            | crate::lsp::FEATURE_LSP
            | crate::kv::FEATURE_KV
            | crate::net::FEATURE_NET
            | crate::extension::FEATURE_EXTENSION
            | crate::channel::FEATURE_CHANNEL
            | crate::process::FEATURE_PROCESS
            | crate::FEATURE_CREATE_STATUS
            | crate::FEATURE_KILL_MODE
            | crate::FEATURE_PTY_DEADLINE
            | crate::FEATURE_SCROLL_BY
            | crate::FEATURE_SURFACE_TOUCH
            | crate::FEATURE_SURFACE_TEXT_INPUT
            | crate::FEATURE_CLIENT_CONTROL
            | crate::desktop::FEATURE_DESKTOP
            | crate::media::FEATURE_DESKTOP_MEDIA
            | crate::process::FEATURE_PROCESS_SESSION_ENV
            | crate::env::FEATURE_ENV
            | crate::process::FEATURE_APP_SOCKET
            | crate::channel::FEATURE_CHANNEL_WATCH
            | crate::FEATURE_CLIENT_ORIGIN;
        assert_eq!(taken & FEATURE_TERM_JOURNAL, 0);
    }
}

#[cfg(test)]
mod wait_tests {
    use super::*;

    #[test]
    fn a_wait_round_trips() {
        let msg = msg_term_wait(7, 3, 41, 5, 64, 30_000, 0, "listening on");
        let req = parse_term_wait(&msg).expect("parses");
        assert_eq!(
            req,
            OutputWaitRequest {
                nonce: 7,
                pty_id: 3,
                from_seq: 41,
                from_col: 5,
                max_bytes: 64,
                timeout_ms: 30_000,
                flags: 0,
                needle: "listening on",
            }
        );
    }

    #[test]
    fn an_empty_needle_is_legal_and_means_any_output() {
        let msg = msg_term_wait(1, 1, 0, 0, 0, 0, 0, "");
        assert_eq!(msg.len(), TERM_WAIT_HEADER);
        assert_eq!(parse_term_wait(&msg).expect("parses").needle, "");
    }

    #[test]
    fn a_truncated_or_foreign_frame_is_refused() {
        let msg = msg_term_wait(1, 1, 0, 0, 0, 0, 0, "up");
        for cut in 0..msg.len() {
            assert!(parse_term_wait(&msg[..cut]).is_none(), "cut at {cut}");
        }
        let mut foreign = msg.clone();
        foreign[0] = C2S_TERM_SINCE;
        assert!(parse_term_wait(&foreign).is_none());
    }

    #[test]
    fn a_needle_longer_than_the_cap_is_clamped_not_truncated_mid_frame() {
        let long = "x".repeat(TERM_WAIT_NEEDLE_MAX + 10);
        let msg = msg_term_wait(1, 1, 0, 0, 0, 0, 0, &long);
        let req = parse_term_wait(&msg).expect("parses");
        assert_eq!(req.needle.len(), TERM_WAIT_NEEDLE_MAX);
    }
}
