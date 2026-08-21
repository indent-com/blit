//! Binary server event journal wire protocol (`blit.events.v1`).
//!
//! The family uses one direction-local envelope opcode. Every request is
//! correlated; live records use a stream id after the correlated start.

use std::fmt;

/// `S2C_HELLO` feature bit for `blit.events.v1`.
pub const FEATURE_EVENTS: u32 = 1 << 31;
/// Direction-local family envelope opcode.
pub const EVENTS: u8 = 0xD0;
pub const EVENTS_VERSION: u8 = 1;
pub const EVENTS_PROTOCOL: &str = "blit.events.v1";

pub const ACTIVATION_WORDS: usize = 4;
pub const ACTIVATION_BYTES: usize = ACTIVATION_WORDS * 8;

pub const EVENTS_CONFIG_GET: u8 = 1;
pub const EVENTS_CONFIG_SET: u8 = 2;
pub const EVENTS_DUMP: u8 = 3;
pub const EVENTS_STREAM_START: u8 = 4;
pub const EVENTS_STREAM_STOP: u8 = 5;
pub const EVENTS_STREAM_LIST: u8 = 6;

pub const EVENTS_CONFIG: u8 = 1;
pub const EVENTS_RESULT: u8 = 2;
pub const EVENTS_DUMPED: u8 = 3;
pub const EVENTS_STREAM_STARTED: u8 = 4;
pub const EVENTS_RECORD: u8 = 5;
pub const EVENTS_STREAM_STOPPED: u8 = 6;
pub const EVENTS_STREAM_GAP: u8 = 7;
pub const EVENTS_STREAMS: u8 = 8;

pub const EVENTS_TARGET_CLIENT: u8 = 0;
pub const EVENTS_TARGET_FILE: u8 = 1;

/// Start a stream with the current retained history before live records.
pub const EVENTS_STREAM_HISTORY: u8 = 1 << 0;
/// Open a file target for append instead of truncating it.
pub const EVENTS_STREAM_APPEND: u8 = 1 << 1;
pub const EVENTS_STREAM_FLAGS: u8 = EVENTS_STREAM_HISTORY | EVENTS_STREAM_APPEND;

/// Magic at the front of every self-describing binary dump.
pub const EVENT_DUMP_MAGIC: &[u8; 8] = b"BLITEVT1";
pub const EVENT_DUMP_HEADER_LEN: usize = 84;
/// Bytes before an event's type-specific binary payload.
pub const EVENT_RECORD_HEADER_LEN: usize = 32;
/// Synthetic record type used only by file streams when their live receiver
/// lagged. Its payload is one little-endian `u64` count.
pub const EVENT_TYPE_STREAM_GAP: u16 = u16::MAX;

/// Stable event ids. The numeric value is also its activation-bit index.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u16)]
pub enum EventType {
    ServerStart = 0,
    ServerStop = 1,
    TaskStart = 2,
    TaskStop = 3,
    ClientConnect = 4,
    ClientDisconnect = 5,
    ClientReject = 6,
    ConfigChange = 7,
    StreamStart = 8,
    StreamStop = 9,
    ProtocolError = 10,
    PtyCreate = 11,
    PtyExit = 12,
    PtyRemove = 13,
    Deadline = 14,
    Capacity = 15,
    FrameRead = 16,
    FrameWrite = 17,
    MessageRead = 18,
    MessageWrite = 19,
    TickStart = 20,
    TickStop = 21,
    TickNudge = 22,
    SessionLock = 23,
    PtyRead = 24,
    PtyWrite = 25,
    PtyParse = 26,
    PtySnapshot = 27,
    PtyResize = 28,
    PtyInput = 29,
    CompositorEvent = 30,
    CompositorCommand = 31,
    SurfaceEncode = 32,
    SurfaceFrame = 33,
    AudioFrame = 34,
    FsRequest = 35,
    GitRequest = 36,
    LspRequest = 37,
    KvRequest = 38,
    NetRequest = 39,
    ProcessRequest = 40,
    ExtensionRequest = 41,
    ChannelRequest = 42,
    ClientControl = 43,
    OutboxQueue = 44,
    Supervisor = 45,
    ConnectionAccept = 46,
    Error = 47,
}

impl EventType {
    pub const fn id(self) -> u16 {
        self as u16
    }

