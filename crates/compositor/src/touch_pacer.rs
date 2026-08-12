//! Bounded playout for browser-originated direct touch.
//!
//! Chromium's Wayland backend timestamps touch on receipt instead of using
//! `wl_touch.time`. A network burst therefore has to be released as distinct
//! frames at the source cadence. This is a small jitter buffer: it preserves
//! an ordinary burst exactly, but keeps only recent motion history if delivery
//! falls behind so a terminal `up` cannot trail the velocity samples forever.

use crate::imp::{TouchPhase, TouchPoint};
use std::collections::{HashMap, HashSet, VecDeque};
use std::time::{Duration, Instant};

const MAX_PACED_STEP_MS: u32 = 100;
const MAX_PENDING_MOTIONS: usize = 8;
const MIN_VELOCITY_MOTIONS: usize = 2;
const MAX_PLAYOUT_LATENCY: Duration = Duration::from_millis(80);

pub(super) struct TouchFrame {
    pub due: Instant,
    pub owner_id: u64,
    sequence_id: u64,
    pub surface_id: u16,
    pub phase: TouchPhase,
    pub time_ms: u32,
    pub contacts: Vec<TouchPoint>,
}

#[derive(Clone, Copy)]
struct SequenceCursor {
    source_time_ms: u32,
    due: Instant,
}

struct Sequence {
    id: u64,
    cursor: Option<SequenceCursor>,
    contacts: HashSet<i32>,
}

#[derive(Default)]
pub(super) struct TouchPacer {
    pending: VecDeque<TouchFrame>,
    sequences: HashMap<u64, Sequence>,
    /// Last position actually selected for playout, per live contact.
    ///
    /// Backlog compaction uses this as the fixed end of its retained stroke.
    /// Without it, a gesture that falls behind after its down was delivered
    /// eventually teleports into the recent tail and Chromium drops inertia.
    played_positions: HashMap<(u64, u64, i32), TouchPoint>,
    next_sequence_id: u64,
}

impl TouchPacer {
    pub fn push(
        &mut self,
        now: Instant,
        owner_id: u64,
        surface_id: u16,
        phase: TouchPhase,
        time_ms: u32,
        contacts: Vec<TouchPoint>,
    ) {
        if phase == TouchPhase::Cancel {
            let sequence_id = self
                .sequences
                .get(&owner_id)
                .map_or(self.next_sequence_id, |sequence| sequence.id);
            self.clear(Some(owner_id));
            self.pending.push_back(TouchFrame {
                due: now,
                owner_id,
                sequence_id,
                surface_id,
                phase,
                time_ms,
                contacts,
            });
            return;
        }

        // An owner can start another sequence while the preceding `up` is
        // still waiting for playout. It gets a fresh source-time epoch while
        // global queue order keeps the new down behind that terminal frame.
        if phase == TouchPhase::Down
            && !self
                .sequences
                .get(&owner_id)
                .is_some_and(|sequence| !sequence.contacts.is_empty())
        {
            self.sequences.remove(&owner_id);
        }

        if !self.sequences.contains_key(&owner_id) {
            let id = self.next_sequence_id;
            self.next_sequence_id = self.next_sequence_id.wrapping_add(1);
            self.sequences.insert(
                owner_id,
                Sequence {
                    id,
                    cursor: None,
                    contacts: HashSet::new(),
                },
            );
        }

        let previous_global_due = self.pending.back().map(|frame| frame.due);
        let sequence = self
            .sequences
            .get_mut(&owner_id)
            .expect("sequence inserted");
        let sequence_id = sequence.id;
        let mut due = match (time_ms, sequence.cursor) {
            (0, _) => now,
            (_, Some(cursor)) => {
                let delta = time_ms.wrapping_sub(cursor.source_time_ms);
                if delta <= MAX_PACED_STEP_MS {
                    cursor
                        .due
                        .checked_add(Duration::from_millis(u64::from(delta)))
                        .unwrap_or(now)
                        .max(now)
                } else {
                    now
                }
            }
            (_, None) => now,
        };
        if let Some(previous_due) = previous_global_due {
            due = due.max(previous_due);
        }

        sequence.cursor = (time_ms != 0).then_some(SequenceCursor {
            source_time_ms: time_ms,
            due,
        });
        match phase {
            TouchPhase::Down => {
                sequence
                    .contacts
                    .extend(contacts.iter().map(|point| point.id));
            }
            TouchPhase::Up => {
                for point in &contacts {
                    sequence.contacts.remove(&point.id);
                }
            }
            TouchPhase::Motion => {}
            TouchPhase::Cancel => unreachable!(),
        }
        let sequence_ended = sequence.contacts.is_empty();

        self.pending.push_back(TouchFrame {
            due,
            owner_id,
            sequence_id,
            surface_id,
            phase,
            time_ms,
            contacts,
        });
        self.compact(now);

        if sequence_ended {
            self.sequences.remove(&owner_id);
        } else {
            self.sync_cursor_due(owner_id);
        }
    }

