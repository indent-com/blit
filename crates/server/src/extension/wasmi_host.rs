//! Wasmi implementation of the `blit_v1` extension host ABI.
//!
//! A [`WasmiAttempt`] owns one dedicated, named operating-system thread. The
//! thread creates a fresh eager Wasmi engine, module, store, and instance, then
//! waits for its owner to publish the attempt before entering `blit_main`.
//! Complete packets cross the thread boundary through acknowledged single-slot
//! handoffs. This bounds storage without making a Tokio worker block on a guest.

use crate::thread_name::{ThreadNames, extension_thread_names};
use std::{
    fmt,
    ops::Range,
    sync::{
        Arc, Condvar, Mutex, MutexGuard,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use tokio::sync::{Notify, oneshot};
use wasmi::{
    Caller, CompilationMode, Config, EnforcedLimits, Engine, Error as WasmiError, ExternType,
    Linker, Memory, Module, Store, StoreLimits, StoreLimitsBuilder, TypedFunc, TypedResumableCall,
    ValType,
};
use wasmparser::{Parser, Payload, TypeRef};

/// Maximum raw module size accepted by the extension runtime.
pub const MODULE_MAX_BYTES: usize = 64 * 1024 * 1024;
/// Maximum complete C2S packet, and maximum adapter frame in either direction.
pub const PACKET_MAX_BYTES: usize = 16 * 1024 * 1024;
/// Maximum entropy request made by one `random` call.
pub const RANDOM_MAX_BYTES: usize = 64 * 1024;

const DEFAULT_MEMORY_BYTES: usize = 128 * 1024 * 1024;
const DEFAULT_TABLE_ELEMENTS: usize = 65_536;
const DEFAULT_VALUE_STACK_BYTES: usize = 128 * 1024;
const DEFAULT_CALL_DEPTH: usize = 1_024;
const DEFAULT_NATIVE_STACK_BYTES: usize = 2 * 1024 * 1024;
const DEFAULT_FUEL_SLICE: u64 = 1_000_000;
const MIN_VALUE_STACK_BYTES: usize = 1_000;
const MIN_NATIVE_STACK_BYTES: usize = 64 * 1024;

/// Per-attempt containment policy sampled by the server at startup.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WasmiHostConfig {
    /// Maximum bytes in the attempt's only linear memory.
    pub memory_bytes: usize,
    /// Maximum elements in the attempt's optional table.
    pub table_elements: usize,
    /// Maximum Wasmi value-stack bytes.
    pub value_stack_bytes: usize,
    /// Maximum Wasmi call depth.
    pub call_depth: usize,
    /// Native stack allocated for the dedicated attempt thread.
    pub native_stack_bytes: usize,
    /// Fuel replenished at each cancellation yield.
    pub fuel_slice: u64,
}

impl Default for WasmiHostConfig {
    fn default() -> Self {
        Self {
            memory_bytes: DEFAULT_MEMORY_BYTES,
            table_elements: DEFAULT_TABLE_ELEMENTS,
            value_stack_bytes: DEFAULT_VALUE_STACK_BYTES,
            call_depth: DEFAULT_CALL_DEPTH,
            native_stack_bytes: DEFAULT_NATIVE_STACK_BYTES,
            fuel_slice: DEFAULT_FUEL_SLICE,
        }
    }
}

impl WasmiHostConfig {
    /// Validate values before passing them to APIs which document panics for
    /// inconsistent limits.
    pub fn validate(&self) -> Result<(), ConfigError> {
        if self.memory_bytes < 64 * 1024 {
            return Err(ConfigError::MemoryTooSmall);
        }
        if self.table_elements == 0 {
            return Err(ConfigError::TableElementsZero);
        }
        if self.value_stack_bytes < MIN_VALUE_STACK_BYTES {
            return Err(ConfigError::ValueStackTooSmall);
        }
        if self.call_depth == 0 {
            return Err(ConfigError::CallDepthZero);
        }
        if self.native_stack_bytes < MIN_NATIVE_STACK_BYTES {
            return Err(ConfigError::NativeStackTooSmall);
        }
        if self.fuel_slice == 0 {
            return Err(ConfigError::FuelSliceZero);
        }
        Ok(())
    }
}

/// Invalid containment configuration.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConfigError {
    MemoryTooSmall,
    TableElementsZero,
    ValueStackTooSmall,
    CallDepthZero,
    NativeStackTooSmall,
    FuelSliceZero,
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let detail = match self {
            Self::MemoryTooSmall => "linear-memory limit is below one WebAssembly page",
            Self::TableElementsZero => "table-element limit is zero",
            Self::ValueStackTooSmall => "value-stack limit is below Wasmi's minimum stack",
            Self::CallDepthZero => "call-depth limit is zero",
            Self::NativeStackTooSmall => "native-stack limit is below 64 KiB",
            Self::FuelSliceZero => "fuel slice is zero",
        };
        f.write_str(detail)
    }
}

impl std::error::Error for ConfigError {}

/// Stable classification used by the extension supervisor.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FailureKind {
    Validation,
    Instantiation,
    AbiMisuse,
    HostFailure,
    Trap,
}

/// A bounded-detail attempt failure.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AttemptFailure {
    pub kind: FailureKind,
    pub detail: String,
}

impl AttemptFailure {
    fn new(kind: FailureKind, detail: impl Into<String>) -> Self {
        let mut detail = detail.into();
        const DETAIL_MAX: usize = 4 * 1024;
        if detail.len() > DETAIL_MAX {
            let mut end = DETAIL_MAX;
            while !detail.is_char_boundary(end) {
                end -= 1;
            }
            detail.truncate(end);
        }
        Self { kind, detail }
    }
}

impl fmt::Display for AttemptFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:?}: {}", self.kind, self.detail)
    }
}

impl std::error::Error for AttemptFailure {}

/// Terminal result reported after the dedicated thread has stopped.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AttemptOutcome {
    Returned(i32),
    Cancelled,
    Failed(AttemptFailure),
}

/// Owned inputs for one Wasmi attempt.
#[derive(Clone, Debug)]
pub struct AttemptSpec {
    pub module: Arc<[u8]>,
    pub module_hash: [u8; 32],
    pub extension_id: u64,
    pub label: Option<String>,
    pub config: WasmiHostConfig,
}

/// Failure to create the dedicated attempt thread.
#[derive(Debug)]
pub enum SpawnError {
    InvalidConfig(ConfigError),
    InvalidExtensionId,
    Thread(std::io::Error),
}

impl fmt::Display for SpawnError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidConfig(error) => write!(f, "invalid Wasmi host configuration: {error}"),
            Self::InvalidExtensionId => f.write_str("extension ID must be non-zero"),
            Self::Thread(error) => write!(f, "failed to spawn extension thread: {error}"),
        }
    }
}

impl std::error::Error for SpawnError {}

