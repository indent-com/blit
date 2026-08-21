//! Versioned structured-event wire and file codec (`blit.events.v1`).
//!
//! The family is deliberately self-contained: every remote packet uses the
//! direction-local [`EVENTS`] opcode and carries a version, kind, flags, and
//! request id. Event records have the same fixed representation on the wire and
//! in files, so a dump can be written without translating records.

use std::fmt;

/// Direction-local `blit.events.v1` envelope opcode.
pub const EVENTS: u8 = 0x96;
/// `S2C_HELLO` feature bit for this family.
pub const FEATURE_EVENTS: u32 = 1 << 31;
/// Version in every remote envelope and canonical file header.
pub const EVENTS_VERSION: u8 = 1;
/// Bytes in the common packet envelope.
pub const EVENTS_HEADER_SIZE: usize = 8;

pub const C2S_CONFIG_GET: u8 = 1;
pub const C2S_CONFIG_SET: u8 = 2;
pub const C2S_DUMP: u8 = 3;
pub const C2S_STREAM_START: u8 = 4;
pub const C2S_STREAM_STOP: u8 = 5;
pub const C2S_FILE_START: u8 = 6;
pub const C2S_FILE_STOP: u8 = 7;
/// Atomically replace the configuration only when it still matches an expected value.
pub const C2S_CONFIG_SET_IF: u8 = 8;

pub const S2C_STATUS: u8 = 0;
pub const S2C_CONFIG: u8 = 1;
pub const S2C_DUMP: u8 = 2;
pub const S2C_STREAM_STATUS: u8 = 3;
pub const S2C_STREAM_DATA: u8 = 4;
pub const S2C_FILE_STATUS: u8 = 5;

/// Continue sending newly appended records after replaying available records.
pub const STREAM_FOLLOW: u8 = 1 << 0;
pub const STREAM_FLAGS: u8 = STREAM_FOLLOW;
/// Open an existing event file for append rather than replacing it.
pub const FILE_APPEND: u8 = 1 << 0;
/// Request durable synchronization as records are written.
pub const FILE_SYNC: u8 = 1 << 1;
pub const FILE_FLAGS: u8 = FILE_APPEND | FILE_SYNC;

pub const EVENTS_RING_MIN: u32 = 1;
pub const EVENTS_RING_MAX: u32 = 1_048_576;
pub const EVENTS_DUMP_MAX_RECORDS: u32 = 65_536;
pub const EVENTS_STREAM_MAX_RECORDS: usize = 65_536;
pub const EVENTS_PATH_MAX: usize = 4096;
pub const EVENTS_DETAIL_MAX: usize = 4096;

/// Exact bytes occupied by one binary event.
pub const EVENT_RECORD_SIZE: usize = 64;
/// Exact bytes occupied by the canonical file header.
pub const EVENT_FILE_HEADER_SIZE: usize = 32;
pub const EVENT_FILE_MAGIC: [u8; 16] = *b"blit.events.v1\0\0";

/// Which event ids the server should retain. Bit `n` activates event id `n`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Activation(pub [u8; 16]);

impl Activation {
    pub const NONE: Self = Self([0; 16]);
    pub const ALL: Self = Self([0xff; 16]);

    pub fn contains(self, event_id: u8) -> bool {
        self.0[event_id as usize / 8] & (1 << (event_id % 8)) != 0
    }

    pub fn set(&mut self, event_id: u8, active: bool) {
        let byte = &mut self.0[event_id as usize / 8];
        let mask = 1 << (event_id % 8);
        if active {
            *byte |= mask;
        } else {
            *byte &= !mask;
        }
    }
}

/// Stable ids with public names. Unlisted ids remain valid activation bits and
/// are rendered numerically by clients.
pub const EVENT_NAMES: &[(u8, &str)] = &[
    (0, "server-starting"),
    (1, "server-started"),
    (2, "server-stopping"),
    (3, "server-stopped"),
    (4, "server-error"),
    (8, "client-connected"),
    (9, "client-ready"),
    (10, "client-disconnecting"),
    (11, "client-disconnected"),
    (12, "client-error"),
    (16, "raw-request-read"),
    (17, "raw-request-dispatch"),
    (18, "raw-request-done"),
    (19, "raw-request-reject"),
    (24, "writer-dequeue"),
    (25, "writer-write-begin"),
    (26, "writer-write-end"),
    (27, "writer-error"),
    (28, "writer-backpressure"),
    (32, "pty-create-request"),
    (33, "pty-create-mutex-acquired"),
    (34, "pty-create-spawn-begin"),
    (35, "pty-create-spawn-end"),
    (36, "pty-create-registered"),
    (37, "pty-create-reply-queued"),
    (38, "pty-create-error"),
    (40, "pty-read"),
    (41, "pty-queue"),
    (42, "pty-drain"),
    (43, "pty-parse"),
    (44, "pty-frame-queued"),
    (45, "pty-input"),
    (46, "pty-resize"),
    (47, "pty-exit"),
    (48, "pty-evict"),
    (49, "pty-io-error"),
    (56, "process-request"),
    (57, "process-spawn"),
    (58, "process-result"),
    (59, "process-io"),
    (60, "process-exit"),
    (61, "process-error"),
    (64, "compositor-started"),
    (65, "compositor-stopped"),
    (66, "compositor-error"),
    (68, "surface-created"),
    (69, "surface-destroyed"),
    (70, "surface-frame-queued"),
    (71, "surface-error"),
    (72, "protocol-core"),
    (73, "protocol-pty"),
    (74, "protocol-process"),
    (75, "protocol-compositor"),
    (76, "protocol-surface"),
    (77, "protocol-input"),
    (78, "protocol-clipboard"),
    (79, "protocol-filesystem"),
    (80, "protocol-network"),
    (81, "protocol-kv"),
    (82, "protocol-browser"),
    (83, "protocol-audio"),
    (84, "protocol-events"),
    (85, "protocol-integration"),
    (87, "protocol-error"),
    (104, "task-spawned"),
    (105, "task-completed"),
    (106, "task-cancelled"),
    (107, "task-failed"),
    (112, "config-changed"),
    (113, "config-error"),
    (114, "ring-dropped"),
    (115, "ring-overwritten"),
    (116, "stream-gap"),
    (117, "stream-error"),
];

