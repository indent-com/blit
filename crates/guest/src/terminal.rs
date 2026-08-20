//! Typed terminal subscriptions with connection-global ACK housekeeping.
//!
//! [`C2S_ACK`](blit_remote::C2S_ACK) retires the oldest terminal frame for
//! the whole connection, not for one PTY. Consequently one
//! [`TerminalSubscriptions`] value must own every typed terminal subscription
//! on a client. It presents at most one logical update at a time and sends its
//! ACK only when that update is explicitly applied or discarded.

use alloc::{string::String, vec::Vec};
use core::fmt;

use blit_remote::{
    Create2Request, FEATURE_CREATE_EXEC, FEATURE_CREATE_NO_SUBSCRIBE, FEATURE_CREATE_STATUS,
    FEATURE_PTY_DEADLINE, S2C_UPDATE, ServerMsg, TerminalState, msg_ack, msg_create2_request,
    msg_subscribe, msg_unsubscribe, parse_server_msg,
};

use crate::{Client, Error as ClientError};

/// Parameters for a correlated `CREATE2` request.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CreateRequest<'a> {
    pub rows: u16,
    pub cols: u16,
    pub tag: &'a str,
    /// Run this through the server's login shell. Leave empty when using
    /// `argv`; setting both is [`Error::CreateFailed`] with `INVALID`.
    pub command: &'a str,
    /// Exec this argv directly, no shell. Needs
    /// [`FEATURE_CREATE_EXEC`](blit_remote::FEATURE_CREATE_EXEC).
    pub argv: Option<&'a [&'a str]>,
    pub cwd: Option<&'a str>,
    /// Environment overrides, applied on top of everything the server derives.
    /// Needs [`FEATURE_CREATE_EXEC`](blit_remote::FEATURE_CREATE_EXEC).
    pub env: &'a [(&'a str, &'a str)],
    pub deadline_ms: Option<u32>,
    /// Whether the server should automatically subscribe this client to the
    /// new terminal's frame stream. Defaults to `true` in the constructors.
    pub subscribe: bool,
}

impl<'a> CreateRequest<'a> {
    /// A shell using the server's default command, tag, cwd, and lifetime.
    pub const fn shell(rows: u16, cols: u16) -> Self {
        Self {
            rows,
            cols,
            tag: "",
            command: "",
            argv: None,
            cwd: None,
            env: &[],
            deadline_ms: None,
            subscribe: true,
        }
    }

    /// Exec `argv` directly, the way a process is started outside a terminal.
    pub const fn exec(rows: u16, cols: u16, argv: &'a [&'a str]) -> Self {
        Self {
            argv: Some(argv),
            ..Self::shell(rows, cols)
        }
    }
}

/// State held for one subscribed PTY.
#[derive(Clone, Debug)]
pub struct Subscription {
    pty_id: u16,
    state: TerminalState,
    synchronized: bool,
    needs_resubscribe: bool,
}

impl Subscription {
    pub const fn pty_id(&self) -> u16 {
        self.pty_id
    }

    /// The latest fully applied terminal state.
    pub const fn state(&self) -> &TerminalState {
        &self.state
    }

    /// Whether a full baseline has been successfully applied since the most
    /// recent subscribe or discarded update.
    pub const fn is_synchronized(&self) -> bool {
        self.synchronized
    }
}

/// Opaque permission to consume the one currently pending logical update.
///
/// Dropping this value does not ACK anything. Use
/// [`discard_pending`](TerminalSubscriptions::discard_pending) to recover if
/// application code abandons a token.
#[derive(Debug)]
#[must_use = "a terminal update must be applied or deliberately discarded"]
pub struct Update {
    sequence: u64,
    pty_id: Option<u16>,
    compressed_len: usize,
}

impl Update {
    /// `None` means the UPDATE packet was too short to carry a PTY ID.
    pub const fn pty_id(&self) -> Option<u16> {
        self.pty_id
    }

    pub const fn compressed_len(&self) -> usize {
        self.compressed_len
    }
}

/// Why a logical update was deliberately discarded.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DiscardReason {
    Application,
    MalformedPacket,
    UnknownSubscription,
    RejectedPayload,
}

