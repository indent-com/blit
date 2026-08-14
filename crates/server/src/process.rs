//! Native non-PTY child processes.
//!
//! The server owns admission and a public catalog. Each logical endpoint owns
//! its pending IDs, subscriptions, and ordinary children. Output offsets and
//! accepted stdin belong to the child generation and are shared by watchers.

use blit_remote::process::*;
use blit_remote::{
    STATUS_BUDGET, STATUS_CONFLICT, STATUS_INVALID, STATUS_NOT_FOUND, STATUS_OK, STATUS_OTHER,
    STATUS_PERMISSION, STATUS_UNKNOWN_ID,
};
use rustc_hash::FxHashMap;
use std::collections::VecDeque;
#[cfg(unix)]
use std::ffi::{OsStr, OsString};
use std::io;
#[cfg(unix)]
use std::os::unix::ffi::{OsStrExt, OsStringExt};
#[cfg(unix)]
use std::os::unix::process::{CommandExt, ExitStatusExt};
#[cfg(unix)]
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex as StdMutex, Weak};
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
use tokio::process::{Child, Command};
use tokio::sync::{Notify, Semaphore, mpsc};
use tokio::task::AbortHandle;

#[cfg(unix)]
use crate::pty;

#[cfg(windows)]
use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, INVALID_HANDLE_VALUE, WAIT_OBJECT_0};
#[cfg(windows)]
use windows_sys::Win32::Globalization::{CSTR_EQUAL, CompareStringOrdinal};
#[cfg(windows)]
use windows_sys::Win32::System::Console::{CTRL_BREAK_EVENT, GenerateConsoleCtrlEvent};
#[cfg(windows)]
use windows_sys::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, TH32CS_SNAPTHREAD, THREADENTRY32, Thread32First, Thread32Next,
};
#[cfg(windows)]
use windows_sys::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
    SetInformationJobObject, TerminateJobObject,
};
#[cfg(windows)]
use windows_sys::Win32::System::Threading::{
    CREATE_NEW_PROCESS_GROUP, CREATE_SUSPENDED, OpenProcess, OpenThread, PROCESS_SYNCHRONIZE,
    ResumeThread, THREAD_SUSPEND_RESUME, WaitForSingleObject,
};

const DEFAULT_MAX_PER_CLIENT: usize = 16;
const DEFAULT_MAX_GLOBAL: usize = 64;
const DEFAULT_MAX_SPAWNING: usize = 8;
const DEFAULT_MAX_WATCHERS_PER_GENERATION: usize = 64;
const DEFAULT_REQUEST_MAX_PER_CLIENT: usize = 16 * 1024 * 1024;
const DEFAULT_REQUEST_MAX: usize = 64 * 1024 * 1024;
const DEFAULT_BUFFER_MAX: usize = 192 * 1024 * 1024;
const OUTBOX_REPLY_HEADROOM_FRAMES: usize = 16 * 1024;
const OUTBOX_REPLY_HEADROOM_BYTES: usize = 32 * 1024 * 1024;
/// Keep one process frame from occupying an entire ordinary bulk-writer turn.
/// The protocol accepts larger packets, but the server emits at most this much
/// stdout or stderr data before the fair scheduler can choose another queue.
const OUTPUT_FRAME_PAYLOAD: usize = 32 * 1024;
const DEFAULT_KILL_GRACE: Duration = Duration::from_secs(2);
const DEFAULT_FINAL_TTL: Duration = Duration::from_secs(5 * 60);
const GUARDED_FRAME_TIMEOUT: Duration = Duration::from_secs(10);

const PENDING_QUEUED: u8 = 0;
const PENDING_ACTIVE: u8 = 1;
const PENDING_DONE: u8 = 2;

type ProcessId = u32;

#[cfg(windows)]
struct JobHandle(HANDLE);

#[cfg(windows)]
unsafe impl Send for JobHandle {}
#[cfg(windows)]
unsafe impl Sync for JobHandle {}

#[cfg(windows)]
impl Drop for JobHandle {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe {
                CloseHandle(self.0);
            }
        }
    }
}

fn env_usize(name: &str, default: usize) -> usize {
    std::env::var(name)
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(default)
}

fn env_duration(name: &str, default: Duration) -> Duration {
    std::env::var(name)
        .ok()
        .and_then(|value| parse_duration(&value))
        .unwrap_or(default)
}

fn parse_duration(value: &str) -> Option<Duration> {
    value
        .parse::<f64>()
        .ok()
        .and_then(|value| Duration::try_from_secs_f64(value).ok())
}

#[derive(Clone)]
struct Policy {
    enabled: bool,
    max_per_endpoint: usize,
    max_generations: usize,
    max_watchers: usize,
    max_watchers_per_generation: usize,
    max_request_per_endpoint: usize,
    max_request: usize,
    max_buffer: usize,
    max_outbox_frames: usize,
    max_outbox_bytes: usize,
    kill_grace: Duration,
    final_ttl: Duration,
}

impl Policy {
    fn from_env(enabled: bool) -> Self {
        let max_per_endpoint = env_usize("BLIT_PROCESS_MAX_PER_CLIENT", DEFAULT_MAX_PER_CLIENT);
        let max_generations = env_usize("BLIT_PROCESS_MAX", DEFAULT_MAX_GLOBAL);
        let default_max_watchers = max_per_endpoint.saturating_mul(max_generations).max(1);
        let stream_processes = max_per_endpoint.min(max_generations);
        // A conforming process can hold 1,024 packets on stdin and both output
        // streams. Keep all default stream windows representable, then reserve
        // an explicit finite allowance for correlated refusal/control replies.
        let default_outbox_frames = stream_processes
            .saturating_mul(3 * PROCESS_MAX_UNACKED_PACKETS)
            .saturating_add(OUTBOX_REPLY_HEADROOM_FRAMES);
        let default_outbox_bytes = stream_processes
            .saturating_mul(2)
            .saturating_mul(PROCESS_DEFAULT_STREAM_WINDOW as usize)
            .saturating_add(OUTBOX_REPLY_HEADROOM_BYTES);
        Self {
            enabled,
            max_per_endpoint,
            max_generations,
            max_watchers: env_usize("BLIT_PROCESS_MAX_WATCHERS", default_max_watchers).max(1),
            max_watchers_per_generation: env_usize(
                "BLIT_PROCESS_MAX_WATCHERS_PER_CHILD",
                DEFAULT_MAX_WATCHERS_PER_GENERATION,
            )
            .max(1),
            max_request_per_endpoint: env_usize(
                "BLIT_PROCESS_REQUEST_MAX_PER_CLIENT",
                DEFAULT_REQUEST_MAX_PER_CLIENT,
            ),
            max_request: env_usize("BLIT_PROCESS_REQUEST_MAX", DEFAULT_REQUEST_MAX),
            max_buffer: env_usize("BLIT_PROCESS_BUFFER_MAX", DEFAULT_BUFFER_MAX),
            max_outbox_frames: env_usize("BLIT_PROCESS_OUTBOX_MAX_FRAMES", default_outbox_frames)
                .max(1),
            max_outbox_bytes: env_usize("BLIT_PROCESS_OUTBOX_MAX_BYTES", default_outbox_bytes)
                .max(1),
            kill_grace: env_duration("BLIT_PROCESS_KILL_GRACE", DEFAULT_KILL_GRACE),
            final_ttl: env_duration("BLIT_PROCESS_DETACHED_RESULT_TTL", DEFAULT_FINAL_TTL),
        }
    }
}

/// A process-family frame for the connection writer.
///
/// Guards make endpoint IDs and generation reservations follow the actual
/// writer, not merely the task which queued a terminal response.
pub(crate) struct Outbound {
    data: Vec<u8>,
    guard: Option<WriterGuard>,
    reservation: Option<OutboxReservation>,
}

impl Outbound {
    fn message(data: Vec<u8>) -> Self {
        Self {
            data,
            guard: None,
            reservation: None,
        }
    }

    fn guarded(data: Vec<u8>, guard: WriterGuard) -> Self {
        Self {
            data,
            guard: Some(guard),
            reservation: None,
        }
    }

    pub(crate) fn into_parts(self) -> (Vec<u8>, OutboundGuard) {
        let Self {
            data,
            guard,
            reservation,
        } = self;
        (
            data,
            OutboundGuard {
                reservation,
                writer: guard,
            },
        )
    }
}

/// Completion state retained by the connection writer through the entire
/// socket write. Dropping it releases both the hard byte reservation and any
/// process-lifecycle action attached to the frame.
pub(crate) struct OutboundGuard {
    // Release queue capacity before lifecycle callbacks run. A callback may
    // make more process state eligible for publication.
    reservation: Option<OutboxReservation>,
    writer: Option<WriterGuard>,
}

impl Drop for OutboundGuard {
    fn drop(&mut self) {
        drop(self.reservation.take());
        drop(self.writer.take());
    }
}

struct OutboxState {
    queued_bytes: AtomicUsize,
    max_bytes: usize,
    overflowed: AtomicBool,
    kick: mpsc::UnboundedSender<String>,
    guarded_frame_timeout: Duration,
}

struct OutboxReservation {
    state: Arc<OutboxState>,
    bytes: usize,
}

impl Drop for OutboxReservation {
    fn drop(&mut self) {
        self.state
            .queued_bytes
            .fetch_sub(self.bytes, Ordering::AcqRel);
    }
}

/// A process endpoint's hard-bounded connection-writer queue.
///
/// Sends stay synchronous because process state locks establish packet order.
/// Overflow is therefore fatal to the endpoint instead of silently dropping a
/// correlated reply or advancing an output cursor past data the client never
/// received.
#[derive(Clone)]
pub(crate) struct OutboundSender {
    tx: mpsc::Sender<Outbound>,
    state: Arc<OutboxState>,
}

impl OutboundSender {
    pub(crate) fn new(
        tx: mpsc::Sender<Outbound>,
        max_bytes: usize,
        kick: mpsc::UnboundedSender<String>,
    ) -> Self {
        Self::with_guarded_frame_timeout(tx, max_bytes, kick, GUARDED_FRAME_TIMEOUT)
    }

    fn with_guarded_frame_timeout(
        tx: mpsc::Sender<Outbound>,
        max_bytes: usize,
        kick: mpsc::UnboundedSender<String>,
        guarded_frame_timeout: Duration,
    ) -> Self {
        Self {
            tx,
            state: Arc::new(OutboxState {
                queued_bytes: AtomicUsize::new(0),
                max_bytes,
                overflowed: AtomicBool::new(false),
                kick,
                guarded_frame_timeout,
            }),
        }
    }

    fn overflow(&self) {
        self.kick("native process outbox exceeded its bounded capacity");
    }

    fn kick(&self, reason: &str) {
        if !self.state.overflowed.swap(true, Ordering::AcqRel) {
            let _ = self.state.kick.send(reason.to_owned());
        }
    }

    fn send(&self, mut outbound: Outbound) -> Result<(), Outbound> {
        if self.state.overflowed.load(Ordering::Acquire) {
            return Err(outbound);
        }
        let bytes = outbound.data.len();
        let reserved = self
            .state
            .queued_bytes
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |queued| {
                queued
                    .checked_add(bytes)
                    .filter(|next| *next <= self.state.max_bytes)
            })
            .is_ok();
        if !reserved {
            self.overflow();
            return Err(outbound);
        }
        outbound.reservation = Some(OutboxReservation {
            state: self.state.clone(),
            bytes,
        });
        let guard_completion = outbound.guard.as_ref().map(WriterGuard::completion);
        match self.tx.try_send(outbound) {
            Ok(()) => {
                if let Some(completed) = guard_completion {
                    let sender = self.clone();
                    tokio::spawn(async move {
                        let finished = completed.notify.notified();
                        if completed.done.load(Ordering::Acquire) {
                            return;
                        }
                        tokio::select! {
                            _ = finished => {}
                            _ = tokio::time::sleep(sender.state.guarded_frame_timeout) => {
                                if !completed.done.load(Ordering::Acquire) {
                                    sender.kick("native process lifecycle frame write timed out");
                                }
                            }
                        }
                    });
                }
                Ok(())
            }
            Err(mpsc::error::TrySendError::Full(outbound)) => {
                self.overflow();
                Err(outbound)
            }
            Err(mpsc::error::TrySendError::Closed(outbound)) => {
                self.kick("native process connection writer closed");
                Err(outbound)
            }
        }
    }
}

struct WriterGuard {
    action: Option<Box<dyn FnOnce() + Send>>,
    completed: Arc<WriterCompletion>,
}

struct WriterCompletion {
    done: AtomicBool,
    notify: Notify,
}

impl WriterGuard {
    fn new(f: impl FnOnce() + Send + 'static) -> Self {
        Self {
            action: Some(Box::new(f)),
            completed: Arc::new(WriterCompletion {
                done: AtomicBool::new(false),
                notify: Notify::new(),
            }),
        }
    }

    fn completion(&self) -> Arc<WriterCompletion> {
        self.completed.clone()
    }
}

impl Drop for WriterGuard {
    fn drop(&mut self) {
        self.completed.done.store(true, Ordering::Release);
        self.completed.notify.notify_waiters();
        if let Some(f) = self.action.take() {
            f();
        }
    }
}

#[derive(Default)]
struct ServerState {
    accepting: bool,
    catalog_revision: u64,
    generations: usize,
    request_bytes: usize,
    buffer_bytes: usize,
    pending: FxHashMap<u64, Weak<Pending>>,
    live: FxHashMap<u64, Weak<Record>>,
    finals: FxHashMap<u64, Arc<FinalRecord>>,
}

#[derive(Clone)]
struct FinalRecord {
    generation: u64,
    pid: ProcessId,
    flags: u8,
    argv0: Vec<u8>,
    buffer_bytes: usize,
    stdin_received: u64,
    stdin_acked: u64,
    stdout_next: u64,
    stderr_next: u64,
    stream_state: u8,
    reason: u8,
    kill_cause: u8,
    code: u32,
    detail: &'static str,
}

struct ServerInner {
    policy: Policy,
    verbose: bool,
    next_generation: AtomicU64,
    next_endpoint: AtomicU64,
    state: StdMutex<ServerState>,
    spawn_slots: Semaphore,
}

#[derive(Clone)]
pub(crate) struct Server(Arc<ServerInner>);

impl Server {
    pub(crate) fn new(verbose: bool, enabled: bool) -> Self {
        let policy = Policy::from_env(enabled);
        let max_spawning = env_usize("BLIT_PROCESS_MAX_SPAWNING", DEFAULT_MAX_SPAWNING).max(1);
        Self(Arc::new(ServerInner {
            policy,
            verbose,
            next_generation: AtomicU64::new(1),
            next_endpoint: AtomicU64::new(1),
            state: StdMutex::new(ServerState {
                accepting: true,
                ..ServerState::default()
            }),
            spawn_slots: Semaphore::new(max_spawning),
        }))
    }

    pub(crate) fn enabled(&self) -> bool {
        self.0.policy.enabled
    }

    pub(crate) fn outbox_limits(&self) -> (usize, usize) {
        (
            self.0.policy.max_outbox_frames,
            self.0.policy.max_outbox_bytes,
        )
    }

    pub(crate) fn endpoint(&self, out: OutboundSender) -> Manager {
        let id = self.0.next_endpoint.fetch_add(1, Ordering::Relaxed);
        Manager {
            server: self.clone(),
            endpoint: Arc::new(Endpoint {
                id,
                state: StdMutex::new(EndpointState {
                    accepting: true,
                    ..EndpointState::default()
                }),
            }),
            out,
        }
    }

    fn reserve_buffer(&self, bytes: usize) -> bool {
        let mut state = self.0.state.lock().unwrap();
        let Some(next) = state.buffer_bytes.checked_add(bytes) else {
            return false;
        };
        if next > self.0.policy.max_buffer {
            return false;
        }
        state.buffer_bytes = next;
        true
    }

    fn release_buffer(&self, bytes: usize) {
        let mut state = self.0.state.lock().unwrap();
        state.buffer_bytes = state.buffer_bytes.saturating_sub(bytes);
    }

    fn finish_detached(&self, record: Arc<Record>, final_record: Arc<FinalRecord>) {
        if record.released.load(Ordering::Acquire) {
            return;
        }
        let installed = {
            let mut state = self.0.state.lock().unwrap();
            let current = state.live.get(&record.generation);
            if !matches!(current, Some(live) if live.ptr_eq(&Arc::downgrade(&record))) {
                false
            } else {
                state.live.remove(&record.generation);
                state.finals.insert(record.generation, final_record.clone());
                state.catalog_revision = state.catalog_revision.wrapping_add(1);
                true
            }
        };
        if !installed {
            return;
        }
        if !record.buffer_released.swap(true, Ordering::AcqRel) {
            self.release_buffer(
                record
                    .buffer_bytes
                    .saturating_sub(final_record.buffer_bytes),
            );
        }
        let server = self.clone();
        tokio::spawn(async move {
            tokio::time::sleep(server.0.policy.final_ttl).await;
            let mut state = server.0.state.lock().unwrap();
            let matches = state
                .finals
                .get(&final_record.generation)
                .is_some_and(|current| Arc::ptr_eq(current, &final_record));
            if matches {
                state.finals.remove(&final_record.generation);
                state.generations = state.generations.saturating_sub(1);
                state.buffer_bytes = state.buffer_bytes.saturating_sub(final_record.buffer_bytes);
                state.catalog_revision = state.catalog_revision.wrapping_add(1);
            }
        });
    }

    fn release_record(&self, record: &Record) {
        if record.released.swap(true, Ordering::AcqRel) {
            return;
        }
        let mut state = self.0.state.lock().unwrap();
        if !record.buffer_released.swap(true, Ordering::AcqRel) {
            state.buffer_bytes = state.buffer_bytes.saturating_sub(record.buffer_bytes);
        }
        let remove = matches!(
            state.live.get(&record.generation),
            Some(live) if std::ptr::eq(live.as_ptr(), record)
        );
        if remove {
            state.live.remove(&record.generation);
            state.generations = state.generations.saturating_sub(1);
            state.catalog_revision = state.catalog_revision.wrapping_add(1);
        }
        drop(state);
        if let Some(owner) = record.owner.upgrade() {
            owner.state.lock().unwrap().owned.remove(&record.generation);
        }
    }

    pub(crate) async fn shutdown(&self) {
        let pending = {
            let mut state = self.0.state.lock().unwrap();
            state.accepting = false;
            state
                .pending
                .values()
                .filter_map(Weak::upgrade)
                .collect::<Vec<_>>()
        };
        for pending in &pending {
            // Exactly one spawn task waits on this notification. `notify_one`
            // stores a permit if that task has not reached its select yet.
            pending.cancel.notify_one();
            if pending
                .phase
                .compare_exchange(
                    PENDING_QUEUED,
                    PENDING_DONE,
                    Ordering::AcqRel,
                    Ordering::Acquire,
                )
                .is_ok()
            {
                release_pending(pending, false);
                pending.mark_completed();
            }
        }
        for pending in &pending {
            while !pending.completed.load(Ordering::Acquire) {
                let done = pending.done.notified();
                if pending.completed.load(Ordering::Acquire) {
                    break;
                }
                done.await;
            }
        }
        let live = {
            let state = self.0.state.lock().unwrap();
            state
                .live
                .values()
                .filter_map(Weak::upgrade)
                .collect::<Vec<_>>()
        };
        // A server-wide shutdown intentionally discards all subscriptions.
        // Remove them before aborting pipes so terminal publication cannot
        // enqueue replies into endpoints which are shutting down too.
        for record in &live {
            release_record_endpoint_slots(record);
        }
        for record in &live {
            terminate_record(
                record,
                PROCESS_KILL_SERVER_SHUTDOWN,
                self.0.policy.kill_grace,
            )
            .await;
        }
        for record in &live {
            self.release_record(record);
        }
        let mut state = self.0.state.lock().unwrap();
        let finals = state.finals.len();
        let final_bytes = state
            .finals
            .values()
            .map(|record| record.buffer_bytes)
            .sum::<usize>();
        if !state.finals.is_empty() {
            state.catalog_revision = state.catalog_revision.wrapping_add(1);
        }
        state.finals.clear();
        state.generations = state.generations.saturating_sub(finals);
        state.buffer_bytes = state.buffer_bytes.saturating_sub(final_bytes);
    }
}

