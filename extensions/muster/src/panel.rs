//! The `blit.muster.v1` channel: what a browser panel reads.
//!
//! One JSON object per message. The design said "full state on every change, no
//! deltas", justified by the state being small — and nesting surfaces under a
//! hundred units is what stopped that being true. So this coalesces instead:
//! transitions mark units dirty, and a flush carries **only those units**. A
//! reader still never has to reconcile a field-level patch, because every unit
//! it receives arrives whole; it just does not receive the ninety-nine that did
//! not change.
//!
//! The typed SDK channel API blocks, so this speaks `remote::channel` directly,
//! the way `@session`'s panel does.

use super::{Muster, Surface};
use blit_ext_muster::journal::Record;
use blit_ext_muster::supervisor::Unit;
use blit_guest::Client;
use blit_guest::remote;
use serde_json::{Value, json};

/// The channel a panel connects to.
pub const CHANNEL_NAME: &str = "blit.muster.v1";

/// How long transitions accumulate before a flush.
///
/// A cold start emits several transitions per unit within a few milliseconds;
/// without this each one would be its own frame. Short enough that a click
/// still feels immediate.
pub const COALESCE_MS: u64 = 80;

/// How much journal a newly connected panel is handed.
///
/// Enough to explain the state it is about to receive — why a unit is in
/// backoff, what the last exit was — without making the greeting a page of
/// history nobody asked for. `@muster journal` is where the rest lives.
pub const BACKFILL: usize = 200;

/// One browser reading the panel.
pub struct Conn {
    pub id: u32,
    /// Credit the peer granted, and how much is spent. A panel that stops
    /// acking must not be allowed to grow the guest's memory.
    window: u64,
    sent: u64,
    acked: u64,
    /// Bytes received, for the cumulative ack owed back.
    received: u64,
    /// This peer missed a flush for want of credit, so the next one it can
    /// afford has to carry everything rather than a diff against a state it
    /// never saw.
    owes_full: bool,
}

impl Conn {
    fn credit(&self) -> u64 {
        self.window
            .saturating_sub(self.sent.saturating_sub(self.acked))
    }
}

impl Muster {
    /// Publish the channel. Failure is not fatal — another copy of the
    /// extension may already hold the name, and the CLI half still works.
    pub(crate) fn open_panel(&mut self, client: &mut Client) {
        self.panel_listener = client
            .listen_channel(CHANNEL_NAME, b"")
            .map(|listener| listener.id())
            .unwrap_or(0);
    }

    /// Note that a unit's row is out of date. The flush decides when to send.
    pub(crate) fn touch(&mut self, name: &str, now: u64) {
        if self.panel_listener != 0 {
            self.dirty.insert(name.to_string());
            self.dirty_since.get_or_insert(now);
        }
    }

    /// Everything changed at once — a reload, a rename, a unit going away.
    ///
    /// A partial frame names only units, so a change to the *shape* — which
    /// instances exist, which units they own — has no partial form to send.
    pub(crate) fn touch_all(&mut self, now: u64) {
        if self.panel_listener != 0 {
            for conn in &mut self.panel_conns {
                conn.owes_full = true;
            }
            self.dirty_since.get_or_insert(now);
        }
    }

    /// When the next flush is due, if anything is waiting.
    ///
    /// `dirty_since` is the only thing that arms a flush, and every producer
    /// sets it. Deriving "waiting" from the dirty set instead would spin: a
    /// peer with no credit leaves `owes_full` set through a flush that could
    /// not send, and a predicate reading that would ask to be woken again
    /// immediately, forever. Credit comes back with an ACK, so an ACK re-arms.
    pub(crate) fn flush_due_ms(&self, _now: u64) -> Option<u64> {
        self.dirty_since.map(|at| at + COALESCE_MS)
    }