/// Result of consuming one logical UPDATE.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum UpdateOutcome {
    Applied {
        pty_id: u16,
    },
    Discarded {
        pty_id: Option<u16>,
        reason: DiscardReason,
    },
}

/// Typed terminal SDK failure.
#[derive(Debug)]
pub enum Error {
    Client(ClientError),
    FeatureMissing(&'static str),
    CreateFailed { status: u8, detail: String },
    InvalidCreateReply,
    UpdatePending,
    StaleUpdate,
    SequenceOverflow,
}

impl fmt::Display for Error {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Client(error) => write!(formatter, "guest client error: {error}"),
            Self::FeatureMissing(feature) => {
                write!(formatter, "server HELLO did not advertise {feature}")
            }
            Self::CreateFailed { status, detail } => {
                write!(formatter, "terminal creation failed with status {status}")?;
                if !detail.is_empty() {
                    write!(formatter, ": {detail}")?;
                }
                Ok(())
            }
            Self::InvalidCreateReply => formatter.write_str("invalid terminal creation reply"),
            Self::UpdatePending => {
                formatter.write_str("the previous terminal update is still pending")
            }
            Self::StaleUpdate => formatter.write_str("terminal update token is stale"),
            Self::SequenceOverflow => formatter.write_str("terminal update sequence overflow"),
        }
    }
}

#[cfg(not(target_arch = "wasm32"))]
impl std::error::Error for Error {}

impl From<ClientError> for Error {
    fn from(value: ClientError) -> Self {
        Self::Client(value)
    }
}

struct Pending {
    sequence: u64,
    packet: Vec<u8>,
    pty_id: Option<u16>,
}

/// Connection-global typed terminal subscription state.
///
/// Keep one instance per [`Client`] and route every terminal UPDATE through
/// it. Unrelated packets remain in `Client`'s bounded pending queue for its
/// other typed wrappers or the raw receive API.
pub struct TerminalSubscriptions {
    subscriptions: Vec<Subscription>,
    pending: Option<Pending>,
    next_sequence: u64,
    next_create_nonce: u16,
}

impl Default for TerminalSubscriptions {
    fn default() -> Self {
        Self::new()
    }
}

impl TerminalSubscriptions {
    pub const fn new() -> Self {
        Self {
            subscriptions: Vec::new(),
            pending: None,
            next_sequence: 1,
            next_create_nonce: 1,
        }
    }

    pub fn subscriptions(&self) -> &[Subscription] {
        &self.subscriptions
    }

    pub fn subscription(&self, pty_id: u16) -> Option<&Subscription> {
        self.subscriptions
            .iter()
            .find(|subscription| subscription.pty_id == pty_id)
    }

    /// Create a terminal with a correlated success-or-failure reply.
    pub fn create(
        &mut self,
        client: &mut Client,
        request: CreateRequest<'_>,
    ) -> Result<u16, Error> {
        let features = client.context().hello.features;
        if features & FEATURE_CREATE_STATUS == 0 {
            return Err(Error::FeatureMissing("FEATURE_CREATE_STATUS"));
        }
        if request.deadline_ms.is_some() && features & FEATURE_PTY_DEADLINE == 0 {
            return Err(Error::FeatureMissing("FEATURE_PTY_DEADLINE"));
        }
        if !request.subscribe && features & FEATURE_CREATE_NO_SUBSCRIBE == 0 {
            return Err(Error::FeatureMissing("FEATURE_CREATE_NO_SUBSCRIBE"));
        }
        // Neither exec field is probeable: an older server ignores the flag and
        // reads the block as command bytes, or spawns a plain shell. Refuse
        // here rather than let it run something else.
        if (request.argv.is_some() || !request.env.is_empty())
            && features & FEATURE_CREATE_EXEC == 0
        {
            return Err(Error::FeatureMissing("FEATURE_CREATE_EXEC"));
        }

        let nonce = self.allocate_create_nonce();
        let packet = msg_create2_request(&Create2Request {
            nonce,
            rows: request.rows,
            cols: request.cols,
            want_status: true,
            no_subscribe: !request.subscribe,
            tag: request.tag,
            cwd: request.cwd,
            deadline_ms: request.deadline_ms,
            env: request.env.to_vec(),
            argv: request.argv.map(<[&str]>::to_vec),
            command: (!request.command.is_empty()).then_some(request.command),
            ..Default::default()
        })
        .map_err(|err| Error::CreateFailed {
            status: err.status,
            detail: String::from(err.detail),
        })?;
        client.send(&packet)?;
        let reply = client
            .recv_matching(|packet| creation_reply(packet, nonce))?
            .ok_or(ClientError::EndpointClosed)?;
        match parse_server_msg(&reply) {
            Some(ServerMsg::CreatedN {
                nonce: reply_nonce,
                pty_id,
                ..
            }) if reply_nonce == nonce => Ok(pty_id),
            Some(ServerMsg::CreateFailed {
                nonce: reply_nonce,
                status,
                detail,
            }) if reply_nonce == nonce => Err(Error::CreateFailed {
                status,
                detail: String::from(detail),
            }),
            _ => Err(Error::InvalidCreateReply),
        }
    }

