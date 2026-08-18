//! Typed native-channel handles for extension guests.

use alloc::{collections::VecDeque, string::String, vec::Vec};
use core::{fmt, ops::Deref};

use blit_remote::{STATUS_OK, channel as wire};

use crate::{Client, Error as ClientError};

/// A reason an extension may put in a client-to-server channel close.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum CloseReason {
    Normal = wire::CHANNEL_CLOSE_NORMAL,
    Cancelled = wire::CHANNEL_CLOSE_CANCELLED,
}

/// Terminal channel state reported by the server.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Closed {
    pub reason: u8,
    pub detail: String,
}

/// One unacknowledged peer message.
///
/// Inspect the payload, then pass this receipt to [`Channel::consume`] or
/// [`Channel::discard`]. Dropping it sends no ACK; use
/// [`Channel::discard_pending`] to recover after abandoning a receipt.
#[derive(Debug, Eq, PartialEq)]
#[must_use = "channel data must be consumed or deliberately discarded"]
pub struct Delivery {
    channel_id: u32,
    through: u64,
    payload: Vec<u8>,
}

impl Delivery {
    /// The complete message payload, still awaiting application consumption.
    pub fn payload(&self) -> &[u8] {
        &self.payload
    }
}

impl AsRef<[u8]> for Delivery {
    fn as_ref(&self) -> &[u8] {
        self.payload()
    }
}

impl Deref for Delivery {
    type Target = [u8];

    fn deref(&self) -> &Self::Target {
        self.payload()
    }
}

/// A connected-channel receive event.
#[derive(Debug, Eq, PartialEq)]
pub enum Event {
    /// One complete peer message which has not yet been acknowledged.
    Data(Delivery),
    /// Peer consumption released sender credit.
    Acknowledged { bytes: u64, available: u64 },
    /// The handle reached its final state.
    Closed(Closed),
}

/// A listener event.
#[derive(Debug)]
pub enum ListenerEvent {
    Accepted(Channel),
    Closed(Closed),
}

/// A typed native-channel failure.
#[derive(Debug)]
pub enum Error {
    Client(ClientError),
    FeatureMissing,
    InvalidOpen,
    Decode(wire::ChannelDecodeError),
    OpenFailed { status: u8, detail: String },
    InvalidSuccess,
    InvalidPayload,
    CreditExhausted { required: u64, available: u64 },
    MessageLimit,
    CounterOverflow,
    DeliveryPending,
    StaleDelivery,
    Protocol(&'static str),
    Closed,
}

impl fmt::Display for Error {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Client(error) => write!(formatter, "guest client error: {error}"),
            Self::FeatureMissing => f_write(
                formatter,
                "server HELLO did not advertise native-channel support",
            ),
            Self::InvalidOpen => f_write(formatter, "invalid native-channel open parameters"),
            Self::Decode(error) => write!(formatter, "invalid native-channel packet: {error}"),
            Self::OpenFailed { status, detail } => {
                write!(formatter, "native-channel open failed with status {status}")?;
                if !detail.is_empty() {
                    write!(formatter, ": {detail}")?;
                }
                Ok(())
            }
            Self::InvalidSuccess => {
                f_write(formatter, "server returned a non-canonical channel success")
            }
            Self::InvalidPayload => f_write(formatter, "channel payload is empty or too large"),
            Self::CreditExhausted {
                required,
                available,
            } => write!(
                formatter,
                "channel needs {required} bytes of credit but only {available} are available"
            ),
            Self::MessageLimit => f_write(formatter, "channel has 1,024 unacknowledged messages"),
            Self::CounterOverflow => f_write(formatter, "channel byte counter overflow"),
            Self::DeliveryPending => {
                f_write(formatter, "the previous channel delivery is still pending")
            }
            Self::StaleDelivery => f_write(formatter, "channel delivery receipt is stale"),
            Self::Protocol(detail) => write!(formatter, "channel protocol error: {detail}"),
            Self::Closed => f_write(formatter, "channel is closing or closed"),
        }
    }
}

