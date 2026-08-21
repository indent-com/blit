use alloc::{collections::VecDeque, vec, vec::Vec};
use core::{
    fmt, mem,
    ops::{Add, Sub},
    time::Duration,
};

use crate::{
    bootstrap::{
        self, Context, EXT_INFO, EXT_INFO_INIT, S2C_AUDIO_FRAME, S2C_FRAGMENT, S2C_HELLO, S2C_READY,
    },
    host,
};

const INITIAL_RECEIVE_CAPACITY: usize = 64 * 1024;
const MAX_LOGICAL_MESSAGE: usize = 64 * 1024 * 1024;
const MAX_FRAGMENT_COUNT: usize = 16_384;
const MAX_PENDING_BYTES: usize = 64 * 1024 * 1024;
const FRAGMENT_FLAG_LAST: u8 = 1;

/// Exit code returned when the entry wrapper cannot complete private bootstrap.
pub const EXIT_BOOTSTRAP_FAILURE: i32 = 70;

pub use host::WaitOutcome;

/// Guest SDK error.
#[derive(Debug)]
pub enum Error {
    Host(host::Error),
    Bootstrap(bootstrap::Error),
    EndpointClosed,
    SendRejected,
    InvalidFragment,
    FragmentTooLarge,
    TooManyFragments,
    PendingBufferFull,
    AllocationFailed,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Host(error) => write!(f, "host ABI error: {error}"),
            Self::Bootstrap(error) => write!(f, "extension bootstrap error: {error}"),
            Self::EndpointClosed => f.write_str("extension endpoint is closed"),
            Self::SendRejected => f.write_str("host rejected a prevalidated packet size"),
            Self::InvalidFragment => f.write_str("invalid or interleaved S2C fragment sequence"),
            Self::FragmentTooLarge => f.write_str("fragment sequence exceeds 64 MiB"),
            Self::TooManyFragments => f.write_str("fragment sequence exceeds 16,384 chunks"),
            Self::PendingBufferFull => f.write_str("pending packet buffer exceeds 64 MiB"),
            Self::AllocationFailed => f.write_str("guest packet allocation failed"),
        }
    }
}

#[cfg(not(target_arch = "wasm32"))]
impl std::error::Error for Error {}

impl From<host::Error> for Error {
    fn from(value: host::Error) -> Self {
        Self::Host(value)
    }
}

impl From<bootstrap::Error> for Error {
    fn from(value: bootstrap::Error) -> Self {
        Self::Bootstrap(value)
    }
}

/// Signed nanoseconds since the Unix epoch.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Realtime(i64);

impl Realtime {
    pub const fn unix_timestamp_nanos(self) -> i64 {
        self.0
    }
}

/// Opaque point in the current attempt's host monotonic-clock domain.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct MonotonicInstant(i64);

impl MonotonicInstant {
    /// The conventional no-deadline value accepted by `blit_v1.wait`.
    pub const MAX: Self = Self(i64::MAX);

    pub const fn raw_nanos(self) -> i64 {
        self.0
    }

    /// Construct an instant from the current attempt's raw host clock domain.
    pub const fn from_raw_nanos(nanos: i64) -> Self {
        Self(nanos)
    }

    pub fn checked_duration_since(self, earlier: Self) -> Option<Duration> {
        let nanos = self.0.checked_sub(earlier.0)?;
        (nanos >= 0).then(|| Duration::from_nanos(nanos as u64))
    }

    fn saturating_add(self, duration: Duration) -> Self {
        let nanos = i64::try_from(duration.as_nanos()).unwrap_or(i64::MAX);
        Self(self.0.saturating_add(nanos))
    }
}

impl Add<Duration> for MonotonicInstant {
    type Output = Self;

    fn add(self, rhs: Duration) -> Self::Output {
        self.saturating_add(rhs)
    }
}

impl Sub for MonotonicInstant {
    type Output = Duration;

    fn sub(self, rhs: Self) -> Self::Output {
        self.checked_duration_since(rhs).unwrap_or(Duration::ZERO)
    }
}

/// A fully bootstrapped extension endpoint.
pub struct Client {
    context: Context,
    receiver: Receiver,
    pending: VecDeque<Vec<u8>>,
    pending_bytes: usize,
    #[cfg(feature = "protocol")]
    next_channel_id: u32,
    #[cfg(feature = "protocol")]
    next_extension_nonce: u16,
}