/// Invalid [`WasmiAttempt`] lifecycle operation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LifecycleError {
    PreparationAlreadyObserved,
    PreparationChannelClosed,
    NotPrepared,
    AlreadyStarted,
    JoinAlreadyTaken,
    ThreadPanicked,
    JoinTaskCancelled,
}

impl fmt::Display for LifecycleError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let detail = match self {
            Self::PreparationAlreadyObserved => "attempt preparation was already observed",
            Self::PreparationChannelClosed => "attempt preparation channel closed",
            Self::NotPrepared => "attempt has not completed preparation",
            Self::AlreadyStarted => "attempt was already started",
            Self::JoinAlreadyTaken => "attempt thread was already joined",
            Self::ThreadPanicked => "extension thread panicked",
            Self::JoinTaskCancelled => "extension join task was cancelled",
        };
        f.write_str(detail)
    }
}

impl std::error::Error for LifecycleError {}

/// Cancellation handle safe to keep in the asynchronous supervisor.
#[derive(Clone, Debug)]
pub struct AttemptCancellation {
    inner: Arc<AttemptShared>,
}

impl AttemptCancellation {
    /// Mark the attempt cancelled, abort both handoffs, and wake every blocking
    /// host call and the pre-start latch.
    pub fn cancel(&self) {
        if !self.inner.io.cancelled.swap(true, Ordering::AcqRel) {
            self.inner.io.abort_handoffs();
        }
        self.inner.start_cv.notify_all();
    }
}

/// Async side of the two acknowledged packet handoffs.
#[derive(Clone, Debug)]
pub struct HostBridge {
    shared: Arc<AttemptShared>,
}

impl HostBridge {
    /// Reserve the sole inbound slot before the adapter allocates or copies a
    /// frame. Committing the returned reservation publishes it to `recv`.
    pub async fn reserve_to_guest(
        &self,
        packet_len: usize,
    ) -> Result<ToGuestReservation, PacketSendError> {
        validate_packet_len(packet_len)?;
        let reservation = self.shared.io.incoming.reserve_async().await?;
        Ok(ToGuestReservation {
            reservation,
            packet_len,
        })
    }

    /// Take the next guest packet. The slot remains reserved until the returned
    /// lease is acknowledged or dropped after the adapter writes or discards it.
    pub async fn recv_from_guest(&self) -> Option<PacketLease> {
        self.shared.io.outgoing.take_async().await
    }

    /// Orderly EOF from the connection writer to the guest. Already published
    /// inbound data remains readable before `recv` reports zero.
    pub fn close_to_guest(&self) {
        self.shared.io.incoming.seal_producer();
    }

    /// Close the connection reader. Pending and future guest `send` calls
    /// return `-1`.
    pub fn close_from_guest(&self) {
        self.shared.io.outgoing.close_consumer();
    }

    /// Abort the logical endpoint and classify the attempt as cancellation.
    pub fn cancel(&self) {
        AttemptCancellation {
            inner: Arc::clone(&self.shared),
        }
        .cancel();
    }

    #[cfg(test)]
    fn mark_bootstrap_complete(&self) {
        self.shared
            .io
            .bootstrap_complete
            .store(true, Ordering::Release);
    }
}

/// Reserved inbound handoff capacity. Dropping it releases the slot without
/// publishing a packet.
#[derive(Debug)]
pub struct ToGuestReservation {
    reservation: SlotReservation,
    packet_len: usize,
}

impl ToGuestReservation {
    pub fn commit(self, packet: Vec<u8>) -> Result<(), PacketSendError> {
        if packet.len() != self.packet_len {
            return Err(PacketSendError::LengthMismatch);
        }
        self.reservation.commit(packet)
    }
}

/// Rejection before an adapter packet enters its bounded handoff.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PacketSendError {
    Empty,
    TooLarge,
    LengthMismatch,
    Closed,
}

impl fmt::Display for PacketSendError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Empty => f.write_str("packet is empty"),
            Self::TooLarge => f.write_str("packet exceeds the 16 MiB adapter cap"),
            Self::LengthMismatch => {
                f.write_str("packet length does not match its handoff reservation")
            }
            Self::Closed => f.write_str("packet handoff is closed"),
        }
    }
}

impl std::error::Error for PacketSendError {}

/// A packet taken by the asynchronous adapter while its single-slot
/// reservation is still held.
#[derive(Debug)]
pub struct PacketLease {
    handoff: Arc<PacketHandoff>,
    packet: Option<Vec<u8>>,
}

impl PacketLease {
    pub fn packet(&self) -> &[u8] {
        self.packet.as_deref().unwrap_or_default()
    }

    /// Release the reservation after the packet has been fully written.
    pub fn acknowledge(mut self) {
        self.packet.take();
        self.handoff.release_reservation();
    }
}

impl Drop for PacketLease {
    fn drop(&mut self) {
        if self.packet.take().is_some() {
            self.handoff.release_reservation();
        }
    }
}

/// Owner of a dedicated Wasmi attempt thread.
#[derive(Debug)]
pub struct WasmiAttempt {
    names: ThreadNames,
    shared: Arc<AttemptShared>,
    bridge: HostBridge,
    prepared_rx: Option<oneshot::Receiver<Result<(), AttemptFailure>>>,
    prepared: bool,
    started: bool,
    thread: Option<thread::JoinHandle<AttemptOutcome>>,
}

impl WasmiAttempt {
    pub fn thread_names(&self) -> &ThreadNames {
        &self.names
    }

    pub fn cancellation(&self) -> AttemptCancellation {
        AttemptCancellation {
            inner: Arc::clone(&self.shared),
        }
    }

    pub fn bridge(&self) -> HostBridge {
        self.bridge.clone()
    }

    /// Wait until eager translation, strict validation, linking, store-limit
    /// checks, and no-start instantiation have completed on the named thread.
    pub async fn wait_prepared(&mut self) -> Result<(), AttemptFailure> {
        let receiver = self.prepared_rx.take().ok_or_else(|| {
            AttemptFailure::new(
                FailureKind::HostFailure,
                LifecycleError::PreparationAlreadyObserved.to_string(),
            )
        })?;
        let result = receiver.await.map_err(|_| {
            AttemptFailure::new(
                FailureKind::HostFailure,
                LifecycleError::PreparationChannelClosed.to_string(),
            )
        })?;
        if result.is_ok() {
            self.prepared = true;
        }
        result
    }

    /// Release `blit_main` only after the supervisor has installed the logical
    /// connection and its private bootstrap pump.
    pub fn start(&mut self) -> Result<(), LifecycleError> {
        if !self.prepared {
            return Err(LifecycleError::NotPrepared);
        }
        if self.started {
            return Err(LifecycleError::AlreadyStarted);
        }
        self.started = true;
        *lock_unpoison(&self.shared.start) = true;
        self.shared.start_cv.notify_all();
        Ok(())
    }

    pub fn cancel(&self) {
        self.cancellation().cancel();
    }

