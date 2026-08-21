//! Process-wide bounded binary event journal.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU8, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use blit_remote::events::{
    ACTIVATION_WORDS, ActivationSet, EVENT_DUMP_HEADER_LEN, EVENT_DUMP_MAGIC,
    EVENT_RECORD_HEADER_LEN, EVENT_TYPE_STREAM_GAP, EVENTS_STREAM_APPEND, EVENTS_STREAM_HISTORY,
    EventType, parse_activation_spec,
};
use tokio::io::AsyncWriteExt;
use tokio::sync::{broadcast, oneshot};

pub(crate) const DEFAULT_RING_SIZE: usize = 1024 * 1024;
pub(crate) const MIN_RING_SIZE: usize = 4 * 1024;
pub(crate) const MAX_RING_SIZE: usize = blit_remote::MAX_LOGICAL_MESSAGE - 4096;
const LIVE_CHANNEL_RECORDS: usize = 4096;
pub(crate) const LIVE_BATCH_MAX_BYTES: usize = 256 * 1024;

#[derive(Clone, Debug)]
pub(crate) struct StartupFile {
    pub path: String,
    pub flags: u8,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct EventStats {
    pub revision: u64,
    pub capacity: usize,
    pub used: usize,
    pub records: u64,
    pub dropped: u64,
    pub next_sequence: u64,
}

struct Ring {
    bytes: Box<[u8]>,
    head: usize,
    used: usize,
    records: u64,
    dropped: u64,
}

impl Ring {
    fn new(capacity: usize) -> Self {
        Self {
            bytes: vec![0; capacity].into_boxed_slice(),
            head: 0,
            used: 0,
            records: 0,
            dropped: 0,
        }
    }

    fn capacity(&self) -> usize {
        self.bytes.len()
    }

    fn copy_out(&self, at: usize, len: usize, out: &mut Vec<u8>) {
        let first = len.min(self.capacity() - at);
        out.extend_from_slice(&self.bytes[at..at + first]);
        if first < len {
            out.extend_from_slice(&self.bytes[..len - first]);
        }
    }

    fn bytes_at(&self, at: usize, len: usize) -> Vec<u8> {
        let mut out = Vec::with_capacity(len);
        self.copy_out(at, len, &mut out);
        out
    }

    fn read_u32(&self, at: usize) -> u32 {
        let mut bytes = [0; 4];
        let first = bytes.len().min(self.capacity() - at);
        bytes[..first].copy_from_slice(&self.bytes[at..at + first]);
        if first < bytes.len() {
            let remaining = bytes.len() - first;
            bytes[first..].copy_from_slice(&self.bytes[..remaining]);
        }
        u32::from_le_bytes(bytes)
    }

    fn write_at(&mut self, at: usize, data: &[u8]) {
        let first = data.len().min(self.capacity() - at);
        self.bytes[at..at + first].copy_from_slice(&data[..first]);
        if first < data.len() {
            self.bytes[..data.len() - first].copy_from_slice(&data[first..]);
        }
    }

    fn oldest_len(&self) -> Option<usize> {
        if self.used < 4 {
            return None;
        }
        let len = self.read_u32(self.head) as usize;
        (len >= EVENT_RECORD_HEADER_LEN && len <= self.used).then_some(len)
    }

    fn evict_oldest(&mut self) {
        let Some(len) = self.oldest_len() else {
            // A corrupt in-memory prefix cannot be recovered record-by-record.
            self.head = 0;
            self.used = 0;
            self.records = 0;
            self.dropped = self.dropped.saturating_add(1);
            return;
        };
        self.head = (self.head + len) % self.capacity();
        self.used -= len;
        self.records = self.records.saturating_sub(1);
        self.dropped = self.dropped.saturating_add(1);
    }

    fn append(&mut self, record: &[u8]) -> bool {
        self.append_parts(record, &[])
    }