    /// Send what has accumulated, to whoever can afford it.
    pub(crate) fn flush_panel(&mut self, client: &mut Client, now: u64) {
        if self.panel_conns.is_empty() {
            self.dirty.clear();
            self.dirty_since = None;
            self.pending_events.clear();
            return;
        }
        if self.flush_due_ms(now).is_none_or(|due| now < due) {
            return;
        }
        let changed: Vec<String> = self.dirty.iter().cloned().collect();
        let partial = self.state_json(&changed).to_string();
        let full = self.state_json_full().to_string();
        let events = (!self.pending_events.is_empty()).then(|| {
            events_json(self.pending_events.iter().map(Record::to_json).collect()).to_string()
        });

        for index in 0..self.panel_conns.len() {
            let owes_full = self.panel_conns[index].owes_full;
            let frame = if owes_full { &full } else { &partial };
            if !owes_full && changed.is_empty() {
                // Nothing for this peer but the events below.
            } else if self.send_json(client, index, frame) {
                self.panel_conns[index].owes_full = false;
            } else {
                // Out of credit. The next affordable frame must be whole.
                self.panel_conns[index].owes_full = true;
                continue;
            }
            // A journal record answers a question that will not repeat, so
            // unlike state it is not re-derivable from the next frame — but it
            // is also not worth stalling a peer over. Dropping the batch loses
            // log lines in a panel whose log view re-reads on open.
            if let Some(events) = &events {
                self.send_json(client, index, events);
            }
        }
        self.dirty.clear();
        self.dirty_since = None;
        self.pending_events.clear();
    }

    /// Queue a journal record for the next flush.
    pub(crate) fn publish_event(&mut self, record: &Record, now: u64) {
        if self.panel_listener == 0 || self.panel_conns.is_empty() {
            return;
        }
        self.pending_events.push(record.clone());
        self.dirty_since.get_or_insert(now);
    }

    fn send_json(&mut self, client: &mut Client, index: usize, text: &str) -> bool {
        let bytes = text.as_bytes();
        if bytes.len() > remote::channel::CHANNEL_MAX_PAYLOAD {
            return false;
        }
        let conn = &mut self.panel_conns[index];
        if (bytes.len() as u64) > conn.credit() {
            return false;
        }
        let Some(message) = remote::channel::msg_channel_data(conn.id, bytes) else {
            return false;
        };
        if client.send(&message).is_err() {
            return false;
        }
        conn.sent = conn.sent.saturating_add(bytes.len() as u64);
        true
    }

    /// The whole view: instances, then every unit.
    fn state_json_full(&self) -> Value {
        let names: Vec<String> = self.units.keys().cloned().collect();
        let mut frame = self.state_json(&names);
        if let Some(map) = frame.as_object_mut() {
            map.insert("full".into(), json!(true));
            map.insert(
                "instances".into(),
                json!(
                    self.instances
                        .iter()
                        .map(|(name, instance)| json!({
                            "name": name,
                            "stack": instance.stack,
                            "members": instance.members,
                        }))
                        .collect::<Vec<_>>()
                ),
            );
            map.insert("dir".into(), json!(self.dir));
        }
        frame
    }

    /// The named units, whole, plus the names that no longer exist.
    fn state_json(&self, names: &[String]) -> Value {
        let mut units = Vec::new();
        let mut gone = Vec::new();
        for name in names {
            match self.units.get(name) {
                Some(unit) => units.push(self.panel_unit(unit)),
                None => gone.push(name.clone()),
            }
        }
        json!({ "type": "state", "units": units, "gone": gone })
    }

    /// One unit as the panel shows it: what it is doing, its terminal, and the
    /// windows that terminal opened.
    fn panel_unit(&self, unit: &Unit) -> Value {
        let surfaces: Vec<Value> = self
            .surfaces_of(&unit.name)
            .into_iter()
            .map(|(id, surface)| surface_json(id, surface))
            .collect();
        let runs: Vec<Value> = unit
            .runs
            .iter()
            .map(|run| json!({ "pty": run.pty, "exitCode": run.exit_code, "seq": run.seq }))
            .collect();
        json!({
            "name": unit.name,
            "instance": unit.instance,
            "description": unit.file.description,
            "phase": unit.phase.as_str(),
            "pty": unit.pty,
            "restarts": unit.failures,
            "lastExit": unit.last_exit,
            "requires": unit.file.requires,
            "autostart": unit.file.autostart,
            // The unit file changed under a running unit and the change is not
            // in what is running — a panel showing a green dot would be lying.
            "stale": unit.stale,
            "type": unit.file.unit_type.as_str(),
            "surfaces": surfaces,
            "runs": runs,
        })
    }

