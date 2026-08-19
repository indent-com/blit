//! Per-command terminal journal driven by OSC 133 semantic-prompt markers
//! (docs/design/term-journal.md).
//!
//! A shell with shell integration enabled brackets each command with four
//! markers: `A` when the prompt starts, `B` when the prompt ends and typing
//! begins, `C` just before the command execs, and `D` when it finishes,
//! optionally carrying the exit status. This module turns that stream into a
//! bounded ring of records, each naming its output as a range of sequences
//! ([`blit_alacritty::TerminalDriver::seq_text`]) rather than of grid rows,
//! so a record stays readable until the scrollback actually evicts it.
//!
//! Shells emit subsets of the markers, emit them in the wrong order after an
//! interrupt, and sometimes emit none at all. Every transition below is
//! therefore total: there is no input sequence that leaves the machine stuck
//! or that loses a record, and a terminal whose shell emits nothing keeps an
//! empty journal at no cost.

use std::collections::VecDeque;
use std::sync::OnceLock;
use std::time::{SystemTime, UNIX_EPOCH};

use blit_remote::journal::{
    CommandRecord, RECORD_EVICTED, RECORD_HAS_EXIT, RECORD_INCOMPLETE, RECORD_NO_COMMAND,
    RECORD_PTY_EXITED, RECORD_RUNNING,
};

/// Which semantic-prompt dialect a marker was written in. A shell that emits
/// both would otherwise be counted twice, so the journal latches onto the
/// first one it sees (§ Dialects).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Dialect {
    /// `OSC 133` — the FinalTerm sequence kitty, WezTerm, foot and others
    /// implement.
    Osc133,
    /// `OSC 633` — VS Code's superset, which additionally names the command
    /// line outright instead of leaving it to be read off the grid.
    Osc633,
}

/// One semantic-prompt marker, and where in the byte stream it ended.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SemanticMark {
    pub kind: MarkKind,
    pub dialect: Dialect,
    /// Offset one past the marker's terminator in the scanned buffer. The
    /// bytes before it must be fed to the terminal before the marker is
    /// applied, or the cursor it is measured against is the wrong one.
    pub at: usize,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MarkKind {
    /// `A` — a new prompt begins.
    PromptStart,
    /// `B` — the prompt has been drawn; what follows is what the user types.
    InputStart,
    /// `C` — the command is about to run; what follows is its output.
    OutputStart,
    /// `D` — the command finished, with an exit status if the shell sent one.
    Finished(Option<i32>),
    /// `633;E` — the command line, given verbatim rather than left to be read
    /// back off the grid.
    CommandLine(String),
}

/// Longest OSC payload the scanner will hold across a chunk boundary.
///
/// Markers are a few bytes; this only has to cover one split across two PTY
/// reads. A cap is required rather than merely tidy: without one, a stream
/// that opens an OSC and never terminates it would grow this buffer without
/// bound.
pub const CARRY_MAX: usize = 512;

/// Longest command line retained per record.
pub fn command_max() -> usize {
    static CACHED: OnceLock<usize> = OnceLock::new();
    *CACHED.get_or_init(|| env_usize("BLIT_TERM_JOURNAL_CMD_MAX", 4096))
}

/// Server ceiling on a client's `max_bytes`, whatever it asked for.
pub fn output_max() -> usize {
    static CACHED: OnceLock<usize> = OnceLock::new();
    *CACHED.get_or_init(|| env_usize("BLIT_TERM_OUTPUT_MAX", 1 << 20).clamp(4096, 8 << 20))
}

/// `BLIT_TERM_JOURNAL=0` turns the family off: the feature bit is withheld,
/// every nonce-bearing request is refused with `PERMISSION`, and no PTY byte
/// is ever scanned for a marker.
///
/// Cached, because the answer is consulted once per OSC on the PTY output
/// path and cannot change within a process.
pub fn enabled() -> bool {
    static CACHED: OnceLock<bool> = OnceLock::new();
    *CACHED.get_or_init(|| !std::env::var("BLIT_TERM_JOURNAL").is_ok_and(|v| v == "0"))
}

