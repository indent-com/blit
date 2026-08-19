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

use serde_json::{Value, json};
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
    /// A `stopCommand` or `reloadCommand` was run. It is a decision about the
    /// unit taken by muster, so it belongs beside the others — and it is the
    /// only record of a terminal that is never the unit's own run.
    Ran,
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
            Event::Ran => "ran",
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
    ///
    /// Optional fields are omitted rather than emitted as null: a reader
    /// distinguishes "this record has no pty" from "the pty is unknown", and
    /// the journal is read by people as often as by programs.
    pub fn to_json(&self) -> Value {
        let mut object = json!({
            "seq": self.seq,
            "ts": self.ts,
            "unit": self.unit,
            "event": self.event.as_str(),
            "phase": self.phase,
        });
        let map = object.as_object_mut().expect("built as an object");
        if let Some(instance) = &self.instance {
            map.insert("instance".into(), json!(instance));
        }
        if let Some(cause) = &self.cause {
            map.insert("cause".into(), json!(cause.to_string()));
        }
        if let Some(pty) = self.pty {
            map.insert("pty".into(), json!(pty));
        }
        if let Some(code) = self.exit_code {
            map.insert("exitCode".into(), json!(code));
        }
        if !self.detail.is_empty() {
            map.insert("detail".into(), json!(self.detail));
        }
        if let Some(keys) = self.env_keys {
            map.insert("envFiles".into(), json!(self.env_files));
            map.insert("envKeys".into(), json!(keys));
        }
        object
    }
}

/// How many records the live tail holds.
///
/// Sized so it is never the binding constraint: bringing up a hundred units
/// emits on the order of five hundred records, so this holds a dozen or so cold
/// starts. It stays bounded only because a crash-looping unit emits records
/// forever — at a 250 ms floor that is four a second — and a supervisor is
/// long-lived. It is a backstop against that, not a budget anyone should have
/// to think about.
pub const RING: usize = 16_384;

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

    /// Stamp and store, returning the stored record so a caller can publish
    /// exactly what it kept.
    pub fn push(&mut self, mut record: Record, now_ms: u64) -> &Record {
        record.seq = self.next_seq;
        record.ts = now_ms;
        self.next_seq += 1;
        if self.records.len() == RING {
            self.records.pop_front();
        }
        self.records.push_back(record);
        self.records.back().expect("just pushed")
    }

    /// Oldest first, which is the order a tail is read in. Double-ended so a
    /// caller wanting the newest N takes from the back rather than collecting
    /// the whole ring to reverse it.
    pub fn tail(&self, count: usize) -> impl DoubleEndedIterator<Item = &Record> {
        let skip = self.records.len().saturating_sub(count);
        self.records.iter().skip(skip)
    }

    /// Records are pushed in sequence order, so a cursor is a partition point
    /// rather than a filter over the whole ring.
    pub fn since(&self, seq: u64) -> impl DoubleEndedIterator<Item = &Record> {
        let at = self.records.partition_point(|record| record.seq < seq);
        self.records.iter().skip(at)
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
        let a = journal
            .push(Record::new("api", Event::Start, "waiting"), 1000)
            .seq;
        let b = journal
            .push(Record::new("api", Event::Spawn, "activating"), 1001)
            .seq;
        assert_eq!((a, b), (41, 42));
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
        let record = Record::new("epic/gateway", Event::Spawn, "activating")
            .pty(7)
            .instance(Some("epic".into()))
            .detail("./target/profiling/blit gateway")
            .env(vec!["/src/blit/.env.local".into()], 9);
        let json = record.to_json();
        assert_eq!(json["envKeys"], json!(9));
        assert_eq!(json["envFiles"], json!(["/src/blit/.env.local"]));
        assert_eq!(json["instance"], json!("epic"));
        assert_eq!(json["pty"], json!(7));
        // The record names the file and counts the keys. It holds no value.
        assert!(!json.to_string().contains("BLIT_PASSPHRASE"));
    }

    #[test]
    fn absent_fields_are_omitted_rather_than_null() {
        let json = Record::new("api", Event::Loaded, "stopped").to_json();
        assert!(json.get("pty").is_none(), "{json}");
        assert!(json.get("cause").is_none(), "{json}");
        assert!(json.get("exitCode").is_none(), "{json}");
        assert!(json.get("detail").is_none(), "{json}");
    }

    #[test]
    fn causes_render_the_unit_that_asked() {
        let record = Record::new("epic/gateway", Event::Start, "waiting")
            .cause(Cause::Dependency("epic/server".into()));
        assert_eq!(record.to_json()["cause"], json!("dependency:epic/server"));
    }

    #[test]
    fn a_detail_with_quotes_and_newlines_survives_a_round_trip() {
        let detail = "cannot enter \"dir\"\nline two";
        let text = Record::new("api", Event::Failed, "failed")
            .detail(detail)
            .to_json()
            .to_string();
        let parsed: Value = serde_json::from_str(&text).expect("valid JSON");
        assert_eq!(parsed["detail"], json!(detail));
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
