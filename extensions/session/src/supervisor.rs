//! Desired-state bookkeeping for supervised applications.
//!
//! Split from the protocol plumbing so the parts that are easy to get wrong —
//! backoff, the failure-count reset, and refusing to double-spawn after a
//! restart — are testable without a server.

extern crate alloc;

use alloc::string::String;
use alloc::vec::Vec;
use core::time::Duration;

/// Backoff constants, matched to the server's own extension supervisor so two
/// layers of restart logic in one session behave the same way.
pub const BACKOFF_BASE: Duration = Duration::from_millis(250);
pub const BACKOFF_MAX: Duration = Duration::from_secs(30);
/// How long a run must last before its failures are forgiven. Without this a
/// crash loop that starts an hour into a session inherits a 30s delay from
/// failures nobody remembers.
pub const HEALTHY_AFTER: Duration = Duration::from_secs(60);

/// What the supervisor is doing about one application.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Phase {
    /// Enabled and believed to be running.
    Running,
    /// Enabled, waiting out a backoff before the next attempt.
    Backoff,
    /// Enabled but nothing has been started yet.
    Idle,
    /// Not enabled; nothing should be running.
    Stopped,
}

/// One supervised application.
#[derive(Clone, Debug)]
pub struct App {
    /// Desktop-entry id, and the name `@session enable <id>` uses.
    pub id: String,
    pub argv: Vec<String>,
    /// Operator intent, which survives a restart of anything.
    pub enabled: bool,
    pub phase: Phase,
    /// Endpoint-local handle for the live child, `None` when nothing runs.
    /// This is the id spawn/control/exit messages are addressed by, and it is
    /// only meaningful to this connection.
    pub process_id: Option<u32>,
    /// Server-global reference for the live child, learned from
    /// `PROCESS_STARTED`. Unlike `process_id` this outlives the connection,
    /// which is what lets a restarted extension re-adopt its own children
    /// instead of spawning a second copy of each.
    ///
    /// Meaningful only together with the boot generation it was recorded
    /// under: a new server hands out the same numbers to different processes.
    pub process_ref: Option<u64>,
    /// Consecutive failures, reset by a run that lasted `HEALTHY_AFTER`.
    pub failures: u32,
    /// Monotonic nanoseconds when the current attempt started.
    pub started_at_ns: Option<i64>,
    /// Monotonic nanoseconds when the next attempt may begin.
    pub next_attempt_ns: Option<i64>,
    /// Exit code of the last attempt, for `status` to report.
    pub last_exit: Option<i32>,
    /// Wayland socket basename this instance was given, so `status` can show
    /// which stamped socket to look for.
    pub wayland_display: Option<String>,
}

impl App {
    pub fn new(id: String, argv: Vec<String>) -> Self {
        Self {
            id,
            argv,
            enabled: false,
            phase: Phase::Stopped,
            process_id: None,
            process_ref: None,
            failures: 0,
            started_at_ns: None,
            next_attempt_ns: None,
            last_exit: None,
            wayland_display: None,
        }
    }
}