pub fn event_name(event_id: u8) -> Option<&'static str> {
    EVENT_NAMES
        .iter()
        .find_map(|(id, name)| (*id == event_id).then_some(*name))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EventConfig {
    pub ring_size: u32,
    pub activation: Activation,
}

impl EventConfig {
    pub fn new(ring_size: u32, activation: Activation) -> Result<Self, EventCodecError> {
        validate_ring_size(ring_size)?;
        Ok(Self {
            ring_size,
            activation,
        })
    }
}

/// The stable 64-byte event representation shared by remote packets and files.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct EventRecord {
    pub sequence: u64,
    pub monotonic_ns: u64,
    pub event_id: u32,
    pub flags: u16,
    pub source: u8,
    pub schema: u8,
    pub connection: u64,
    pub subject: u64,
    pub args: [u64; 3],
}

impl EventRecord {
    pub fn encode(self) -> [u8; EVENT_RECORD_SIZE] {
        let mut out = [0; EVENT_RECORD_SIZE];
        out[0..8].copy_from_slice(&self.sequence.to_le_bytes());
        out[8..16].copy_from_slice(&self.monotonic_ns.to_le_bytes());
        out[16..20].copy_from_slice(&self.event_id.to_le_bytes());
        out[20..22].copy_from_slice(&self.flags.to_le_bytes());
        out[22] = self.source;
        out[23] = self.schema;
        out[24..32].copy_from_slice(&self.connection.to_le_bytes());
        out[32..40].copy_from_slice(&self.subject.to_le_bytes());
        out[40..48].copy_from_slice(&self.args[0].to_le_bytes());
        out[48..56].copy_from_slice(&self.args[1].to_le_bytes());
        out[56..64].copy_from_slice(&self.args[2].to_le_bytes());
        out
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, EventCodecError> {
        if bytes.len() != EVENT_RECORD_SIZE {
            return Err(EventCodecError::invalid(None));
        }
        Ok(Self {
            sequence: le_u64(&bytes[0..8]),
            monotonic_ns: le_u64(&bytes[8..16]),
            event_id: le_u32(&bytes[16..20]),
            flags: le_u16(&bytes[20..22]),
            source: bytes[22],
            schema: bytes[23],
            connection: le_u64(&bytes[24..32]),
            subject: le_u64(&bytes[32..40]),
            args: [
                le_u64(&bytes[40..48]),
                le_u64(&bytes[48..56]),
                le_u64(&bytes[56..64]),
            ],
        })
    }
}

/// Canonical prefix for a file containing consecutive [`EventRecord`] bytes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EventFileHeader;

impl EventFileHeader {
    pub const CANONICAL: Self = Self;

    pub fn encode(self) -> [u8; EVENT_FILE_HEADER_SIZE] {
        let mut out = [0; EVENT_FILE_HEADER_SIZE];
        out[..16].copy_from_slice(&EVENT_FILE_MAGIC);
        out[16] = EVENTS_VERSION;
        out[18..20].copy_from_slice(&(EVENT_FILE_HEADER_SIZE as u16).to_le_bytes());
        out[20..22].copy_from_slice(&(EVENT_RECORD_SIZE as u16).to_le_bytes());
        out
    }