fn f_write(formatter: &mut fmt::Formatter<'_>, text: &str) -> fmt::Result {
    formatter.write_str(text)
}

#[cfg(not(target_arch = "wasm32"))]
impl std::error::Error for Error {}

impl From<ClientError> for Error {
    fn from(value: ClientError) -> Self {
        Self::Client(value)
    }
}

impl From<wire::ChannelDecodeError> for Error {
    fn from(value: wire::ChannelDecodeError) -> Self {
        Self::Decode(value)
    }
}

/// A process-global named channel listener owned by this attempt.
#[derive(Debug)]
pub struct Listener {
    id: u32,
    name: String,
    closing: bool,
    closed: bool,
}

impl Listener {
    pub const fn id(&self) -> u32 {
        self.id
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    /// Wait for one accepted connection or the listener's final close.
    pub fn accept(&mut self, client: &mut Client) -> Result<ListenerEvent, Error> {
        if self.closed {
            return Err(Error::Closed);
        }
        let packet = client
            .recv_matching(|packet| listener_packet(packet, self.id))?
            .ok_or(ClientError::EndpointClosed)?;
        self.interpret(&packet)
    }

    /// Interpret a packet the caller already read, without blocking.
    ///
    /// `Ok(None)` means the packet is not this listener's, so the caller should
    /// route it elsewhere. An extension that also has to watch timers or other
    /// families cannot use [`accept`](Self::accept): it blocks until *its*
    /// packet arrives while everything else queues behind it, so a backoff timer
    /// never fires and a process exit is never seen. Such a caller owns the
    /// receive loop (`Client::wait_until` then `Client::recv`) and routes each
    /// packet here.
    pub fn offer(&mut self, packet: &[u8]) -> Result<Option<ListenerEvent>, Error> {
        if self.closed {
            return Err(Error::Closed);
        }
        if !listener_packet(packet, self.id) {
            return Ok(None);
        }
        self.interpret(packet).map(Some)
    }

    fn interpret(&mut self, packet: &[u8]) -> Result<ListenerEvent, Error> {
        match wire::parse_channel_message(packet)? {
            Some(wire::ChannelMessage::Accepted {
                channel_id,
                listener_id,
                window,
                peer,
                metadata,
            }) if listener_id == self.id => {
                if channel_id & 1 == 0 || window == 0 || peer.is_empty() {
                    return Err(Error::InvalidSuccess);
                }
                Ok(ListenerEvent::Accepted(Channel::new(
                    channel_id, window, peer, metadata,
                )))
            }
            Some(wire::ChannelMessage::Closed {
                channel_id,
                reason,
                detail,
            }) if channel_id == self.id => {
                self.closed = true;
                Ok(ListenerEvent::Closed(Closed {
                    reason,
                    detail: String::from(detail),
                }))
            }
            _ => Err(Error::Protocol("unexpected listener packet")),
        }
    }

    /// Close the listener. Existing accepted channels remain connected.
    pub fn close(&mut self, client: &mut Client, reason: CloseReason) -> Result<(), Error> {
        if self.closing || self.closed {
            return Ok(());
        }
        let packet = wire::msg_channel_close(self.id, reason as u8)
            .ok_or(Error::Protocol("invalid listener close"))?;
        client.send(&packet)?;
        self.closing = true;
        Ok(())
    }
}

/// One full-duplex, message-preserving native channel.
#[derive(Debug)]
pub struct Channel {
    id: u32,
    window: u64,
    peer: String,
    metadata: Vec<u8>,
    sent: u64,
    acknowledged: u64,
    sent_boundaries: VecDeque<u64>,
    received: u64,
    pending_delivery: Option<PendingDelivery>,
    closing: bool,
    closed: bool,
}

impl Channel {
    fn new(id: u32, window: u64, peer: &str, metadata: &[u8]) -> Self {
        Self {
            id,
            window,
            peer: String::from(peer),
            metadata: metadata.to_vec(),
            sent: 0,
            acknowledged: 0,
            sent_boundaries: VecDeque::new(),
            received: 0,
            pending_delivery: None,
            closing: false,
            closed: false,
        }
    }

