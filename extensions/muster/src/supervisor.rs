//! Phases, backoff, retention and dependency order.
//!
//! Split from the protocol plumbing so the parts that are easy to get wrong are
//! testable without a server: what a clean exit means under each restart
//! policy, which terminals to reap, and what order a dependency graph starts
//! and stops in.

use crate::config::UnitFile;
use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

/// Matched to the server's own extension supervisor and to `@session`, so three
/// layers of restart logic in one server do not each invent their own numbers.
pub const BACKOFF_BASE: Duration = Duration::from_millis(250);
pub const BACKOFF_MAX: Duration = Duration::from_secs(30);
/// How long a run must last before its failures are forgiven.
pub const HEALTHY_AFTER: Duration = Duration::from_secs(60);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Phase {
    Stopped,
    /// Wanted; a `requires` dependency is not ready.
    Waiting,
    /// PTY created, `readyWhen` not yet satisfied.
    Activating,
    Running,
    /// A `oneshot` finished 0. Counts as ready until the file changes.
    Exited,
    Backoff,
    Failed,
    /// Stopped by hand: ignores `autostart` until started again.
    Held,
}

impl Phase {
    pub const fn as_str(self) -> &'static str {
        match self {
            Phase::Stopped => "stopped",
            Phase::Waiting => "waiting",
            Phase::Activating => "activating",
            Phase::Running => "running",
            Phase::Exited => "exited",
            Phase::Backoff => "backoff",
            Phase::Failed => "failed",
            Phase::Held => "held",
        }
    }

    /// Whether a dependent may proceed.
    pub const fn is_ready(self) -> bool {
        matches!(self, Phase::Running | Phase::Exited)
    }

    /// Whether a terminal is expected to be alive.
    pub const fn is_live(self) -> bool {
        matches!(self, Phase::Activating | Phase::Running)
    }
}

/// One finished run, retained so the reason it finished stays addressable.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Run {
    pub pty: u16,
    pub seq: u64,
    pub exit_code: i32,
    pub started_ms: u64,
    pub ended_ms: u64,
}

/// Everything the supervisor knows about one unit.
#[derive(Clone, Debug)]
pub struct Unit {
    pub name: String,
    pub instance: Option<String>,
    pub file: UnitFile,
    pub phase: Phase,
    /// The live terminal, if there is one.
    pub pty: Option<u16>,
    /// Monotonic per unit, and part of the tag, so a corpse is never mistaken
    /// for the live run.
    pub seq: u64,
    pub started_ms: u64,
    pub failures: u32,
    pub next_attempt_ms: u64,
    /// When `readyWhen` gives up.
    pub deadline_ms: u64,
    pub last_exit: Option<i32>,
    /// Newest first.
    pub runs: Vec<Run>,
    /// The unit file changed under a running unit.
    pub stale: bool,
    /// An explicit restart is waiting for the current terminal to die.
    pub restart_pending: bool,
    /// A stop is in flight; SIGKILL when this comes due.
    pub kill_at_ms: u64,
}

impl Unit {
    pub fn new(name: String, instance: Option<String>, file: UnitFile) -> Self {
        Self {
            name,
            instance,
            file,
            phase: Phase::Stopped,
            pty: None,
            seq: 0,
            started_ms: 0,
            failures: 0,
            next_attempt_ms: 0,
            deadline_ms: 0,
            last_exit: None,
            runs: Vec::new(),
            stale: false,
            restart_pending: false,
            kill_at_ms: 0,
        }
    }

    /// The tag a terminal carries. `<seq>` is what makes an old run
    /// distinguishable from the live one after the supervisor restarts.
    pub fn tag(&self, seq: u64) -> String {
        tag_for(&self.name, seq)
    }

    /// Whether an exit should be retried, under this unit's declared policy.
    ///
    /// The policy is obeyed literally. `@session` refuses to retry exit 0 even
    /// when told to, because a second Chromium hands its argv to the first and
    /// exits 0; a terminal unit has no such hazard, and the blit dev server
    /// depends on the opposite — it exits 0 on purpose when replaced, and
    /// retrying that is an infinite loop.
    pub fn wants_restart(&self, exit_code: i32) -> bool {
        if exit_code == 0 {
            self.file.restart_on_success
        } else {
            self.file.restart_on_failure
        }
    }