    /// Join without blocking a Tokio executor worker.
    pub async fn join(mut self) -> Result<AttemptOutcome, LifecycleError> {
        let handle = self.thread.take().ok_or(LifecycleError::JoinAlreadyTaken)?;
        tokio::task::spawn_blocking(move || {
            handle.join().map_err(|_| LifecycleError::ThreadPanicked)
        })
        .await
        .map_err(|_| LifecycleError::JoinTaskCancelled)?
    }
}

impl Drop for WasmiAttempt {
    fn drop(&mut self) {
        if self.thread.is_some() {
            self.cancel();
        }
    }
}

/// Spawn a fresh named thread. Engine construction and all Wasmi work occur on
/// that thread rather than on an async executor worker.
pub fn spawn_attempt(spec: AttemptSpec) -> Result<WasmiAttempt, SpawnError> {
    spec.config.validate().map_err(SpawnError::InvalidConfig)?;
    if spec.extension_id == 0 {
        return Err(SpawnError::InvalidExtensionId);
    }
    let names = extension_thread_names(spec.label.as_deref(), &spec.module_hash, spec.extension_id);
    let io = Arc::new(HostIo::new());
    let shared = Arc::new(AttemptShared {
        io: Arc::clone(&io),
        start: Mutex::new(false),
        start_cv: Condvar::new(),
    });
    let bridge = HostBridge {
        shared: Arc::clone(&shared),
    };
    let (prepared_tx, prepared_rx) = oneshot::channel();
    let thread_shared = Arc::clone(&shared);
    let stack_size = spec.config.native_stack_bytes;
    let thread = thread::Builder::new()
        .name(names.os.clone())
        .stack_size(stack_size)
        .spawn(move || attempt_thread(spec, thread_shared, prepared_tx))
        .map_err(SpawnError::Thread)?;

    Ok(WasmiAttempt {
        names,
        shared,
        bridge,
        prepared_rx: Some(prepared_rx),
        prepared: false,
        started: false,
        thread: Some(thread),
    })
}

/// Perform upload-time validation and eager translation. Callers should run
/// this on their bounded blocking validation pool.
pub fn validate_module(wasm: &[u8], config: &WasmiHostConfig) -> Result<(), AttemptFailure> {
    config
        .validate()
        .map_err(|error| AttemptFailure::new(FailureKind::Validation, error.to_string()))?;
    compile_module(wasm, config).map(|_| ())
}

fn attempt_thread(
    spec: AttemptSpec,
    shared: Arc<AttemptShared>,
    prepared_tx: oneshot::Sender<Result<(), AttemptFailure>>,
) -> AttemptOutcome {
    lower_current_thread_priority();
    let mut runner = match PreparedRunner::new(&spec, Arc::clone(&shared.io)) {
        Ok(runner) => runner,
        Err(error) => {
            let _ = prepared_tx.send(Err(error.clone()));
            shared.io.abort_handoffs();
            return AttemptOutcome::Failed(error);
        }
    };
    if prepared_tx.send(Ok(())).is_err() {
        shared.io.abort_handoffs();
        return AttemptOutcome::Cancelled;
    }

    let mut started = lock_unpoison(&shared.start);
    while !*started && !shared.io.cancelled.load(Ordering::Acquire) {
        started = wait_unpoison(&shared.start_cv, started);
    }
    drop(started);
    if shared.io.cancelled.load(Ordering::Acquire) {
        shared.io.abort_handoffs();
        return AttemptOutcome::Cancelled;
    }

    let outcome = runner.run();
    match &outcome {
        AttemptOutcome::Returned(_) => {
            // Preserve an accepted final C2S packet for the adapter, but stop
            // producing replies to a guest which has returned.
            shared.io.outgoing.seal_producer();
            shared.io.incoming.close_consumer();
        }
        AttemptOutcome::Cancelled | AttemptOutcome::Failed(_) => shared.io.abort_handoffs(),
    }
    outcome
}

struct PreparedRunner {
    store: Store<StoreData>,
    main: TypedFunc<(), i32>,
    fuel_slice: u64,
}

impl PreparedRunner {
    fn new(spec: &AttemptSpec, io: Arc<HostIo>) -> Result<Self, AttemptFailure> {
        let (engine, module) = compile_module(&spec.module, &spec.config)?;
        // Repeat the raw no-start/shape scan immediately before instantiation.
        // Wasmi 1.0 combines instantiation and start execution in its public
        // Linker API, so this check is the safety fence which makes that call
        // incapable of executing guest code.
        validate_binary_shape(&spec.module)?;

        let limits = StoreLimitsBuilder::new()
            .memory_size(spec.config.memory_bytes)
            .table_elements(spec.config.table_elements)
            .instances(1)
            .tables(1)
            .memories(1)
            .build();
        let mut store = Store::new(&engine, StoreData { io, limits });
        store.limiter(|data| &mut data.limits);
        store
            .set_fuel(spec.config.fuel_slice)
            .map_err(|error| failure(FailureKind::HostFailure, "set initial fuel", error))?;
        let mut linker = Linker::new(&engine);
        define_host_abi(&mut linker).map_err(|error| {
            AttemptFailure::new(
                FailureKind::HostFailure,
                format!("failed to define blit_v1 imports: {error}"),
            )
        })?;
        let instance = linker
            .instantiate_and_start(&mut store, &module)
            .map_err(|error| failure(FailureKind::Instantiation, "instantiate module", error))?;
        let main = instance
            .get_typed_func::<(), i32>(&store, "blit_main")
            .map_err(|error| failure(FailureKind::Instantiation, "resolve blit_main", error))?;
        Ok(Self {
            store,
            main,
            fuel_slice: spec.config.fuel_slice,
        })
    }

    fn run(&mut self) -> AttemptOutcome {
        let mut call = match self.main.call_resumable(&mut self.store, ()) {
            Ok(call) => call,
            Err(error) => return classify_execution_error(&self.store.data().io, &error),
        };
        loop {
            match call {
                TypedResumableCall::Finished(code) => {
                    if self.store.data().io.cancelled.load(Ordering::Acquire) {
                        return AttemptOutcome::Cancelled;
                    }
                    return AttemptOutcome::Returned(code);
                }
                TypedResumableCall::HostTrap(trap) => {
                    return classify_execution_error(&self.store.data().io, trap.host_error());
                }
                TypedResumableCall::OutOfFuel(invocation) => {
                    if self.store.data().io.cancelled.load(Ordering::Acquire) {
                        return AttemptOutcome::Cancelled;
                    }
                    // A single translated block can require more fuel than the
                    // configured slice. Grant exactly what is needed in that
                    // exceptional case so execution can reach its next yield.
                    let fuel = self.fuel_slice.max(invocation.required_fuel());
                    if let Err(error) = self.store.set_fuel(fuel) {
                        return AttemptOutcome::Failed(failure(
                            FailureKind::HostFailure,
                            "replenish Wasmi fuel",
                            error,
                        ));
                    }
                    call = match invocation.resume(&mut self.store) {
                        Ok(call) => call,
                        Err(error) => {
                            return classify_execution_error(&self.store.data().io, &error);
                        }
                    };
                }
            }
        }
    }
}

