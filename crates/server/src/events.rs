//! Server-owned structured event recording.

#![allow(dead_code, unused_imports, unused_macros)]

use blit_remote::events::{
    Activation, C2S_CONFIG_GET, C2S_CONFIG_SET, C2S_DUMP, C2S_FILE_START, C2S_FILE_STOP,
    C2S_STREAM_START, C2S_STREAM_STOP, EVENT_FILE_HEADER_SIZE, EVENT_RECORD_SIZE, EVENTS_RING_MAX,
    EVENTS_RING_MIN, EventConfig, EventFileHeader, EventRecord, EventRequest, FILE_APPEND,
    FILE_SYNC, STREAM_FLAGS, STREAM_FOLLOW, msg_event_config, msg_event_dump,
    msg_event_file_status, msg_event_status, msg_event_stream_data, msg_event_stream_status,
    parse_event_request,
};
use std::collections::HashMap;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock, RwLock};
use std::time::Instant;
use tokio::fs::{File, OpenOptions};
use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt};
use tokio::sync::{Mutex as AsyncMutex, mpsc, oneshot, watch};
use tokio::task::JoinHandle;

pub(crate) const DEFAULT_RING_BYTES: u64 = 1024 * 1024;
pub(crate) const DEFAULT_RING_RECORDS: u32 = (DEFAULT_RING_BYTES / EVENT_RECORD_SIZE as u64) as u32;
pub(crate) const MAX_FILE_STREAMS: usize = 4;

/// Stable event ids. Values are activation-bit positions and must remain below 128.
#[repr(u8)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum EventId {
    ServerStarting = 0,
    ServerStarted = 1,
    ServerStopping = 2,
    ServerStopped = 3,
    ServerError = 4,
    ClientConnected = 8,
    ClientReady = 9,
    ClientDisconnecting = 10,
    ClientDisconnected = 11,
    ClientError = 12,
    RawRequestRead = 16,
    RawRequestDispatch = 17,
    RawRequestDone = 18,
    RawRequestReject = 19,
    WriterDequeue = 24,
    WriterWriteBegin = 25,
    WriterWriteEnd = 26,
    WriterError = 27,
    WriterBackpressure = 28,
    PtyCreateRequest = 32,
    PtyCreateMutexAcquired = 33,
    PtyCreateSpawnBegin = 34,
    PtyCreateSpawnEnd = 35,
    PtyCreateRegistered = 36,
    PtyCreateReplyQueued = 37,
    PtyCreateError = 38,
    PtyRead = 40,
    PtyQueue = 41,
    PtyDrain = 42,
    PtyParse = 43,
    PtyFrameQueued = 44,
    PtyInput = 45,
    PtyResize = 46,
    PtyExit = 47,
    PtyEvict = 48,
    PtyIoError = 49,
    ProcessRequest = 56,
    ProcessSpawn = 57,
    ProcessResult = 58,
    ProcessIo = 59,
    ProcessExit = 60,
    ProcessError = 61,
    CompositorStarted = 64,
    CompositorStopped = 65,
    CompositorError = 66,
    SurfaceCreated = 68,
    SurfaceDestroyed = 69,
    SurfaceFrameQueued = 70,
    SurfaceError = 71,
    ProtocolCore = 72,
    ProtocolPty = 73,
    ProtocolProcess = 74,
    ProtocolCompositor = 75,
    ProtocolSurface = 76,
    ProtocolInput = 77,
    ProtocolClipboard = 78,
    ProtocolFilesystem = 79,
    ProtocolNetwork = 80,
    ProtocolKv = 81,
    ProtocolBrowser = 82,
    ProtocolAudio = 83,
    ProtocolEvents = 84,
    ProtocolIntegration = 85,
    ProtocolError = 87,
    TaskSpawned = 104,
    TaskCompleted = 105,
    TaskCancelled = 106,
    TaskFailed = 107,
    ConfigChanged = 112,
    ConfigError = 113,
    RingDropped = 114,
    RingOverwritten = 115,
    StreamGap = 116,
    StreamError = 117,
}