impl Client {
    /// Consume `HELLO`, the normal initial burst through `READY`, and the
    /// private `EXT_INFO(INIT)`. No send API is available before this returns.
    pub fn bootstrap() -> Result<Self, Error> {
        Self::bootstrap_with_initial(drop)
    }

    /// Bootstrap while handing each normal pre-`READY` packet to a consumer.
    ///
    /// Packets are delivered one at a time and are not also retained by the
    /// client. This lets a guest build only the initial state it needs without
    /// requiring enough memory for the aggregate bootstrap burst. In
    /// particular, a legal 64 MiB logical packet can be consumed before the
    /// remaining handshake arrives. [`bootstrap`](Self::bootstrap)
    /// deliberately consumes and discards these normal initial-state packets.
    pub fn bootstrap_with_initial(mut consume: impl FnMut(Vec<u8>)) -> Result<Self, Error> {
        let mut receiver = Receiver::new();
        let hello_packet = receiver
            .recv_logical()?
            .ok_or(bootstrap::Error::EndpointClosed)?;
        let hello = bootstrap::parse_hello(&hello_packet)?;

        loop {
            let packet = receiver
                .recv_logical()?
                .ok_or(bootstrap::Error::EndpointClosed)?;
            match packet.as_slice() {
                [S2C_READY] => break,
                [S2C_HELLO, ..] => return Err(bootstrap::Error::DuplicateHello.into()),
                [EXT_INFO, EXT_INFO_INIT, ..] => {
                    return Err(bootstrap::Error::InitBeforeReady.into());
                }
                _ => consume(packet),
            }
        }

        let init = receiver
            .recv_logical()?
            .ok_or(bootstrap::Error::EndpointClosed)?;
        let context = bootstrap::parse_init(&init, hello)?;
        Ok(Self {
            context,
            receiver,
            pending: VecDeque::new(),
            pending_bytes: 0,
            #[cfg(feature = "protocol")]
            next_channel_id: 2,
            #[cfg(feature = "protocol")]
            next_extension_nonce: 1,
        })
    }

    /// Immutable attempt identity and argument vector.
    pub const fn context(&self) -> &Context {
        &self.context
    }

    /// Send one complete client-to-server packet.
    pub fn send(&mut self, packet: &[u8]) -> Result<(), Error> {
        match host::send(packet)? {
            host::SendOutcome::Accepted => Ok(()),
            host::SendOutcome::Closed => Err(Error::EndpointClosed),
            host::SendOutcome::RejectedSize => Err(Error::SendRejected),
        }
    }

    /// Receive one complete logical server packet, reassembling fragments.
    pub fn recv(&mut self) -> Result<Option<Vec<u8>>, Error> {
        if let Some(packet) = self.pop_pending() {
            return Ok(Some(packet));
        }
        self.receiver.recv_logical()
    }

    /// Receive until a predicate accepts a packet while preserving unmatched
    /// packets for subsequent [`recv`](Self::recv) calls.
    pub fn recv_matching(
        &mut self,
        predicate: impl FnMut(&[u8]) -> bool,
    ) -> Result<Option<Vec<u8>>, Error> {
        self.recv_matching_deadline(predicate, MonotonicInstant::MAX)
    }