    pub const fn name(self) -> &'static str {
        EVENT_TYPE_CATALOG[self as usize].1
    }

    pub fn from_name(name: &str) -> Option<Self> {
        EVENT_TYPE_CATALOG
            .iter()
            .find_map(|&(kind, candidate)| (candidate == name).then_some(kind))
    }
}

pub const EVENT_TYPE_CATALOG: &[(EventType, &str)] = &[
    (EventType::ServerStart, "server.start"),
    (EventType::ServerStop, "server.stop"),
    (EventType::TaskStart, "task.start"),
    (EventType::TaskStop, "task.stop"),
    (EventType::ClientConnect, "client.connect"),
    (EventType::ClientDisconnect, "client.disconnect"),
    (EventType::ClientReject, "client.reject"),
    (EventType::ConfigChange, "config.change"),
    (EventType::StreamStart, "stream.start"),
    (EventType::StreamStop, "stream.stop"),
    (EventType::ProtocolError, "protocol.error"),
    (EventType::PtyCreate, "pty.create"),
    (EventType::PtyExit, "pty.exit"),
    (EventType::PtyRemove, "pty.remove"),
    (EventType::Deadline, "pty.deadline"),
    (EventType::Capacity, "server.capacity"),
    (EventType::FrameRead, "frame.read"),
    (EventType::FrameWrite, "frame.write"),
    (EventType::MessageRead, "message.read"),
    (EventType::MessageWrite, "message.write"),
    (EventType::TickStart, "tick.start"),
    (EventType::TickStop, "tick.stop"),
    (EventType::TickNudge, "tick.nudge"),
    (EventType::SessionLock, "session.lock"),
    (EventType::PtyRead, "pty.read"),
    (EventType::PtyWrite, "pty.write"),
    (EventType::PtyParse, "pty.parse"),
    (EventType::PtySnapshot, "pty.snapshot"),
    (EventType::PtyResize, "pty.resize"),
    (EventType::PtyInput, "pty.input"),
    (EventType::CompositorEvent, "compositor.event"),
    (EventType::CompositorCommand, "compositor.command"),
    (EventType::SurfaceEncode, "surface.encode"),
    (EventType::SurfaceFrame, "surface.frame"),
    (EventType::AudioFrame, "audio.frame"),
    (EventType::FsRequest, "fs.request"),
    (EventType::GitRequest, "git.request"),
    (EventType::LspRequest, "lsp.request"),
    (EventType::KvRequest, "kv.request"),
    (EventType::NetRequest, "net.request"),
    (EventType::ProcessRequest, "process.request"),
    (EventType::ExtensionRequest, "extension.request"),
    (EventType::ChannelRequest, "channel.request"),
    (EventType::ClientControl, "client.control"),
    (EventType::OutboxQueue, "outbox.queue"),
    (EventType::Supervisor, "supervisor.event"),
    (EventType::ConnectionAccept, "connection.accept"),
    (EventType::Error, "server.error"),
];

/// The exact 256-bit activation set carried by the protocol.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ActivationSet(pub [u64; ACTIVATION_WORDS]);

impl ActivationSet {
    pub const fn low_throughput() -> Self {
        // IDs 0 through 15 are intentionally the low-volume lifecycle set.
        Self([u16::MAX as u64, 0, 0, 0])
    }

    pub const fn all() -> Self {
        Self([u64::MAX; ACTIVATION_WORDS])
    }

    pub const fn enabled(self, kind: EventType) -> bool {
        let id = kind.id() as usize;
        self.0[id / 64] & (1u64 << (id % 64)) != 0
    }

    pub fn set(&mut self, kind: EventType, enabled: bool) {
        let id = kind.id() as usize;
        let bit = 1u64 << (id % 64);
        if enabled {
            self.0[id / 64] |= bit;
        } else {
            self.0[id / 64] &= !bit;
        }
    }

    pub fn to_bytes(self) -> [u8; ACTIVATION_BYTES] {
        let mut bytes = [0; ACTIVATION_BYTES];
        for (index, word) in self.0.into_iter().enumerate() {
            bytes[index * 8..index * 8 + 8].copy_from_slice(&word.to_le_bytes());
        }
        bytes
    }

    pub fn from_bytes(bytes: &[u8]) -> Option<Self> {
        if bytes.len() != ACTIVATION_BYTES {
            return None;
        }
        let mut words = [0; ACTIVATION_WORDS];
        for (index, word) in words.iter_mut().enumerate() {
            *word = u64::from_le_bytes(bytes[index * 8..index * 8 + 8].try_into().ok()?);
        }
        Some(Self(words))
    }
}