    /// Record that the live run ended, and decide what happens next.
    ///
    /// Returns the retained runs that must now be closed to stay within `keep`.
    pub fn note_exit(&mut self, exit_code: i32, now_ms: u64, random: u64) -> Vec<Run> {
        let pty = self.pty.take();
        self.last_exit = Some(exit_code);
        self.deadline_ms = 0;
        self.kill_at_ms = 0;

        if let Some(pty) = pty {
            self.runs.insert(
                0,
                Run {
                    pty,
                    seq: self.seq,
                    exit_code,
                    started_ms: self.started_ms,
                    ended_ms: now_ms,
                },
            );
        }

        // A run that lasted, or that ended cleanly, is not evidence of a loop.
        let healthy = exit_code == 0
            || now_ms.saturating_sub(self.started_ms) >= HEALTHY_AFTER.as_millis() as u64;
        if healthy {
            self.failures = 0;
        }

        // An exit that follows a stop is the stop completing, not a crash.
        // Running the restart policy over it resurrects a unit somebody asked
        // to stop — the signal that killed it is a nonzero exit, and
        // `restartOnFailure` defaults on, so `@muster stop` on a *live* unit
        // would come back after its backoff. A unit stopped while already dead
        // never reaches here, which is why this hid.
        if !self.phase.is_live() {
            return self.reap();
        }

        if self.file.unit_type == crate::config::UnitType::Oneshot && exit_code == 0 {
            self.phase = Phase::Exited;
        } else if !self.wants_restart(exit_code) {
            self.phase = if exit_code == 0 {
                Phase::Stopped
            } else {
                Phase::Failed
            };
        } else {
            self.arm_retry(now_ms, random);
        }
        self.reap()
    }

    /// The start never happened: the create was refused, or its environment
    /// could not be resolved.
    ///
    /// Distinct from `note_exit` because there is no run and no exit code. An
    /// earlier version faked both — phase `Running`, exit `-1` — which put a
    /// number indistinguishable from a real 255 into `last_exit` and into
    /// `@muster status`.
    pub fn note_failed_start(&mut self, now_ms: u64, random: u64) {
        self.deadline_ms = 0;
        self.kill_at_ms = 0;
        self.pty = None;
        if self.file.restart_on_failure {
            self.arm_retry(now_ms, random);
        } else {
            self.phase = Phase::Failed;
        }
    }

    /// Count the failure and either back off or give up.
    fn arm_retry(&mut self, now_ms: u64, random: u64) {
        self.failures += 1;
        if self.file.start_limit > 0 && self.failures >= self.file.start_limit {
            self.phase = Phase::Failed;
            return;
        }
        self.phase = Phase::Backoff;
        let delay = match self.file.restart_delay {
            Some(fixed) => Duration::from_millis(fixed.ms()),
            None => backoff(self.failures, random),
        };
        self.next_attempt_ms = now_ms + delay.as_millis() as u64;
    }

    /// Retained runs beyond `keep`, oldest first, for the caller to close.
    pub fn reap(&mut self) -> Vec<Run> {
        let keep = self.file.keep as usize;
        if self.runs.len() <= keep {
            return Vec::new();
        }
        // `runs` is newest-first, so the tail is what ages out.
        self.runs.split_off(keep)
    }

    /// Whether an attempt is due now.
    pub fn attempt_due(&self, now_ms: u64) -> bool {
        match self.phase {
            Phase::Waiting => true,
            Phase::Backoff => now_ms >= self.next_attempt_ms,
            _ => false,
        }
    }

    /// The next moment this unit needs attention, if any.
    pub fn next_deadline_ms(&self) -> Option<u64> {
        let mut soonest: Option<u64> = None;
        let mut consider = |at: u64| {
            if at > 0 {
                soonest = Some(soonest.map_or(at, |s: u64| s.min(at)));
            }
        };
        if self.phase == Phase::Backoff {
            consider(self.next_attempt_ms);
        }
        if self.phase == Phase::Activating {
            consider(self.deadline_ms);
        }
        consider(self.kill_at_ms);
        soonest
    }
}