impl EventId {
    const ALL: &'static [Self] = &[
        Self::ServerStarting,
        Self::ServerStarted,
        Self::ServerStopping,
        Self::ServerStopped,
        Self::ServerError,
        Self::ClientConnected,
        Self::ClientReady,
        Self::ClientDisconnecting,
        Self::ClientDisconnected,
        Self::ClientError,
        Self::RawRequestRead,
        Self::RawRequestDispatch,
        Self::RawRequestDone,
        Self::RawRequestReject,
        Self::WriterDequeue,
        Self::WriterWriteBegin,
        Self::WriterWriteEnd,
        Self::WriterError,
        Self::WriterBackpressure,
        Self::PtyCreateRequest,
        Self::PtyCreateMutexAcquired,
        Self::PtyCreateSpawnBegin,
        Self::PtyCreateSpawnEnd,
        Self::PtyCreateRegistered,
        Self::PtyCreateReplyQueued,
        Self::PtyCreateError,
        Self::PtyRead,
        Self::PtyQueue,
        Self::PtyDrain,
        Self::PtyParse,
        Self::PtyFrameQueued,
        Self::PtyInput,
        Self::PtyResize,
        Self::PtyExit,
        Self::PtyEvict,
        Self::PtyIoError,
        Self::ProcessRequest,
        Self::ProcessSpawn,
        Self::ProcessResult,
        Self::ProcessIo,
        Self::ProcessExit,
        Self::ProcessError,
        Self::CompositorStarted,
        Self::CompositorStopped,
        Self::CompositorError,
        Self::SurfaceCreated,
        Self::SurfaceDestroyed,
        Self::SurfaceFrameQueued,
        Self::SurfaceError,
        Self::ProtocolCore,
        Self::ProtocolPty,
        Self::ProtocolProcess,
        Self::ProtocolCompositor,
        Self::ProtocolSurface,
        Self::ProtocolInput,
        Self::ProtocolClipboard,
        Self::ProtocolFilesystem,
        Self::ProtocolNetwork,
        Self::ProtocolKv,
        Self::ProtocolBrowser,
        Self::ProtocolAudio,
        Self::ProtocolEvents,
        Self::ProtocolIntegration,
        Self::ProtocolError,
        Self::TaskSpawned,
        Self::TaskCompleted,
        Self::TaskCancelled,
        Self::TaskFailed,
        Self::ConfigChanged,
        Self::ConfigError,
        Self::RingDropped,
        Self::RingOverwritten,
        Self::StreamGap,
        Self::StreamError,
    ];

    const fn family(self) -> EventFamily {
        match self as u8 {
            0..=7 => EventFamily::Server,
            8..=15 => EventFamily::Client,
            16..=23 => EventFamily::Request,
            24..=31 => EventFamily::Writer,
            32..=55 => EventFamily::Pty,
            56..=63 => EventFamily::Process,
            64..=67 => EventFamily::Compositor,
            68..=71 => EventFamily::Surface,
            72..=103 => EventFamily::Protocol,
            104..=111 => EventFamily::Task,
            _ => EventFamily::Recorder,
        }
    }

    const fn name(self) -> &'static str {
        match self {
            Self::ServerStarting => "server-starting",
            Self::ServerStarted => "server-started",
            Self::ServerStopping => "server-stopping",
            Self::ServerStopped => "server-stopped",
            Self::ServerError => "server-error",
            Self::ClientConnected => "client-connected",
            Self::ClientReady => "client-ready",
            Self::ClientDisconnecting => "client-disconnecting",
            Self::ClientDisconnected => "client-disconnected",
            Self::ClientError => "client-error",
            Self::RawRequestRead => "raw-request-read",
            Self::RawRequestDispatch => "raw-request-dispatch",
            Self::RawRequestDone => "raw-request-done",
            Self::RawRequestReject => "raw-request-reject",
            Self::WriterDequeue => "writer-dequeue",
            Self::WriterWriteBegin => "writer-write-begin",
            Self::WriterWriteEnd => "writer-write-end",
            Self::WriterError => "writer-error",
            Self::WriterBackpressure => "writer-backpressure",
            Self::PtyCreateRequest => "pty-create-request",
            Self::PtyCreateMutexAcquired => "pty-create-mutex-acquired",
            Self::PtyCreateSpawnBegin => "pty-create-spawn-begin",
            Self::PtyCreateSpawnEnd => "pty-create-spawn-end",
            Self::PtyCreateRegistered => "pty-create-registered",
            Self::PtyCreateReplyQueued => "pty-create-reply-queued",
            Self::PtyCreateError => "pty-create-error",
            Self::PtyRead => "pty-read",
            Self::PtyQueue => "pty-queue",
            Self::PtyDrain => "pty-drain",
            Self::PtyParse => "pty-parse",
            Self::PtyFrameQueued => "pty-frame-queued",
            Self::PtyInput => "pty-input",
            Self::PtyResize => "pty-resize",
            Self::PtyExit => "pty-exit",
            Self::PtyEvict => "pty-evict",
            Self::PtyIoError => "pty-io-error",
            Self::ProcessRequest => "process-request",
            Self::ProcessSpawn => "process-spawn",
            Self::ProcessResult => "process-result",
            Self::ProcessIo => "process-io",
            Self::ProcessExit => "process-exit",
            Self::ProcessError => "process-error",
            Self::CompositorStarted => "compositor-started",
            Self::CompositorStopped => "compositor-stopped",
            Self::CompositorError => "compositor-error",
            Self::SurfaceCreated => "surface-created",
            Self::SurfaceDestroyed => "surface-destroyed",
            Self::SurfaceFrameQueued => "surface-frame-queued",
            Self::SurfaceError => "surface-error",
            Self::ProtocolCore => "protocol-core",
            Self::ProtocolPty => "protocol-pty",
            Self::ProtocolProcess => "protocol-process",
            Self::ProtocolCompositor => "protocol-compositor",
            Self::ProtocolSurface => "protocol-surface",
            Self::ProtocolInput => "protocol-input",
            Self::ProtocolClipboard => "protocol-clipboard",
            Self::ProtocolFilesystem => "protocol-filesystem",
            Self::ProtocolNetwork => "protocol-network",
            Self::ProtocolKv => "protocol-kv",
            Self::ProtocolBrowser => "protocol-browser",
            Self::ProtocolAudio => "protocol-audio",
            Self::ProtocolEvents => "protocol-events",
            Self::ProtocolIntegration => "protocol-integration",
            Self::ProtocolError => "protocol-error",
            Self::TaskSpawned => "task-spawned",
            Self::TaskCompleted => "task-completed",
            Self::TaskCancelled => "task-cancelled",
            Self::TaskFailed => "task-failed",
            Self::ConfigChanged => "config-changed",
            Self::ConfigError => "config-error",
            Self::RingDropped => "ring-dropped",
            Self::RingOverwritten => "ring-overwritten",
            Self::StreamGap => "stream-gap",
            Self::StreamError => "stream-error",
        }
    }

    fn named(name: &str) -> Option<Self> {
        let normalized = name.replace('_', "-").to_ascii_lowercase();
        Self::ALL
            .iter()
            .copied()
            .find(|event| event.name() == normalized)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum EventFamily {
    Server,
    Client,
    Request,
    Writer,
    Pty,
    Process,
    Compositor,
    Surface,
    Protocol,
    Task,
    Recorder,
}

impl EventFamily {
    fn named(name: &str) -> Option<Self> {
        match name.to_ascii_lowercase().as_str() {
            "server" | "lifecycle" => Some(Self::Server),
            "client" | "clients" => Some(Self::Client),
            "request" | "requests" | "raw-request" | "raw-requests" => Some(Self::Request),
            "writer" | "writers" => Some(Self::Writer),
            "pty" | "pty-create" => Some(Self::Pty),
            "process" | "processes" => Some(Self::Process),
            "compositor" => Some(Self::Compositor),
            "surface" | "surfaces" => Some(Self::Surface),
            "protocol" | "protocols" | "integration" | "integrations" => Some(Self::Protocol),
            "task" | "tasks" => Some(Self::Task),
            "recorder" | "config" | "ring" | "stream" => Some(Self::Recorder),
            _ => None,
        }
    }
}

fn default_activation() -> Activation {
    let mut activation = Activation::NONE;
    for event in [
        EventId::ServerStarting,
        EventId::ServerStarted,
        EventId::ServerStopping,
        EventId::ServerStopped,
        EventId::ServerError,
        EventId::ClientConnected,
        EventId::ClientReady,
        EventId::ClientDisconnecting,
        EventId::ClientDisconnected,
        EventId::RawRequestReject,
        EventId::WriterError,
        EventId::PtyCreateRequest,
        EventId::PtyCreateMutexAcquired,
        EventId::PtyCreateSpawnBegin,
        EventId::PtyCreateSpawnEnd,
        EventId::PtyCreateRegistered,
        EventId::PtyCreateReplyQueued,
        EventId::PtyCreateError,
        EventId::PtyExit,
        EventId::PtyEvict,
        EventId::ProcessRequest,
        EventId::ProcessSpawn,
        EventId::ProcessResult,
        EventId::ProcessExit,
        EventId::CompositorStarted,
        EventId::CompositorStopped,
        EventId::SurfaceCreated,
        EventId::SurfaceDestroyed,
        EventId::ConfigChanged,
    ] {
        activation.set(event as u8, true);
    }
    activation
}

struct Slot {
    writing: AtomicBool,
    sequence: AtomicU64,
    monotonic_ns: AtomicU64,
    metadata: AtomicU64,
    connection: AtomicU64,
    subject: AtomicU64,
    args: [AtomicU64; 3],
}

impl Slot {
    fn empty() -> Self {
        Self {
            writing: AtomicBool::new(false),
            sequence: AtomicU64::new(0),
            monotonic_ns: AtomicU64::new(0),
            metadata: AtomicU64::new(0),
            connection: AtomicU64::new(0),
            subject: AtomicU64::new(0),
            args: std::array::from_fn(|_| AtomicU64::new(0)),
        }
    }

    fn write(&self, record: EventRecord) -> bool {
        if self
            .writing
            .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
            .is_err()
        {
            return false;
        }
        self.sequence.store(0, Ordering::Release);
        self.monotonic_ns
            .store(record.monotonic_ns, Ordering::Relaxed);
        self.metadata.store(
            record.event_id as u64
                | ((record.flags as u64) << 32)
                | ((record.source as u64) << 48)
                | ((record.schema as u64) << 56),
            Ordering::Relaxed,
        );
        self.connection.store(record.connection, Ordering::Relaxed);
        self.subject.store(record.subject, Ordering::Relaxed);
        for (target, value) in self.args.iter().zip(record.args) {
            target.store(value, Ordering::Relaxed);
        }
        self.sequence.store(record.sequence, Ordering::Release);
        self.writing.store(false, Ordering::Release);
        true
    }

    fn read(&self, expected: u64) -> Option<EventRecord> {
        if self.writing.load(Ordering::Acquire) {
            return None;
        }
        let sequence = self.sequence.load(Ordering::Acquire);
        if sequence != expected {
            return None;
        }
        let metadata = self.metadata.load(Ordering::Relaxed);
        let record = EventRecord {
            sequence,
            monotonic_ns: self.monotonic_ns.load(Ordering::Relaxed),
            event_id: metadata as u32,
            flags: (metadata >> 32) as u16,
            source: (metadata >> 48) as u8,
            schema: (metadata >> 56) as u8,
            connection: self.connection.load(Ordering::Relaxed),
            subject: self.subject.load(Ordering::Relaxed),
            args: std::array::from_fn(|index| self.args[index].load(Ordering::Relaxed)),
        };
        if self.writing.load(Ordering::Acquire) || self.sequence.load(Ordering::Acquire) != expected
        {
            None
        } else {
            Some(record)
        }
    }
}

struct Ring {
    slots: Box<[Slot]>,
}

impl Ring {
    fn new(size: u32) -> Self {
        Self {
            slots: (0..size).map(|_| Slot::empty()).collect(),
        }
    }

    fn len(&self) -> u64 {
        self.slots.len() as u64
    }

    fn slot(&self, sequence: u64) -> &Slot {
        &self.slots[(sequence % self.len()) as usize]
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct SequenceGap {
    pub first_sequence: u64,
    pub next_sequence: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct EventSnapshot {
    pub first_sequence: u64,
    pub next_sequence: u64,
    pub overwritten: u64,
    pub gaps: Vec<SequenceGap>,
    pub records: Vec<EventRecord>,
}

pub(crate) struct EventRecorder {
    config_lock: Mutex<()>,
    activation: [AtomicU64; 2],
    ring: RwLock<Arc<Ring>>,
    next_sequence: AtomicU64,
    dropped: AtomicU64,
    started: Instant,
    changed: watch::Sender<u64>,
}

impl Default for EventRecorder {
    fn default() -> Self {
        Self::new(EventConfig {
            ring_size: DEFAULT_RING_RECORDS,
            activation: default_activation(),
        })
        .expect("default event configuration is valid")
    }
}

impl EventRecorder {
    pub(crate) fn new(config: EventConfig) -> Result<Self, String> {
        validate_config(config)?;
        let words = activation_words(config.activation);
        Ok(Self {
            config_lock: Mutex::new(()),
            activation: [AtomicU64::new(words[0]), AtomicU64::new(words[1])],
            ring: RwLock::new(Arc::new(Ring::new(config.ring_size))),
            next_sequence: AtomicU64::new(1),
            dropped: AtomicU64::new(0),
            started: Instant::now(),
            changed: watch::channel(0).0,
        })
    }

    #[inline]
    pub(crate) fn enabled(&self, event: EventId) -> bool {
        let id = event as u8;
        self.activation[id as usize / 64].load(Ordering::Relaxed) & (1 << (id % 64)) != 0
    }

    pub(crate) fn config(&self) -> EventConfig {
        let _guard = self
            .config_lock
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        self.config_unlocked()
    }

    fn config_unlocked(&self) -> EventConfig {
        let ring_size = self
            .ring
            .read()
            .unwrap_or_else(|error| error.into_inner())
            .len() as u32;
        let words = [
            self.activation[0].load(Ordering::Acquire),
            self.activation[1].load(Ordering::Acquire),
        ];
        EventConfig {
            ring_size,
            activation: words_activation(words),
        }
    }

    pub(crate) fn set_config(&self, config: EventConfig) -> Result<(), String> {
        validate_config(config)?;
        let _guard = self
            .config_lock
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        self.set_config_unlocked(config);
        Ok(())
    }

    pub(crate) fn set_config_if(
        &self,
        expected: EventConfig,
        config: EventConfig,
    ) -> Result<bool, String> {
        validate_config(expected)?;
        validate_config(config)?;
        let _guard = self
            .config_lock
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if self.config_unlocked() != expected {
            return Ok(false);
        }
        self.set_config_unlocked(config);
        Ok(true)
    }

    fn set_config_unlocked(&self, config: EventConfig) {
        if self.config_unlocked().ring_size != config.ring_size {
            self.resize(config.ring_size);
        }
        let words = activation_words(config.activation);
        self.activation[0].store(words[0], Ordering::Release);
        self.activation[1].store(words[1], Ordering::Release);
        self.changed
            .send_replace(self.next_sequence.load(Ordering::Acquire));
    }

    fn resize(&self, size: u32) {
        let mut guard = self.ring.write().unwrap_or_else(|error| error.into_inner());
        let old = Arc::clone(&guard);
        let replacement = Arc::new(Ring::new(size));
        let edge = self.next_sequence.load(Ordering::Acquire);
        let first = edge.saturating_sub(size as u64).max(1);
        for sequence in first..edge {
            if let Some(record) = old.slot(sequence).read(sequence) {
                replacement.slot(sequence).write(record);
            }
        }
        *guard = replacement;
    }

    /// Attempts one bounded, allocation-free append. Failure means a reported sequence gap.
    #[allow(clippy::too_many_arguments)]
    #[inline]
    pub(crate) fn record(
        &self,
        event: EventId,
        flags: u16,
        source: u8,
        schema: u8,
        connection: u64,
        subject: u64,
        args: [u64; 3],
    ) -> bool {
        if !self.enabled(event) {
            return false;
        }
        let sequence = self.next_sequence.fetch_add(1, Ordering::Relaxed);
        let Ok(ring) = self.ring.try_read() else {
            self.dropped.fetch_add(1, Ordering::Relaxed);
            self.changed.send_replace(sequence.saturating_add(1));
            return false;
        };
        let record = EventRecord {
            sequence,
            monotonic_ns: self.monotonic_ns(),
            event_id: event as u32,
            flags,
            source,
            schema,
            connection,
            subject,
            args,
        };
        let written = ring.slot(sequence).write(record);
        if written {
            self.changed.send_replace(sequence.saturating_add(1));
        } else {
            self.dropped.fetch_add(1, Ordering::Relaxed);
            self.changed.send_replace(sequence.saturating_add(1));
        }
        written
    }

    pub(crate) fn dropped(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }

    fn monotonic_ns(&self) -> u64 {
        self.started.elapsed().as_nanos().min(u64::MAX as u128) as u64
    }

    pub(crate) fn oldest_sequence(&self) -> u64 {
        let edge = self.next_sequence.load(Ordering::Acquire);
        let size = self
            .ring
            .read()
            .unwrap_or_else(|error| error.into_inner())
            .len();
        edge.saturating_sub(size).max(1)
    }

    pub(crate) fn snapshot(&self, from_sequence: u64, limit: usize) -> EventSnapshot {
        let ring = self.ring.read().unwrap_or_else(|error| error.into_inner());
        let edge = self.next_sequence.load(Ordering::Acquire);
        let requested = from_sequence.max(1);
        let retained = edge.saturating_sub(ring.len()).max(1);
        let first = requested.max(retained).min(edge);
        let overwritten = first.saturating_sub(requested);
        let end = edge.min(first.saturating_add(limit as u64));
        let mut records = Vec::with_capacity((end - first) as usize);
        let mut gaps = Vec::new();
        let mut gap_start = None;
        for sequence in first..end {
            if let Some(record) = ring.slot(sequence).read(sequence) {
                if let Some(start) = gap_start.take() {
                    gaps.push(SequenceGap {
                        first_sequence: start,
                        next_sequence: sequence,
                    });
                }
                records.push(record);
            } else if gap_start.is_none() {
                gap_start = Some(sequence);
            }
        }
        if let Some(start) = gap_start {
            gaps.push(SequenceGap {
                first_sequence: start,
                next_sequence: end,
            });
        }
        EventSnapshot {
            first_sequence: first,
            next_sequence: end,
            overwritten,
            gaps,
            records,
        }
    }

    fn subscribe(&self) -> watch::Receiver<u64> {
        self.changed.subscribe()
    }
}

fn validate_config(config: EventConfig) -> Result<(), String> {
    if !(EVENTS_RING_MIN..=EVENTS_RING_MAX).contains(&config.ring_size) {
        return Err(format!(
            "event ring size must be in {EVENTS_RING_MIN}..={EVENTS_RING_MAX}"
        ));
    }
    Ok(())
}

fn activation_words(activation: Activation) -> [u64; 2] {
    [
        u64::from_le_bytes(activation.0[..8].try_into().unwrap()),
        u64::from_le_bytes(activation.0[8..].try_into().unwrap()),
    ]
}

fn words_activation(words: [u64; 2]) -> Activation {
    let mut bytes = [0; 16];
    bytes[..8].copy_from_slice(&words[0].to_le_bytes());
    bytes[8..].copy_from_slice(&words[1].to_le_bytes());
    Activation(bytes)
}

struct GlobalEvents {
    recorder: Arc<EventRecorder>,
    startup_file: Option<PathBuf>,
}

static GLOBAL: OnceLock<GlobalEvents> = OnceLock::new();

/// Installs startup configuration before any event macro observes the recorder.
pub(crate) fn initialize(config: EventStartupConfig) -> Result<&'static EventRecorder, String> {
    let recorder = Arc::new(EventRecorder::new(config.config)?);
    GLOBAL
        .set(GlobalEvents {
            recorder,
            startup_file: config.file,
        })
        .map_err(|_| "event recorder is already initialized".to_string())?;
    Ok(global())
}

fn global_state() -> &'static GlobalEvents {
    GLOBAL.get_or_init(|| GlobalEvents {
        recorder: Arc::new(EventRecorder::default()),
        startup_file: None,
    })
}

pub(crate) fn global() -> &'static EventRecorder {
    global_state().recorder.as_ref()
}

pub(crate) fn global_arc() -> Arc<EventRecorder> {
    Arc::clone(&global_state().recorder)
}

pub(crate) fn startup_file() -> Option<&'static Path> {
    global_state().startup_file.as_deref()
}