    /// Accept, ack, close and command, on the raw channel wire.
    pub(crate) fn route_panel(&mut self, client: &mut Client, packet: &[u8], now: u64) -> bool {
        let Ok(Some(message)) = remote::channel::parse_channel_message(packet) else {
            return false;
        };
        match message {
            remote::channel::ChannelMessage::Accepted {
                channel_id,
                listener_id,
                window,
                ..
            } => {
                if listener_id != self.panel_listener || self.panel_listener == 0 {
                    return false;
                }
                // A second Accepted for a live id would leave two records
                // sharing one peer's credit, and both would undercount it.
                if self.panel_conns.iter().any(|conn| conn.id == channel_id) {
                    return true;
                }
                self.panel_conns.push(Conn {
                    id: channel_id,
                    window,
                    sent: 0,
                    acked: 0,
                    received: 0,
                    // A new peer has seen nothing, so its first frame is whole
                    // by the same rule that governs one which missed a flush.
                    owes_full: true,
                });
                let index = self.panel_conns.len() - 1;
                let hello = json!({ "type": "hello", "version": 1, "dir": self.dir });
                self.send_json(client, index, &hello.to_string());
                // The journal is the one thing a fresh reader cannot derive
                // from the state frame that follows: it is what already
                // happened. Everything else it is about to be told.
                let backfill =
                    events_json(self.journal.tail(BACKFILL).map(Record::to_json).collect())
                        .to_string();
                self.send_json(client, index, &backfill);
                self.dirty_since.get_or_insert(now);
                true
            }
            remote::channel::ChannelMessage::Ack { channel_id, bytes } => {
                let Some(conn) = self.panel_conns.iter_mut().find(|c| c.id == channel_id) else {
                    return false;
                };
                conn.acked = conn.acked.max(bytes);
                // Credit is back, so a frame this peer could not afford can be
                // retried. Nothing else would wake the loop for it.
                if conn.owes_full {
                    self.dirty_since.get_or_insert(now);
                }
                true
            }
            remote::channel::ChannelMessage::Closed { channel_id, .. } => {
                self.panel_conns.retain(|conn| conn.id != channel_id);
                true
            }
            remote::channel::ChannelMessage::Data {
                channel_id,
                payload,
            } => {
                let Some(index) = self.panel_conns.iter().position(|c| c.id == channel_id) else {
                    return false;
                };
                // Ack before acting: the command's effect arrives as a state
                // frame, and withholding the ack until then would stall a panel
                // whose window is exactly one command deep.
                let conn = &mut self.panel_conns[index];
                conn.received = conn.received.saturating_add(payload.len() as u64);
                let received = conn.received;
                let _ = client.send(&remote::channel::msg_channel_ack(channel_id, received));
                let text = String::from_utf8_lossy(payload).trim().to_string();
                let (verb, name) = match text.split_once(' ') {
                    Some((verb, name)) => (verb, name.trim()),
                    None => (text.as_str(), ""),
                };
                self.panel_command(client, verb, name, now);
                true
            }
            _ => false,
        }
    }
}

/// A batch of journal records, in one message.
///
/// The `type` is here rather than on the record because a record's own `event`
/// field already names a thing that happened, and two keys called `event` and
/// `type` on one object is a reader's coin toss.
fn events_json(records: Vec<Value>) -> Value {
    json!({ "type": "events", "records": records })
}

fn surface_json(id: u16, surface: &Surface) -> Value {
    json!({
        "id": id,
        "title": surface.title,
        "width": surface.width,
        "height": surface.height,
    })
}