/// A unit belonging to an instance is `<instance>/<template>`.
///
/// Reads as a path because that is what it is: the instance groups, the
/// template names. It also sorts the way you want — every unit of one instance
/// together — and cannot collide with a plain unit, whose name is a filename
/// and therefore holds no separator.
pub fn qualified(instance: &str, template: &str) -> String {
    format!("{instance}/{template}")
}

/// The instance and template a qualified name refers to.
pub fn unqualify(name: &str) -> Option<(&str, &str)> {
    name.split_once('/')
}

/// The prefix every muster-owned terminal tag carries.
pub const TAG_PREFIX: &str = "muster/";

/// `muster/<unit>/<seq>`.
///
/// Written at create and read back at adoption, where it is the only evidence
/// linking a live PTY to a unit — so the two directions live together and are
/// tested against each other rather than open-coded at both ends.
pub fn tag_for(unit: &str, seq: u64) -> String {
    format!("{TAG_PREFIX}{unit}/{seq}")
}

/// The unit and sequence a tag names, or `None` if it is not one of ours.
pub fn parse_tag(tag: &str) -> Option<(&str, u64)> {
    let rest = tag.strip_prefix(TAG_PREFIX)?;
    let (unit, seq) = rest.rsplit_once('/')?;
    if unit.is_empty() {
        return None;
    }
    Some((unit, seq.parse().ok()?))
}

/// Exponential with full jitter.
///
/// Jitter matters more here than for one application: a stack brings several
/// units up at once, and a shared cause — a database that is not up yet, a
/// rebuild that has not finished — would otherwise have them all retry in
/// lockstep forever.
pub fn backoff(failures: u32, random: u64) -> Duration {
    if failures == 0 {
        return Duration::ZERO;
    }
    let shift = (failures - 1).min(16);
    let scaled = BACKOFF_BASE.saturating_mul(1u32 << shift);
    let cap = if scaled > BACKOFF_MAX {
        BACKOFF_MAX
    } else {
        scaled
    };
    let cap_ns = cap.as_nanos() as u64;
    if cap_ns == 0 {
        return Duration::ZERO;
    }
    Duration::from_nanos(random % cap_ns)
}

/// A dependency cycle, named by its members so the file can be fixed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cycle(pub Vec<String>);

/// Topological order over `requires` + `wants`, restricted to `units`.
///
/// `after` orders without pulling anything in, so it participates in the sort
/// but never in the closure.
pub fn start_order(units: &BTreeMap<String, Unit>, roots: &[String]) -> Result<Vec<String>, Cycle> {
    let mut order = Vec::new();
    let mut done = BTreeSet::new();
    let mut path = Vec::new();
    for root in roots {
        visit(units, root, &mut order, &mut done, &mut path)?;
    }
    Ok(order)
}

fn visit(
    units: &BTreeMap<String, Unit>,
    name: &str,
    order: &mut Vec<String>,
    done: &mut BTreeSet<String>,
    path: &mut Vec<String>,
) -> Result<(), Cycle> {
    if done.contains(name) {
        return Ok(());
    }
    if let Some(at) = path.iter().position(|n| n == name) {
        let mut ring = path[at..].to_vec();
        ring.push(name.to_string());
        return Err(Cycle(ring));
    }
    let Some(unit) = units.get(name) else {
        // A dangling name is a `doctor` finding, not a cycle. Ordering simply
        // has nothing to place.
        return Ok(());
    };
    path.push(name.to_string());
    for dep in unit
        .file
        .requires
        .iter()
        .chain(&unit.file.wants)
        .chain(&unit.file.after)
    {
        visit(units, dep, order, done, path)?;
    }
    path.pop();
    done.insert(name.to_string());
    order.push(name.to_string());
    Ok(())
}

/// Everything reachable through `requires` and `wants` — what starting a unit
/// starts. `after` is ordering only and is deliberately not followed.
pub fn start_closure(units: &BTreeMap<String, Unit>, root: &str) -> Vec<String> {
    let mut seen = BTreeSet::new();
    let mut stack = vec![root.to_string()];
    while let Some(name) = stack.pop() {
        if !seen.insert(name.clone()) {
            continue;
        }
        if let Some(unit) = units.get(&name) {
            for dep in unit.file.requires.iter().chain(&unit.file.wants) {
                stack.push(dep.clone());
            }
        }
    }
    seen.into_iter().collect()
}