#[cfg(test)]
pub(crate) fn global_file_streams() -> Arc<FileStreamManager> {
    static FILES: OnceLock<Arc<FileStreamManager>> = OnceLock::new();
    FILES
        .get_or_init(|| Arc::new(FileStreamManager::default()))
        .clone()
}

macro_rules! blit_event_enabled {
    ($event:expr) => {{ $crate::events::global().enabled($event) }};
}

macro_rules! blit_event {
    ($event:expr) => {{
        let event = $event;
        if $crate::events::global().enabled(event) {
            $crate::events::global().record(event, 0, 0, 0, 0, 0, [0, 0, 0])
        } else {
            false
        }
    }};
    ($event:expr, $connection:expr, $subject:expr, $arg0:expr, $arg1:expr, $arg2:expr) => {{
        let event = $event;
        if $crate::events::global().enabled(event) {
            $crate::events::global().record(
                event,
                0,
                0,
                0,
                $connection,
                $subject,
                [$arg0, $arg1, $arg2],
            )
        } else {
            false
        }
    }};
    ($event:expr, flags: $flags:expr, source: $source:expr, schema: $schema:expr,
     connection: $connection:expr, subject: $subject:expr, args: [$arg0:expr, $arg1:expr, $arg2:expr]) => {{
        let event = $event;
        if $crate::events::global().enabled(event) {
            $crate::events::global().record(
                event,
                $flags,
                $source,
                $schema,
                $connection,
                $subject,
                [$arg0, $arg1, $arg2],
            )
        } else {
            false
        }
    }};
}

pub(crate) use blit_event;
pub(crate) use blit_event_enabled;

pub(crate) struct DispatchGuard {
    connection: u64,
    opcode: u8,
}

impl DispatchGuard {
    pub(crate) fn new(connection: u64, opcode: u8, bytes: usize) -> Self {
        blit_event!(
            EventId::RawRequestDispatch,
            connection,
            opcode as u64,
            bytes as u64,
            0,
            0
        );
        Self { connection, opcode }
    }
}