    pub const fn id(&self) -> u32 {
        self.id
    }

    pub const fn window(&self) -> u64 {
        self.window
    }

    pub fn peer(&self) -> &str {
        &self.peer
    }

    pub fn metadata(&self) -> &[u8] {
        &self.metadata
    }

    pub fn available_credit(&self) -> u64 {
        self.acknowledged
            .saturating_add(self.window)
            .saturating_sub(self.sent)
    }

    pub fn unacknowledged_messages(&self) -> usize {
        self.sent_boundaries.len()
    }

    /// Whether one received DATA message still awaits consumption or discard.
    pub const fn has_pending_delivery(&self) -> bool {
        self.pending_delivery.is_some()
    }

    /// Send one message if both byte and message credit are available.
    pub fn send(&mut self, client: &mut Client, payload: &[u8]) -> Result<(), Error> {
        if self.closing || self.closed {
            return Err(Error::Closed);
        }
        let packet = wire::msg_channel_data(self.id, payload).ok_or(Error::InvalidPayload)?;
        if self.sent_boundaries.len() >= wire::CHANNEL_MAX_UNCONSUMED_MESSAGES {
            return Err(Error::MessageLimit);
        }
        let required = u64::try_from(payload.len()).map_err(|_| Error::InvalidPayload)?;
        let end = self
            .sent
            .checked_add(required)
            .ok_or(Error::CounterOverflow)?;
        let limit = self
            .acknowledged
            .checked_add(self.window)
            .ok_or(Error::CounterOverflow)?;
        if end > limit {
            return Err(Error::CreditExhausted {
                required,
                available: limit.saturating_sub(self.sent),
            });
        }
        client.send(&packet)?;
        self.sent = end;
        self.sent_boundaries.push_back(end);
        Ok(())
    }

    /// Receive channel data, credit, or final closure.
    ///
    /// DATA is returned as an unacknowledged [`Delivery`]. Another receive is
    /// rejected until that receipt is passed to [`consume`](Self::consume) or
    /// [`discard`](Self::discard).
    pub fn receive(&mut self, client: &mut Client) -> Result<Event, Error> {
        if self.closed {
            return Err(Error::Closed);
        }
        if self.pending_delivery.is_some() {
            return Err(Error::DeliveryPending);
        }
        let packet = client
            .recv_matching(|packet| connected_packet(packet, self.id))?
            .ok_or(ClientError::EndpointClosed)?;
        self.interpret(&packet)
    }

    /// Interpret a packet the caller already read, without blocking.
    ///
    /// `Ok(None)` means the packet is not this channel's. See
    /// [`Listener::offer`] for why a supervising extension needs this rather
    /// than [`receive`](Self::receive).
    pub fn offer(&mut self, packet: &[u8]) -> Result<Option<Event>, Error> {
        if self.closed {
            return Err(Error::Closed);
        }
        if self.pending_delivery.is_some() {
            return Err(Error::DeliveryPending);
        }
        if !connected_packet(packet, self.id) {
            return Ok(None);
        }
        self.interpret(packet).map(Some)
    }