    fn append_parts(&mut self, header: &[u8], payload: &[u8]) -> bool {
        let Some(len) = header.len().checked_add(payload.len()) else {
            self.dropped = self.dropped.saturating_add(1);
            return false;
        };
        if len > self.capacity() {
            self.dropped = self.dropped.saturating_add(1);
            return false;
        }
        while self.capacity() - self.used < len {
            self.evict_oldest();
        }
        let tail = (self.head + self.used) % self.capacity();
        self.write_at(tail, header);
        self.write_at((tail + header.len()) % self.capacity(), payload);
        self.used += len;
        self.records = self.records.saturating_add(1);
        true
    }

    fn record_vecs(&self) -> Vec<Vec<u8>> {
        let mut result = Vec::with_capacity(self.records.min(usize::MAX as u64) as usize);
        let mut at = self.head;
        let mut left = self.used;
        while left >= EVENT_RECORD_HEADER_LEN {
            let len = self.read_u32(at) as usize;
            if len < EVENT_RECORD_HEADER_LEN || len > left {
                break;
            }
            result.push(self.bytes_at(at, len));
            at = (at + len) % self.capacity();
            left -= len;
        }
        result
    }

    fn resize(&mut self, capacity: usize) {
        if capacity == self.capacity() {
            return;
        }
        let records = self.record_vecs();
        let mut replacement = Ring::new(capacity);
        replacement.dropped = self.dropped;
        for record in records {
            replacement.append(&record);
        }
        *self = replacement;
    }
}

pub(crate) struct EventLog {
    activations: [AtomicU64; ACTIVATION_WORDS],
    ring: Mutex<Ring>,
    config_revision: AtomicU64,
    next_sequence: AtomicU64,
    next_stream_id: AtomicU32,
    started: Instant,
    started_unix_ns: u64,
    live_tx: broadcast::Sender<Arc<[u8]>>,
    file_streams: Mutex<HashMap<u32, FileStreamTask>>,
}

struct FileStreamTask {
    stop: oneshot::Sender<()>,
    join: tokio::task::JoinHandle<Result<(), String>>,
    path: String,
    flags: u8,
    progress: Arc<FileStreamProgress>,
}

type ClientEventStream = (u32, Option<Vec<u8>>, broadcast::Receiver<Arc<[u8]>>);
type EventSubscription = (Option<Vec<u8>>, u64, broadcast::Receiver<Arc<[u8]>>);

struct FileStreamProgress {
    state: AtomicU8,
    records: AtomicU64,
    bytes: AtomicU64,
    lost: AtomicU64,
    error: Mutex<Option<String>>,
}

impl FileStreamProgress {
    fn new(records: u64, bytes: u64) -> Arc<Self> {
        Arc::new(Self {
            state: AtomicU8::new(blit_remote::events::EVENTS_STREAM_STATE_RUNNING),
            records: AtomicU64::new(records),
            bytes: AtomicU64::new(bytes),
            lost: AtomicU64::new(0),
            error: Mutex::new(None),
        })
    }

    fn fail(&self, error: String) -> String {
        self.state.store(
            blit_remote::events::EVENTS_STREAM_STATE_FAILED,
            Ordering::Release,
        );
        *self.error.lock().expect("event file progress poisoned") = Some(error.clone());
        error
    }

    fn stop(&self) {
        self.state.store(
            blit_remote::events::EVENTS_STREAM_STATE_STOPPED,
            Ordering::Release,
        );
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ConfigureError {
    InvalidSize,
    Conflict,
}

impl std::fmt::Display for ConfigureError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::InvalidSize => "ring size is outside the supported range",
            Self::Conflict => "event configuration revision changed",
        })
    }
}

#[derive(Debug)]
pub(crate) enum StartFileStreamError {
    Io(String),
}

impl std::fmt::Display for StartFileStreamError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(error) => f.write_str(error),
        }
    }
}

pub(crate) struct FileStreamInfo {
    pub id: u32,
    pub state: u8,
    pub flags: u8,
    pub records: u64,
    pub bytes: u64,
    pub lost: u64,
    pub path: String,
    pub error: String,
}