#[derive(Default)]
struct EndpointState {
    accepting: bool,
    request_bytes: usize,
    slots: FxHashMap<u32, EndpointSlot>,
    /// Ordinary processes remain owned after their creator unsubscribes.
    owned: FxHashMap<u64, Weak<Record>>,
}

enum EndpointSlot {
    Pending(Arc<Pending>),
    Bound(Arc<Record>),
    Reply(u64),
}

struct Endpoint {
    id: u64,
    state: StdMutex<EndpointState>,
}

fn endpoint_usage(state: &EndpointState) -> usize {
    let unbound_owned = state.owned.keys().filter(|generation| {
        !state.slots.values().any(
            |slot| matches!(slot, EndpointSlot::Bound(record) if record.generation == **generation),
        )
    });
    state.slots.len().saturating_add(unbound_owned.count())
}

/// Project usage after adding a watch, once the caller has established that
/// this endpoint does not already watch the generation.
fn endpoint_usage_after_watch(state: &EndpointState, process_ref: ProcessRef) -> usize {
    endpoint_usage(state).saturating_add(usize::from(!state.owned.contains_key(&process_ref)))
}

fn active_watcher_count(state: &ServerState) -> usize {
    let pending = state
        .pending
        .values()
        .filter(|pending| pending.strong_count() != 0)
        .count();
    state
        .live
        .values()
        .filter_map(Weak::upgrade)
        .fold(pending, |count, record| {
            count.saturating_add(record.inner.lock().unwrap().bindings.len())
        })
}

struct Pending {
    generation: u64,
    process_id: u32,
    detachable: bool,
    request_bytes: usize,
    endpoint: Weak<Endpoint>,
    server: Weak<ServerInner>,
    out: OutboundSender,
    phase: AtomicU8,
    endpoint_lost: AtomicBool,
    request_released: AtomicBool,
    /// `phase = DONE` prevents duplicate completion. This separate flag is
    /// published only after registry/accounting transition is fully visible.
    completed: AtomicBool,
    cancel: Notify,
    done: Notify,
}

impl Pending {
    fn mark_completed(&self) {
        self.completed.store(true, Ordering::Release);
        self.done.notify_waiters();
    }
}

#[derive(Clone)]
struct BindingStream {
    floor: u64,
    acked: u64,
    frames: VecDeque<u64>,
}

struct Binding {
    endpoint_id: u64,
    process_id: u32,
    endpoint: Weak<Endpoint>,
    out: OutboundSender,
    stdout: BindingStream,
    stderr: Option<BindingStream>,
}

struct StreamState {
    next: u64,
}

#[derive(Clone, Copy)]
enum ChildOutcome {
    Returned(u32),
    #[cfg(unix)]
    Signalled(u32),
    HostFailure,
}

#[derive(Clone, Copy)]
struct ExitOverride {
    reason: u8,
    kill_cause: u8,
}

struct InputChunk {
    end: u64,
    data: Vec<u8>,
}

struct Spawned {
    child: Child,
    pid: ProcessId,
    #[cfg(windows)]
    job: JobHandle,
}

struct RecordInner {
    bindings: Vec<Binding>,
    /// At most one endpoint may advance the generation-wide stdin cursor.
    /// This is an openly reacquirable writer role, not an authorization token.
    stdin_controller: Option<u64>,
    stdin_tx: Option<mpsc::Sender<InputChunk>>,
    stdin_received: u64,
    stdin_acked: u64,
    stdin_frames: VecDeque<u64>,
    stdin_state: u8,
    stdin_closed_by_child: bool,
    stdin_writer_done: bool,
    stdout: StreamState,
    stderr: Option<StreamState>,
    stdout_readers: u8,
    stderr_readers: u8,
    child_outcome: Option<ChildOutcome>,
    tree_cleanup_done: bool,
    exit_override: Option<ExitOverride>,
    terminal_queued: bool,
    cleanup_detail: &'static str,
    output_aborts: Vec<AbortHandle>,
    stdin_abort: Option<AbortHandle>,
}

struct Record {
    generation: u64,
    detachable: bool,
    pid: ProcessId,
    argv0: Vec<u8>,
    owner: Weak<Endpoint>,
    #[cfg(windows)]
    job: JobHandle,
    merged: bool,
    buffer_bytes: usize,
    server: Server,
    inner: StdMutex<RecordInner>,
    changed: Notify,
    reaped: AtomicBool,
    reaped_notify: Notify,
    terminal_notify: Notify,
    buffer_released: AtomicBool,
    released: AtomicBool,
}

impl Record {
    fn mark_reaped(&self) {
        self.reaped.store(true, Ordering::Release);
        self.reaped_notify.notify_waiters();
    }

    async fn wait_reaped(&self) {
        while !self.reaped.load(Ordering::Acquire) {
            let notified = self.reaped_notify.notified();
            if self.reaped.load(Ordering::Acquire) {
                return;
            }
            notified.await;
        }
    }

    async fn wait_terminal(&self) {
        loop {
            let notified = self.terminal_notify.notified();
            if self.inner.lock().unwrap().terminal_queued {
                return;
            }
            notified.await;
        }
    }

    async fn wait_tree_cleanup(&self) {
        loop {
            let changed = self.changed.notified();
            if self.inner.lock().unwrap().tree_cleanup_done {
                return;
            }
            changed.await;
        }
    }
}

#[derive(Clone)]
pub(crate) struct Manager {
    server: Server,
    endpoint: Arc<Endpoint>,
    out: OutboundSender,
}

impl Manager {
    pub(crate) fn send(&self, data: Vec<u8>) {
        let _ = self.out.send(Outbound::message(data));
    }

    fn spawn_reply(&self, nonce: u16, process_id: u32, status: u8, detail: &str) {
        if let Ok(msg) = msg_process_started(ProcessStarted {
            nonce,
            status,
            process_id,
            process_ref: 0,
            stdin_window: 0,
            stdout_window: 0,
            stderr_window: 0,
            detail,
        }) {
            self.send(msg);
        }
    }

    pub(crate) fn spawn(&self, data: &[u8], pty_cwd: Option<&[u8]>) {
        let nonce = read_u16(data, 1);
        let process_id = read_u32(data, 3);
        let req = match parse_process_spawn(data) {
            Ok(req) => req,
            Err(error) => {
                self.spawn_reply(nonce, process_id, error.status(), "malformed process spawn");
                return;
            }
        };
        #[cfg(windows)]
        if let Err(error) = validate_process_spawn_for_windows(&req, windows_env_keys_equal) {
            self.spawn_reply(
                req.nonce,
                req.process_id,
                error.status(),
                "process strings must be valid Windows UTF-8",
            );
            return;
        }
        if req.cwd_kind == PROCESS_CWD_FROM_PTY && pty_cwd.is_none() {
            self.spawn_reply(
                req.nonce,
                req.process_id,
                STATUS_NOT_FOUND,
                "source terminal has no working directory",
            );
            return;
        }
        let detachable = req.flags & PROCESS_SPAWN_DETACHABLE != 0;
        let generation = self
            .server
            .0
            .next_generation
            .fetch_add(1, Ordering::Relaxed);
        let pending = Arc::new(Pending {
            generation,
            process_id: req.process_id,
            detachable,
            request_bytes: data.len(),
            endpoint: Arc::downgrade(&self.endpoint),
            server: Arc::downgrade(&self.server.0),
            out: self.out.clone(),
            phase: AtomicU8::new(PENDING_QUEUED),
            endpoint_lost: AtomicBool::new(false),
            request_released: AtomicBool::new(false),
            completed: AtomicBool::new(false),
            cancel: Notify::new(),
            done: Notify::new(),
        });
        {
            // Server first, endpoint second is the process-family lock order.
            let mut server = self.server.0.state.lock().unwrap();
            let mut endpoint = self.endpoint.state.lock().unwrap();
            let request_next_server = server.request_bytes.checked_add(data.len());
            let request_next_endpoint = endpoint.request_bytes.checked_add(data.len());
            if !server.accepting || !endpoint.accepting {
                drop(endpoint);
                drop(server);
                self.spawn_reply(
                    req.nonce,
                    req.process_id,
                    STATUS_PERMISSION,
                    "process admission closed",
                );
                return;
            }
            if endpoint.slots.contains_key(&req.process_id) {
                drop(endpoint);
                drop(server);
                self.spawn_reply(
                    req.nonce,
                    req.process_id,
                    STATUS_CONFLICT,
                    "process id in use",
                );
                return;
            }
            let budget = endpoint_usage(&endpoint) >= self.server.0.policy.max_per_endpoint
                || server.generations >= self.server.0.policy.max_generations
                || active_watcher_count(&server) >= self.server.0.policy.max_watchers
                || request_next_server.is_none_or(|n| n > self.server.0.policy.max_request)
                || request_next_endpoint
                    .is_none_or(|n| n > self.server.0.policy.max_request_per_endpoint);
            if budget {
                drop(endpoint);
                drop(server);
                self.spawn_reply(
                    req.nonce,
                    req.process_id,
                    STATUS_BUDGET,
                    "process admission budget reached",
                );
                return;
            }
            server.generations += 1;
            server.request_bytes = request_next_server.unwrap();
            server.pending.insert(generation, Arc::downgrade(&pending));
            endpoint.request_bytes = request_next_endpoint.unwrap();
            endpoint
                .slots
                .insert(req.process_id, EndpointSlot::Pending(pending.clone()));
        }
        let request = data.to_vec();
        let pty_cwd = pty_cwd.map(ToOwned::to_owned);
        let manager = self.clone();
        tokio::spawn(async move {
            manager.run_spawn(pending, request, pty_cwd).await;
        });
    }

