//! Process-global native-channel registry and flow-control state.

use blit_remote::channel::{
    CHANNEL_CLOSE_PEER_GONE, CHANNEL_CLOSE_PROTOCOL_VIOLATION, CHANNEL_MAX_UNCONSUMED_MESSAGES,
    CHANNEL_WINDOW_BYTES, ChannelRequest, msg_channel_accepted, msg_channel_ack,
    msg_channel_closed, msg_channel_data, msg_channel_opened,
};
use blit_remote::{
    STATUS_BUDGET, STATUS_CANCELLED, STATUS_CONFLICT, STATUS_INVALID, STATUS_NOT_FOUND, STATUS_OK,
    STATUS_PERMISSION,
};
use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicU64, AtomicUsize, Ordering},
};

const DEFAULT_MAX_LISTEN_PER_CLIENT: usize = 64;
const DEFAULT_MAX_LISTENERS: usize = 1024;
const DEFAULT_MAX_PER_CLIENT: usize = 64;
const DEFAULT_MAX_CONNECTED: usize = 128;
const DEFAULT_BUFFER_MAX: u64 = 256 * 1024 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
struct ChannelKey {
    endpoint: u64,
    channel_id: u32,
}

#[derive(Clone, Debug)]
struct Listener {
    name: String,
    metadata: Vec<u8>,
    token: [u8; 16],
}

/// Immutable listener identity used to fence extension command publication.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ListenerSnapshot {
    pub endpoint: u64,
    pub channel_id: u32,
    pub generation: u64,
    pub name: String,
    pub token: [u8; 16],
}

#[derive(Debug)]
struct Handle {
    peer: ChannelKey,
    reservation: PairReservation,
    slot: HandleSlotReservation,
    sent: u64,
    acked: u64,
    /// Cumulative sent-byte positions which are legal ACK boundaries.
    unconsumed_boundaries: VecDeque<u64>,
}

/// One connected pair's global admission reservation. Handles and every
/// queued frame derived from the pair retain a clone; capacity is returned
/// only after routing is gone and the final queued frame is written/dropped.
#[derive(Clone, Debug)]
struct PairReservation {
    _inner: Arc<PairReservationInner>,
}

#[derive(Debug)]
struct PairReservationInner {
    active_pairs: Arc<AtomicUsize>,
    reserved_window_bytes: Arc<AtomicU64>,
    window_bytes: u64,
}

impl Drop for PairReservationInner {
    fn drop(&mut self) {
        let pairs = self.active_pairs.fetch_sub(1, Ordering::Relaxed);
        let bytes = self
            .reserved_window_bytes
            .fetch_sub(self.window_bytes, Ordering::Relaxed);
        debug_assert!(pairs > 0);
        debug_assert!(bytes >= self.window_bytes);
    }
}

/// One endpoint's connected-handle admission slot. The live handle and every
/// frame queued for that endpoint share this guard, so closing routing state
/// cannot recycle the slot while an earlier or final frame is still queued.
#[derive(Clone, Debug)]
struct HandleSlotReservation {
    _inner: Arc<HandleSlotReservationInner>,
}

#[derive(Debug)]
struct HandleSlotReservationInner {
    endpoint_slots: Arc<Mutex<HashMap<u64, usize>>>,
    endpoint: u64,
}

impl Drop for HandleSlotReservationInner {
    fn drop(&mut self) {
        let mut slots = self
            .endpoint_slots
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let Some(count) = slots.get_mut(&self.endpoint) else {
            return;
        };
        debug_assert!(*count > 0);
        *count = count.saturating_sub(1);
        let remove = *count == 0;
        if remove {
            slots.remove(&self.endpoint);
        }
    }
}

/// A previously charged channel-outbox message. The charge travels with the
/// delivery through the queue and is released only after its write or drop.
#[derive(Debug)]
pub(crate) struct OutboxReservation {
    tracking: Option<Arc<crate::OutboxTracking>>,
    bytes: usize,
}

impl OutboxReservation {
    pub(crate) fn new(tracking: Arc<crate::OutboxTracking>, bytes: usize) -> Self {
        Self {
            tracking: Some(tracking),
            bytes,
        }
    }

    fn shrink_to(&mut self, bytes: usize) {
        assert!(
            bytes <= self.bytes,
            "replacement channel notification exceeds its reservation"
        );
        if let Some(tracking) = &self.tracking {
            tracking.shrink_reserved(self.bytes, bytes);
        }
        self.bytes = bytes;
    }

    #[cfg(test)]
    fn untracked(bytes: usize) -> Self {
        Self {
            tracking: None,
            bytes,
        }
    }
}

impl Drop for OutboxReservation {
    fn drop(&mut self) {
        if let Some(tracking) = &self.tracking {
            tracking.release(self.bytes);
        }
    }
}

#[derive(Debug)]
struct DrainReservation {
    draining: Arc<Mutex<HashSet<ChannelKey>>>,
    key: ChannelKey,
}

impl Drop for DrainReservation {
    fn drop(&mut self) {
        self.draining
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(&self.key);
    }
}

#[derive(Clone, Copy, Debug)]
struct Limits {
    listen_per_client: usize,
    listeners: usize,
    handles_per_client: usize,
    connected_pairs: usize,
    buffer_bytes: u64,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            listen_per_client: DEFAULT_MAX_LISTEN_PER_CLIENT,
            listeners: DEFAULT_MAX_LISTENERS,
            handles_per_client: DEFAULT_MAX_PER_CLIENT,
            connected_pairs: DEFAULT_MAX_CONNECTED,
            buffer_bytes: DEFAULT_BUFFER_MAX,
        }
    }
}