fn env_usize(key: &str, default: usize) -> usize {
    std::env::var(key)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Parse one OSC payload into a marker, if it is one.
///
/// `payload` is everything between `ESC ]` and the terminator. Anything that
/// is not a semantic-prompt marker — including the `133;P`, `133;L` and
/// `633;P` variants nothing here consumes — returns `None` and is left for
/// the terminal emulator, which drops it as it always has.
pub fn parse_mark(payload: &[u8]) -> Option<(MarkKind, Dialect)> {
    let (dialect, rest) = if let Some(rest) = payload.strip_prefix(b"133;") {
        (Dialect::Osc133, rest)
    } else {
        (Dialect::Osc633, payload.strip_prefix(b"633;")?)
    };
    let kind = *rest.first()?;
    // A letter must stand alone or introduce `;`-separated parameters;
    // `133;Abc` is not a prompt-start with noise, it is not ours at all.
    let params = match rest.get(1) {
        None => &b""[..],
        Some(b';') => &rest[2..],
        Some(_) => return None,
    };
    let mark = match kind {
        b'A' => MarkKind::PromptStart,
        b'B' => MarkKind::InputStart,
        b'C' => MarkKind::OutputStart,
        b'D' => MarkKind::Finished(parse_exit(params)),
        b'E' if dialect == Dialect::Osc633 => MarkKind::CommandLine(decode_vscode_command(params)),
        _ => return None,
    };
    Some((mark, dialect))
}

/// The exit status on a `D` marker. Absent is normal: plenty of shells send
/// a bare `D`, and `D;` with nothing after it means the same thing.
fn parse_exit(params: &[u8]) -> Option<i32> {
    let field = params.split(|&b| b == b';').next()?;
    if field.is_empty() {
        return None;
    }
    std::str::from_utf8(field).ok()?.trim().parse::<i32>().ok()
}

/// `633;E` escapes its command line as `\xHH`, plus `\\` for a literal
/// backslash, so that a `;` inside the command cannot end the parameter.
fn decode_vscode_command(params: &[u8]) -> String {
    let field = params.split(|&b| b == b';').next().unwrap_or(b"");
    let mut out = Vec::with_capacity(field.len());
    let mut i = 0;
    while i < field.len() {
        if field[i] == b'\\' && i + 1 < field.len() {
            match field[i + 1] {
                b'x' | b'X' if i + 3 < field.len() => {
                    let hex = std::str::from_utf8(&field[i + 2..i + 4]).ok();
                    match hex.and_then(|h| u8::from_str_radix(h, 16).ok()) {
                        Some(byte) => {
                            out.push(byte);
                            i += 4;
                        }
                        None => {
                            out.push(field[i]);
                            i += 1;
                        }
                    }
                }
                b'\\' => {
                    out.push(b'\\');
                    i += 2;
                }
                _ => {
                    out.push(field[i]);
                    i += 1;
                }
            }
        } else {
            out.push(field[i]);
            i += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Retain the tail of `data` when it ends inside an unterminated OSC, so the
/// next chunk can be scanned as if the split had not happened.
///
/// Returns the byte offset the tail starts at, or `None` when the chunk does
/// not end mid-sequence. Only an OSC introducer counts: a trailing `ESC`
/// alone is also retained, since the `]` may be the next chunk's first byte.
pub fn unterminated_osc_tail(data: &[u8]) -> Option<usize> {
    // Scan backwards for the last ESC; if any terminator follows it, the
    // sequence closed inside this chunk and nothing needs carrying.
    let esc = data.iter().rposition(|&b| b == 0x1b)?;
    if esc + 1 < data.len() && data[esc + 1] != b']' {
        return None;
    }
    let mut i = esc + 2;
    while i < data.len() {
        if data[i] == 0x07 || (data[i] == 0x1b && i + 1 < data.len() && data[i + 1] == b'\\') {
            return None;
        }
        i += 1;
    }
    (data.len() - esc <= CARRY_MAX).then_some(esc)
}

/// What the journal state machine is waiting for.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum State {
    /// No marker seen yet, or the last command has finished.
    #[default]
    Idle,
    /// Saw `A`: a prompt is being drawn.
    Prompt,
    /// Saw `B`: the region from here to the next `C` is the command line.
    Input { seq: u64, col: u16 },
    /// Saw `C`: a command is running and `open` is its record.
    Running,
}

/// A PTY's command history: a bounded ring of finished commands plus whatever
/// is running now.
pub struct CommandJournal {
    records: VecDeque<CommandRecord>,
    state: State,
    /// The record for the running command, moved into `records` on `D`.
    open: Option<CommandRecord>,
    /// A `633;E` command line seen since the last `C`, which beats reading
    /// the command back off the grid.
    declared_command: Option<String>,
    /// Index the next command will take. Never reused, never rewound.
    next_index: u64,
    /// Lowest index still retained; rises as the ring evicts.
    oldest_index: u64,
    /// The dialect this PTY's shell speaks, latched on the first marker.
    dialect: Option<Dialect>,
    max_records: usize,
    max_command: usize,
}

impl Default for CommandJournal {
    fn default() -> Self {
        Self {
            records: VecDeque::new(),
            state: State::Idle,
            open: None,
            declared_command: None,
            next_index: 0,
            oldest_index: 0,
            dialect: None,
            max_records: env_usize("BLIT_TERM_JOURNAL_MAX", 256).max(1),
            max_command: command_max(),
        }
    }
}

/// Everything the journal needs to know about the terminal when a marker
/// lands: where the cursor is, and how to read the text a `B`..`C` region
/// left on the grid.
pub struct MarkContext<'a> {
    pub cursor_seq: u64,
    pub cursor_col: u16,
    /// Text from `(seq, col)` through the end of row `end_seq`, for
    /// recovering a command line off the grid.
    pub read: &'a dyn Fn(u64, u16, u64) -> String,
}

impl CommandJournal {
    pub fn next_index(&self) -> u64 {
        self.next_index
    }

    pub fn oldest_index(&self) -> u64 {
        self.oldest_index
    }

    /// Every record, oldest first, with the running one last.
    pub fn iter(&self) -> impl Iterator<Item = &CommandRecord> {
        self.records.iter().chain(self.open.iter())
    }

    pub fn get(&self, index: u64) -> Option<&CommandRecord> {
        self.iter().find(|r| r.index == index)
    }

    /// The newest record, running or not.
    pub fn latest(&self) -> Option<&CommandRecord> {
        self.open.as_ref().or_else(|| self.records.back())
    }

    /// The running command's index, if one is running.
    pub fn running_index(&self) -> Option<u64> {
        self.open.as_ref().map(|r| r.index)
    }

    /// A snapshot of `index` with the live fields filled in: a running
    /// command's output end is wherever the terminal is now, and any record
    /// whose start has fallen out of the scrollback says so.
    pub fn snapshot(&self, index: u64, cursor_seq: u64, oldest_seq: u64) -> Option<CommandRecord> {
        let mut record = self.get(index)?.clone();
        if record.running() {
            record.end_seq = cursor_seq.max(record.start_seq);
        }
        if record.start_seq < oldest_seq {
            record.flags |= RECORD_EVICTED;
        }
        Some(record)
    }

    /// Apply one marker. `ctx` must describe the terminal *after* every byte
    /// preceding the marker has been processed.
    pub fn apply(&mut self, mark: &SemanticMark, ctx: &MarkContext<'_>) {
        // A shell that speaks both dialects would otherwise open two records
        // per command. First one wins; `633;E` is additive and always taken,
        // since it only ever supplies text the other dialect lacks.
        if !matches!(mark.kind, MarkKind::CommandLine(_)) {
            match self.dialect {
                None => self.dialect = Some(mark.dialect),
                Some(latched) if latched != mark.dialect => return,
                Some(_) => {}
            }
        }
        match &mark.kind {
            MarkKind::PromptStart => {
                // A prompt while a command is running means the command ended
                // without saying so — Ctrl-C, a shell that only emits A, or a
                // reset. Close it honestly rather than leaving it running
                // forever.
                self.close_open(ctx.cursor_seq, None, RECORD_INCOMPLETE);
                self.declared_command = None;
                self.state = State::Prompt;
            }
            MarkKind::InputStart => {
                self.state = State::Input {
                    seq: ctx.cursor_seq,
                    col: ctx.cursor_col,
                };
            }
            MarkKind::OutputStart => {
                // Two `C`s without a `D` between them: the first command's
                // output ends where the second's begins.
                self.close_open(ctx.cursor_seq, None, RECORD_INCOMPLETE);
                let command = self.recover_command(ctx);
                let mut flags = RECORD_RUNNING;
                if command.is_empty() {
                    flags |= RECORD_NO_COMMAND;
                }
                let index = self.next_index;
                self.next_index += 1;
                self.open = Some(CommandRecord {
                    index,
                    flags,
                    exit_code: 0,
                    start_seq: ctx.cursor_seq,
                    end_seq: ctx.cursor_seq,
                    started_ms: now_ms(),
                    ended_ms: 0,
                    command,
                });
                self.declared_command = None;
                self.state = State::Running;
            }
            MarkKind::Finished(exit) => {
                // A `D` with nothing open is a shell reporting the status of
                // something it never announced starting. There is no record
                // to attach it to, so it is dropped.
                self.close_open(ctx.cursor_seq, *exit, 0);
                self.state = State::Idle;
            }
            MarkKind::CommandLine(cmd) => {
                let mut cmd = cmd.clone();
                truncate_utf8(&mut cmd, self.max_command);
                self.declared_command = Some(cmd);
            }
        }
    }

    /// The command line for the record being opened: what the shell declared,
    /// else whatever sits between the `B` marker and here.
    fn recover_command(&mut self, ctx: &MarkContext<'_>) -> String {
        if let Some(cmd) = self.declared_command.take() {
            return cmd;
        }
        let State::Input { seq, col } = self.state else {
            // `C` with no preceding `B`: there is no input region to read.
            return String::new();
        };
        // The command line ends where the cursor is now, less the newline
        // that ran it — which has already moved the cursor to the next row,
        // so reading to the row above is reading exactly the typed text.
        let end_seq = if ctx.cursor_col == 0 && ctx.cursor_seq > seq {
            ctx.cursor_seq - 1
        } else {
            ctx.cursor_seq
        };
        let mut text = (ctx.read)(seq, col, end_seq);
        // A multi-line command (a continuation, a here-doc) arrives with the
        // rows it occupied; keep them, they are the command.
        let trimmed = text.trim_end().len();
        text.truncate(trimmed);
        truncate_utf8(&mut text, self.max_command);
        text
    }

    /// Finish the running command, if any, and retire it into the ring.
    fn close_open(&mut self, end_seq: u64, exit: Option<i32>, extra_flags: u8) {
        let Some(mut record) = self.open.take() else {
            return;
        };
        record.flags &= !RECORD_RUNNING;
        record.flags |= extra_flags;
        record.end_seq = end_seq.max(record.start_seq);
        record.ended_ms = now_ms();
        if let Some(code) = exit {
            record.flags |= RECORD_HAS_EXIT;
            record.exit_code = code;
        }
        self.records.push_back(record);
        while self.records.len() > self.max_records {
            self.records.pop_front();
            self.oldest_index += 1;
        }
    }

    /// The PTY's process is gone. A command still running when that happens
    /// never gets its `D`, so close it and say why.
    pub fn note_pty_exit(&mut self, end_seq: u64) {
        self.close_open(end_seq, None, RECORD_INCOMPLETE | RECORD_PTY_EXITED);
        self.state = State::Idle;
    }

    /// A restarted PTY is a new shell in the same slot; its predecessor's
    /// commands describe output that has been reset away.
    pub fn reset(&mut self) {
        self.records.clear();
        self.open = None;
        self.declared_command = None;
        self.state = State::Idle;
        self.dialect = None;
        self.oldest_index = self.next_index;
    }
}

/// A client blocked on a command finishing (`C2S_TERM_JOURNAL_WAIT`).
///
/// Waiting is server-side on purpose: a client polling a journal to find out
/// whether `make` is done is the pattern this whole family exists to remove.
pub struct Waiter {
    pub client_id: u64,
    pub nonce: u16,
    pub pty_id: u16,
    /// The command being waited on. `None` until a command is running to
    /// attach to — `JOURNAL_INDEX_LATEST` on an idle shell means "the next
    /// one", so the index is latched the moment one starts and never moves
    /// again.
    pub index: Option<u64>,
    pub deadline: std::time::Instant,
}

/// Per connection, so a client cannot park unbounded state on the server.
///
/// Generous on purpose. A `Waiter` is forty bytes of bookkeeping, and every
/// scan over the list — admission, [`resolve_journal_waiters`], and the
/// min-deadline in `earliest_armed_deadline` — sits in the lifecycle loop
/// rather than the delivery tick, so the cost is per event and not per frame.
/// 32 was chosen when a client waiting on more than a handful of terminals was
/// hypothetical. A supervisor waits on one terminal per unit, and a hundred
/// units is an ordinary directory. It stays bounded because what needs
/// preventing is a client parking unbounded state, not a client waiting on the
/// terminals it owns.
pub const MAX_WAITERS_PER_CLIENT: usize = 4096;

/// Longest a `C2S_TERM_JOURNAL_WAIT` may block before answering with the
/// record as it stands.
pub const WAIT_TIMEOUT_MAX_MS: u32 = 24 * 60 * 60 * 1000;

/// What should happen to a waiter now.
pub enum WaitOutcome {
    /// Nothing yet.
    Pending,
    /// Answer with this record and drop the waiter.
    Ready(CommandRecord),
    /// Answer `NOT_FOUND` and drop the waiter: the PTY is gone, or the record
    /// was evicted before the wait could see it finish.
    Gone,
}

impl Waiter {
    /// Decide a waiter against a PTY's journal.
    ///
    /// `journal` is `None` when the PTY itself has gone away.
    pub fn poll(
        &mut self,
        journal: Option<&CommandJournal>,
        cursor_seq: u64,
        oldest_seq: u64,
        now: std::time::Instant,
    ) -> WaitOutcome {
        let Some(journal) = journal else {
            return WaitOutcome::Gone;
        };
        let expired = now >= self.deadline;
        if self.index.is_none() {
            self.index = journal.running_index();
        }
        let Some(index) = self.index else {
            // Still nothing running. On timeout there is no record to hand
            // back, so say so rather than invent an empty one.
            return if expired {
                WaitOutcome::Gone
            } else {
                WaitOutcome::Pending
            };
        };
        match journal.snapshot(index, cursor_seq, oldest_seq) {
            Some(record) if !record.running() => WaitOutcome::Ready(record),
            // Running: answer on timeout with the record as it stands, still
            // flagged RUNNING so the caller can tell it timed out.
            Some(record) if expired => WaitOutcome::Ready(record),
            Some(_) => WaitOutcome::Pending,
            // Not in the ring. Below the floor it was evicted; above it, the
            // command has not started yet and is still worth waiting for.
            None if index < journal.oldest_index() || expired => WaitOutcome::Gone,
            None => WaitOutcome::Pending,
        }
    }
}

/// Truncate to at most `max` bytes without splitting a character.
fn truncate_utf8(s: &mut String, max: usize) {
    if s.len() <= max {
        return;
    }
    let mut end = max;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    s.truncate(end);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx(seq: u64, col: u16) -> MarkContext<'static> {
        MarkContext {
            cursor_seq: seq,
            cursor_col: col,
            read: &|_, _, _| String::new(),
        }
    }

    fn mark(kind: MarkKind) -> SemanticMark {
        SemanticMark {
            kind,
            dialect: Dialect::Osc133,
            at: 0,
        }
    }

    fn mark633(kind: MarkKind) -> SemanticMark {
        SemanticMark {
            kind,
            dialect: Dialect::Osc633,
            at: 0,
        }
    }

    // ── Payload parsing ────────────────────────────────────────────────

    #[test]
    fn parses_the_four_standard_markers() {
        assert_eq!(
            parse_mark(b"133;A"),
            Some((MarkKind::PromptStart, Dialect::Osc133))
        );
        assert_eq!(
            parse_mark(b"133;B"),
            Some((MarkKind::InputStart, Dialect::Osc133))
        );
        assert_eq!(
            parse_mark(b"133;C"),
            Some((MarkKind::OutputStart, Dialect::Osc133))
        );
        assert_eq!(
            parse_mark(b"133;D;0"),
            Some((MarkKind::Finished(Some(0)), Dialect::Osc133))
        );
        assert_eq!(
            parse_mark(b"133;D;130"),
            Some((MarkKind::Finished(Some(130)), Dialect::Osc133))
        );
    }

    #[test]
    fn tolerates_a_finish_marker_with_no_status() {
        // Both spellings are common in the wild.
        assert_eq!(
            parse_mark(b"133;D"),
            Some((MarkKind::Finished(None), Dialect::Osc133))
        );
        assert_eq!(
            parse_mark(b"133;D;"),
            Some((MarkKind::Finished(None), Dialect::Osc133))
        );
    }

    #[test]
    fn tolerates_a_malformed_status() {
        for payload in [
            &b"133;D;banana"[..],
            b"133;D;99999999999999999999",
            b"133;D;1.5",
            b"133;D; ;aid=3",
        ] {
            assert_eq!(
                parse_mark(payload),
                Some((MarkKind::Finished(None), Dialect::Osc133)),
                "{:?} should degrade to an unknown status, not be dropped",
                String::from_utf8_lossy(payload)
            );
        }
    }

    #[test]
    fn ignores_trailing_marker_parameters() {
        assert_eq!(
            parse_mark(b"133;A;aid=17;cl=m"),
            Some((MarkKind::PromptStart, Dialect::Osc133))
        );
        assert_eq!(
            parse_mark(b"133;D;3;aid=17"),
            Some((MarkKind::Finished(Some(3)), Dialect::Osc133))
        );
    }

    #[test]
    fn ignores_markers_that_are_not_ours() {
        for payload in [
            &b"7;file:///tmp"[..],
            b"0;a title",
            b"133",
            b"133;",
            b"133;P;k=i",
            b"133;L",
            b"133;Z",
            b"133;Abc",
            b"633;P;Cwd=/tmp",
            b"1337;File=inline",
            b"",
        ] {
            assert_eq!(
                parse_mark(payload),
                None,
                "{:?} is not a semantic-prompt marker",
                String::from_utf8_lossy(payload)
            );
        }
    }

    #[test]
    fn parses_the_vscode_command_line() {
        assert_eq!(
            parse_mark(b"633;E;ls -la"),
            Some((MarkKind::CommandLine("ls -la".into()), Dialect::Osc633))
        );
        // `;` and the escape character itself are hex-escaped by the shell
        // integration script so they cannot end the parameter.
        assert_eq!(
            parse_mark(b"633;E;echo a\\x3bb"),
            Some((MarkKind::CommandLine("echo a;b".into()), Dialect::Osc633))
        );
        assert_eq!(
            parse_mark(b"633;E;a\\\\b"),
            Some((MarkKind::CommandLine("a\\b".into()), Dialect::Osc633))
        );
        // A truncated escape is data, not a panic.
        assert_eq!(
            parse_mark(b"633;E;tail\\x"),
            Some((MarkKind::CommandLine("tail\\x".into()), Dialect::Osc633))
        );
        // `E` is VS Code's alone; 133 has no such marker.
        assert_eq!(parse_mark(b"133;E;ls"), None);
    }

    // ── Split sequences ────────────────────────────────────────────────

    #[test]
    fn carries_a_sequence_split_across_chunks() {
        assert_eq!(unterminated_osc_tail(b"output\x1b]133;C"), Some(6));
        assert_eq!(unterminated_osc_tail(b"output\x1b]"), Some(6));
        assert_eq!(unterminated_osc_tail(b"output\x1b"), Some(6));
    }

    #[test]
    fn carries_nothing_when_the_sequence_completed() {
        assert_eq!(unterminated_osc_tail(b"\x1b]133;C\x07more"), None);
        assert_eq!(unterminated_osc_tail(b"\x1b]133;C\x1b\\"), None);
        assert_eq!(unterminated_osc_tail(b"plain output"), None);
        // A CSI is somebody else's problem; only OSC needs the carry.
        assert_eq!(unterminated_osc_tail(b"\x1b[0m"), None);
    }

    #[test]
    fn refuses_to_carry_an_unbounded_tail() {
        let mut data = vec![0x1b, b']'];
        data.extend(std::iter::repeat_n(b'x', CARRY_MAX));
        assert_eq!(
            unterminated_osc_tail(&data),
            None,
            "a sequence that never terminates must not grow the carry buffer"
        );
    }

    // ── State machine ──────────────────────────────────────────────────

    fn run(marks: &[(SemanticMark, u64)]) -> CommandJournal {
        let mut journal = CommandJournal::default();
        for (mark, seq) in marks {
            journal.apply(mark, &ctx(*seq, 0));
        }
        journal
    }

    #[test]
    fn a_full_cycle_produces_one_finished_record() {
        let journal = run(&[
            (mark(MarkKind::PromptStart), 10),
            (mark(MarkKind::InputStart), 10),
            (mark(MarkKind::OutputStart), 11),
            (mark(MarkKind::Finished(Some(0))), 20),
        ]);
        let records: Vec<_> = journal.iter().collect();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].index, 0);
        assert_eq!(records[0].start_seq, 11);
        assert_eq!(records[0].end_seq, 20);
        assert_eq!(records[0].exit(), Some(0));
        assert!(!records[0].running());
        assert_eq!(journal.next_index(), 1);
    }

    #[test]
    fn a_running_command_is_visible_before_it_finishes() {
        let journal = run(&[
            (mark(MarkKind::PromptStart), 5),
            (mark(MarkKind::InputStart), 5),
            (mark(MarkKind::OutputStart), 6),
        ]);
        let latest = journal.latest().expect("a record");
        assert!(latest.running());
        assert_eq!(journal.running_index(), Some(0));
        // Its end follows the terminal until it finishes.
        let snap = journal.snapshot(0, 42, 0).expect("snapshot");
        assert_eq!(snap.end_seq, 42);
    }

    #[test]
    fn output_start_without_input_start_still_records() {
        let journal = run(&[
            (mark(MarkKind::PromptStart), 1),
            (mark(MarkKind::OutputStart), 2),
            (mark(MarkKind::Finished(Some(1))), 5),
        ]);
        let record = journal.get(0).expect("a record");
        assert_eq!(record.flags & RECORD_NO_COMMAND, RECORD_NO_COMMAND);
        assert_eq!(record.command, "");
        assert_eq!(record.exit(), Some(1));
    }

    #[test]
    fn a_finish_with_nothing_running_is_dropped() {
        let journal = run(&[
            (mark(MarkKind::Finished(Some(0))), 3),
            (mark(MarkKind::Finished(Some(0))), 4),
        ]);
        assert_eq!(journal.iter().count(), 0);
        assert_eq!(journal.next_index(), 0);
    }

    #[test]
    fn a_second_finish_does_not_reopen_the_record() {
        let journal = run(&[
            (mark(MarkKind::OutputStart), 1),
            (mark(MarkKind::Finished(Some(0))), 4),
            (mark(MarkKind::Finished(Some(7))), 9),
        ]);
        assert_eq!(journal.iter().count(), 1);
        assert_eq!(journal.get(0).unwrap().exit(), Some(0));
        assert_eq!(journal.get(0).unwrap().end_seq, 4);
    }

    #[test]
    fn a_new_prompt_closes_an_interrupted_command() {
        // Ctrl-C: the shell redraws its prompt and never sends `D`.
        let journal = run(&[
            (mark(MarkKind::OutputStart), 3),
            (mark(MarkKind::PromptStart), 8),
        ]);
        let record = journal.get(0).expect("a record");
        assert!(
            !record.running(),
            "an interrupted command is not still running"
        );
        assert_eq!(record.flags & RECORD_INCOMPLETE, RECORD_INCOMPLETE);
        assert_eq!(record.exit(), None);
        assert_eq!(record.end_seq, 8);
    }

    #[test]
    fn back_to_back_output_markers_split_into_two_records() {
        let journal = run(&[
            (mark(MarkKind::OutputStart), 2),
            (mark(MarkKind::OutputStart), 6),
            (mark(MarkKind::Finished(Some(0))), 9),
        ]);
        let records: Vec<_> = journal.iter().collect();
        assert_eq!(records.len(), 2);
        assert_eq!((records[0].start_seq, records[0].end_seq), (2, 6));
        assert_eq!(records[0].flags & RECORD_INCOMPLETE, RECORD_INCOMPLETE);
        assert_eq!((records[1].start_seq, records[1].end_seq), (6, 9));
        assert_eq!(records[1].exit(), Some(0));
    }

    #[test]
    fn markers_arriving_out_of_order_never_wedge_the_machine() {
        // Every permutation of a cycle's markers, replayed twice: whatever
        // the shell does, the journal must still accept a clean cycle after.
        let kinds = [
            MarkKind::Finished(Some(0)),
            MarkKind::OutputStart,
            MarkKind::InputStart,
            MarkKind::PromptStart,
        ];
        for a in 0..4 {
            for b in 0..4 {
                for c in 0..4 {
                    let mut journal = CommandJournal::default();
                    for (i, k) in [a, b, c].iter().enumerate() {
                        journal.apply(&mark(kinds[*k].clone()), &ctx(i as u64, 0));
                    }
                    let before = journal.next_index();
                    for (i, k) in [
                        MarkKind::PromptStart,
                        MarkKind::InputStart,
                        MarkKind::OutputStart,
                        MarkKind::Finished(Some(42)),
                    ]
                    .into_iter()
                    .enumerate()
                    {
                        journal.apply(&mark(k), &ctx(100 + i as u64, 0));
                    }
                    let last = journal.latest().expect("the clean cycle recorded");
                    assert_eq!(
                        last.exit(),
                        Some(42),
                        "a clean cycle after [{a},{b},{c}] must still record"
                    );
                    assert!(!last.running());
                    assert!(journal.next_index() > before);
                }
            }
        }
    }

    #[test]
    fn indices_are_stable_while_the_ring_evicts() {
        unsafe { std::env::set_var("BLIT_TERM_JOURNAL_MAX", "3") };
        let mut journal = CommandJournal::default();
        unsafe { std::env::remove_var("BLIT_TERM_JOURNAL_MAX") };
        for i in 0..10u64 {
            journal.apply(&mark(MarkKind::OutputStart), &ctx(i * 2, 0));
            journal.apply(&mark(MarkKind::Finished(Some(0))), &ctx(i * 2 + 1, 0));
        }
        assert_eq!(journal.next_index(), 10);
        assert_eq!(journal.oldest_index(), 7);
        assert_eq!(journal.iter().count(), 3);
        let indices: Vec<u64> = journal.iter().map(|r| r.index).collect();
        assert_eq!(indices, vec![7, 8, 9], "indices are never renumbered");
        assert!(journal.get(3).is_none(), "evicted records are gone");
        assert_eq!(journal.get(9).map(|r| r.index), Some(9));
    }

    #[test]
    fn evicted_output_is_flagged_at_read_time() {
        let journal = run(&[
            (mark(MarkKind::OutputStart), 10),
            (mark(MarkKind::Finished(Some(0))), 20),
        ]);
        let fresh = journal.snapshot(0, 30, 5).expect("snapshot");
        assert_eq!(fresh.flags & RECORD_EVICTED, 0);
        let stale = journal.snapshot(0, 3000, 500).expect("snapshot");
        assert_eq!(stale.flags & RECORD_EVICTED, RECORD_EVICTED);
    }

    #[test]
    fn a_pty_exit_closes_whatever_was_running() {
        let mut journal = run(&[(mark(MarkKind::OutputStart), 4)]);
        journal.note_pty_exit(9);
        let record = journal.get(0).expect("a record");
        assert!(!record.running());
        assert_eq!(record.flags & RECORD_PTY_EXITED, RECORD_PTY_EXITED);
        assert_eq!(record.end_seq, 9);
    }

    #[test]
    fn a_restart_retires_the_old_shells_commands() {
        let mut journal = run(&[
            (mark(MarkKind::OutputStart), 1),
            (mark(MarkKind::Finished(Some(0))), 2),
        ]);
        journal.reset();
        assert_eq!(journal.iter().count(), 0);
        assert_eq!(
            journal.oldest_index(),
            journal.next_index(),
            "indices keep going up so a stale client index cannot alias"
        );
        journal.apply(&mark(MarkKind::OutputStart), &ctx(0, 0));
        assert_eq!(journal.latest().unwrap().index, 1);
    }

    #[test]
    fn a_shell_speaking_both_dialects_is_not_counted_twice() {
        let mut journal = CommandJournal::default();
        for (kind, dialect633) in [
            (MarkKind::PromptStart, true),
            (MarkKind::InputStart, true),
            (MarkKind::OutputStart, true),
            (MarkKind::Finished(Some(0)), true),
        ] {
            journal.apply(&mark(kind.clone()), &ctx(1, 0));
            if dialect633 {
                journal.apply(&mark633(kind), &ctx(1, 0));
            }
        }
        assert_eq!(
            journal.iter().count(),
            1,
            "the second dialect must not open a second record"
        );
    }

    #[test]
    fn a_declared_command_line_beats_reading_the_grid() {
        let mut journal = CommandJournal::default();
        journal.apply(&mark(MarkKind::PromptStart), &ctx(0, 0));
        journal.apply(&mark(MarkKind::InputStart), &ctx(0, 2));
        journal.apply(
            &mark633(MarkKind::CommandLine("make -j8".into())),
            &ctx(0, 2),
        );
        journal.apply(
            &SemanticMark {
                kind: MarkKind::OutputStart,
                dialect: Dialect::Osc133,
                at: 0,
            },
            &MarkContext {
                cursor_seq: 1,
                cursor_col: 0,
                read: &|_, _, _| "$ make -j8".into(),
            },
        );
        assert_eq!(journal.latest().unwrap().command, "make -j8");
    }

    #[test]
    fn a_command_line_is_read_back_off_the_grid() {
        let mut journal = CommandJournal::default();
        journal.apply(&mark(MarkKind::PromptStart), &ctx(4, 0));
        journal.apply(&mark(MarkKind::InputStart), &ctx(4, 2));
        journal.apply(
            &mark(MarkKind::OutputStart),
            &MarkContext {
                cursor_seq: 5,
                cursor_col: 0,
                read: &|s, c, e| {
                    assert_eq!(
                        (s, c, e),
                        (4, 2, 4),
                        "read the typed region, not the prompt"
                    );
                    "cargo test   ".into()
                },
            },
        );
        assert_eq!(journal.latest().unwrap().command, "cargo test");
    }

    #[test]
    fn an_enormous_command_line_is_bounded() {
        let mut journal = CommandJournal {
            max_command: 8,
            ..CommandJournal::default()
        };
        journal.apply(&mark633(MarkKind::CommandLine("é".repeat(50))), &ctx(0, 0));
        journal.apply(&mark(MarkKind::OutputStart), &ctx(0, 0));
        let command = &journal.latest().unwrap().command;
        assert!(command.len() <= 8);
        assert!(command.chars().all(|c| c == 'é'), "no split character");
    }

    // ── Waiting ────────────────────────────────────────────────────────

    fn waiter(index: Option<u64>, timeout_ms: u64) -> Waiter {
        Waiter {
            client_id: 1,
            nonce: 7,
            pty_id: 2,
            index,
            deadline: std::time::Instant::now() + std::time::Duration::from_millis(timeout_ms),
        }
    }

    fn expired_waiter(index: Option<u64>) -> Waiter {
        let mut w = waiter(index, 0);
        w.deadline = std::time::Instant::now() - std::time::Duration::from_secs(1);
        w
    }

    #[test]
    fn a_waiter_resolves_when_the_command_finishes() {
        let mut journal = run(&[(mark(MarkKind::OutputStart), 3)]);
        let mut w = waiter(None, 60_000);
        let now = std::time::Instant::now();
        assert!(matches!(
            w.poll(Some(&journal), 5, 0, now),
            WaitOutcome::Pending
        ));
        assert_eq!(w.index, Some(0), "the running command is latched");

        journal.apply(&mark(MarkKind::Finished(Some(3))), &ctx(9, 0));
        match w.poll(Some(&journal), 9, 0, now) {
            WaitOutcome::Ready(record) => assert_eq!(record.exit(), Some(3)),
            _ => panic!("a finished command must resolve its waiter"),
        }
    }

    #[test]
    fn a_waiter_on_an_idle_shell_latches_the_next_command() {
        let mut journal = CommandJournal::default();
        let mut w = waiter(None, 60_000);
        let now = std::time::Instant::now();
        assert!(matches!(
            w.poll(Some(&journal), 0, 0, now),
            WaitOutcome::Pending
        ));
        assert_eq!(w.index, None);

        journal.apply(&mark(MarkKind::OutputStart), &ctx(1, 0));
        assert!(matches!(
            w.poll(Some(&journal), 1, 0, now),
            WaitOutcome::Pending
        ));
        assert_eq!(w.index, Some(0));
    }

    #[test]
    fn a_timeout_hands_back_the_record_still_running() {
        let journal = run(&[(mark(MarkKind::OutputStart), 3)]);
        let mut w = expired_waiter(None);
        match w.poll(Some(&journal), 40, 0, std::time::Instant::now()) {
            WaitOutcome::Ready(record) => {
                assert!(record.running(), "a timeout does not finish the command");
                assert_eq!(record.end_seq, 40, "its output still runs to the bottom");
            }
            _ => panic!("a timeout must answer, not hang"),
        }
    }

    #[test]
    fn a_waiter_gives_up_on_a_vanished_pty_or_record() {
        let journal = CommandJournal::default();
        assert!(matches!(
            waiter(None, 60_000).poll(None, 0, 0, std::time::Instant::now()),
            WaitOutcome::Gone
        ));
        assert!(matches!(
            expired_waiter(None).poll(Some(&journal), 0, 0, std::time::Instant::now()),
            WaitOutcome::Gone
        ));
    }

    #[test]
    fn a_waiter_on_an_evicted_record_gives_up_rather_than_hanging() {
        unsafe { std::env::set_var("BLIT_TERM_JOURNAL_MAX", "1") };
        let mut journal = CommandJournal::default();
        unsafe { std::env::remove_var("BLIT_TERM_JOURNAL_MAX") };
        for i in 0..4u64 {
            journal.apply(&mark(MarkKind::OutputStart), &ctx(i, 0));
            journal.apply(&mark(MarkKind::Finished(Some(0))), &ctx(i, 0));
        }
        let mut w = waiter(Some(0), 60_000);
        assert!(matches!(
            w.poll(Some(&journal), 9, 0, std::time::Instant::now()),
            WaitOutcome::Gone
        ));
    }

    #[test]
    fn a_waiter_on_a_future_index_keeps_waiting() {
        let journal = run(&[
            (mark(MarkKind::OutputStart), 1),
            (mark(MarkKind::Finished(Some(0))), 2),
        ]);
        let mut w = waiter(Some(5), 60_000);
        assert!(matches!(
            w.poll(Some(&journal), 3, 0, std::time::Instant::now()),
            WaitOutcome::Pending
        ));
    }

    #[test]
    fn a_terminal_with_no_shell_integration_stays_empty() {
        let journal = CommandJournal::default();
        assert_eq!(journal.next_index(), 0);
        assert_eq!(journal.iter().count(), 0);
        assert_eq!(journal.latest(), None);
        assert_eq!(journal.snapshot(0, 0, 0), None);
    }
}