    /// Parse the prefix of a file. Reserved bytes must be zero so there is only
    /// one valid v1 header representation.
    pub fn decode(bytes: &[u8]) -> Result<Self, EventCodecError> {
        if bytes.len() < EVENT_FILE_HEADER_SIZE {
            return Err(EventCodecError::invalid(None));
        }
        if bytes[..16] != EVENT_FILE_MAGIC
            || bytes[16] != EVENTS_VERSION
            || bytes[17] != 0
            || le_u16(&bytes[18..20]) as usize != EVENT_FILE_HEADER_SIZE
            || le_u16(&bytes[20..22]) as usize != EVENT_RECORD_SIZE
            || bytes[22..EVENT_FILE_HEADER_SIZE]
                .iter()
                .any(|byte| *byte != 0)
        {
            return Err(EventCodecError::invalid(None));
        }
        Ok(Self)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EventRequest<'a> {
    ConfigGet {
        request_id: u32,
    },
    ConfigSet {
        request_id: u32,
        config: EventConfig,
    },
    ConfigSetIf {
        request_id: u32,
        expected: EventConfig,
        config: EventConfig,
    },
    Dump {
        request_id: u32,
        from_sequence: u64,
        limit: u32,
    },
    StreamStart {
        request_id: u32,
        stream_id: u32,
        from_sequence: u64,
        flags: u8,
    },
    StreamStop {
        request_id: u32,
        stream_id: u32,
    },
    FileStart {
        request_id: u32,
        stream_id: u32,
        flags: u8,
        path: &'a str,
    },
    FileStop {
        request_id: u32,
        stream_id: u32,
    },
}

impl EventRequest<'_> {
    pub fn request_id(&self) -> u32 {
        match self {
            Self::ConfigGet { request_id }
            | Self::ConfigSet { request_id, .. }
            | Self::ConfigSetIf { request_id, .. }
            | Self::Dump { request_id, .. }
            | Self::StreamStart { request_id, .. }
            | Self::StreamStop { request_id, .. }
            | Self::FileStart { request_id, .. }
            | Self::FileStop { request_id, .. } => *request_id,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EventMessage {
    Status {
        request_id: u32,
        request_kind: u8,
        status: u8,
    },
    Config {
        request_id: u32,
        status: u8,
        config: EventConfig,
    },
    Dump {
        request_id: u32,
        status: u8,
        first_sequence: u64,
        next_sequence: u64,
        records: Vec<EventRecord>,
    },
    StreamStatus {
        request_id: u32,
        status: u8,
        stream_id: u32,
        next_sequence: u64,
    },
    StreamData {
        stream_id: u32,
        records: Vec<EventRecord>,
    },
    FileStatus {
        request_id: u32,
        status: u8,
        stream_id: u32,
        records_written: u64,
        bytes_written: u64,
        detail: String,
    },
}

/// A bounded decode failure. Once the eight-byte envelope is present,
/// `request_id` is retained so dispatch can always send a correlated status.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EventCodecError {
    pub kind: EventCodecErrorKind,
    pub request_id: Option<u32>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EventCodecErrorKind {
    NotEvents,
    Truncated,
    UnsupportedVersion,
    UnknownKind,
    InvalidFlags,
    Invalid,
    TooLarge,
    InvalidUtf8,
}

impl EventCodecError {
    const fn new(kind: EventCodecErrorKind, request_id: Option<u32>) -> Self {
        Self { kind, request_id }
    }

    const fn invalid(request_id: Option<u32>) -> Self {
        Self::new(EventCodecErrorKind::Invalid, request_id)
    }

    pub fn status(self) -> u8 {
        match self.kind {
            EventCodecErrorKind::TooLarge => crate::STATUS_TOO_LARGE,
            _ => crate::STATUS_INVALID,
        }
    }

    /// Build the correlated generic reply available for every malformed request
    /// whose envelope was complete.
    pub fn status_reply(self, request_kind: u8) -> Option<Vec<u8>> {
        Some(msg_event_status(
            self.request_id?,
            request_kind,
            self.status(),
        ))
    }
}

impl fmt::Display for EventCodecError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self.kind {
            EventCodecErrorKind::NotEvents => "not an events packet",
            EventCodecErrorKind::Truncated => "events packet is truncated",
            EventCodecErrorKind::UnsupportedVersion => "unsupported events version",
            EventCodecErrorKind::UnknownKind => "unknown events kind",
            EventCodecErrorKind::InvalidFlags => "invalid events flags",
            EventCodecErrorKind::Invalid => "invalid events packet",
            EventCodecErrorKind::TooLarge => "events field exceeds its limit",
            EventCodecErrorKind::InvalidUtf8 => "events text is not valid UTF-8",
        })
    }
}

pub fn events_header(packet: &[u8]) -> Result<(u8, u32, &[u8]), EventCodecError> {
    if packet.first() != Some(&EVENTS) {
        return Err(EventCodecError::new(EventCodecErrorKind::NotEvents, None));
    }
    if packet.len() < EVENTS_HEADER_SIZE {
        return Err(EventCodecError::new(EventCodecErrorKind::Truncated, None));
    }
    let request_id = le_u32(&packet[4..8]);
    if packet[1] != EVENTS_VERSION {
        return Err(EventCodecError::new(
            EventCodecErrorKind::UnsupportedVersion,
            Some(request_id),
        ));
    }
    if packet[3] != 0 {
        return Err(EventCodecError::new(
            EventCodecErrorKind::InvalidFlags,
            Some(request_id),
        ));
    }
    Ok((packet[2], request_id, &packet[8..]))
}

pub fn parse_event_request(packet: &[u8]) -> Result<EventRequest<'_>, EventCodecError> {
    let (kind, request_id, body) = events_header(packet)?;
    let invalid = || EventCodecError::invalid(Some(request_id));
    match kind {
        C2S_CONFIG_GET if body.is_empty() => Ok(EventRequest::ConfigGet { request_id }),
        C2S_CONFIG_GET => Err(invalid()),
        C2S_CONFIG_SET => {
            exact(body, 20, request_id)?;
            let config = parse_config(body, request_id)?;
            Ok(EventRequest::ConfigSet { request_id, config })
        }
        C2S_CONFIG_SET_IF => {
            exact(body, 40, request_id)?;
            Ok(EventRequest::ConfigSetIf {
                request_id,
                expected: parse_config(&body[..20], request_id)?,
                config: parse_config(&body[20..], request_id)?,
            })
        }
        C2S_DUMP => {
            exact(body, 12, request_id)?;
            let limit = le_u32(&body[8..12]);
            validate_limit(limit, request_id)?;
            Ok(EventRequest::Dump {
                request_id,
                from_sequence: le_u64(&body[..8]),
                limit,
            })
        }
        C2S_STREAM_START => {
            exact(body, 13, request_id)?;
            let flags = body[12];
            if flags & !STREAM_FLAGS != 0 {
                return Err(EventCodecError::new(
                    EventCodecErrorKind::InvalidFlags,
                    Some(request_id),
                ));
            }
            Ok(EventRequest::StreamStart {
                request_id,
                stream_id: le_u32(&body[..4]),
                from_sequence: le_u64(&body[4..12]),
                flags,
            })
        }
        C2S_STREAM_STOP | C2S_FILE_STOP => {
            exact(body, 4, request_id)?;
            let stream_id = le_u32(body);
            if kind == C2S_STREAM_STOP {
                Ok(EventRequest::StreamStop {
                    request_id,
                    stream_id,
                })
            } else {
                Ok(EventRequest::FileStop {
                    request_id,
                    stream_id,
                })
            }
        }
        C2S_FILE_START => {
            if body.len() < 7 {
                return Err(EventCodecError::new(
                    EventCodecErrorKind::Truncated,
                    Some(request_id),
                ));
            }
            let stream_id = le_u32(&body[..4]);
            let flags = body[4];
            if flags & !FILE_FLAGS != 0 {
                return Err(EventCodecError::new(
                    EventCodecErrorKind::InvalidFlags,
                    Some(request_id),
                ));
            }
            let path_len = le_u16(&body[5..7]) as usize;
            if path_len == 0 {
                return Err(invalid());
            }
            if path_len > EVENTS_PATH_MAX {
                return Err(EventCodecError::new(
                    EventCodecErrorKind::TooLarge,
                    Some(request_id),
                ));
            }
            if body.len() < 7 + path_len {
                return Err(EventCodecError::new(
                    EventCodecErrorKind::Truncated,
                    Some(request_id),
                ));
            }
            if body.len() != 7 + path_len {
                return Err(invalid());
            }
            let path = std::str::from_utf8(&body[7..]).map_err(|_| {
                EventCodecError::new(EventCodecErrorKind::InvalidUtf8, Some(request_id))
            })?;
            if path.as_bytes().contains(&0) {
                return Err(invalid());
            }
            Ok(EventRequest::FileStart {
                request_id,
                stream_id,
                flags,
                path,
            })
        }
        _ => Err(EventCodecError::new(
            EventCodecErrorKind::UnknownKind,
            Some(request_id),
        )),
    }
}