impl EventLog {
    pub(crate) fn from_env() -> (Arc<Self>, Option<StartupFile>) {
        let size = std::env::var("BLIT_EVENTS_SIZE")
            .ok()
            .and_then(|value| value.parse::<usize>().ok())
            .filter(|size| (MIN_RING_SIZE..=MAX_RING_SIZE).contains(size))
            .unwrap_or(DEFAULT_RING_SIZE);
        let activations = match std::env::var("BLIT_EVENTS") {
            Ok(spec) => match parse_activation_spec(&spec) {
                Ok(set) => set,
                Err(error) => {
                    eprintln!("blit-server: ignoring invalid BLIT_EVENTS: {error}");
                    ActivationSet::low_throughput()
                }
            },
            Err(_) => ActivationSet::low_throughput(),
        };
        let file = std::env::var("BLIT_EVENTS_FILE")
            .ok()
            .filter(|path| !path.is_empty())
            .map(|path| {
                let mut flags = 0;
                if !std::env::var("BLIT_EVENTS_FILE_HISTORY").is_ok_and(|value| value == "0") {
                    flags |= EVENTS_STREAM_HISTORY;
                }
                if std::env::var("BLIT_EVENTS_FILE_APPEND").is_ok_and(|value| value == "1") {
                    flags |= EVENTS_STREAM_APPEND;
                }
                StartupFile { path, flags }
            });
        let (live_tx, _) = broadcast::channel(LIVE_CHANNEL_RECORDS);
        let started_unix_ns = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
            .min(u64::MAX as u128) as u64;
        let log = Arc::new(Self {
            activations: std::array::from_fn(|index| AtomicU64::new(activations.0[index])),
            ring: Mutex::new(Ring::new(size)),
            config_revision: AtomicU64::new(1),
            next_sequence: AtomicU64::new(0),
            next_stream_id: AtomicU32::new(1),
            started: Instant::now(),
            started_unix_ns,
            live_tx,
            file_streams: Mutex::new(HashMap::new()),
        });
        (log, file)
    }

    #[cfg(test)]
    pub(crate) fn new(size: usize, activations: ActivationSet) -> Arc<Self> {
        let (live_tx, _) = broadcast::channel(16);
        Arc::new(Self {
            activations: std::array::from_fn(|index| AtomicU64::new(activations.0[index])),
            ring: Mutex::new(Ring::new(size)),
            config_revision: AtomicU64::new(1),
            next_sequence: AtomicU64::new(0),
            next_stream_id: AtomicU32::new(1),
            started: Instant::now(),
            started_unix_ns: 0,
            live_tx,
            file_streams: Mutex::new(HashMap::new()),
        })
    }

    #[inline]
    pub(crate) fn enabled(&self, kind: EventType) -> bool {
        let id = kind.id() as usize;
        self.activations[id / 64].load(Ordering::Relaxed) & (1u64 << (id % 64)) != 0
    }

    pub(crate) fn activations(&self) -> ActivationSet {
        ActivationSet(std::array::from_fn(|index| {
            self.activations[index].load(Ordering::Acquire)
        }))
    }

    pub(crate) fn configure(
        &self,
        size: usize,
        activations: ActivationSet,
        expected_revision: Option<u64>,
    ) -> Result<EventStats, ConfigureError> {
        if !(MIN_RING_SIZE..=MAX_RING_SIZE).contains(&size) {
            return Err(ConfigureError::InvalidSize);
        }
        let mut ring = self.ring.lock().expect("event ring poisoned");
        let revision = self.config_revision.load(Ordering::Acquire);
        if expected_revision.is_some_and(|expected| expected != revision) {
            return Err(ConfigureError::Conflict);
        }
        ring.resize(size);
        for (word, value) in self.activations.iter().zip(activations.0) {
            word.store(value, Ordering::Release);
        }
        let revision = if revision >= blit_remote::events::EVENTS_CONFIG_REVISION_ANY - 1 {
            1
        } else {
            revision + 1
        };
        self.config_revision.store(revision, Ordering::Release);
        Ok(self.stats_locked(&ring, revision))
    }

    #[cfg(test)]
    pub(crate) fn stats(&self) -> EventStats {
        let ring = self.ring.lock().expect("event ring poisoned");
        let revision = self.config_revision.load(Ordering::Acquire);
        self.stats_locked(&ring, revision)
    }