    /// Like [`recv_matching`](Self::recv_matching), but returns `Ok(None)` if the
    /// deadline expires before a matching packet arrives.
    pub fn recv_matching_deadline(
        &mut self,
        mut predicate: impl FnMut(&[u8]) -> bool,
        deadline: MonotonicInstant,
    ) -> Result<Option<Vec<u8>>, Error> {
        let mut skipped = VecDeque::new();
        let mut skipped_bytes = 0usize;
        loop {
            let next = if let Some(packet) = self.pending.pop_front() {
                self.pending_bytes -= packet.len();
                Some(packet)
            } else if deadline == MonotonicInstant::MAX {
                match self.receiver.recv_logical() {
                    Ok(packet) => packet,
                    Err(error) => {
                        self.restore_skipped(skipped, skipped_bytes);
                        return Err(error);
                    }
                }
            } else {
                match self.wait_until(deadline)? {
                    WaitOutcome::Closed => {
                        self.restore_skipped(skipped, skipped_bytes);
                        return Err(Error::EndpointClosed);
                    }
                    WaitOutcome::Deadline => {
                        self.restore_skipped(skipped, skipped_bytes);
                        return Ok(None);
                    }
                    WaitOutcome::Packet => match self.receiver.recv_logical() {
                        Ok(packet) => packet,
                        Err(error) => {
                            self.restore_skipped(skipped, skipped_bytes);
                            return Err(error);
                        }
                    },
                }
            };
            let Some(packet) = next else {
                self.restore_skipped(skipped, skipped_bytes);
                return Ok(None);
            };
            if predicate(&packet) {
                self.restore_skipped(skipped, skipped_bytes);
                return Ok(Some(packet));
            }
            skipped_bytes = skipped_bytes
                .checked_add(packet.len())
                .ok_or(Error::PendingBufferFull)?;
            if skipped_bytes > MAX_PENDING_BYTES {
                self.restore_skipped(skipped, skipped_bytes - packet.len());
                return Err(Error::PendingBufferFull);
            }
            skipped.push_back(packet);
        }
    }

    pub fn realtime_now(&self) -> Realtime {
        Realtime(host::clock(host::ClockKind::Realtime))
    }

    pub fn monotonic_now(&self) -> MonotonicInstant {
        MonotonicInstant(host::clock(host::ClockKind::Monotonic))
    }

    /// Park without dequeuing until an absolute monotonic deadline.
    pub fn wait_until(&self, deadline: MonotonicInstant) -> Result<WaitOutcome, Error> {
        host::wait(deadline.0).map_err(Into::into)
    }

    /// Park until a packet arrives or the endpoint closes.
    pub fn wait(&self) -> Result<WaitOutcome, Error> {
        host::wait(i64::MAX).map_err(Into::into)
    }

    /// Sleep efficiently while continuing to drain complete incoming packets
    /// into the bounded pending queue.
    pub fn sleep(&mut self, duration: Duration) -> Result<(), Error> {
        let deadline = self.monotonic_now() + duration;
        loop {
            match self.wait_until(deadline)? {
                WaitOutcome::Deadline => return Ok(()),
                WaitOutcome::Closed => return Err(Error::EndpointClosed),
                WaitOutcome::Packet => {
                    let packet = self.receiver.recv_logical()?.ok_or(Error::EndpointClosed)?;
                    self.push_pending(packet)?;
                }
            }
        }
    }

    /// Fill bytes directly from host entropy.
    pub fn random(&self, destination: &mut [u8]) -> Result<(), Error> {
        host::random(destination).map_err(Into::into)
    }

    fn push_pending(&mut self, packet: Vec<u8>) -> Result<(), Error> {
        push_bounded(&mut self.pending, &mut self.pending_bytes, packet)
    }

    pub(crate) fn pop_pending(&mut self) -> Option<Vec<u8>> {
        let packet = self.pending.pop_front()?;
        self.pending_bytes -= packet.len();
        Some(packet)
    }

    fn restore_skipped(&mut self, mut skipped: VecDeque<Vec<u8>>, skipped_bytes: usize) {
        while let Some(packet) = skipped.pop_back() {
            self.pending.push_front(packet);
        }
        self.pending_bytes += skipped_bytes;
    }

    #[cfg(feature = "protocol")]
    pub(crate) fn allocate_channel_id(&mut self) -> u32 {
        let channel_id = self.next_channel_id;
        self.next_channel_id = self.next_channel_id.wrapping_add(2);
        if self.next_channel_id == 0 {
            self.next_channel_id = 2;
        }
        channel_id
    }

    #[cfg(feature = "protocol")]
    pub(crate) fn allocate_extension_nonce(&mut self) -> u16 {
        let nonce = self.next_extension_nonce;
        self.next_extension_nonce = self.next_extension_nonce.wrapping_add(1);
        if self.next_extension_nonce == 0 {
            self.next_extension_nonce = 1;
        }
        nonce
    }
}

fn push_bounded(
    pending: &mut VecDeque<Vec<u8>>,
    pending_bytes: &mut usize,
    packet: Vec<u8>,
) -> Result<(), Error> {
    let total = pending_bytes
        .checked_add(packet.len())
        .ok_or(Error::PendingBufferFull)?;
    if total > MAX_PENDING_BYTES {
        return Err(Error::PendingBufferFull);
    }
    pending.push_back(packet);
    *pending_bytes = total;
    Ok(())
}