    /// Create and immediately start a typed subscription.
    pub fn create_and_subscribe(
        &mut self,
        client: &mut Client,
        request: CreateRequest<'_>,
    ) -> Result<u16, Error> {
        let pty_id = self.create(client, request)?;
        self.subscribe(client, pty_id, request.rows, request.cols)?;
        Ok(pty_id)
    }

    /// Subscribe, or repeat a subscription to request a fresh keyframe.
    ///
    /// This is rejected while an UPDATE awaits consumption so the global ACK
    /// order cannot be disturbed.
    pub fn subscribe(
        &mut self,
        client: &mut Client,
        pty_id: u16,
        rows: u16,
        cols: u16,
    ) -> Result<(), Error> {
        self.require_no_pending()?;
        client.send(&msg_subscribe(pty_id))?;
        let replacement = Subscription {
            pty_id,
            state: TerminalState::new(rows, cols),
            synchronized: false,
            needs_resubscribe: false,
        };
        if let Some(index) = self.index_of(pty_id) {
            self.subscriptions[index] = replacement;
        } else {
            self.subscriptions.push(replacement);
        }
        Ok(())
    }

    /// Stop typed updates for one PTY.
    pub fn unsubscribe(&mut self, client: &mut Client, pty_id: u16) -> Result<(), Error> {
        self.require_no_pending()?;
        client.send(&msg_unsubscribe(pty_id))?;
        if let Some(index) = self.index_of(pty_id) {
            self.subscriptions.remove(index);
        }
        Ok(())
    }

    /// Receive the oldest logical terminal UPDATE without acknowledging it.
    pub fn next_update(&mut self, client: &mut Client) -> Result<Update, Error> {
        self.require_no_pending()?;
        self.flush_resubscriptions(client)?;
        let sequence = self.next_sequence;
        let next_sequence = sequence.checked_add(1).ok_or(Error::SequenceOverflow)?;
        let packet = client
            .recv_matching(|packet| packet.first() == Some(&S2C_UPDATE))?
            .ok_or(ClientError::EndpointClosed)?;
        self.next_sequence = next_sequence;
        let pty_id = packet
            .get(1..3)
            .map(|bytes| u16::from_le_bytes([bytes[0], bytes[1]]));
        let compressed_len = packet.len().saturating_sub(3);
        self.pending = Some(Pending {
            sequence,
            packet,
            pty_id,
        });
        Ok(Update {
            sequence,
            pty_id,
            compressed_len,
        })
    }