    fn stats_locked(&self, ring: &Ring, revision: u64) -> EventStats {
        EventStats {
            revision,
            capacity: ring.capacity(),
            used: ring.used,
            records: ring.records,
            dropped: ring.dropped,
            next_sequence: self.next_sequence.load(Ordering::Acquire),
        }
    }

    pub(crate) fn configuration(&self) -> (EventStats, ActivationSet) {
        let ring = self.ring.lock().expect("event ring poisoned");
        let revision = self.config_revision.load(Ordering::Acquire);
        (self.stats_locked(&ring, revision), self.activations())
    }

    pub(crate) fn record(&self, kind: EventType, flags: u16, payload: &[u8]) {
        let sequence = self.next_sequence.fetch_add(1, Ordering::Relaxed);
        let monotonic_ns = self.started.elapsed().as_nanos().min(u64::MAX as u128) as u64;
        let unix_ns = self.started_unix_ns.saturating_add(monotonic_ns);
        let max_payload = blit_remote::MAX_LOGICAL_MESSAGE
            .saturating_sub(EVENT_RECORD_HEADER_LEN)
            .saturating_sub(9);
        let payload = &payload[..payload.len().min(max_payload)];
        let len = EVENT_RECORD_HEADER_LEN + payload.len();
        let mut header = [0; EVENT_RECORD_HEADER_LEN];
        header[0..4].copy_from_slice(&(len as u32).to_le_bytes());
        header[4..6].copy_from_slice(&kind.id().to_le_bytes());
        header[6..8].copy_from_slice(&flags.to_le_bytes());
        header[8..16].copy_from_slice(&sequence.to_le_bytes());
        header[16..24].copy_from_slice(&monotonic_ns.to_le_bytes());
        header[24..32].copy_from_slice(&unix_ns.to_le_bytes());
        let mut ring = self.ring.lock().expect("event ring poisoned");
        ring.append_parts(&header, payload);
        // Live streams still see a record that is larger than the configured
        // ring. The ring's dropped counter reports that it was not retained.
        if self.live_tx.receiver_count() != 0 {
            let mut record = Vec::with_capacity(len);
            record.extend_from_slice(&header);
            record.extend_from_slice(payload);
            let _ = self.live_tx.send(record.into());
        }
    }

    fn dump_locked(&self, ring: &Ring) -> Vec<u8> {
        let activations = self.activations();
        let mut dump = Vec::with_capacity(EVENT_DUMP_HEADER_LEN + ring.used);
        dump.extend_from_slice(EVENT_DUMP_MAGIC);
        dump.extend_from_slice(&(EVENT_DUMP_HEADER_LEN as u16).to_le_bytes());
        dump.extend_from_slice(&(blit_remote::events::EVENTS_VERSION as u16).to_le_bytes());
        dump.extend_from_slice(&(ring.capacity() as u64).to_le_bytes());
        dump.extend_from_slice(&(ring.used as u64).to_le_bytes());
        dump.extend_from_slice(&ring.records.to_le_bytes());
        dump.extend_from_slice(&ring.dropped.to_le_bytes());
        dump.extend_from_slice(&self.next_sequence.load(Ordering::Acquire).to_le_bytes());
        dump.extend_from_slice(&activations.to_bytes());
        debug_assert_eq!(dump.len(), EVENT_DUMP_HEADER_LEN);
        for record in ring.record_vecs() {
            dump.extend_from_slice(&record);
        }
        dump
    }

    pub(crate) fn dump(&self) -> Vec<u8> {
        let ring = self.ring.lock().expect("event ring poisoned");
        self.dump_locked(&ring)
    }

    fn empty_dump_locked(&self, ring: &Ring) -> Vec<u8> {
        let mut dump = Vec::with_capacity(EVENT_DUMP_HEADER_LEN);
        dump.extend_from_slice(EVENT_DUMP_MAGIC);
        dump.extend_from_slice(&(EVENT_DUMP_HEADER_LEN as u16).to_le_bytes());
        dump.extend_from_slice(&(blit_remote::events::EVENTS_VERSION as u16).to_le_bytes());
        dump.extend_from_slice(&(ring.capacity() as u64).to_le_bytes());
        dump.extend_from_slice(&0u64.to_le_bytes());
        dump.extend_from_slice(&0u64.to_le_bytes());
        dump.extend_from_slice(&ring.dropped.to_le_bytes());
        dump.extend_from_slice(&self.next_sequence.load(Ordering::Acquire).to_le_bytes());
        dump.extend_from_slice(&self.activations().to_bytes());
        debug_assert_eq!(dump.len(), EVENT_DUMP_HEADER_LEN);
        dump
    }