    async fn run_spawn(&self, pending: Arc<Pending>, request: Vec<u8>, pty_cwd: Option<Vec<u8>>) {
        let permit = tokio::select! {
            permit = self.server.0.spawn_slots.acquire() => permit.ok(),
            _ = pending.cancel.notified() => None,
        };
        let Some(permit) = permit else {
            return;
        };
        if pending
            .phase
            .compare_exchange(
                PENDING_QUEUED,
                PENDING_ACTIVE,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .is_err()
        {
            return;
        }
        let req = parse_process_spawn(&request).expect("retained validated spawn");
        let merged = req.flags & PROCESS_SPAWN_MERGE_STDERR != 0;
        let streams = if merged { 2 } else { 3 };
        let buffer_bytes =
            (PROCESS_DEFAULT_STREAM_WINDOW as usize * streams).saturating_add(req.argv[0].len());
        if !self.server.reserve_buffer(buffer_bytes) {
            drop(permit);
            complete_spawn_failure(
                &pending,
                req.nonce,
                STATUS_BUDGET,
                "process stream budget reached",
            );
            return;
        }
        let mut command = command_for(&req, pty_cwd.as_deref());
        let merged_reader = if merged {
            match configure_merged_output(&mut command) {
                Ok(reader) => Some(reader),
                Err(error) => {
                    self.server.release_buffer(buffer_bytes);
                    drop(permit);
                    complete_spawn_failure(&pending, req.nonce, STATUS_OTHER, &error.to_string());
                    return;
                }
            }
        } else {
            None
        };
        let spawned = spawn_child(&mut command);
        drop(permit);
        let spawned = match spawned {
            Ok(spawned) => spawned,
            Err(error) => {
                self.server.release_buffer(buffer_bytes);
                let status = match error.kind() {
                    io::ErrorKind::NotFound => STATUS_NOT_FOUND,
                    io::ErrorKind::PermissionDenied => STATUS_PERMISSION,
                    io::ErrorKind::InvalidInput => STATUS_INVALID,
                    _ => STATUS_OTHER,
                };
                complete_spawn_failure(&pending, req.nonce, status, &error.to_string());
                return;
            }
        };
        let Spawned {
            mut child,
            pid,
            #[cfg(windows)]
            job,
        } = spawned;
        let stdin = child.stdin.take().expect("piped stdin");
        let stdout = (!merged).then(|| child.stdout.take().expect("piped stdout"));
        let stderr = (!merged).then(|| child.stderr.take().expect("piped stderr"));
        let (stdin_tx, stdin_rx) = mpsc::channel(PROCESS_MAX_UNACKED_PACKETS);
        let bindings = pending
            .endpoint
            .upgrade()
            .filter(|endpoint| endpoint.state.lock().unwrap().accepting)
            .map(|endpoint| {
                vec![Binding::new(
                    endpoint.id,
                    pending.process_id,
                    Arc::downgrade(&endpoint),
                    pending.out.clone(),
                    merged,
                    0,
                    0,
                )]
            })
            .unwrap_or_default();
        let stdin_controller = bindings.first().map(|binding| binding.endpoint_id);
        let record = Arc::new(Record {
            generation: pending.generation,
            detachable: pending.detachable,
            pid,
            argv0: req.argv[0].to_vec(),
            owner: pending.endpoint.clone(),
            #[cfg(windows)]
            job,
            merged,
            buffer_bytes,
            server: self.server.clone(),
            inner: StdMutex::new(RecordInner {
                bindings,
                stdin_controller,
                stdin_tx: Some(stdin_tx),
                stdin_received: 0,
                stdin_acked: 0,
                stdin_frames: VecDeque::new(),
                stdin_state: PROCESS_STDIN_ACCEPTING,
                stdin_closed_by_child: false,
                stdin_writer_done: false,
                stdout: StreamState { next: 0 },
                stderr: (!merged).then_some(StreamState { next: 0 }),
                stdout_readers: 1,
                stderr_readers: if merged { 0 } else { 1 },
                child_outcome: None,
                tree_cleanup_done: false,
                exit_override: None,
                terminal_queued: false,
                cleanup_detail: "",
                output_aborts: Vec::new(),
                stdin_abort: None,
            }),
            changed: Notify::new(),
            reaped: AtomicBool::new(false),
            reaped_notify: Notify::new(),
            terminal_notify: Notify::new(),
            buffer_released: AtomicBool::new(false),
            released: AtomicBool::new(false),
        });
        // Install every task and abort handle before publishing the live
        // record. The semaphore keeps them from emitting output or observing
        // exit until STARTED has been queued, while its stored permits make
        // publication safe even if a task has not been polled yet.
        let task_start = Arc::new(Semaphore::new(0));
        let stdin_start = task_start.clone();
        let stdin_record = record.clone();
        let stdin_task = tokio::spawn(async move {
            let permit = stdin_start
                .acquire()
                .await
                .expect("spawn task gate remains open");
            permit.forget();
            stdin_writer(stdin_record, stdin, stdin_rx).await;
        });
        let stdout_start = task_start.clone();
        let stdout_record = record.clone();
        let stdout_task = if let Some(reader) = merged_reader {
            tokio::spawn(async move {
                let permit = stdout_start
                    .acquire()
                    .await
                    .expect("spawn task gate remains open");
                permit.forget();
                output_reader(stdout_record, PROCESS_STREAM_STDOUT, reader).await;
            })
        } else {
            tokio::spawn(async move {
                let permit = stdout_start
                    .acquire()
                    .await
                    .expect("spawn task gate remains open");
                permit.forget();
                output_reader(
                    stdout_record,
                    PROCESS_STREAM_STDOUT,
                    stdout.expect("separate stdout pipe"),
                )
                .await;
            })
        };
        let stderr_task = stderr.map(|reader| {
            let start = task_start.clone();
            let record = record.clone();
            tokio::spawn(async move {
                let permit = start.acquire().await.expect("spawn task gate remains open");
                permit.forget();
                output_reader(record, PROCESS_STREAM_STDERR, reader).await;
            })
        });
        let task_count = 3 + usize::from(stderr_task.is_some());
        {
            let mut inner = record.inner.lock().unwrap();
            inner.stdin_abort = Some(stdin_task.abort_handle());
            inner.output_aborts = vec![stdout_task.abort_handle()];
            if let Some(stderr_task) = &stderr_task {
                inner.output_aborts.push(stderr_task.abort_handle());
            }
        }
        let wait_start = task_start.clone();
        let wait_record = record.clone();
        tokio::spawn(async move {
            let permit = wait_start
                .acquire()
                .await
                .expect("spawn task gate remains open");
            permit.forget();
            wait_child(wait_record, child).await;
        });

        let installed_bound = transfer_pending_to_record(&pending, &record);
        if !installed_bound && !pending.detachable {
            let mut inner = record.inner.lock().unwrap();
            if graceful_terminate(&record).is_ok() {
                inner.exit_override = Some(ExitOverride {
                    reason: PROCESS_EXIT_KILLED,
                    kill_cause: PROCESS_KILL_OWNER_LOST,
                });
            }
            drop(inner);
            schedule_terminate_timeout(record.clone(), PROCESS_KILL_OWNER_LOST);
        }
        if installed_bound {
            let started = msg_process_started(ProcessStarted {
                nonce: req.nonce,
                status: STATUS_OK,
                process_id: req.process_id,
                process_ref: record.generation,
                stdin_window: PROCESS_DEFAULT_STREAM_WINDOW,
                stdout_window: PROCESS_DEFAULT_STREAM_WINDOW,
                stderr_window: if merged {
                    0
                } else {
                    PROCESS_DEFAULT_STREAM_WINDOW
                },
                detail: "",
            })
            .expect("valid started reply");
            let _ = pending.out.send(Outbound::message(started));
        }
        task_start.add_permits(task_count);
        if self.server.0.verbose {
            eprintln!(
                "C2S_PROCESS_SPAWN: generation={} process_id={} pid={} argv0={:?}",
                record.generation,
                req.process_id,
                pid,
                String::from_utf8_lossy(req.argv[0])
            );
        }
    }

    pub(crate) fn handle(&self, data: &[u8]) {
        let Some(&opcode) = data.first() else {
            return;
        };
        match opcode {
            C2S_PROCESS_STDIN => self.stdin(data),
            C2S_PROCESS_OUTPUT_ACK => self.output_ack(data),
            C2S_PROCESS_CONTROL => self.control(data),
            C2S_PROCESS_LIST => self.list(data),
            C2S_PROCESS_WATCH => self.watch(data),
            _ => {}
        }
    }

    fn get(&self, process_id: u32) -> Option<Arc<Record>> {
        match self.endpoint.state.lock().unwrap().slots.get(&process_id) {
            Some(EndpointSlot::Bound(record)) => Some(record.clone()),
            _ => None,
        }
    }

    fn evict(&self, record: &Arc<Record>, process_id: u32, reason: &str) {
        let binding = {
            let mut inner = record.inner.lock().unwrap();
            binding_index(&inner, self.endpoint.id, process_id).map(|index| {
                if inner.stdin_controller == Some(self.endpoint.id) {
                    inner.stdin_controller = None;
                }
                inner.bindings.swap_remove(index)
            })
        };
        if let Some(binding) = binding {
            binding.out.kick(reason);
            record.changed.notify_waiters();
        }
    }

    fn stdin(&self, data: &[u8]) {
        let process_id = read_u32(data, 1);
        let Some(record) = self.get(process_id) else {
            return;
        };
        let input = match parse_process_stdin(data) {
            Ok(input) => input,
            Err(_) => {
                self.evict(&record, process_id, "invalid native process stdin packet");
                return;
            }
        };
        let mut inner = record.inner.lock().unwrap();
        if binding_index(&inner, self.endpoint.id, process_id).is_none()
            || inner.child_outcome.is_some()
            || inner.terminal_queued
        {
            return;
        }
        if inner.stdin_controller != Some(self.endpoint.id) {
            send_stdin_ack_to(&inner, self.endpoint.id, process_id);
            return;
        }
        if inner.stdin_state != PROCESS_STDIN_ACCEPTING {
            send_stdin_ack_to(&inner, self.endpoint.id, process_id);
            return;
        }
        let Some(end) = input.offset.checked_add(input.data.len() as u64) else {
            send_stdin_ack_to(&inner, self.endpoint.id, process_id);
            return;
        };
        let Some(limit) = inner.stdin_acked.checked_add(PROCESS_DEFAULT_STREAM_WINDOW) else {
            send_stdin_ack_to(&inner, self.endpoint.id, process_id);
            return;
        };
        if input.offset != inner.stdin_received
            || end > limit
            || inner.stdin_frames.len() >= PROCESS_MAX_UNACKED_PACKETS
        {
            // Concurrent watchers can race at the same lifetime cursor. A
            // stale write loses the race; it must never kill the shared child.
            send_stdin_ack_to(&inner, self.endpoint.id, process_id);
            return;
        }
        let Some(tx) = inner.stdin_tx.as_ref() else {
            send_stdin_ack_to(&inner, self.endpoint.id, process_id);
            return;
        };
        match tx.try_send(InputChunk {
            end,
            data: input.data.to_vec(),
        }) {
            Ok(()) => {
                inner.stdin_received = end;
                inner.stdin_frames.push_back(end);
            }
            Err(mpsc::error::TrySendError::Closed(_)) => {
                inner.stdin_state = PROCESS_STDIN_CLOSED;
                inner.stdin_closed_by_child = true;
                inner.stdin_tx = None;
            }
            Err(mpsc::error::TrySendError::Full(_)) => {
                send_stdin_ack_to(&inner, self.endpoint.id, process_id);
            }
        }
    }

    fn output_ack(&self, data: &[u8]) {
        let process_id = read_u32(data, 1);
        let Some(record) = self.get(process_id) else {
            return;
        };
        let ack = match parse_process_output_ack(data) {
            Ok(ack) => ack,
            Err(_) => {
                self.evict(&record, process_id, "invalid native process output ACK");
                return;
            }
        };
        let mut inner = record.inner.lock().unwrap();
        let Some(index) = binding_index(&inner, self.endpoint.id, process_id) else {
            return;
        };
        if inner.terminal_queued {
            return;
        }
        let next = match ack.stream {
            PROCESS_STREAM_STDOUT => inner.stdout.next,
            PROCESS_STREAM_STDERR if !record.merged => inner.stderr.as_ref().unwrap().next,
            _ => {
                drop(inner);
                self.evict(&record, process_id, "invalid native process output stream");
                return;
            }
        };
        let binding = &mut inner.bindings[index];
        let credit = if ack.stream == PROCESS_STREAM_STDOUT {
            &mut binding.stdout
        } else {
            binding.stderr.as_mut().expect("separate stderr binding")
        };
        if ack.bytes < credit.floor || ack.bytes < credit.acked || ack.bytes > next {
            drop(inner);
            self.evict(&record, process_id, "invalid native process output cursor");
            return;
        }
        credit.acked = ack.bytes;
        while credit.frames.front().is_some_and(|end| *end <= ack.bytes) {
            credit.frames.pop_front();
        }
        drop(inner);
        record.changed.notify_waiters();
    }

    fn control(&self, data: &[u8]) {
        let nonce = read_u16(data, 1);
        let process_id = read_u32(data, 3);
        let control = match parse_process_control(data) {
            Ok(control) => control,
            Err(error) => {
                self.send(msg_process_controlled(ProcessControlled {
                    nonce,
                    status: error.status(),
                    process_id,
                    detail: "invalid process control",
                }));
                return;
            }
        };
        let Some(record) = self.get(control.process_id) else {
            self.send(msg_process_controlled(ProcessControlled {
                nonce: control.nonce,
                status: STATUS_UNKNOWN_ID,
                process_id: control.process_id,
                detail: "unknown process id",
            }));
            return;
        };
        let mut timeout_cause = None;
        let mut unwatch = false;
        let mut rejected_outbound = None;
        {
            // Terminalization takes this lock too. The correlated control
            // response is queued while held, so an induced EXIT cannot pass it.
            let mut inner = record.inner.lock().unwrap();
            let binding = binding_index(&inner, self.endpoint.id, process_id);
            let (status, detail) =
                if binding.is_none() || inner.child_outcome.is_some() || inner.terminal_queued {
                    (STATUS_UNKNOWN_ID, "process has exited or moved")
                } else {
                    match control.action {
                        PROCESS_CONTROL_CLOSE_STDIN => {
                            if inner.stdin_state == PROCESS_STDIN_ACCEPTING {
                                inner.stdin_state = PROCESS_STDIN_CLOSING;
                                inner.stdin_tx.take();
                                send_stdin_ack(&inner, inner.stdin_acked, PROCESS_STDIN_CLOSING);
                            }
                            (STATUS_OK, "")
                        }
                        PROCESS_CONTROL_TERMINATE => match graceful_terminate(&record) {
                            Ok(()) => {
                                timeout_cause = Some(PROCESS_KILL_TERMINATE_TIMEOUT);
                                (STATUS_OK, "")
                            }
                            Err(error) => (STATUS_OTHER, os_error_detail(error)),
                        },
                        PROCESS_CONTROL_KILL => match force_kill(&record) {
                            Ok(()) => {
                                inner.exit_override = Some(ExitOverride {
                                    reason: PROCESS_EXIT_KILLED,
                                    kill_cause: PROCESS_KILL_CLIENT,
                                });
                                (STATUS_OK, "")
                            }
                            Err(error) => (STATUS_OTHER, os_error_detail(error)),
                        },
                        PROCESS_CONTROL_SIGNAL => control_signal(&record, control.value),
                        PROCESS_CONTROL_UNWATCH => {
                            unwatch = true;
                            (STATUS_OK, "")
                        }
                        _ => unreachable!("codec validated action"),
                    }
                };
            let reply = msg_process_controlled(ProcessControlled {
                nonce: control.nonce,
                status,
                process_id: control.process_id,
                detail,
            });
            if unwatch && status == STATUS_OK {
                let binding = inner
                    .bindings
                    .swap_remove(binding.expect("binding checked"));
                if inner.stdin_controller == Some(self.endpoint.id) {
                    inner.stdin_controller = None;
                }
                let endpoint = self.endpoint.clone();
                let record_for_guard = record.clone();
                let guard = WriterGuard::new(move || {
                    remove_bound_slot(&endpoint, process_id, &record_for_guard);
                    record_for_guard.changed.notify_waiters();
                });
                rejected_outbound = binding.out.send(Outbound::guarded(reply, guard)).err();
            } else {
                let _ = self.out.send(Outbound::message(reply));
            }
        }
        drop(rejected_outbound);
        if unwatch {
            record.changed.notify_waiters();
        }
        if let Some(cause) = timeout_cause {
            schedule_terminate_timeout(record, cause);
        }
    }

    fn list(&self, data: &[u8]) {
        let nonce = read_u16(data, 1);
        let list = match parse_process_list(data) {
            Ok(list) => list,
            Err(error) => {
                if data.len() >= 3 {
                    self.send(
                        msg_process_listed(ProcessListed {
                            nonce,
                            status: error.status(),
                            revision: 0,
                            entries: Vec::new(),
                            detail: "invalid process list request",
                        })
                        .expect("bounded process list error"),
                    );
                }
                return;
            }
        };
        enum Source {
            Live(Arc<Record>),
            Final(Arc<FinalRecord>),
        }
        struct Entry {
            process_ref: ProcessRef,
            state: u8,
            flags: u8,
            pid: u32,
            argv0: Vec<u8>,
        }
        let snapshot = {
            let server = self.server.0.state.lock().unwrap();
            let capacity = server
                .live
                .len()
                .saturating_add(server.finals.len())
                .min(PROCESS_MAX_LIST_ENTRIES.saturating_add(1));
            let mut sources = Vec::with_capacity(capacity);
            let mut encoded_bytes = 14usize;
            let mut push = |source: Source, argv0_len: usize| -> Result<(), ()> {
                if sources.len() >= PROCESS_MAX_LIST_ENTRIES {
                    return Err(());
                }
                encoded_bytes = encoded_bytes
                    .checked_add(18)
                    .and_then(|bytes| bytes.checked_add(argv0_len))
                    .ok_or(())?;
                if encoded_bytes > PROCESS_MAX_LIST_BYTES {
                    return Err(());
                }
                sources.push(source);
                Ok(())
            };
            let mut overflow = false;
            for record in server.live.values().filter_map(Weak::upgrade) {
                if push(Source::Live(record.clone()), record.argv0.len()).is_err() {
                    overflow = true;
                    break;
                }
            }
            if !overflow {
                for record in server.finals.values() {
                    if push(Source::Final(record.clone()), record.argv0.len()).is_err() {
                        overflow = true;
                        break;
                    }
                }
            }
            (!overflow).then_some((server.catalog_revision, sources))
        };
        let Some((revision, sources)) = snapshot else {
            self.send(
                msg_process_listed(ProcessListed {
                    nonce: list.nonce,
                    status: STATUS_BUDGET,
                    revision: 0,
                    entries: Vec::new(),
                    detail: "process catalog exceeds the list reply limit",
                })
                .expect("bounded process list refusal"),
            );
            return;
        };
        let mut entries = sources
            .into_iter()
            .map(|source| match source {
                Source::Live(record) => Entry {
                    process_ref: record.generation,
                    state: PROCESS_STATE_RUNNING,
                    flags: record_flags(&record),
                    pid: record.pid,
                    argv0: record.argv0.clone(),
                },
                Source::Final(record) => Entry {
                    process_ref: record.generation,
                    state: PROCESS_STATE_EXITED,
                    flags: record.flags,
                    pid: record.pid,
                    argv0: record.argv0.clone(),
                },
            })
            .collect::<Vec<_>>();
        entries.sort_unstable_by_key(|entry| entry.process_ref);
        let listed_entries = entries
            .iter()
            .map(|entry| ProcessListEntry {
                process_ref: entry.process_ref,
                state: entry.state,
                flags: entry.flags,
                pid: entry.pid,
                argv0: &entry.argv0,
            })
            .collect();
        match msg_process_listed(ProcessListed {
            nonce: list.nonce,
            status: STATUS_OK,
            revision,
            entries: listed_entries,
            detail: "",
        }) {
            Ok(reply) => self.send(reply),
            Err(_) => self.send(
                msg_process_listed(ProcessListed {
                    nonce: list.nonce,
                    status: STATUS_BUDGET,
                    revision: 0,
                    entries: Vec::new(),
                    detail: "process catalog exceeds the list reply limit",
                })
                .expect("bounded process list refusal"),
            ),
        }
    }

    fn watch(&self, data: &[u8]) {
        let nonce = read_u16(data, 1);
        let process_id = read_u32(data, 3);
        let process_ref = read_u64(data, 7);
        let watch = match parse_process_watch(data) {
            Ok(watch) => watch,
            Err(error) => {
                self.send(watched_error(
                    nonce,
                    process_id,
                    process_ref,
                    error.status(),
                    "invalid process watch",
                ));
                return;
            }
        };
        let server = self.server.0.state.lock().unwrap();
        let mut endpoint = self.endpoint.state.lock().unwrap();
        if !server.accepting || !endpoint.accepting {
            drop(endpoint);
            drop(server);
            self.send(watched_error(
                watch.nonce,
                watch.process_id,
                watch.process_ref,
                STATUS_PERMISSION,
                "process admission closed",
            ));
            return;
        }
        if endpoint.slots.contains_key(&watch.process_id) {
            drop(endpoint);
            drop(server);
            self.send(watched_error(
                watch.nonce,
                watch.process_id,
                watch.process_ref,
                STATUS_CONFLICT,
                "process id in use",
            ));
            return;
        }
        if let Some(weak) = server.live.get(&watch.process_ref) {
            let Some(record) = weak.upgrade() else {
                drop(endpoint);
                drop(server);
                self.send(watched_error(
                    watch.nonce,
                    watch.process_id,
                    watch.process_ref,
                    STATUS_NOT_FOUND,
                    "process not found",
                ));
                return;
            };
            let global_watch_budget_full =
                active_watcher_count(&server) >= self.server.0.policy.max_watchers;
            let mut inner = record.inner.lock().unwrap();
            if inner
                .bindings
                .iter()
                .any(|binding| binding.endpoint_id == self.endpoint.id)
            {
                drop(inner);
                drop(endpoint);
                drop(server);
                self.send(watched_error(
                    watch.nonce,
                    watch.process_id,
                    watch.process_ref,
                    STATUS_CONFLICT,
                    "endpoint already watches this process",
                ));
                return;
            }
            if global_watch_budget_full
                || inner.bindings.len() >= self.server.0.policy.max_watchers_per_generation
            {
                drop(inner);
                drop(endpoint);
                drop(server);
                self.send(watched_error(
                    watch.nonce,
                    watch.process_id,
                    watch.process_ref,
                    STATUS_BUDGET,
                    "process watcher limit reached",
                ));
                return;
            }
            // An ordinary owner which previously unwatched is already charged
            // for this generation. Re-watching replaces that ownership-only
            // charge with a live slot instead of consuming another one.
            if endpoint_usage_after_watch(&endpoint, watch.process_ref)
                > self.server.0.policy.max_per_endpoint
            {
                drop(inner);
                drop(endpoint);
                drop(server);
                self.send(watched_error(
                    watch.nonce,
                    watch.process_id,
                    watch.process_ref,
                    STATUS_BUDGET,
                    "endpoint process limit reached",
                ));
                return;
            }
            if inner.terminal_queued {
                drop(inner);
                drop(endpoint);
                drop(server);
                self.send(watched_error(
                    watch.nonce,
                    watch.process_id,
                    watch.process_ref,
                    STATUS_CONFLICT,
                    "process is publishing its exit",
                ));
                return;
            }
            let wants_stdin = watch.flags & PROCESS_WATCH_STDIN != 0;
            if wants_stdin
                && (inner.child_outcome.is_some()
                    || inner.stdin_controller.is_some()
                    || inner.stdin_state != PROCESS_STDIN_ACCEPTING)
            {
                drop(inner);
                drop(endpoint);
                drop(server);
                self.send(watched_error(
                    watch.nonce,
                    watch.process_id,
                    watch.process_ref,
                    STATUS_CONFLICT,
                    "process stdin is unavailable",
                ));
                return;
            }
            let stdout_next = inner.stdout.next;
            let stderr_next = inner.stderr.as_ref().map_or(0, |stream| stream.next);
            let writable = wants_stdin;
            let mut streams = stream_state(&inner, record.merged);
            if writable && streams & PROCESS_STREAM_STDIN_ACCEPTING != 0 {
                streams |= PROCESS_STREAM_STDIN_WRITABLE;
            }
            let reply = msg_process_watched(ProcessWatched {
                nonce: watch.nonce,
                status: STATUS_OK,
                process_id: watch.process_id,
                process_ref: watch.process_ref,
                state: PROCESS_STATE_RUNNING,
                stream_state: streams,
                stdin_received: inner.stdin_received,
                stdin_acked: inner.stdin_acked,
                stdout_next,
                stderr_next,
                stdin_window: if streams & PROCESS_STREAM_STDIN_WRITABLE != 0 {
                    PROCESS_DEFAULT_STREAM_WINDOW
                } else {
                    0
                },
                stdout_window: if streams & PROCESS_STREAM_STDOUT_OPEN != 0 {
                    PROCESS_DEFAULT_STREAM_WINDOW
                } else {
                    0
                },
                stderr_window: if streams & PROCESS_STREAM_STDERR_OPEN != 0 {
                    PROCESS_DEFAULT_STREAM_WINDOW
                } else {
                    0
                },
                exit_reason: 0,
                kill_cause: 0,
                exit_code: 0,
                detail: "",
            })
            .expect("valid running watch");
            // Reserve and queue the snapshot before publishing the binding.
            // This endpoint's read loop cannot process a response under the
            // new local ID until this synchronous handler returns, while pipe
            // readers remain excluded by the record lock.
            let Err(rejected) = self.out.send(Outbound::message(reply)) else {
                if writable {
                    inner.stdin_controller = Some(self.endpoint.id);
                }
                inner.bindings.push(Binding::new(
                    self.endpoint.id,
                    watch.process_id,
                    Arc::downgrade(&self.endpoint),
                    self.out.clone(),
                    record.merged,
                    stdout_next,
                    stderr_next,
                ));
                endpoint
                    .slots
                    .insert(watch.process_id, EndpointSlot::Bound(record.clone()));
                drop(inner);
                drop(endpoint);
                drop(server);
                record.changed.notify_waiters();
                return;
            };
            drop(inner);
            drop(endpoint);
            drop(server);
            drop(rejected);
        } else if let Some(final_record) = server.finals.get(&watch.process_ref) {
            if endpoint_usage(&endpoint) >= self.server.0.policy.max_per_endpoint {
                drop(endpoint);
                drop(server);
                self.send(watched_error(
                    watch.nonce,
                    watch.process_id,
                    watch.process_ref,
                    STATUS_BUDGET,
                    "endpoint process limit reached",
                ));
                return;
            }
            if watch.flags & PROCESS_WATCH_STDIN != 0 {
                drop(endpoint);
                drop(server);
                self.send(watched_error(
                    watch.nonce,
                    watch.process_id,
                    watch.process_ref,
                    STATUS_CONFLICT,
                    "process stdin is unavailable",
                ));
                return;
            }
            let reply_id = self
                .server
                .0
                .next_generation
                .fetch_add(1, Ordering::Relaxed);
            let final_record = final_record.clone();
            endpoint
                .slots
                .insert(watch.process_id, EndpointSlot::Reply(reply_id));
            let reply = msg_process_watched(ProcessWatched {
                nonce: watch.nonce,
                status: STATUS_OK,
                process_id: watch.process_id,
                process_ref: watch.process_ref,
                state: PROCESS_STATE_EXITED,
                stream_state: final_record.stream_state,
                stdin_received: final_record.stdin_received,
                stdin_acked: final_record.stdin_acked,
                stdout_next: final_record.stdout_next,
                stderr_next: final_record.stderr_next,
                stdin_window: 0,
                stdout_window: 0,
                stderr_window: 0,
                exit_reason: final_record.reason,
                kill_cause: final_record.kill_cause,
                exit_code: final_record.code,
                detail: final_record.detail,
            })
            .expect("valid final watch");
            drop(endpoint);
            drop(server);
            let endpoint = self.endpoint.clone();
            let local_id = watch.process_id;
            let guard = WriterGuard::new(move || {
                let mut state = endpoint.state.lock().unwrap();
                if matches!(state.slots.get(&local_id), Some(EndpointSlot::Reply(id)) if *id == reply_id)
                {
                    state.slots.remove(&local_id);
                }
            });
            let _ = self.out.send(Outbound::guarded(reply, guard));
        } else {
            drop(endpoint);
            drop(server);
            self.send(watched_error(
                watch.nonce,
                watch.process_id,
                watch.process_ref,
                STATUS_NOT_FOUND,
                "process not found",
            ));
        }
    }

    pub(crate) async fn shutdown(&self) {
        let (slots, owned) = {
            let mut endpoint = self.endpoint.state.lock().unwrap();
            endpoint.accepting = false;
            (
                std::mem::take(&mut endpoint.slots),
                std::mem::take(&mut endpoint.owned),
            )
        };
        let mut ordinary = owned
            .into_iter()
            .filter_map(|(generation, record)| record.upgrade().map(|record| (generation, record)))
            .collect::<FxHashMap<_, _>>();
        let mut active_pending = Vec::new();
        for (process_id, slot) in slots {
            match slot {
                EndpointSlot::Pending(pending) => {
                    pending.endpoint_lost.store(true, Ordering::Release);
                    // Store cancellation even if run_spawn has not polled its
                    // semaphore/cancel select yet.
                    pending.cancel.notify_one();
                    if pending
                        .phase
                        .compare_exchange(
                            PENDING_QUEUED,
                            PENDING_DONE,
                            Ordering::AcqRel,
                            Ordering::Acquire,
                        )
                        .is_ok()
                    {
                        release_pending(&pending, false);
                        pending.mark_completed();
                    } else if !pending.completed.load(Ordering::Acquire) {
                        active_pending.push(pending);
                    }
                }
                EndpointSlot::Bound(record) => {
                    let mut inner = record.inner.lock().unwrap();
                    if let Some(index) = binding_index(&inner, self.endpoint.id, process_id) {
                        inner.bindings.swap_remove(index);
                        if inner.stdin_controller == Some(self.endpoint.id) {
                            inner.stdin_controller = None;
                        }
                    }
                    drop(inner);
                    record.changed.notify_waiters();
                }
                EndpointSlot::Reply(_) => {}
            }
        }
        // A native spawn call which had already acquired its semaphore cannot
        // be canceled safely. Let it finish installing its unbound result,
        // then collect ordinary children here before the connection returns.
        // Detachable children deliberately remain in the server registry.
        for pending in active_pending {
            while !pending.completed.load(Ordering::Acquire) {
                let done = pending.done.notified();
                if pending.completed.load(Ordering::Acquire) {
                    break;
                }
                done.await;
            }
            if pending.detachable {
                continue;
            }
            let record = self
                .server
                .0
                .state
                .lock()
                .unwrap()
                .live
                .get(&pending.generation)
                .and_then(Weak::upgrade);
            if let Some(record) = record {
                ordinary.insert(record.generation, record);
            }
        }
        let ordinary = ordinary.into_values().collect::<Vec<_>>();
        for record in &ordinary {
            let mut inner = record.inner.lock().unwrap();
            inner.stdin_tx.take();
            if inner.child_outcome.is_none()
                && inner.exit_override.is_none()
                && cleanup_terminate(record).is_ok()
            {
                inner.exit_override = Some(ExitOverride {
                    reason: PROCESS_EXIT_KILLED,
                    kill_cause: PROCESS_KILL_OWNER_LOST,
                });
            }
        }
        wait_and_force(
            &ordinary,
            PROCESS_KILL_OWNER_LOST,
            self.server.0.policy.kill_grace,
        )
        .await;
        for record in &ordinary {
            abort_pipes(record);
        }
        // Pipe abortion makes terminal publication eligible. Keep shutdown
        // bounded, but leave any unusually slow record live so its own waiter
        // can publish the eventual EXIT instead of orphaning peer watchers.
        let terminals = async {
            for record in &ordinary {
                record.wait_terminal().await;
            }
        };
        let _ = tokio::time::timeout(
            self.server
                .0
                .policy
                .kill_grace
                .max(Duration::from_millis(100)),
            terminals,
        )
        .await;
        self.endpoint.state.lock().unwrap().request_bytes = 0;
    }
}

impl Binding {
    fn new(
        endpoint_id: u64,
        process_id: u32,
        endpoint: Weak<Endpoint>,
        out: OutboundSender,
        merged: bool,
        stdout_floor: u64,
        stderr_floor: u64,
    ) -> Self {
        Self {
            endpoint_id,
            process_id,
            endpoint,
            out,
            stdout: BindingStream {
                floor: stdout_floor,
                acked: stdout_floor,
                frames: VecDeque::new(),
            },
            stderr: (!merged).then(|| BindingStream {
                floor: stderr_floor,
                acked: stderr_floor,
                frames: VecDeque::new(),
            }),
        }
    }
}

fn read_u16(data: &[u8], offset: usize) -> u16 {
    data.get(offset..offset + 2)
        .map(|bytes| u16::from_le_bytes([bytes[0], bytes[1]]))
        .unwrap_or(0)
}

fn read_u32(data: &[u8], offset: usize) -> u32 {
    data.get(offset..offset + 4)
        .map(|bytes| u32::from_le_bytes(bytes.try_into().unwrap()))
        .unwrap_or(0)
}

fn read_u64(data: &[u8], offset: usize) -> u64 {
    data.get(offset..offset + 8)
        .map(|bytes| u64::from_le_bytes(bytes.try_into().unwrap()))
        .unwrap_or(0)
}

fn record_flags(record: &Record) -> u8 {
    let mut flags = 0;
    if record.merged {
        flags |= PROCESS_SPAWN_MERGE_STDERR;
    }
    if record.detachable {
        flags |= PROCESS_SPAWN_DETACHABLE;
    }
    flags
}

fn binding_index(inner: &RecordInner, endpoint_id: u64, process_id: u32) -> Option<usize> {
    inner
        .bindings
        .iter()
        .position(|binding| binding.endpoint_id == endpoint_id && binding.process_id == process_id)
}

fn remove_binding_at(inner: &mut RecordInner, index: usize) -> Binding {
    let endpoint_id = inner.bindings[index].endpoint_id;
    let binding = inner.bindings.swap_remove(index);
    if inner.stdin_controller == Some(endpoint_id) {
        inner.stdin_controller = None;
    }
    binding
}

fn remove_bound_slot(endpoint: &Endpoint, process_id: u32, record: &Arc<Record>) {
    let mut state = endpoint.state.lock().unwrap();
    if matches!(state.slots.get(&process_id), Some(EndpointSlot::Bound(current)) if Arc::ptr_eq(current, record))
    {
        state.slots.remove(&process_id);
    }
}

fn release_record_endpoint_slots(record: &Arc<Record>) {
    let bindings = {
        let mut inner = record.inner.lock().unwrap();
        inner.stdin_controller = None;
        std::mem::take(&mut inner.bindings)
    };
    for binding in bindings {
        if let Some(endpoint) = binding.endpoint.upgrade() {
            remove_bound_slot(&endpoint, binding.process_id, record);
        }
    }
    record.changed.notify_waiters();
}

fn release_pending(pending: &Arc<Pending>, keep_generation: bool) {
    let Some(server) = pending.server.upgrade() else {
        return;
    };
    let mut server_state = server.state.lock().unwrap();
    let endpoint = pending.endpoint.upgrade();
    let mut endpoint_state = endpoint
        .as_ref()
        .map(|endpoint| endpoint.state.lock().unwrap());
    server_state.pending.remove(&pending.generation);
    let release_request = !pending.request_released.swap(true, Ordering::AcqRel);
    if release_request {
        server_state.request_bytes = server_state
            .request_bytes
            .saturating_sub(pending.request_bytes);
    }
    if !keep_generation {
        server_state.generations = server_state.generations.saturating_sub(1);
    }
    if let Some(endpoint_state) = endpoint_state.as_mut() {
        if release_request {
            endpoint_state.request_bytes = endpoint_state
                .request_bytes
                .saturating_sub(pending.request_bytes);
        }
        if !keep_generation
            && matches!(endpoint_state.slots.get(&pending.process_id), Some(EndpointSlot::Pending(current)) if current.generation == pending.generation)
        {
            endpoint_state.slots.remove(&pending.process_id);
        }
    }
}

fn complete_spawn_failure(pending: &Arc<Pending>, nonce: u16, status: u8, detail: &str) {
    if pending.phase.swap(PENDING_DONE, Ordering::AcqRel) == PENDING_DONE {
        return;
    }
    let endpoint_alive = pending
        .endpoint
        .upgrade()
        .is_some_and(|endpoint| endpoint.state.lock().unwrap().accepting);
    if !endpoint_alive {
        release_pending(pending, false);
        pending.mark_completed();
        return;
    }
    // Request storage is no longer needed. The generation and endpoint ID stay
    // reserved through the failed STARTED frame's writer guard.
    release_pending(pending, true);
    let msg = msg_process_started(ProcessStarted {
        nonce,
        status,
        process_id: pending.process_id,
        process_ref: 0,
        stdin_window: 0,
        stdout_window: 0,
        stderr_window: 0,
        detail,
    })
    .unwrap_or_else(|_| {
        msg_process_started(ProcessStarted {
            nonce,
            status: STATUS_OTHER,
            process_id: pending.process_id,
            process_ref: 0,
            stdin_window: 0,
            stdout_window: 0,
            stderr_window: 0,
            detail: "process spawn failed",
        })
        .expect("bounded fallback detail")
    });
    let pending_for_guard = pending.clone();
    let guard = WriterGuard::new(move || release_pending(&pending_for_guard, false));
    let _ = pending.out.send(Outbound::guarded(msg, guard));
    pending.mark_completed();
}

fn transfer_pending_to_record(pending: &Arc<Pending>, record: &Arc<Record>) -> bool {
    if pending.phase.swap(PENDING_DONE, Ordering::AcqRel) == PENDING_DONE {
        return false;
    }
    let Some(server) = pending.server.upgrade() else {
        pending.mark_completed();
        return false;
    };
    let mut server_state = server.state.lock().unwrap();
    let endpoint = pending.endpoint.upgrade();
    let mut endpoint_state = endpoint
        .as_ref()
        .map(|endpoint| endpoint.state.lock().unwrap());
    server_state.pending.remove(&pending.generation);
    let release_request = !pending.request_released.swap(true, Ordering::AcqRel);
    if release_request {
        server_state.request_bytes = server_state
            .request_bytes
            .saturating_sub(pending.request_bytes);
    }
    server_state
        .live
        .insert(pending.generation, Arc::downgrade(record));
    server_state.catalog_revision = server_state.catalog_revision.wrapping_add(1);
    let mut installed_bound = false;
    if let Some(endpoint_state) = endpoint_state.as_mut() {
        if release_request {
            endpoint_state.request_bytes = endpoint_state
                .request_bytes
                .saturating_sub(pending.request_bytes);
        }
        let owns_pending = matches!(endpoint_state.slots.get(&pending.process_id), Some(EndpointSlot::Pending(current)) if current.generation == pending.generation);
        if server_state.accepting
            && endpoint_state.accepting
            && owns_pending
            && !pending.endpoint_lost.load(Ordering::Acquire)
        {
            endpoint_state
                .slots
                .insert(pending.process_id, EndpointSlot::Bound(record.clone()));
            if !pending.detachable {
                endpoint_state
                    .owned
                    .insert(pending.generation, Arc::downgrade(record));
            }
            installed_bound = true;
        } else if owns_pending {
            endpoint_state.slots.remove(&pending.process_id);
        }
    }
    if !installed_bound {
        let mut inner = record.inner.lock().unwrap();
        inner.bindings.clear();
        inner.stdin_controller = None;
    }
    pending.mark_completed();
    installed_bound
}

fn watched_error(
    nonce: u16,
    process_id: u32,
    process_ref: ProcessRef,
    status: u8,
    detail: &str,
) -> Vec<u8> {
    msg_process_watched(ProcessWatched {
        nonce,
        status,
        process_id,
        process_ref,
        state: 0,
        stream_state: 0,
        stdin_received: 0,
        stdin_acked: 0,
        stdout_next: 0,
        stderr_next: 0,
        stdin_window: 0,
        stdout_window: 0,
        stderr_window: 0,
        exit_reason: 0,
        kill_cause: 0,
        exit_code: 0,
        detail,
    })
    .expect("bounded watch detail")
}

#[cfg(unix)]
fn command_for(req: &ProcessSpawnRequest<'_>, pty_cwd: Option<&[u8]>) -> Command {
    let mut command = Command::new(OsStr::from_bytes(req.argv[0]));
    command.args(req.argv[1..].iter().map(|arg| OsStr::from_bytes(arg)));
    for (key, value) in &req.env {
        command.env(
            OsString::from_vec(key.to_vec()),
            OsString::from_vec(value.to_vec()),
        );
    }
    let cwd = match req.cwd_kind {
        PROCESS_CWD_EXPLICIT => Some(req.cwd),
        PROCESS_CWD_FROM_PTY => pty_cwd,
        _ => None,
    };
    if let Some(cwd) = cwd {
        command.current_dir(PathBuf::from(OsString::from_vec(cwd.to_vec())));
    }
    command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    command.as_std_mut().process_group(0);
    #[cfg(any(target_os = "macos", target_os = "ios"))]
    let apple_fd_directory_available = apple_fd_directory_available();
    #[cfg(any(
        target_os = "macos",
        target_os = "ios",
        not(any(
            target_os = "linux",
            target_os = "macos",
            target_os = "ios",
            target_os = "freebsd",
            target_os = "netbsd",
            target_os = "solaris",
            target_os = "illumos",
        )),
    ))]
    let inherited_fd_limit = inherited_fd_limit();
    // SAFETY: this runs after fork in the child and only invokes libc calls.
    unsafe {
        command.as_std_mut().pre_exec(move || {
            libc::signal(libc::SIGPIPE, libc::SIG_DFL);
            // Enumerate the forked child's actual descriptor table rather than
            // taking a racy parent-side snapshot. `FD_CLOEXEC` leaves Rust's
            // private exec-error pipe usable until exec while preventing every
            // descriptor at or above 3 from reaching the requested program.
            #[cfg(any(
                target_os = "linux",
                target_os = "freebsd",
                target_os = "netbsd",
                target_os = "solaris",
                target_os = "illumos",
            ))]
            close_fds::set_fds_cloexec(3, &[]);
            // close_fds enumerates /dev/fd on Apple. Check it in the parent so
            // unusual chroots retain a complete, if slower, numeric fallback.
            #[cfg(any(target_os = "macos", target_os = "ios"))]
            if apple_fd_directory_available {
                close_fds::set_fds_cloexec(3, &[]);
            } else {
                mark_fd_range_cloexec(inherited_fd_limit);
            }
            // Keep generic Unix builds correct even when close_fds has no
            // native descriptor-table iterator for the target. This path is
            // intentionally slower; supported server platforms use the fast
            // directory or close-range implementations above.
            #[cfg(not(any(
                target_os = "linux",
                target_os = "macos",
                target_os = "ios",
                target_os = "freebsd",
                target_os = "netbsd",
                target_os = "solaris",
                target_os = "illumos",
            )))]
            mark_fd_range_cloexec(inherited_fd_limit);
            Ok(())
        });
    }
    command
}