pub fn parse_event_message(packet: &[u8]) -> Result<EventMessage, EventCodecError> {
    let (kind, request_id, body) = events_header(packet)?;
    match kind {
        S2C_STATUS => {
            exact(body, 2, request_id)?;
            Ok(EventMessage::Status {
                request_id,
                request_kind: body[0],
                status: body[1],
            })
        }
        S2C_CONFIG => {
            exact(body, 21, request_id)?;
            let config = EventConfig::new(
                le_u32(&body[1..5]),
                Activation(body[5..21].try_into().expect("checked length")),
            )
            .map_err(|mut error| {
                error.request_id = Some(request_id);
                error
            })?;
            Ok(EventMessage::Config {
                request_id,
                status: body[0],
                config,
            })
        }
        S2C_DUMP => {
            if body.len() < 21 {
                return Err(EventCodecError::new(
                    EventCodecErrorKind::Truncated,
                    Some(request_id),
                ));
            }
            let count = le_u32(&body[17..21]);
            if count > EVENTS_DUMP_MAX_RECORDS {
                return Err(EventCodecError::new(
                    EventCodecErrorKind::TooLarge,
                    Some(request_id),
                ));
            }
            let records = decode_records(&body[21..], count as usize, request_id)?;
            Ok(EventMessage::Dump {
                request_id,
                status: body[0],
                first_sequence: le_u64(&body[1..9]),
                next_sequence: le_u64(&body[9..17]),
                records,
            })
        }
        S2C_STREAM_STATUS => {
            exact(body, 13, request_id)?;
            Ok(EventMessage::StreamStatus {
                request_id,
                status: body[0],
                stream_id: le_u32(&body[1..5]),
                next_sequence: le_u64(&body[5..13]),
            })
        }
        S2C_STREAM_DATA => {
            if request_id != 0 {
                return Err(EventCodecError::invalid(Some(request_id)));
            }
            if body.len() < 8 {
                return Err(EventCodecError::new(
                    EventCodecErrorKind::Truncated,
                    Some(0),
                ));
            }
            let count = le_u32(&body[4..8]) as usize;
            if count > EVENTS_STREAM_MAX_RECORDS {
                return Err(EventCodecError::new(EventCodecErrorKind::TooLarge, Some(0)));
            }
            Ok(EventMessage::StreamData {
                stream_id: le_u32(&body[..4]),
                records: decode_records(&body[8..], count, 0)?,
            })
        }
        S2C_FILE_STATUS => {
            if body.len() < 23 {
                return Err(EventCodecError::new(
                    EventCodecErrorKind::Truncated,
                    Some(request_id),
                ));
            }
            let detail_len = le_u16(&body[21..23]) as usize;
            if detail_len > EVENTS_DETAIL_MAX {
                return Err(EventCodecError::new(
                    EventCodecErrorKind::TooLarge,
                    Some(request_id),
                ));
            }
            if body.len() < 23 + detail_len {
                return Err(EventCodecError::new(
                    EventCodecErrorKind::Truncated,
                    Some(request_id),
                ));
            }
            if body.len() != 23 + detail_len {
                return Err(EventCodecError::invalid(Some(request_id)));
            }
            let detail = std::str::from_utf8(&body[23..])
                .map_err(|_| {
                    EventCodecError::new(EventCodecErrorKind::InvalidUtf8, Some(request_id))
                })?
                .to_owned();
            Ok(EventMessage::FileStatus {
                request_id,
                status: body[0],
                stream_id: le_u32(&body[1..5]),
                records_written: le_u64(&body[5..13]),
                bytes_written: le_u64(&body[13..21]),
                detail,
            })
        }
        _ => Err(EventCodecError::new(
            EventCodecErrorKind::UnknownKind,
            Some(request_id),
        )),
    }
}