struct Receiver {
    buffer: Vec<u8>,
    fragments: Vec<u8>,
    fragment_count: usize,
}

#[cfg(test)]
#[allow(clippy::items_after_test_module)]
mod tests {
    use super::*;
    use crate::native_host;
    use alloc::{rc::Rc, string::ToString};
    use std::cell::RefCell;

    #[derive(Default)]
    struct State {
        incoming: VecDeque<Vec<u8>>,
        sent: Vec<Vec<u8>>,
        recv_capacities: Vec<usize>,
        wait_deadlines: Vec<i64>,
        random_chunks: Vec<usize>,
        random_byte: u8,
        realtime: i64,
        monotonic: i64,
        closed: bool,
    }

    struct MockHost(Rc<RefCell<State>>);

    impl native_host::Host for MockHost {
        fn send(&mut self, packet: &[u8]) -> i32 {
            let mut state = self.0.borrow_mut();
            if state.closed {
                return -1;
            }
            state.sent.push(packet.to_vec());
            0
        }

        fn recv(&mut self, buffer: &mut [u8]) -> i32 {
            let mut state = self.0.borrow_mut();
            state.recv_capacities.push(buffer.len());
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
            state.wait_deadlines.push(deadline);
            if !state.incoming.is_empty() {
                1
            } else if deadline <= state.monotonic {
                0
            } else if state.closed {
                2
            } else {
                state.monotonic = deadline;
                0
            }
        }

        fn clock(&mut self, kind: i32) -> i64 {
            let state = self.0.borrow();
            match kind {
                0 => state.realtime,
                1 => state.monotonic,
                _ => panic!("invalid clock kind"),
            }
        }

        fn random(&mut self, destination: &mut [u8]) {
            let mut state = self.0.borrow_mut();
            state.random_chunks.push(destination.len());
            for byte in destination {
                *byte = state.random_byte;
                state.random_byte = state.random_byte.wrapping_add(1);
            }
        }
    }

    fn hello() -> Vec<u8> {
        let mut packet = vec![S2C_HELLO];
        packet.extend_from_slice(&1u16.to_le_bytes());
        packet.extend_from_slice(&bootstrap::FEATURE_EXTENSION.to_le_bytes());
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
        packet.push(0b1111);
        packet.extend_from_slice(&[42; 32]);
        packet.extend_from_slice(&4u16.to_le_bytes());
        packet.extend_from_slice(b"demo");
        packet.extend_from_slice(&2u16.to_le_bytes());
        packet.extend_from_slice(&5u32.to_le_bytes());
        packet.extend_from_slice(b"alpha");
        packet.extend_from_slice(&5u32.to_le_bytes());
        packet.extend_from_slice("βeta".as_bytes());
        packet
    }

    fn boot_state(initial: impl IntoIterator<Item = Vec<u8>>) -> Rc<RefCell<State>> {
        let state = Rc::new(RefCell::new(State::default()));
        state.borrow_mut().incoming.extend(
            [hello()]
                .into_iter()
                .chain(initial)
                .chain([vec![S2C_READY], init()]),
        );
        state
    }

    fn boot(state: &Rc<RefCell<State>>) -> (native_host::Guard, Client) {
        let guard = native_host::install(MockHost(Rc::clone(state)));
        let client = Client::bootstrap().expect("valid bootstrap");
        (guard, client)
    }