impl Drop for DispatchGuard {
    fn drop(&mut self) {
        blit_event!(
            EventId::RawRequestDone,
            self.connection,
            self.opcode as u64,
            0,
            0,
            0
        );
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct EventConfigOverrides {
    pub ring_bytes: Option<u64>,
    pub events: Option<String>,
    pub file: Option<PathBuf>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct EventStartupConfig {
    pub config: EventConfig,
    pub file: Option<PathBuf>,
}

impl EventStartupConfig {
    pub(crate) fn resolve(overrides: EventConfigOverrides) -> Result<Self, String> {
        Self::resolve_with(overrides, |name| {
            std::env::var(name).map(Some).or_else(|error| match error {
                std::env::VarError::NotPresent => Ok(None),
                std::env::VarError::NotUnicode(_) => Err(format!("{name} must be valid UTF-8")),
            })
        })
    }

    fn resolve_with<F>(overrides: EventConfigOverrides, read_env: F) -> Result<Self, String>
    where
        F: Fn(&str) -> Result<Option<String>, String>,
    {
        let bytes = match overrides.ring_bytes {
            Some(bytes) => bytes,
            None => read_env("BLIT_EVENTS_BYTES")?
                .map(|value| parse_bytes(&value))
                .transpose()?
                .unwrap_or(DEFAULT_RING_BYTES),
        };
        if bytes % EVENT_RECORD_SIZE as u64 != 0 {
            return Err(format!(
                "BLIT_EVENTS_BYTES must be a multiple of {EVENT_RECORD_SIZE}"
            ));
        }
        let records = bytes / EVENT_RECORD_SIZE as u64;
        if !(EVENTS_RING_MIN as u64..=EVENTS_RING_MAX as u64).contains(&records) {
            return Err(format!(
                "BLIT_EVENTS_BYTES must select {EVENTS_RING_MIN}..={EVENTS_RING_MAX} records"
            ));
        }
        let events = match overrides.events {
            Some(value) => Some(value),
            None => read_env("BLIT_EVENTS")?,
        };
        let activation = events
            .as_deref()
            .map(parse_activation)
            .transpose()?
            .unwrap_or_else(default_activation);
        let file = match overrides.file {
            Some(path) => Some(path),
            None => read_env("BLIT_EVENTS_FILE")?
                .map(|value| {
                    if value.is_empty() {
                        Err("BLIT_EVENTS_FILE must not be empty".to_string())
                    } else {
                        Ok(PathBuf::from(value))
                    }
                })
                .transpose()?,
        };
        Ok(Self {
            config: EventConfig {
                ring_size: records as u32,
                activation,
            },
            file,
        })
    }
}

fn parse_bytes(input: &str) -> Result<u64, String> {
    let trimmed = input.trim();
    let split = trimmed
        .find(|character: char| !character.is_ascii_digit())
        .unwrap_or(trimmed.len());
    let number = trimmed[..split]
        .parse::<u64>()
        .map_err(|_| format!("invalid BLIT_EVENTS_BYTES value {input:?}"))?;
    let suffix = trimmed[split..].trim().to_ascii_lowercase();
    let multiplier = match suffix.as_str() {
        "" | "b" => 1,
        "k" | "kb" | "kib" => 1024,
        "m" | "mb" | "mib" => 1024 * 1024,
        "g" | "gb" | "gib" => 1024 * 1024 * 1024,
        _ => return Err(format!("invalid BLIT_EVENTS_BYTES suffix {suffix:?}")),
    };
    number
        .checked_mul(multiplier)
        .ok_or_else(|| "BLIT_EVENTS_BYTES is too large".to_string())
}

fn parse_activation(input: &str) -> Result<Activation, String> {
    let selectors: Vec<_> = input
        .split([',', ' ', '\t', '\n'])
        .filter(|selector| !selector.is_empty())
        .collect();
    if selectors.is_empty() {
        return Err("BLIT_EVENTS must contain at least one selector".to_string());
    }
    let modifying = selectors[0].starts_with(['+', '-']);
    let mut activation = if modifying {
        default_activation()
    } else {
        Activation::NONE
    };
    for raw in selectors {
        let (active, selector) = match raw.as_bytes()[0] {
            b'+' => (true, &raw[1..]),
            b'-' => (false, &raw[1..]),
            _ => (true, raw),
        };
        if selector.is_empty() {
            return Err("empty BLIT_EVENTS selector".to_string());
        }
        match selector.to_ascii_lowercase().as_str() {
            "all" => {
                activation = if active {
                    Activation::ALL
                } else {
                    Activation::NONE
                }
            }
            "none" => {
                activation = if active {
                    Activation::NONE
                } else {
                    Activation::ALL
                }
            }
            "default" => {
                let defaults = default_activation();
                for event in EventId::ALL {
                    if defaults.contains(*event as u8) {
                        activation.set(*event as u8, active);
                    }
                }
            }
            name => {
                if let Some(family) = EventFamily::named(name) {
                    for event in EventId::ALL {
                        if event.family() == family {
                            activation.set(*event as u8, active);
                        }
                    }
                } else if let Some(event) = EventId::named(name) {
                    activation.set(event as u8, active);
                } else {
                    return Err(format!("unknown BLIT_EVENTS selector {selector:?}"));
                }
            }
        }
    }
    Ok(activation)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum FileStreamState {
    Starting,
    Running,
    Stopped,
    Failed,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct FileStreamStatus {
    pub stream_id: u32,
    pub state: FileStreamState,
    pub records_written: u64,
    pub bytes_written: u64,
    pub detail: String,
}

struct FileStreamProgress {
    state: Mutex<FileStreamState>,
    records_written: AtomicU64,
    bytes_written: AtomicU64,
    detail: Mutex<String>,
}

impl FileStreamProgress {
    fn new() -> Self {
        Self {
            state: Mutex::new(FileStreamState::Starting),
            records_written: AtomicU64::new(0),
            bytes_written: AtomicU64::new(0),
            detail: Mutex::new(String::new()),
        }
    }

    fn status(&self, stream_id: u32) -> FileStreamStatus {
        FileStreamStatus {
            stream_id,
            state: *self.state.lock().unwrap_or_else(|error| error.into_inner()),
            records_written: self.records_written.load(Ordering::Acquire),
            bytes_written: self.bytes_written.load(Ordering::Acquire),
            detail: self
                .detail
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .clone(),
        }
    }

    fn set_state(&self, state: FileStreamState, detail: impl Into<String>) {
        *self.state.lock().unwrap_or_else(|error| error.into_inner()) = state;
        *self
            .detail
            .lock()
            .unwrap_or_else(|error| error.into_inner()) = detail.into();
    }
}

struct FileStream {
    progress: Arc<FileStreamProgress>,
    stop: watch::Sender<bool>,
    task: JoinHandle<()>,
}

pub(crate) struct FileStreamManager {
    recorder: Arc<EventRecorder>,
    streams: AsyncMutex<HashMap<u32, FileStream>>,
    max_streams: usize,
    startup_file: Option<PathBuf>,
}

impl Default for FileStreamManager {
    fn default() -> Self {
        Self::with_startup_file(
            global_arc(),
            MAX_FILE_STREAMS,
            startup_file().map(Path::to_path_buf),
        )
    }
}

impl FileStreamManager {
    pub(crate) fn new(recorder: Arc<EventRecorder>, max_streams: usize) -> Self {
        Self::with_startup_file(recorder, max_streams, None)
    }

    pub(crate) fn with_startup_file(
        recorder: Arc<EventRecorder>,
        max_streams: usize,
        startup_file: Option<PathBuf>,
    ) -> Self {
        Self {
            recorder,
            streams: AsyncMutex::new(HashMap::new()),
            max_streams,
            startup_file,
        }
    }

    /// Starts the file selected during initialization as stream zero.
    pub(crate) async fn start_startup_file(&self) -> Result<Option<FileStreamStatus>, String> {
        match &self.startup_file {
            Some(path) => self.start(0, path.clone(), 0).await.map(Some),
            None => Ok(None),
        }
    }

    pub(crate) async fn start(
        &self,
        stream_id: u32,
        path: PathBuf,
        flags: u8,
    ) -> Result<FileStreamStatus, String> {
        if flags & !(FILE_APPEND | FILE_SYNC) != 0 {
            return Err("invalid event file flags".to_string());
        }
        let mut streams = self.streams.lock().await;
        if streams.contains_key(&stream_id) {
            return Err(format!("event file stream {stream_id} already exists"));
        }
        if streams.len() >= self.max_streams {
            return Err("too many event file streams".to_string());
        }
        let progress = Arc::new(FileStreamProgress::new());
        let (stop, receiver) = watch::channel(false);
        let task_progress = Arc::clone(&progress);
        let recorder = Arc::clone(&self.recorder);
        let (opened_tx, opened_rx) = oneshot::channel();
        let task = tokio::spawn(async move {
            if let Err(error) = file_stream_task(
                &recorder,
                &path,
                flags,
                receiver,
                &task_progress,
                Some(opened_tx),
            )
            .await
            {
                task_progress.set_state(FileStreamState::Failed, error.to_string());
            }
        });
        match opened_rx.await {
            Ok(Ok(())) => {}
            Ok(Err(error)) => {
                let _ = task.await;
                return Err(error);
            }
            Err(_) => {
                let _ = task.await;
                return Err("event file stream ended before opening the file".to_string());
            }
        }
        let status = progress.status(stream_id);
        streams.insert(
            stream_id,
            FileStream {
                progress,
                stop,
                task,
            },
        );
        Ok(status)
    }

    pub(crate) async fn status(&self, stream_id: u32) -> Option<FileStreamStatus> {
        self.streams
            .lock()
            .await
            .get(&stream_id)
            .map(|stream| stream.progress.status(stream_id))
    }

    pub(crate) async fn stop(&self, stream_id: u32) -> Result<FileStreamStatus, String> {
        let stream = self
            .streams
            .lock()
            .await
            .remove(&stream_id)
            .ok_or_else(|| format!("event file stream {stream_id} not found"))?;
        let _ = stream.stop.send(true);
        stream
            .task
            .await
            .map_err(|error| format!("event file stream task failed: {error}"))?;
        Ok(stream.progress.status(stream_id))
    }

    pub(crate) async fn shutdown(&self) -> Vec<FileStreamStatus> {
        let streams = {
            let mut guard = self.streams.lock().await;
            guard.drain().collect::<Vec<_>>()
        };
        for (_, stream) in &streams {
            let _ = stream.stop.send(true);
        }
        let mut statuses = Vec::with_capacity(streams.len());
        for (stream_id, stream) in streams {
            let _ = stream.task.await;
            statuses.push(stream.progress.status(stream_id));
        }
        statuses
    }
}

const CLIENT_STREAM_PACKET_RECORDS: usize = 256;
const MAX_CLIENT_STREAMS: usize = 8;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ClientStreamState {
    Starting,
    Replaying,
    Following,
    Stopped,
    Failed,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ClientStreamStatus {
    pub connection: u64,
    pub stream_id: u32,
    pub state: ClientStreamState,
    pub next_sequence: u64,
    pub records_sent: u64,
    pub gaps: u64,
    pub detail: String,
}

struct ClientStreamProgress {
    state: Mutex<ClientStreamState>,
    next_sequence: AtomicU64,
    records_sent: AtomicU64,
    gaps: AtomicU64,
    detail: Mutex<String>,
}

impl ClientStreamProgress {
    fn new(from_sequence: u64) -> Self {
        Self {
            state: Mutex::new(ClientStreamState::Starting),
            next_sequence: AtomicU64::new(from_sequence),
            records_sent: AtomicU64::new(0),
            gaps: AtomicU64::new(0),
            detail: Mutex::new(String::new()),
        }
    }

    fn status(&self, connection: u64, stream_id: u32) -> ClientStreamStatus {
        ClientStreamStatus {
            connection,
            stream_id,
            state: *self.state.lock().unwrap_or_else(|error| error.into_inner()),
            next_sequence: self.next_sequence.load(Ordering::Acquire),
            records_sent: self.records_sent.load(Ordering::Acquire),
            gaps: self.gaps.load(Ordering::Acquire),
            detail: self
                .detail
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .clone(),
        }
    }

    fn set_state(&self, state: ClientStreamState, detail: impl Into<String>) {
        *self.state.lock().unwrap_or_else(|error| error.into_inner()) = state;
        *self
            .detail
            .lock()
            .unwrap_or_else(|error| error.into_inner()) = detail.into();
    }
}

struct ClientStream {
    progress: Arc<ClientStreamProgress>,
    stop: watch::Sender<bool>,
    task: JoinHandle<()>,
}

/// Owns all event streams for one client connection.
pub(crate) struct ClientStreamManager {
    connection: u64,
    recorder: Arc<EventRecorder>,
    sender: mpsc::Sender<Vec<u8>>,
    streams: AsyncMutex<HashMap<u32, ClientStream>>,
    max_streams: usize,
}

impl ClientStreamManager {
    pub(crate) fn new(
        connection: u64,
        recorder: Arc<EventRecorder>,
        sender: mpsc::Sender<Vec<u8>>,
    ) -> Self {
        Self::with_limit(connection, recorder, sender, MAX_CLIENT_STREAMS)
    }

    pub(crate) fn with_limit(
        connection: u64,
        recorder: Arc<EventRecorder>,
        sender: mpsc::Sender<Vec<u8>>,
        max_streams: usize,
    ) -> Self {
        Self {
            connection,
            recorder,
            sender,
            streams: AsyncMutex::new(HashMap::new()),
            max_streams,
        }
    }

    pub(crate) async fn start(
        &self,
        _request_id: u32,
        stream_id: u32,
        from_sequence: u64,
        flags: u8,
    ) -> Result<ClientStreamStatus, String> {
        let (status, start) = self.prepare_start(stream_id, from_sequence, flags).await?;
        let _ = start.send(());
        Ok(status)
    }

    async fn prepare_start(
        &self,
        stream_id: u32,
        from_sequence: u64,
        flags: u8,
    ) -> Result<(ClientStreamStatus, oneshot::Sender<()>), String> {
        if flags & !STREAM_FLAGS != 0 {
            return Err("invalid event stream flags".to_string());
        }
        let mut streams = self.streams.lock().await;
        if streams.contains_key(&stream_id) {
            return Err(format!("event client stream {stream_id} already exists"));
        }
        if streams.len() >= self.max_streams {
            return Err("too many event client streams".to_string());
        }
        let cursor = if from_sequence == 0 {
            self.recorder.oldest_sequence()
        } else if from_sequence == u64::MAX {
            self.recorder.next_sequence.load(Ordering::Acquire)
        } else {
            from_sequence
        };
        let progress = Arc::new(ClientStreamProgress::new(cursor));
        let (stop, receiver) = watch::channel(false);
        let recorder = Arc::clone(&self.recorder);
        let sender = self.sender.clone();
        let task_progress = Arc::clone(&progress);
        let follow = flags & STREAM_FOLLOW != 0;
        let (start, begin) = oneshot::channel();
        let task = tokio::spawn(async move {
            if begin.await.is_err() {
                task_progress.set_state(ClientStreamState::Stopped, "");
                return;
            }
            client_stream_task(
                recorder,
                sender,
                stream_id,
                cursor,
                follow,
                receiver,
                task_progress,
            )
            .await;
        });
        let status = progress.status(self.connection, stream_id);
        streams.insert(
            stream_id,
            ClientStream {
                progress,
                stop,
                task,
            },
        );
        Ok((status, start))
    }

    pub(crate) async fn status(&self, stream_id: u32) -> Option<ClientStreamStatus> {
        self.streams
            .lock()
            .await
            .get(&stream_id)
            .map(|stream| stream.progress.status(self.connection, stream_id))
    }

    pub(crate) async fn stop(&self, stream_id: u32) -> Result<ClientStreamStatus, String> {
        let stream = self
            .streams
            .lock()
            .await
            .remove(&stream_id)
            .ok_or_else(|| format!("event client stream {stream_id} not found"))?;
        let _ = stream.stop.send(true);
        stream
            .task
            .await
            .map_err(|error| format!("event client stream task failed: {error}"))?;
        Ok(stream.progress.status(self.connection, stream_id))
    }

    pub(crate) async fn shutdown(&self) -> Vec<ClientStreamStatus> {
        let streams = {
            let mut guard = self.streams.lock().await;
            guard.drain().collect::<Vec<_>>()
        };
        for (_, stream) in &streams {
            let _ = stream.stop.send(true);
        }
        let mut statuses = Vec::with_capacity(streams.len());
        for (stream_id, stream) in streams {
            let _ = stream.task.await;
            statuses.push(stream.progress.status(self.connection, stream_id));
        }
        statuses
    }
}

fn operation_status(error: &str) -> u8 {
    if error.ends_with("not found") {
        blit_remote::STATUS_NOT_FOUND
    } else {
        blit_remote::STATUS_OTHER
    }
}

async fn send_protocol(sender: &mpsc::Sender<Vec<u8>>, packet: Vec<u8>) {
    let _ = sender.send(packet).await;
}

/// Handles one `blit.events.v1` request without taking the session mutex.
/// File opens, writes, flushes, and joins remain in spawned file tasks.
pub(crate) async fn dispatch(
    packet: &[u8],
    recorder: &Arc<EventRecorder>,
    client_streams: &Arc<ClientStreamManager>,
    file_streams: &Arc<FileStreamManager>,
    sender: &mpsc::Sender<Vec<u8>>,
) {
    let request = match parse_event_request(packet) {
        Ok(request) => request,
        Err(error) => {
            if let Some(kind) = packet.get(2).copied()
                && let Some(reply) = error.status_reply(kind)
            {
                send_protocol(sender, reply).await;
            }
            return;
        }
    };
    match request {
        EventRequest::ConfigGet { request_id } => {
            let reply = msg_event_config(request_id, blit_remote::STATUS_OK, recorder.config())
                .expect("recorder always has a valid event config");
            send_protocol(sender, reply).await;
        }
        EventRequest::ConfigSet { request_id, config } => {
            let status = recorder
                .set_config(config)
                .map(|()| blit_remote::STATUS_OK)
                .unwrap_or(blit_remote::STATUS_INVALID);
            if status == blit_remote::STATUS_OK {
                let words = activation_words(config.activation);
                recorder.record(
                    EventId::ConfigChanged,
                    0,
                    0,
                    0,
                    0,
                    request_id as u64,
                    [config.ring_size as u64, words[0], words[1]],
                );
            }
            let reply = msg_event_config(request_id, status, recorder.config())
                .expect("recorder always has a valid event config");
            send_protocol(sender, reply).await;
        }
        EventRequest::ConfigSetIf {
            request_id,
            expected,
            config,
        } => {
            let status = match recorder.set_config_if(expected, config) {
                Ok(true) => blit_remote::STATUS_OK,
                Ok(false) => blit_remote::STATUS_CONFLICT,
                Err(_) => blit_remote::STATUS_INVALID,
            };
            if status == blit_remote::STATUS_OK {
                let words = activation_words(config.activation);
                recorder.record(
                    EventId::ConfigChanged,
                    0,
                    0,
                    0,
                    0,
                    request_id as u64,
                    [config.ring_size as u64, words[0], words[1]],
                );
            }
            let reply = msg_event_config(request_id, status, recorder.config())
                .expect("recorder always has a valid event config");
            send_protocol(sender, reply).await;
        }
        EventRequest::Dump {
            request_id,
            from_sequence,
            limit,
        } => {
            let snapshot = recorder.snapshot(from_sequence, limit as usize);
            let status = if snapshot.overwritten != 0 || !snapshot.gaps.is_empty() {
                blit_remote::STATUS_BUDGET
            } else {
                blit_remote::STATUS_OK
            };
            let reply = msg_event_dump(
                request_id,
                status,
                snapshot.first_sequence,
                snapshot.next_sequence,
                &snapshot.records,
            )
            .expect("request decoder bounded the event dump");
            send_protocol(sender, reply).await;
        }
        EventRequest::StreamStart {
            request_id,
            stream_id,
            from_sequence,
            flags,
        } => match client_streams
            .prepare_start(stream_id, from_sequence, flags)
            .await
        {
            Ok((status, start)) => {
                send_protocol(
                    sender,
                    msg_event_stream_status(
                        request_id,
                        blit_remote::STATUS_OK,
                        stream_id,
                        status.next_sequence,
                    ),
                )
                .await;
                let _ = start.send(());
            }
            Err(error) => {
                send_protocol(
                    sender,
                    msg_event_stream_status(
                        request_id,
                        operation_status(&error),
                        stream_id,
                        from_sequence,
                    ),
                )
                .await;
            }
        },
        EventRequest::StreamStop {
            request_id,
            stream_id,
        } => {
            let result = client_streams.stop(stream_id).await;
            let (status, next_sequence) = match result {
                Ok(status) => (blit_remote::STATUS_OK, status.next_sequence),
                Err(error) => (operation_status(&error), 0),
            };
            send_protocol(
                sender,
                msg_event_stream_status(request_id, status, stream_id, next_sequence),
            )
            .await;
        }
        EventRequest::FileStart {
            request_id,
            stream_id,
            flags,
            path,
        } => {
            let result = file_streams
                .start(stream_id, PathBuf::from(path), flags)
                .await;
            let (status, records, bytes, detail) = match result {
                Ok(status) => (
                    blit_remote::STATUS_OK,
                    status.records_written,
                    status.bytes_written,
                    status.detail,
                ),
                Err(error) => (operation_status(&error), 0, 0, error),
            };
            let reply =
                msg_event_file_status(request_id, status, stream_id, records, bytes, &detail)
                    .expect("bounded event file status detail");
            send_protocol(sender, reply).await;
        }
        EventRequest::FileStop {
            request_id,
            stream_id,
        } => {
            let result = file_streams.stop(stream_id).await;
            let (status, records, bytes, detail) = match result {
                Ok(status) => (
                    blit_remote::STATUS_OK,
                    status.records_written,
                    status.bytes_written,
                    status.detail,
                ),
                Err(error) => (operation_status(&error), 0, 0, error),
            };
            if let Ok(reply) =
                msg_event_file_status(request_id, status, stream_id, records, bytes, &detail)
            {
                send_protocol(sender, reply).await;
            }
        }
    }
}

async fn send_stream_packet(
    sender: &mpsc::Sender<Vec<u8>>,
    packet: Vec<u8>,
    stop: &mut watch::Receiver<bool>,
) -> Result<(), &'static str> {
    tokio::select! {
        result = sender.send(packet) => result.map_err(|_| "client writer closed"),
        result = stop.changed() => {
            if result.is_err() || *stop.borrow() {
                Err("stream stopped")
            } else {
                Ok(())
            }
        }
    }
}

async fn client_stream_task(
    recorder: Arc<EventRecorder>,
    sender: mpsc::Sender<Vec<u8>>,
    stream_id: u32,
    mut cursor: u64,
    follow: bool,
    mut stop: watch::Receiver<bool>,
    progress: Arc<ClientStreamProgress>,
) {
    progress.set_state(ClientStreamState::Replaying, "");
    let mut changed = recorder.subscribe();
    loop {
        if *stop.borrow() {
            progress.set_state(ClientStreamState::Stopped, "");
            return;
        }
        let snapshot = recorder.snapshot(cursor, CLIENT_STREAM_PACKET_RECORDS);
        let missing = snapshot.overwritten
            + snapshot
                .gaps
                .iter()
                .map(|gap| gap.next_sequence - gap.first_sequence)
                .sum::<u64>();
        if missing != 0 {
            progress.gaps.fetch_add(missing, Ordering::Release);
            recorder.record(
                EventId::StreamGap,
                0,
                0,
                0,
                0,
                stream_id as u64,
                [missing, snapshot.first_sequence, snapshot.next_sequence],
            );
            let gap_edge = snapshot
                .gaps
                .last()
                .map_or(snapshot.first_sequence, |gap| gap.next_sequence);
            let packet =
                msg_event_stream_status(0, blit_remote::STATUS_BUDGET, stream_id, gap_edge);
            if let Err(error) = send_stream_packet(&sender, packet, &mut stop).await {
                let state = if error == "stream stopped" {
                    ClientStreamState::Stopped
                } else {
                    ClientStreamState::Failed
                };
                progress.set_state(state, error);
                return;
            }
        }
        if !snapshot.records.is_empty() {
            let count = snapshot.records.len() as u64;
            let packet =
                msg_event_stream_data(stream_id, recorder.monotonic_ns(), &snapshot.records)
                    .expect("stream packet is capped below the codec limit");
            if let Err(error) = send_stream_packet(&sender, packet, &mut stop).await {
                let state = if error == "stream stopped" {
                    ClientStreamState::Stopped
                } else {
                    ClientStreamState::Failed
                };
                progress.set_state(state, error);
                return;
            }
            progress.records_sent.fetch_add(count, Ordering::Release);
        }
        cursor = snapshot.next_sequence;
        progress.next_sequence.store(cursor, Ordering::Release);
        if cursor < recorder.next_sequence.load(Ordering::Acquire) {
            continue;
        }
        if !follow {
            progress.set_state(ClientStreamState::Stopped, "");
            return;
        }
        progress.set_state(ClientStreamState::Following, "");
        tokio::select! {
            result = changed.changed() => {
                if result.is_err() {
                    progress.set_state(ClientStreamState::Failed, "event recorder closed");
                    return;
                }
            },
            result = stop.changed() => {
                if result.is_err() || *stop.borrow() {
                    progress.set_state(ClientStreamState::Stopped, "");
                    return;
                }
            }
        }
    }
}

async fn open_event_file(
    path: &Path,
    flags: u8,
    progress: &FileStreamProgress,
) -> io::Result<File> {
    let append = flags & FILE_APPEND != 0;
    let mut options = OpenOptions::new();
    options.create(true).read(true).write(true);
    if !append {
        options.truncate(true);
    }
    let mut file = options.open(path).await?;
    let length = file.metadata().await?.len();
    if append && length != 0 {
        if length < EVENT_FILE_HEADER_SIZE as u64
            || !(length - EVENT_FILE_HEADER_SIZE as u64).is_multiple_of(EVENT_RECORD_SIZE as u64)
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "existing event file has an invalid length",
            ));
        }
        let mut header = [0; EVENT_FILE_HEADER_SIZE];
        file.read_exact(&mut header).await?;
        EventFileHeader::decode(&header)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error.to_string()))?;
        file.seek(std::io::SeekFrom::End(0)).await?;
    } else {
        file.write_all(&EventFileHeader::CANONICAL.encode()).await?;
        progress
            .bytes_written
            .fetch_add(EVENT_FILE_HEADER_SIZE as u64, Ordering::Release);
    }
    Ok(file)
}