    /// Whether commands already accepted for `owner` still describe contacts
    /// that have not received a terminal event.
    pub fn has_contacts(&self, owner: Option<u64>) -> bool {
        self.sequences.iter().any(|(active_owner, sequence)| {
            !sequence.contacts.is_empty() && owner.is_none_or(|owner| owner == *active_owner)
        })
    }

    pub fn clear(&mut self, owner: Option<u64>) {
        self.pending
            .retain(|frame| owner.is_some_and(|owner| owner != frame.owner_id));
        self.played_positions
            .retain(|(active_owner, _, _), _| owner.is_some_and(|owner| owner != *active_owner));
        if let Some(owner) = owner {
            self.sequences.remove(&owner);
        } else {
            self.sequences.clear();
        }
    }

    pub fn pop_due(&mut self, now: Instant) -> Option<TouchFrame> {
        let frame = self.pending.pop_front_if(|frame| frame.due <= now)?;

        match frame.phase {
            TouchPhase::Down | TouchPhase::Motion => {
                for point in &frame.contacts {
                    self.played_positions
                        .insert((frame.owner_id, frame.sequence_id, point.id), *point);
                }
            }
            TouchPhase::Up => {
                for point in &frame.contacts {
                    self.played_positions
                        .remove(&(frame.owner_id, frame.sequence_id, point.id));
                }
            }
            TouchPhase::Cancel => {
                self.played_positions
                    .retain(|(owner_id, _, _), _| *owner_id != frame.owner_id);
            }
        }

        // A long render/encode pass can make several deadlines overdue. Move
        // the undispatched tail forward instead of flushing it as one receipt-
        // timestamped burst, then shed old history if that would exceed the
        // bounded playout latency.
        if self.pending.front().is_some_and(|next| next.due <= now) {
            let lateness = now.saturating_duration_since(frame.due);
            for pending in &mut self.pending {
                pending.due = pending.due.checked_add(lateness).unwrap_or(now);
            }
            self.compact(now);
        }
        self.sync_all_cursor_due();
        Some(frame)
    }

    pub fn next_deadline(&self) -> Option<Instant> {
        self.pending.front().map(|frame| frame.due)
    }

    fn compact(&mut self, now: Instant) {
        loop {
            let too_many = self
                .pending
                .iter()
                .filter(|frame| frame.phase == TouchPhase::Motion)
                .fold(HashMap::<(u64, u64), usize>::new(), |mut counts, frame| {
                    *counts
                        .entry((frame.owner_id, frame.sequence_id))
                        .or_default() += 1;
                    counts
                })
                .into_iter()
                .find_map(|(sequence, count)| (count > MAX_PENDING_MOTIONS).then_some(sequence));
            let too_late = self.pending.back().is_some_and(|frame| {
                frame.due.saturating_duration_since(now) > MAX_PLAYOUT_LATENCY
            });
            let sequence =
                too_many.or_else(|| too_late.then(|| self.oldest_droppable_sequence()).flatten());
            let Some((owner_id, sequence_id)) = sequence else {
                break;
            };
            if !self.drop_oldest_motion(owner_id, sequence_id) {
                break;
            }
        }
        self.sync_all_cursor_due();
    }