    #[test]
    fn bootstrap_parses_context_and_consumes_initial_packets() {
        let state = boot_state([vec![0x03, 1, 0, 99]]);
        state.borrow_mut().realtime = -15;
        state.borrow_mut().monotonic = 200;
        let (_guard, mut client) = boot(&state);

        assert_eq!(client.context().hello.protocol_version, 1);
        assert_eq!(client.context().hello.boot_generation, Some(55));
        assert_eq!(
            client.context().hello.server_version.as_deref(),
            Some("test")
        );
        assert_eq!(client.context().extension_id, 7);
        assert_eq!(client.context().definition_revision, 9);
        assert_eq!(client.context().attempt, 11);
        assert_eq!(client.context().task_id, 13);
        assert_eq!(client.context().module_hash, [42; 32]);
        assert_eq!(client.context().name.as_deref(), Some("demo"));
        assert_eq!(
            client.context().args,
            ["alpha".to_string(), "βeta".to_string()]
        );
        assert!(client.context().detached);
        assert!(client.context().persistent);
        assert!(client.context().enabled);
        assert!(client.context().desired_running);
        assert_eq!(client.realtime_now().unix_timestamp_nanos(), -15);
        assert_eq!(client.monotonic_now().raw_nanos(), 200);
        state.borrow_mut().incoming.push_back(vec![0x44]);
        assert_eq!(client.recv().unwrap(), Some(vec![0x44]));

        client.send(&[0x08]).unwrap();
        assert_eq!(state.borrow().sent, [vec![0x08]]);
    }

    #[test]
    fn bootstrap_can_stream_initial_packets_to_application_state() {
        let state = boot_state([vec![0x03, 1, 0, 99], vec![0x04, 2]]);
        let guard = native_host::install(MockHost(Rc::clone(&state)));
        let mut initial = Vec::new();
        let mut client = Client::bootstrap_with_initial(|packet| initial.push(packet)).unwrap();

        assert_eq!(initial, [vec![0x03, 1, 0, 99], vec![0x04, 2]]);
        assert_eq!(client.recv().unwrap(), None);
        drop(guard);
    }

    #[test]
    fn receive_retries_with_exact_host_reported_capacity() {
        let state = boot_state([]);
        let (_guard, mut client) = boot(&state);
        state.borrow_mut().recv_capacities.clear();
        state.borrow_mut().incoming.push_back(vec![6; 100_000]);

        assert_eq!(client.recv().unwrap().unwrap().len(), 100_000);
        assert_eq!(state.borrow().recv_capacities, [64 * 1024, 100_000]);
    }

    #[test]
    fn fragments_reassemble_around_audio_packets() {
        let state = boot_state([]);
        let (_guard, mut client) = boot(&state);
        state.borrow_mut().incoming.extend([
            vec![S2C_FRAGMENT, 0, 0x91, 1],
            vec![S2C_AUDIO_FRAME, 7, 8],
            vec![S2C_FRAGMENT, FRAGMENT_FLAG_LAST, 2, 3],
        ]);

        assert_eq!(client.recv().unwrap(), Some(vec![S2C_AUDIO_FRAME, 7, 8]));
        assert_eq!(client.recv().unwrap(), Some(vec![0x91, 1, 2, 3]));
    }

    #[test]
    fn fragments_reject_empty_non_final_chunks() {
        let state = boot_state([]);
        let (_guard, mut client) = boot(&state);
        state.borrow_mut().incoming.push_back(vec![S2C_FRAGMENT, 0]);

        assert!(matches!(client.recv(), Err(Error::InvalidFragment)));
    }

    #[test]
    fn recv_matching_keeps_unmatched_packets_in_order() {
        let state = boot_state([]);
        let (_guard, mut client) = boot(&state);
        state
            .borrow_mut()
            .incoming
            .extend([vec![1], vec![2], vec![3], vec![4]]);

        assert_eq!(
            client.recv_matching(|packet| packet == [3]).unwrap(),
            Some(vec![3])
        );
        assert_eq!(client.recv().unwrap(), Some(vec![1]));
        assert_eq!(client.recv().unwrap(), Some(vec![2]));
        assert_eq!(client.recv().unwrap(), Some(vec![4]));
    }

    #[test]
    fn recv_matching_deadline_restores_packets_when_time_expires() {
        let state = boot_state([]);
        let (_guard, mut client) = boot(&state);
        state.borrow_mut().monotonic = 100;
        state.borrow_mut().incoming.extend([vec![1], vec![2]]);

        assert_eq!(
            client
                .recv_matching_deadline(|packet| packet == [3], MonotonicInstant(125))
                .unwrap(),
            None
        );
        assert_eq!(state.borrow().wait_deadlines, [125, 125, 125]);
        assert_eq!(client.recv().unwrap(), Some(vec![1]));
        assert_eq!(client.recv().unwrap(), Some(vec![2]));
    }