/// Delay before attempt number `failures` (1 = the first retry).
///
/// Exponential from `BACKOFF_BASE`, capped at `BACKOFF_MAX`, then full jitter:
/// a uniform sample in `[0, cap)`. Jitter matters because a session brings
/// several applications up at once, and a shared cause — a GPU reset, a
/// compositor restart — would otherwise have them all retry in lockstep
/// forever.
///
/// `random` supplies one uniform u64; the caller passes the host's entropy.
pub fn backoff(failures: u32, random: u64) -> Duration {
    if failures == 0 {
        return Duration::ZERO;
    }
    // Clamp the shift before it can reach 64 and wrap to zero.
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

impl App {
    /// Record that the current attempt exited, and decide what happens next.
    ///
    /// `now_ns` is the monotonic clock; `random` is one uniform u64 for jitter.
    pub fn note_exit(&mut self, code: i32, now_ns: i64, random: u64) {
        self.last_exit = Some(code);
        self.process_id = None;
        self.process_ref = None;
        self.wayland_display = None;
        // A run that stayed up is not evidence of a crash loop, whatever came
        // before it. Checked before the increment so one long run clears the
        // history rather than merely pausing its growth.
        let ran_long_enough = self
            .started_at_ns
            .map(|started| now_ns.saturating_sub(started) >= HEALTHY_AFTER.as_nanos() as i64)
            .unwrap_or(false);
        if ran_long_enough || code == 0 {
            self.failures = 0;
        }
        self.started_at_ns = None;
        self.next_attempt_ns = None;
        if !self.enabled {
            self.phase = Phase::Stopped;
            return;
        }
        // A clean exit is not something to recover from, so it is not retried.
        //
        // This is what stops a single-instance application respawning forever.
        // Start a second Brave while one is already up and the new process hands
        // its arguments to the running one and exits 0 immediately — every
        // browser and most Electron apps behave this way. Retrying that is an
        // infinite loop in which every iteration "succeeds".
        //
        // It is the right rule for a window the user simply closed, too: the
        // application is done, and `enabled` staying true means it starts again
        // with the next session rather than being fought over now.
        if code == 0 {
            self.phase = Phase::Stopped;
            return;
        }
        self.failures = self.failures.saturating_add(1);
        let delay = backoff(self.failures, random);
        self.phase = Phase::Backoff;
        self.next_attempt_ns = Some(now_ns.saturating_add(delay.as_nanos() as i64));
    }

    /// Record a started attempt.
    ///
    /// The server-global reference is not known yet — it arrives with
    /// `PROCESS_STARTED`, after the spawn has been accepted — so this records
    /// only the local handle and [`note_process_ref`](Self::note_process_ref)
    /// fills the other half in.
    pub fn note_started(&mut self, process_id: u32, wayland_display: String, now_ns: i64) {
        self.phase = Phase::Running;
        self.process_id = Some(process_id);
        self.process_ref = None;
        self.wayland_display = Some(wayland_display);
        self.started_at_ns = Some(now_ns);
        self.next_attempt_ns = None;
    }

    /// Record the server-global reference for the attempt already running.
    ///
    /// Ignored unless it still describes the live child: a `PROCESS_STARTED`
    /// can arrive after the app has already been stopped or has exited, and
    /// persisting that reference would have the next session adopt a corpse.
    pub fn note_process_ref(&mut self, process_id: u32, process_ref: u64) -> bool {
        if self.process_id != Some(process_id) {
            return false;
        }
        self.process_ref = Some(process_ref);
        true
    }

    /// Re-adopt a child that outlived the extension that spawned it.
    ///
    /// `started_at_ns` is set to now rather than to the original start, which
    /// is the conservative choice: the real uptime is unknown, so the run has
    /// to earn `HEALTHY_AFTER` again before its failures are forgiven.
    pub fn note_adopted(
        &mut self,
        process_id: u32,
        process_ref: u64,
        wayland_display: Option<String>,
        now_ns: i64,
    ) {
        self.phase = Phase::Running;
        self.process_id = Some(process_id);
        self.process_ref = Some(process_ref);
        self.wayland_display = wayland_display;
        self.started_at_ns = Some(now_ns);
        self.next_attempt_ns = None;
    }

    /// Whether an attempt is due at `now_ns`.
    pub fn attempt_due(&self, now_ns: i64) -> bool {
        if !self.enabled {
            return false;
        }
        match self.phase {
            Phase::Idle => true,
            Phase::Backoff => self.next_attempt_ns.is_none_or(|due| now_ns >= due),
            Phase::Running | Phase::Stopped => false,
        }
    }
}

/// The earliest deadline any application is waiting on, if any.
///
/// Takes an iterator rather than a slice: this runs once per packet, and
/// collecting a `Vec<App>` to find a minimum meant deep-cloning every id,
/// argv and socket name on every wake-up.
pub fn next_deadline_ns<'a>(apps: impl Iterator<Item = &'a App>) -> Option<i64> {
    apps.filter(|app| app.enabled && app.phase == Phase::Backoff)
        .filter_map(|app| app.next_attempt_ns)
        .min()
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::string::ToString;
    use alloc::vec;

    fn app() -> App {
        let mut app = App::new("legcord".to_string(), vec!["legcord".to_string()]);
        app.enabled = true;
        app.phase = Phase::Idle;
        app
    }

    /// Full jitter means the delay is *at most* the cap, never exactly it — so
    /// asserting a fixed value would be asserting the absence of jitter.
    #[test]
    fn backoff_grows_then_caps_and_always_stays_under_the_cap() {
        assert_eq!(backoff(0, u64::MAX), Duration::ZERO);
        // u64::MAX % cap is cap-1, the largest a sample can be.
        assert!(backoff(1, u64::MAX) < BACKOFF_BASE);
        assert!(backoff(4, u64::MAX) < BACKOFF_BASE * 8);
        for failures in [10, 20, 100, u32::MAX] {
            assert!(
                backoff(failures, u64::MAX) < BACKOFF_MAX,
                "{failures} failures must stay under the cap"
            );
        }
        // A zero sample is legal and means retry at once.
        assert_eq!(backoff(5, 0), Duration::ZERO);
    }

    #[test]
    fn a_crash_schedules_a_retry_and_a_clean_long_run_forgives_the_history() {
        let mut app = app();
        app.note_started(7, "blit-app-legcord-a".to_string(), 0);
        assert_eq!(app.phase, Phase::Running);

        // Dies immediately: one failure, and a retry is scheduled.
        app.note_exit(1, 1_000, 0);
        assert_eq!(app.phase, Phase::Backoff);
        assert_eq!(app.failures, 1);
        assert_eq!(app.last_exit, Some(1));
        // Both handles are dropped — reusing either after exit would watch a
        // corpse, and a persisted process_ref would have the next session
        // adopt one.
        assert!(app.process_id.is_none());
        assert!(app.process_ref.is_none());

        // Dies immediately again: failures accumulate.
        app.note_started(8, "d".to_string(), 2_000);
        app.note_exit(1, 3_000, 0);
        assert_eq!(app.failures, 2);

        // Now a run that lasts: the history is cleared, so the next retry is
        // fast again rather than inheriting a long delay.
        app.note_started(9, "d".to_string(), 0);
        let long = HEALTHY_AFTER.as_nanos() as i64 + 1;
        app.note_exit(1, long, 0);
        assert_eq!(app.failures, 1, "a healthy run resets the count");
    }

    /// The Brave case: a single-instance application handed its arguments to the
    /// copy already running and exited 0 at once. Retrying makes an infinite
    /// loop out of a success, so a clean exit must not schedule an attempt —
    /// however fast it came, and however many times it happens.
    #[test]
    fn a_clean_exit_is_not_retried_however_quickly_it_arrives() {
        let mut app = app();
        for round in 0..5 {
            app.note_started(round + 1, "d".to_string(), 0);
            app.note_exit(0, 1, 0);
            assert_eq!(
                app.phase,
                Phase::Stopped,
                "round {round}: a clean exit must not queue another attempt"
            );
            assert!(app.next_attempt_ns.is_none());
            assert!(
                !app.attempt_due(i64::MAX),
                "round {round}: nothing may become due later either"
            );
            assert_eq!(app.failures, 0, "a clean exit is not a failure");
            // Still enabled, so the next session starts it again.
            assert!(app.enabled);
        }
    }

    /// A crash is still retried — the clean-exit rule must not have turned the
    /// supervisor into something that never restarts anything.
    #[test]
    fn a_failing_exit_is_still_retried() {
        let mut app = app();
        app.note_started(1, "d".to_string(), 0);
        app.note_exit(1, 1, 0);
        assert_eq!(app.phase, Phase::Backoff);
        assert_eq!(app.failures, 1);
        assert!(app.next_attempt_ns.is_some());
    }

    #[test]
    fn disabling_stops_the_restart_loop() {
        let mut app = app();
        app.note_started(1, "d".to_string(), 0);
        app.enabled = false;
        app.note_exit(0, 1_000, 0);
        assert_eq!(app.phase, Phase::Stopped);
        assert!(app.next_attempt_ns.is_none());
        assert!(!app.attempt_due(i64::MAX), "a disabled app is never due");
    }

    #[test]
    fn an_attempt_is_due_only_once_its_backoff_elapses() {
        let mut app = app();
        assert!(app.attempt_due(0), "an idle enabled app starts at once");
        app.note_started(1, "d".to_string(), 0);
        assert!(!app.attempt_due(i64::MAX), "a running app is not due");
        app.note_exit(1, 0, u64::MAX);
        let due = app.next_attempt_ns.expect("a deadline was set");
        assert!(!app.attempt_due(due - 1));
        assert!(app.attempt_due(due));
    }

    #[test]
    fn the_next_deadline_is_the_earliest_pending_one() {
        let mut a = app();
        let mut b = App::new("other".to_string(), vec!["other".to_string()]);
        b.enabled = true;
        a.phase = Phase::Backoff;
        a.next_attempt_ns = Some(500);
        b.phase = Phase::Backoff;
        b.next_attempt_ns = Some(200);
        assert_eq!(next_deadline_ns([&a, &b].into_iter()), Some(200));
        // A running app is not waiting on anything.
        a.phase = Phase::Running;
        b.phase = Phase::Running;
        assert_eq!(next_deadline_ns([&a, &b].into_iter()), None);
    }

    /// The server-global reference is what a restarted extension re-adopts by,
    /// so it must only ever describe the child that is live right now.
    #[test]
    fn a_process_ref_is_only_recorded_for_the_attempt_still_running() {
        let mut app = app();
        app.note_started(7, "d".to_string(), 0);
        assert!(app.process_ref.is_none(), "not known until STARTED lands");
        assert!(app.note_process_ref(7, 0xdead));
        assert_eq!(app.process_ref, Some(0xdead));

        // A STARTED for a superseded attempt must not overwrite it.
        assert!(!app.note_process_ref(6, 0xbeef));
        assert_eq!(app.process_ref, Some(0xdead));

        // After an exit there is nothing to attach a reference to, so a
        // straggling STARTED cannot resurrect one.
        app.note_exit(1, 1_000, 0);
        assert!(!app.note_process_ref(7, 0xbeef));
        assert!(app.process_ref.is_none());
    }

    /// Re-adoption replaces a spawn, so the app must come back as Running --
    /// anything else and `reconcile` starts a second copy of a live child.
    #[test]
    fn an_adopted_child_is_running_and_not_due_for_another_attempt() {
        let mut app = app();
        app.failures = 3;
        app.note_adopted(42, 0xfeed, Some("blit-app-legcord-a".to_string()), 5_000);
        assert_eq!(app.phase, Phase::Running);
        assert_eq!(app.process_id, Some(42));
        assert_eq!(app.process_ref, Some(0xfeed));
        assert!(!app.attempt_due(i64::MAX), "an adopted child is not due");

        // Uptime is measured from the adoption, not from the unknown original
        // start, so the run still has to earn the reset.
        app.note_exit(1, 5_000 + HEALTHY_AFTER.as_nanos() as i64 - 1, 0);
        assert_eq!(app.failures, 4, "a short post-adoption run is a failure");
    }
}