#[cfg(windows)]
fn command_for(req: &ProcessSpawnRequest<'_>, pty_cwd: Option<&[u8]>) -> Command {
    let argv = req
        .argv
        .iter()
        .map(|value| std::str::from_utf8(value).expect("Windows spawn validated UTF-8"))
        .collect::<Vec<_>>();
    let mut command = Command::new(argv[0]);
    command.args(&argv[1..]);
    for (key, value) in &req.env {
        command.env(
            std::str::from_utf8(key).expect("Windows env key validated UTF-8"),
            std::str::from_utf8(value).expect("Windows env value validated UTF-8"),
        );
    }
    let cwd = match req.cwd_kind {
        PROCESS_CWD_EXPLICIT => Some(req.cwd),
        PROCESS_CWD_FROM_PTY => pty_cwd,
        _ => None,
    };
    if let Some(cwd) = cwd {
        command.current_dir(std::str::from_utf8(cwd).expect("Windows cwd validated UTF-8"));
    }
    command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    // Suspension closes the otherwise unavoidable race between CreateProcess
    // and assigning the child to its kill-on-close job.
    command.creation_flags(CREATE_NEW_PROCESS_GROUP | CREATE_SUSPENDED);
    command
}

#[cfg(all(
    unix,
    any(
        target_os = "macos",
        target_os = "ios",
        not(any(
            target_os = "linux",
            target_os = "macos",
            target_os = "ios",
            target_os = "freebsd",
            target_os = "netbsd",
            target_os = "solaris",
            target_os = "illumos",
        )),
    )
))]
fn inherited_fd_limit() -> libc::c_int {
    let mut limit = std::mem::MaybeUninit::<libc::rlimit>::uninit();
    let hard = unsafe {
        if libc::getrlimit(libc::RLIMIT_NOFILE, limit.as_mut_ptr()) == 0 {
            Some(limit.assume_init().rlim_max)
        } else {
            None
        }
    };
    if let Some(hard) = hard.filter(|value| *value != libc::RLIM_INFINITY) {
        return hard.min(i32::MAX as libc::rlim_t) as libc::c_int;
    }
    let open_max = unsafe { libc::sysconf(libc::_SC_OPEN_MAX) };
    if open_max > 0 {
        open_max.min(i32::MAX as libc::c_long) as libc::c_int
    } else {
        65_536
    }
}

#[cfg(all(
    unix,
    any(
        target_os = "macos",
        target_os = "ios",
        not(any(
            target_os = "linux",
            target_os = "macos",
            target_os = "ios",
            target_os = "freebsd",
            target_os = "netbsd",
            target_os = "solaris",
            target_os = "illumos",
        )),
    )
))]
fn mark_fd_range_cloexec(limit: libc::c_int) {
    for fd in 3..limit {
        let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
        if flags >= 0 && flags & libc::FD_CLOEXEC == 0 {
            unsafe {
                libc::fcntl(fd, libc::F_SETFD, flags | libc::FD_CLOEXEC);
            }
        }
    }
}