struct StoreData {
    io: Arc<HostIo>,
    limits: StoreLimits,
}

fn configured_engine(config: &WasmiHostConfig) -> Engine {
    let mut engine_config = Config::default();
    engine_config
        .wasm_memory64(false)
        .wasm_multi_memory(false)
        .compilation_mode(CompilationMode::Eager)
        .ignore_custom_sections(true)
        .consume_fuel(true)
        .enforced_limits(EnforcedLimits::strict())
        .set_max_recursion_depth(config.call_depth)
        .set_min_stack_height(MIN_VALUE_STACK_BYTES.min(config.value_stack_bytes))
        .set_max_stack_height(config.value_stack_bytes)
        .set_max_cached_stacks(0);
    Engine::new(&engine_config)
}

fn compile_module(
    wasm: &[u8],
    config: &WasmiHostConfig,
) -> Result<(Engine, Module), AttemptFailure> {
    if wasm.len() > MODULE_MAX_BYTES {
        return Err(AttemptFailure::new(
            FailureKind::Validation,
            "module exceeds the 64 MiB object cap",
        ));
    }
    validate_binary_shape(wasm)?;
    let engine = configured_engine(config);
    let module = Module::new(&engine, wasm)
        .map_err(|error| failure(FailureKind::Validation, "validate/translate module", error))?;
    validate_module_types(&module)?;
    Ok((engine, module))
}

fn validate_binary_shape(wasm: &[u8]) -> Result<(), AttemptFailure> {
    let mut memories = 0_u32;
    let mut tables = 0_u32;
    for payload in Parser::new(0).parse_all(wasm) {
        let payload = payload.map_err(|error| {
            failure(
                FailureKind::Validation,
                "parse WebAssembly module shape",
                error,
            )
        })?;
        match payload {
            Payload::ImportSection(reader) => {
                for import in reader {
                    let import = import.map_err(|error| {
                        failure(FailureKind::Validation, "parse module import", error)
                    })?;
                    if !matches!(import.ty, TypeRef::Func(_)) {
                        return Err(AttemptFailure::new(
                            FailureKind::Validation,
                            "only function imports from blit_v1 are permitted",
                        ));
                    }
                }
            }
            Payload::MemorySection(reader) => {
                memories = memories.checked_add(reader.count()).ok_or_else(|| {
                    AttemptFailure::new(FailureKind::Validation, "memory count overflow")
                })?;
                for memory in reader {
                    let memory = memory.map_err(|error| {
                        failure(FailureKind::Validation, "parse linear memory", error)
                    })?;
                    if memory.memory64 {
                        return Err(AttemptFailure::new(
                            FailureKind::Validation,
                            "64-bit linear memories are not permitted",
                        ));
                    }
                }
            }
            Payload::TableSection(reader) => {
                tables = tables.checked_add(reader.count()).ok_or_else(|| {
                    AttemptFailure::new(FailureKind::Validation, "table count overflow")
                })?;
            }
            Payload::StartSection { .. } => {
                return Err(AttemptFailure::new(
                    FailureKind::Validation,
                    "WebAssembly start functions are not permitted",
                ));
            }
            _ => {}
        }
    }
    if memories != 1 {
        return Err(AttemptFailure::new(
            FailureKind::Validation,
            format!("module must define exactly one linear memory; found {memories}"),
        ));
    }
    if tables > 1 {
        return Err(AttemptFailure::new(
            FailureKind::Validation,
            format!("module may define at most one table; found {tables}"),
        ));
    }
    Ok(())
}

fn validate_module_types(module: &Module) -> Result<(), AttemptFailure> {
    let mut seen = 0_u8;
    for import in module.imports() {
        if import.module() != "blit_v1" {
            return Err(AttemptFailure::new(
                FailureKind::Validation,
                format!(
                    "unsupported import module `{}` for `{}`",
                    import.module(),
                    import.name()
                ),
            ));
        }
        let Some(function) = import.ty().func() else {
            return Err(AttemptFailure::new(
                FailureKind::Validation,
                format!("non-function import `blit_v1.{}`", import.name()),
            ));
        };
        let (bit, valid) = match import.name() {
            "send" => (
                1,
                signature(
                    function.params(),
                    function.results(),
                    &[ValType::I32, ValType::I32],
                    &[ValType::I32],
                ),
            ),
            "recv" => (
                2,
                signature(
                    function.params(),
                    function.results(),
                    &[ValType::I32, ValType::I32],
                    &[ValType::I32],
                ),
            ),
            "wait" => (
                4,
                signature(
                    function.params(),
                    function.results(),
                    &[ValType::I64],
                    &[ValType::I32],
                ),
            ),
            "clock" => (
                8,
                signature(
                    function.params(),
                    function.results(),
                    &[ValType::I32],
                    &[ValType::I64],
                ),
            ),
            "random" => (
                16,
                signature(
                    function.params(),
                    function.results(),
                    &[ValType::I32, ValType::I32],
                    &[],
                ),
            ),
            other => {
                return Err(AttemptFailure::new(
                    FailureKind::Validation,
                    format!("unsupported import `blit_v1.{other}`"),
                ));
            }
        };
        if seen & bit != 0 {
            return Err(AttemptFailure::new(
                FailureKind::Validation,
                format!("duplicate import `blit_v1.{}`", import.name()),
            ));
        }
        seen |= bit;
        if !valid {
            return Err(AttemptFailure::new(
                FailureKind::Validation,
                format!("invalid signature for `blit_v1.{}`", import.name()),
            ));
        }
    }

    match module.get_export("memory") {
        Some(ExternType::Memory(memory)) if !memory.is_64() => {}
        Some(_) => {
            return Err(AttemptFailure::new(
                FailureKind::Validation,
                "export `memory` is not a 32-bit linear memory",
            ));
        }
        None => {
            return Err(AttemptFailure::new(
                FailureKind::Validation,
                "module does not export its linear memory as `memory`",
            ));
        }
    }
    match module.get_export("blit_main") {
        Some(ExternType::Func(function))
            if signature(function.params(), function.results(), &[], &[ValType::I32]) => {}
        Some(_) => {
            return Err(AttemptFailure::new(
                FailureKind::Validation,
                "export `blit_main` must have type () -> i32",
            ));
        }
        None => {
            return Err(AttemptFailure::new(
                FailureKind::Validation,
                "module does not export `blit_main`",
            ));
        }
    }
    Ok(())
}

fn signature(
    actual_params: &[ValType],
    actual_results: &[ValType],
    params: &[ValType],
    results: &[ValType],
) -> bool {
    actual_params == params && actual_results == results
}