    pub(crate) fn snapshot_and_subscribe(
        &self,
        history: bool,
        empty_header: bool,
    ) -> EventSubscription {
        let ring = self.ring.lock().expect("event ring poisoned");
        let receiver = self.live_tx.subscribe();
        let records = if history { ring.records } else { 0 };
        let dump = if history {
            Some(self.dump_locked(&ring))
        } else if empty_header {
            Some(self.empty_dump_locked(&ring))
        } else {
            None
        };
        (dump, records, receiver)
    }

    fn allocate_stream_id(&self) -> u32 {
        loop {
            let id = self.next_stream_id.fetch_add(1, Ordering::Relaxed);
            if id != 0 {
                return id;
            }
        }
    }

    pub(crate) fn client_stream(&self, history: bool) -> ClientEventStream {
        let id = self.allocate_stream_id();
        let (dump, _, receiver) = self.snapshot_and_subscribe(history, true);
        (id, dump, receiver)
    }

    pub(crate) async fn start_file_stream(
        self: &Arc<Self>,
        path: &str,
        flags: u8,
    ) -> Result<u32, StartFileStreamError> {
        let append = flags & EVENTS_STREAM_APPEND != 0;
        let history = flags & EVENTS_STREAM_HISTORY != 0;
        let mut options = tokio::fs::OpenOptions::new();
        options
            .create(true)
            .write(true)
            .append(append)
            .truncate(!append);
        let mut file = options
            .open(path)
            .await
            .map_err(|error| StartFileStreamError::Io(format!("cannot open {path}: {error}")))?;
        let id = self.allocate_stream_id();
        // Every recording invocation starts a self-describing segment, even
        // when appending and/or starting from now. This is also the initial
        // write whose flush gates the successful START response.
        let (dump, history_records, mut receiver) = self.snapshot_and_subscribe(history, true);
        let initial_bytes = dump.as_ref().map_or(0, Vec::len) as u64;
        if let Some(dump) = dump {
            file.write_all(&dump).await.map_err(|error| {
                StartFileStreamError::Io(format!("cannot initialize {path}: {error}"))
            })?;
            file.flush().await.map_err(|error| {
                StartFileStreamError::Io(format!("cannot flush initial data to {path}: {error}"))
            })?;
        }
        let progress = FileStreamProgress::new(history_records, initial_bytes);
        let task_progress = progress.clone();
        let task_path = path.to_owned();
        let (stop, mut stopped) = oneshot::channel();
        let task = tokio::spawn(async move {
            let result = async {
                loop {
                    let next = tokio::select! {
                        biased;
                        _ = &mut stopped => break,
                        next = receiver.recv() => next,
                    };
                    match next {
                        Ok(record) => {
                            write_stream_bytes(&mut file, &record, 0, &task_progress, &task_path)
                                .await?;
                        }
                        Err(broadcast::error::RecvError::Lagged(lost)) => {
                            let record = gap_record(lost);
                            write_stream_bytes(
                                &mut file,
                                &record,
                                lost,
                                &task_progress,
                                &task_path,
                            )
                            .await?;
                        }
                        Err(broadcast::error::RecvError::Closed) => break,
                    }
                }
                loop {
                    match receiver.try_recv() {
                        Ok(record) => {
                            write_stream_bytes(&mut file, &record, 0, &task_progress, &task_path)
                                .await?;
                        }
                        Err(broadcast::error::TryRecvError::Lagged(lost)) => {
                            write_stream_bytes(
                                &mut file,
                                &gap_record(lost),
                                lost,
                                &task_progress,
                                &task_path,
                            )
                            .await?;
                        }
                        Err(broadcast::error::TryRecvError::Empty)
                        | Err(broadcast::error::TryRecvError::Closed) => break,
                    }
                }
                file.flush()
                    .await
                    .map_err(|error| format!("cannot flush {task_path}: {error}"))?;
                Ok(())
            }
            .await;
            match result {
                Ok(()) => task_progress.stop(),
                Err(error) => {
                    return Err(task_progress.fail(error));
                }
            }
            Ok(())
        });
        self.file_streams
            .lock()
            .expect("event file streams poisoned")
            .insert(
                id,
                FileStreamTask {
                    stop,
                    join: task,
                    path: path.to_owned(),
                    flags,
                    progress,
                },
            );
        Ok(id)
    }

