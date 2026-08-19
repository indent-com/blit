//! A journal of supervision decisions, not of unit output.
//!
//! Output is the terminal — `blit terminal journal <pty>` already reads it with
//! exit codes and sequence cursors. What is not recoverable from a terminal is
//! *why* a unit was started, stopped, or given up on, which is the question
//! this answers.
//!
//! Environment values never appear here. A `spawn` record names the files it
//! read and counts the keys they produced: enough to diagnose "it did not pick
//! up my `.env`", not enough to leak one.

use std::collections::VecDeque;
use std::fmt;

/// What happened.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Event {
    Loaded,
    Changed,
    Invalid,
    Unloaded,
    Cycle,
    Start,
    Spawn,
    Ready,
    Exit,
    Restart,
    Reaped,
    Stop,
    Failed,
    Adopted,
}

impl Event {
    pub const fn as_str(self) -> &'static str {
        match self {
            Event::Loaded => "loaded",
            Event::Changed => "changed",
            Event::Invalid => "invalid",
            Event::Unloaded => "unloaded",
            Event::Cycle => "cycle",
            Event::Start => "start",
            Event::Spawn => "spawn",
            Event::Ready => "ready",
            Event::Exit => "exit",
            Event::Restart => "restart",
            Event::Reaped => "reaped",
            Event::Stop => "stop",
            Event::Failed => "failed",
            Event::Adopted => "adopted",
        }
    }
}

impl fmt::Display for Event {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Who asked for this.
///
/// A closed vocabulary rather than free text, because "who asked for this" is
/// the question the journal exists to answer and prose does not answer it
/// reliably.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Cause {
    Autostart,
    Dependency(String),
    Command,
    File,
    Crash,
    Policy,
    Adopt,
}

impl fmt::Display for Cause {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Cause::Autostart => f.write_str("autostart"),
            Cause::Dependency(unit) => write!(f, "dependency:{unit}"),
            Cause::Command => f.write_str("command"),
            Cause::File => f.write_str("file"),
            Cause::Crash => f.write_str("crash"),
            Cause::Policy => f.write_str("policy"),
            Cause::Adopt => f.write_str("adopt"),
        }
    }
}

#[derive(Debug, Clone)]
pub struct Record {
    pub seq: u64,
    pub ts: u64,
    pub unit: String,
    pub instance: Option<String>,
    pub event: Event,
    pub phase: &'static str,
    pub cause: Option<Cause>,
    pub pty: Option<u16>,
    pub exit_code: Option<i32>,
    pub detail: String,
    pub env_files: Vec<String>,
    pub env_keys: Option<usize>,
}

impl Record {
    pub fn new(unit: impl Into<String>, event: Event, phase: &'static str) -> Self {
        Self {
            seq: 0,
            ts: 0,
            unit: unit.into(),
            instance: None,
            event,
            phase,
            cause: None,
            pty: None,
            exit_code: None,
            detail: String::new(),
            env_files: Vec::new(),
            env_keys: None,
        }
    }

    pub fn cause(mut self, cause: Cause) -> Self {
        self.cause = Some(cause);
        self
    }

    pub fn pty(mut self, pty: u16) -> Self {
        self.pty = Some(pty);
        self
    }

    pub fn exit_code(mut self, code: i32) -> Self {
        self.exit_code = Some(code);
        self
    }

    pub fn detail(mut self, detail: impl Into<String>) -> Self {
        self.detail = detail.into();
        self
    }

    pub fn instance(mut self, instance: Option<String>) -> Self {
        self.instance = instance;
        self
    }

    pub fn env(mut self, files: Vec<String>, keys: usize) -> Self {
        self.env_files = files;
        self.env_keys = Some(keys);
        self
    }