    fn interpret(&mut self, packet: &[u8]) -> Result<Event, Error> {
        match wire::parse_channel_message(packet)? {
            Some(wire::ChannelMessage::Data {
                channel_id,
                payload,
            }) if channel_id == self.id => {
                let bytes = u64::try_from(payload.len()).map_err(|_| Error::CounterOverflow)?;
                let through = self
                    .received
                    .checked_add(bytes)
                    .ok_or(Error::CounterOverflow)?;
                self.pending_delivery = Some(PendingDelivery { through });
                Ok(Event::Data(Delivery {
                    channel_id: self.id,
                    through,
                    payload: payload.to_vec(),
                }))
            }
            Some(wire::ChannelMessage::Ack { channel_id, bytes }) if channel_id == self.id => {
                self.apply_ack(bytes)?;
                Ok(Event::Acknowledged {
                    bytes,
                    available: self.available_credit(),
                })
            }
            Some(wire::ChannelMessage::Closed {
                channel_id,
                reason,
                detail,
            }) if channel_id == self.id => {
                self.closed = true;
                Ok(Event::Closed(Closed {
                    reason,
                    detail: String::from(detail),
                }))
            }
            _ => Err(Error::Protocol("unexpected connected-channel packet")),
        }
    }

    /// Consume one DATA delivery and send its cumulative ACK exactly once.
    ///
    /// The payload remains inspectable through [`Delivery::payload`] before
    /// this call and is returned by value after the ACK is accepted.
    pub fn consume(&mut self, client: &mut Client, delivery: Delivery) -> Result<Vec<u8>, Error> {
        self.finish_delivery(client, &delivery)?;
        Ok(delivery.payload)
    }

    /// Deliberately discard one DATA delivery and send its cumulative ACK
    /// exactly once.
    pub fn discard(&mut self, client: &mut Client, delivery: Delivery) -> Result<(), Error> {
        self.finish_delivery(client, &delivery)
    }

    /// Recover after the application dropped a [`Delivery`] receipt.
    pub fn discard_pending(&mut self, client: &mut Client) -> Result<(), Error> {
        let pending = self.pending_delivery.take().ok_or(Error::StaleDelivery)?;
        self.acknowledge_pending(client, pending)
    }

    /// Begin an orderly or cancelled close. Repeated calls are idempotent.
    pub fn close(&mut self, client: &mut Client, reason: CloseReason) -> Result<(), Error> {
        if self.closing || self.closed {
            return Ok(());
        }
        let packet = wire::msg_channel_close(self.id, reason as u8)
            .ok_or(Error::Protocol("invalid channel close"))?;
        client.send(&packet)?;
        self.closing = true;
        Ok(())
    }

    fn apply_ack(&mut self, bytes: u64) -> Result<(), Error> {
        if bytes < self.acknowledged || bytes > self.sent {
            return Err(Error::Protocol("ACK is outside the sent byte range"));
        }
        if bytes != self.acknowledged && !self.sent_boundaries.contains(&bytes) {
            return Err(Error::Protocol("ACK is not on a sent-message boundary"));
        }
        self.acknowledged = bytes;
        while self
            .sent_boundaries
            .front()
            .is_some_and(|boundary| *boundary <= bytes)
        {
            self.sent_boundaries.pop_front();
        }
        Ok(())
    }

    fn finish_delivery(&mut self, client: &mut Client, delivery: &Delivery) -> Result<(), Error> {
        let pending = self.pending_delivery.take().ok_or(Error::StaleDelivery)?;
        if delivery.channel_id != self.id || pending.through != delivery.through {
            self.pending_delivery = Some(pending);
            return Err(Error::StaleDelivery);
        }
        self.acknowledge_pending(client, pending)
    }