    /// Apply an UPDATE to its [`TerminalState`], then send exactly one ACK.
    ///
    /// A malformed packet, an UPDATE for an unknown subscription, or a
    /// payload rejected by `TerminalState` is deliberately discarded instead.
    /// A rejected known payload also repeats `SUBSCRIBE`, forcing the next
    /// update to be a full keyframe.
    pub fn apply_update(
        &mut self,
        client: &mut Client,
        update: Update,
    ) -> Result<UpdateOutcome, Error> {
        let pending = self.take_pending(&update)?;
        let Some(pty_id) = pending.pty_id else {
            return self.finish_discard(client, pending, None, DiscardReason::MalformedPacket);
        };
        let Some(index) = self.index_of(pty_id) else {
            return self.finish_discard(
                client,
                pending,
                Some(pty_id),
                DiscardReason::UnknownSubscription,
            );
        };
        let previous = self.subscriptions[index].state.clone();
        let applied = self.subscriptions[index]
            .state
            .feed_compressed(&pending.packet[3..]);
        if !applied {
            self.subscriptions[index].state = previous;
            return self.finish_discard(
                client,
                pending,
                Some(pty_id),
                DiscardReason::RejectedPayload,
            );
        }

        if let Err(error) = client.send(&msg_ack()) {
            self.subscriptions[index].state = previous;
            self.pending = Some(pending);
            return Err(error.into());
        }
        self.subscriptions[index].synchronized = true;
        Ok(UpdateOutcome::Applied { pty_id })
    }

    /// Deliberately discard the current UPDATE, ACK it, and request a fresh
    /// baseline when it belonged to a known subscription.
    pub fn discard_update(
        &mut self,
        client: &mut Client,
        update: Update,
    ) -> Result<UpdateOutcome, Error> {
        let pending = self.take_pending(&update)?;
        let pty_id = pending.pty_id;
        self.finish_discard(client, pending, pty_id, DiscardReason::Application)
    }

    /// Recover after the application dropped its opaque [`Update`] token.
    pub fn discard_pending(&mut self, client: &mut Client) -> Result<UpdateOutcome, Error> {
        let pending = self.pending.take().ok_or(Error::StaleUpdate)?;
        let pty_id = pending.pty_id;
        self.finish_discard(client, pending, pty_id, DiscardReason::Application)
    }

    fn finish_discard(
        &mut self,
        client: &mut Client,
        pending: Pending,
        pty_id: Option<u16>,
        reason: DiscardReason,
    ) -> Result<UpdateOutcome, Error> {
        if let Err(error) = client.send(&msg_ack()) {
            self.pending = Some(pending);
            return Err(error.into());
        }
        if let Some(pty_id) = pty_id
            && let Some(index) = self.index_of(pty_id)
        {
            self.subscriptions[index].synchronized = false;
            self.subscriptions[index].needs_resubscribe = true;
            self.flush_resubscription(client, index)?;
        }
        Ok(UpdateOutcome::Discarded { pty_id, reason })
    }

    fn take_pending(&mut self, update: &Update) -> Result<Pending, Error> {
        if self
            .pending
            .as_ref()
            .is_none_or(|pending| pending.sequence != update.sequence)
        {
            return Err(Error::StaleUpdate);
        }
        Ok(self.pending.take().expect("checked pending update"))
    }

    fn flush_resubscriptions(&mut self, client: &mut Client) -> Result<(), Error> {
        for index in 0..self.subscriptions.len() {
            self.flush_resubscription(client, index)?;
        }
        Ok(())
    }

    fn flush_resubscription(&mut self, client: &mut Client, index: usize) -> Result<(), Error> {
        if self.subscriptions[index].needs_resubscribe {
            client.send(&msg_subscribe(self.subscriptions[index].pty_id))?;
            self.subscriptions[index].needs_resubscribe = false;
        }
        Ok(())
    }

    fn require_no_pending(&self) -> Result<(), Error> {
        if self.pending.is_some() {
            Err(Error::UpdatePending)
        } else {
            Ok(())
        }
    }

    fn index_of(&self, pty_id: u16) -> Option<usize> {
        self.subscriptions
            .iter()
            .position(|subscription| subscription.pty_id == pty_id)
    }

    fn allocate_create_nonce(&mut self) -> u16 {
        let nonce = self.next_create_nonce;
        self.next_create_nonce = self.next_create_nonce.wrapping_add(1);
        if self.next_create_nonce == 0 {
            self.next_create_nonce = 1;
        }
        nonce
    }
}

