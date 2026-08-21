//! Process-wide bounded binary event journal.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
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

#[derive(Clone, Debug)]
pub(crate) struct StartupFile {
    pub path: String,
    pub flags: u8,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct EventStats {
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
        let prefix = self.bytes_at(self.head, 4);
        let len = u32::from_le_bytes(prefix.try_into().expect("four bytes")) as usize;
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
        if record.len() > self.capacity() {
            self.dropped = self.dropped.saturating_add(1);
            return false;
        }
        while self.capacity() - self.used < record.len() {
            self.evict_oldest();
        }
        let tail = (self.head + self.used) % self.capacity();
        self.write_at(tail, record);
        self.used += record.len();
        self.records = self.records.saturating_add(1);
        true
    }

    fn record_vecs(&self) -> Vec<Vec<u8>> {
        let mut result = Vec::with_capacity(self.records.min(usize::MAX as u64) as usize);
        let mut at = self.head;
        let mut left = self.used;
        while left >= EVENT_RECORD_HEADER_LEN {
            let prefix = self.bytes_at(at, 4);
            let len = u32::from_le_bytes(prefix.try_into().expect("four bytes")) as usize;
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
    next_sequence: AtomicU64,
    next_stream_id: AtomicU32,
    started: Instant,
    started_unix_ns: u64,
    live_tx: broadcast::Sender<Arc<[u8]>>,
    file_streams: Mutex<HashMap<u32, FileStreamTask>>,
}

struct FileStreamTask {
    stop: oneshot::Sender<()>,
    join: tokio::task::JoinHandle<()>,
    path: String,
    flags: u8,
}

type ClientEventStream = (u32, Option<Vec<u8>>, broadcast::Receiver<Arc<[u8]>>);

pub(crate) struct FileStreamInfo {
    pub id: u32,
    pub running: bool,
    pub flags: u8,
    pub path: String,
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
    ) -> Result<EventStats, &'static str> {
        if !(MIN_RING_SIZE..=MAX_RING_SIZE).contains(&size) {
            return Err("ring size is outside the supported range");
        }
        self.ring.lock().expect("event ring poisoned").resize(size);
        for (word, value) in self.activations.iter().zip(activations.0) {
            word.store(value, Ordering::Release);
        }
        Ok(self.stats())
    }

    pub(crate) fn stats(&self) -> EventStats {
        let ring = self.ring.lock().expect("event ring poisoned");
        EventStats {
            capacity: ring.capacity(),
            used: ring.used,
            records: ring.records,
            dropped: ring.dropped,
            next_sequence: self.next_sequence.load(Ordering::Acquire),
        }
    }

    pub(crate) fn record(&self, kind: EventType, flags: u16, payload: &[u8]) {
        let sequence = self.next_sequence.fetch_add(1, Ordering::Relaxed);
        let monotonic_ns = self.started.elapsed().as_nanos().min(u64::MAX as u128) as u64;
        let unix_ns = self.started_unix_ns.saturating_add(monotonic_ns);
        let max_payload = (u32::MAX as usize).saturating_sub(EVENT_RECORD_HEADER_LEN);
        let payload = &payload[..payload.len().min(max_payload)];
        let len = EVENT_RECORD_HEADER_LEN + payload.len();
        let mut record = Vec::with_capacity(len);
        record.extend_from_slice(&(len as u32).to_le_bytes());
        record.extend_from_slice(&kind.id().to_le_bytes());
        record.extend_from_slice(&flags.to_le_bytes());
        record.extend_from_slice(&sequence.to_le_bytes());
        record.extend_from_slice(&monotonic_ns.to_le_bytes());
        record.extend_from_slice(&unix_ns.to_le_bytes());
        record.extend_from_slice(payload);
        let mut ring = self.ring.lock().expect("event ring poisoned");
        ring.append(&record);
        // Live streams still see a record that is larger than the configured
        // ring. The ring's dropped counter reports that it was not retained.
        if self.live_tx.receiver_count() != 0 {
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
    ) -> (Option<Vec<u8>>, broadcast::Receiver<Arc<[u8]>>) {
        let ring = self.ring.lock().expect("event ring poisoned");
        let receiver = self.live_tx.subscribe();
        let dump = if history {
            Some(self.dump_locked(&ring))
        } else if empty_header {
            Some(self.empty_dump_locked(&ring))
        } else {
            None
        };
        (dump, receiver)
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
        let (dump, receiver) = self.snapshot_and_subscribe(history, true);
        (id, dump, receiver)
    }

    pub(crate) async fn start_file_stream(
        self: &Arc<Self>,
        path: &str,
        flags: u8,
    ) -> Result<u32, String> {
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
            .map_err(|error| format!("cannot open {path}: {error}"))?;
        let existing = if append {
            file.metadata().await.map(|meta| meta.len()).unwrap_or(0)
        } else {
            0
        };
        let id = self.allocate_stream_id();
        let (dump, mut receiver) = self.snapshot_and_subscribe(history, existing == 0);
        let (stop, mut stopped) = oneshot::channel();
        let task = tokio::spawn(async move {
            if let Some(dump) = dump
                && file.write_all(&dump).await.is_err()
            {
                return;
            }
            loop {
                let next = tokio::select! {
                    biased;
                    _ = &mut stopped => break,
                    next = receiver.recv() => next,
                };
                match next {
                    Ok(record) => {
                        if file.write_all(&record).await.is_err() {
                            break;
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(lost)) => {
                        let record = gap_record(lost);
                        if file.write_all(&record).await.is_err() {
                            break;
                        }
                    }
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
            loop {
                match receiver.try_recv() {
                    Ok(record) => {
                        if file.write_all(&record).await.is_err() {
                            return;
                        }
                    }
                    Err(broadcast::error::TryRecvError::Lagged(lost)) => {
                        if file.write_all(&gap_record(lost)).await.is_err() {
                            return;
                        }
                    }
                    Err(broadcast::error::TryRecvError::Empty)
                    | Err(broadcast::error::TryRecvError::Closed) => break,
                }
            }
            let _ = file.flush().await;
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
                running: !task.join.is_finished(),
                flags: task.flags,
                path: task.path.clone(),
            })
            .collect::<Vec<_>>();
        result.sort_unstable_by_key(|stream| stream.id);
        result
    }

    pub(crate) async fn stop_file_stream(&self, id: u32) -> bool {
        let task = self
            .file_streams
            .lock()
            .expect("event file streams poisoned")
            .remove(&id);
        if let Some(task) = task {
            let _ = task.stop.send(());
            let _ = task.join.await;
            true
        } else {
            false
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
        log.configure(MIN_RING_SIZE, ActivationSet::all()).unwrap();
        let dump = log.dump();
        assert_eq!(dump.last().copied(), Some(99));
        assert!(log.stats().used <= MIN_RING_SIZE);
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
        let streams = log.file_streams();
        assert_eq!(streams.len(), 1);
        assert_eq!(streams[0].id, stream_id);
        assert!(streams[0].running);
        assert_eq!(streams[0].flags, EVENTS_STREAM_HISTORY);
        assert_eq!(streams[0].path, path_text);
        log.record(EventType::Error, 0, &[9]);
        assert!(log.stop_file_stream(stream_id).await);
        assert!(log.file_streams().is_empty());

        let bytes = tokio::fs::read(&path).await.unwrap();
        tokio::fs::remove_file(&path).await.unwrap();
        assert_eq!(&bytes[..EVENT_DUMP_MAGIC.len()], EVENT_DUMP_MAGIC);
        assert_eq!(bytes.last().copied(), Some(9));
    }
}