/// Parse `all`, `none`, `default`, exact names, and `category.*` selectors.
/// A leading `+`/`-` expression edits the default set; otherwise the set
/// starts empty and is built left-to-right.
pub fn parse_activation_spec(spec: &str) -> Result<ActivationSet, String> {
    let first = spec.split(',').map(str::trim).find(|part| !part.is_empty());
    let mut set = if first.is_some_and(|part| part.starts_with(['+', '-'])) {
        ActivationSet::low_throughput()
    } else {
        ActivationSet::default()
    };
    for raw in spec
        .split(',')
        .map(str::trim)
        .filter(|part| !part.is_empty())
    {
        let (enabled, selector) = match raw.as_bytes().first() {
            Some(b'+') => (true, &raw[1..]),
            Some(b'-') => (false, &raw[1..]),
            _ => (true, raw),
        };
        match selector {
            "all" => {
                set = if enabled {
                    ActivationSet::all()
                } else {
                    ActivationSet::default()
                }
            }
            "none" => {
                if enabled {
                    set = ActivationSet::default();
                }
            }
            "default" => {
                if enabled {
                    set = ActivationSet::low_throughput();
                } else {
                    for &(kind, _) in EVENT_TYPE_CATALOG {
                        if ActivationSet::low_throughput().enabled(kind) {
                            set.set(kind, false);
                        }
                    }
                }
            }
            _ => {
                let mut matched = false;
                if let Some(prefix) = selector.strip_suffix(".*") {
                    for &(kind, name) in EVENT_TYPE_CATALOG {
                        if name
                            .strip_prefix(prefix)
                            .is_some_and(|tail| tail.starts_with('.'))
                        {
                            set.set(kind, enabled);
                            matched = true;
                        }
                    }
                } else if let Some(kind) = EventType::from_name(selector) {
                    set.set(kind, enabled);
                    matched = true;
                }
                if !matched {
                    return Err(format!("unknown event selector {selector:?}"));
                }
            }
        }
    }
    Ok(set)
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EventsRequest<'a> {
    ConfigGet {
        nonce: u16,
    },
    ConfigSet {
        nonce: u16,
        size: u64,
        activations: ActivationSet,
    },
    Dump {
        nonce: u16,
    },
    StreamStart {
        nonce: u16,
        target: u8,
        flags: u8,
        path: &'a str,
    },
    StreamStop {
        nonce: u16,
        stream_id: u32,
    },
    StreamList {
        nonce: u16,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EventsDecodeError {
    NotEvents,
    UnsupportedVersion,
    Truncated,
    TrailingBytes,
    InvalidTarget,
    InvalidFlags,
    InvalidPath,
    InvalidOperation,
}

impl fmt::Display for EventsDecodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::NotEvents => "not an events packet",
            Self::UnsupportedVersion => "unsupported events protocol version",
            Self::Truncated => "events packet is truncated",
            Self::TrailingBytes => "events packet has trailing bytes",
            Self::InvalidTarget => "invalid events stream target",
            Self::InvalidFlags => "invalid events stream flags",
            Self::InvalidPath => "invalid events stream path",
            Self::InvalidOperation => "invalid events operation",
        })
    }
}