impl Client {
    /// Start connection-global typed terminal subscription bookkeeping.
    pub const fn terminal_subscriptions(&self) -> TerminalSubscriptions {
        TerminalSubscriptions::new()
    }
}

fn creation_reply(packet: &[u8], expected_nonce: u16) -> bool {
    matches!(
        parse_server_msg(packet),
        Some(ServerMsg::CreatedN { nonce, .. } | ServerMsg::CreateFailed { nonce, .. })
            if nonce == expected_nonce
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{bootstrap, native_host};
    use alloc::{collections::VecDeque, rc::Rc, vec, vec::Vec};
    use core::cell::RefCell;

    use blit_remote::{
        C2S_ACK, C2S_CREATE2, C2S_SUBSCRIBE, FrameState, S2C_CREATED_N, build_update_msg,
    };

    #[derive(Default)]
    struct State {
        incoming: VecDeque<Vec<u8>>,
        sent: Vec<Vec<u8>>,
    }

    struct MockHost(Rc<RefCell<State>>);

    impl native_host::Host for MockHost {
        fn send(&mut self, packet: &[u8]) -> i32 {
            self.0.borrow_mut().sent.push(packet.to_vec());
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

        fn wait(&mut self, _: i64) -> i32 {
            if self.0.borrow().incoming.is_empty() {
                2
            } else {
                1
            }
        }

        fn clock(&mut self, _: i32) -> i64 {
            0
        }

        fn random(&mut self, destination: &mut [u8]) {
            destination.fill(7);
        }
    }

    const ALL_FEATURES: u32 = bootstrap::FEATURE_EXTENSION
        | FEATURE_CREATE_STATUS
        | FEATURE_PTY_DEADLINE
        | FEATURE_CREATE_EXEC
        | FEATURE_CREATE_NO_SUBSCRIBE;

    fn hello_with(features: u32) -> Vec<u8> {
        let mut packet = vec![bootstrap::S2C_HELLO];
        packet.extend_from_slice(&1u16.to_le_bytes());
        packet.extend_from_slice(&features.to_le_bytes());
        packet
    }

    fn init() -> Vec<u8> {
        let mut packet = vec![bootstrap::EXT_INFO, bootstrap::EXT_INFO_INIT];
        packet.extend_from_slice(&7u64.to_le_bytes());
        packet.extend_from_slice(&9u64.to_le_bytes());
        packet.extend_from_slice(&11u64.to_le_bytes());
        packet.extend_from_slice(&13u32.to_le_bytes());
        packet.push(0b1110);
        packet.extend_from_slice(&[42; 32]);
        packet.extend_from_slice(&4u16.to_le_bytes());
        packet.extend_from_slice(b"demo");
        packet.extend_from_slice(&0u16.to_le_bytes());
        packet
    }

    fn boot() -> (native_host::Guard, Rc<RefCell<State>>, Client) {
        boot_features(ALL_FEATURES)
    }

    /// Boot against a server that does not advertise `missing`.
    fn boot_without(missing: u32) -> (native_host::Guard, Rc<RefCell<State>>, Client) {
        boot_features(ALL_FEATURES & !missing)
    }

    fn boot_features(features: u32) -> (native_host::Guard, Rc<RefCell<State>>, Client) {
        let state = Rc::new(RefCell::new(State::default()));
        state.borrow_mut().incoming.extend([
            hello_with(features),
            vec![bootstrap::S2C_READY],
            init(),
        ]);
        let guard = native_host::install(MockHost(Rc::clone(&state)));
        let client = Client::bootstrap().expect("valid extension bootstrap");
        (guard, state, client)
    }

    fn baseline(pty_id: u16, title: &str) -> Vec<u8> {
        let mut frame = FrameState::new(2, 4);
        frame.set_title(title);
        build_update_msg(pty_id, &frame, &FrameState::default()).expect("baseline update")
    }

    fn ack_count(state: &State) -> usize {
        state
            .sent
            .iter()
            .filter(|packet| packet.as_slice() == [C2S_ACK])
            .count()
    }

    #[test]
    fn update_is_not_acked_until_application_then_exactly_once() {
        let (_guard, state, mut client) = boot();
        let mut terminals = client.terminal_subscriptions();
        terminals.subscribe(&mut client, 4, 2, 4).unwrap();
        state.borrow_mut().incoming.push_back(baseline(4, "ready"));

        let update = terminals.next_update(&mut client).unwrap();
        assert_eq!(update.pty_id(), Some(4));
        assert_eq!(ack_count(&state.borrow()), 0);
        assert!(matches!(
            terminals.next_update(&mut client),
            Err(Error::UpdatePending)
        ));
        assert_eq!(ack_count(&state.borrow()), 0);

        assert_eq!(
            terminals.apply_update(&mut client, update).unwrap(),
            UpdateOutcome::Applied { pty_id: 4 }
        );
        assert_eq!(ack_count(&state.borrow()), 1);
        let subscription = terminals.subscription(4).unwrap();
        assert!(subscription.is_synchronized());
        assert_eq!(subscription.state().title(), "ready");
    }

    #[test]
    fn updates_are_consumed_in_global_fifo_order_across_subscriptions() {
        let (_guard, state, mut client) = boot();
        let mut terminals = TerminalSubscriptions::new();
        terminals.subscribe(&mut client, 4, 2, 4).unwrap();
        terminals.subscribe(&mut client, 9, 2, 4).unwrap();
        state
            .borrow_mut()
            .incoming
            .extend([baseline(4, "first"), baseline(9, "second")]);

        let first = terminals.next_update(&mut client).unwrap();
        assert_eq!(first.pty_id(), Some(4));
        terminals.apply_update(&mut client, first).unwrap();
        assert_eq!(ack_count(&state.borrow()), 1);

        let second = terminals.next_update(&mut client).unwrap();
        assert_eq!(second.pty_id(), Some(9));
        assert_eq!(ack_count(&state.borrow()), 1);
        assert_eq!(
            terminals.discard_update(&mut client, second).unwrap(),
            UpdateOutcome::Discarded {
                pty_id: Some(9),
                reason: DiscardReason::Application,
            }
        );
        let sent = &state.borrow().sent;
        assert_eq!(ack_count(&state.borrow()), 2);
        assert_eq!(sent[sent.len() - 2], vec![C2S_ACK]);
        assert_eq!(sent[sent.len() - 1], msg_subscribe(9));
        assert!(!terminals.subscription(9).unwrap().is_synchronized());
    }

    #[test]
    fn malformed_and_unknown_updates_are_safely_discarded() {
        let (_guard, state, mut client) = boot();
        let mut terminals = TerminalSubscriptions::new();
        terminals.subscribe(&mut client, 4, 2, 4).unwrap();
        state.borrow_mut().incoming.extend([
            vec![0xfe, 1, 2],
            vec![S2C_UPDATE],
            baseline(77, "unknown"),
            vec![S2C_UPDATE, 4, 0, 1, 2, 3],
        ]);

        let malformed = terminals.next_update(&mut client).unwrap();
        assert_eq!(malformed.pty_id(), None);
        assert_eq!(
            terminals.apply_update(&mut client, malformed).unwrap(),
            UpdateOutcome::Discarded {
                pty_id: None,
                reason: DiscardReason::MalformedPacket,
            }
        );

        let unknown = terminals.next_update(&mut client).unwrap();
        assert_eq!(
            terminals.apply_update(&mut client, unknown).unwrap(),
            UpdateOutcome::Discarded {
                pty_id: Some(77),
                reason: DiscardReason::UnknownSubscription,
            }
        );

        let rejected = terminals.next_update(&mut client).unwrap();
        assert_eq!(
            terminals.apply_update(&mut client, rejected).unwrap(),
            UpdateOutcome::Discarded {
                pty_id: Some(4),
                reason: DiscardReason::RejectedPayload,
            }
        );
        assert_eq!(ack_count(&state.borrow()), 3);
        assert_eq!(client.recv().unwrap(), Some(vec![0xfe, 1, 2]));
        assert!(!terminals.subscription(4).unwrap().is_synchronized());
        assert_eq!(state.borrow().sent.last(), Some(&msg_subscribe(4)));
    }

    #[test]
    fn correlated_create_can_subscribe_without_exposing_manual_ack() {
        let (_guard, state, mut client) = boot();
        state
            .borrow_mut()
            .incoming
            .push_back(vec![S2C_CREATED_N, 1, 0, 33, 0]);
        let mut terminals = TerminalSubscriptions::new();
        let request = CreateRequest {
            tag: "worker",
            command: "cargo test",
            cwd: Some("/work"),
            deadline_ms: Some(5_000),
            ..CreateRequest::shell(24, 80)
        };

        assert_eq!(
            terminals
                .create_and_subscribe(&mut client, request)
                .unwrap(),
            33
        );
        let sent = &state.borrow().sent;
        assert_eq!(sent[0][0], C2S_CREATE2);
        assert_ne!(sent[0][7] & blit_remote::CREATE2_WANT_STATUS, 0);
        assert_eq!(sent[1], vec![C2S_SUBSCRIBE, 33, 0]);
        assert_eq!(ack_count(&state.borrow()), 0);
    }

    #[test]
    fn correlated_create_can_skip_the_automatic_subscription() {
        let (_guard, state, mut client) = boot();
        state
            .borrow_mut()
            .incoming
            .push_back(vec![S2C_CREATED_N, 1, 0, 33, 0]);
        let mut terminals = TerminalSubscriptions::new();
        let request = CreateRequest {
            subscribe: false,
            ..CreateRequest::shell(24, 80)
        };

        assert_eq!(terminals.create(&mut client, request).unwrap(), 33);
        let sent = &state.borrow().sent;
        let parsed = blit_remote::parse_create2(&sent[0]).unwrap();
        assert!(parsed.no_subscribe);
        assert_eq!(sent.len(), 1);
    }

    #[test]
    fn no_subscribe_create_needs_the_feature_bit() {
        let (_guard, state, mut client) = boot_without(FEATURE_CREATE_NO_SUBSCRIBE);
        let mut terminals = TerminalSubscriptions::new();
        let request = CreateRequest {
            subscribe: false,
            ..CreateRequest::shell(24, 80)
        };

        assert!(matches!(
            terminals.create(&mut client, request),
            Err(Error::FeatureMissing("FEATURE_CREATE_NO_SUBSCRIBE"))
        ));
        assert!(state.borrow().sent.is_empty());
    }

    #[test]
    fn an_exec_request_carries_argv_and_environment() {
        let (_guard, state, mut client) = boot();
        state
            .borrow_mut()
            .incoming
            .push_back(vec![S2C_CREATED_N, 1, 0, 7, 0]);
        let mut terminals = TerminalSubscriptions::new();
        let request = CreateRequest {
            cwd: Some("/work"),
            env: &[("RUST_LOG", "debug")],
            ..CreateRequest::exec(24, 80, &["cargo", "test", "--release"])
        };

        assert_eq!(terminals.create(&mut client, request).unwrap(), 7);
        let sent = &state.borrow().sent;
        let parsed = blit_remote::parse_create2(&sent[0]).unwrap();
        assert_eq!(
            parsed.argv.as_deref(),
            Some(&["cargo", "test", "--release"][..])
        );
        assert_eq!(parsed.env, vec![("RUST_LOG", "debug")]);
        assert_eq!(parsed.cwd, Some("/work"));
        assert_eq!(parsed.command, None);
    }

    /// An older server ignores the bit and spawns a plain shell, so asking for
    /// an exec it cannot do has to fail rather than silently become something
    /// else.
    #[test]
    fn an_exec_request_needs_the_feature_bit() {
        let (_guard, _state, mut client) = boot_without(FEATURE_CREATE_EXEC);
        let mut terminals = TerminalSubscriptions::new();
        let mut missing = |request| {
            matches!(
                terminals.create(&mut client, request),
                Err(Error::FeatureMissing("FEATURE_CREATE_EXEC"))
            )
        };
        assert!(missing(CreateRequest::exec(24, 80, &["htop"])));
        assert!(missing(CreateRequest {
            env: &[("FOO", "bar")],
            ..CreateRequest::shell(24, 80)
        }));
    }
}