    pub(crate) fn file_streams(&self) -> Vec<FileStreamInfo> {
        let streams = self
            .file_streams
            .lock()
            .expect("event file streams poisoned");
        let mut result = streams
            .iter()
            .map(|(&id, task)| FileStreamInfo {
                id,
                state: task.progress.state.load(Ordering::Acquire),
                flags: task.flags,
                records: task.progress.records.load(Ordering::Acquire),
                bytes: task.progress.bytes.load(Ordering::Acquire),
                lost: task.progress.lost.load(Ordering::Acquire),
                path: task.path.clone(),
                error: task
                    .progress
                    .error
                    .lock()
                    .expect("event file progress poisoned")
                    .clone()
                    .unwrap_or_default(),
            })
            .collect::<Vec<_>>();
        result.sort_unstable_by_key(|stream| stream.id);
        result
    }

    pub(crate) async fn stop_file_stream(&self, id: u32) -> Result<bool, String> {
        let task = self
            .file_streams
            .lock()
            .expect("event file streams poisoned")
            .remove(&id);
        if let Some(task) = task {
            let _ = task.stop.send(());
            match task.join.await {
                Ok(Ok(())) => Ok(true),
                Ok(Err(error)) => Err(error),
                Err(error) => Err(format!("event recording task failed: {error}")),
            }
        } else {
            Ok(false)
        }
    }

    pub(crate) async fn shutdown_file_streams(&self) {
        let tasks: Vec<_> = self
            .file_streams
            .lock()
            .expect("event file streams poisoned")
            .drain()
            .map(|(_, task)| task)
            .collect();
        let mut joins = Vec::with_capacity(tasks.len());
        for task in tasks {
            let _ = task.stop.send(());
            joins.push(task.join);
        }
        for join in joins {
            let _ = join.await;
        }
    }
}

async fn write_stream_bytes(
    file: &mut tokio::fs::File,
    record: &[u8],
    lost: u64,
    progress: &FileStreamProgress,
    path: &str,
) -> Result<(), String> {
    file.write_all(record)
        .await
        .map_err(|error| format!("cannot write {path}: {error}"))?;
    progress.records.fetch_add(1, Ordering::Relaxed);
    progress
        .bytes
        .fetch_add(record.len() as u64, Ordering::Relaxed);
    progress.lost.fetch_add(lost, Ordering::Relaxed);
    Ok(())
}

fn gap_record(lost: u64) -> Vec<u8> {
    let len = EVENT_RECORD_HEADER_LEN + 8;
    let mut record = Vec::with_capacity(len);
    record.extend_from_slice(&(len as u32).to_le_bytes());
    record.extend_from_slice(&EVENT_TYPE_STREAM_GAP.to_le_bytes());
    record.extend_from_slice(&0u16.to_le_bytes());
    record.extend_from_slice(&0u64.to_le_bytes());
    record.extend_from_slice(&0u64.to_le_bytes());
    record.extend_from_slice(&0u64.to_le_bytes());
    record.extend_from_slice(&lost.to_le_bytes());
    record
}

pub(crate) fn payload_name(name: &str) -> Vec<u8> {
    let bytes = name.as_bytes();
    let len = bytes.len().min(u16::MAX as usize);
    let mut payload = Vec::with_capacity(2 + len);
    payload.extend_from_slice(&(len as u16).to_le_bytes());
    payload.extend_from_slice(&bytes[..len]);
    payload
}

