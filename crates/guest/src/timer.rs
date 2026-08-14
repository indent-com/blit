//! Guest-side timers and packet dispatch over `blit_v1.wait`.
//!
//! [`EventLoop`] keeps one-shot callbacks in deadline order and always gives a
//! ready packet priority over due timers. It never polls the clock in a busy
//! loop: the nearest timer deadline is passed directly to the host wait call.

use alloc::{boxed::Box, collections::BinaryHeap, vec::Vec};
use core::{cmp::Ordering, time::Duration};

use crate::{Client, Error, MonotonicInstant, WaitOutcome};

/// Stable handle for a scheduled one-shot callback.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd)]
pub struct TimerId(u64);

impl TimerId {
    /// Process-local numeric value, useful for diagnostics.
    pub const fn get(self) -> u64 {
        self.0
    }
}

/// One unit of work dispatched by [`EventLoop::dispatch_once`].
#[derive(Debug, Eq, PartialEq)]
pub enum EventLoopEvent {
    /// One complete logical packet, including fragment reassembly.
    Packet(Vec<u8>),
    /// Every callback due at the observed monotonic time was run.
    Timers { fired: usize },
    /// The endpoint closed with no packet left to dispatch.
    Closed,
}

/// Why [`EventLoop::run`] returned normally.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EventLoopExit {
    /// A packet handler or timer callback called [`EventLoop::stop`].
    Stopped,
    /// The logical endpoint closed and its mailbox was empty.
    Closed,
}

type TimerCallback<'callback> = Box<dyn FnOnce(&mut Client, &mut EventLoop<'callback>) + 'callback>;

struct Timer<'callback> {
    id: TimerId,
    deadline: MonotonicInstant,
    order: u128,
    callback: TimerCallback<'callback>,
}

impl PartialEq for Timer<'_> {
    fn eq(&self, other: &Self) -> bool {
        self.id == other.id
    }
}

impl Eq for Timer<'_> {}

impl PartialOrd for Timer<'_> {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Timer<'_> {
    fn cmp(&self, other: &Self) -> Ordering {
        // BinaryHeap is a max-heap. Reverse deadline and insertion order so
        // its root is the earliest timer and equal deadlines remain FIFO.
        other
            .deadline
            .cmp(&self.deadline)
            .then_with(|| other.order.cmp(&self.order))
    }
}

/// Single-threaded packet loop with a guest-side monotonic timer min-heap.
///
/// Timer callbacks are one-shot. A callback receives the loop itself, so it
/// may schedule another timer, cancel one, or stop [`run`](Self::run).
pub struct EventLoop<'callback> {
    timers: BinaryHeap<Timer<'callback>>,
    next_id: u64,
    next_order: u128,
    stopped: bool,
}

impl Default for EventLoop<'_> {
    fn default() -> Self {
        Self::new()
    }
}

impl<'callback> EventLoop<'callback> {
    pub const fn new() -> Self {
        Self {
            timers: BinaryHeap::new(),
            next_id: 1,
            next_order: 0,
            stopped: false,
        }
    }

    /// Number of callbacks currently scheduled.
    pub fn len(&self) -> usize {
        self.timers.len()
    }

    pub fn is_empty(&self) -> bool {
        self.timers.is_empty()
    }

    /// Earliest scheduled deadline, if any.
    pub fn next_deadline(&self) -> Option<MonotonicInstant> {
        self.timers.peek().map(|timer| timer.deadline)
    }