#[cfg(any(target_os = "macos", target_os = "ios"))]
fn apple_fd_directory_available() -> bool {
    let directory = unsafe {
        libc::open(
            b"/dev/fd\0".as_ptr().cast(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC,
        )
    };
    if directory < 0 {
        return false;
    }
    unsafe {
        libc::close(directory);
    }
    true
}

fn configure_merged_output(command: &mut Command) -> io::Result<tokio::fs::File> {
    let (reader, writer) = os_pipe::pipe()?;
    let stderr = writer.try_clone()?;
    command.stdout(Stdio::from(writer));
    command.stderr(Stdio::from(stderr));
    #[cfg(unix)]
    let reader = std::fs::File::from(std::os::fd::OwnedFd::from(reader));
    #[cfg(windows)]
    let reader = std::fs::File::from(std::os::windows::io::OwnedHandle::from(reader));
    Ok(tokio::fs::File::from_std(reader))
}

#[cfg(unix)]
fn spawn_child(command: &mut Command) -> io::Result<Spawned> {
    pty::spawn_registered_child(|| {
        let child = command.spawn()?;
        let pid = child
            .id()
            .ok_or_else(|| io::Error::other("spawn returned no child pid"))?;
        let registered_pid = libc::pid_t::try_from(pid)
            .map_err(|_| io::Error::other("child pid exceeds native pid_t"))?;
        Ok((registered_pid, Spawned { child, pid }))
    })
}

#[cfg(windows)]
fn spawn_child(command: &mut Command) -> io::Result<Spawned> {
    let job = create_kill_on_close_job()?;
    let mut child = command.spawn()?;
    let pid = child
        .id()
        .ok_or_else(|| io::Error::other("spawn returned no child pid"))?;
    let process = child
        .raw_handle()
        .ok_or_else(|| io::Error::other("spawn returned no process handle"))?
        as HANDLE;
    if unsafe { AssignProcessToJobObject(job.0, process) } == 0 {
        let error = io::Error::last_os_error();
        let _ = child.start_kill();
        return Err(error);
    }
    if let Err(error) = resume_primary_thread(pid) {
        let _ = child.start_kill();
        return Err(error);
    }
    Ok(Spawned { child, pid, job })
}

#[cfg(windows)]
fn resume_primary_thread(pid: u32) -> io::Result<()> {
    unsafe {
        let snapshot = CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0);
        if snapshot == INVALID_HANDLE_VALUE {
            return Err(io::Error::last_os_error());
        }
        let mut entry: THREADENTRY32 = std::mem::zeroed();
        entry.dwSize = std::mem::size_of::<THREADENTRY32>() as u32;
        let mut found = Thread32First(snapshot, &mut entry) != 0;
        while found {
            if entry.th32OwnerProcessID == pid {
                let thread = OpenThread(THREAD_SUSPEND_RESUME, 0, entry.th32ThreadID);
                if thread.is_null() {
                    let error = io::Error::last_os_error();
                    CloseHandle(snapshot);
                    return Err(error);
                }
                let resumed = ResumeThread(thread);
                CloseHandle(thread);
                CloseHandle(snapshot);
                return if resumed == u32::MAX {
                    Err(io::Error::last_os_error())
                } else {
                    Ok(())
                };
            }
            found = Thread32Next(snapshot, &mut entry) != 0;
        }
        CloseHandle(snapshot);
        Err(io::Error::other("spawned process has no primary thread"))
    }
}

#[cfg(windows)]
fn create_kill_on_close_job() -> io::Result<JobHandle> {
    unsafe {
        let job = CreateJobObjectW(std::ptr::null(), std::ptr::null());
        if job.is_null() {
            return Err(io::Error::last_os_error());
        }
        let handle = JobHandle(job);
        let mut limits: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
        limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        if SetInformationJobObject(
            handle.0,
            JobObjectExtendedLimitInformation,
            (&raw const limits).cast(),
            std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
        ) == 0
        {
            return Err(io::Error::last_os_error());
        }
        Ok(handle)
    }
}

#[cfg(windows)]
fn windows_env_keys_equal(left: &str, right: &str) -> bool {
    let left = left.encode_utf16().collect::<Vec<_>>();
    let right = right.encode_utf16().collect::<Vec<_>>();
    unsafe {
        CompareStringOrdinal(
            left.as_ptr(),
            left.len() as i32,
            right.as_ptr(),
            right.len() as i32,
            1,
        ) == CSTR_EQUAL
    }
}

fn send_stdin_ack(inner: &RecordInner, bytes: u64, stdin_state: u8) {
    for binding in &inner.bindings {
        if let Ok(msg) = msg_process_stdin_ack(ProcessStdinAck {
            process_id: binding.process_id,
            bytes,
            stdin_state,
        }) {
            let _ = binding.out.send(Outbound::message(msg));
        }
    }
}

fn send_stdin_ack_to(inner: &RecordInner, endpoint_id: u64, process_id: u32) {
    let Some(index) = binding_index(inner, endpoint_id, process_id) else {
        return;
    };
    let binding = &inner.bindings[index];
    if let Ok(msg) = msg_process_stdin_ack(ProcessStdinAck {
        process_id,
        bytes: inner.stdin_acked,
        stdin_state: inner.stdin_state,
    }) {
        let _ = binding.out.send(Outbound::message(msg));
    }
}

async fn stdin_writer(
    record: Arc<Record>,
    mut stdin: tokio::process::ChildStdin,
    mut input: mpsc::Receiver<InputChunk>,
) {
    while let Some(chunk) = input.recv().await {
        if stdin.write_all(&chunk.data).await.is_err() {
            {
                let mut inner = record.inner.lock().unwrap();
                let changed = inner.stdin_state != PROCESS_STDIN_CLOSED;
                inner.stdin_state = PROCESS_STDIN_CLOSED;
                inner.stdin_closed_by_child = true;
                inner.stdin_writer_done = true;
                inner.stdin_tx.take();
                if changed {
                    send_stdin_ack(&inner, inner.stdin_acked, PROCESS_STDIN_CLOSED);
                }
            }
            record.changed.notify_waiters();
            try_queue_terminal(&record);
            return;
        }
        {
            let mut inner = record.inner.lock().unwrap();
            inner.stdin_acked = chunk.end;
            while inner
                .stdin_frames
                .front()
                .is_some_and(|end| *end <= chunk.end)
            {
                inner.stdin_frames.pop_front();
            }
            send_stdin_ack(&inner, inner.stdin_acked, inner.stdin_state);
        }
    }
    drop(stdin);
    {
        let mut inner = record.inner.lock().unwrap();
        let changed = inner.stdin_state != PROCESS_STDIN_CLOSED;
        inner.stdin_state = PROCESS_STDIN_CLOSED;
        inner.stdin_writer_done = true;
        if changed {
            send_stdin_ack(&inner, inner.stdin_acked, PROCESS_STDIN_CLOSED);
        }
    }
    record.changed.notify_waiters();
    try_queue_terminal(&record);
}

async fn output_reader(record: Arc<Record>, stream: u8, mut reader: impl AsyncRead + Unpin) {
    let mut buffer = vec![0u8; OUTPUT_FRAME_PAYLOAD];
    loop {
        match reader.read(&mut buffer).await {
            Ok(0) => break,
            Err(_) => {
                host_failure(&record, "process output pipe read failed");
                break;
            }
            Ok(n) => {
                let mut inner = record.inner.lock().unwrap();
                let state = if stream == PROCESS_STREAM_STDOUT {
                    &mut inner.stdout
                } else {
                    inner.stderr.as_mut().expect("separate stderr")
                };
                let offset = state.next;
                let Some(next) = offset.checked_add(n as u64) else {
                    drop(inner);
                    protocol_violation(&record);
                    return;
                };
                state.next = next;
                let mut evicted = Vec::new();
                let mut index = 0;
                while index < inner.bindings.len() {
                    let has_credit = {
                        let binding = &inner.bindings[index];
                        let credit = if stream == PROCESS_STREAM_STDOUT {
                            &binding.stdout
                        } else {
                            binding.stderr.as_ref().expect("separate stderr binding")
                        };
                        let available = offset
                            .checked_sub(credit.acked)
                            .and_then(|debt| PROCESS_DEFAULT_STREAM_WINDOW.checked_sub(debt));
                        available.is_some_and(|bytes| bytes >= n as u64)
                            && credit.frames.len() < PROCESS_MAX_UNACKED_PACKETS
                    };
                    if !has_credit {
                        evicted.push(remove_binding_at(&mut inner, index));
                        continue;
                    }
                    let process_id = inner.bindings[index].process_id;
                    let output = ProcessOutput {
                        process_id,
                        offset,
                        data: &buffer[..n],
                    };
                    let msg = if stream == PROCESS_STREAM_STDOUT {
                        msg_process_stdout(output)
                    } else {
                        msg_process_stderr(output)
                    };
                    let sent = msg.is_ok_and(|msg| {
                        inner.bindings[index]
                            .out
                            .send(Outbound::message(msg))
                            .is_ok()
                    });
                    if sent {
                        let binding = &mut inner.bindings[index];
                        let credit = if stream == PROCESS_STREAM_STDOUT {
                            &mut binding.stdout
                        } else {
                            binding.stderr.as_mut().expect("separate stderr binding")
                        };
                        credit.frames.push_back(next);
                        index += 1;
                    } else {
                        evicted.push(remove_binding_at(&mut inner, index));
                    }
                }
                drop(inner);
                for binding in evicted {
                    binding
                        .out
                        .kick("native process watcher exceeded its output window");
                }
            }
        }
    }
    stream_closed(&record, stream);
}

fn stream_closed(record: &Arc<Record>, stream: u8) {
    {
        let mut inner = record.inner.lock().unwrap();
        if stream == PROCESS_STREAM_STDOUT {
            inner.stdout_readers = inner.stdout_readers.saturating_sub(1);
        } else {
            inner.stderr_readers = inner.stderr_readers.saturating_sub(1);
        }
    }
    record.changed.notify_waiters();
    try_queue_terminal(record);
}

async fn wait_child(record: Arc<Record>, mut child: Child) {
    let result = child.wait().await;
    #[cfg(unix)]
    let outcome = match result {
        Ok(status) => {
            pty::deregister_child_pid(record.pid as libc::pid_t);
            if let Some(code) = status.code() {
                ChildOutcome::Returned(code as u32)
            } else if let Some(signal) = status.signal() {
                ChildOutcome::Signalled(signal as u32)
            } else {
                ChildOutcome::HostFailure
            }
        }
        Err(_) => match pty::take_reaped_child_status(record.pid as libc::pid_t) {
            Some(status) if status >= 0 => ChildOutcome::Returned(status as u32),
            Some(status) => ChildOutcome::Signalled(status.unsigned_abs()),
            None => ChildOutcome::HostFailure,
        },
    };
    #[cfg(windows)]
    let outcome = match result {
        Ok(status) => status
            .code()
            .map(|code| ChildOutcome::Returned(code as u32))
            .unwrap_or(ChildOutcome::HostFailure),
        Err(_) => ChildOutcome::HostFailure,
    };
    {
        let mut inner = record.inner.lock().unwrap();
        inner.child_outcome = Some(outcome);
        if inner.stdin_state == PROCESS_STDIN_ACCEPTING {
            inner.stdin_state = PROCESS_STDIN_CLOSING;
            send_stdin_ack(&inner, inner.stdin_acked, PROCESS_STDIN_CLOSING);
        }
        inner.stdin_tx.take();
    }
    record.mark_reaped();
    #[cfg(unix)]
    let _ = graceful_terminate(&record);
    schedule_residual_cleanup(record.clone());
    try_queue_terminal(&record);
}

fn schedule_residual_cleanup(record: Arc<Record>) {
    tokio::spawn(async move {
        let deadline = tokio::time::sleep(record.server.0.policy.kill_grace);
        tokio::pin!(deadline);
        loop {
            let changed = record.changed.notified();
            if io_tasks_done(&record.inner.lock().unwrap()) {
                break;
            }
            tokio::select! {
                _ = changed => continue,
                _ = &mut deadline => break,
            }
        }
        // The direct child is already reaped, so this targets only residual
        // group/job members. Running it as soon as their inherited pipes close
        // also avoids a Unix process-group-ID reuse window.
        let cleanup_failed = force_kill(&record)
            .err()
            .is_some_and(|error| !process_tree_already_absent(&error));
        let (stdin_abort, output_aborts) = {
            let mut inner = record.inner.lock().unwrap();
            if inner.tree_cleanup_done {
                return;
            }
            inner.tree_cleanup_done = true;
            if cleanup_failed {
                inner.exit_override = Some(ExitOverride {
                    reason: PROCESS_EXIT_HOST_FAILURE,
                    kill_cause: 0,
                });
                inner.cleanup_detail = "residual process tree force-kill failed";
            }
            if io_tasks_done(&inner) {
                (None, Vec::new())
            } else {
                if !cleanup_failed {
                    inner.cleanup_detail = "residual process tree required forceful cleanup";
                }
                let stdin_changed = inner.stdin_state != PROCESS_STDIN_CLOSED;
                inner.stdin_tx.take();
                inner.stdin_state = PROCESS_STDIN_CLOSED;
                inner.stdin_writer_done = true;
                inner.stdout_readers = 0;
                inner.stderr_readers = 0;
                if stdin_changed {
                    send_stdin_ack(&inner, inner.stdin_acked, PROCESS_STDIN_CLOSED);
                }
                (
                    inner.stdin_abort.take(),
                    std::mem::take(&mut inner.output_aborts),
                )
            }
        };
        if let Some(abort) = stdin_abort {
            abort.abort();
        }
        for abort in output_aborts {
            abort.abort();
        }
        record.changed.notify_waiters();
        try_queue_terminal(&record);
    });
}

fn io_tasks_done(inner: &RecordInner) -> bool {
    inner.stdin_writer_done && inner.stdout_readers == 0 && inner.stderr_readers == 0
}

fn schedule_terminate_timeout(record: Arc<Record>, cause: u8) {
    tokio::spawn(async move {
        tokio::select! {
            _ = record.wait_reaped() => return,
            _ = tokio::time::sleep(record.server.0.policy.kill_grace) => {}
        }
        let mut inner = record.inner.lock().unwrap();
        if inner.child_outcome.is_none() && !inner.terminal_queued && force_kill(&record).is_ok() {
            inner.exit_override = Some(ExitOverride {
                reason: PROCESS_EXIT_KILLED,
                kill_cause: cause,
            });
        }
    });
}

fn outcome_fields(outcome: ChildOutcome, override_: Option<ExitOverride>) -> (u8, u8, u32) {
    if matches!(outcome, ChildOutcome::HostFailure) {
        return (PROCESS_EXIT_HOST_FAILURE, 0, 0);
    }
    if let Some(override_) = override_ {
        return (override_.reason, override_.kill_cause, 0);
    }
    match outcome {
        ChildOutcome::Returned(code) => (PROCESS_EXIT_RETURNED, 0, code),
        #[cfg(unix)]
        ChildOutcome::Signalled(signal) => (PROCESS_EXIT_SIGNALLED, 0, signal),
        ChildOutcome::HostFailure => (PROCESS_EXIT_HOST_FAILURE, 0, 0),
    }
}

fn stream_state(inner: &RecordInner, merged: bool) -> u8 {
    let mut state = match inner.stdin_state {
        PROCESS_STDIN_ACCEPTING => PROCESS_STREAM_STDIN_ACCEPTING,
        PROCESS_STDIN_CLOSING => PROCESS_STREAM_STDIN_CLOSING,
        _ => PROCESS_STREAM_STDIN_CLOSED,
    };
    if inner.stdout_readers > 0 {
        state |= PROCESS_STREAM_STDOUT_OPEN;
    }
    if inner.stderr_readers > 0 {
        state |= PROCESS_STREAM_STDERR_OPEN;
    }
    if merged {
        state |= PROCESS_STREAM_MERGED_STDERR;
    }
    state
}

fn try_queue_terminal(record: &Arc<Record>) {
    let terminal = {
        let mut inner = record.inner.lock().unwrap();
        let Some(outcome) = inner.child_outcome else {
            return;
        };
        if !inner.tree_cleanup_done
            || !inner.stdin_writer_done
            || inner.stdout_readers != 0
            || inner.stderr_readers != 0
            || inner.terminal_queued
        {
            return;
        }
        inner.terminal_queued = true;
        let (reason, kill_cause, code) = outcome_fields(outcome, inner.exit_override);
        let final_record = Arc::new(FinalRecord {
            generation: record.generation,
            pid: record.pid,
            flags: record_flags(record),
            argv0: record.argv0.clone(),
            buffer_bytes: record.argv0.len(),
            stdin_received: inner.stdin_received,
            stdin_acked: inner.stdin_acked,
            stdout_next: inner.stdout.next,
            stderr_next: inner.stderr.as_ref().map_or(0, |stream| stream.next),
            stream_state: stream_state(&inner, record.merged),
            reason,
            kill_cause,
            code,
            detail: inner.cleanup_detail,
        });
        inner.stdin_controller = None;
        (std::mem::take(&mut inner.bindings), final_record)
    };
    record.terminal_notify.notify_waiters();
    let (bindings, final_record) = terminal;
    if bindings.is_empty() {
        finish_terminal(record.clone(), final_record);
        return;
    }
    let remaining = Arc::new(AtomicUsize::new(bindings.len()));
    for binding in bindings {
        let msg = msg_process_exit(ProcessExit {
            process_id: binding.process_id,
            reason: final_record.reason,
            kill_cause: final_record.kill_cause,
            code: final_record.code,
            detail: final_record.detail,
        })
        .expect("valid process exit");
        let endpoint = binding.endpoint.upgrade();
        let record_for_guard = record.clone();
        let final_for_guard = final_record.clone();
        let remaining_for_guard = remaining.clone();
        let process_id = binding.process_id;
        let guard = WriterGuard::new(move || {
            if let Some(endpoint) = endpoint {
                remove_bound_slot(&endpoint, process_id, &record_for_guard);
            }
            if remaining_for_guard.fetch_sub(1, Ordering::AcqRel) == 1 {
                finish_terminal(record_for_guard, final_for_guard);
            }
        });
        let _ = binding.out.send(Outbound::guarded(msg, guard));
    }
}

fn finish_terminal(record: Arc<Record>, final_record: Arc<FinalRecord>) {
    if record.detachable {
        let server = record.server.clone();
        server.finish_detached(record, final_record);
    } else {
        record.server.release_record(&record);
    }
}

fn protocol_violation(record: &Arc<Record>) {
    {
        let mut inner = record.inner.lock().unwrap();
        if inner.exit_override.is_none() {
            inner.exit_override = Some(ExitOverride {
                reason: PROCESS_EXIT_PROTOCOL_VIOLATION,
                kill_cause: 0,
            });
        }
        inner.stdin_tx.take();
    }
    let _ = force_kill(record);
}

fn host_failure(record: &Arc<Record>, detail: &'static str) {
    {
        let mut inner = record.inner.lock().unwrap();
        inner.exit_override = Some(ExitOverride {
            reason: PROCESS_EXIT_HOST_FAILURE,
            kill_cause: 0,
        });
        inner.cleanup_detail = detail;
        inner.stdin_tx.take();
    }
    let _ = force_kill(record);
}

async fn terminate_record(record: &Arc<Record>, cause: u8, grace: Duration) {
    {
        let mut inner = record.inner.lock().unwrap();
        inner.stdin_tx.take();
        if inner.child_outcome.is_none()
            && inner.exit_override.is_none()
            && cleanup_terminate(record).is_ok()
        {
            inner.exit_override = Some(ExitOverride {
                reason: PROCESS_EXIT_KILLED,
                kill_cause: cause,
            });
        }
    }
    record.changed.notify_waiters();
    if tokio::time::timeout(grace, record.wait_reaped())
        .await
        .is_err()
    {
        {
            let mut inner = record.inner.lock().unwrap();
            if inner.child_outcome.is_none() && force_kill(record).is_ok() {
                inner.exit_override = Some(ExitOverride {
                    reason: PROCESS_EXIT_KILLED,
                    kill_cause: cause,
                });
            }
        }
        let _ =
            tokio::time::timeout(grace.max(Duration::from_millis(100)), record.wait_reaped()).await;
    }
    abort_pipes(record);
    let _ = tokio::time::timeout(
        grace.max(Duration::from_millis(100)),
        record.wait_tree_cleanup(),
    )
    .await;
}

async fn wait_and_force(records: &[Arc<Record>], cause: u8, grace: Duration) {
    let graceful = async {
        for record in records {
            record.wait_reaped().await;
        }
    };
    if tokio::time::timeout(grace, graceful).await.is_err() {
        for record in records {
            if !record.reaped.load(Ordering::Acquire) {
                let mut inner = record.inner.lock().unwrap();
                if inner.child_outcome.is_none() && force_kill(record).is_ok() {
                    inner.exit_override = Some(ExitOverride {
                        reason: PROCESS_EXIT_KILLED,
                        kill_cause: cause,
                    });
                }
            }
        }
        let forced = async {
            for record in records {
                record.wait_reaped().await;
            }
        };
        let _ = tokio::time::timeout(grace.max(Duration::from_millis(100)), forced).await;
    }
}