/// Units that `requires` this one, transitively — what a stop takes with it.
pub fn dependents(units: &BTreeMap<String, Unit>, root: &str) -> Vec<String> {
    let mut found = BTreeSet::new();
    let mut frontier = vec![root.to_string()];
    while let Some(name) = frontier.pop() {
        for (other, unit) in units {
            if unit.file.requires.contains(&name) && found.insert(other.clone()) {
                frontier.push(other.clone());
            }
        }
    }
    found.into_iter().collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Duration as ConfigDuration, UnitType};

    fn unit_from(json: &str) -> Unit {
        let file: UnitFile = serde_json::from_str(json).unwrap();
        Unit::new("api".into(), None, file)
    }

    fn plain() -> Unit {
        unit_from(r#"{"command":["a"]}"#)
    }

    #[test]
    fn a_crash_backs_off_and_a_clean_exit_stops() {
        let mut unit = plain();
        unit.phase = Phase::Running;
        unit.pty = Some(7);
        unit.note_exit(1, 1000, 0);
        assert_eq!(unit.phase, Phase::Backoff);
        assert_eq!(unit.failures, 1);

        let mut unit = plain();
        unit.phase = Phase::Running;
        unit.note_exit(0, 1000, 0);
        assert_eq!(unit.phase, Phase::Stopped, "exit 0 is not a failure");
    }

    #[test]
    fn restart_on_success_is_obeyed_literally() {
        let mut unit = unit_from(r#"{"command":["a"],"restartOnSuccess":true}"#);
        unit.phase = Phase::Running;
        unit.note_exit(0, 1000, 0);
        assert_eq!(unit.phase, Phase::Backoff);
    }

    #[test]
    fn restart_off_fails_on_a_crash_and_stops_on_a_clean_exit() {
        let mut unit = unit_from(r#"{"command":["a"],"restartOnFailure":false}"#);
        unit.phase = Phase::Running;
        unit.note_exit(3, 1000, 0);
        assert_eq!(unit.phase, Phase::Failed);
        assert_eq!(unit.last_exit, Some(3));
    }

    #[test]
    fn a_oneshot_that_succeeds_counts_as_ready() {
        let mut unit = unit_from(r#"{"command":["a"],"type":"oneshot"}"#);
        assert_eq!(unit.file.unit_type, UnitType::Oneshot);
        unit.phase = Phase::Activating;
        unit.note_exit(0, 1000, 0);
        assert_eq!(unit.phase, Phase::Exited);
        assert!(unit.phase.is_ready());
    }

    #[test]
    fn a_long_run_forgives_earlier_failures() {
        let mut unit = plain();
        unit.failures = 4;
        unit.phase = Phase::Running;
        unit.started_ms = 0;
        unit.note_exit(1, HEALTHY_AFTER.as_millis() as u64 + 1, 0);
        assert_eq!(unit.failures, 1, "reset, then counted this one");
    }

    #[test]
    fn start_limit_gives_up() {
        let mut unit = unit_from(r#"{"command":["a"],"startLimit":2}"#);
        unit.phase = Phase::Running;
        unit.note_exit(1, 10, 0);
        assert_eq!(unit.phase, Phase::Backoff);
        unit.phase = Phase::Running;
        unit.started_ms = 10;
        unit.note_exit(1, 20, 0);
        assert_eq!(unit.phase, Phase::Failed);
    }

    #[test]
    fn a_fixed_delay_replaces_the_schedule() {
        let mut unit = unit_from(r#"{"command":["a"],"restartDelay":"2s"}"#);
        assert_eq!(unit.file.restart_delay, Some(ConfigDuration(2000)));
        unit.phase = Phase::Running;
        unit.note_exit(1, 1000, u64::MAX);
        assert_eq!(unit.next_attempt_ms, 3000, "jitter does not apply");
    }

    #[test]
    fn backoff_is_bounded_and_jittered() {
        assert_eq!(backoff(0, 12345), Duration::ZERO);
        for failures in 1..40 {
            let delay = backoff(failures, u64::MAX);
            assert!(delay < BACKOFF_MAX, "{failures} gave {delay:?}");
        }
        // Full jitter: the same failure count spans its whole window.
        assert_eq!(backoff(4, 0), Duration::ZERO);
        assert!(backoff(4, u64::MAX) > Duration::from_millis(1));
    }

    #[test]
    fn retention_keeps_the_newest_and_hands_back_the_rest() {
        let mut unit = unit_from(r#"{"command":["a"],"keep":2,"restartOnFailure":true}"#);
        for run in 0..4u16 {
            unit.pty = Some(run);
            unit.seq = run as u64;
            unit.phase = Phase::Running;
            let closed = unit.note_exit(1, 1000 + run as u64, 0);
            if run < 2 {
                assert!(closed.is_empty(), "under the limit");
            } else {
                assert_eq!(closed.len(), 1, "one ages out per run past the limit");
            }
        }
        assert_eq!(unit.runs.len(), 2);
        assert_eq!(unit.runs[0].pty, 3, "newest first");
        assert_eq!(unit.runs[1].pty, 2);
    }

    #[test]
    fn keep_zero_retains_nothing() {
        let mut unit = unit_from(r#"{"command":["a"],"keep":0}"#);
        unit.pty = Some(7);
        unit.phase = Phase::Running;
        let closed = unit.note_exit(1, 1000, 0);
        assert_eq!(closed.len(), 1);
        assert!(unit.runs.is_empty());
    }

    fn graph(pairs: &[(&str, &str)]) -> BTreeMap<String, Unit> {
        let mut units = BTreeMap::new();
        for (name, json) in pairs {
            let file: UnitFile = serde_json::from_str(json).unwrap();
            units.insert(
                (*name).to_string(),
                Unit::new((*name).to_string(), None, file),
            );
        }
        units
    }

    #[test]
    fn dependencies_start_before_dependents() {
        let units = graph(&[
            ("db", r#"{"command":["db"]}"#),
            ("migrate", r#"{"command":["m"],"requires":["db"]}"#),
            ("api", r#"{"command":["a"],"requires":["db","migrate"]}"#),
        ]);
        let order = start_order(&units, &["api".into()]).unwrap();
        let at = |n: &str| order.iter().position(|x| x == n).unwrap();
        assert!(at("db") < at("migrate"));
        assert!(at("migrate") < at("api"));
    }

    #[test]
    fn a_cycle_is_refused_and_names_its_members() {
        let units = graph(&[
            ("a", r#"{"command":["a"],"requires":["b"]}"#),
            ("b", r#"{"command":["b"],"requires":["a"]}"#),
        ]);
        let Cycle(ring) = start_order(&units, &["a".into()]).unwrap_err();
        assert!(ring.contains(&"a".to_string()) && ring.contains(&"b".to_string()));
    }

    #[test]
    fn after_orders_without_pulling_in() {
        let units = graph(&[
            ("api", r#"{"command":["a"]}"#),
            ("stripe", r#"{"command":["s"],"after":["api"]}"#),
        ]);
        assert_eq!(start_closure(&units, "stripe"), vec!["stripe".to_string()]);
        let order = start_order(&units, &["stripe".into()]).unwrap();
        assert!(order.iter().position(|x| x == "api") < order.iter().position(|x| x == "stripe"));
    }

    #[test]
    fn wants_is_pulled_in_but_requires_is_what_cascades() {
        let units = graph(&[
            ("db", r#"{"command":["db"]}"#),
            ("mail", r#"{"command":["m"]}"#),
            (
                "api",
                r#"{"command":["a"],"requires":["db"],"wants":["mail"]}"#,
            ),
        ]);
        let closure = start_closure(&units, "api");
        assert!(closure.contains(&"mail".to_string()), "wants starts it");
        // Stopping the database takes the API with it; the mail catcher stays.
        assert_eq!(dependents(&units, "db"), vec!["api".to_string()]);
        assert!(dependents(&units, "mail").is_empty());
    }

    #[test]
    fn a_dangling_dependency_does_not_break_ordering() {
        let units = graph(&[("api", r#"{"command":["a"],"requires":["ghost"]}"#)]);
        assert!(start_order(&units, &["api".into()]).is_ok());
    }
}

#[cfg(test)]
mod tag_tests {
    use super::*;

    #[test]
    fn a_tag_round_trips_through_the_parser() {
        for (unit, seq) in [("api", 0u64), ("epic/gateway", 41), ("a-b_c.d", u64::MAX)] {
            let tag = tag_for(unit, seq);
            assert_eq!(parse_tag(&tag), Some((unit, seq)), "{tag}");
        }
    }

    #[test]
    fn a_unit_name_may_contain_the_separator() {
        // Only the last `/` separates the sequence, so a name is free to hold
        // one — which an instance of a stack in a nested directory can.
        let tag = tag_for("epic/stacks/web", 7);
        assert_eq!(parse_tag(&tag), Some(("epic/stacks/web", 7)));
    }

    #[test]
    fn foreign_and_malformed_tags_are_refused() {
        for tag in ["", "muster/", "muster/api", "session/api/0", "muster/api/x"] {
            assert_eq!(parse_tag(tag), None, "{tag}");
        }
    }
}

#[cfg(test)]
mod naming_tests {
    use super::*;

    #[test]
    fn an_instance_qualifies_its_templates_as_a_path() {
        assert_eq!(qualified("epic", "server"), "epic/server");
        assert_eq!(unqualify("epic/server"), Some(("epic", "server")));
    }

    #[test]
    fn a_plain_unit_is_never_mistaken_for_a_qualified_one() {
        // A top-level unit is named by a filename, which holds no separator,
        // so the two namespaces cannot collide.
        assert_eq!(unqualify("postgres"), None);
    }

    #[test]
    fn qualification_survives_the_tag_round_trip() {
        let name = qualified("epic", "server");
        let tag = tag_for(&name, 3);
        assert_eq!(tag, "muster/epic/server/3");
        assert_eq!(parse_tag(&tag), Some((name.as_str(), 3)));
    }
}

#[cfg(test)]
mod stop_tests {
    use super::*;

    fn running(json: &str) -> Unit {
        let file: UnitFile = serde_json::from_str(json).unwrap();
        let mut unit = Unit::new("api".into(), None, file);
        unit.phase = Phase::Running;
        unit.pty = Some(7);
        unit
    }

    #[test]
    fn a_stopped_unit_stays_stopped_when_its_signal_lands() {
        // `stop` sets the phase and signals; the exit arrives afterwards. The
        // signal makes that exit nonzero, so running the restart policy over
        // it would resurrect exactly what was asked to stop.
        let mut unit = running(r#"{"command":["a"]}"#);
        unit.phase = Phase::Held;
        unit.note_exit(143, 1000, 0);
        assert_eq!(unit.phase, Phase::Held);
        assert_eq!(unit.failures, 0, "a deliberate stop is not a failure");
        assert_eq!(unit.next_attempt_ms, 0, "and arms no retry");
    }

    #[test]
    fn a_cascade_stop_also_absorbs_its_own_exit() {
        let mut unit = running(r#"{"command":["a"],"restartOnSuccess":true}"#);
        unit.phase = Phase::Stopped;
        unit.note_exit(143, 1000, 0);
        assert_eq!(unit.phase, Phase::Stopped);
    }

    #[test]
    fn the_exit_is_still_recorded_and_reaped() {
        let mut unit = running(r#"{"command":["a"],"keep":0}"#);
        unit.phase = Phase::Held;
        let closed = unit.note_exit(143, 1000, 0);
        assert_eq!(unit.last_exit, Some(143), "status still visible in status");
        assert!(unit.pty.is_none(), "the terminal is no longer live");
        assert_eq!(closed.len(), 1, "keep:0 still reaps the run");
    }

    #[test]
    fn a_crash_while_running_is_untouched_by_this() {
        let mut unit = running(r#"{"command":["a"]}"#);
        unit.note_exit(1, 1000, 0);
        assert_eq!(unit.phase, Phase::Backoff);
        assert_eq!(unit.failures, 1);
    }
}