pub fn msg_config_get(request_id: u32) -> Vec<u8> {
    envelope(C2S_CONFIG_GET, request_id, 0)
}

pub fn msg_config_set(request_id: u32, config: EventConfig) -> Result<Vec<u8>, EventCodecError> {
    validate_ring_size(config.ring_size)?;
    let mut msg = envelope(C2S_CONFIG_SET, request_id, 20);
    push_config(&mut msg, config);
    Ok(msg)
}

pub fn msg_config_set_if(
    request_id: u32,
    expected: EventConfig,
    config: EventConfig,
) -> Result<Vec<u8>, EventCodecError> {
    validate_ring_size(expected.ring_size)?;
    validate_ring_size(config.ring_size)?;
    let mut msg = envelope(C2S_CONFIG_SET_IF, request_id, 40);
    push_config(&mut msg, expected);
    push_config(&mut msg, config);
    Ok(msg)
}

pub fn msg_dump(
    request_id: u32,
    from_sequence: u64,
    limit: u32,
) -> Result<Vec<u8>, EventCodecError> {
    validate_limit(limit, request_id)?;
    let mut msg = envelope(C2S_DUMP, request_id, 12);
    msg.extend_from_slice(&from_sequence.to_le_bytes());
    msg.extend_from_slice(&limit.to_le_bytes());
    Ok(msg)
}

pub fn msg_stream_start(
    request_id: u32,
    stream_id: u32,
    from_sequence: u64,
    flags: u8,
) -> Result<Vec<u8>, EventCodecError> {
    if flags & !STREAM_FLAGS != 0 {
        return Err(EventCodecError::new(
            EventCodecErrorKind::InvalidFlags,
            Some(request_id),
        ));
    }
    let mut msg = envelope(C2S_STREAM_START, request_id, 13);
    msg.extend_from_slice(&stream_id.to_le_bytes());
    msg.extend_from_slice(&from_sequence.to_le_bytes());
    msg.push(flags);
    Ok(msg)
}

pub fn msg_stream_stop(request_id: u32, stream_id: u32) -> Vec<u8> {
    id_request(C2S_STREAM_STOP, request_id, stream_id)
}

pub fn msg_file_start(
    request_id: u32,
    stream_id: u32,
    flags: u8,
    path: &str,
) -> Result<Vec<u8>, EventCodecError> {
    if flags & !FILE_FLAGS != 0 {
        return Err(EventCodecError::new(
            EventCodecErrorKind::InvalidFlags,
            Some(request_id),
        ));
    }
    if path.is_empty() || path.as_bytes().contains(&0) {
        return Err(EventCodecError::invalid(Some(request_id)));
    }
    if path.len() > EVENTS_PATH_MAX {
        return Err(EventCodecError::new(
            EventCodecErrorKind::TooLarge,
            Some(request_id),
        ));
    }
    let mut msg = envelope(C2S_FILE_START, request_id, 7 + path.len());
    msg.extend_from_slice(&stream_id.to_le_bytes());
    msg.push(flags);
    msg.extend_from_slice(&(path.len() as u16).to_le_bytes());
    msg.extend_from_slice(path.as_bytes());
    Ok(msg)
}

pub fn msg_file_stop(request_id: u32, stream_id: u32) -> Vec<u8> {
    id_request(C2S_FILE_STOP, request_id, stream_id)
}

pub fn msg_event_status(request_id: u32, request_kind: u8, status: u8) -> Vec<u8> {
    let mut msg = envelope(S2C_STATUS, request_id, 2);
    msg.extend_from_slice(&[request_kind, status]);
    msg
}

pub fn msg_event_config(
    request_id: u32,
    status: u8,
    config: EventConfig,
) -> Result<Vec<u8>, EventCodecError> {
    validate_ring_size(config.ring_size)?;
    let mut msg = envelope(S2C_CONFIG, request_id, 21);
    msg.push(status);
    msg.extend_from_slice(&config.ring_size.to_le_bytes());
    msg.extend_from_slice(&config.activation.0);
    Ok(msg)
}

pub fn msg_event_dump(
    request_id: u32,
    status: u8,
    first_sequence: u64,
    next_sequence: u64,
    records: &[EventRecord],
) -> Result<Vec<u8>, EventCodecError> {
    if records.len() > EVENTS_DUMP_MAX_RECORDS as usize {
        return Err(EventCodecError::new(
            EventCodecErrorKind::TooLarge,
            Some(request_id),
        ));
    }
    let mut msg = envelope(S2C_DUMP, request_id, 21 + records.len() * EVENT_RECORD_SIZE);
    msg.push(status);
    msg.extend_from_slice(&first_sequence.to_le_bytes());
    msg.extend_from_slice(&next_sequence.to_le_bytes());
    msg.extend_from_slice(&(records.len() as u32).to_le_bytes());
    push_records(&mut msg, records);
    Ok(msg)
}

pub fn msg_event_stream_status(
    request_id: u32,
    status: u8,
    stream_id: u32,
    next_sequence: u64,
) -> Vec<u8> {
    let mut msg = envelope(S2C_STREAM_STATUS, request_id, 13);
    msg.push(status);
    msg.extend_from_slice(&stream_id.to_le_bytes());
    msg.extend_from_slice(&next_sequence.to_le_bytes());
    msg
}

pub fn msg_event_stream_data(
    stream_id: u32,
    records: &[EventRecord],
) -> Result<Vec<u8>, EventCodecError> {
    if records.len() > EVENTS_STREAM_MAX_RECORDS {
        return Err(EventCodecError::new(EventCodecErrorKind::TooLarge, None));
    }
    let mut msg = envelope(S2C_STREAM_DATA, 0, 8 + records.len() * EVENT_RECORD_SIZE);
    msg.extend_from_slice(&stream_id.to_le_bytes());
    msg.extend_from_slice(&(records.len() as u32).to_le_bytes());
    push_records(&mut msg, records);
    Ok(msg)
}