fn abort_pipes(record: &Arc<Record>) {
    let (stdin_abort, output_aborts) = {
        let mut inner = record.inner.lock().unwrap();
        inner.stdin_tx.take();
        inner.stdin_state = PROCESS_STDIN_CLOSED;
        inner.stdin_writer_done = true;
        inner.stdout_readers = 0;
        inner.stderr_readers = 0;
        (
            inner.stdin_abort.take(),
            std::mem::take(&mut inner.output_aborts),
        )
    };
    if let Some(abort) = stdin_abort {
        abort.abort();
    }
    for abort in output_aborts {
        abort.abort();
    }
    record.changed.notify_waiters();
    try_queue_terminal(record);
}

#[cfg(unix)]
fn signal_group(pid: ProcessId, signal: libc::c_int) -> io::Result<()> {
    let pid = libc::pid_t::try_from(pid).map_err(|_| io::Error::other("invalid process id"))?;
    if unsafe { libc::kill(-pid, signal) } == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

#[cfg(unix)]
fn graceful_terminate(record: &Record) -> io::Result<()> {
    signal_group(record.pid, libc::SIGTERM)
}

#[cfg(windows)]
fn graceful_terminate(record: &Record) -> io::Result<()> {
    if unsafe { GenerateConsoleCtrlEvent(CTRL_BREAK_EVENT, record.pid) } != 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

#[cfg(unix)]
fn force_kill(record: &Record) -> io::Result<()> {
    signal_group(record.pid, libc::SIGKILL)
}

#[cfg(windows)]
fn force_kill(record: &Record) -> io::Result<()> {
    if unsafe { TerminateJobObject(record.job.0, 1) } != 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

#[cfg(unix)]
fn process_tree_already_absent(error: &io::Error) -> bool {
    error.raw_os_error() == Some(libc::ESRCH)
}

#[cfg(windows)]
fn process_tree_already_absent(_error: &io::Error) -> bool {
    false
}

#[cfg(unix)]
fn cleanup_terminate(record: &Record) -> io::Result<()> {
    graceful_terminate(record)
}

#[cfg(windows)]
fn cleanup_terminate(record: &Record) -> io::Result<()> {
    force_kill(record)
}

#[cfg(unix)]
fn control_signal(record: &Record, value: u32) -> (u8, &'static str) {
    let signal = i32::try_from(value).ok().filter(|signal| *signal > 0);
    match signal {
        Some(signal) => match signal_group(record.pid, signal) {
            Ok(()) => (STATUS_OK, ""),
            Err(error) if error.raw_os_error() == Some(libc::EINVAL) => {
                (STATUS_INVALID, "invalid signal")
            }
            Err(error) => (STATUS_OTHER, os_error_detail(error)),
        },
        None => (STATUS_INVALID, "invalid signal"),
    }
}

#[cfg(windows)]
fn control_signal(record: &Record, value: u32) -> (u8, &'static str) {
    if value != CTRL_BREAK_EVENT {
        return (STATUS_OTHER, "signal is unsupported on Windows");
    }
    match graceful_terminate(record) {
        Ok(()) => (STATUS_OK, ""),
        Err(_) => (STATUS_OTHER, "console control is unavailable"),
    }
}

#[cfg(unix)]
fn os_error_detail(error: io::Error) -> &'static str {
    match error.raw_os_error() {
        Some(libc::ESRCH) => "process already exited",
        Some(libc::EPERM) => "permission denied signaling process group",
        Some(libc::EINVAL) => "invalid signal",
        _ => "process control failed",
    }
}

#[cfg(windows)]
fn os_error_detail(_error: io::Error) -> &'static str {
    "process control failed"
}

#[cfg(test)]
mod outbox_tests {
    use super::*;

    #[test]
    fn duration_policy_rejects_panicking_float_values() {
        assert_eq!(parse_duration("1.5"), Some(Duration::from_millis(1500)));
        assert!(parse_duration("-1").is_none());
        assert!(parse_duration("NaN").is_none());
        assert!(parse_duration("1e300").is_none());
    }

    #[test]
    fn host_failure_outcome_precedes_a_kill_cause() {
        assert_eq!(
            outcome_fields(
                ChildOutcome::HostFailure,
                Some(ExitOverride {
                    reason: PROCESS_EXIT_KILLED,
                    kill_cause: PROCESS_KILL_CLIENT,
                }),
            ),
            (PROCESS_EXIT_HOST_FAILURE, 0, 0)
        );
    }

    #[cfg(unix)]
    #[test]
    fn only_a_missing_unix_process_tree_is_already_clean() {
        assert!(process_tree_already_absent(&io::Error::from_raw_os_error(
            libc::ESRCH
        )));
        assert!(!process_tree_already_absent(&io::Error::from_raw_os_error(
            libc::EPERM
        )));
    }

    #[test]
    fn frame_overflow_kicks_once_and_runs_rejected_writer_guards() {
        let (tx, mut rx) = mpsc::channel(1);
        let (kick, mut kick_rx) = mpsc::unbounded_channel();
        let out = OutboundSender::new(tx, 1024, kick);
        assert!(out.send(Outbound::message(vec![1])).is_ok());

        let dropped = Arc::new(AtomicUsize::new(0));
        let dropped_for_guard = dropped.clone();
        assert!(
            out.send(Outbound::guarded(
                vec![2],
                WriterGuard::new(move || {
                    dropped_for_guard.fetch_add(1, Ordering::Relaxed);
                }),
            ))
            .is_err()
        );
        assert_eq!(dropped.load(Ordering::Relaxed), 1);
        assert_eq!(rx.len(), 1);
        assert!(kick_rx.try_recv().is_ok());

        assert!(out.send(Outbound::message(vec![3])).is_err());
        assert!(kick_rx.try_recv().is_err(), "overflow kick is one-shot");
        drop(rx.try_recv().expect("the admitted frame"));
        assert_eq!(out.state.queued_bytes.load(Ordering::Acquire), 0);
    }

    #[test]
    fn byte_overflow_is_bounded_even_when_frame_slots_remain() {
        let (tx, mut rx) = mpsc::channel(4);
        let (kick, mut kick_rx) = mpsc::unbounded_channel();
        let out = OutboundSender::new(tx, 3, kick);
        assert!(out.send(Outbound::message(vec![1, 2, 3])).is_ok());
        assert!(out.send(Outbound::message(vec![4])).is_err());
        assert_eq!(rx.len(), 1);
        assert!(kick_rx.try_recv().is_ok());
        let admitted = rx.try_recv().expect("the admitted frame");
        let (data, guard) = admitted.into_parts();
        assert_eq!(data, vec![1, 2, 3]);
        assert_eq!(
            out.state.queued_bytes.load(Ordering::Acquire),
            3,
            "dequeue must retain the byte reservation until the socket write completes"
        );
        drop(guard);
        assert_eq!(out.state.queued_bytes.load(Ordering::Acquire), 0);
    }

    #[test]
    fn closed_writer_kicks_the_endpoint() {
        let (tx, rx) = mpsc::channel(1);
        let (kick, mut kick_rx) = mpsc::unbounded_channel();
        let out = OutboundSender::new(tx, 1024, kick);
        drop(rx);

        assert!(out.send(Outbound::message(vec![1])).is_err());
        assert!(
            kick_rx
                .try_recv()
                .expect("closed writer did not kick endpoint")
                .contains("writer closed")
        );
        assert!(out.send(Outbound::message(vec![2])).is_err());
        assert!(kick_rx.try_recv().is_err(), "writer kick is one-shot");
        assert_eq!(out.state.queued_bytes.load(Ordering::Acquire), 0);
    }

    #[tokio::test]
    async fn stalled_guarded_frame_kicks_the_endpoint() {
        let (tx, mut rx) = mpsc::channel(1);
        let (kick, mut kick_rx) = mpsc::unbounded_channel();
        let out =
            OutboundSender::with_guarded_frame_timeout(tx, 1024, kick, Duration::from_millis(10));
        let dropped = Arc::new(AtomicBool::new(false));
        let dropped_for_guard = dropped.clone();
        assert!(
            out.send(Outbound::guarded(
                vec![1],
                WriterGuard::new(move || dropped_for_guard.store(true, Ordering::Release)),
            ))
            .is_ok()
        );
        let reason = tokio::time::timeout(Duration::from_secs(1), kick_rx.recv())
            .await
            .expect("guard watchdog did not fire")
            .expect("kick channel closed");
        assert!(reason.contains("timed out"));
        assert!(!dropped.load(Ordering::Acquire));
        drop(rx.recv().await.expect("guarded frame"));
        assert!(dropped.load(Ordering::Acquire));
    }

    #[tokio::test]
    async fn completed_guarded_frame_cancels_its_watchdog() {
        let (tx, mut rx) = mpsc::channel(1);
        let (kick, mut kick_rx) = mpsc::unbounded_channel();
        let out =
            OutboundSender::with_guarded_frame_timeout(tx, 1024, kick, Duration::from_millis(20));
        assert!(
            out.send(Outbound::guarded(vec![1], WriterGuard::new(|| {})))
                .is_ok()
        );
        drop(rx.recv().await.expect("guarded frame"));
        tokio::time::sleep(Duration::from_millis(40)).await;
        assert!(kick_rx.try_recv().is_err());
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::fd::AsRawFd;

    async fn recv(rx: &mut mpsc::Receiver<Outbound>) -> Vec<u8> {
        let outbound = tokio::time::timeout(Duration::from_secs(5), rx.recv())
            .await
            .expect("process message timeout")
            .expect("process outbox closed");
        let (data, guard) = outbound.into_parts();
        drop(guard);
        data
    }

    fn outbox(server: &Server) -> (OutboundSender, mpsc::Receiver<Outbound>) {
        let (max_frames, max_bytes) = server.outbox_limits();
        let (out, rx) = mpsc::channel(max_frames);
        let (kick, _kick_rx) = mpsc::unbounded_channel();
        (OutboundSender::new(out, max_bytes, kick), rx)
    }

    fn manager() -> (Server, Manager, mpsc::Receiver<Outbound>) {
        let server = Server::new(false, true);
        let (out, rx) = outbox(&server);
        let manager = server.endpoint(out);
        (server, manager, rx)
    }

    fn server_with_watcher_limits(max_watchers: usize, max_per_child: usize) -> Server {
        let mut server = Server::new(false, true);
        let inner = Arc::get_mut(&mut server.0).expect("new server has no other owners");
        inner.policy.max_watchers = max_watchers;
        inner.policy.max_watchers_per_generation = max_per_child;
        server
    }

    #[test]
    fn owner_rewatch_replaces_its_unbound_usage_charge() {
        let mut endpoint = EndpointState::default();
        endpoint.owned.insert(7, Weak::new());
        assert_eq!(endpoint_usage(&endpoint), 1);
        assert_eq!(endpoint_usage_after_watch(&endpoint, 7), 1);
        assert_eq!(endpoint_usage_after_watch(&endpoint, 8), 2);
    }

    fn spawn_message(id: u32, flags: u8, argv: Vec<&[u8]>) -> Vec<u8> {
        msg_process_spawn(&ProcessSpawnRequest {
            nonce: id as u16,
            process_id: id,
            flags,
            cwd_kind: PROCESS_CWD_DEFAULT,
            src_pty_id: 0,
            cwd: b"",
            argv,
            env: vec![],
        })
        .unwrap()
    }

    async fn await_started(rx: &mut mpsc::Receiver<Outbound>) -> ProcessStarted<'static> {
        let msg = recv(rx).await;
        let reply = parse_process_started(&msg).unwrap();
        ProcessStarted {
            nonce: reply.nonce,
            status: reply.status,
            process_id: reply.process_id,
            process_ref: reply.process_ref,
            stdin_window: reply.stdin_window,
            stdout_window: reply.stdout_window,
            stderr_window: reply.stderr_window,
            detail: "",
        }
    }

    #[tokio::test]
    async fn ordinary_process_streams_binary_output_and_exit() {
        let (_server, manager, mut rx) = manager();
        manager.spawn(
            &spawn_message(
                41,
                0,
                vec![b"/bin/sh", b"-c", b"printf 'a\\000b'; printf err >&2"],
            ),
            None,
        );
        assert_eq!(await_started(&mut rx).await.status, STATUS_OK);
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        loop {
            let msg = recv(&mut rx).await;
            match msg[0] {
                S2C_PROCESS_STDOUT => {
                    let output = parse_process_stdout(&msg).unwrap();
                    stdout.extend_from_slice(output.data);
                    manager.handle(
                        &msg_process_output_ack(ProcessOutputAck {
                            process_id: 41,
                            stream: PROCESS_STREAM_STDOUT,
                            bytes: output.offset + output.data.len() as u64,
                        })
                        .unwrap(),
                    );
                }
                S2C_PROCESS_STDERR => {
                    let output = parse_process_stderr(&msg).unwrap();
                    stderr.extend_from_slice(output.data);
                    manager.handle(
                        &msg_process_output_ack(ProcessOutputAck {
                            process_id: 41,
                            stream: PROCESS_STREAM_STDERR,
                            bytes: output.offset + output.data.len() as u64,
                        })
                        .unwrap(),
                    );
                }
                S2C_PROCESS_STDIN_ACK => {}
                S2C_PROCESS_EXIT => break,
                opcode => panic!("unexpected process opcode {opcode:#x}"),
            }
        }
        assert_eq!(stdout, b"a\0b");
        assert_eq!(stderr, b"err");
        manager.shutdown().await;
    }

    #[tokio::test]
    async fn server_caps_output_frames_for_bulk_fairness() {
        let (_server, manager, mut rx) = manager();
        manager.spawn(
            &spawn_message(
                42,
                0,
                vec![
                    b"/bin/sh",
                    b"-c",
                    b"dd if=/dev/zero bs=65536 count=2 2>/dev/null",
                ],
            ),
            None,
        );
        assert_eq!(await_started(&mut rx).await.status, STATUS_OK);
        let mut received = 0u64;
        loop {
            let msg = recv(&mut rx).await;
            match msg[0] {
                S2C_PROCESS_STDOUT => {
                    let output = parse_process_stdout(&msg).unwrap();
                    assert!(output.data.len() <= OUTPUT_FRAME_PAYLOAD);
                    received += output.data.len() as u64;
                    manager.handle(
                        &msg_process_output_ack(ProcessOutputAck {
                            process_id: 42,
                            stream: PROCESS_STREAM_STDOUT,
                            bytes: output.offset + output.data.len() as u64,
                        })
                        .unwrap(),
                    );
                }
                S2C_PROCESS_STDIN_ACK => {}
                S2C_PROCESS_EXIT => break,
                opcode => panic!("unexpected process opcode {opcode:#x}"),
            }
        }
        assert_eq!(received, 2 * 65_536);
        manager.shutdown().await;
    }

    #[tokio::test]
    async fn pending_id_conflicts_before_spawn_finishes() {
        let server = Server::new(false, true);
        let permit = server.0.spawn_slots.acquire().await.unwrap();
        let (out, mut rx) = outbox(&server);
        let manager = server.endpoint(out);
        let msg = spawn_message(9, 0, vec![b"true"]);
        manager.spawn(&msg, None);
        manager.spawn(&msg, None);
        let reply_msg = recv(&mut rx).await;
        let reply = parse_process_started(&reply_msg).unwrap();
        assert_eq!(reply.status, STATUS_CONFLICT);
        drop(permit);
        assert_eq!(await_started(&mut rx).await.status, STATUS_OK);
        manager.shutdown().await;
    }

    #[tokio::test]
    async fn endpoint_shutdown_cancels_queued_spawn_and_releases_admission() {
        let (server, manager, _rx) = manager();
        let permits = u32::try_from(server.0.spawn_slots.available_permits()).unwrap();
        let held = server.0.spawn_slots.acquire_many(permits).await.unwrap();
        let endpoint = Arc::downgrade(&manager.endpoint);
        manager.spawn(&spawn_message(10, 0, vec![b"sleep", b"30"]), None);

        tokio::time::timeout(Duration::from_secs(1), manager.shutdown())
            .await
            .expect("queued spawn cleanup timed out");
        {
            let state = server.0.state.lock().unwrap();
            assert_eq!((state.generations, state.request_bytes), (0, 0));
            assert!(state.pending.is_empty());
        }
        drop(manager);
        tokio::time::timeout(Duration::from_secs(1), async {
            while endpoint.upgrade().is_some() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("canceled spawn task retained its request behind the semaphore");
        drop(held);
        server.shutdown().await;
    }

    #[tokio::test]
    async fn endpoint_shutdown_waits_for_pending_registry_transition() {
        let (server, manager, _rx) = manager();
        let pending = Arc::new(Pending {
            generation: 99,
            process_id: 10,
            detachable: false,
            request_bytes: 1,
            endpoint: Arc::downgrade(&manager.endpoint),
            server: Arc::downgrade(&server.0),
            out: manager.out.clone(),
            // Simulate completion having claimed the pending operation but
            // not yet published its registry/accounting transition.
            phase: AtomicU8::new(PENDING_DONE),
            endpoint_lost: AtomicBool::new(false),
            request_released: AtomicBool::new(false),
            completed: AtomicBool::new(false),
            cancel: Notify::new(),
            done: Notify::new(),
        });
        {
            let mut state = server.0.state.lock().unwrap();
            state.generations = 1;
            state.request_bytes = 1;
            state.pending.insert(99, Arc::downgrade(&pending));
        }
        {
            let mut endpoint = manager.endpoint.state.lock().unwrap();
            endpoint.request_bytes = 1;
            endpoint
                .slots
                .insert(10, EndpointSlot::Pending(pending.clone()));
        }

        let closing = manager.clone();
        let shutdown = tokio::spawn(async move { closing.shutdown().await });
        tokio::task::yield_now().await;
        assert!(!shutdown.is_finished());
        release_pending(&pending, false);
        pending.mark_completed();
        tokio::time::timeout(Duration::from_secs(1), shutdown)
            .await
            .expect("endpoint did not observe pending completion")
            .unwrap();
        {
            let state = server.0.state.lock().unwrap();
            assert_eq!((state.generations, state.request_bytes), (0, 0));
            assert!(state.pending.is_empty());
        }
        server.shutdown().await;
    }

    #[tokio::test]
    async fn terminal_generation_is_held_until_exit_writer_guard_drops() {
        let (server, manager, mut rx) = manager();
        manager.spawn(&spawn_message(44, 0, vec![b"true"]), None);
        let started = await_started(&mut rx).await;
        let (data, guard) = loop {
            let outbound = tokio::time::timeout(Duration::from_secs(5), rx.recv())
                .await
                .expect("process exit timeout")
                .expect("process outbox closed");
            let (data, guard) = outbound.into_parts();
            if data.first() == Some(&S2C_PROCESS_EXIT) {
                break (data, guard);
            }
            drop(guard);
        };
        assert_eq!(parse_process_exit(&data).unwrap().process_id, 44);
        {
            let state = server.0.state.lock().unwrap();
            assert_eq!(state.generations, 1);
            assert!(state.live.contains_key(&started.process_ref));
        }
        drop(guard);
        {
            let state = server.0.state.lock().unwrap();
            assert_eq!(state.generations, 0);
            assert!(!state.live.contains_key(&started.process_ref));
        }
        manager.shutdown().await;
        server.shutdown().await;
    }

    #[tokio::test]
    async fn process_catalog_and_output_are_public_to_concurrent_watchers() {
        let (server, first, mut first_rx) = manager();
        first.spawn(
            &spawn_message(
                12,
                PROCESS_SPAWN_DETACHABLE,
                vec![b"/bin/sh", b"-c", b"sleep .1; printf shared; sleep .1"],
            ),
            None,
        );
        let started = await_started(&mut first_rx).await;
        assert_eq!(started.status, STATUS_OK);
        let record = first.get(12).unwrap();

        let (out, mut second_rx) = outbox(&server);
        let second = server.endpoint(out);
        second.handle(&msg_process_list(ProcessList { nonce: 1 }));
        let listed_message = recv(&mut second_rx).await;
        let listed = parse_process_listed(&listed_message).unwrap();
        assert!(listed.entries.iter().any(|entry| {
            entry.process_ref == started.process_ref
                && entry.state == PROCESS_STATE_RUNNING
                && entry.argv0 == b"/bin/sh"
        }));

        second.handle(
            &msg_process_watch(ProcessWatch {
                nonce: 2,
                process_id: 44,
                process_ref: started.process_ref,
                flags: 0,
            })
            .unwrap(),
        );
        let watched_message = recv(&mut second_rx).await;
        let watched = parse_process_watched(&watched_message).unwrap();
        assert_eq!(
            (watched.status, watched.state, watched.process_ref),
            (STATUS_OK, PROCESS_STATE_RUNNING, started.process_ref)
        );

        let (first_end, second_end) = tokio::join!(
            async {
                loop {
                    let message = recv(&mut first_rx).await;
                    if message[0] == S2C_PROCESS_STDOUT {
                        let output = parse_process_stdout(&message).unwrap();
                        assert_eq!(output.data, b"shared");
                        break output.offset + output.data.len() as u64;
                    }
                }
            },
            async {
                loop {
                    let message = recv(&mut second_rx).await;
                    if message[0] == S2C_PROCESS_STDOUT {
                        let output = parse_process_stdout(&message).unwrap();
                        assert_eq!(output.data, b"shared");
                        break output.offset + output.data.len() as u64;
                    }
                }
            }
        );
        assert_eq!(first_end, second_end);
        first.handle(
            &msg_process_output_ack(ProcessOutputAck {
                process_id: 12,
                stream: PROCESS_STREAM_STDOUT,
                bytes: first_end,
            })
            .unwrap(),
        );
        {
            let inner = record.inner.lock().unwrap();
            assert_eq!(inner.bindings.len(), 2);
            assert_eq!(
                inner
                    .bindings
                    .iter()
                    .find(|binding| binding.endpoint_id == first.endpoint.id)
                    .unwrap()
                    .stdout
                    .acked,
                first_end
            );
            assert_eq!(
                inner
                    .bindings
                    .iter()
                    .find(|binding| binding.endpoint_id == second.endpoint.id)
                    .unwrap()
                    .stdout
                    .acked,
                0
            );
        }
        second.handle(
            &msg_process_output_ack(ProcessOutputAck {
                process_id: 44,
                stream: PROCESS_STREAM_STDOUT,
                bytes: second_end,
            })
            .unwrap(),
        );
        while recv(&mut first_rx).await[0] != S2C_PROCESS_EXIT {}
        while recv(&mut second_rx).await[0] != S2C_PROCESS_EXIT {}
        second.shutdown().await;
        first.shutdown().await;
        server.shutdown().await;
    }

    #[tokio::test]
    async fn per_child_watcher_capacity_is_reused_after_unwatch() {
        let server = server_with_watcher_limits(8, 1);
        let (owner_out, mut owner_rx) = outbox(&server);
        let owner = server.endpoint(owner_out);
        let (peer_out, mut peer_rx) = outbox(&server);
        let peer = server.endpoint(peer_out);
        owner.spawn(&spawn_message(60, 0, vec![b"sleep", b"30"]), None);
        let started = await_started(&mut owner_rx).await;

        let watch = msg_process_watch(ProcessWatch {
            nonce: 1,
            process_id: 61,
            process_ref: started.process_ref,
            flags: 0,
        })
        .unwrap();
        peer.handle(&watch);
        assert_eq!(
            parse_process_watched(&recv(&mut peer_rx).await)
                .unwrap()
                .status,
            STATUS_BUDGET
        );

        owner.handle(
            &msg_process_control(ProcessControl {
                nonce: 2,
                process_id: 60,
                action: PROCESS_CONTROL_UNWATCH,
                value: 0,
            })
            .unwrap(),
        );
        assert_eq!(recv(&mut owner_rx).await[0], S2C_PROCESS_CONTROLLED);
        peer.handle(&watch);
        assert_eq!(
            parse_process_watched(&recv(&mut peer_rx).await)
                .unwrap()
                .status,
            STATUS_OK
        );

        peer.shutdown().await;
        owner.shutdown().await;
        server.shutdown().await;
    }

    #[tokio::test]
    async fn global_watcher_capacity_reserves_spawns_and_is_reused() {
        let server = server_with_watcher_limits(1, 8);
        let (first_out, mut first_rx) = outbox(&server);
        let first = server.endpoint(first_out);
        let (second_out, mut second_rx) = outbox(&server);
        let second = server.endpoint(second_out);
        first.spawn(&spawn_message(62, 0, vec![b"sleep", b"30"]), None);
        assert_eq!(await_started(&mut first_rx).await.status, STATUS_OK);

        second.spawn(&spawn_message(63, 0, vec![b"sleep", b"30"]), None);
        assert_eq!(await_started(&mut second_rx).await.status, STATUS_BUDGET);

        first.handle(
            &msg_process_control(ProcessControl {
                nonce: 3,
                process_id: 62,
                action: PROCESS_CONTROL_UNWATCH,
                value: 0,
            })
            .unwrap(),
        );
        assert_eq!(recv(&mut first_rx).await[0], S2C_PROCESS_CONTROLLED);
        second.spawn(&spawn_message(63, 0, vec![b"sleep", b"30"]), None);
        assert_eq!(await_started(&mut second_rx).await.status, STATUS_OK);

        second.shutdown().await;
        first.shutdown().await;
        server.shutdown().await;
    }

    #[tokio::test]
    async fn unwatch_is_local_and_a_peer_watcher_can_control_the_child() {
        let (server, owner, mut owner_rx) = manager();
        owner.spawn(&spawn_message(17, 0, vec![b"sleep", b"30"]), None);
        let started = await_started(&mut owner_rx).await;
        let (out, mut peer_rx) = outbox(&server);
        let peer = server.endpoint(out);
        peer.handle(
            &msg_process_watch(ProcessWatch {
                nonce: 1,
                process_id: 18,
                process_ref: started.process_ref,
                flags: 0,
            })
            .unwrap(),
        );
        assert_eq!(
            parse_process_watched(&recv(&mut peer_rx).await)
                .unwrap()
                .status,
            STATUS_OK
        );

        owner.handle(
            &msg_process_control(ProcessControl {
                nonce: 2,
                process_id: 17,
                action: PROCESS_CONTROL_UNWATCH,
                value: 0,
            })
            .unwrap(),
        );
        assert_eq!(recv(&mut owner_rx).await[0], S2C_PROCESS_CONTROLLED);
        assert!(owner.get(17).is_none());
        assert!(
            owner
                .endpoint
                .state
                .lock()
                .unwrap()
                .owned
                .contains_key(&started.process_ref)
        );

        peer.handle(
            &msg_process_control(ProcessControl {
                nonce: 3,
                process_id: 18,
                action: PROCESS_CONTROL_KILL,
                value: 0,
            })
            .unwrap(),
        );
        let mut controlled = false;
        let exit = loop {
            let message = recv(&mut peer_rx).await;
            match message[0] {
                S2C_PROCESS_CONTROLLED => controlled = true,
                S2C_PROCESS_EXIT => break parse_process_exit(&message).unwrap().reason,
                _ => {}
            }
        };
        assert!(controlled);
        assert_eq!(exit, PROCESS_EXIT_KILLED);
        peer.shutdown().await;
        owner.shutdown().await;
        server.shutdown().await;
    }

    #[tokio::test]
    async fn a_slow_watcher_is_evicted_without_blocking_an_acking_watcher() {
        let (server, owner, mut owner_rx) = manager();
        owner.spawn(
            &spawn_message(
                46,
                PROCESS_SPAWN_DETACHABLE,
                vec![
                    b"/bin/sh",
                    b"-c",
                    b"sleep .1; i=0; while [ $i -lt 40 ]; do dd if=/dev/zero bs=32768 count=1 2>/dev/null; i=$((i+1)); sleep .01; done",
                ],
            ),
            None,
        );
        let started = await_started(&mut owner_rx).await;
        let (max_frames, max_bytes) = server.outbox_limits();
        let (tx, mut slow_rx) = mpsc::channel(max_frames);
        let (kick, mut kick_rx) = mpsc::unbounded_channel();
        let slow = server.endpoint(OutboundSender::new(tx, max_bytes, kick));
        slow.handle(
            &msg_process_watch(ProcessWatch {
                nonce: 1,
                process_id: 47,
                process_ref: started.process_ref,
                flags: 0,
            })
            .unwrap(),
        );
        assert_eq!(recv(&mut slow_rx).await[0], S2C_PROCESS_WATCHED);

        let mut received = 0u64;
        loop {
            let message = recv(&mut owner_rx).await;
            match message[0] {
                S2C_PROCESS_STDOUT => {
                    let output = parse_process_stdout(&message).unwrap();
                    received = output.offset + output.data.len() as u64;
                    owner.handle(
                        &msg_process_output_ack(ProcessOutputAck {
                            process_id: 46,
                            stream: PROCESS_STREAM_STDOUT,
                            bytes: received,
                        })
                        .unwrap(),
                    );
                }
                S2C_PROCESS_EXIT => break,
                _ => {}
            }
        }
        assert_eq!(received, 40 * 32_768);
        assert!(
            tokio::time::timeout(Duration::from_secs(2), kick_rx.recv())
                .await
                .unwrap()
                .is_some()
        );
        slow.shutdown().await;
        owner.shutdown().await;
        server.shutdown().await;
    }

    #[tokio::test]
    async fn stdin_writer_is_exclusive_and_reacquired_by_a_fresh_watch() {
        let (server, owner, mut owner_rx) = manager();
        owner.spawn(
            &spawn_message(19, 0, vec![b"/bin/sh", b"-c", b"cat >/dev/null"]),
            None,
        );
        let started = await_started(&mut owner_rx).await;
        let (out, mut peer_rx) = outbox(&server);
        let peer = server.endpoint(out);
        peer.handle(
            &msg_process_watch(ProcessWatch {
                nonce: 1,
                process_id: 20,
                process_ref: started.process_ref,
                flags: 0,
            })
            .unwrap(),
        );
        let peer_watched_message = recv(&mut peer_rx).await;
        let peer_watched = parse_process_watched(&peer_watched_message).unwrap();
        assert_eq!(peer_watched.stream_state & PROCESS_STREAM_STDIN_WRITABLE, 0);
        assert_eq!(peer_watched.stdin_window, 0);
        peer.handle(
            &msg_process_stdin(ProcessStdin {
                process_id: 20,
                offset: 0,
                data: b"peer",
            })
            .unwrap(),
        );
        assert_eq!(
            owner.get(19).unwrap().inner.lock().unwrap().stdin_received,
            0
        );

        let (out, mut writer_rx) = outbox(&server);
        let writer = server.endpoint(out);
        writer.handle(
            &msg_process_watch(ProcessWatch {
                nonce: 2,
                process_id: 21,
                process_ref: started.process_ref,
                flags: PROCESS_WATCH_STDIN,
            })
            .unwrap(),
        );
        let conflict_message = recv(&mut writer_rx).await;
        assert_eq!(
            parse_process_watched(&conflict_message).unwrap().status,
            STATUS_CONFLICT
        );
        assert!(writer.get(21).is_none());

        owner.handle(
            &msg_process_control(ProcessControl {
                nonce: 3,
                process_id: 19,
                action: PROCESS_CONTROL_UNWATCH,
                value: 0,
            })
            .unwrap(),
        );
        assert_eq!(recv(&mut owner_rx).await[0], S2C_PROCESS_CONTROLLED);

        writer.handle(
            &msg_process_watch(ProcessWatch {
                nonce: 4,
                process_id: 21,
                process_ref: started.process_ref,
                flags: PROCESS_WATCH_STDIN,
            })
            .unwrap(),
        );
        let writer_watched_message = recv(&mut writer_rx).await;
        let writer_watched = parse_process_watched(&writer_watched_message).unwrap();
        assert_ne!(
            writer_watched.stream_state & PROCESS_STREAM_STDIN_WRITABLE,
            0
        );
        assert_eq!(writer_watched.stdin_window, PROCESS_DEFAULT_STREAM_WINDOW);
        writer.handle(
            &msg_process_stdin(ProcessStdin {
                process_id: 21,
                offset: 0,
                data: b"writer",
            })
            .unwrap(),
        );
        writer.handle(
            &msg_process_control(ProcessControl {
                nonce: 5,
                process_id: 21,
                action: PROCESS_CONTROL_CLOSE_STDIN,
                value: 0,
            })
            .unwrap(),
        );
        let (reason, code) = loop {
            let message = recv(&mut writer_rx).await;
            if message[0] == S2C_PROCESS_EXIT {
                let exit = parse_process_exit(&message).unwrap();
                break (exit.reason, exit.code);
            }
        };
        assert_eq!((reason, code), (PROCESS_EXIT_RETURNED, 0));
        while recv(&mut peer_rx).await[0] != S2C_PROCESS_EXIT {}
        writer.shutdown().await;
        peer.shutdown().await;
        owner.shutdown().await;
        server.shutdown().await;
    }

    #[tokio::test]
    async fn failed_watch_snapshot_does_not_publish_or_take_stdin() {
        let (server, owner, mut owner_rx) = manager();
        owner.spawn(&spawn_message(22, 0, vec![b"sleep", b"30"]), None);
        let started = await_started(&mut owner_rx).await;
        let record = owner.get(22).unwrap();
        owner.handle(
            &msg_process_control(ProcessControl {
                nonce: 1,
                process_id: 22,
                action: PROCESS_CONTROL_UNWATCH,
                value: 0,
            })
            .unwrap(),
        );
        assert_eq!(recv(&mut owner_rx).await[0], S2C_PROCESS_CONTROLLED);

        let (tx, mut rx) = mpsc::channel(1);
        let (kick, mut kick_rx) = mpsc::unbounded_channel();
        let watcher = server.endpoint(OutboundSender::new(tx, 1024 * 1024, kick));
        watcher.send(vec![0xaa]);
        watcher.handle(
            &msg_process_watch(ProcessWatch {
                nonce: 2,
                process_id: 23,
                process_ref: started.process_ref,
                flags: PROCESS_WATCH_STDIN,
            })
            .unwrap(),
        );

        assert_eq!(recv(&mut rx).await, vec![0xaa]);
        assert!(
            tokio::time::timeout(Duration::from_secs(1), kick_rx.recv())
                .await
                .unwrap()
                .is_some()
        );
        assert!(watcher.get(23).is_none());
        {
            let inner = record.inner.lock().unwrap();
            assert!(inner.stdin_controller.is_none());
            assert!(
                inner
                    .bindings
                    .iter()
                    .all(|binding| binding.endpoint_id != watcher.endpoint.id)
            );
        }

        watcher.shutdown().await;
        owner.shutdown().await;
        server.shutdown().await;
    }

    #[tokio::test]
    async fn final_detached_result_is_public_and_repeatable() {
        let (server, owner, mut owner_rx) = manager();
        owner.spawn(
            &spawn_message(13, PROCESS_SPAWN_DETACHABLE, vec![b"true"]),
            None,
        );
        let started = await_started(&mut owner_rx).await;
        while recv(&mut owner_rx).await[0] != S2C_PROCESS_EXIT {}
        for id in [50, 51] {
            let (out, mut watcher_rx) = outbox(&server);
            let watcher = server.endpoint(out);
            watcher.handle(
                &msg_process_watch(ProcessWatch {
                    nonce: id as u16,
                    process_id: id,
                    process_ref: started.process_ref,
                    flags: 0,
                })
                .unwrap(),
            );
            let message = recv(&mut watcher_rx).await;
            let watched = parse_process_watched(&message).unwrap();
            assert_eq!(
                (watched.status, watched.state, watched.exit_code),
                (STATUS_OK, PROCESS_STATE_EXITED, 0)
            );
            watcher.shutdown().await;
        }
        owner.shutdown().await;
        server.shutdown().await;
    }

    #[tokio::test]
    async fn one_endpoint_cannot_watch_the_same_generation_twice() {
        let (server, manager, mut rx) = manager();
        manager.spawn(
            &spawn_message(46, PROCESS_SPAWN_DETACHABLE, vec![b"sleep", b"30"]),
            None,
        );
        let started = await_started(&mut rx).await;
        manager.handle(
            &msg_process_watch(ProcessWatch {
                nonce: 1,
                process_id: 47,
                process_ref: started.process_ref,
                flags: 0,
            })
            .unwrap(),
        );
        let watched_message = recv(&mut rx).await;
        assert_eq!(
            parse_process_watched(&watched_message).unwrap().status,
            STATUS_CONFLICT
        );
        manager.shutdown().await;
        server.shutdown().await;
    }

    #[tokio::test]
    async fn endpoint_shutdown_reaps_owned_process() {
        let (_server, manager, mut rx) = manager();
        manager.spawn(&spawn_message(43, 0, vec![b"sleep", b"30"]), None);
        assert_eq!(await_started(&mut rx).await.status, STATUS_OK);
        tokio::time::timeout(Duration::from_secs(5), manager.shutdown())
            .await
            .expect("shutdown remained bounded");
        assert!(manager.endpoint.state.lock().unwrap().slots.is_empty());
    }

    #[tokio::test]
    async fn owner_shutdown_publishes_owner_lost_exit_to_peer_watchers() {
        let (server, owner, mut owner_rx) = manager();
        owner.spawn(&spawn_message(48, 0, vec![b"sleep", b"30"]), None);
        let started = await_started(&mut owner_rx).await;

        let (out, mut peer_rx) = outbox(&server);
        let peer = server.endpoint(out);
        peer.handle(
            &msg_process_watch(ProcessWatch {
                nonce: 1,
                process_id: 49,
                process_ref: started.process_ref,
                flags: 0,
            })
            .unwrap(),
        );
        assert_eq!(
            parse_process_watched(&recv(&mut peer_rx).await)
                .unwrap()
                .status,
            STATUS_OK
        );

        tokio::time::timeout(Duration::from_secs(5), owner.shutdown())
            .await
            .expect("owner shutdown remained bounded");
        let (reason, kill_cause) = loop {
            let message = recv(&mut peer_rx).await;
            if message[0] == S2C_PROCESS_EXIT {
                let exit = parse_process_exit(&message).unwrap();
                break (exit.reason, exit.kill_cause);
            }
        };
        assert_eq!(
            (reason, kill_cause),
            (PROCESS_EXIT_KILLED, PROCESS_KILL_OWNER_LOST)
        );
        assert!(peer.get(49).is_none());
        assert!(
            !server
                .0
                .state
                .lock()
                .unwrap()
                .live
                .contains_key(&started.process_ref)
        );

        peer.shutdown().await;
        server.shutdown().await;
    }

    #[tokio::test]
    async fn normal_exit_force_cleans_closed_pipe_descendant() {
        let (server, manager, mut rx) = manager();
        manager.spawn(
            &spawn_message(
                49,
                0,
                vec![
                    b"/bin/sh",
                    b"-c",
                    b"(trap '' TERM; exec </dev/null >/dev/null 2>&1; sleep 30) & printf %s \"$!\"",
                ],
            ),
            None,
        );
        assert_eq!(await_started(&mut rx).await.status, STATUS_OK);
        let mut pid_bytes = Vec::new();
        loop {
            let message = recv(&mut rx).await;
            match message[0] {
                S2C_PROCESS_STDOUT => {
                    let output = parse_process_stdout(&message).unwrap();
                    pid_bytes.extend_from_slice(output.data);
                    manager.handle(
                        &msg_process_output_ack(ProcessOutputAck {
                            process_id: 49,
                            stream: PROCESS_STREAM_STDOUT,
                            bytes: output.offset + output.data.len() as u64,
                        })
                        .unwrap(),
                    );
                }
                S2C_PROCESS_STDIN_ACK => {}
                S2C_PROCESS_EXIT => {
                    assert_eq!(
                        parse_process_exit(&message).unwrap().reason,
                        PROCESS_EXIT_RETURNED
                    );
                    break;
                }
                opcode => panic!("unexpected process opcode {opcode:#x}"),
            }
        }
        let descendant: libc::pid_t = std::str::from_utf8(&pid_bytes).unwrap().parse().unwrap();
        let gone = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if unsafe { libc::kill(descendant, 0) } == -1
                    && io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH)
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .is_ok();
        if !gone {
            unsafe {
                libc::kill(descendant, libc::SIGKILL);
            }
        }
        assert!(gone, "closed-pipe descendant survived terminal cleanup");
        manager.shutdown().await;
        server.shutdown().await;
    }

    #[tokio::test]
    async fn spawn_failure_releases_id_request_and_stream_budgets() {
        let (server, manager, mut rx) = manager();
        manager.spawn(
            &spawn_message(61, 0, vec![b"/definitely/not/a/blit-test-program"]),
            None,
        );
        assert_eq!(await_started(&mut rx).await.status, STATUS_NOT_FOUND);
        assert!(manager.endpoint.state.lock().unwrap().slots.is_empty());
        {
            let state = server.0.state.lock().unwrap();
            assert_eq!(
                (state.generations, state.request_bytes, state.buffer_bytes),
                (0, 0, 0)
            );
        }

        manager.spawn(&spawn_message(61, 0, vec![b"true"]), None);
        assert_eq!(await_started(&mut rx).await.status, STATUS_OK);
        while recv(&mut rx).await[0] != S2C_PROCESS_EXIT {}
        manager.shutdown().await;
        server.shutdown().await;
    }

    #[tokio::test]
    async fn stdin_eof_offsets_and_merged_stderr_roundtrip() {
        let (server, manager, mut rx) = manager();
        manager.spawn(
            &spawn_message(
                62,
                PROCESS_SPAWN_MERGE_STDERR,
                vec![b"/bin/sh", b"-c", b"cat; printf err >&2"],
            ),
            None,
        );
        let started = await_started(&mut rx).await;
        assert_eq!(started.status, STATUS_OK);
        assert_eq!(started.stderr_window, 0);

        manager.handle(
            &msg_process_stdin(ProcessStdin {
                process_id: 62,
                offset: 0,
                data: b"abc",
            })
            .unwrap(),
        );
        manager.handle(
            &msg_process_stdin(ProcessStdin {
                process_id: 62,
                offset: 3,
                data: b"def",
            })
            .unwrap(),
        );
        manager.handle(
            &msg_process_control(ProcessControl {
                nonce: 9,
                process_id: 62,
                action: PROCESS_CONTROL_CLOSE_STDIN,
                value: 0,
            })
            .unwrap(),
        );

        let mut output = Vec::new();
        let mut max_stdin_ack = 0;
        let mut saw_closed = false;
        loop {
            let message = recv(&mut rx).await;
            match message[0] {
                S2C_PROCESS_STDOUT => {
                    output.extend_from_slice(parse_process_stdout(&message).unwrap().data)
                }
                S2C_PROCESS_STDERR => panic!("merged stderr used its own stream"),
                S2C_PROCESS_STDIN_ACK => {
                    let ack = parse_process_stdin_ack(&message).unwrap();
                    max_stdin_ack = max_stdin_ack.max(ack.bytes);
                    saw_closed |= ack.stdin_state == PROCESS_STDIN_CLOSED;
                }
                S2C_PROCESS_CONTROLLED => {
                    assert_eq!(
                        parse_process_controlled(&message).unwrap().status,
                        STATUS_OK
                    )
                }
                S2C_PROCESS_EXIT => {
                    assert_eq!(parse_process_exit(&message).unwrap().code, 0);
                    break;
                }
                opcode => panic!("unexpected process opcode {opcode:#x}"),
            }
        }
        assert_eq!(max_stdin_ack, 6);
        assert!(saw_closed);
        assert_eq!(output, b"abcdeferr");
        manager.shutdown().await;
        server.shutdown().await;
    }

    #[tokio::test]
    async fn invalid_subscriber_stream_traffic_does_not_kill_the_child() {
        let (server, owner, mut owner_rx) = manager();
        owner.spawn(
            &spawn_message(
                63,
                PROCESS_SPAWN_DETACHABLE,
                vec![b"/bin/sh", b"-c", b"sleep .1; printf x; sleep 30"],
            ),
            None,
        );
        let started = await_started(&mut owner_rx).await;
        owner.handle(
            &msg_process_stdin(ProcessStdin {
                process_id: 63,
                offset: 1,
                data: b"x",
            })
            .unwrap(),
        );
        let ack_message = recv(&mut owner_rx).await;
        let ack = parse_process_stdin_ack(&ack_message).unwrap();
        assert_eq!(ack.bytes, 0);

        let (max_frames, max_bytes) = server.outbox_limits();
        let (tx, mut peer_rx) = mpsc::channel(max_frames);
        let (kick, mut kick_rx) = mpsc::unbounded_channel();
        let peer = server.endpoint(OutboundSender::new(tx, max_bytes, kick));
        peer.handle(
            &msg_process_watch(ProcessWatch {
                nonce: 1,
                process_id: 64,
                process_ref: started.process_ref,
                flags: 0,
            })
            .unwrap(),
        );
        assert_eq!(recv(&mut peer_rx).await[0], S2C_PROCESS_WATCHED);
        let peer_end = loop {
            let message = recv(&mut peer_rx).await;
            if message[0] == S2C_PROCESS_STDOUT {
                let output = parse_process_stdout(&message).unwrap();
                break output.offset + output.data.len() as u64;
            }
        };
        peer.handle(
            &msg_process_output_ack(ProcessOutputAck {
                process_id: 64,
                stream: PROCESS_STREAM_STDOUT,
                bytes: peer_end + 1,
            })
            .unwrap(),
        );
        assert!(
            tokio::time::timeout(Duration::from_secs(1), kick_rx.recv())
                .await
                .unwrap()
                .is_some()
        );

        owner.handle(
            &msg_process_control(ProcessControl {
                nonce: 2,
                process_id: 63,
                action: PROCESS_CONTROL_KILL,
                value: 0,
            })
            .unwrap(),
        );
        loop {
            let message = recv(&mut owner_rx).await;
            if message[0] == S2C_PROCESS_EXIT {
                assert_eq!(
                    parse_process_exit(&message).unwrap().reason,
                    PROCESS_EXIT_KILLED
                );
                break;
            }
        }
        peer.shutdown().await;
        owner.shutdown().await;
        server.shutdown().await;
    }

    #[tokio::test]
    async fn detached_final_expiry_is_not_refreshed_by_retrieval() {
        let mut server = Server::new(false, true);
        Arc::get_mut(&mut server.0).unwrap().policy.final_ttl = Duration::from_millis(200);
        let (out, mut rx) = outbox(&server);
        let manager = server.endpoint(out);
        manager.spawn(
            &spawn_message(65, PROCESS_SPAWN_DETACHABLE, vec![b"true"]),
            None,
        );
        let started = await_started(&mut rx).await;
        assert_eq!(started.status, STATUS_OK);
        while recv(&mut rx).await[0] != S2C_PROCESS_EXIT {}

        let (out, mut watch_rx) = outbox(&server);
        let watcher = server.endpoint(out);
        watcher.handle(
            &msg_process_watch(ProcessWatch {
                nonce: 1,
                process_id: 66,
                process_ref: started.process_ref,
                flags: 0,
            })
            .unwrap(),
        );
        let watched_message = recv(&mut watch_rx).await;
        let watched = parse_process_watched(&watched_message).unwrap();
        assert_eq!(
            (watched.status, watched.state),
            (STATUS_OK, PROCESS_STATE_EXITED)
        );

        tokio::time::timeout(Duration::from_secs(1), async {
            while server.0.state.lock().unwrap().generations != 0 {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("detached final record did not expire");
        watcher.handle(
            &msg_process_watch(ProcessWatch {
                nonce: 2,
                process_id: 67,
                process_ref: started.process_ref,
                flags: 0,
            })
            .unwrap(),
        );
        let expired_message = recv(&mut watch_rx).await;
        assert_eq!(
            parse_process_watched(&expired_message).unwrap().status,
            STATUS_NOT_FOUND
        );
        assert_eq!(server.0.state.lock().unwrap().generations, 0);
        watcher.shutdown().await;
        manager.shutdown().await;
        server.shutdown().await;
    }

    #[tokio::test]
    async fn command_inherits_the_server_environment_and_allows_overrides() {
        // HOME was deliberately excluded by the former baseline allowlist, so
        // this locks in full process-environment inheritance rather than only
        // proving that PATH still works.
        let inherited_home = std::env::var_os("HOME").expect("test process has HOME");
        let mut request = ProcessSpawnRequest {
            nonce: 1,
            process_id: 1,
            flags: 0,
            cwd_kind: PROCESS_CWD_DEFAULT,
            src_pty_id: 0,
            cwd: b"",
            argv: vec![b"/bin/sh", b"-c", b"printf %s \"$HOME\""],
            env: vec![],
        };

        let inherited = command_for(&request, None).output().await.unwrap();
        assert!(inherited.status.success());
        assert_eq!(inherited.stdout, inherited_home.as_bytes());

        request.env = vec![(b"HOME", b"/explicit")];
        let overridden = command_for(&request, None).output().await.unwrap();
        assert!(overridden.status.success());
        assert_eq!(overridden.stdout, b"/explicit");
    }

    #[tokio::test]
    async fn descriptors_opened_after_command_construction_are_not_inherited() {
        let request = ProcessSpawnRequest {
            nonce: 1,
            process_id: 1,
            flags: 0,
            cwd_kind: PROCESS_CWD_DEFAULT,
            src_pty_id: 0,
            cwd: b"",
            argv: vec![
                b"/bin/sh",
                b"-c",
                b"test ! -e /proc/self/fd/$1 && test ! -e /dev/fd/$1",
                b"sh",
            ],
            env: vec![],
        };
        let mut command = command_for(&request, None);

        // F_DUPFD deliberately creates a descriptor without FD_CLOEXEC after
        // command_for captured its pre-exec closure. A parent-side descriptor
        // snapshot would miss it; child-side enumeration must not.
        let (reader, _writer) = os_pipe::pipe().unwrap();
        let inherited = unsafe { libc::fcntl(reader.as_raw_fd(), libc::F_DUPFD, 200) };
        assert!(inherited >= 200);
        command.arg(inherited.to_string());

        let spawned = spawn_child(&mut command);
        unsafe {
            libc::close(inherited);
        }
        let mut spawned = spawned.expect("spawn descriptor probe");
        let result = spawned.child.wait().await;
        let success = match result {
            Ok(status) => {
                pty::deregister_child_pid(spawned.pid as libc::pid_t);
                status.success()
            }
            Err(_) => pty::take_reaped_child_status(spawned.pid as libc::pid_t) == Some(0),
        };
        assert!(success, "the late-opened descriptor reached the child");
    }
}

#[cfg(all(test, windows))]
mod windows_tests {
    use super::*;

    fn manager() -> (Server, Manager, mpsc::Receiver<Outbound>) {
        let server = Server::new(false, true);
        let (max_frames, max_bytes) = server.outbox_limits();
        let (tx, rx) = mpsc::channel(max_frames);
        let (kick, _kick_rx) = mpsc::unbounded_channel();
        let manager = server.endpoint(OutboundSender::new(tx, max_bytes, kick));
        (server, manager, rx)
    }

    fn spawn_message(id: u32, argv: Vec<&[u8]>) -> Vec<u8> {
        spawn_message_with_env(id, argv, vec![])
    }

    fn spawn_message_with_env(id: u32, argv: Vec<&[u8]>, env: Vec<(&[u8], &[u8])>) -> Vec<u8> {
        msg_process_spawn(&ProcessSpawnRequest {
            nonce: id as u16,
            process_id: id,
            flags: 0,
            cwd_kind: PROCESS_CWD_DEFAULT,
            src_pty_id: 0,
            cwd: b"",
            argv,
            env,
        })
        .unwrap()
    }

    async fn recv(rx: &mut mpsc::Receiver<Outbound>) -> Vec<u8> {
        let outbound = tokio::time::timeout(Duration::from_secs(10), rx.recv())
            .await
            .expect("process message timeout")
            .expect("process outbox closed");
        let (data, guard) = outbound.into_parts();
        drop(guard);
        data
    }

    #[tokio::test]
    async fn windows_job_spawn_streams_and_resumes_the_primary_thread() {
        let (server, manager, mut rx) = manager();
        let program = std::env::current_exe()
            .unwrap()
            .to_str()
            .expect("test executable path is UTF-8")
            .as_bytes()
            .to_vec();
        let argv = [
            program,
            b"--ignored".to_vec(),
            b"--exact".to_vec(),
            b"process::windows_tests::windows_output_fixture".to_vec(),
            b"--nocapture".to_vec(),
        ];
        manager.spawn(
            &spawn_message(1, argv.iter().map(Vec::as_slice).collect()),
            None,
        );

        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let exit = loop {
            let message = recv(&mut rx).await;
            match message[0] {
                S2C_PROCESS_STARTED => {
                    assert_eq!(parse_process_started(&message).unwrap().status, STATUS_OK)
                }
                S2C_PROCESS_STDOUT => {
                    stdout.extend_from_slice(parse_process_stdout(&message).unwrap().data)
                }
                S2C_PROCESS_STDERR => {
                    stderr.extend_from_slice(parse_process_stderr(&message).unwrap().data)
                }
                S2C_PROCESS_STDIN_ACK => {}
                S2C_PROCESS_EXIT => {
                    let exit = parse_process_exit(&message).unwrap();
                    break (exit.reason, exit.code);
                }
                opcode => panic!("unexpected process opcode {opcode:#x}"),
            }
        };
        assert!(String::from_utf8_lossy(&stdout).contains("out"));
        assert!(String::from_utf8_lossy(&stderr).contains("err"));
        assert_eq!(exit, (PROCESS_EXIT_RETURNED, 7));
        manager.shutdown().await;
        server.shutdown().await;
    }

    #[tokio::test]
    async fn windows_endpoint_shutdown_terminates_the_job() {
        let (server, manager, mut rx) = manager();
        let program = std::env::current_exe()
            .unwrap()
            .to_str()
            .expect("test executable path is UTF-8")
            .as_bytes()
            .to_vec();
        let argv = [
            program,
            b"--ignored".to_vec(),
            b"--exact".to_vec(),
            b"process::windows_tests::windows_sleep_fixture".to_vec(),
            b"--nocapture".to_vec(),
        ];
        manager.spawn(
            &spawn_message(2, argv.iter().map(Vec::as_slice).collect()),
            None,
        );
        loop {
            let message = recv(&mut rx).await;
            if message[0] == S2C_PROCESS_STARTED {
                assert_eq!(parse_process_started(&message).unwrap().status, STATUS_OK);
                break;
            }
        }
        tokio::time::timeout(Duration::from_secs(10), manager.shutdown())
            .await
            .expect("job cleanup remained bounded");
        assert!(manager.endpoint.state.lock().unwrap().slots.is_empty());
        server.shutdown().await;
    }

    #[tokio::test]
    async fn windows_job_shutdown_terminates_descendants() {
        let (server, manager, mut rx) = manager();
        let program = std::env::current_exe()
            .unwrap()
            .to_str()
            .expect("test executable path is UTF-8")
            .as_bytes()
            .to_vec();
        let marker = std::env::temp_dir().join(format!(
            "blit-process-descendant-{}.pid",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&marker);
        let marker_text = marker.to_str().expect("temporary path is UTF-8").as_bytes();
        let argv = [
            program,
            b"--ignored".to_vec(),
            b"--exact".to_vec(),
            b"process::windows_tests::windows_descendant_parent_fixture".to_vec(),
            b"--nocapture".to_vec(),
        ];
        manager.spawn(
            &spawn_message_with_env(
                3,
                argv.iter().map(Vec::as_slice).collect(),
                vec![(b"BLIT_PROCESS_TEST_DESCENDANT_PID", marker_text)],
            ),
            None,
        );
        loop {
            let message = recv(&mut rx).await;
            if message[0] == S2C_PROCESS_STARTED {
                assert_eq!(parse_process_started(&message).unwrap().status, STATUS_OK);
                break;
            }
        }
        let descendant_pid = tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                if let Ok(pid) = std::fs::read_to_string(&marker)
                    && let Ok(pid) = pid.parse::<u32>()
                {
                    break pid;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("descendant fixture did not start");
        let descendant = unsafe { OpenProcess(PROCESS_SYNCHRONIZE, 0, descendant_pid) };
        assert!(!descendant.is_null(), "open descendant process handle");

        tokio::time::timeout(Duration::from_secs(10), manager.shutdown())
            .await
            .expect("job cleanup remained bounded");
        let wait = unsafe { WaitForSingleObject(descendant, 5_000) };
        unsafe {
            CloseHandle(descendant);
        }
        let _ = std::fs::remove_file(&marker);
        assert_eq!(wait, WAIT_OBJECT_0, "descendant escaped the process job");
        server.shutdown().await;
    }

    #[test]
    #[ignore = "spawned by windows_job_spawn_streams_and_resumes_the_primary_thread"]
    fn windows_output_fixture() {
        use std::io::Write;

        std::io::stdout().write_all(b"out").unwrap();
        std::io::stdout().flush().unwrap();
        std::io::stderr().write_all(b"err").unwrap();
        std::io::stderr().flush().unwrap();
        std::process::exit(7);
    }

    #[test]
    #[ignore = "spawned by windows_endpoint_shutdown_terminates_the_job"]
    fn windows_sleep_fixture() {
        std::thread::sleep(Duration::from_secs(30));
    }

    #[test]
    #[ignore = "spawned by windows_job_shutdown_terminates_descendants"]
    fn windows_descendant_parent_fixture() {
        let child = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--ignored",
                "--exact",
                "process::windows_tests::windows_sleep_fixture",
                "--nocapture",
            ])
            .spawn()
            .unwrap();
        let marker = std::env::var("BLIT_PROCESS_TEST_DESCENDANT_PID").unwrap();
        std::fs::write(marker, child.id().to_string()).unwrap();
        std::thread::sleep(Duration::from_secs(30));
    }
}