fn define_host_abi(linker: &mut Linker<StoreData>) -> Result<(), wasmi::errors::LinkerError> {
    linker.func_wrap("blit_v1", "send", host_send)?;
    linker.func_wrap("blit_v1", "recv", host_recv)?;
    linker.func_wrap("blit_v1", "wait", host_wait)?;
    linker.func_wrap("blit_v1", "clock", host_clock)?;
    linker.func_wrap("blit_v1", "random", host_random)?;
    Ok(())
}

fn host_send(caller: Caller<'_, StoreData>, pointer: i32, length: i32) -> Result<i32, WasmiError> {
    let offset = parse_pointer(pointer)?;
    if length < 0 {
        return Err(abi_misuse("send length is negative"));
    }
    let length = usize::try_from(length).map_err(|_| abi_misuse("invalid send length"))?;
    let memory = exported_memory(&caller)?;
    checked_guest_range(&caller, memory, offset, length)?;
    if length == 0 || length > PACKET_MAX_BYTES {
        return Ok(-2);
    }
    let io = Arc::clone(&caller.data().io);
    if !io.bootstrap_complete.load(Ordering::Acquire) {
        return Err(abi_misuse("send called before EXT_INFO(INIT) was received"));
    }
    let Some(reservation) = io.outgoing.reserve_blocking() else {
        return Ok(-1);
    };
    let mut packet = vec![0_u8; length];
    memory
        .read(&caller, offset, &mut packet)
        .map_err(|error| abi_misuse(format!("invalid send memory range: {error}")))?;
    if reservation.commit(packet).is_err() {
        return Ok(-1);
    }
    Ok(0)
}

fn host_recv(
    mut caller: Caller<'_, StoreData>,
    pointer: i32,
    capacity: i32,
) -> Result<i32, WasmiError> {
    let offset = parse_pointer(pointer)?;
    if capacity < 0 {
        return Err(abi_misuse("recv capacity is negative"));
    }
    let capacity = usize::try_from(capacity).map_err(|_| abi_misuse("invalid recv capacity"))?;
    let memory = exported_memory(&caller)?;
    checked_guest_range(&caller, memory, offset, capacity)?;
    let io = Arc::clone(&caller.data().io);
    let incoming = Arc::clone(&io.incoming);
    let mut received_init = false;
    let result = incoming.copy_blocking(capacity, |packet| {
        received_init = packet.get(..2)
            == Some(
                &[
                    blit_remote::extension::EXT_INFO,
                    blit_remote::extension::EXT_INFO_INIT,
                ][..],
            );
        memory
            .write(&mut caller, offset, packet)
            .map_err(|error| abi_misuse(format!("invalid recv memory range: {error}")))
    })?;
    if received_init {
        io.bootstrap_complete.store(true, Ordering::Release);
    }
    Ok(match result {
        CopyResult::Closed => 0,
        CopyResult::Copied(length) | CopyResult::TooSmall(length) => length as i32,
    })
}

fn host_wait(caller: Caller<'_, StoreData>, deadline_ns: i64) -> i32 {
    match caller.data().io.wait_for_incoming(deadline_ns) {
        WaitResult::Deadline => 0,
        WaitResult::Packet => 1,
        WaitResult::Closed => 2,
    }
}

fn host_clock(caller: Caller<'_, StoreData>, kind: i32) -> Result<i64, WasmiError> {
    match kind {
        0 => Ok(realtime_ns()),
        1 => Ok(caller.data().io.monotonic_ns()),
        _ => Err(abi_misuse(format!("unsupported clock kind {kind}"))),
    }
}

fn host_random(
    mut caller: Caller<'_, StoreData>,
    pointer: i32,
    length: i32,
) -> Result<(), WasmiError> {
    let offset = parse_pointer(pointer)?;
    if length < 0 {
        return Err(abi_misuse("random length is negative"));
    }
    let length = usize::try_from(length).map_err(|_| abi_misuse("invalid random length"))?;
    if length > RANDOM_MAX_BYTES {
        return Err(abi_misuse("random request exceeds 64 KiB"));
    }
    let memory = exported_memory(&caller)?;
    let range = checked_guest_range(&caller, memory, offset, length)?;
    if range.is_empty() {
        return Ok(());
    }
    let bytes = memory
        .data_mut(&mut caller)
        .get_mut(range)
        .ok_or_else(|| abi_misuse("invalid random memory range"))?;
    getrandom::fill(bytes)
        .map_err(|error| host_failure(format!("operating-system entropy failed: {error}")))
}

fn parse_pointer(pointer: i32) -> Result<usize, WasmiError> {
    usize::try_from(pointer).map_err(|_| abi_misuse("guest pointer is negative"))
}

fn exported_memory(caller: &Caller<'_, StoreData>) -> Result<Memory, WasmiError> {
    caller
        .get_export("memory")
        .and_then(|export| export.into_memory())
        .ok_or_else(|| host_failure("validated instance lost exported memory"))
}

fn checked_guest_range(
    caller: &Caller<'_, StoreData>,
    memory: Memory,
    offset: usize,
    length: usize,
) -> Result<Range<usize>, WasmiError> {
    let end = offset
        .checked_add(length)
        .ok_or_else(|| abi_misuse("guest memory range overflows"))?;
    if end > memory.data_size(caller) {
        return Err(abi_misuse("guest memory range is out of bounds"));
    }
    Ok(offset..end)
}

#[derive(Clone, Debug)]
struct HostTrap {
    kind: FailureKind,
    detail: String,
}

impl fmt::Display for HostTrap {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.detail)
    }
}

impl std::error::Error for HostTrap {}

impl wasmi::errors::HostError for HostTrap {}

fn abi_misuse(detail: impl Into<String>) -> WasmiError {
    WasmiError::host(HostTrap {
        kind: FailureKind::AbiMisuse,
        detail: detail.into(),
    })
}

fn host_failure(detail: impl Into<String>) -> WasmiError {
    WasmiError::host(HostTrap {
        kind: FailureKind::HostFailure,
        detail: detail.into(),
    })
}

fn classify_execution_error(io: &HostIo, error: &WasmiError) -> AttemptOutcome {
    if io.cancelled.load(Ordering::Acquire) {
        return AttemptOutcome::Cancelled;
    }
    if let Some(host) = error.downcast_ref::<HostTrap>() {
        return AttemptOutcome::Failed(AttemptFailure::new(host.kind, host.detail.clone()));
    }
    AttemptOutcome::Failed(failure(FailureKind::Trap, "guest trapped", error))
}

fn failure(kind: FailureKind, context: &str, error: impl fmt::Display) -> AttemptFailure {
    AttemptFailure::new(kind, format!("{context}: {error}"))
}

#[derive(Debug)]
struct AttemptShared {
    io: Arc<HostIo>,
    start: Mutex<bool>,
    start_cv: Condvar,
}

#[derive(Debug)]
struct HostIo {
    incoming: Arc<PacketHandoff>,
    outgoing: Arc<PacketHandoff>,
    cancelled: AtomicBool,
    bootstrap_complete: AtomicBool,
    monotonic_origin: Instant,
}