    fn oldest_droppable_sequence(&self) -> Option<(u64, u64)> {
        let mut counts = HashMap::<(u64, u64), usize>::new();
        for frame in &self.pending {
            if frame.phase == TouchPhase::Motion {
                *counts
                    .entry((frame.owner_id, frame.sequence_id))
                    .or_default() += 1;
            }
        }
        self.pending.iter().find_map(|frame| {
            (frame.phase == TouchPhase::Motion
                && counts
                    .get(&(frame.owner_id, frame.sequence_id))
                    .copied()
                    .unwrap_or_default()
                    > MIN_VELOCITY_MOTIONS)
                .then_some((frame.owner_id, frame.sequence_id))
        })
    }

    /// Drop an owner's oldest motion, merge any otherwise-lost contact update
    /// into its next motion, and close the removed inter-frame gap. The newest
    /// positions and at least two velocity samples always survive.
    fn drop_oldest_motion(&mut self, owner_id: u64, sequence_id: u64) -> bool {
        let Some(index) = self.pending.iter().position(|frame| {
            frame.owner_id == owner_id
                && frame.sequence_id == sequence_id
                && frame.phase == TouchPhase::Motion
        }) else {
            return false;
        };
        let Some(next_index) =
            self.pending
                .iter()
                .enumerate()
                .skip(index + 1)
                .find_map(|(index, frame)| {
                    (frame.owner_id == owner_id
                        && frame.sequence_id == sequence_id
                        && frame.phase == TouchPhase::Motion)
                        .then_some(index)
                })
        else {
            return false;
        };
        let removed_due = self.pending[index].due;
        let next_due = self.pending[next_index].due;
        let removed = self.pending.remove(index).expect("motion index exists");
        let next_index = next_index - 1;

        let next = &mut self.pending[next_index];
        for point in removed.contacts {
            if !next.contacts.iter().any(|next| next.id == point.id) {
                next.contacts.push(point);
            }
        }

        let closed_gap = next_due.saturating_duration_since(removed_due);
        if !closed_gap.is_zero() {
            for frame in self.pending.iter_mut().skip(index) {
                frame.due = frame.due.checked_sub(closed_gap).unwrap_or(removed_due);
            }
        }
        self.smooth_pending_sequence(owner_id, sequence_id);
        true
    }

    /// Resample a compacted tail between the last played (or queued down)
    /// position and the newest source position.
    ///
    /// Keeping only the newest raw samples is not sufficient: after a long
    /// burst their first point can be far from the contact position Chromium
    /// already saw. Chromium scrolls that discontinuity but excludes the
    /// gesture from fling velocity. A short interpolated tail preserves the
    /// endpoint, bounded latency, per-contact atomicity, and a continuous path.
    fn smooth_pending_sequence(&mut self, owner_id: u64, sequence_id: u64) {
        let mut anchors = HashMap::<i32, TouchPoint>::new();
        for ((played_owner, played_sequence, id), point) in &self.played_positions {
            if (*played_owner, *played_sequence) == (owner_id, sequence_id) {
                anchors.insert(*id, *point);
            }
        }
        for frame in &self.pending {
            if (frame.owner_id, frame.sequence_id, frame.phase)
                != (owner_id, sequence_id, TouchPhase::Down)
            {
                continue;
            }
            for point in &frame.contacts {
                anchors.entry(point.id).or_insert(*point);
            }
        }

        let mut targets = HashMap::<i32, TouchPoint>::new();
        for frame in &self.pending {
            if (frame.owner_id, frame.sequence_id) != (owner_id, sequence_id)
                || !matches!(frame.phase, TouchPhase::Motion | TouchPhase::Up)
            {
                continue;
            }
            for point in &frame.contacts {
                targets.insert(point.id, *point);
            }
        }

        for (id, target) in targets {
            let Some(anchor) = anchors.get(&id) else {
                continue;
            };
            let occurrences: Vec<usize> = self
                .pending
                .iter()
                .enumerate()
                .filter_map(|(index, frame)| {
                    ((frame.owner_id, frame.sequence_id, frame.phase)
                        == (owner_id, sequence_id, TouchPhase::Motion)
                        && frame.contacts.iter().any(|point| point.id == id))
                    .then_some(index)
                })
                .collect();
            let count = occurrences.len();
            for (offset, index) in occurrences.into_iter().enumerate() {
                let progress = (offset + 1) as f64 / count as f64;
                let point = self.pending[index]
                    .contacts
                    .iter_mut()
                    .find(|point| point.id == id)
                    .expect("motion occurrence contains contact");
                point.x = anchor.x + (target.x - anchor.x) * progress;
                point.y = anchor.y + (target.y - anchor.y) * progress;
            }
        }
    }