pub fn parse_events_request(packet: &[u8]) -> Result<EventsRequest<'_>, EventsDecodeError> {
    if packet.first() != Some(&EVENTS) {
        return Err(EventsDecodeError::NotEvents);
    }
    if packet.len() < 5 {
        return Err(EventsDecodeError::Truncated);
    }
    if packet[1] != EVENTS_VERSION {
        return Err(EventsDecodeError::UnsupportedVersion);
    }
    let op = packet[2];
    let nonce = u16::from_le_bytes([packet[3], packet[4]]);
    let body = &packet[5..];
    match op {
        EVENTS_CONFIG_GET if body.is_empty() => Ok(EventsRequest::ConfigGet { nonce }),
        EVENTS_CONFIG_GET => Err(EventsDecodeError::TrailingBytes),
        EVENTS_CONFIG_SET => {
            if body.len() < 8 + ACTIVATION_BYTES {
                return Err(EventsDecodeError::Truncated);
            }
            if body.len() != 8 + ACTIVATION_BYTES {
                return Err(EventsDecodeError::TrailingBytes);
            }
            Ok(EventsRequest::ConfigSet {
                nonce,
                size: u64::from_le_bytes(body[..8].try_into().expect("checked length")),
                activations: ActivationSet::from_bytes(&body[8..]).expect("checked length"),
            })
        }
        EVENTS_DUMP if body.is_empty() => Ok(EventsRequest::Dump { nonce }),
        EVENTS_DUMP => Err(EventsDecodeError::TrailingBytes),
        EVENTS_STREAM_START => {
            if body.len() < 4 {
                return Err(EventsDecodeError::Truncated);
            }
            let target = body[0];
            let flags = body[1];
            if target > EVENTS_TARGET_FILE {
                return Err(EventsDecodeError::InvalidTarget);
            }
            if flags & !EVENTS_STREAM_FLAGS != 0
                || target == EVENTS_TARGET_CLIENT && flags & EVENTS_STREAM_APPEND != 0
            {
                return Err(EventsDecodeError::InvalidFlags);
            }
            let path_len = u16::from_le_bytes([body[2], body[3]]) as usize;
            if body.len() < 4 + path_len {
                return Err(EventsDecodeError::Truncated);
            }
            if body.len() != 4 + path_len {
                return Err(EventsDecodeError::TrailingBytes);
            }
            let path =
                std::str::from_utf8(&body[4..]).map_err(|_| EventsDecodeError::InvalidPath)?;
            if path.as_bytes().contains(&0)
                || (target == EVENTS_TARGET_CLIENT && !path.is_empty())
                || (target == EVENTS_TARGET_FILE && path.is_empty())
            {
                return Err(EventsDecodeError::InvalidPath);
            }
            Ok(EventsRequest::StreamStart {
                nonce,
                target,
                flags,
                path,
            })
        }
        EVENTS_STREAM_STOP => {
            if body.len() < 4 {
                return Err(EventsDecodeError::Truncated);
            }
            if body.len() != 4 {
                return Err(EventsDecodeError::TrailingBytes);
            }
            Ok(EventsRequest::StreamStop {
                nonce,
                stream_id: u32::from_le_bytes(body.try_into().expect("checked length")),
            })
        }
        EVENTS_STREAM_LIST if body.is_empty() => Ok(EventsRequest::StreamList { nonce }),
        EVENTS_STREAM_LIST => Err(EventsDecodeError::TrailingBytes),
        _ => Err(EventsDecodeError::InvalidOperation),
    }
}

fn request(op: u8, nonce: u16) -> Vec<u8> {
    let mut msg = vec![EVENTS, EVENTS_VERSION, op];
    msg.extend_from_slice(&nonce.to_le_bytes());
    msg
}

pub fn msg_events_config_get(nonce: u16) -> Vec<u8> {
    request(EVENTS_CONFIG_GET, nonce)
}

pub fn msg_events_config_set(nonce: u16, size: u64, activations: ActivationSet) -> Vec<u8> {
    let mut msg = request(EVENTS_CONFIG_SET, nonce);
    msg.extend_from_slice(&size.to_le_bytes());
    msg.extend_from_slice(&activations.to_bytes());
    msg
}

pub fn msg_events_dump(nonce: u16) -> Vec<u8> {
    request(EVENTS_DUMP, nonce)
}

pub fn msg_events_stream_start(nonce: u16, target: u8, flags: u8, path: &str) -> Vec<u8> {
    let path = path.as_bytes();
    let len = path.len().min(u16::MAX as usize);
    let mut msg = request(EVENTS_STREAM_START, nonce);
    msg.push(target);
    msg.push(flags);
    msg.extend_from_slice(&(len as u16).to_le_bytes());
    msg.extend_from_slice(&path[..len]);
    msg
}

pub fn msg_events_stream_stop(nonce: u16, stream_id: u32) -> Vec<u8> {
    let mut msg = request(EVENTS_STREAM_STOP, nonce);
    msg.extend_from_slice(&stream_id.to_le_bytes());
    msg
}