    /// Schedule a one-shot callback at an absolute host-monotonic deadline.
    pub fn schedule_at<F>(&mut self, deadline: MonotonicInstant, callback: F) -> TimerId
    where
        F: FnOnce(&mut Client, &mut EventLoop<'callback>) + 'callback,
    {
        let id = self.allocate_id();
        let order = self.next_order;
        self.next_order = self.next_order.wrapping_add(1);
        self.timers.push(Timer {
            id,
            deadline,
            order,
            callback: Box::new(callback),
        });
        id
    }

    /// Schedule a one-shot callback relative to the current monotonic time.
    pub fn schedule_after<F>(&mut self, client: &Client, duration: Duration, callback: F) -> TimerId
    where
        F: FnOnce(&mut Client, &mut EventLoop<'callback>) + 'callback,
    {
        self.schedule_at(client.monotonic_now() + duration, callback)
    }

    /// Cancel a callback which has not run yet.
    pub fn cancel(&mut self, id: TimerId) -> bool {
        let before = self.timers.len();
        self.timers.retain(|timer| timer.id != id);
        self.timers.len() != before
    }

    /// Drop every pending callback.
    pub fn clear(&mut self) {
        self.timers.clear();
    }

    /// Request that [`run`](Self::run) return after the current dispatch.
    pub fn stop(&mut self) {
        self.stopped = true;
    }

    /// Clear a previous stop request so [`run`](Self::run) may be entered again.
    pub fn resume(&mut self) {
        self.stopped = false;
    }

    /// Dispatch one packet or one deadline wake.
    ///
    /// Packets already buffered by typed helpers are returned before calling
    /// the host. The host contract itself gives a newly queued packet priority
    /// over a simultaneous deadline, so callbacks cannot overtake packets.
    pub fn dispatch_once(&mut self, client: &mut Client) -> Result<EventLoopEvent, Error> {
        if let Some(packet) = client.pop_pending() {
            return Ok(EventLoopEvent::Packet(packet));
        }

        let deadline = self.next_deadline().unwrap_or(MonotonicInstant::MAX);
        match client.wait_until(deadline)? {
            WaitOutcome::Packet => match client.recv()? {
                Some(packet) => Ok(EventLoopEvent::Packet(packet)),
                None => Ok(EventLoopEvent::Closed),
            },
            WaitOutcome::Deadline => Ok(EventLoopEvent::Timers {
                fired: self.fire_due(client),
            }),
            WaitOutcome::Closed => Ok(EventLoopEvent::Closed),
        }
    }

    /// Run until the endpoint closes or [`stop`](Self::stop) is requested.
    ///
    /// Timer callbacks run internally. Complete packets are passed to
    /// `on_packet`; it may also schedule or cancel timers through the supplied
    /// loop reference.
    pub fn run<F>(&mut self, client: &mut Client, mut on_packet: F) -> Result<EventLoopExit, Error>
    where
        F: FnMut(&mut Client, &mut EventLoop<'callback>, Vec<u8>),
    {
        while !self.stopped {
            match self.dispatch_once(client)? {
                EventLoopEvent::Packet(packet) => on_packet(client, self, packet),
                EventLoopEvent::Timers { .. } => {}
                EventLoopEvent::Closed => return Ok(EventLoopExit::Closed),
            }
        }
        Ok(EventLoopExit::Stopped)
    }

    fn fire_due(&mut self, client: &mut Client) -> usize {
        let mut fired = 0usize;
        loop {
            let now = client.monotonic_now();
            if !self
                .timers
                .peek()
                .is_some_and(|timer| timer.deadline <= now)
            {
                break;
            }
            let timer = self.timers.pop().expect("peeked timer exists");
            (timer.callback)(client, self);
            fired = fired.saturating_add(1);
        }
        fired
    }

    fn allocate_id(&mut self) -> TimerId {
        loop {
            let id = TimerId(self.next_id);
            self.next_id = self.next_id.wrapping_add(1);
            if self.next_id == 0 {
                self.next_id = 1;
            }
            if id.0 != 0 && !self.timers.iter().any(|timer| timer.id == id) {
                return id;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        bootstrap::{EXT_INFO, EXT_INFO_INIT, FEATURE_EXTENSION, S2C_HELLO, S2C_READY},
        native_host,
    };
    use alloc::{collections::VecDeque, rc::Rc, vec};
    use core::cell::RefCell;

    #[derive(Default)]
    struct State {
        incoming: VecDeque<Vec<u8>>,
        monotonic: i64,
        waits: Vec<i64>,
        closed: bool,
    }

    struct MockHost(Rc<RefCell<State>>);

    impl native_host::Host for MockHost {
        fn send(&mut self, _packet: &[u8]) -> i32 {
            0
        }

        fn recv(&mut self, buffer: &mut [u8]) -> i32 {
            let mut state = self.0.borrow_mut();
            let Some(packet) = state.incoming.front() else {
                return 0;
            };
            let len = packet.len();
            if len <= buffer.len() {
                buffer[..len].copy_from_slice(packet);
                state.incoming.pop_front();
            }
            len as i32
        }

        fn wait(&mut self, deadline: i64) -> i32 {
            let mut state = self.0.borrow_mut();
            state.waits.push(deadline);
            if !state.incoming.is_empty() {
                return 1;
            }
            if state.closed {
                return 2;
            }
            state.monotonic = state.monotonic.max(deadline);
            0
        }

        fn clock(&mut self, kind: i32) -> i64 {
            assert_eq!(kind, 1);
            self.0.borrow().monotonic
        }

        fn random(&mut self, _destination: &mut [u8]) {}
    }

    fn hello() -> Vec<u8> {
        let mut packet = vec![S2C_HELLO];
        packet.extend_from_slice(&1u16.to_le_bytes());
        packet.extend_from_slice(&FEATURE_EXTENSION.to_le_bytes());
        packet.extend_from_slice(&55u64.to_le_bytes());
        packet.extend_from_slice(&4u16.to_le_bytes());
        packet.extend_from_slice(b"test");
        packet
    }

    fn init() -> Vec<u8> {
        let mut packet = vec![EXT_INFO, EXT_INFO_INIT];
        packet.extend_from_slice(&7u64.to_le_bytes());
        packet.extend_from_slice(&9u64.to_le_bytes());
        packet.extend_from_slice(&11u64.to_le_bytes());
        packet.extend_from_slice(&13u32.to_le_bytes());
        packet.push(0);
        packet.extend_from_slice(&[42; 32]);
        packet.extend_from_slice(&0u16.to_le_bytes());
        packet.extend_from_slice(&0u16.to_le_bytes());
        packet
    }

    fn boot() -> (native_host::Guard, Rc<RefCell<State>>, Client) {
        let state = Rc::new(RefCell::new(State::default()));
        state
            .borrow_mut()
            .incoming
            .extend([hello(), vec![S2C_READY], init()]);
        let guard = native_host::install(MockHost(Rc::clone(&state)));
        let client = Client::bootstrap().expect("valid bootstrap");
        state.borrow_mut().waits.clear();
        (guard, state, client)
    }

    #[test]
    fn nearest_deadline_is_waited_and_equal_deadlines_are_fifo() {
        let (_guard, state, mut client) = boot();
        state.borrow_mut().monotonic = 100;
        let calls = Rc::new(RefCell::new(Vec::new()));
        let mut event_loop = EventLoop::new();
        let late = Rc::clone(&calls);
        event_loop.schedule_after(&client, Duration::from_nanos(30), move |_, _| {
            late.borrow_mut().push(3)
        });
        for value in [1, 2] {
            let calls = Rc::clone(&calls);
            event_loop.schedule_after(&client, Duration::from_nanos(10), move |_, _| {
                calls.borrow_mut().push(value)
            });
        }

        assert_eq!(
            event_loop.dispatch_once(&mut client).unwrap(),
            EventLoopEvent::Timers { fired: 2 }
        );
        assert_eq!(*calls.borrow(), [1, 2]);
        assert_eq!(state.borrow().waits, [110]);
        assert_eq!(event_loop.next_deadline().unwrap().raw_nanos(), 130);

        assert_eq!(
            event_loop.dispatch_once(&mut client).unwrap(),
            EventLoopEvent::Timers { fired: 1 }
        );
        assert_eq!(*calls.borrow(), [1, 2, 3]);
        assert_eq!(state.borrow().waits, [110, 130]);
    }

    #[test]
    fn packet_wake_precedes_an_already_due_timer() {
        let (_guard, state, mut client) = boot();
        state.borrow_mut().incoming.push_back(vec![0x44]);
        let timer_ran = Rc::new(RefCell::new(false));
        let ran = Rc::clone(&timer_ran);
        let mut event_loop = EventLoop::new();
        event_loop.schedule_after(&client, Duration::ZERO, move |_, _| {
            *ran.borrow_mut() = true
        });

        assert_eq!(
            event_loop.dispatch_once(&mut client).unwrap(),
            EventLoopEvent::Packet(vec![0x44])
        );
        assert!(!*timer_ran.borrow());
        assert_eq!(
            event_loop.dispatch_once(&mut client).unwrap(),
            EventLoopEvent::Timers { fired: 1 }
        );
        assert!(*timer_ran.borrow());
        assert_eq!(state.borrow().waits, [0, 0]);
    }

    #[test]
    fn callbacks_can_cancel_reschedule_and_stop_the_loop() {
        let (_guard, _state, mut client) = boot();
        let calls = Rc::new(RefCell::new(Vec::new()));
        let mut event_loop = EventLoop::new();
        let cancelled_calls = Rc::clone(&calls);
        let cancelled = event_loop.schedule_after(&client, Duration::ZERO, move |_, _| {
            cancelled_calls.borrow_mut().push(9)
        });
        assert!(event_loop.cancel(cancelled));
        assert!(!event_loop.cancel(cancelled));

        let first_calls = Rc::clone(&calls);
        event_loop.schedule_after(&client, Duration::ZERO, move |client, event_loop| {
            first_calls.borrow_mut().push(1);
            let second_calls = Rc::clone(&first_calls);
            event_loop.schedule_after(client, Duration::ZERO, move |_, event_loop| {
                second_calls.borrow_mut().push(2);
                event_loop.stop();
            });
        });

        assert_eq!(
            event_loop
                .run(&mut client, |_, _, _| unreachable!())
                .unwrap(),
            EventLoopExit::Stopped
        );
        assert_eq!(*calls.borrow(), [1, 2]);
        assert!(event_loop.is_empty());
    }

    #[test]
    fn closed_endpoint_ends_an_idle_event_loop() {
        let (_guard, state, mut client) = boot();
        state.borrow_mut().closed = true;
        let mut event_loop = EventLoop::new();

        assert_eq!(
            event_loop
                .run(&mut client, |_, _, _| unreachable!())
                .unwrap(),
            EventLoopExit::Closed
        );
        assert_eq!(state.borrow().waits, [i64::MAX]);
    }
}