pub fn msg_event_file_status(
    request_id: u32,
    status: u8,
    stream_id: u32,
    records_written: u64,
    bytes_written: u64,
    detail: &str,
) -> Result<Vec<u8>, EventCodecError> {
    if detail.len() > EVENTS_DETAIL_MAX {
        return Err(EventCodecError::new(
            EventCodecErrorKind::TooLarge,
            Some(request_id),
        ));
    }
    let mut msg = envelope(S2C_FILE_STATUS, request_id, 23 + detail.len());
    msg.push(status);
    msg.extend_from_slice(&stream_id.to_le_bytes());
    msg.extend_from_slice(&records_written.to_le_bytes());
    msg.extend_from_slice(&bytes_written.to_le_bytes());
    msg.extend_from_slice(&(detail.len() as u16).to_le_bytes());
    msg.extend_from_slice(detail.as_bytes());
    Ok(msg)
}

fn envelope(kind: u8, request_id: u32, body_len: usize) -> Vec<u8> {
    let mut msg = Vec::with_capacity(EVENTS_HEADER_SIZE + body_len);
    msg.extend_from_slice(&[EVENTS, EVENTS_VERSION, kind, 0]);
    msg.extend_from_slice(&request_id.to_le_bytes());
    msg
}

fn id_request(kind: u8, request_id: u32, stream_id: u32) -> Vec<u8> {
    let mut msg = envelope(kind, request_id, 4);
    msg.extend_from_slice(&stream_id.to_le_bytes());
    msg
}

fn parse_config(body: &[u8], request_id: u32) -> Result<EventConfig, EventCodecError> {
    EventConfig::new(
        le_u32(&body[..4]),
        Activation(body[4..20].try_into().expect("checked config length")),
    )
    .map_err(|mut error| {
        error.request_id = Some(request_id);
        error
    })
}

fn validate_ring_size(ring_size: u32) -> Result<(), EventCodecError> {
    if !(EVENTS_RING_MIN..=EVENTS_RING_MAX).contains(&ring_size) {
        return Err(EventCodecError::new(EventCodecErrorKind::TooLarge, None));
    }
    Ok(())
}

fn validate_limit(limit: u32, request_id: u32) -> Result<(), EventCodecError> {
    if limit == 0 || limit > EVENTS_DUMP_MAX_RECORDS {
        return Err(EventCodecError::new(
            EventCodecErrorKind::TooLarge,
            Some(request_id),
        ));
    }
    Ok(())
}

fn exact(body: &[u8], len: usize, request_id: u32) -> Result<(), EventCodecError> {
    if body.len() < len {
        Err(EventCodecError::new(
            EventCodecErrorKind::Truncated,
            Some(request_id),
        ))
    } else if body.len() > len {
        Err(EventCodecError::invalid(Some(request_id)))
    } else {
        Ok(())
    }
}

fn decode_records(
    bytes: &[u8],
    count: usize,
    request_id: u32,
) -> Result<Vec<EventRecord>, EventCodecError> {
    let expected = count
        .checked_mul(EVENT_RECORD_SIZE)
        .ok_or_else(|| EventCodecError::new(EventCodecErrorKind::TooLarge, Some(request_id)))?;
    if bytes.len() < expected {
        return Err(EventCodecError::new(
            EventCodecErrorKind::Truncated,
            Some(request_id),
        ));
    }
    if bytes.len() > expected {
        return Err(EventCodecError::invalid(Some(request_id)));
    }
    bytes
        .chunks_exact(EVENT_RECORD_SIZE)
        .map(EventRecord::decode)
        .collect()
}

fn push_config(out: &mut Vec<u8>, config: EventConfig) {
    out.extend_from_slice(&config.ring_size.to_le_bytes());
    out.extend_from_slice(&config.activation.0);
}

fn push_records(out: &mut Vec<u8>, records: &[EventRecord]) {
    for record in records {
        out.extend_from_slice(&record.encode());
    }
}

fn le_u16(bytes: &[u8]) -> u16 {
    u16::from_le_bytes(bytes.try_into().expect("fixed field"))
}

fn le_u32(bytes: &[u8]) -> u32 {
    u32::from_le_bytes(bytes.try_into().expect("fixed field"))
}