    #[test]
    fn entropy_fills_are_chunked_at_host_limit() {
        let state = boot_state([]);
        let (_guard, client) = boot(&state);
        let mut bytes = vec![0; host::MAX_RANDOM_CHUNK * 2 + 1];

        client.random(&mut bytes).unwrap();
        assert_eq!(
            state.borrow().random_chunks,
            [host::MAX_RANDOM_CHUNK, host::MAX_RANDOM_CHUNK, 1]
        );
        assert_ne!(&bytes[..16], &[0; 16]);
    }

    #[test]
    fn sleep_drains_packets_and_reuses_one_absolute_deadline() {
        let state = boot_state([]);
        let (_guard, mut client) = boot(&state);
        state.borrow_mut().monotonic = 100;
        state.borrow_mut().incoming.push_back(vec![0x44]);

        client.sleep(Duration::from_nanos(25)).unwrap();

        assert_eq!(state.borrow().wait_deadlines, [125, 125]);
        assert_eq!(state.borrow().monotonic, 125);
        assert_eq!(client.recv().unwrap(), Some(vec![0x44]));
    }

    #[test]
    fn bootstrap_rejects_init_before_ready() {
        let state = Rc::new(RefCell::new(State::default()));
        state.borrow_mut().incoming.extend([hello(), init()]);
        let _guard = native_host::install(MockHost(state));

        assert!(matches!(
            Client::bootstrap(),
            Err(Error::Bootstrap(bootstrap::Error::InitBeforeReady))
        ));
    }
}

impl Receiver {
    fn new() -> Self {
        Self {
            buffer: vec![0; INITIAL_RECEIVE_CAPACITY],
            fragments: Vec::new(),
            fragment_count: 0,
        }
    }

    fn recv_logical(&mut self) -> Result<Option<Vec<u8>>, Error> {
        loop {
            let packet = match self.recv_frame()? {
                Some(packet) => packet,
                None if self.fragments.is_empty() => return Ok(None),
                None => return Err(Error::InvalidFragment),
            };
            if packet.first() == Some(&S2C_FRAGMENT) {
                let complete = self.push_fragment(&packet)?;
                if complete.is_some() {
                    return Ok(complete);
                }
                continue;
            }
            if !self.fragments.is_empty() && packet.first() != Some(&S2C_AUDIO_FRAME) {
                return Err(Error::InvalidFragment);
            }
            return Ok(Some(packet));
        }
    }

    fn recv_frame(&mut self) -> Result<Option<Vec<u8>>, Error> {
        loop {
            match host::recv(&mut self.buffer)? {
                host::RecvOutcome::Closed => return Ok(None),
                host::RecvOutcome::NeedsCapacity(required) => {
                    self.buffer
                        .try_reserve_exact(required.saturating_sub(self.buffer.len()))
                        .map_err(|_| Error::AllocationFailed)?;
                    self.buffer.resize(required, 0);
                }
                host::RecvOutcome::Copied(len) => return Ok(Some(self.buffer[..len].to_vec())),
            }
        }
    }

    fn push_fragment(&mut self, packet: &[u8]) -> Result<Option<Vec<u8>>, Error> {
        if packet.len() < 3 || packet[1] & !FRAGMENT_FLAG_LAST != 0 {
            return Err(Error::InvalidFragment);
        }
        self.fragment_count = self
            .fragment_count
            .checked_add(1)
            .ok_or(Error::TooManyFragments)?;
        if self.fragment_count > MAX_FRAGMENT_COUNT {
            return Err(Error::TooManyFragments);
        }
        let chunk = &packet[2..];
        let total = self
            .fragments
            .len()
            .checked_add(chunk.len())
            .ok_or(Error::FragmentTooLarge)?;
        if total > MAX_LOGICAL_MESSAGE {
            return Err(Error::FragmentTooLarge);
        }
        self.fragments
            .try_reserve_exact(chunk.len())
            .map_err(|_| Error::AllocationFailed)?;
        self.fragments.extend_from_slice(chunk);
        if packet[1] & FRAGMENT_FLAG_LAST == 0 {
            return Ok(None);
        }
        self.fragment_count = 0;
        let complete = mem::take(&mut self.fragments);
        if complete.is_empty() {
            return Err(Error::InvalidFragment);
        }
        Ok(Some(complete))
    }
}