impl HostIo {
    fn new() -> Self {
        Self {
            incoming: Arc::new(PacketHandoff::new()),
            outgoing: Arc::new(PacketHandoff::new()),
            cancelled: AtomicBool::new(false),
            bootstrap_complete: AtomicBool::new(false),
            monotonic_origin: Instant::now(),
        }
    }

    fn abort_handoffs(&self) {
        self.incoming.abort();
        self.outgoing.abort();
    }

    fn monotonic_ns(&self) -> i64 {
        duration_to_i64_ns(self.monotonic_origin.elapsed())
    }

    fn wait_for_incoming(&self, deadline_ns: i64) -> WaitResult {
        let mut state = lock_unpoison(&self.incoming.state);
        loop {
            if state.packet.is_some() {
                return WaitResult::Packet;
            }
            let now = self.monotonic_ns();
            if deadline_ns <= now {
                return WaitResult::Deadline;
            }
            if state.producer_closed || state.consumer_closed {
                return WaitResult::Closed;
            }
            if deadline_ns == i64::MAX {
                state = wait_unpoison(&self.incoming.ready_cv, state);
            } else {
                let timeout = Duration::from_nanos((deadline_ns - now) as u64);
                state = wait_timeout_unpoison(&self.incoming.ready_cv, state, timeout);
            }
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum WaitResult {
    Deadline,
    Packet,
    Closed,
}

#[derive(Debug)]
struct PacketHandoff {
    state: Mutex<SlotState>,
    ready_cv: Condvar,
    available_cv: Condvar,
    ready_async: Notify,
    available_async: Notify,
}

#[derive(Debug, Default)]
struct SlotState {
    packet: Option<Vec<u8>>,
    reserved: bool,
    producer_closed: bool,
    consumer_closed: bool,
}

impl PacketHandoff {
    fn new() -> Self {
        Self {
            state: Mutex::new(SlotState::default()),
            ready_cv: Condvar::new(),
            available_cv: Condvar::new(),
            ready_async: Notify::new(),
            available_async: Notify::new(),
        }
    }

    fn reserve_blocking(self: &Arc<Self>) -> Option<SlotReservation> {
        let mut state = lock_unpoison(&self.state);
        while state.reserved && !state.producer_closed && !state.consumer_closed {
            state = wait_unpoison(&self.available_cv, state);
        }
        if state.producer_closed || state.consumer_closed {
            return None;
        }
        state.reserved = true;
        Some(SlotReservation {
            handoff: Arc::clone(self),
            committed: false,
        })
    }

    async fn reserve_async(self: &Arc<Self>) -> Result<SlotReservation, PacketSendError> {
        loop {
            let notified = self.available_async.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            {
                let mut state = lock_unpoison(&self.state);
                if state.producer_closed || state.consumer_closed {
                    return Err(PacketSendError::Closed);
                }
                if !state.reserved {
                    state.reserved = true;
                    return Ok(SlotReservation {
                        handoff: Arc::clone(self),
                        committed: false,
                    });
                }
            }
            notified.await;
        }
    }

    async fn take_async(self: &Arc<Self>) -> Option<PacketLease> {
        loop {
            let notified = self.ready_async.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            {
                let mut state = lock_unpoison(&self.state);
                if let Some(packet) = state.packet.take() {
                    return Some(PacketLease {
                        handoff: Arc::clone(self),
                        packet: Some(packet),
                    });
                }
                if state.producer_closed || state.consumer_closed {
                    return None;
                }
            }
            notified.await;
        }
    }

    fn copy_blocking<E>(
        &self,
        capacity: usize,
        copy: impl FnOnce(&[u8]) -> Result<(), E>,
    ) -> Result<CopyResult, E> {
        let mut state = lock_unpoison(&self.state);
        loop {
            if let Some(packet) = state.packet.as_deref() {
                let length = packet.len();
                if length > capacity {
                    return Ok(CopyResult::TooSmall(length));
                }
                copy(packet)?;
                state.packet.take();
                state.reserved = false;
                drop(state);
                self.notify_available();
                return Ok(CopyResult::Copied(length));
            }
            if state.producer_closed || state.consumer_closed {
                return Ok(CopyResult::Closed);
            }
            state = wait_unpoison(&self.ready_cv, state);
        }
    }

    fn release_reservation(&self) {
        let mut state = lock_unpoison(&self.state);
        if state.reserved {
            state.reserved = false;
            state.packet.take();
            drop(state);
            self.notify_available();
        }
    }

    fn seal_producer(&self) {
        let mut state = lock_unpoison(&self.state);
        state.producer_closed = true;
        drop(state);
        self.notify_ready();
        self.notify_available();
    }

    fn close_consumer(&self) {
        let mut state = lock_unpoison(&self.state);
        state.consumer_closed = true;
        state.packet.take();
        state.reserved = false;
        drop(state);
        self.notify_ready();
        self.notify_available();
    }

    fn abort(&self) {
        let mut state = lock_unpoison(&self.state);
        state.producer_closed = true;
        state.consumer_closed = true;
        state.packet.take();
        state.reserved = false;
        drop(state);
        self.notify_ready();
        self.notify_available();
    }

    fn notify_ready(&self) {
        self.ready_cv.notify_all();
        self.ready_async.notify_waiters();
    }

    fn notify_available(&self) {
        self.available_cv.notify_all();
        self.available_async.notify_waiters();
    }
}

#[derive(Debug)]
struct SlotReservation {
    handoff: Arc<PacketHandoff>,
    committed: bool,
}

impl SlotReservation {
    fn commit(mut self, packet: Vec<u8>) -> Result<(), PacketSendError> {
        let mut state = lock_unpoison(&self.handoff.state);
        if state.producer_closed || state.consumer_closed {
            state.reserved = false;
            drop(state);
            self.handoff.notify_available();
            self.committed = true;
            return Err(PacketSendError::Closed);
        }
        state.packet = Some(packet);
        drop(state);
        self.handoff.notify_ready();
        self.committed = true;
        Ok(())
    }
}

impl Drop for SlotReservation {
    fn drop(&mut self) {
        if !self.committed {
            self.handoff.release_reservation();
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CopyResult {
    Closed,
    TooSmall(usize),
    Copied(usize),
}

fn validate_packet_len(packet_len: usize) -> Result<(), PacketSendError> {
    if packet_len == 0 {
        return Err(PacketSendError::Empty);
    }
    if packet_len > PACKET_MAX_BYTES {
        return Err(PacketSendError::TooLarge);
    }
    Ok(())
}

fn realtime_ns() -> i64 {
    match SystemTime::now().duration_since(UNIX_EPOCH) {
        Ok(duration) => duration_to_i64_ns(duration),
        Err(error) => -duration_to_i64_ns(error.duration()),
    }
}

fn duration_to_i64_ns(duration: Duration) -> i64 {
    i64::try_from(duration.as_nanos()).unwrap_or(i64::MAX)
}

fn lock_unpoison<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn wait_unpoison<'a, T>(condvar: &Condvar, guard: MutexGuard<'a, T>) -> MutexGuard<'a, T> {
    condvar
        .wait(guard)
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn wait_timeout_unpoison<'a, T>(
    condvar: &Condvar,
    guard: MutexGuard<'a, T>,
    timeout: Duration,
) -> MutexGuard<'a, T> {
    condvar
        .wait_timeout(guard, timeout)
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .0
}

#[cfg(any(target_os = "android", target_os = "linux"))]
fn lower_current_thread_priority() {
    // Linux nice values are per-thread. `who = 0` targets only the calling
    // thread, so this never changes the server process as a whole.
    unsafe {
        let _ = libc::setpriority(libc::PRIO_PROCESS, 0, 5);
    }
}

#[cfg(target_os = "windows")]
fn lower_current_thread_priority() {
    use windows_sys::Win32::System::Threading::{
        GetCurrentThread, SetThreadPriority, THREAD_PRIORITY_BELOW_NORMAL,
    };
    unsafe {
        let _ = SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_BELOW_NORMAL);
    }
}

#[cfg(not(any(target_os = "android", target_os = "linux", target_os = "windows")))]
fn lower_current_thread_priority() {}

#[cfg(test)]
mod tests {
    use super::*;

    const HASH: [u8; 32] = [0x42; 32];

    fn wasm(source: &str) -> Arc<[u8]> {
        wat::parse_str(source).unwrap().into()
    }

    fn spec(source: &str) -> AttemptSpec {
        AttemptSpec {
            module: wasm(source),
            module_hash: HASH,
            extension_id: 0x1234_7f2a,
            label: Some("Builder".to_owned()),
            config: WasmiHostConfig::default(),
        }
    }

    async fn prepared(source: &str) -> WasmiAttempt {
        let mut attempt = spawn_attempt(spec(source)).unwrap();
        attempt.wait_prepared().await.unwrap();
        attempt.bridge().mark_bootstrap_complete();
        attempt
    }

    async fn send_to_guest(bridge: &HostBridge, packet: Vec<u8>) {
        let packet_len = packet.len();
        bridge
            .reserve_to_guest(packet_len)
            .await
            .unwrap()
            .commit(packet)
            .unwrap();
    }

    #[test]
    fn validates_minimal_module_and_strict_shape() {
        let valid = wasm(
            r#"(module
                (memory (export "memory") 1)
                (func (export "blit_main") (result i32) i32.const 7)
            )"#,
        );
        validate_module(&valid, &WasmiHostConfig::default()).unwrap();

        let missing_memory = wasm(
            r#"(module
                (func (export "blit_main") (result i32) i32.const 0)
            )"#,
        );
        assert_eq!(
            validate_module(&missing_memory, &WasmiHostConfig::default())
                .unwrap_err()
                .kind,
            FailureKind::Validation
        );

        let start = wasm(
            r#"(module
                (memory (export "memory") 1)
                (func $start)
                (start $start)
                (func (export "blit_main") (result i32) i32.const 0)
            )"#,
        );
        let error = validate_module(&start, &WasmiHostConfig::default()).unwrap_err();
        assert_eq!(error.kind, FailureKind::Validation);
        assert!(error.detail.contains("start"));
    }

    #[test]
    fn rejects_unknown_duplicate_and_mistyped_imports() {
        for source in [
            r#"(module
                (import "env" "send" (func (param i32 i32) (result i32)))
                (memory (export "memory") 1)
                (func (export "blit_main") (result i32) i32.const 0)
            )"#,
            r#"(module
                (import "blit_v1" "send" (func (param i32) (result i32)))
                (memory (export "memory") 1)
                (func (export "blit_main") (result i32) i32.const 0)
            )"#,
            r#"(module
                (import "blit_v1" "send" (func (param i32 i32) (result i32)))
                (import "blit_v1" "send" (func (param i32 i32) (result i32)))
                (memory (export "memory") 1)
                (func (export "blit_main") (result i32) i32.const 0)
            )"#,
        ] {
            let error = validate_module(&wasm(source), &WasmiHostConfig::default()).unwrap_err();
            assert_eq!(error.kind, FailureKind::Validation);
        }
    }

    #[tokio::test]
    async fn send_holds_slot_until_adapter_acknowledges() {
        let mut attempt = prepared(
            r#"(module
                (import "blit_v1" "send" (func $send (param i32 i32) (result i32)))
                (memory (export "memory") 1)
                (data (i32.const 0) "\01\02")
                (func (export "blit_main") (result i32)
                    i32.const 0 i32.const 1 call $send drop
                    i32.const 1 i32.const 1 call $send drop
                    i32.const 7)
            )"#,
        )
        .await;
        let bridge = attempt.bridge();
        attempt.start().unwrap();

        let first = bridge.recv_from_guest().await.unwrap();
        assert_eq!(first.packet(), &[1]);
        assert!(
            tokio::time::timeout(Duration::from_millis(25), bridge.recv_from_guest())
                .await
                .is_err()
        );
        first.acknowledge();

        let second = bridge.recv_from_guest().await.unwrap();
        assert_eq!(second.packet(), &[2]);
        second.acknowledge();
        assert_eq!(attempt.join().await.unwrap(), AttemptOutcome::Returned(7));
    }

    #[tokio::test]
    async fn normal_return_preserves_last_accepted_packet_for_drain() {
        let mut attempt = prepared(
            r#"(module
                (import "blit_v1" "send" (func $send (param i32 i32) (result i32)))
                (memory (export "memory") 1)
                (data (i32.const 0) "\7f")
                (func (export "blit_main") (result i32)
                    i32.const 0 i32.const 1 call $send drop
                    i32.const 9)
            )"#,
        )
        .await;
        let bridge = attempt.bridge();
        attempt.start().unwrap();
        assert_eq!(attempt.join().await.unwrap(), AttemptOutcome::Returned(9));
        let packet = bridge.recv_from_guest().await.unwrap();
        assert_eq!(packet.packet(), &[0x7f]);
        packet.acknowledge();
        assert!(bridge.recv_from_guest().await.is_none());
    }

    #[tokio::test]
    async fn send_reports_size_and_closed_conditions() {
        let mut too_large = prepared(
            r#"(module
                (import "blit_v1" "send" (func $send (param i32 i32) (result i32)))
                (memory (export "memory") 257)
                (func (export "blit_main") (result i32)
                    i32.const 0 i32.const 16777217 call $send)
            )"#,
        )
        .await;
        too_large.start().unwrap();
        assert_eq!(
            too_large.join().await.unwrap(),
            AttemptOutcome::Returned(-2)
        );

        let mut closed = prepared(
            r#"(module
                (import "blit_v1" "send" (func $send (param i32 i32) (result i32)))
                (memory (export "memory") 1)
                (func (export "blit_main") (result i32)
                    i32.const 0 i32.const 1 call $send)
            )"#,
        )
        .await;
        closed.bridge().close_from_guest();
        closed.start().unwrap();
        assert_eq!(closed.join().await.unwrap(), AttemptOutcome::Returned(-1));
    }

    #[tokio::test]
    async fn recv_retains_oversize_packet_then_copies_it() {
        let mut attempt = prepared(
            r#"(module
                (import "blit_v1" "recv" (func $recv (param i32 i32) (result i32)))
                (memory (export "memory") 1)
                (func (export "blit_main") (result i32)
                    i32.const 0 i32.const 1 call $recv drop
                    i32.const 0 i32.const 3 call $recv drop
                    i32.const 0 i32.load8_u)
            )"#,
        )
        .await;
        let bridge = attempt.bridge();
        attempt.start().unwrap();
        send_to_guest(&bridge, vec![42, 8, 9]).await;
        assert_eq!(attempt.join().await.unwrap(), AttemptOutcome::Returned(42));
    }

    #[tokio::test]
    async fn recv_reports_orderly_endpoint_close() {
        let mut attempt = prepared(
            r#"(module
                (import "blit_v1" "recv" (func $recv (param i32 i32) (result i32)))
                (memory (export "memory") 1)
                (func (export "blit_main") (result i32)
                    i32.const 0 i32.const 1 call $recv)
            )"#,
        )
        .await;
        let bridge = attempt.bridge();
        attempt.start().unwrap();
        bridge.close_to_guest();
        assert_eq!(attempt.join().await.unwrap(), AttemptOutcome::Returned(0));
    }

    #[tokio::test]
    async fn wait_observes_packet_before_deadline_or_closure() {
        let source = r#"(module
            (import "blit_v1" "wait" (func $wait (param i64) (result i32)))
            (memory (export "memory") 1)
            (func (export "blit_main") (result i32)
                i64.const 9223372036854775807 call $wait)
        )"#;
        let mut packet = prepared(source).await;
        let bridge = packet.bridge();
        packet.start().unwrap();
        send_to_guest(&bridge, vec![1]).await;
        assert_eq!(packet.join().await.unwrap(), AttemptOutcome::Returned(1));

        let mut closed = prepared(source).await;
        let bridge = closed.bridge();
        closed.start().unwrap();
        bridge.close_to_guest();
        assert_eq!(closed.join().await.unwrap(), AttemptOutcome::Returned(2));
    }

    #[tokio::test]
    async fn clocks_and_entropy_are_direct_host_calls() {
        let mut attempt = prepared(
            r#"(module
                (import "blit_v1" "clock" (func $clock (param i32) (result i64)))
                (import "blit_v1" "random" (func $random (param i32 i32)))
                (memory (export "memory") 1)
                (func (export "blit_main") (result i32)
                    i32.const 0 i32.const 8 call $random
                    i32.const 0 i64.load i64.eqz i32.eqz
                    i32.const 0 call $clock i64.const 0 i64.gt_s
                    i32.and)
            )"#,
        )
        .await;
        attempt.start().unwrap();
        assert_eq!(attempt.join().await.unwrap(), AttemptOutcome::Returned(1));
    }

    #[tokio::test]
    async fn invalid_host_call_is_structured_abi_failure() {
        let mut attempt = prepared(
            r#"(module
                (import "blit_v1" "clock" (func $clock (param i32) (result i64)))
                (memory (export "memory") 1)
                (func (export "blit_main") (result i32)
                    i32.const 2 call $clock drop i32.const 0)
            )"#,
        )
        .await;
        attempt.start().unwrap();
        let AttemptOutcome::Failed(error) = attempt.join().await.unwrap() else {
            panic!("expected ABI failure")
        };
        assert_eq!(error.kind, FailureKind::AbiMisuse);
        assert!(error.detail.contains("clock kind"));
    }

    #[tokio::test]
    async fn send_before_receiving_init_is_an_abi_failure() {
        let mut attempt = spawn_attempt(spec(
            r#"(module
                (import "blit_v1" "send" (func $send (param i32 i32) (result i32)))
                (memory (export "memory") 1)
                (data (i32.const 0) "\01")
                (func (export "blit_main") (result i32)
                    i32.const 0 i32.const 1 call $send)
            )"#,
        ))
        .unwrap();
        attempt.wait_prepared().await.unwrap();
        attempt.start().unwrap();
        let AttemptOutcome::Failed(error) = attempt.join().await.unwrap() else {
            panic!("expected pre-INIT ABI failure")
        };
        assert_eq!(error.kind, FailureKind::AbiMisuse);
        assert!(error.detail.contains("before EXT_INFO(INIT)"));
    }

    #[tokio::test]
    async fn fuel_slices_make_compute_loop_cancellable() {
        let mut attempt = prepared(
            r#"(module
                (memory (export "memory") 1)
                (func (export "blit_main") (result i32)
                    (loop $spin br $spin)
                    unreachable)
            )"#,
        )
        .await;
        attempt.start().unwrap();
        tokio::time::sleep(Duration::from_millis(5)).await;
        attempt.cancel();
        let outcome = tokio::time::timeout(Duration::from_secs(2), attempt.join())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(outcome, AttemptOutcome::Cancelled);
    }

    #[tokio::test]
    async fn cancellation_wakes_blocked_recv_and_wins_return_race() {
        let mut attempt = prepared(
            r#"(module
                (import "blit_v1" "recv" (func $recv (param i32 i32) (result i32)))
                (memory (export "memory") 1)
                (func (export "blit_main") (result i32)
                    i32.const 0 i32.const 1 call $recv)
            )"#,
        )
        .await;
        attempt.start().unwrap();
        tokio::time::sleep(Duration::from_millis(5)).await;
        attempt.cancel();
        assert_eq!(attempt.join().await.unwrap(), AttemptOutcome::Cancelled);
    }

    #[tokio::test]
    async fn store_limits_reject_large_initial_memory_before_start() {
        let mut attempt_spec = spec(
            r#"(module
                (memory (export "memory") 2)
                (func (export "blit_main") (result i32) i32.const 0)
            )"#,
        );
        attempt_spec.config.memory_bytes = 64 * 1024;
        let mut attempt = spawn_attempt(attempt_spec).unwrap();
        let error = attempt.wait_prepared().await.unwrap_err();
        assert_eq!(error.kind, FailureKind::Instantiation);
        assert!(matches!(
            attempt.join().await.unwrap(),
            AttemptOutcome::Failed(_)
        ));
    }

    #[tokio::test]
    async fn attempt_thread_uses_compacted_rfc_name() {
        let attempt = prepared(
            r#"(module
                (memory (export "memory") 1)
                (func (export "blit_main") (result i32) i32.const 0)
            )"#,
        )
        .await;
        assert_eq!(attempt.thread_names().logical, "blit-ext:builder#7f2a");
        assert!(attempt.thread_names().os.ends_with("7f2a"));
        attempt.cancel();
        assert_eq!(attempt.join().await.unwrap(), AttemptOutcome::Cancelled);
    }
}