    fn acknowledge_pending(
        &mut self,
        client: &mut Client,
        pending: PendingDelivery,
    ) -> Result<(), Error> {
        if let Err(error) = client.send(&wire::msg_channel_ack(self.id, pending.through)) {
            self.pending_delivery = Some(pending);
            return Err(error.into());
        }
        self.received = pending.through;
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct PendingDelivery {
    through: u64,
}

impl Client {
    /// Publish one process-global named listener and wait for its `OPENED`.
    pub fn listen_channel(&mut self, name: &str, metadata: &[u8]) -> Result<Listener, Error> {
        self.require_channels()?;
        let id = self.allocate_channel_id();
        let request = wire::msg_channel_listen(id, name, metadata).ok_or(Error::InvalidOpen)?;
        self.send(&request)?;
        let opened = receive_opened(self, id)?;
        if opened.window != 0 || !opened.peer.is_empty() || !opened.metadata.is_empty() {
            return Err(Error::InvalidSuccess);
        }
        Ok(Listener {
            id,
            name: String::from(name),
            closing: false,
            closed: false,
        })
    }

    /// Connect to a listener, optionally fencing it by discovery token.
    pub fn connect_channel(
        &mut self,
        name: &str,
        metadata: &[u8],
        listener_token: Option<[u8; 16]>,
    ) -> Result<Channel, Error> {
        self.require_channels()?;
        let id = self.allocate_channel_id();
        let request = wire::msg_channel_connect(id, name, metadata, listener_token)
            .ok_or(Error::InvalidOpen)?;
        self.send(&request)?;
        let opened = receive_opened(self, id)?;
        if opened.window == 0 || opened.peer.is_empty() {
            return Err(Error::InvalidSuccess);
        }
        Ok(Channel::new(
            id,
            opened.window,
            &opened.peer,
            &opened.metadata,
        ))
    }

    fn require_channels(&self) -> Result<(), Error> {
        if self.context().hello.features & wire::FEATURE_CHANNEL == 0 {
            Err(Error::FeatureMissing)
        } else {
            Ok(())
        }
    }
}

struct Opened {
    window: u64,
    peer: String,
    metadata: Vec<u8>,
}

fn receive_opened(client: &mut Client, id: u32) -> Result<Opened, Error> {
    let packet = client
        .recv_matching(|packet| {
            matches!(
                wire::channel_header(packet),
                Ok((wire::CHANNEL_OPENED, channel_id, _)) if channel_id == id
            )
        })?
        .ok_or(ClientError::EndpointClosed)?;
    match wire::parse_channel_message(&packet)? {
        Some(wire::ChannelMessage::Opened {
            channel_id,
            status,
            window,
            peer,
            metadata,
            detail,
        }) if channel_id == id => {
            if status != STATUS_OK {
                return Err(Error::OpenFailed {
                    status,
                    detail: String::from(detail),
                });
            }
            Ok(Opened {
                window,
                peer: String::from(peer),
                metadata: metadata.to_vec(),
            })
        }
        _ => Err(Error::Protocol("unexpected OPENED packet")),
    }
}

fn listener_packet(packet: &[u8], listener_id: u32) -> bool {
    match wire::parse_channel_message(packet) {
        Ok(Some(wire::ChannelMessage::Accepted {
            listener_id: id, ..
        })) => id == listener_id,
        Ok(Some(wire::ChannelMessage::Closed { channel_id, .. })) => channel_id == listener_id,
        _ => false,
    }
}

fn connected_packet(packet: &[u8], id: u32) -> bool {
    matches!(
        wire::channel_header(packet),
        Ok((wire::CHANNEL_DATA | wire::CHANNEL_ACK | wire::CHANNEL_CLOSED, channel_id, _))
            if channel_id == id
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::native_host;
    use alloc::{collections::VecDeque, rc::Rc, vec, vec::Vec};
    use core::cell::RefCell;

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
            destination.fill(9);
        }
    }

    fn hello() -> Vec<u8> {
        let mut packet = vec![0x07];
        packet.extend_from_slice(&1u16.to_le_bytes());
        packet.extend_from_slice(
            &(blit_remote::extension::FEATURE_EXTENSION | wire::FEATURE_CHANNEL).to_le_bytes(),
        );
        packet
    }

    fn init() -> Vec<u8> {
        let mut packet = vec![0x92, 1];
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
        let state = Rc::new(RefCell::new(State::default()));
        state
            .borrow_mut()
            .incoming
            .extend([hello(), vec![0x09], init()]);
        let guard = native_host::install(MockHost(Rc::clone(&state)));
        let client = Client::bootstrap().expect("valid extension bootstrap");
        (guard, state, client)
    }

    fn channel_acks(state: &State, expected_channel_id: u32) -> Vec<u64> {
        state
            .sent
            .iter()
            .filter_map(|packet| match wire::parse_channel_request(packet) {
                Ok(Some(wire::ChannelRequest::Ack { channel_id, bytes }))
                    if channel_id == expected_channel_id =>
                {
                    Some(bytes)
                }
                _ => None,
            })
            .collect()
    }

    #[test]
    fn listener_accept_data_ack_credit_and_close_housekeeping() {
        let (_guard, state, mut client) = boot();
        state
            .borrow_mut()
            .incoming
            .push_back(wire::msg_channel_opened(2, STATUS_OK, 0, "", b"", "").unwrap());
        let mut listener = client
            .listen_channel("com.example.commands", b"listener")
            .unwrap();
        assert_eq!(listener.id(), 2);
        assert_eq!(
            wire::parse_channel_request(&state.borrow().sent[0]).unwrap(),
            Some(wire::ChannelRequest::Listen {
                channel_id: 2,
                name: "com.example.commands",
                metadata: b"listener",
            })
        );

        state.borrow_mut().incoming.push_back(
            wire::msg_channel_accepted(3, 2, 5, "client:0000000000000001", b"caller").unwrap(),
        );
        let ListenerEvent::Accepted(mut channel) = listener.accept(&mut client).unwrap() else {
            panic!("expected accepted channel");
        };
        assert_eq!(channel.id(), 3);
        assert_eq!(channel.peer(), "client:0000000000000001");
        assert_eq!(channel.metadata(), b"caller");

        channel.send(&mut client, b"hello").unwrap();
        assert_eq!(channel.available_credit(), 0);
        assert!(matches!(
            channel.send(&mut client, b"x"),
            Err(Error::CreditExhausted {
                required: 1,
                available: 0
            })
        ));

        state
            .borrow_mut()
            .incoming
            .push_back(wire::msg_channel_ack(3, 5));
        assert_eq!(
            channel.receive(&mut client).unwrap(),
            Event::Acknowledged {
                bytes: 5,
                available: 5,
            }
        );
        assert_eq!(channel.unacknowledged_messages(), 0);

        state
            .borrow_mut()
            .incoming
            .push_back(wire::msg_channel_data(3, b"peer").unwrap());
        let Event::Data(delivery) = channel.receive(&mut client).unwrap() else {
            panic!("expected channel DATA");
        };
        assert_eq!(delivery.payload(), b"peer");
        assert!(channel_acks(&state.borrow(), 3).is_empty());
        assert_eq!(channel.consume(&mut client, delivery).unwrap(), b"peer");
        assert_eq!(channel_acks(&state.borrow(), 3), [4]);

        channel.close(&mut client, CloseReason::Normal).unwrap();
        state
            .borrow_mut()
            .incoming
            .push_back(wire::msg_channel_closed(3, wire::CHANNEL_CLOSE_NORMAL, "done").unwrap());
        assert_eq!(
            channel.receive(&mut client).unwrap(),
            Event::Closed(Closed {
                reason: wire::CHANNEL_CLOSE_NORMAL,
                detail: String::from("done"),
            })
        );

        listener.close(&mut client, CloseReason::Normal).unwrap();
        state
            .borrow_mut()
            .incoming
            .push_back(wire::msg_channel_closed(2, wire::CHANNEL_CLOSE_NORMAL, "").unwrap());
        assert!(matches!(
            listener.accept(&mut client).unwrap(),
            ListenerEvent::Closed(Closed {
                reason: wire::CHANNEL_CLOSE_NORMAL,
                ..
            })
        ));
    }

    #[test]
    fn data_ack_waits_for_consumption_and_advances_once_per_delivery() {
        let (_guard, state, mut client) = boot();
        state.borrow_mut().incoming.push_back(
            wire::msg_channel_opened(2, STATUS_OK, 20, "client:0000000000000001", b"", "").unwrap(),
        );
        let mut channel = client.connect_channel("service", b"", None).unwrap();
        state.borrow_mut().incoming.extend([
            wire::msg_channel_data(2, b"abc").unwrap(),
            wire::msg_channel_data(2, b"de").unwrap(),
        ]);

        let Event::Data(first) = channel.receive(&mut client).unwrap() else {
            panic!("expected first channel DATA");
        };
        assert_eq!(first.payload(), b"abc");
        assert!(channel.has_pending_delivery());
        assert!(channel_acks(&state.borrow(), 2).is_empty());
        assert!(matches!(
            channel.receive(&mut client),
            Err(Error::DeliveryPending)
        ));

        assert_eq!(channel.consume(&mut client, first).unwrap(), b"abc");
        assert!(!channel.has_pending_delivery());
        assert_eq!(channel_acks(&state.borrow(), 2), [3]);

        let Event::Data(second) = channel.receive(&mut client).unwrap() else {
            panic!("expected second channel DATA");
        };
        assert_eq!(second.payload(), b"de");
        assert_eq!(channel_acks(&state.borrow(), 2), [3]);
        channel.discard(&mut client, second).unwrap();
        assert_eq!(channel_acks(&state.borrow(), 2), [3, 5]);

        assert!(matches!(
            channel.discard_pending(&mut client),
            Err(Error::StaleDelivery)
        ));
        assert_eq!(channel_acks(&state.borrow(), 2), [3, 5]);
    }

    #[test]
    fn dropped_delivery_can_be_explicitly_discarded() {
        let (_guard, state, mut client) = boot();
        state.borrow_mut().incoming.push_back(
            wire::msg_channel_opened(2, STATUS_OK, 20, "client:0000000000000001", b"", "").unwrap(),
        );
        let mut channel = client.connect_channel("service", b"", None).unwrap();
        state
            .borrow_mut()
            .incoming
            .push_back(wire::msg_channel_data(2, b"abandoned").unwrap());

        let Event::Data(delivery) = channel.receive(&mut client).unwrap() else {
            panic!("expected channel DATA");
        };
        drop(delivery);
        assert!(channel_acks(&state.borrow(), 2).is_empty());
        channel.discard_pending(&mut client).unwrap();
        assert_eq!(channel_acks(&state.borrow(), 2), [9]);
    }

    #[test]
    fn connect_carries_discovery_token_and_listener_metadata() {
        let (_guard, state, mut client) = boot();
        state.borrow_mut().incoming.push_back(
            wire::msg_channel_opened(2, STATUS_OK, 17, "ext:0000000000000002:4", b"provider", "")
                .unwrap(),
        );
        let token = [7; 16];
        let channel = client
            .connect_channel("blit.cli.demo", b"caller", Some(token))
            .unwrap();
        assert_eq!(channel.window(), 17);
        assert_eq!(channel.metadata(), b"provider");
        assert_eq!(
            wire::parse_channel_request(&state.borrow().sent[0]).unwrap(),
            Some(wire::ChannelRequest::Connect {
                channel_id: 2,
                name: "blit.cli.demo",
                metadata: b"caller",
                listener_token: Some(token),
            })
        );
    }

    #[test]
    fn wrapper_rejects_an_ack_between_message_boundaries() {
        let (_guard, state, mut client) = boot();
        state.borrow_mut().incoming.push_back(
            wire::msg_channel_opened(2, STATUS_OK, 20, "client:0000000000000001", b"", "").unwrap(),
        );
        let mut channel = client.connect_channel("service", b"", None).unwrap();
        channel.send(&mut client, b"hello").unwrap();
        state
            .borrow_mut()
            .incoming
            .push_back(wire::msg_channel_ack(2, 3));
        assert!(matches!(
            channel.receive(&mut client),
            Err(Error::Protocol("ACK is not on a sent-message boundary"))
        ));
    }
}