async fn file_stream_task(
    recorder: &EventRecorder,
    path: &Path,
    flags: u8,
    mut stop: watch::Receiver<bool>,
    progress: &FileStreamProgress,
    opened: Option<oneshot::Sender<Result<(), String>>>,
) -> io::Result<()> {
    let mut file = match open_event_file(path, flags, progress).await {
        Ok(file) => {
            progress.set_state(FileStreamState::Running, "");
            if let Some(opened) = opened {
                let _ = opened.send(Ok(()));
            }
            file
        }
        Err(error) => {
            if let Some(opened) = opened {
                let _ = opened.send(Err(error.to_string()));
            }
            return Err(error);
        }
    };
    let mut changed = recorder.subscribe();
    let mut cursor = recorder.oldest_sequence();
    let mut stop_edge = None;
    loop {
        if *stop.borrow() && stop_edge.is_none() {
            stop_edge = Some(recorder.next_sequence.load(Ordering::Acquire));
        }
        let snapshot = recorder.snapshot(cursor, 1024);
        if !snapshot.records.is_empty() {
            for record in &snapshot.records {
                file.write_all(&record.encode()).await?;
            }
            let count = snapshot.records.len() as u64;
            progress.records_written.fetch_add(count, Ordering::Release);
            progress
                .bytes_written
                .fetch_add(count * EVENT_RECORD_SIZE as u64, Ordering::Release);
        }
        cursor = snapshot.next_sequence;
        if flags & FILE_SYNC != 0 && !snapshot.records.is_empty() {
            file.sync_data().await?;
        }
        if stop_edge.is_some_and(|edge| cursor >= edge) {
            break;
        }
        if cursor < recorder.next_sequence.load(Ordering::Acquire) {
            continue;
        }
        tokio::select! {
            result = changed.changed() => {
                if result.is_err() {
                    break;
                }
            },
            result = stop.changed() => {
                if result.is_err() || *stop.borrow() {
                    stop_edge.get_or_insert_with(|| recorder.next_sequence.load(Ordering::Acquire));
                }
            }
        }
    }
    file.flush().await?;
    if flags & FILE_SYNC != 0 {
        file.sync_data().await?;
    }
    progress.set_state(FileStreamState::Stopped, "");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;
    use std::thread;
    use std::time::Duration;

    fn recorder(size: u32, activation: Activation) -> EventRecorder {
        EventRecorder::new(EventConfig {
            ring_size: size,
            activation,
        })
        .unwrap()
    }

    fn all_recorder(size: u32) -> EventRecorder {
        recorder(size, Activation::ALL)
    }

    fn record_value(recorder: &EventRecorder, value: u64) {
        assert!(recorder.record(EventId::RawRequestRead, 2, 3, 4, 5, 6, [value, 8, 9]));
    }

    #[test]
    fn default_is_one_mib_and_only_low_rate_lifecycle() {
        let recorder = EventRecorder::default();
        assert_eq!(recorder.config().ring_size, 16_384);
        assert!(recorder.enabled(EventId::ServerStarted));
        assert!(recorder.enabled(EventId::ServerStopping));
        assert!(recorder.enabled(EventId::ClientConnected));
        assert!(!recorder.enabled(EventId::RawRequestRead));
        assert!(!recorder.enabled(EventId::PtyRead));
    }

    #[test]
    fn record_round_trips_all_fields() {
        let recorder = all_recorder(4);
        record_value(&recorder, 7);
        let snapshot = recorder.snapshot(1, 10);
        assert_eq!(snapshot.first_sequence, 1);
        assert_eq!(snapshot.next_sequence, 2);
        assert_eq!(snapshot.gaps, vec![]);
        let record = snapshot.records[0];
        assert_eq!(record.sequence, 1);
        assert_eq!(record.event_id, EventId::RawRequestRead as u32);
        assert_eq!(record.flags, 2);
        assert_eq!(record.source, 3);
        assert_eq!(record.schema, 4);
        assert_eq!(record.connection, 5);
        assert_eq!(record.subject, 6);
        assert_eq!(record.args, [7, 8, 9]);
    }

    #[test]
    fn ring_reports_overwrite_and_returns_ordered_records() {
        let recorder = all_recorder(3);
        for value in 0..6 {
            record_value(&recorder, value);
        }
        let snapshot = recorder.snapshot(1, 10);
        assert_eq!(snapshot.first_sequence, 4);
        assert_eq!(snapshot.next_sequence, 7);
        assert_eq!(snapshot.overwritten, 3);
        assert_eq!(
            snapshot
                .records
                .iter()
                .map(|record| record.sequence)
                .collect::<Vec<_>>(),
            vec![4, 5, 6]
        );
        assert_eq!(
            snapshot
                .records
                .iter()
                .map(|record| record.args[0])
                .collect::<Vec<_>>(),
            vec![3, 4, 5]
        );
    }

    #[test]
    fn snapshot_reports_complete_record_gaps() {
        let recorder = all_recorder(4);
        record_value(&recorder, 1);
        recorder.next_sequence.fetch_add(2, Ordering::Relaxed);
        record_value(&recorder, 4);
        let snapshot = recorder.snapshot(1, 10);
        assert_eq!(snapshot.records.len(), 2);
        assert_eq!(
            snapshot.gaps,
            vec![SequenceGap {
                first_sequence: 2,
                next_sequence: 4,
            }]
        );
    }

    #[test]
    fn resize_preserves_newest_records() {
        let recorder = all_recorder(5);
        for value in 0..5 {
            record_value(&recorder, value);
        }
        recorder
            .set_config(EventConfig {
                ring_size: 3,
                activation: Activation::ALL,
            })
            .unwrap();
        let snapshot = recorder.snapshot(1, 10);
        assert_eq!(snapshot.overwritten, 2);
        assert_eq!(
            snapshot
                .records
                .iter()
                .map(|record| record.args[0])
                .collect::<Vec<_>>(),
            vec![2, 3, 4]
        );
        recorder
            .set_config(EventConfig {
                ring_size: 8,
                activation: Activation::ALL,
            })
            .unwrap();
        assert_eq!(recorder.snapshot(1, 10).records.len(), 3);
    }

    #[test]
    fn activation_can_change_at_runtime() {
        let recorder = recorder(4, Activation::NONE);
        assert!(!recorder.record(EventId::PtyCreateRegistered, 0, 0, 0, 0, 0, [0; 3]));
        let mut activation = Activation::NONE;
        activation.set(EventId::PtyCreateRegistered as u8, true);
        recorder
            .set_config(EventConfig {
                ring_size: 4,
                activation,
            })
            .unwrap();
        assert!(recorder.record(EventId::PtyCreateRegistered, 0, 0, 0, 0, 0, [0; 3]));
    }

    #[test]
    fn conditional_config_set_is_atomic() {
        let recorder = recorder(4, Activation::NONE);
        let initial = recorder.config();
        let replacement = EventConfig {
            ring_size: 8,
            activation: Activation::ALL,
        };
        assert_eq!(recorder.set_config_if(initial, replacement), Ok(true));
        assert_eq!(recorder.config(), replacement);
        assert_eq!(recorder.set_config_if(initial, initial), Ok(false));
        assert_eq!(recorder.config(), replacement);
    }

    #[test]
    fn activation_parser_supports_events_families_and_modifiers() {
        let activation = parse_activation("none,pty,+task-failed").unwrap();
        assert!(activation.contains(EventId::PtyCreateRegistered as u8));
        assert!(activation.contains(EventId::PtyRead as u8));
        assert!(activation.contains(EventId::TaskFailed as u8));
        assert!(!activation.contains(EventId::ServerStarted as u8));

        let activation = parse_activation("-config-changed,+request").unwrap();
        assert!(activation.contains(EventId::ServerStarted as u8));
        assert!(!activation.contains(EventId::ConfigChanged as u8));
        assert!(activation.contains(EventId::RawRequestDone as u8));
        assert!(parse_activation("wat").is_err());
    }

    #[test]
    fn environment_resolution_honors_typed_overrides() {
        let config = EventStartupConfig::resolve_with(
            EventConfigOverrides {
                ring_bytes: Some(128),
                events: Some("process-exit".to_string()),
                file: Some(PathBuf::from("override.events")),
            },
            |name| {
                Ok(match name {
                    "BLIT_EVENTS_BYTES" => Some("1MiB".to_string()),
                    "BLIT_EVENTS" => Some("all".to_string()),
                    "BLIT_EVENTS_FILE" => Some("env.events".to_string()),
                    _ => None,
                })
            },
        )
        .unwrap();
        assert_eq!(config.config.ring_size, 2);
        assert!(
            config
                .config
                .activation
                .contains(EventId::ProcessExit as u8)
        );
        assert!(!config.config.activation.contains(EventId::PtyRead as u8));
        assert_eq!(config.file, Some(PathBuf::from("override.events")));
        assert_eq!(parse_bytes("1MiB").unwrap(), DEFAULT_RING_BYTES);
        assert!(parse_bytes("63").is_ok());
    }

    #[test]
    fn invalid_ring_configuration_is_rejected() {
        assert!(
            EventRecorder::new(EventConfig {
                ring_size: 0,
                activation: Activation::NONE
            })
            .is_err()
        );
        assert!(
            EventStartupConfig::resolve_with(
                EventConfigOverrides {
                    ring_bytes: Some(65),
                    ..Default::default()
                },
                |_| Ok(None)
            )
            .is_err()
        );
    }

    #[test]
    fn disabled_record_does_not_advance_sequence() {
        let recorder = recorder(2, Activation::NONE);
        assert!(!recorder.record(EventId::PtyRead, 0, 0, 0, 0, 0, [0; 3]));
        assert_eq!(recorder.next_sequence.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn disabled_macro_arguments_are_not_evaluated() {
        static CALLS: AtomicUsize = AtomicUsize::new(0);
        fn argument() -> u64 {
            CALLS.fetch_add(1, Ordering::Relaxed);
            1
        }
        assert!(!blit_event_enabled!(EventId::PtyRead));
        assert!(!blit_event!(
            EventId::PtyRead,
            argument(),
            argument(),
            argument(),
            argument(),
            argument()
        ));
        assert_eq!(CALLS.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn concurrent_writers_produce_complete_globally_ordered_records() {
        let recorder = Arc::new(all_recorder(8192));
        let writers = 8;
        let per_writer = 500;
        let mut threads = Vec::new();
        for writer in 0..writers {
            let recorder = Arc::clone(&recorder);
            threads.push(thread::spawn(move || {
                for value in 0..per_writer {
                    while !recorder.record(
                        EventId::TaskCompleted,
                        0,
                        writer as u8,
                        0,
                        writer,
                        value,
                        [writer, value, writer ^ value],
                    ) {
                        thread::yield_now();
                    }
                }
            }));
        }
        for thread in threads {
            thread.join().unwrap();
        }
        let snapshot = recorder.snapshot(1, writers as usize * per_writer as usize);
        assert!(snapshot.gaps.is_empty());
        assert_eq!(
            snapshot.records.len(),
            writers as usize * per_writer as usize
        );
        for (index, record) in snapshot.records.iter().enumerate() {
            assert_eq!(record.sequence, index as u64 + 1);
            assert_eq!(record.args[2], record.args[0] ^ record.args[1]);
            assert_eq!(record.source as u64, record.connection);
        }
    }

    #[tokio::test]
    async fn notification_wakes_a_consumer() {
        let recorder = all_recorder(4);
        let mut changed = recorder.subscribe();
        record_value(&recorder, 1);
        tokio::time::timeout(Duration::from_secs(1), changed.changed())
            .await
            .unwrap()
            .unwrap();
    }

    #[tokio::test]
    async fn file_stream_writes_header_records_and_flushes_on_stop() {
        let recorder = Arc::new(all_recorder(16));
        record_value(&recorder, 1);
        let manager = FileStreamManager::new(Arc::clone(&recorder), 1);
        let path = std::env::temp_dir().join(format!(
            "blit-events-{}-{}.bin",
            std::process::id(),
            recorder.started.elapsed().as_nanos()
        ));
        manager.start(7, path.clone(), 0).await.unwrap();
        for _ in 0..100 {
            if manager
                .status(7)
                .await
                .is_some_and(|status| status.records_written == 1)
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        let status = manager.stop(7).await.unwrap();
        assert_eq!(status.state, FileStreamState::Stopped);
        assert_eq!(status.records_written, 1);
        let bytes = tokio::fs::read(&path).await.unwrap();
        EventFileHeader::decode(&bytes[..EVENT_FILE_HEADER_SIZE]).unwrap();
        assert_eq!(bytes.len(), EVENT_FILE_HEADER_SIZE + EVENT_RECORD_SIZE);
        let record = EventRecord::decode(&bytes[EVENT_FILE_HEADER_SIZE..]).unwrap();
        assert_eq!(record.args[0], 1);
        let _ = tokio::fs::remove_file(path).await;
    }

    #[tokio::test]
    async fn client_stream_replays_then_follows_notifications() {
        use blit_remote::events::{EventMessage, parse_event_message};

        let recorder = Arc::new(all_recorder(16));
        record_value(&recorder, 1);
        let (sender, mut packets) = mpsc::channel(2);
        let manager = ClientStreamManager::new(44, Arc::clone(&recorder), sender);
        manager.start(7, 9, 1, STREAM_FOLLOW).await.unwrap();

        let packet = tokio::time::timeout(Duration::from_secs(1), packets.recv())
            .await
            .unwrap()
            .unwrap();
        let EventMessage::StreamData {
            stream_id, records, ..
        } = parse_event_message(&packet).unwrap()
        else {
            panic!("expected stream data");
        };
        assert_eq!(stream_id, 9);
        assert_eq!(
            records
                .iter()
                .map(|record| record.args[0])
                .collect::<Vec<_>>(),
            vec![1]
        );

        record_value(&recorder, 2);
        let packet = tokio::time::timeout(Duration::from_secs(1), packets.recv())
            .await
            .unwrap()
            .unwrap();
        let EventMessage::StreamData { records, .. } = parse_event_message(&packet).unwrap() else {
            panic!("expected stream data");
        };
        assert_eq!(records[0].args[0], 2);

        let statuses = manager.shutdown().await;
        assert_eq!(statuses.len(), 1);
        assert_eq!(statuses[0].connection, 44);
        assert_eq!(statuses[0].state, ClientStreamState::Stopped);
        assert_eq!(statuses[0].records_sent, 2);
    }

    #[tokio::test]
    async fn client_stream_reports_replay_gaps_before_data() {
        use blit_remote::events::{EventMessage, parse_event_message};

        let recorder = Arc::new(all_recorder(2));
        for value in 0..4 {
            record_value(&recorder, value);
        }
        let (sender, mut packets) = mpsc::channel(2);
        let manager = ClientStreamManager::new(1, Arc::clone(&recorder), sender);
        manager.start(11, 12, 1, 0).await.unwrap();

        let packet = tokio::time::timeout(Duration::from_secs(1), packets.recv())
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(
            parse_event_message(&packet).unwrap(),
            EventMessage::StreamStatus {
                request_id: 0,
                status: blit_remote::STATUS_BUDGET,
                stream_id: 12,
                next_sequence: 3,
            }
        ));
        let packet = tokio::time::timeout(Duration::from_secs(1), packets.recv())
            .await
            .unwrap()
            .unwrap();
        let EventMessage::StreamData { records, .. } = parse_event_message(&packet).unwrap() else {
            panic!("expected stream data");
        };
        assert_eq!(
            records
                .iter()
                .map(|record| record.sequence)
                .collect::<Vec<_>>(),
            vec![3, 4]
        );

        let status = manager.stop(12).await.unwrap();
        assert_eq!(status.gaps, 2);
        assert_eq!(status.next_sequence, 5);
    }

    #[tokio::test]
    async fn client_stream_shutdown_cancels_a_blocked_writer() {
        let recorder = Arc::new(all_recorder(4));
        record_value(&recorder, 1);
        let (sender, _packets) = mpsc::channel(1);
        sender.send(vec![0]).await.unwrap();
        let manager = ClientStreamManager::new(1, recorder, sender);
        manager.start(1, 2, 1, STREAM_FOLLOW).await.unwrap();

        let statuses = tokio::time::timeout(Duration::from_secs(1), manager.shutdown())
            .await
            .unwrap();
        assert_eq!(statuses[0].state, ClientStreamState::Stopped);
    }

    #[tokio::test]
    async fn protocol_dispatch_correlates_config_dump_and_errors() {
        use blit_remote::events::{
            EventMessage, msg_config_get, msg_config_set_if, msg_dump, parse_event_message,
        };

        let recorder = Arc::new(all_recorder(8));
        record_value(&recorder, 17);
        let (sender, mut packets) = mpsc::channel(8);
        let client_streams = Arc::new(ClientStreamManager::new(
            9,
            Arc::clone(&recorder),
            sender.clone(),
        ));
        let file_streams = Arc::new(FileStreamManager::new(Arc::clone(&recorder), 1));

        dispatch(
            &msg_config_get(41),
            &recorder,
            &client_streams,
            &file_streams,
            &sender,
        )
        .await;
        assert!(matches!(
            parse_event_message(&packets.recv().await.unwrap()).unwrap(),
            EventMessage::Config {
                request_id: 41,
                status: blit_remote::STATUS_OK,
                ..
            }
        ));

        let initial = recorder.config();
        let replacement = EventConfig {
            ring_size: 16,
            activation: Activation::NONE,
        };
        dispatch(
            &msg_config_set_if(44, initial, replacement).unwrap(),
            &recorder,
            &client_streams,
            &file_streams,
            &sender,
        )
        .await;
        assert!(matches!(
            parse_event_message(&packets.recv().await.unwrap()).unwrap(),
            EventMessage::Config {
                request_id: 44,
                status: blit_remote::STATUS_OK,
                config,
            } if config == replacement
        ));
        dispatch(
            &msg_config_set_if(45, initial, initial).unwrap(),
            &recorder,
            &client_streams,
            &file_streams,
            &sender,
        )
        .await;
        assert!(matches!(
            parse_event_message(&packets.recv().await.unwrap()).unwrap(),
            EventMessage::Config {
                request_id: 45,
                status: blit_remote::STATUS_CONFLICT,
                config,
            } if config == replacement
        ));

        dispatch(
            &msg_dump(42, 1, 4).unwrap(),
            &recorder,
            &client_streams,
            &file_streams,
            &sender,
        )
        .await;
        assert!(matches!(
            parse_event_message(&packets.recv().await.unwrap()).unwrap(),
            EventMessage::Dump {
                request_id: 42,
                status: blit_remote::STATUS_OK,
                ref records,
                ..
            } if records.len() == 1 && records[0].args[0] == 17
        ));

        let mut malformed = msg_config_get(43);
        malformed.push(0);
        dispatch(
            &malformed,
            &recorder,
            &client_streams,
            &file_streams,
            &sender,
        )
        .await;
        assert!(matches!(
            parse_event_message(&packets.recv().await.unwrap()).unwrap(),
            EventMessage::Status {
                request_id: 43,
                request_kind: C2S_CONFIG_GET,
                status: blit_remote::STATUS_INVALID,
            }
        ));
    }

    #[tokio::test]
    async fn file_stream_limit_append_validation_and_shutdown() {
        let recorder = Arc::new(all_recorder(4));
        let manager = FileStreamManager::new(Arc::clone(&recorder), 1);
        let base = std::env::temp_dir().join(format!(
            "blit-events-limit-{}-{}",
            std::process::id(),
            recorder.started.elapsed().as_nanos()
        ));
        manager.start(1, base.clone(), 0).await.unwrap();
        assert!(
            manager
                .start(2, base.with_extension("two"), 0)
                .await
                .is_err()
        );
        let statuses = manager.shutdown().await;
        assert_eq!(statuses.len(), 1);
        assert_eq!(statuses[0].state, FileStreamState::Stopped);

        tokio::fs::write(&base, b"not an event file").await.unwrap();
        let manager = FileStreamManager::new(Arc::clone(&recorder), 1);
        assert!(manager.start(3, base.clone(), FILE_APPEND).await.is_err());
        assert!(manager.status(3).await.is_none());
        let _ = tokio::fs::remove_file(base).await;
    }

    #[tokio::test]
    async fn one_append_wakes_every_client_stream() {
        use blit_remote::events::{EventMessage, parse_event_message};

        let recorder = Arc::new(all_recorder(8));
        let (sender_a, mut packets_a) = mpsc::channel(4);
        let (sender_b, mut packets_b) = mpsc::channel(4);
        let manager_a = ClientStreamManager::new(1, Arc::clone(&recorder), sender_a);
        let manager_b = ClientStreamManager::new(2, Arc::clone(&recorder), sender_b);
        manager_a
            .start(1, 10, u64::MAX, STREAM_FOLLOW)
            .await
            .unwrap();
        manager_b
            .start(2, 20, u64::MAX, STREAM_FOLLOW)
            .await
            .unwrap();

        record_value(&recorder, 77);
        for packets in [&mut packets_a, &mut packets_b] {
            let packet = tokio::time::timeout(Duration::from_secs(1), packets.recv())
                .await
                .unwrap()
                .unwrap();
            let EventMessage::StreamData { records, .. } = parse_event_message(&packet).unwrap()
            else {
                panic!("expected stream data");
            };
            assert_eq!(records.len(), 1);
            assert_eq!(records[0].args[0], 77);
        }

        manager_a.shutdown().await;
        manager_b.shutdown().await;
    }

    #[tokio::test]
    async fn file_start_validates_before_success_and_stop_frees_the_slot() {
        let recorder = Arc::new(all_recorder(8));
        let manager = FileStreamManager::new(Arc::clone(&recorder), 1);
        let base = std::env::temp_dir().join(format!(
            "blit-events-stop-{}-{}",
            std::process::id(),
            recorder.started.elapsed().as_nanos()
        ));

        assert!(manager.start(1, std::env::temp_dir(), 0).await.is_err());
        assert!(manager.status(1).await.is_none());

        manager.start(2, base.clone(), 0).await.unwrap();
        record_value(&recorder, 91);
        let stopped = manager.stop(2).await.unwrap();
        assert_eq!(stopped.state, FileStreamState::Stopped);
        assert_eq!(stopped.records_written, 1);

        manager.start(3, base.clone(), 0).await.unwrap();
        let stopped = manager.stop(3).await.unwrap();
        assert_eq!(stopped.state, FileStreamState::Stopped);
        let _ = tokio::fs::remove_file(base).await;
    }
}