pub fn msg_events_stream_list(nonce: u16) -> Vec<u8> {
    request(EVENTS_STREAM_LIST, nonce)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EventStreamInfo<'a> {
    pub stream_id: u32,
    pub running: bool,
    pub flags: u8,
    pub path: &'a str,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EventsMessage<'a> {
    Config {
        nonce: u16,
        size: u64,
        used: u64,
        records: u64,
        dropped: u64,
        next_sequence: u64,
        activations: ActivationSet,
    },
    Result {
        nonce: u16,
        status: u8,
        stream_id: u32,
        detail: &'a str,
    },
    Dump {
        nonce: u16,
        bytes: &'a [u8],
    },
    StreamStarted {
        nonce: u16,
        status: u8,
        stream_id: u32,
        detail: &'a str,
    },
    Record {
        stream_id: u32,
        record: &'a [u8],
    },
    StreamStopped {
        stream_id: u32,
        status: u8,
        detail: &'a str,
    },
    StreamGap {
        stream_id: u32,
        lost: u64,
    },
    Streams {
        nonce: u16,
        streams: Vec<EventStreamInfo<'a>>,
    },
}

fn response(kind: u8, nonce: u16) -> Vec<u8> {
    let mut msg = vec![EVENTS, EVENTS_VERSION, kind];
    msg.extend_from_slice(&nonce.to_le_bytes());
    msg
}

pub fn msg_events_config(
    nonce: u16,
    size: u64,
    used: u64,
    records: u64,
    dropped: u64,
    next_sequence: u64,
    activations: ActivationSet,
) -> Vec<u8> {
    let mut msg = response(EVENTS_CONFIG, nonce);
    for value in [size, used, records, dropped, next_sequence] {
        msg.extend_from_slice(&value.to_le_bytes());
    }
    msg.extend_from_slice(&activations.to_bytes());
    msg
}

pub fn msg_events_result(nonce: u16, status: u8, stream_id: u32, detail: &str) -> Vec<u8> {
    let mut msg = response(EVENTS_RESULT, nonce);
    msg.push(status);
    msg.extend_from_slice(&stream_id.to_le_bytes());
    msg.extend_from_slice(detail.as_bytes());
    msg
}

pub fn msg_events_dumped(nonce: u16, dump: &[u8]) -> Vec<u8> {
    let mut msg = response(EVENTS_DUMPED, nonce);
    msg.extend_from_slice(dump);
    msg
}

pub fn msg_events_stream_started(nonce: u16, status: u8, stream_id: u32, detail: &str) -> Vec<u8> {
    let mut msg = response(EVENTS_STREAM_STARTED, nonce);
    msg.push(status);
    msg.extend_from_slice(&stream_id.to_le_bytes());
    msg.extend_from_slice(detail.as_bytes());
    msg
}

pub fn msg_events_record(stream_id: u32, record: &[u8]) -> Vec<u8> {
    let mut msg = vec![EVENTS, EVENTS_VERSION, EVENTS_RECORD];
    msg.extend_from_slice(&stream_id.to_le_bytes());
    msg.extend_from_slice(record);
    msg
}

pub fn msg_events_stream_stopped(stream_id: u32, status: u8, detail: &str) -> Vec<u8> {
    let mut msg = vec![EVENTS, EVENTS_VERSION, EVENTS_STREAM_STOPPED];
    msg.extend_from_slice(&stream_id.to_le_bytes());
    msg.push(status);
    msg.extend_from_slice(detail.as_bytes());
    msg
}

pub fn msg_events_stream_gap(stream_id: u32, lost: u64) -> Vec<u8> {
    let mut msg = vec![EVENTS, EVENTS_VERSION, EVENTS_STREAM_GAP];
    msg.extend_from_slice(&stream_id.to_le_bytes());
    msg.extend_from_slice(&lost.to_le_bytes());
    msg
}

pub fn msg_events_streams(nonce: u16, streams: &[EventStreamInfo<'_>]) -> Vec<u8> {
    let count = streams.len().min(u16::MAX as usize);
    let mut msg = response(EVENTS_STREAMS, nonce);
    msg.extend_from_slice(&(count as u16).to_le_bytes());
    for stream in &streams[..count] {
        let mut path_len = stream.path.len().min(u16::MAX as usize);
        while !stream.path.is_char_boundary(path_len) {
            path_len -= 1;
        }
        msg.extend_from_slice(&stream.stream_id.to_le_bytes());
        msg.push(u8::from(stream.running));
        msg.push(stream.flags & EVENTS_STREAM_FLAGS);
        msg.extend_from_slice(&(path_len as u16).to_le_bytes());
        msg.extend_from_slice(&stream.path.as_bytes()[..path_len]);
    }
    msg
}

pub fn parse_events_message(packet: &[u8]) -> Result<EventsMessage<'_>, EventsDecodeError> {
    if packet.first() != Some(&EVENTS) {
        return Err(EventsDecodeError::NotEvents);
    }
    if packet.len() < 3 {
        return Err(EventsDecodeError::Truncated);
    }
    if packet[1] != EVENTS_VERSION {
        return Err(EventsDecodeError::UnsupportedVersion);
    }
    match packet[2] {
        EVENTS_CONFIG => {
            let expected = 5 + 5 * 8 + ACTIVATION_BYTES;
            if packet.len() < expected {
                return Err(EventsDecodeError::Truncated);
            }
            if packet.len() != expected {
                return Err(EventsDecodeError::TrailingBytes);
            }
            let nonce = u16::from_le_bytes([packet[3], packet[4]]);
            let mut at = 5;
            let mut take_u64 = || {
                let value = u64::from_le_bytes(packet[at..at + 8].try_into().expect("checked"));
                at += 8;
                value
            };
            Ok(EventsMessage::Config {
                nonce,
                size: take_u64(),
                used: take_u64(),
                records: take_u64(),
                dropped: take_u64(),
                next_sequence: take_u64(),
                activations: ActivationSet::from_bytes(&packet[at..]).expect("checked"),
            })
        }
        EVENTS_RESULT | EVENTS_STREAM_STARTED => {
            if packet.len() < 10 {
                return Err(EventsDecodeError::Truncated);
            }
            let nonce = u16::from_le_bytes([packet[3], packet[4]]);
            let status = packet[5];
            let stream_id = u32::from_le_bytes(packet[6..10].try_into().expect("checked"));
            let detail =
                std::str::from_utf8(&packet[10..]).map_err(|_| EventsDecodeError::InvalidPath)?;
            if packet[2] == EVENTS_RESULT {
                Ok(EventsMessage::Result {
                    nonce,
                    status,
                    stream_id,
                    detail,
                })
            } else {
                Ok(EventsMessage::StreamStarted {
                    nonce,
                    status,
                    stream_id,
                    detail,
                })
            }
        }
        EVENTS_DUMPED => {
            if packet.len() < 5 {
                return Err(EventsDecodeError::Truncated);
            }
            Ok(EventsMessage::Dump {
                nonce: u16::from_le_bytes([packet[3], packet[4]]),
                bytes: &packet[5..],
            })
        }
        EVENTS_RECORD => {
            if packet.len() < 7 + EVENT_RECORD_HEADER_LEN {
                return Err(EventsDecodeError::Truncated);
            }
            let record_len =
                u32::from_le_bytes(packet[7..11].try_into().expect("checked")) as usize;
            if record_len < EVENT_RECORD_HEADER_LEN || packet.len() < 7 + record_len {
                return Err(EventsDecodeError::Truncated);
            }
            if packet.len() != 7 + record_len {
                return Err(EventsDecodeError::TrailingBytes);
            }
            Ok(EventsMessage::Record {
                stream_id: u32::from_le_bytes(packet[3..7].try_into().expect("checked")),
                record: &packet[7..],
            })
        }
        EVENTS_STREAM_STOPPED => {
            if packet.len() < 8 {
                return Err(EventsDecodeError::Truncated);
            }
            Ok(EventsMessage::StreamStopped {
                stream_id: u32::from_le_bytes(packet[3..7].try_into().expect("checked")),
                status: packet[7],
                detail: std::str::from_utf8(&packet[8..])
                    .map_err(|_| EventsDecodeError::InvalidPath)?,
            })
        }
        EVENTS_STREAM_GAP => {
            if packet.len() < 15 {
                return Err(EventsDecodeError::Truncated);
            }
            if packet.len() != 15 {
                return Err(EventsDecodeError::TrailingBytes);
            }
            Ok(EventsMessage::StreamGap {
                stream_id: u32::from_le_bytes(packet[3..7].try_into().expect("checked")),
                lost: u64::from_le_bytes(packet[7..15].try_into().expect("checked")),
            })
        }
        EVENTS_STREAMS => {
            if packet.len() < 7 {
                return Err(EventsDecodeError::Truncated);
            }
            let nonce = u16::from_le_bytes([packet[3], packet[4]]);
            let count = u16::from_le_bytes([packet[5], packet[6]]) as usize;
            let mut at = 7;
            let mut streams = Vec::with_capacity(count);
            for _ in 0..count {
                if packet.len().saturating_sub(at) < 8 {
                    return Err(EventsDecodeError::Truncated);
                }
                let stream_id = u32::from_le_bytes(packet[at..at + 4].try_into().expect("checked"));
                let running = match packet[at + 4] {
                    0 => false,
                    1 => true,
                    _ => return Err(EventsDecodeError::InvalidFlags),
                };
                let flags = packet[at + 5];
                if flags & !EVENTS_STREAM_FLAGS != 0 {
                    return Err(EventsDecodeError::InvalidFlags);
                }
                let path_len = u16::from_le_bytes([packet[at + 6], packet[at + 7]]) as usize;
                at += 8;
                if packet.len().saturating_sub(at) < path_len {
                    return Err(EventsDecodeError::Truncated);
                }
                let path = std::str::from_utf8(&packet[at..at + path_len])
                    .map_err(|_| EventsDecodeError::InvalidPath)?;
                at += path_len;
                streams.push(EventStreamInfo {
                    stream_id,
                    running,
                    flags,
                    path,
                });
            }
            if at != packet.len() {
                return Err(EventsDecodeError::TrailingBytes);
            }
            Ok(EventsMessage::Streams { nonce, streams })
        }
        _ => Err(EventsDecodeError::InvalidOperation),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn activation_selectors_are_composable() {
        let set = parse_activation_spec("default,+frame.*,-client.disconnect").unwrap();
        assert!(set.enabled(EventType::ServerStart));
        assert!(set.enabled(EventType::FrameRead));
        assert!(set.enabled(EventType::FrameWrite));
        assert!(!set.enabled(EventType::ClientDisconnect));
        assert!(!set.enabled(EventType::PtyRead));
        assert_eq!(
            parse_activation_spec("default,-none").unwrap(),
            ActivationSet::low_throughput()
        );
    }

    #[test]
    fn request_codecs_round_trip() {
        let activations = ActivationSet([1, 2, 3, 4]);
        assert_eq!(
            parse_events_request(&msg_events_config_set(9, 1024, activations)).unwrap(),
            EventsRequest::ConfigSet {
                nonce: 9,
                size: 1024,
                activations
            }
        );
        assert_eq!(
            parse_events_request(&msg_events_stream_start(
                7,
                EVENTS_TARGET_FILE,
                EVENTS_STREAM_HISTORY | EVENTS_STREAM_APPEND,
                "/tmp/events.bin",
            ))
            .unwrap(),
            EventsRequest::StreamStart {
                nonce: 7,
                target: EVENTS_TARGET_FILE,
                flags: EVENTS_STREAM_HISTORY | EVENTS_STREAM_APPEND,
                path: "/tmp/events.bin",
            }
        );
        assert_eq!(
            parse_events_request(&msg_events_stream_stop(8, 42)).unwrap(),
            EventsRequest::StreamStop {
                nonce: 8,
                stream_id: 42
            }
        );
        assert_eq!(
            parse_events_request(&msg_events_stream_list(10)).unwrap(),
            EventsRequest::StreamList { nonce: 10 }
        );
    }

    #[test]
    fn response_codecs_round_trip() {
        let activations = ActivationSet([5, 6, 7, 8]);
        let msg = msg_events_config(2, 100, 80, 3, 4, 9, activations);
        assert_eq!(
            parse_events_message(&msg).unwrap(),
            EventsMessage::Config {
                nonce: 2,
                size: 100,
                used: 80,
                records: 3,
                dropped: 4,
                next_sequence: 9,
                activations,
            }
        );
        let result = msg_events_stream_started(4, crate::STATUS_OK, 11, "");
        assert_eq!(
            parse_events_message(&result).unwrap(),
            EventsMessage::StreamStarted {
                nonce: 4,
                status: crate::STATUS_OK,
                stream_id: 11,
                detail: "",
            }
        );
        let streams = [EventStreamInfo {
            stream_id: 12,
            running: true,
            flags: EVENTS_STREAM_HISTORY,
            path: "/tmp/events.bin",
        }];
        let message = msg_events_streams(5, &streams);
        assert_eq!(
            parse_events_message(&message).unwrap(),
            EventsMessage::Streams {
                nonce: 5,
                streams: streams.to_vec(),
            }
        );
    }
}