impl Limits {
    fn from_env() -> Self {
        let defaults = Self::default();
        Self {
            listen_per_client: crate::deployment_usize(
                "BLIT_CHANNEL_MAX_LISTEN_PER_CLIENT",
                defaults.listen_per_client,
            ),
            listeners: crate::deployment_usize("BLIT_CHANNEL_MAX_LISTENERS", defaults.listeners),
            handles_per_client: crate::deployment_usize(
                "BLIT_CHANNEL_MAX_PER_CLIENT",
                defaults.handles_per_client,
            ),
            connected_pairs: crate::deployment_usize(
                "BLIT_CHANNEL_MAX_CONNECTED",
                defaults.connected_pairs,
            ),
            buffer_bytes: crate::deployment_u64("BLIT_CHANNEL_BUFFER_MAX", defaults.buffer_bytes),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PairAdmissionError {
    Connected,
    Window,
}

impl PairAdmissionError {
    const fn detail(self) -> &'static str {
        match self {
            Self::Connected => "connected channel budget exhausted",
            Self::Window => "channel window budget exhausted",
        }
    }
}

/// One packet destined for a logical endpoint.
#[derive(Debug)]
pub(crate) struct Delivery {
    pub endpoint: u64,
    pub packet: Vec<u8>,
    _reservation: Option<PairReservation>,
    _slot: Option<HandleSlotReservation>,
    _drain: Option<DrainReservation>,
    pub(crate) outbox_reservation: Option<OutboxReservation>,
}

impl PartialEq for Delivery {
    fn eq(&self, other: &Self) -> bool {
        self.endpoint == other.endpoint && self.packet == other.packet
    }
}

impl Eq for Delivery {}

/// Named listeners and connected pairs shared by every logical endpoint.
pub(crate) struct ChannelFabric {
    enabled: bool,
    shutting_down: bool,
    boot_generation: u64,
    next_listener_generation: u64,
    next_server_id: HashMap<u64, u32>,
    peer_names: HashMap<u64, String>,
    listener_names: HashMap<String, ChannelKey>,
    listeners: HashMap<ChannelKey, Listener>,
    handles: HashMap<ChannelKey, Handle>,
    /// Connected-handle slots include live handles and terminal handles whose
    /// already-emitted frames have not drained from their endpoint writer.
    endpoint_slots: Arc<Mutex<HashMap<u64, usize>>>,
    /// IDs whose terminal packet is still queued at their endpoint.
    draining: Arc<Mutex<HashSet<ChannelKey>>>,
    active_pairs: Arc<AtomicUsize>,
    reserved_window_bytes: Arc<AtomicU64>,
    limits: Limits,
}

impl ChannelFabric {
    pub(crate) fn new(boot_generation: u64) -> Self {
        Self {
            enabled: crate::channels_enabled(),
            shutting_down: false,
            boot_generation,
            next_listener_generation: 1,
            next_server_id: HashMap::new(),
            peer_names: HashMap::new(),
            listener_names: HashMap::new(),
            listeners: HashMap::new(),
            handles: HashMap::new(),
            endpoint_slots: Arc::new(Mutex::new(HashMap::new())),
            draining: Arc::new(Mutex::new(HashSet::new())),
            active_pairs: Arc::new(AtomicUsize::new(0)),
            reserved_window_bytes: Arc::new(AtomicU64::new(0)),
            limits: Limits::from_env(),
        }
    }

    pub(crate) fn advertised(&self) -> bool {
        self.enabled && !self.shutting_down
    }

    /// Seal listener and pair admission. Live endpoints are then retired by
    /// their ordinary connection cleanup, preserving CLOSED ordering and all
    /// reservation guards.
    pub(crate) fn begin_shutdown(&mut self) {
        self.shutting_down = true;
    }

    pub(crate) fn listener_snapshot(
        &self,
        endpoint: u64,
        channel_id: u32,
    ) -> Option<ListenerSnapshot> {
        let listener = self.listeners.get(&ChannelKey {
            endpoint,
            channel_id,
        })?;
        Some(ListenerSnapshot {
            endpoint,
            channel_id,
            generation: u64::from_le_bytes(listener.token[8..].try_into().ok()?),
            name: listener.name.clone(),
            token: listener.token,
        })
    }

    pub(crate) fn register_endpoint(&mut self, endpoint: u64, peer_name: String) {
        debug_assert!(blit_remote::channel::valid_peer_name(&peer_name));
        let old = self.peer_names.insert(endpoint, peer_name);
        debug_assert!(old.is_none(), "logical endpoint registered twice");
    }

    /// Decode and apply one complete `0x95` C2S packet. This is the narrow
    /// wire-to-fabric boundary shared by socket and future in-process logical
    /// endpoints.
    pub(crate) fn handle_packet_reserved(
        &mut self,
        endpoint: u64,
        packet: &[u8],
        mut reserve_outbox: impl FnMut(u64, usize) -> Option<OutboxReservation>,
    ) -> Vec<Delivery> {
        let Ok((kind, channel_id, _)) = blit_remote::channel::channel_header(packet) else {
            return Vec::new();
        };
        match blit_remote::channel::parse_channel_request(packet) {
            Ok(Some(request)) if self.enabled && !self.shutting_down => {
                self.handle_reserved(endpoint, request, &mut reserve_outbox)
            }
            Ok(Some(_)) => self.refuse(endpoint, kind, channel_id),
            Ok(None) => Vec::new(),
            // A disabled family replies only to decodable operations which
            // normally have replies. Malformed and fire-and-forget packets
            // allocate nothing and are dropped.
            Err(_) if !self.enabled => Vec::new(),
            Err(error) => self.malformed(endpoint, kind, channel_id, &error.to_string()),
        }
    }

    fn handle_reserved(
        &mut self,
        endpoint: u64,
        request: ChannelRequest<'_>,
        reserve_outbox: &mut impl FnMut(u64, usize) -> Option<OutboxReservation>,
    ) -> Vec<Delivery> {
        match request {
            ChannelRequest::Listen {
                channel_id,
                name,
                metadata,
            } => self.listen(endpoint, channel_id, name, metadata, reserve_outbox),
            ChannelRequest::Connect {
                channel_id,
                name,
                metadata,
                listener_token,
            } => self.connect(
                endpoint,
                channel_id,
                name,
                metadata,
                listener_token,
                reserve_outbox,
            ),
            ChannelRequest::Data {
                channel_id,
                payload,
            } => self.data(endpoint, channel_id, payload),
            ChannelRequest::Ack { channel_id, bytes } => self.ack(endpoint, channel_id, bytes),
            ChannelRequest::Close { channel_id, reason } => {
                self.close(endpoint, channel_id, reason)
            }
        }
    }

    #[cfg(test)]
    fn handle_packet(&mut self, endpoint: u64, packet: &[u8]) -> Vec<Delivery> {
        self.handle_packet_reserved(endpoint, packet, |_, bytes| {
            Some(OutboxReservation::untracked(bytes))
        })
    }

    #[cfg(test)]
    fn handle(&mut self, endpoint: u64, request: ChannelRequest<'_>) -> Vec<Delivery> {
        self.handle_reserved(endpoint, request, &mut |_, bytes| {
            Some(OutboxReservation::untracked(bytes))
        })
    }

    pub(crate) fn refuse(&self, endpoint: u64, kind: u8, channel_id: u32) -> Vec<Delivery> {
        if matches!(
            kind,
            blit_remote::channel::CHANNEL_LISTEN | blit_remote::channel::CHANNEL_CONNECT
        ) {
            vec![self.failed_open(
                endpoint,
                channel_id,
                STATUS_PERMISSION,
                if self.shutting_down {
                    "server is shutting down"
                } else {
                    "native channels are disabled"
                },
            )]
        } else {
            Vec::new()
        }
    }

    pub(crate) fn malformed(
        &mut self,
        endpoint: u64,
        kind: u8,
        channel_id: u32,
        detail: &str,
    ) -> Vec<Delivery> {
        if matches!(
            kind,
            blit_remote::channel::CHANNEL_LISTEN | blit_remote::channel::CHANNEL_CONNECT
        ) {
            vec![self.failed_open(endpoint, channel_id, STATUS_INVALID, detail)]
        } else {
            self.protocol_violation(endpoint, channel_id, detail)
        }
    }

    fn listen(
        &mut self,
        endpoint: u64,
        channel_id: u32,
        name: &str,
        metadata: &[u8],
        reserve_outbox: &mut impl FnMut(u64, usize) -> Option<OutboxReservation>,
    ) -> Vec<Delivery> {
        let key = ChannelKey {
            endpoint,
            channel_id,
        };
        if self.key_is_live(key) {
            return vec![self.failed_open(
                endpoint,
                channel_id,
                STATUS_CONFLICT,
                "channel id is already live",
            )];
        }
        if self.listener_names.contains_key(name) {
            return vec![self.failed_open(
                endpoint,
                channel_id,
                STATUS_CONFLICT,
                "channel name already has a listener",
            )];
        }
        if self.listeners.len() >= self.limits.listeners
            || self.listener_count(endpoint) >= self.limits.listen_per_client
        {
            return vec![self.failed_open(
                endpoint,
                channel_id,
                STATUS_BUDGET,
                "channel listener budget exhausted",
            )];
        }
        let Some(generation) = self.next_listener_generation.checked_add(1) else {
            return vec![self.failed_open(
                endpoint,
                channel_id,
                STATUS_BUDGET,
                "channel listener generation exhausted",
            )];
        };
        let listener_generation = self.next_listener_generation;
        let mut token = [0; 16];
        token[..8].copy_from_slice(&self.boot_generation.to_le_bytes());
        token[8..].copy_from_slice(&listener_generation.to_le_bytes());
        let mut delivery = opened(endpoint, channel_id, STATUS_OK, "");
        let Some(outbox_reservation) = reserve_outbox(endpoint, delivery.packet.len()) else {
            // Hard outbox admission cancelled the endpoint. Nothing was
            // published and the client-created ID remains immediately free.
            return Vec::new();
        };
        delivery.outbox_reservation = Some(outbox_reservation);

        self.next_listener_generation = generation;
        self.listener_names.insert(name.to_owned(), key);
        self.listeners.insert(
            key,
            Listener {
                name: name.to_owned(),
                metadata: metadata.to_vec(),
                token,
            },
        );
        vec![delivery]
    }

    fn connect(
        &mut self,
        endpoint: u64,
        channel_id: u32,
        name: &str,
        metadata: &[u8],
        listener_token: Option<[u8; 16]>,
        reserve_outbox: &mut impl FnMut(u64, usize) -> Option<OutboxReservation>,
    ) -> Vec<Delivery> {
        let connector_key = ChannelKey {
            endpoint,
            channel_id,
        };
        if self.key_is_live(connector_key) {
            return vec![self.failed_open(
                endpoint,
                channel_id,
                STATUS_CONFLICT,
                "channel id is already live",
            )];
        }
        let Some(&listener_key) = self.listener_names.get(name) else {
            return vec![self.failed_open(
                endpoint,
                channel_id,
                STATUS_NOT_FOUND,
                "channel listener was not found",
            )];
        };
        let listener = self
            .listeners
            .get(&listener_key)
            .expect("listener name and record are committed together")
            .clone();
        if listener_token.is_some_and(|token| token != listener.token) {
            return vec![self.failed_open(
                endpoint,
                channel_id,
                STATUS_CONFLICT,
                "channel listener generation changed",
            )];
        }

        let Some((accepted_id, next_server_id)) = self.server_id_candidate(listener_key.endpoint)
        else {
            return vec![self.failed_open(
                endpoint,
                channel_id,
                STATUS_BUDGET,
                "accepted channel id space exhausted",
            )];
        };
        let accepted_key = ChannelKey {
            endpoint: listener_key.endpoint,
            channel_id: accepted_id,
        };

        let listener_peer = self.peer_name(listener_key.endpoint);
        let connector_peer = self.peer_name(endpoint);
        let connector_packet = msg_channel_opened(
            channel_id,
            STATUS_OK,
            CHANNEL_WINDOW_BYTES,
            &listener_peer,
            &listener.metadata,
            "",
        )
        .expect("server channel fields obey fixed limits");
        let listener_packet = msg_channel_accepted(
            accepted_id,
            listener_key.channel_id,
            CHANNEL_WINDOW_BYTES,
            &connector_peer,
            metadata,
        )
        .expect("validated request fields obey fixed limits");

        let (reservation, connector_slot, accepted_slot) = match self
            .reserve_pair(endpoint, listener_key.endpoint)
        {
            Ok(reservations) => reservations,
            Err(error) => {
                return vec![self.failed_open(endpoint, channel_id, STATUS_BUDGET, error.detail())];
            }
        };

        // Reserve the two initial notifications before either handle becomes
        // visible. Endpoint order is stable; if the listener is encountered
        // first and fails, reserve only the connector's smaller cancellation
        // reply afterward.
        let (connector_outbox, listener_outbox) = if endpoint <= listener_key.endpoint {
            let Some(connector_outbox) = reserve_outbox(endpoint, connector_packet.len()) else {
                return Vec::new();
            };
            let Some(listener_outbox) =
                reserve_outbox(listener_key.endpoint, listener_packet.len())
            else {
                return self.cancelled_connect(endpoint, channel_id, connector_outbox);
            };
            (connector_outbox, listener_outbox)
        } else {
            let Some(listener_outbox) =
                reserve_outbox(listener_key.endpoint, listener_packet.len())
            else {
                let mut cancelled = self.failed_open(endpoint, channel_id, STATUS_CANCELLED, "");
                let Some(connector_outbox) = reserve_outbox(endpoint, cancelled.packet.len())
                else {
                    return Vec::new();
                };
                cancelled.outbox_reservation = Some(connector_outbox);
                return vec![cancelled];
            };
            let Some(connector_outbox) = reserve_outbox(endpoint, connector_packet.len()) else {
                return Vec::new();
            };
            (connector_outbox, listener_outbox)
        };

        debug_assert_eq!(self.listener_names.get(name), Some(&listener_key));
        debug_assert_eq!(
            self.listeners.get(&listener_key).map(|entry| entry.token),
            Some(listener.token)
        );

        // Publication is one non-awaiting commit after every fallible
        // reservation. In particular, the accepted-ID cursor advances only
        // here, so failed admission leaves that server ID reusable.
        self.next_server_id
            .insert(listener_key.endpoint, next_server_id);
        self.handles.insert(
            connector_key,
            Handle {
                peer: accepted_key,
                reservation: reservation.clone(),
                slot: connector_slot.clone(),
                sent: 0,
                acked: 0,
                unconsumed_boundaries: VecDeque::new(),
            },
        );
        self.handles.insert(
            accepted_key,
            Handle {
                peer: connector_key,
                reservation: reservation.clone(),
                slot: accepted_slot.clone(),
                sent: 0,
                acked: 0,
                unconsumed_boundaries: VecDeque::new(),
            },
        );

        vec![
            Delivery {
                endpoint,
                packet: connector_packet,
                _reservation: Some(reservation.clone()),
                _slot: Some(connector_slot),
                _drain: None,
                outbox_reservation: Some(connector_outbox),
            },
            Delivery {
                endpoint: listener_key.endpoint,
                packet: listener_packet,
                _reservation: Some(reservation),
                _slot: Some(accepted_slot),
                _drain: None,
                outbox_reservation: Some(listener_outbox),
            },
        ]
    }

    fn data(&mut self, endpoint: u64, channel_id: u32, payload: &[u8]) -> Vec<Delivery> {
        let key = ChannelKey {
            endpoint,
            channel_id,
        };
        if self.listeners.contains_key(&key) {
            return self.protocol_violation(endpoint, channel_id, "listener received channel data");
        }
        let Some(handle) = self.handles.get(&key) else {
            return Vec::new();
        };
        let Some(sent) = handle.sent.checked_add(payload.len() as u64) else {
            return self.protocol_violation(endpoint, channel_id, "channel byte counter overflow");
        };
        let credit_end = handle.acked.checked_add(CHANNEL_WINDOW_BYTES);
        if credit_end.is_none_or(|credit_end| sent > credit_end)
            || handle.unconsumed_boundaries.len() >= CHANNEL_MAX_UNCONSUMED_MESSAGES
        {
            return self.protocol_violation(endpoint, channel_id, "channel send window exceeded");
        }
        let peer = handle.peer;
        let reservation = handle.reservation.clone();
        let slot = self
            .handles
            .get(&peer)
            .expect("connected channel handles are paired")
            .slot
            .clone();
        let handle = self.handles.get_mut(&key).expect("handle remained live");
        handle.sent = sent;
        handle.unconsumed_boundaries.push_back(sent);
        vec![Delivery {
            endpoint: peer.endpoint,
            packet: msg_channel_data(peer.channel_id, payload)
                .expect("validated request payload obeys fixed limits"),
            _reservation: Some(reservation),
            _slot: Some(slot),
            _drain: None,
            outbox_reservation: None,
        }]
    }

    fn ack(&mut self, endpoint: u64, channel_id: u32, bytes: u64) -> Vec<Delivery> {
        let key = ChannelKey {
            endpoint,
            channel_id,
        };
        if self.listeners.contains_key(&key) {
            return self.protocol_violation(endpoint, channel_id, "listener received channel ACK");
        }
        let Some((peer_key, reservation)) = self
            .handles
            .get(&key)
            .map(|handle| (handle.peer, handle.reservation.clone()))
        else {
            return Vec::new();
        };
        let peer = self
            .handles
            .get(&peer_key)
            .expect("connected channel handles are paired");
        let valid = bytes >= peer.acked
            && bytes <= peer.sent
            && (bytes == peer.acked || peer.unconsumed_boundaries.contains(&bytes));
        if !valid {
            return self.protocol_violation(endpoint, channel_id, "channel ACK is invalid");
        }
        let peer = self
            .handles
            .get_mut(&peer_key)
            .expect("connected channel handles are paired");
        peer.acked = bytes;
        while peer
            .unconsumed_boundaries
            .front()
            .is_some_and(|boundary| *boundary <= bytes)
        {
            peer.unconsumed_boundaries.pop_front();
        }
        let slot = peer.slot.clone();
        vec![Delivery {
            endpoint: peer_key.endpoint,
            packet: msg_channel_ack(peer_key.channel_id, bytes),
            _reservation: Some(reservation),
            _slot: Some(slot),
            _drain: None,
            outbox_reservation: None,
        }]
    }

    fn close(&mut self, endpoint: u64, channel_id: u32, reason: u8) -> Vec<Delivery> {
        let key = ChannelKey {
            endpoint,
            channel_id,
        };
        if self.listeners.contains_key(&key) {
            self.remove_listener(key);
            let drain = self.begin_drain(key);
            return vec![closed(key, reason, "", None, None, Some(drain))];
        }
        if self.handles.contains_key(&key) {
            return self.close_pair(key, reason, "", true);
        }
        Vec::new()
    }

    fn protocol_violation(
        &mut self,
        endpoint: u64,
        channel_id: u32,
        detail: &str,
    ) -> Vec<Delivery> {
        let key = ChannelKey {
            endpoint,
            channel_id,
        };
        if self.listeners.contains_key(&key) {
            self.remove_listener(key);
            let drain = self.begin_drain(key);
            return vec![closed(
                key,
                CHANNEL_CLOSE_PROTOCOL_VIOLATION,
                detail,
                None,
                None,
                Some(drain),
            )];
        }
        if self.handles.contains_key(&key) {
            return self.close_pair(key, CHANNEL_CLOSE_PROTOCOL_VIOLATION, detail, true);
        }
        Vec::new()
    }

    /// Remove every object owned by a departing endpoint and notify surviving
    /// channel peers. Already accepted pairs are independent of listeners.
    pub(crate) fn close_endpoint(&mut self, endpoint: u64) -> Vec<Delivery> {
        self.peer_names.remove(&endpoint);
        let listener_keys: Vec<_> = self
            .listeners
            .keys()
            .filter(|key| key.endpoint == endpoint)
            .copied()
            .collect();
        for key in listener_keys {
            self.remove_listener(key);
        }

        let handle_keys: Vec<_> = self
            .handles
            .keys()
            .filter(|key| key.endpoint == endpoint)
            .copied()
            .collect();
        let mut deliveries = Vec::new();
        for key in handle_keys {
            if let Some((peer, reservation, peer_slot)) =
                self.handles.get(&key).and_then(|handle| {
                    let peer = handle.peer;
                    self.handles.get(&peer).map(|peer_handle| {
                        (peer, handle.reservation.clone(), peer_handle.slot.clone())
                    })
                })
            {
                self.remove_pair(key);
                if peer.endpoint != endpoint {
                    let drain = self.begin_drain(peer);
                    deliveries.push(closed(
                        peer,
                        CHANNEL_CLOSE_PEER_GONE,
                        "channel peer disconnected",
                        Some(reservation),
                        Some(peer_slot),
                        Some(drain),
                    ));
                }
            }
        }
        deliveries
    }

    fn close_pair(
        &mut self,
        key: ChannelKey,
        reason: u8,
        detail: &str,
        notify_both: bool,
    ) -> Vec<Delivery> {
        let Some((peer, reservation, key_slot, peer_slot)) =
            self.handles.get(&key).and_then(|handle| {
                let peer = handle.peer;
                self.handles.get(&peer).map(|peer_handle| {
                    (
                        peer,
                        handle.reservation.clone(),
                        handle.slot.clone(),
                        peer_handle.slot.clone(),
                    )
                })
            })
        else {
            return Vec::new();
        };
        self.remove_pair(key);
        let key_drain = self.begin_drain(key);
        let mut deliveries = vec![closed(
            key,
            reason,
            detail,
            Some(reservation.clone()),
            Some(key_slot),
            Some(key_drain),
        )];
        if notify_both {
            let peer_drain = self.begin_drain(peer);
            deliveries.push(closed(
                peer,
                reason,
                detail,
                Some(reservation),
                Some(peer_slot),
                Some(peer_drain),
            ));
        }
        deliveries
    }

    fn remove_listener(&mut self, key: ChannelKey) {
        if let Some(listener) = self.listeners.remove(&key) {
            self.listener_names.remove(&listener.name);
        }
    }

    fn remove_pair(&mut self, key: ChannelKey) {
        if let Some(handle) = self.handles.remove(&key) {
            self.handles.remove(&handle.peer);
        }
    }

    fn listener_count(&self, endpoint: u64) -> usize {
        self.listeners
            .keys()
            .filter(|key| key.endpoint == endpoint)
            .count()
    }

    fn reserve_pair(
        &self,
        connector_endpoint: u64,
        accepted_endpoint: u64,
    ) -> Result<
        (
            PairReservation,
            HandleSlotReservation,
            HandleSlotReservation,
        ),
        PairAdmissionError,
    > {
        let (connector_slot, accepted_slot) =
            self.reserve_handle_slots(connector_endpoint, accepted_endpoint)?;
        if self.active_pairs.load(Ordering::Relaxed) >= self.limits.connected_pairs {
            return Err(PairAdmissionError::Connected);
        }

        let pair_window = CHANNEL_WINDOW_BYTES.saturating_mul(2);
        let current_window_bytes = self.reserved_window_bytes.load(Ordering::Relaxed);
        let Some(next_window_bytes) = current_window_bytes.checked_add(pair_window) else {
            return Err(PairAdmissionError::Window);
        };
        if next_window_bytes > self.limits.buffer_bytes {
            return Err(PairAdmissionError::Window);
        }

        self.active_pairs.fetch_add(1, Ordering::Relaxed);
        self.reserved_window_bytes
            .fetch_add(pair_window, Ordering::Relaxed);
        Ok((
            PairReservation {
                _inner: Arc::new(PairReservationInner {
                    active_pairs: self.active_pairs.clone(),
                    reserved_window_bytes: self.reserved_window_bytes.clone(),
                    window_bytes: pair_window,
                }),
            },
            connector_slot,
            accepted_slot,
        ))
    }

    fn reserve_handle_slots(
        &self,
        connector_endpoint: u64,
        accepted_endpoint: u64,
    ) -> Result<(HandleSlotReservation, HandleSlotReservation), PairAdmissionError> {
        let mut slots = self
            .endpoint_slots
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let connector_count = slots.get(&connector_endpoint).copied().unwrap_or(0);
        let connector_needed = usize::from(connector_endpoint == accepted_endpoint) + 1;
        if connector_count
            .checked_add(connector_needed)
            .is_none_or(|count| count > self.limits.handles_per_client)
        {
            return Err(PairAdmissionError::Connected);
        }
        if connector_endpoint != accepted_endpoint {
            let accepted_count = slots.get(&accepted_endpoint).copied().unwrap_or(0);
            if accepted_count
                .checked_add(1)
                .is_none_or(|count| count > self.limits.handles_per_client)
            {
                return Err(PairAdmissionError::Connected);
            }
        }

        *slots.entry(connector_endpoint).or_default() += 1;
        *slots.entry(accepted_endpoint).or_default() += 1;
        drop(slots);
        let reservation = |endpoint| HandleSlotReservation {
            _inner: Arc::new(HandleSlotReservationInner {
                endpoint_slots: self.endpoint_slots.clone(),
                endpoint,
            }),
        };
        Ok((
            reservation(connector_endpoint),
            reservation(accepted_endpoint),
        ))
    }

    fn cancelled_connect(
        &self,
        endpoint: u64,
        channel_id: u32,
        mut outbox_reservation: OutboxReservation,
    ) -> Vec<Delivery> {
        let mut delivery = self.failed_open(endpoint, channel_id, STATUS_CANCELLED, "");
        // The successful OPENED is at least as large as this empty-detail
        // cancellation, so its already-held reservation remains sufficient.
        outbox_reservation.shrink_to(delivery.packet.len());
        delivery.outbox_reservation = Some(outbox_reservation);
        vec![delivery]
    }

    fn key_is_live(&self, key: ChannelKey) -> bool {
        self.listeners.contains_key(&key)
            || self.handles.contains_key(&key)
            || self
                .draining
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .contains(&key)
    }

    fn begin_drain(&self, key: ChannelKey) -> DrainReservation {
        let inserted = self
            .draining
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(key);
        debug_assert!(inserted, "channel ID entered drain twice");
        DrainReservation {
            draining: self.draining.clone(),
            key,
        }
    }

    fn failed_open(&self, endpoint: u64, channel_id: u32, status: u8, detail: &str) -> Delivery {
        let key = ChannelKey {
            endpoint,
            channel_id,
        };
        let drain = (!self.key_is_live(key)).then(|| self.begin_drain(key));
        opened_with_drain(endpoint, channel_id, status, detail, drain)
    }

    fn server_id_candidate(&self, endpoint: u64) -> Option<(u32, u32)> {
        let mut candidate = self.next_server_id.get(&endpoint).copied().unwrap_or(1);
        loop {
            let key = ChannelKey {
                endpoint,
                channel_id: candidate,
            };
            let next = candidate.checked_add(2)?;
            if !self.key_is_live(key) {
                return Some((candidate, next));
            }
            candidate = next;
        }
    }

    fn peer_name(&self, endpoint: u64) -> String {
        self.peer_names
            .get(&endpoint)
            .cloned()
            .unwrap_or_else(|| network_peer_name(endpoint))
    }
}

fn opened(endpoint: u64, channel_id: u32, status: u8, detail: &str) -> Delivery {
    opened_with_drain(endpoint, channel_id, status, detail, None)
}

fn opened_with_drain(
    endpoint: u64,
    channel_id: u32,
    status: u8,
    detail: &str,
    drain: Option<DrainReservation>,
) -> Delivery {
    Delivery {
        endpoint,
        packet: msg_channel_opened(channel_id, status, 0, "", &[], detail)
            .expect("server channel detail obeys fixed limit"),
        _reservation: None,
        _slot: None,
        _drain: drain,
        outbox_reservation: None,
    }
}

fn closed(
    key: ChannelKey,
    reason: u8,
    detail: &str,
    reservation: Option<PairReservation>,
    slot: Option<HandleSlotReservation>,
    drain: Option<DrainReservation>,
) -> Delivery {
    Delivery {
        endpoint: key.endpoint,
        packet: msg_channel_closed(key.channel_id, reason, detail)
            .expect("server channel detail obeys fixed limit"),
        _reservation: reservation,
        _slot: slot,
        _drain: drain,
        outbox_reservation: None,
    }
}

fn network_peer_name(endpoint: u64) -> String {
    format!("client:{endpoint:016x}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use blit_remote::channel::{
        CHANNEL_CLOSE_CANCELLED, ChannelMessage, msg_channel_connect, msg_channel_listen,
        parse_channel_message,
    };

    fn fabric() -> ChannelFabric {
        let mut fabric = ChannelFabric::new(0x1234);
        fabric.enabled = true;
        fabric.limits = Limits::default();
        fabric
    }

    fn listen(fabric: &mut ChannelFabric, endpoint: u64, id: u32, name: &str) {
        let output = fabric.handle(
            endpoint,
            ChannelRequest::Listen {
                channel_id: id,
                name,
                metadata: b"listener-meta",
            },
        );
        assert!(matches!(
            parse_channel_message(&output[0].packet).unwrap(),
            Some(ChannelMessage::Opened {
                status: STATUS_OK,
                window: 0,
                ..
            })
        ));
    }

    fn connect(fabric: &mut ChannelFabric, endpoint: u64, id: u32, name: &str) -> u32 {
        let output = fabric.handle(
            endpoint,
            ChannelRequest::Connect {
                channel_id: id,
                name,
                metadata: b"connector-meta",
                listener_token: None,
            },
        );
        assert_eq!(output.len(), 2);
        assert!(matches!(
            parse_channel_message(&output[0].packet).unwrap(),
            Some(ChannelMessage::Opened {
                status: STATUS_OK,
                window: CHANNEL_WINDOW_BYTES,
                ..
            })
        ));
        let Some(ChannelMessage::Accepted { channel_id, .. }) =
            parse_channel_message(&output[1].packet).unwrap()
        else {
            panic!("listener did not receive ACCEPTED")
        };
        assert_eq!(channel_id & 1, 1);
        channel_id
    }

    fn endpoint_slots(fabric: &ChannelFabric, endpoint: u64) -> usize {
        fabric
            .endpoint_slots
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(&endpoint)
            .copied()
            .unwrap_or(0)
    }

    #[test]
    fn two_clients_exchange_messages_and_ack_at_boundaries() {
        let mut fabric = fabric();
        let output = fabric.handle_packet(
            1,
            &msg_channel_listen(2, "com.example.test", b"listener-meta").unwrap(),
        );
        assert!(matches!(
            parse_channel_message(&output[0].packet).unwrap(),
            Some(ChannelMessage::Opened {
                status: STATUS_OK,
                window: 0,
                ..
            })
        ));
        drop(output);
        let output = fabric.handle_packet(
            2,
            &msg_channel_connect(4, "com.example.test", b"connector-meta", None).unwrap(),
        );
        assert_eq!(output.len(), 2);
        let Some(ChannelMessage::Accepted {
            channel_id: accepted_id,
            ..
        }) = parse_channel_message(&output[1].packet).unwrap()
        else {
            panic!("listener did not receive ACCEPTED")
        };
        drop(output);

        let output = fabric.handle_packet(2, &msg_channel_data(4, b"hello").unwrap());
        assert_eq!(output[0].endpoint, 1);
        assert_eq!(
            parse_channel_message(&output[0].packet).unwrap(),
            Some(ChannelMessage::Data {
                channel_id: accepted_id,
                payload: b"hello",
            })
        );
        drop(output);

        let output = fabric.handle_packet(1, &msg_channel_ack(accepted_id, 5));
        assert_eq!(output[0].endpoint, 2);
        assert_eq!(
            parse_channel_message(&output[0].packet).unwrap(),
            Some(ChannelMessage::Ack {
                channel_id: 4,
                bytes: 5,
            })
        );
        drop(output);

        let output = fabric.handle_packet(
            2,
            &blit_remote::channel::msg_channel_close(4, CHANNEL_CLOSE_CANCELLED).unwrap(),
        );
        assert_eq!(output.len(), 2);
        assert!(output.iter().all(|delivery| matches!(
            parse_channel_message(&delivery.packet).unwrap(),
            Some(ChannelMessage::Closed {
                reason: CHANNEL_CLOSE_CANCELLED,
                ..
            })
        )));
        assert!(fabric.handles.is_empty());
        assert_eq!(fabric.active_pairs.load(Ordering::Relaxed), 1);
        assert_eq!(
            fabric.reserved_window_bytes.load(Ordering::Relaxed),
            CHANNEL_WINDOW_BYTES * 2
        );
        drop(output);
        assert_eq!(fabric.active_pairs.load(Ordering::Relaxed), 0);
        assert_eq!(fabric.reserved_window_bytes.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn duplicate_listener_names_conflict_without_replacing_owner() {
        let mut fabric = fabric();
        listen(&mut fabric, 1, 2, "same");
        let output = fabric.handle(
            2,
            ChannelRequest::Listen {
                channel_id: 4,
                name: "same",
                metadata: b"",
            },
        );
        assert!(matches!(
            parse_channel_message(&output[0].packet).unwrap(),
            Some(ChannelMessage::Opened {
                status: STATUS_CONFLICT,
                ..
            })
        ));
        assert_eq!(fabric.listener_names["same"].endpoint, 1);
    }

    #[test]
    fn registered_extension_peer_identity_is_forwarded_verbatim() {
        let mut fabric = fabric();
        fabric.register_endpoint(1, "ext:0000000000000042:3".to_owned());
        listen(&mut fabric, 1, 2, "service");
        let output = fabric.handle(
            2,
            ChannelRequest::Connect {
                channel_id: 4,
                name: "service",
                metadata: b"",
                listener_token: None,
            },
        );
        assert!(matches!(
            parse_channel_message(&output[0].packet).unwrap(),
            Some(ChannelMessage::Opened {
                peer: "ext:0000000000000042:3",
                ..
            })
        ));
    }

    #[test]
    fn stale_listener_token_cannot_connect_to_replacement() {
        let mut fabric = fabric();
        listen(&mut fabric, 1, 2, "service");
        let old = fabric.listeners[&ChannelKey {
            endpoint: 1,
            channel_id: 2,
        }]
            .token;
        fabric.close(1, 2, blit_remote::channel::CHANNEL_CLOSE_NORMAL);
        listen(&mut fabric, 2, 4, "service");
        let output = fabric.handle(
            3,
            ChannelRequest::Connect {
                channel_id: 6,
                name: "service",
                metadata: b"",
                listener_token: Some(old),
            },
        );
        assert!(matches!(
            parse_channel_message(&output[0].packet).unwrap(),
            Some(ChannelMessage::Opened {
                status: STATUS_CONFLICT,
                ..
            })
        ));
        assert!(fabric.handles.is_empty());
    }

    #[test]
    fn ack_must_land_on_a_sent_message_boundary() {
        let mut fabric = fabric();
        listen(&mut fabric, 1, 2, "service");
        let accepted_id = connect(&mut fabric, 2, 4, "service");
        fabric.data(2, 4, b"abc");
        fabric.data(2, 4, b"de");
        let output = fabric.ack(1, accepted_id, 4);
        assert_eq!(output.len(), 2);
        assert!(output.iter().all(|delivery| matches!(
            parse_channel_message(&delivery.packet).unwrap(),
            Some(ChannelMessage::Closed {
                reason: CHANNEL_CLOSE_PROTOCOL_VIOLATION,
                ..
            })
        )));
        assert!(fabric.handles.is_empty());
    }

    #[test]
    fn disconnect_notifies_only_the_surviving_peer() {
        let mut fabric = fabric();
        listen(&mut fabric, 1, 2, "service");
        connect(&mut fabric, 2, 4, "service");
        let output = fabric.close_endpoint(1);
        assert_eq!(output.len(), 1);
        assert_eq!(output[0].endpoint, 2);
        assert!(matches!(
            parse_channel_message(&output[0].packet).unwrap(),
            Some(ChannelMessage::Closed {
                channel_id: 4,
                reason: CHANNEL_CLOSE_PEER_GONE,
                ..
            })
        ));
        assert!(fabric.listeners.is_empty());
        assert!(fabric.handles.is_empty());
        assert_eq!(fabric.active_pairs.load(Ordering::Relaxed), 1);
        drop(output);
        assert_eq!(fabric.active_pairs.load(Ordering::Relaxed), 0);
        assert_eq!(fabric.reserved_window_bytes.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn pair_admission_is_atomic_at_capacity() {
        let mut fabric = fabric();
        fabric.limits.connected_pairs = 0;
        listen(&mut fabric, 1, 2, "service");
        let output = fabric.handle(
            2,
            ChannelRequest::Connect {
                channel_id: 4,
                name: "service",
                metadata: b"",
                listener_token: None,
            },
        );
        assert!(matches!(
            parse_channel_message(&output[0].packet).unwrap(),
            Some(ChannelMessage::Opened {
                status: STATUS_BUDGET,
                ..
            })
        ));
        assert!(fabric.handles.is_empty());
        assert_eq!(fabric.active_pairs.load(Ordering::Relaxed), 0);
        assert_eq!(fabric.reserved_window_bytes.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn listener_success_is_reserved_before_publication() {
        let mut fabric = fabric();
        let generation = fabric.next_listener_generation;
        let output = fabric.handle_reserved(
            7,
            ChannelRequest::Listen {
                channel_id: 2,
                name: "service",
                metadata: b"",
            },
            &mut |endpoint, _| {
                assert_eq!(endpoint, 7);
                None
            },
        );
        assert!(output.is_empty());
        assert!(fabric.listeners.is_empty());
        assert!(fabric.listener_names.is_empty());
        assert_eq!(fabric.next_listener_generation, generation);
        assert!(!fabric.key_is_live(ChannelKey {
            endpoint: 7,
            channel_id: 2,
        }));

        let output = fabric.handle(
            7,
            ChannelRequest::Listen {
                channel_id: 2,
                name: "service",
                metadata: b"",
            },
        );
        assert_eq!(output.len(), 1);
        assert_eq!(
            u64::from_le_bytes(
                fabric.listeners[&ChannelKey {
                    endpoint: 7,
                    channel_id: 2,
                }]
                    .token[8..]
                    .try_into()
                    .unwrap()
            ),
            generation
        );
    }

    #[test]
    fn connector_notification_failure_publishes_nothing_and_reuses_accepted_id() {
        let mut fabric = fabric();
        listen(&mut fabric, 1, 2, "service");
        let mut attempts = Vec::new();
        let output = fabric.handle_reserved(
            2,
            ChannelRequest::Connect {
                channel_id: 4,
                name: "service",
                metadata: b"",
                listener_token: None,
            },
            &mut |endpoint, bytes| {
                attempts.push(endpoint);
                (endpoint != 2).then(|| OutboxReservation::untracked(bytes))
            },
        );
        assert_eq!(attempts, vec![1, 2]);
        assert!(output.is_empty());
        assert!(fabric.handles.is_empty());
        assert_eq!(endpoint_slots(&fabric, 1), 0);
        assert_eq!(endpoint_slots(&fabric, 2), 0);
        assert_eq!(fabric.active_pairs.load(Ordering::Relaxed), 0);
        assert_eq!(fabric.reserved_window_bytes.load(Ordering::Relaxed), 0);
        assert!(!fabric.next_server_id.contains_key(&1));

        assert_eq!(connect(&mut fabric, 2, 4, "service"), 1);
    }

    #[test]
    fn listener_notification_failure_cancels_connector_without_publishing_pair() {
        let mut fabric = fabric();
        listen(&mut fabric, 2, 2, "service");
        let mut attempts = Vec::new();
        let output = fabric.handle_reserved(
            1,
            ChannelRequest::Connect {
                channel_id: 4,
                name: "service",
                metadata: b"",
                listener_token: None,
            },
            &mut |endpoint, bytes| {
                attempts.push(endpoint);
                (endpoint != 2).then(|| OutboxReservation::untracked(bytes))
            },
        );
        assert_eq!(attempts, vec![1, 2]);
        assert_eq!(output.len(), 1);
        assert_eq!(output[0].endpoint, 1);
        assert!(matches!(
            parse_channel_message(&output[0].packet).unwrap(),
            Some(ChannelMessage::Opened {
                status: STATUS_CANCELLED,
                window: 0,
                ..
            })
        ));
        assert!(fabric.handles.is_empty());
        assert_eq!(endpoint_slots(&fabric, 1), 0);
        assert_eq!(endpoint_slots(&fabric, 2), 0);
        assert_eq!(fabric.active_pairs.load(Ordering::Relaxed), 0);
        assert_eq!(fabric.reserved_window_bytes.load(Ordering::Relaxed), 0);
        assert!(!fabric.next_server_id.contains_key(&2));
        drop(output);

        assert_eq!(connect(&mut fabric, 1, 4, "service"), 1);
    }

    #[test]
    fn earlier_queued_frame_retains_endpoint_slot_after_close() {
        let mut fabric = fabric();
        fabric.limits.handles_per_client = 1;
        listen(&mut fabric, 1, 2, "service");
        let mut initial = fabric.handle(
            2,
            ChannelRequest::Connect {
                channel_id: 4,
                name: "service",
                metadata: b"",
                listener_token: None,
            },
        );
        let connector_opened = initial.remove(0);
        drop(initial);
        assert_eq!(endpoint_slots(&fabric, 1), 1);
        assert_eq!(endpoint_slots(&fabric, 2), 1);

        let closed = fabric.close(2, 4, CHANNEL_CLOSE_CANCELLED);
        drop(closed);
        assert_eq!(endpoint_slots(&fabric, 1), 0);
        assert_eq!(endpoint_slots(&fabric, 2), 1);

        let refused = fabric.handle(
            2,
            ChannelRequest::Connect {
                channel_id: 6,
                name: "service",
                metadata: b"",
                listener_token: None,
            },
        );
        assert!(matches!(
            parse_channel_message(&refused[0].packet).unwrap(),
            Some(ChannelMessage::Opened {
                status: STATUS_BUDGET,
                ..
            })
        ));
        drop(refused);

        drop(connector_opened);
        assert_eq!(endpoint_slots(&fabric, 2), 0);
        assert_eq!(connect(&mut fabric, 2, 6, "service"), 3);
    }

    #[test]
    fn final_closed_retains_endpoint_slot_until_it_drains() {
        let mut fabric = fabric();
        fabric.limits.handles_per_client = 1;
        listen(&mut fabric, 1, 2, "service");
        let initial = fabric.handle(
            2,
            ChannelRequest::Connect {
                channel_id: 4,
                name: "service",
                metadata: b"",
                listener_token: None,
            },
        );
        drop(initial);
        let mut closed = fabric.close(2, 4, CHANNEL_CLOSE_CANCELLED);
        let listener_closed = closed.remove(
            closed
                .iter()
                .position(|delivery| delivery.endpoint == 1)
                .unwrap(),
        );
        drop(closed);
        assert_eq!(endpoint_slots(&fabric, 1), 1);
        assert_eq!(endpoint_slots(&fabric, 2), 0);

        let refused = fabric.handle(
            3,
            ChannelRequest::Connect {
                channel_id: 6,
                name: "service",
                metadata: b"",
                listener_token: None,
            },
        );
        assert!(matches!(
            parse_channel_message(&refused[0].packet).unwrap(),
            Some(ChannelMessage::Opened {
                status: STATUS_BUDGET,
                ..
            })
        ));
        drop(refused);

        drop(listener_closed);
        assert_eq!(endpoint_slots(&fabric, 1), 0);
        assert_eq!(connect(&mut fabric, 3, 6, "service"), 3);
    }

    #[test]
    fn failed_open_id_is_reserved_until_its_reply_drains() {
        let mut fabric = fabric();
        let first = fabric.handle(
            1,
            ChannelRequest::Connect {
                channel_id: 2,
                name: "missing",
                metadata: b"",
                listener_token: None,
            },
        );
        assert!(matches!(
            parse_channel_message(&first[0].packet).unwrap(),
            Some(ChannelMessage::Opened {
                status: STATUS_NOT_FOUND,
                ..
            })
        ));

        let duplicate = fabric.handle(
            1,
            ChannelRequest::Connect {
                channel_id: 2,
                name: "missing",
                metadata: b"",
                listener_token: None,
            },
        );
        assert!(matches!(
            parse_channel_message(&duplicate[0].packet).unwrap(),
            Some(ChannelMessage::Opened {
                status: STATUS_CONFLICT,
                ..
            })
        ));
        drop(duplicate);
        assert!(fabric.key_is_live(ChannelKey {
            endpoint: 1,
            channel_id: 2,
        }));

        drop(first);
        assert!(!fabric.key_is_live(ChannelKey {
            endpoint: 1,
            channel_id: 2,
        }));
    }

    #[test]
    fn closed_ids_cannot_be_reused_until_both_final_frames_drain() {
        let mut fabric = fabric();
        listen(&mut fabric, 1, 2, "service");
        let accepted_id = connect(&mut fabric, 2, 4, "service");
        let closed = fabric.close(2, 4, CHANNEL_CLOSE_CANCELLED);
        assert!(fabric.key_is_live(ChannelKey {
            endpoint: 2,
            channel_id: 4,
        }));
        assert!(fabric.key_is_live(ChannelKey {
            endpoint: 1,
            channel_id: accepted_id,
        }));

        let duplicate = fabric.handle(
            2,
            ChannelRequest::Connect {
                channel_id: 4,
                name: "service",
                metadata: b"",
                listener_token: None,
            },
        );
        assert!(matches!(
            parse_channel_message(&duplicate[0].packet).unwrap(),
            Some(ChannelMessage::Opened {
                status: STATUS_CONFLICT,
                ..
            })
        ));
        drop(duplicate);
        drop(closed);
        assert!(!fabric.key_is_live(ChannelKey {
            endpoint: 2,
            channel_id: 4,
        }));
        assert!(!fabric.key_is_live(ChannelKey {
            endpoint: 1,
            channel_id: accepted_id,
        }));
    }

    #[test]
    fn disabled_family_refuses_only_decodable_opens() {
        let mut fabric = fabric();
        fabric.enabled = false;
        assert!(!fabric.advertised());

        let output = fabric.handle_packet(1, &msg_channel_listen(2, "service", b"").unwrap());
        assert!(matches!(
            parse_channel_message(&output[0].packet).unwrap(),
            Some(ChannelMessage::Opened {
                status: STATUS_PERMISSION,
                window: 0,
                ..
            })
        ));

        assert!(
            fabric
                .handle_packet(1, &[blit_remote::channel::CHANNEL, 1, 2, 0, 0, 0])
                .is_empty()
        );
        assert!(fabric.listeners.is_empty());
    }

    #[test]
    fn shutdown_seals_new_opens_and_preserves_normal_endpoint_cleanup() {
        let mut fabric = fabric();
        listen(&mut fabric, 1, 2, "existing");

        fabric.begin_shutdown();
        assert!(!fabric.advertised());
        let output = fabric.handle_packet(2, &msg_channel_listen(4, "late", b"").unwrap());
        assert!(matches!(
            parse_channel_message(&output[0].packet).unwrap(),
            Some(ChannelMessage::Opened {
                status: STATUS_PERMISSION,
                window: 0,
                ..
            })
        ));
        assert_eq!(fabric.listeners.len(), 1);

        assert!(fabric.close_endpoint(1).is_empty());
        assert!(fabric.listeners.is_empty());
    }
}