    fn sync_all_cursor_due(&mut self) {
        let owners: Vec<u64> = self.sequences.keys().copied().collect();
        for owner in owners {
            self.sync_cursor_due(owner);
        }
    }

    fn sync_cursor_due(&mut self, owner_id: u64) {
        let Some(due) = self
            .pending
            .iter()
            .rev()
            .find(|frame| {
                frame.owner_id == owner_id
                    && self
                        .sequences
                        .get(&owner_id)
                        .is_some_and(|sequence| frame.sequence_id == sequence.id)
            })
            .map(|frame| frame.due)
        else {
            return;
        };
        if let Some(cursor) = self
            .sequences
            .get_mut(&owner_id)
            .and_then(|sequence| sequence.cursor.as_mut())
        {
            cursor.due = due;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{MAX_PENDING_MOTIONS, MAX_PLAYOUT_LATENCY, TouchPacer};
    use crate::imp::{TouchPhase, TouchPoint};
    use std::time::{Duration, Instant};

    fn point(id: i32, x: f64) -> Vec<TouchPoint> {
        vec![TouchPoint { id, x, y: 0.0 }]
    }

    #[test]
    fn an_ordinary_burst_keeps_every_source_interval() {
        let now = Instant::now();
        let mut pacer = TouchPacer::default();
        pacer.push(now, 1, 1, TouchPhase::Down, 1_000, point(10, 0.0));
        for i in 1..=5u32 {
            pacer.push(
                now,
                1,
                1,
                TouchPhase::Motion,
                1_000 + i * 8,
                point(10, f64::from(i)),
            );
        }

        let due: Vec<_> = pacer
            .pending
            .iter()
            .map(|frame| frame.due.duration_since(now))
            .collect();
        assert_eq!(
            due,
            [
                Duration::ZERO,
                Duration::from_millis(8),
                Duration::from_millis(16),
                Duration::from_millis(24),
                Duration::from_millis(32),
                Duration::from_millis(40),
            ]
        );
    }

    #[test]
    fn a_sustained_burst_keeps_recent_positions_and_bounds_release_latency() {
        let now = Instant::now();
        let mut pacer = TouchPacer::default();
        pacer.push(now, 1, 1, TouchPhase::Down, 1_000, point(10, 0.0));
        for i in 1..=120u32 {
            pacer.push(
                now,
                1,
                1,
                TouchPhase::Motion,
                1_000 + i * 8,
                point(10, f64::from(i)),
            );
        }
        pacer.push(now, 1, 1, TouchPhase::Up, 1_968, point(10, 120.0));

        let motions: Vec<_> = pacer
            .pending
            .iter()
            .filter(|frame| frame.phase == TouchPhase::Motion)
            .collect();
        assert!((2..=8).contains(&motions.len()));
        let down = pacer
            .pending
            .iter()
            .find(|frame| frame.phase == TouchPhase::Down)
            .expect("queued down");
        assert_eq!(down.contacts[0].x, 0.0, "compaction moved the hit test");
        assert!(
            motions
                .iter()
                .zip(motions.iter().skip(1))
                .all(|(left, right)| left.contacts[0].x < right.contacts[0].x),
            "retained positions are not a continuous stroke"
        );
        assert_eq!(motions.last().unwrap().contacts[0].x, 120.0);
        assert!(motions.first().unwrap().time_ms >= 1_900);
        assert!(pacer.pending.back().unwrap().due.duration_since(now) <= MAX_PLAYOUT_LATENCY);
    }

    #[test]
    fn compaction_stays_continuous_after_playout_has_started() {
        let base = Instant::now();
        let mut pacer = TouchPacer::default();
        pacer.push(base, 1, 1, TouchPhase::Down, 1_000, point(10, 0.0));
        assert_eq!(pacer.pop_due(base).expect("down due").contacts[0].x, 0.0);
        pacer.push(
            base + Duration::from_millis(8),
            1,
            1,
            TouchPhase::Motion,
            1_008,
            point(10, 1.0),
        );
        assert_eq!(
            pacer
                .pop_due(base + Duration::from_millis(8))
                .expect("first motion due")
                .contacts[0]
                .x,
            1.0
        );

        for i in 2..=120u32 {
            pacer.push(
                base + Duration::from_millis(8),
                1,
                1,
                TouchPhase::Motion,
                1_000 + i * 8,
                point(10, f64::from(i)),
            );
        }

        let positions: Vec<f64> = pacer
            .pending
            .iter()
            .filter(|frame| frame.phase == TouchPhase::Motion)
            .map(|frame| frame.contacts[0].x)
            .collect();
        assert_eq!(positions.len(), MAX_PENDING_MOTIONS);
        assert_eq!(positions.last(), Some(&120.0));
        assert!(positions[0] > 1.0);
        assert!(
            positions.windows(2).all(|pair| pair[0] < pair[1]),
            "compacted tail jumped or reversed after playout: {positions:?}"
        );
    }

    #[test]
    fn successive_gestures_from_one_owner_never_merge() {
        let now = Instant::now();
        let mut pacer = TouchPacer::default();
        pacer.push(now, 1, 1, TouchPhase::Down, 1_000, point(10, 0.0));
        for i in 1..=12u32 {
            pacer.push(
                now,
                1,
                1,
                TouchPhase::Motion,
                1_000 + i * 8,
                point(10, f64::from(i)),
            );
        }
        pacer.push(now, 1, 1, TouchPhase::Up, 1_104, point(10, 12.0));
        pacer.push(now, 1, 1, TouchPhase::Down, 2_000, point(20, 0.0));
        for i in 1..=12u32 {
            pacer.push(
                now,
                1,
                1,
                TouchPhase::Motion,
                2_000 + i * 8,
                point(20, f64::from(i)),
            );
        }

        let second_down = pacer
            .pending
            .iter()
            .position(|frame| frame.phase == TouchPhase::Down && frame.contacts[0].id == 20)
            .expect("second gesture down");
        assert!(
            pacer
                .pending
                .iter()
                .skip(second_down)
                .all(|frame| { frame.contacts.iter().all(|contact| contact.id == 20) })
        );
    }

    #[test]
    fn cancel_is_immediate_and_discards_the_owners_tail() {
        let now = Instant::now();
        let mut pacer = TouchPacer::default();
        pacer.push(now, 1, 1, TouchPhase::Down, 1_000, point(10, 0.0));
        pacer.push(now, 1, 1, TouchPhase::Motion, 1_008, point(10, 1.0));
        pacer.push(
            now + Duration::from_millis(1),
            1,
            1,
            TouchPhase::Cancel,
            0,
            Vec::new(),
        );

        assert_eq!(pacer.pending.len(), 1);
        assert_eq!(pacer.pending[0].phase, TouchPhase::Cancel);
        assert_eq!(pacer.pending[0].due, now + Duration::from_millis(1));
        assert!(!pacer.has_contacts(Some(1)));
    }

    #[test]
    fn an_overdue_tail_is_rebased_without_becoming_unbounded() {
        let base = Instant::now();
        let mut pacer = TouchPacer::default();
        pacer.push(base, 1, 1, TouchPhase::Down, 1_000, point(10, 0.0));
        for i in 1..=20u32 {
            pacer.push(
                base,
                1,
                1,
                TouchPhase::Motion,
                1_000 + i * 8,
                point(10, f64::from(i)),
            );
        }
        let late = base + Duration::from_millis(200);
        assert!(pacer.pop_due(late).is_some());
        assert!(pacer.pending.back().unwrap().due <= late + MAX_PLAYOUT_LATENCY);
    }
}