pub(crate) fn payload_client(client_id: u64) -> Vec<u8> {
    client_id.to_le_bytes().to_vec()
}

pub(crate) fn payload_pty_create_stage(
    client_id: u64,
    nonce: u16,
    stage: u8,
    status: u8,
    pty_id: u16,
) -> [u8; 14] {
    let mut payload = [0; 14];
    payload[..8].copy_from_slice(&client_id.to_le_bytes());
    payload[8..10].copy_from_slice(&nonce.to_le_bytes());
    payload[10] = stage;
    payload[11] = status;
    payload[12..14].copy_from_slice(&pty_id.to_le_bytes());
    payload
}

pub(crate) fn payload_frame(client_id: u64, frame: &[u8]) -> Vec<u8> {
    let mut payload = Vec::with_capacity(12 + frame.len());
    payload.extend_from_slice(&client_id.to_le_bytes());
    payload.extend_from_slice(&(frame.len().min(u32::MAX as usize) as u32).to_le_bytes());
    payload.extend_from_slice(&frame[..frame.len().min(u32::MAX as usize)]);
    payload
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ring_wrap_preserves_complete_newest_records() {
        let log = EventLog::new(160, ActivationSet::all());
        for value in 0..8u8 {
            log.record(EventType::Error, 0, &[value; 8]);
        }
        let dump = log.dump();
        assert_eq!(&dump[..8], EVENT_DUMP_MAGIC);
        let records = &dump[EVENT_DUMP_HEADER_LEN..];
        assert_eq!(records.len() % (EVENT_RECORD_HEADER_LEN + 8), 0);
        assert!(records.len() <= 160);
        assert_eq!(records.last().copied(), Some(7));
        assert!(log.stats().dropped > 0);
    }

    #[test]
    fn disabled_event_building_can_be_guarded() {
        let log = EventLog::new(4096, ActivationSet::default());
        assert!(!log.enabled(EventType::FrameRead));
        assert_eq!(log.stats().records, 0);
    }

    #[test]
    fn oversized_records_are_live_even_when_not_retained() {
        let log = EventLog::new(64, ActivationSet::all());
        let (_, _, mut receiver) = log.client_stream(false);
        log.record(EventType::Error, 0, &[7; 64]);
        let record = receiver.try_recv().unwrap();
        assert_eq!(record.last().copied(), Some(7));
        assert_eq!(log.stats().records, 0);
        assert_eq!(log.stats().dropped, 1);
    }

    #[test]
    fn shrinking_keeps_the_newest_records() {
        let log = EventLog::new(MIN_RING_SIZE * 2, ActivationSet::all());
        for value in 0..100u8 {
            log.record(EventType::Error, 0, &[value; 64]);
        }
        log.configure(MIN_RING_SIZE, ActivationSet::all(), None)
            .unwrap();
        let dump = log.dump();
        assert_eq!(dump.last().copied(), Some(99));
        assert!(log.stats().used <= MIN_RING_SIZE);
    }

    #[test]
    fn configuration_revision_rejects_a_stale_replace() {
        let log = EventLog::new(MIN_RING_SIZE, ActivationSet::low_throughput());
        assert_eq!(log.stats().revision, 1);
        let changed = log
            .configure(MIN_RING_SIZE * 2, ActivationSet::all(), Some(1))
            .unwrap();
        assert_eq!(changed.revision, 2);
        assert!(matches!(
            log.configure(MIN_RING_SIZE, ActivationSet::default(), Some(1)),
            Err(ConfigureError::Conflict)
        ));
        let (current, activations) = log.configuration();
        assert_eq!(current.revision, 2);
        assert_eq!(current.capacity, MIN_RING_SIZE * 2);
        assert_eq!(activations, ActivationSet::all());
    }

    #[tokio::test]
    async fn file_stream_flushes_history_and_live_records() {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path =
            std::env::temp_dir().join(format!("blit-events-{}-{unique}.bin", std::process::id()));
        let path_text = path.to_str().unwrap();
        let log = EventLog::new(MIN_RING_SIZE, ActivationSet::all());
        log.record(EventType::ServerStart, 0, &[1]);
        let stream_id = log
            .start_file_stream(path_text, EVENTS_STREAM_HISTORY)
            .await
            .unwrap();
        let initialized = tokio::fs::read(&path).await.unwrap();
        assert_eq!(&initialized[..EVENT_DUMP_MAGIC.len()], EVENT_DUMP_MAGIC);
        assert_eq!(initialized.last().copied(), Some(1));
        let streams = log.file_streams();
        assert_eq!(streams.len(), 1);
        assert_eq!(streams[0].id, stream_id);
        assert_eq!(
            streams[0].state,
            blit_remote::events::EVENTS_STREAM_STATE_RUNNING
        );
        assert_eq!(streams[0].flags, EVENTS_STREAM_HISTORY);
        assert_eq!(streams[0].path, path_text);
        log.record(EventType::Error, 0, &[9]);
        assert!(log.stop_file_stream(stream_id).await.unwrap());
        assert!(log.file_streams().is_empty());

        let bytes = tokio::fs::read(&path).await.unwrap();
        tokio::fs::remove_file(&path).await.unwrap();
        assert_eq!(&bytes[..EVENT_DUMP_MAGIC.len()], EVENT_DUMP_MAGIC);
        assert_eq!(bytes.last().copied(), Some(9));
    }

    #[tokio::test]
    async fn from_now_file_stream_flushes_its_header_before_start_returns() {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "blit-events-now-{}-{unique}.bin",
            std::process::id()
        ));
        let path_text = path.to_str().unwrap();
        let log = EventLog::new(MIN_RING_SIZE, ActivationSet::all());

        let stream_id = log.start_file_stream(path_text, 0).await.unwrap();
        let initialized = tokio::fs::read(&path).await.unwrap();
        assert_eq!(initialized.len(), EVENT_DUMP_HEADER_LEN);
        assert_eq!(&initialized[..EVENT_DUMP_MAGIC.len()], EVENT_DUMP_MAGIC);
        assert!(log.stop_file_stream(stream_id).await.unwrap());

        tokio::fs::remove_file(path).await.unwrap();
    }

    #[tokio::test]
    async fn file_stream_status_and_stop_preserve_write_failure() {
        let log = EventLog::new(MIN_RING_SIZE, ActivationSet::all());
        let (stop, stopped) = oneshot::channel();
        drop(stopped);
        let progress = FileStreamProgress::new(7, 99);
        progress.lost.store(2, Ordering::Relaxed);
        progress.fail("disk full".into());
        let join = tokio::spawn(async { Err::<(), String>("disk full".into()) });
        log.file_streams.lock().unwrap().insert(
            42,
            FileStreamTask {
                stop,
                join,
                path: "/tmp/failed.bin".into(),
                flags: EVENTS_STREAM_HISTORY,
                progress,
            },
        );

        let streams = log.file_streams();
        assert_eq!(streams.len(), 1);
        assert_eq!(
            streams[0].state,
            blit_remote::events::EVENTS_STREAM_STATE_FAILED
        );
        assert_eq!(streams[0].records, 7);
        assert_eq!(streams[0].bytes, 99);
        assert_eq!(streams[0].lost, 2);
        assert_eq!(streams[0].error, "disk full");
        assert_eq!(log.stop_file_stream(42).await, Err("disk full".into()));
        assert!(log.file_streams().is_empty());
    }

    #[tokio::test]
    async fn file_streams_have_no_process_wide_admission_cap() {
        let log = EventLog::new(MIN_RING_SIZE, ActivationSet::all());
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let mut streams = Vec::new();
        for index in 0..9 {
            let path = std::env::temp_dir().join(format!(
                "blit-events-uncapped-{}-{unique}-{index}.bin",
                std::process::id()
            ));
            let id = log
                .start_file_stream(path.to_str().unwrap(), 0)
                .await
                .unwrap();
            streams.push((id, path));
        }
        assert_eq!(log.file_streams().len(), 9);
        for (id, path) in streams {
            assert!(log.stop_file_stream(id).await.unwrap());
            tokio::fs::remove_file(path).await.unwrap();
        }
    }
}