    /// One JSON object, the same shape the channel and `--json` emit.
    pub fn to_json(&self) -> String {
        let mut out = format!(
            r#"{{"seq":{},"ts":{},"unit":{},"event":"{}","phase":"{}""#,
            self.seq,
            self.ts,
            quote(&self.unit),
            self.event,
            self.phase
        );
        if let Some(instance) = &self.instance {
            out.push_str(&format!(r#","instance":{}"#, quote(instance)));
        }
        if let Some(cause) = &self.cause {
            out.push_str(&format!(r#","cause":"{cause}""#));
        }
        if let Some(pty) = self.pty {
            out.push_str(&format!(r#","pty":{pty}"#));
        }
        if let Some(code) = self.exit_code {
            out.push_str(&format!(r#","exitCode":{code}"#));
        }
        if !self.detail.is_empty() {
            out.push_str(&format!(r#","detail":{}"#, quote(&self.detail)));
        }
        if let Some(keys) = self.env_keys {
            let files: Vec<String> = self.env_files.iter().map(|f| quote(f)).collect();
            out.push_str(&format!(
                r#","envFiles":[{}],"envKeys":{keys}"#,
                files.join(",")
            ));
        }
        out.push('}');
        out
    }
}

/// Minimal JSON string escaping — enough for paths, unit names and details.
pub fn quote(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 2);
    out.push('"');
    for c in text.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// How many records the live tail holds.
pub const RING: usize = 1024;
/// How many are mirrored into KV, so the answer survives a server restart.
pub const DURABLE: usize = 256;

#[derive(Debug, Default)]
pub struct Journal {
    records: VecDeque<Record>,
    next_seq: u64,
}

impl Journal {
    pub fn new(next_seq: u64) -> Self {
        Self {
            records: VecDeque::new(),
            next_seq,
        }
    }

    pub fn next_seq(&self) -> u64 {
        self.next_seq
    }

    /// Stamp and store. Returns the stored record so a caller can publish the
    /// same bytes it persisted.
    pub fn push(&mut self, mut record: Record, now_ms: u64) -> Record {
        record.seq = self.next_seq;
        record.ts = now_ms;
        self.next_seq += 1;
        if self.records.len() == RING {
            self.records.pop_front();
        }
        self.records.push_back(record.clone());
        record
    }

    /// Newest last, oldest first — the order a tail wants.
    pub fn tail(&self, count: usize) -> impl Iterator<Item = &Record> {
        let skip = self.records.len().saturating_sub(count);
        self.records.iter().skip(skip)
    }

    pub fn since(&self, seq: u64) -> impl Iterator<Item = &Record> {
        self.records.iter().filter(move |r| r.seq >= seq)
    }

    pub fn len(&self) -> usize {
        self.records.len()
    }

    pub fn is_empty(&self) -> bool {
        self.records.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sequence_numbers_are_monotonic_and_resume() {
        let mut journal = Journal::new(41);
        let a = journal.push(Record::new("api", Event::Start, "waiting"), 1000);
        let b = journal.push(Record::new("api", Event::Spawn, "activating"), 1001);
        assert_eq!((a.seq, b.seq), (41, 42));
        assert_eq!(journal.next_seq(), 43);
    }

    #[test]
    fn the_ring_bounds_itself_without_losing_the_newest() {
        let mut journal = Journal::new(0);
        for _ in 0..RING + 10 {
            journal.push(Record::new("api", Event::Exit, "backoff"), 0);
        }
        assert_eq!(journal.len(), RING);
        let newest = journal.tail(1).next().unwrap().seq;
        assert_eq!(newest, (RING + 10 - 1) as u64);
    }

    #[test]
    fn a_spawn_record_counts_env_keys_and_never_carries_values() {
        let record = Record::new("gateway@epic", Event::Spawn, "activating")
            .pty(7)
            .instance(Some("epic".into()))
            .detail("./target/profiling/blit gateway")
            .env(vec!["/src/blit/.env.local".into()], 9);
        let json = record.to_json();
        assert!(json.contains(r#""envKeys":9"#), "{json}");
        assert!(json.contains(r#""instance":"epic""#), "{json}");
        assert!(json.contains(r#""pty":7"#), "{json}");
        // The record names the file and counts the keys. It holds no value.
        assert!(!json.contains("BLIT_PASSPHRASE"), "{json}");
    }

    #[test]
    fn causes_render_the_unit_that_asked() {
        let record = Record::new("gateway@epic", Event::Start, "waiting")
            .cause(Cause::Dependency("server@epic".into()));
        assert!(
            record
                .to_json()
                .contains(r#""cause":"dependency:server@epic""#)
        );
    }

    #[test]
    fn quoting_survives_a_detail_with_quotes_and_newlines() {
        let json = Record::new("api", Event::Failed, "failed")
            .detail("cannot enter \"dir\"\nline two")
            .to_json();
        assert!(json.contains(r#"cannot enter \"dir\"\nline two"#), "{json}");
        assert!(serde_json_roundtrips(&json), "{json}");
    }

    fn serde_json_roundtrips(text: &str) -> bool {
        serde_json::from_str::<serde_json::Value>(text).is_ok()
    }

    #[test]
    fn since_filters_by_cursor() {
        let mut journal = Journal::new(0);
        for _ in 0..5 {
            journal.push(Record::new("api", Event::Exit, "backoff"), 0);
        }
        assert_eq!(journal.since(3).count(), 2);
    }
}