fn le_u64(bytes: &[u8]) -> u64 {
    u64::from_le_bytes(bytes.try_into().expect("fixed field"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record() -> EventRecord {
        EventRecord {
            sequence: 0x0102_0304_0506_0708,
            monotonic_ns: 0x1112_1314_1516_1718,
            event_id: 0x2122_2324,
            flags: 0x3132,
            source: 0x41,
            schema: 0x42,
            connection: 0x5152_5354_5556_5758,
            subject: 0x6162_6364_6566_6768,
            args: [
                0x7172_7374_7576_7778,
                0x8182_8384_8586_8788,
                0x9192_9394_9596_9798,
            ],
        }
    }

    fn config() -> EventConfig {
        EventConfig::new(4096, Activation([0x5a; 16])).unwrap()
    }

    #[test]
    fn allocations_are_locked() {
        assert_eq!(EVENTS, 0x96);
        assert_eq!(FEATURE_EVENTS, 1 << 31);
        assert_eq!(EVENTS_VERSION, 1);
    }

    #[test]
    fn event_record_has_fixed_size_and_golden_bytes() {
        assert_eq!(std::mem::size_of::<EventRecord>(), EVENT_RECORD_SIZE);
        let bytes = record().encode();
        assert_eq!(bytes.len(), 64);
        assert_eq!(
            bytes,
            [
                8, 7, 6, 5, 4, 3, 2, 1, 24, 23, 22, 21, 20, 19, 18, 17, 36, 35, 34, 33, 50, 49, 65,
                66, 88, 87, 86, 85, 84, 83, 82, 81, 104, 103, 102, 101, 100, 99, 98, 97, 120, 119,
                118, 117, 116, 115, 114, 113, 136, 135, 134, 133, 132, 131, 130, 129, 152, 151,
                150, 149, 148, 147, 146, 145,
            ]
        );
        assert_eq!(EventRecord::decode(&bytes), Ok(record()));
        assert!(EventRecord::decode(&bytes[..63]).is_err());
        assert!(EventRecord::decode(&[0; 65]).is_err());
    }

    #[test]
    fn canonical_file_header_is_golden_and_strict() {
        let header = EventFileHeader::CANONICAL.encode();
        assert_eq!(header.len(), EVENT_FILE_HEADER_SIZE);
        assert_eq!(
            header,
            [
                b'b', b'l', b'i', b't', b'.', b'e', b'v', b'e', b'n', b't', b's', b'.', b'v', b'1',
                0, 0, 1, 0, 32, 0, 64, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
            ]
        );
        assert_eq!(EventFileHeader::decode(&header), Ok(EventFileHeader));
        let mut file = header.to_vec();
        file.extend_from_slice(&record().encode());
        assert_eq!(EventFileHeader::decode(&file), Ok(EventFileHeader));
        for cut in 0..EVENT_FILE_HEADER_SIZE {
            assert!(EventFileHeader::decode(&header[..cut]).is_err());
        }
        for index in [0, 16, 17, 18, 20, 31] {
            let mut bad = header;
            bad[index] ^= 1;
            assert!(EventFileHeader::decode(&bad).is_err(), "byte {index}");
        }
    }

    #[test]
    fn activation_is_a_128_bit_set() {
        let mut activation = Activation::NONE;
        for id in [0, 7, 8, 127] {
            activation.set(id, true);
            assert!(activation.contains(id));
        }
        activation.set(8, false);
        assert!(!activation.contains(8));
        assert!(activation.contains(127));
    }

    #[test]
    fn request_golden_bytes_and_round_trips() {
        assert_eq!(
            msg_config_get(0x1234_5678),
            vec![0x96, 1, 1, 0, 0x78, 0x56, 0x34, 0x12]
        );
        let set = msg_config_set(7, config()).unwrap();
        assert_eq!(
            parse_event_request(&set),
            Ok(EventRequest::ConfigSet {
                request_id: 7,
                config: config()
            })
        );
        let conditional = msg_config_set_if(
            8,
            config(),
            EventConfig::new(8192, Activation::ALL).unwrap(),
        )
        .unwrap();
        assert_eq!(
            parse_event_request(&conditional),
            Ok(EventRequest::ConfigSetIf {
                request_id: 8,
                expected: config(),
                config: EventConfig::new(8192, Activation::ALL).unwrap(),
            })
        );
        let dump = msg_dump(8, 99, 12).unwrap();
        assert_eq!(
            parse_event_request(&dump),
            Ok(EventRequest::Dump {
                request_id: 8,
                from_sequence: 99,
                limit: 12
            })
        );
        let start = msg_stream_start(9, 22, 100, STREAM_FOLLOW).unwrap();
        assert_eq!(
            parse_event_request(&start),
            Ok(EventRequest::StreamStart {
                request_id: 9,
                stream_id: 22,
                from_sequence: 100,
                flags: STREAM_FOLLOW
            })
        );
        assert_eq!(
            parse_event_request(&msg_stream_stop(10, 22)),
            Ok(EventRequest::StreamStop {
                request_id: 10,
                stream_id: 22
            })
        );
        let file = msg_file_start(11, 23, FILE_APPEND, "/tmp/blit.events").unwrap();
        assert_eq!(
            parse_event_request(&file),
            Ok(EventRequest::FileStart {
                request_id: 11,
                stream_id: 23,
                flags: FILE_APPEND,
                path: "/tmp/blit.events"
            })
        );
        assert_eq!(
            parse_event_request(&msg_file_stop(12, 23)),
            Ok(EventRequest::FileStop {
                request_id: 12,
                stream_id: 23
            })
        );
    }

    #[test]
    fn replies_round_trip() {
        let cases = [
            msg_event_status(1, C2S_CONFIG_SET, crate::STATUS_INVALID),
            msg_event_config(2, crate::STATUS_OK, config()).unwrap(),
            msg_event_dump(3, crate::STATUS_OK, 4, 6, &[record()]).unwrap(),
            msg_event_stream_status(4, crate::STATUS_OK, 5, 6),
            msg_event_stream_data(5, &[record(), record()]).unwrap(),
            msg_event_file_status(6, crate::STATUS_OTHER, 7, 8, 512, "disk full").unwrap(),
        ];
        assert!(matches!(
            parse_event_message(&cases[0]),
            Ok(EventMessage::Status { .. })
        ));
        assert!(matches!(
            parse_event_message(&cases[1]),
            Ok(EventMessage::Config { .. })
        ));
        assert!(
            matches!(parse_event_message(&cases[2]), Ok(EventMessage::Dump { records, .. }) if records == vec![record()])
        );
        assert!(matches!(
            parse_event_message(&cases[3]),
            Ok(EventMessage::StreamStatus { .. })
        ));
        assert!(
            matches!(parse_event_message(&cases[4]), Ok(EventMessage::StreamData { records, .. }) if records.len() == 2)
        );
        assert!(
            matches!(parse_event_message(&cases[5]), Ok(EventMessage::FileStatus { detail, .. }) if detail == "disk full")
        );
    }

    #[test]
    fn every_truncated_known_packet_is_rejected() {
        let requests = [
            msg_config_set(7, config()).unwrap(),
            msg_config_set_if(
                7,
                config(),
                EventConfig::new(8192, Activation::ALL).unwrap(),
            )
            .unwrap(),
            msg_dump(7, 1, 1).unwrap(),
            msg_stream_start(7, 1, 1, 0).unwrap(),
            msg_stream_stop(7, 1),
            msg_file_start(7, 1, 0, "/tmp/x").unwrap(),
            msg_file_stop(7, 1),
        ];
        for packet in requests {
            for cut in 0..packet.len() {
                assert!(
                    parse_event_request(&packet[..cut]).is_err(),
                    "request cut {cut}"
                );
            }
        }
        let replies = [
            msg_event_config(7, 0, config()).unwrap(),
            msg_event_dump(7, 0, 1, 2, &[record()]).unwrap(),
            msg_event_stream_status(7, 0, 1, 2),
            msg_event_stream_data(1, &[record()]).unwrap(),
            msg_event_file_status(7, 0, 1, 2, 128, "ok").unwrap(),
        ];
        for packet in replies {
            for cut in 0..packet.len() {
                assert!(
                    parse_event_message(&packet[..cut]).is_err(),
                    "reply cut {cut}"
                );
            }
        }
    }

    #[test]
    fn unknown_versions_kinds_and_flags_are_rejected_with_correlation() {
        for (index, value, kind) in [
            (1, 2, EventCodecErrorKind::UnsupportedVersion),
            (2, 0xff, EventCodecErrorKind::UnknownKind),
            (3, 1, EventCodecErrorKind::InvalidFlags),
        ] {
            let mut packet = msg_config_get(0x4433_2211);
            packet[index] = value;
            let error = parse_event_request(&packet).unwrap_err();
            assert_eq!(error.kind, kind);
            assert_eq!(error.request_id, Some(0x4433_2211));
            let reply = error.status_reply(packet[2]).unwrap();
            assert!(matches!(
                parse_event_message(&reply),
                Ok(EventMessage::Status {
                    request_id: 0x4433_2211,
                    status: crate::STATUS_INVALID,
                    ..
                })
            ));
        }
    }

    #[test]
    fn operation_flags_and_trailing_bytes_are_rejected() {
        assert!(matches!(
            msg_stream_start(1, 1, 1, 0x80).unwrap_err().kind,
            EventCodecErrorKind::InvalidFlags
        ));
        assert!(matches!(
            msg_file_start(1, 1, 0x80, "x").unwrap_err().kind,
            EventCodecErrorKind::InvalidFlags
        ));
        let mut packet = msg_config_get(1);
        packet.push(0);
        assert!(parse_event_request(&packet).is_err());
        let mut stream = msg_stream_start(1, 1, 1, 0).unwrap();
        stream[20] = 0x80;
        assert_eq!(
            parse_event_request(&stream).unwrap_err().kind,
            EventCodecErrorKind::InvalidFlags
        );
    }

    #[test]
    fn oversized_paths_and_counts_are_bounded() {
        let path = "x".repeat(EVENTS_PATH_MAX + 1);
        assert_eq!(
            msg_file_start(9, 1, 0, &path).unwrap_err().kind,
            EventCodecErrorKind::TooLarge
        );

        let mut packet = envelope(C2S_FILE_START, 9, 7);
        packet.extend_from_slice(&1u32.to_le_bytes());
        packet.push(0);
        packet.extend_from_slice(&((EVENTS_PATH_MAX + 1) as u16).to_le_bytes());
        assert_eq!(
            parse_event_request(&packet).unwrap_err().kind,
            EventCodecErrorKind::TooLarge
        );
        assert_eq!(
            msg_dump(9, 0, EVENTS_DUMP_MAX_RECORDS + 1)
                .unwrap_err()
                .kind,
            EventCodecErrorKind::TooLarge
        );
        assert!(EventConfig::new(EVENTS_RING_MAX + 1, Activation::ALL).is_err());
    }

    #[test]
    fn record_count_cannot_claim_past_packet() {
        let mut dump = msg_event_dump(4, 0, 1, 2, &[record()]).unwrap();
        dump[25..29].copy_from_slice(&2u32.to_le_bytes());
        assert_eq!(
            parse_event_message(&dump).unwrap_err().kind,
            EventCodecErrorKind::Truncated
        );

        let mut stream = msg_event_stream_data(3, &[record()]).unwrap();
        stream[12..16].copy_from_slice(&(EVENTS_STREAM_MAX_RECORDS as u32 + 1).to_le_bytes());
        assert_eq!(
            parse_event_message(&stream).unwrap_err().kind,
            EventCodecErrorKind::TooLarge
        );
    }

    #[test]
    fn paths_are_utf8_nonempty_and_nul_free() {
        assert!(msg_file_start(1, 1, 0, "").is_err());
        assert!(msg_file_start(1, 1, 0, "a\0b").is_err());
        let mut packet = msg_file_start(1, 1, 0, "x").unwrap();
        *packet.last_mut().unwrap() = 0xff;
        assert_eq!(
            parse_event_request(&packet).unwrap_err().kind,
            EventCodecErrorKind::InvalidUtf8
        );
    }
}
