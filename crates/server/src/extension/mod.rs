//! Server-side extension service and supervisor.

mod command_directory;
pub(crate) mod quickjs_host;
pub(crate) mod wasmi_host;

use self::command_directory::{CommandDirectory, CommandListener, CommandOwner, DiscoveryPage};
use self::wasmi_host::{
    AttemptCancellation, AttemptFailure, AttemptOutcome, AttemptSpec as WasmiAttemptSpec,
    FailureKind, WasmiHostConfig,
};
use crate::extension_catalog::{
    BlockedState, CatalogError, ExtensionCatalog, PersistentDefinition,
};
use crate::extension_jobs::EndpointTracker;
use crate::extension_store::{
    BeginUpload, ChunkUploadCommit, ObjectHash, ObjectRead, ObjectStore, ObjectStoreConfig,
    ObjectStoreError, PreparedBeginUpload, PreparedPut, PutChunk, UploadCreationCommit,
};
use blit_remote::extension::{
    self as wire, EXT_CONTROL_ATTACH, EXT_CONTROL_CANCEL, EXT_CONTROL_DISABLE, EXT_CONTROL_ENABLE,
    EXT_CONTROL_LIST, EXT_CONTROL_REMOVE, EXT_CONTROL_RESTART, EXT_CONTROL_STATUS,
    EXT_CONTROL_UNFOLLOW, EXT_EXIT_CANCELLED, EXT_EXIT_HOST_FAILURE, EXT_EXIT_PROTOCOL_VIOLATION,
    EXT_EXIT_RESOURCE_LIMIT, EXT_EXIT_RETURNED, EXT_EXIT_SERVER_SHUTDOWN, EXT_EXIT_SLOW_CONSUMER,
    EXT_EXIT_TRAPPED, EXT_EXIT_UPDATED, EXT_FLAG_DESIRED_RUNNING, EXT_FLAG_DETACH,
    EXT_FLAG_ENABLED, EXT_FLAG_PERSIST, EXT_PHASE_BACKOFF, EXT_PHASE_BLOCKED,
    EXT_PHASE_NEED_OBJECT, EXT_PHASE_QUEUED, EXT_PHASE_RUNNING, EXT_PHASE_STOPPED,
    EXT_PHASE_STOPPING, EXT_PHASE_VALIDATING, EXT_PUT_BEGIN, EXT_PUT_FINAL, EXT_RESTART_ALWAYS,
    EXT_RESTART_ON_FAILURE, EXT_RUN_DETACH, EXT_RUN_PERSIST, EXT_RUN_UPDATE, EXT_STATUS_BUDGET,
    EXT_STATUS_CONFLICT, EXT_STATUS_INVALID, EXT_STATUS_NOT_FOUND, EXT_STATUS_OK, EXT_STATUS_OTHER,
    EXT_STATUS_PERMISSION, EXT_STATUS_TOO_LARGE, EXT_STATUS_UNKNOWN_ID, ExtensionExit,
    ExtensionInfoStatus, ExtensionOutputEvent, ExtensionPutStatus, ExtensionRecord,
    ExtensionRequest, ExtensionStatus,
};
use std::collections::{HashMap, HashSet, VecDeque};
use std::future::Future;
use std::pin::Pin;
use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicUsize, Ordering},
};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
#[cfg(test)]
use tokio::sync::mpsc;
use tokio::sync::{Mutex, Notify, OwnedSemaphorePermit, Semaphore, oneshot, watch};

const DEFAULT_MAX_TRANSIENT: usize = 128;
const DEFAULT_MAX_PERSISTENT: usize = 128;
const DEFAULT_FOLLOW_MAX_PER_ENDPOINT: usize = 128;
const DEFAULT_FOLLOW_MAX: usize = 4_096;
const DEFAULT_MAX_RUNNING: usize = 4;
const DEFAULT_MAX_VALIDATING: usize = 2;
const DEFAULT_ARGUMENT_STORE_MAX: usize = 256 * 1024 * 1024;
const DEFAULT_OUTPUT_RETAIN_MAX: usize = 64 * 1024 * 1024;
const OUTPUT_RETAIN_PER_EXTENSION: usize = 4 * 1024 * 1024;
const DEFAULT_PENDING_TIMEOUT: Duration = Duration::from_secs(5 * 60);
const DEFAULT_TERMINAL_RETAIN: Duration = Duration::from_secs(30);
const BACKOFF_BASE: Duration = Duration::from_millis(250);
const BACKOFF_MAX: Duration = Duration::from_secs(30);
const WASM_MAGIC: &[u8; 4] = b"\0asm";

fn is_wasm_module(object: &[u8]) -> bool {
    object.starts_with(WASM_MAGIC)
}

fn validate_extension_object(
    object: &[u8],
    config: &WasmiHostConfig,
) -> Result<(), AttemptFailure> {
    if is_wasm_module(object) {
        wasmi_host::validate_module(object, config)
    } else {
        quickjs_host::validate_source(object, config)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum DispatchOutcome {
    Continue,
    Close,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Interrupt {
    Cancelled,
    Updated,
    Restarted,
    Disabled,
    OwnerClosed,
    ServerShutdown,
}

enum ObjectProbe {
    Hit(ObjectRead),
    Miss,
    Durability(ObjectStoreError),
}

#[derive(Clone)]
struct AttemptControl {
    definition_revision: u64,
    attempt: u64,
    task_id: u32,
    host: AttemptCancellation,
    connection: super::ConnectionCancellation,
}

#[derive(Clone)]
struct RetainedRecord {
    sequence: u64,
    clock: u64,
    packet: Arc<RetainedPacket>,
}

#[derive(Debug)]
pub(crate) struct RetainedPacket {
    bytes: Vec<u8>,
    _reservation: Option<OutputReservation>,
}

impl std::ops::Deref for RetainedPacket {
    type Target = [u8];

    fn deref(&self) -> &Self::Target {
        &self.bytes
    }
}

#[derive(Clone, Copy)]
struct FollowerCursor {
    next_sequence: u64,
    replay_through: Option<u64>,
}

#[derive(Clone)]
struct Definition {
    extension_id: u64,
    definition_revision: u64,
    flags: u8,
    restart: u8,
    hash: ObjectHash,
    name: String,
    /// Arguments are resident only for transient definitions and uncommitted
    /// persistent creations. Committed persistent values stay in redb.
    args: Option<Vec<Vec<u8>>>,
    argument_bytes: usize,
    argument_reservation: Option<Arc<ArgumentReservation>>,
    owner_endpoint: Option<u64>,
    phase: u8,
    attempt: u64,
    last_running_attempt: u64,
    task_id: u32,
    next_start_unix_ms: u64,
    detail: String,
    next_output_sequence: u64,
    retained: VecDeque<RetainedRecord>,
    terminal_replay: VecDeque<RetainedRecord>,
    retained_bytes: usize,
    followers: HashMap<u64, FollowerCursor>,
    pending_deadline: Option<Instant>,
    release_deadline: Option<Instant>,
    generation: u64,
    failure_count: u32,
    interrupt: Option<Interrupt>,
    control: Option<AttemptControl>,
    object_pinned: bool,
    catalog_committed: bool,
    wake: Arc<Notify>,
}

impl Definition {
    fn persistent(&self) -> bool {
        self.flags & EXT_FLAG_PERSIST != 0
    }

    fn enabled(&self) -> bool {
        self.flags & EXT_FLAG_ENABLED != 0
    }

    fn desired(&self) -> bool {
        self.flags & EXT_FLAG_DESIRED_RUNNING != 0
    }

    fn latest_output_sequence(&self) -> u64 {
        self.next_output_sequence.saturating_sub(1)
    }

    fn set_flag(&mut self, bit: u8, value: bool) {
        if value {
            self.flags |= bit;
        } else {
            self.flags &= !bit;
        }
    }
}

struct ServiceState {
    store: Option<ObjectStore>,
    diagnostic: Option<String>,
    definitions: HashMap<u64, Definition>,
    endpoints: HashMap<u64, super::TrackedOutboxSender>,
    endpoint_wakes: HashMap<u64, Arc<Notify>>,
    supervisors: HashSet<u64>,
    supervisor_completions: HashMap<u64, Vec<Arc<SupervisorCompletion>>>,
    task_ids: HashSet<u32>,
    retained_bytes: usize,
    output_budget: Arc<OutputBudget>,
    retention_clock: u64,
    shutting_down: bool,
    commands: CommandDirectory,
}

#[derive(Debug)]
struct SupervisorCompletion {
    done: watch::Sender<bool>,
}

impl SupervisorCompletion {
    fn new() -> Arc<Self> {
        let (done, _) = watch::channel(false);
        Arc::new(Self { done })
    }

    fn complete(&self) {
        self.done.send_replace(true);
    }

    fn is_complete(&self) -> bool {
        *self.done.borrow()
    }

    async fn wait(&self) {
        let mut done = self.done.subscribe();
        while !*done.borrow_and_update() && done.changed().await.is_ok() {}
    }
}

struct SupervisorCompletionGuard(Arc<SupervisorCompletion>);

impl Drop for SupervisorCompletionGuard {
    fn drop(&mut self) {
        self.0.complete();
    }
}

/// Reader-allocated FIFO link for PUT jobs. Unlike semaphore admission, this
/// link is established synchronously in wire order before tasks are spawned,
/// so a non-validating middle chunk cannot overtake a BEGIN waiting for the
/// validation lane.
struct UploadOrder {
    previous: Option<Arc<SupervisorCompletion>>,
    current: Arc<SupervisorCompletion>,
}

impl UploadOrder {
    async fn wait(&self) {
        if let Some(previous) = &self.previous {
            previous.wait().await;
        }
    }
}

impl Drop for UploadOrder {
    fn drop(&mut self) {
        self.current.complete();
    }
}

#[derive(Debug)]
struct OutputBudget {
    max: usize,
    used: AtomicUsize,
}

impl OutputBudget {
    fn new(max: usize) -> Arc<Self> {
        Arc::new(Self {
            max,
            used: AtomicUsize::new(0),
        })
    }

    fn try_reserve(self: &Arc<Self>, bytes: usize) -> Option<OutputReservation> {
        let mut used = self.used.load(Ordering::Acquire);
        loop {
            let next = used.checked_add(bytes)?;
            if next > self.max {
                return None;
            }
            match self
                .used
                .compare_exchange_weak(used, next, Ordering::AcqRel, Ordering::Acquire)
            {
                Ok(_) => {
                    return Some(OutputReservation {
                        budget: Arc::clone(self),
                        bytes,
                    });
                }
                Err(actual) => used = actual,
            }
        }
    }

    #[cfg(test)]
    fn used(&self) -> usize {
        self.used.load(Ordering::Acquire)
    }
}

#[derive(Debug)]
struct OutputReservation {
    budget: Arc<OutputBudget>,
    bytes: usize,
}

impl Drop for OutputReservation {
    fn drop(&mut self) {
        let previous = self.budget.used.fetch_sub(self.bytes, Ordering::AcqRel);
        debug_assert!(previous >= self.bytes);
    }
}

struct ArgumentBudget {
    max: usize,
    used: AtomicUsize,
    notify: Notify,
    #[cfg(test)]
    contentions: AtomicUsize,
}

impl ArgumentBudget {
    fn new(max: usize) -> Arc<Self> {
        Arc::new(Self {
            max,
            used: AtomicUsize::new(0),
            notify: Notify::new(),
            #[cfg(test)]
            contentions: AtomicUsize::new(0),
        })
    }

    fn try_reserve(self: &Arc<Self>, bytes: usize) -> Option<Arc<ArgumentReservation>> {
        let mut used = self.used.load(Ordering::Acquire);
        loop {
            let next = used.checked_add(bytes)?;
            if next > self.max {
                #[cfg(test)]
                self.contentions.fetch_add(1, Ordering::Relaxed);
                return None;
            }
            match self
                .used
                .compare_exchange_weak(used, next, Ordering::AcqRel, Ordering::Acquire)
            {
                Ok(_) => {
                    return Some(Arc::new(ArgumentReservation {
                        budget: Arc::clone(self),
                        bytes,
                    }));
                }
                Err(actual) => used = actual,
            }
        }
    }

    #[cfg(test)]
    fn used(&self) -> usize {
        self.used.load(Ordering::Acquire)
    }

    #[cfg(test)]
    fn contentions(&self) -> usize {
        self.contentions.load(Ordering::Relaxed)
    }
}

struct ArgumentReservation {
    budget: Arc<ArgumentBudget>,
    bytes: usize,
}

impl Drop for ArgumentReservation {
    fn drop(&mut self) {
        let previous = self.budget.used.fetch_sub(self.bytes, Ordering::AcqRel);
        debug_assert!(previous >= self.bytes);
        self.budget.notify.notify_waiters();
        // Retain one permit for a waiter which observed contention just
        // before this release but has not registered its future yet.
        self.budget.notify.notify_one();
    }
}

#[cfg(test)]
type CatalogHook = Arc<std::sync::Mutex<Option<Arc<dyn Fn() + Send + Sync>>>>;

/// Process-global extension storage, lifecycle registry, and fair admission.
pub(crate) struct ExtensionService {
    enabled: bool,
    available: bool,
    persist_allowed: bool,
    max_transient: usize,
    max_persistent: usize,
    follow_max_per_endpoint: usize,
    follow_max: usize,
    argument_budget: Arc<ArgumentBudget>,
    /// Accounts request bytes retained by network-origin FINAL validation.
    /// Extension-origin requests are additionally covered by EndpointTracker.
    validation_request_budget: Arc<ArgumentBudget>,
    output_retain_max: usize,
    pending_timeout: Duration,
    terminal_retain: Duration,
    host_config: WasmiHostConfig,
    running: Arc<Semaphore>,
    validating: Arc<Semaphore>,
    /// Serializes state transitions which temporarily move an upload or LRU
    /// victim out of the object-store owner while filesystem work is detached.
    /// Control/status paths never acquire this mutex.
    store_io: Mutex<()>,
    /// Orders durable catalog operations while their redb I/O runs on the
    /// blocking pool. The service-state mutex is never held across this lane.
    catalog_io: Mutex<()>,
    catalog: Arc<std::sync::Mutex<Option<ExtensionCatalog>>>,
    upload_tails: std::sync::Mutex<HashMap<u64, Arc<SupervisorCompletion>>>,
    maintenance_started: AtomicBool,
    #[cfg(test)]
    validation_hook: std::sync::Mutex<Option<Arc<dyn Fn() + Send + Sync>>>,
    #[cfg(test)]
    storage_hook: std::sync::Mutex<Option<Arc<dyn Fn() + Send + Sync>>>,
    #[cfg(test)]
    catalog_hook: CatalogHook,
    inner: Mutex<ServiceState>,
}

impl ExtensionService {
    pub(crate) fn from_env(persist_allowed: bool, name: &crate::ServerName) -> Arc<Self> {
        let enabled = crate::extensions_enabled();
        let max_running =
            crate::deployment_usize("BLIT_EXT_MAX_RUNNING", host_running_default()).clamp(1, 4);
        let max_validating =
            crate::deployment_usize("BLIT_EXT_MAX_VALIDATING", DEFAULT_MAX_VALIDATING).max(1);
        let validation_request_max = usize::try_from(
            crate::deployment_u64("BLIT_EXT_MODULE_MAX", wire::EXT_MAX_MODULE)
                .min(wire::EXT_MAX_MODULE),
        )
        .unwrap_or(usize::MAX);
        let host_config = WasmiHostConfig {
            memory_bytes: crate::deployment_usize("BLIT_EXT_MEMORY_MAX", 128 * 1024 * 1024),
            table_elements: crate::deployment_usize("BLIT_EXT_TABLE_ELEMENTS_MAX", 65_536),
            value_stack_bytes: crate::deployment_usize("BLIT_EXT_VALUE_STACK_MAX", 128 * 1024),
            call_depth: crate::deployment_usize("BLIT_EXT_CALL_DEPTH_MAX", 1_024),
            native_stack_bytes: crate::deployment_usize("BLIT_EXT_STACK_SIZE", 2 * 1024 * 1024),
            fuel_slice: crate::deployment_u64("BLIT_EXT_FUEL_SLICE", 1_000_000),
        };

        let mut diagnostic = None;
        let mut store = None;
        let mut catalog = None;
        let mut definitions = HashMap::new();

        if enabled {
            let opened = ObjectStoreConfig::from_env(name)
                .ok_or_else(|| "extension cache directory is unavailable".to_owned())
                .and_then(|config| ObjectStore::open(config).map_err(|error| error.to_string()));
            match opened {
                Ok(mut opened_store) => match ExtensionCatalog::from_env(name) {
                    Ok(mut opened_catalog) => {
                        for persistent in opened_catalog.list() {
                            let mut definition = definition_from_persistent(persistent);
                            let object_block = if opened_store.pin(&definition.hash).is_ok() {
                                definition.object_pinned = true;
                                (!opened_store.is_usable(&definition.hash)).then_some(
                                    "persistent extension object exceeds the configured module limit",
                                )
                            } else {
                                Some("persistent extension object is absent from the cache")
                            };
                            if let Some(block_detail) = object_block {
                                definition.phase = EXT_PHASE_BLOCKED;
                                definition.detail = block_detail.into();
                                if let Err(error) = opened_catalog.set_lifecycle(
                                    definition.extension_id,
                                    None,
                                    None,
                                    None,
                                    None,
                                    None,
                                    Some(0),
                                    Some(BlockedState::Set(&definition.detail)),
                                ) {
                                    diagnostic = Some(error.to_string());
                                }
                            }
                            if !persist_allowed && definition.desired() && definition.enabled() {
                                definition.phase = EXT_PHASE_BLOCKED;
                                definition.detail =
                                    "persistent extensions are disabled on this server".into();
                            }
                            if definition.phase != EXT_PHASE_BACKOFF {
                                definition.next_start_unix_ms = 0;
                            }
                            definitions.insert(definition.extension_id, definition);
                        }
                        let list_fits = definitions.len() <= u16::MAX as usize && {
                            let records = definitions
                                .values()
                                .map(extension_record)
                                .collect::<Vec<_>>();
                            wire::msg_extension_list(0, EXT_STATUS_OK, &records).is_some()
                        };
                        if !list_fits {
                            diagnostic = Some(
                                "persistent extension catalog exceeds the wire list ceiling".into(),
                            );
                        } else if let Err(error) = opened_store.finish_startup_gc() {
                            diagnostic = Some(error.to_string());
                        } else if host_config.validate().is_err() {
                            diagnostic =
                                Some("invalid extension runtime containment limits".into());
                        } else {
                            store = Some(opened_store);
                            catalog = Some(opened_catalog);
                        }
                    }
                    Err(error) => diagnostic = Some(error.to_string()),
                },
                Err(error) => diagnostic = Some(error),
            }
        }

        let configured_max_transient =
            crate::deployment_usize("BLIT_EXT_MAX_TRANSIENT", DEFAULT_MAX_TRANSIENT);
        let configured_max_persistent =
            crate::deployment_usize("BLIT_EXT_MAX_PERSISTENT", DEFAULT_MAX_PERSISTENT);
        if configured_max_transient.saturating_add(configured_max_persistent) > u16::MAX as usize {
            diagnostic.get_or_insert_with(|| {
                "extension persistent and transient caps exceed the wire list ceiling".into()
            });
        }
        let effective_max_transient =
            configured_max_transient.min((u16::MAX as usize).saturating_sub(definitions.len()));
        let output_retain_max =
            crate::deployment_usize("BLIT_EXT_OUTPUT_RETAIN_MAX", DEFAULT_OUTPUT_RETAIN_MAX);

        if let Some(detail) = diagnostic.as_deref() {
            eprintln!("blit-server: extension subsystem disabled: {detail}");
        }

        let available = enabled && store.is_some() && catalog.is_some() && diagnostic.is_none();
        Arc::new(Self {
            enabled,
            available,
            persist_allowed,
            max_transient: effective_max_transient,
            max_persistent: configured_max_persistent,
            follow_max_per_endpoint: crate::deployment_usize(
                "BLIT_EXT_FOLLOW_MAX_PER_CLIENT",
                DEFAULT_FOLLOW_MAX_PER_ENDPOINT,
            ),
            follow_max: crate::deployment_usize("BLIT_EXT_FOLLOW_MAX", DEFAULT_FOLLOW_MAX),
            argument_budget: ArgumentBudget::new(crate::deployment_usize(
                "BLIT_EXT_ARGUMENT_STORE_MAX",
                DEFAULT_ARGUMENT_STORE_MAX,
            )),
            validation_request_budget: ArgumentBudget::new(validation_request_max),
            output_retain_max,
            pending_timeout: Duration::from_secs(crate::deployment_u64(
                "BLIT_EXT_PENDING_TIMEOUT",
                DEFAULT_PENDING_TIMEOUT.as_secs(),
            )),
            terminal_retain: Duration::from_secs(
                crate::deployment_u64(
                    "BLIT_EXT_TERMINAL_RETAIN",
                    DEFAULT_TERMINAL_RETAIN.as_secs(),
                )
                .max(1),
            ),
            host_config,
            running: Arc::new(Semaphore::new(max_running)),
            validating: Arc::new(Semaphore::new(max_validating)),
            store_io: Mutex::new(()),
            catalog_io: Mutex::new(()),
            catalog: Arc::new(std::sync::Mutex::new(catalog)),
            upload_tails: std::sync::Mutex::new(HashMap::new()),
            maintenance_started: AtomicBool::new(false),
            #[cfg(test)]
            validation_hook: std::sync::Mutex::new(None),
            #[cfg(test)]
            storage_hook: std::sync::Mutex::new(None),
            #[cfg(test)]
            catalog_hook: Arc::new(std::sync::Mutex::new(None)),
            inner: Mutex::new(ServiceState {
                store,
                diagnostic,
                definitions,
                endpoints: HashMap::new(),
                endpoint_wakes: HashMap::new(),
                supervisors: HashSet::new(),
                supervisor_completions: HashMap::new(),
                task_ids: HashSet::new(),
                retained_bytes: 0,
                output_budget: OutputBudget::new(output_retain_max),
                retention_clock: 0,
                shutting_down: false,
                commands: CommandDirectory::default(),
            }),
        })
    }

    pub(crate) fn advertised(&self) -> bool {
        self.enabled && self.available
    }

    async fn catalog_call<R, F>(&self, operation: F) -> Result<R, CatalogError>
    where
        R: Send + 'static,
        F: FnOnce(&mut ExtensionCatalog) -> Result<R, CatalogError> + Send + 'static,
    {
        let catalog = Arc::clone(&self.catalog);
        #[cfg(test)]
        let catalog_hook = Arc::clone(&self.catalog_hook);
        tokio::task::spawn_blocking(move || {
            #[cfg(test)]
            if let Some(hook) = catalog_hook.lock().expect("catalog hook lock").clone() {
                hook();
            }
            let mut catalog = catalog
                .lock()
                .map_err(|_| CatalogError::Storage("extension catalog lock poisoned".into()))?;
            operation(catalog.as_mut().ok_or(CatalogError::Unavailable)?)
        })
        .await
        .map_err(|error| {
            CatalogError::Storage(format!("extension catalog worker failed: {error}"))
        })?
    }

    async fn definition_arguments(
        &self,
        definition: &Definition,
    ) -> Result<Vec<Vec<u8>>, CatalogError> {
        if let Some(arguments) = &definition.args {
            return Ok(arguments.clone());
        }
        let extension_id = definition.extension_id;
        let definition_revision = definition.definition_revision;
        let expected_bytes = definition.argument_bytes;
        let arguments = self
            .catalog_call(move |catalog| catalog.load_arguments(extension_id, definition_revision))
            .await?
            .into_iter()
            .map(String::into_bytes)
            .collect::<Vec<_>>();
        if encoded_argument_bytes(&arguments) != expected_bytes {
            return Err(CatalogError::Storage(
                "persistent extension argument metadata changed".into(),
            ));
        }
        Ok(arguments)
    }

    async fn commit_catalog_create(
        &self,
        definition: &Definition,
    ) -> Result<PersistentDefinition, (u8, String)> {
        let arguments = definition
            .args
            .as_ref()
            .ok_or((
                EXT_STATUS_OTHER,
                "pending extension arguments are unavailable".into(),
            ))?
            .iter()
            .map(|argument| {
                std::str::from_utf8(argument)
                    .map(str::to_owned)
                    .map_err(|_| {
                        (
                            EXT_STATUS_INVALID,
                            "persistent arguments must be UTF-8".into(),
                        )
                    })
            })
            .collect::<Result<Vec<_>, _>>()?;
        let extension_id = definition.extension_id;
        let hash = definition.hash;
        let name = definition.name.clone();
        let restart = definition.restart;
        {
            let mut inner = self.inner.lock().await;
            inner
                .store
                .as_mut()
                .ok_or((EXT_STATUS_OTHER, "object store is unavailable".into()))?
                .pin(&hash)
                .map_err(|error| (object_status(&error), error.to_string()))?;
        }
        let committed = self
            .catalog_call(move |catalog| {
                catalog.create_with_id(extension_id, hash, name, arguments, restart)
            })
            .await;
        if let Err(error) = committed {
            let mut inner = self.inner.lock().await;
            if let Some(store) = inner.store.as_mut() {
                store.unpin(&hash);
            }
            return Err((catalog_status(&error), error.to_string()));
        }
        Ok(committed.expect("checked catalog create result"))
    }

    async fn commit_catalog_update(
        &self,
        current: &Definition,
        hash: ObjectHash,
        args: Vec<Vec<u8>>,
        restart: u8,
    ) -> Result<PersistentDefinition, CatalogError> {
        let changed_hash = hash != current.hash;
        let acquired_pin = changed_hash || !current.object_pinned;
        let arguments = args
            .into_iter()
            .map(|argument| {
                String::from_utf8(argument)
                    .map_err(|_| CatalogError::Invalid("persistent arguments must be UTF-8"))
            })
            .collect::<Result<Vec<_>, _>>()?;
        if acquired_pin {
            let mut inner = self.inner.lock().await;
            inner
                .store
                .as_mut()
                .ok_or(CatalogError::Unavailable)?
                .pin(&hash)
                .map_err(|error| CatalogError::Storage(error.to_string()))?;
        }
        let extension_id = current.extension_id;
        let definition_revision = current.definition_revision;
        let name = current.name.clone();
        let updated = self
            .catalog_call(move |catalog| {
                catalog.update(
                    extension_id,
                    definition_revision,
                    &name,
                    hash,
                    arguments,
                    restart,
                )
            })
            .await;
        let mut inner = self.inner.lock().await;
        match updated {
            Ok(updated) => {
                inner
                    .commands
                    .invalidate_definition(current.extension_id, current.definition_revision);
                if changed_hash
                    && current.object_pinned
                    && let Some(store) = inner.store.as_mut()
                {
                    store.unpin(&current.hash);
                }
                Ok(updated)
            }
            Err(error) => {
                if acquired_pin && let Some(store) = inner.store.as_mut() {
                    store.unpin(&hash);
                }
                Err(error)
            }
        }
    }

    async fn persist_attempt_counters_catalog(
        &self,
        extension_id: u64,
        attempt: u64,
        last_running: u64,
        persistent: bool,
    ) -> Result<(), CatalogError> {
        if !persistent {
            return Ok(());
        }
        self.catalog_call(move |catalog| {
            catalog
                .set_lifecycle(
                    extension_id,
                    None,
                    None,
                    Some(attempt),
                    Some(last_running),
                    None,
                    None,
                    None,
                )
                .map(|_| ())
        })
        .await
    }

    async fn persist_terminal_catalog(&self, definition: &Definition) -> Result<(), CatalogError> {
        if !definition.persistent() {
            return Ok(());
        }
        let extension_id = definition.extension_id;
        let enabled = definition.enabled();
        let desired = definition.desired();
        let attempt = definition.attempt;
        let last_running_attempt = definition.last_running_attempt;
        let failure_count = definition.failure_count;
        let next_start_unix_ms = definition.next_start_unix_ms;
        self.catalog_call(move |catalog| {
            catalog
                .set_lifecycle(
                    extension_id,
                    Some(enabled),
                    Some(desired),
                    Some(attempt),
                    Some(last_running_attempt),
                    Some(failure_count),
                    Some(next_start_unix_ms),
                    Some(BlockedState::Clear),
                )
                .map(|_| ())
        })
        .await
    }

    fn validate_module(&self, module: &[u8]) -> Result<(), String> {
        #[cfg(test)]
        if let Some(hook) = self
            .validation_hook
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
        {
            hook();
        }
        validate_extension_object(module, &self.host_config).map_err(|error| error.to_string())
    }

    fn before_storage_io(&self) {
        #[cfg(test)]
        if let Some(hook) = self
            .storage_hook
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
        {
            hook();
        }
    }

    async fn probe_object(&self, hash: ObjectHash, durable: bool) -> ObjectProbe {
        let reserved = {
            let mut inner = self.inner.lock().await;
            inner
                .store
                .as_mut()
                .ok_or(ObjectStoreError::NotFound)
                .and_then(|store| store.reserve_read(&hash))
        };
        let read = match reserved {
            Ok(read) => read,
            Err(_) => return ObjectProbe::Miss,
        };
        let validation = tokio::task::block_in_place(|| {
            let module = read.read_verified()?;
            self.validate_module(&module)
                .map_err(ObjectStoreError::InvalidModule)?;
            Ok::<(), ObjectStoreError>(())
        });
        match validation {
            Ok(()) => {
                {
                    let mut inner = self.inner.lock().await;
                    if let Some(store) = inner.store.as_mut() {
                        store.mark_executable(&hash);
                    }
                }
                if durable && let Err(error) = tokio::task::block_in_place(|| read.sync()) {
                    return ObjectProbe::Durability(error);
                }
                if let Err(error) = self.persist_store_lru().await {
                    return ObjectProbe::Durability(error);
                }
                ObjectProbe::Hit(read)
            }
            Err(error) => {
                let missing = matches!(
                    &error,
                    ObjectStoreError::Io(io) if io.kind() == std::io::ErrorKind::NotFound
                );
                let invalid = matches!(
                    &error,
                    ObjectStoreError::HashMismatch | ObjectStoreError::InvalidModule(_)
                );
                if missing || (invalid && read.remove_file().is_ok()) {
                    let mut inner = self.inner.lock().await;
                    if let Some(store) = inner.store.as_mut() {
                        store.forget_removed(&hash);
                    }
                    mark_hash_unpinned(&mut inner, &hash);
                }
                ObjectProbe::Miss
            }
        }
    }

    pub(crate) async fn register_endpoint<S>(
        self: &Arc<Self>,
        state: super::AppState,
        endpoint: u64,
        sender: S,
    ) where
        S: Into<super::TrackedOutboxSender>,
    {
        self.register_endpoint_inner(Some(state), endpoint, sender.into())
            .await;
    }

    #[cfg(test)]
    async fn register_untracked_endpoint(
        self: &Arc<Self>,
        endpoint: u64,
        sender: super::TrackedOutboxSender,
    ) {
        self.register_endpoint_inner(None, endpoint, sender).await;
    }

    async fn register_endpoint_inner(
        self: &Arc<Self>,
        state: Option<super::AppState>,
        endpoint: u64,
        sender: super::TrackedOutboxSender,
    ) {
        let wake = Arc::new(Notify::new());
        sender.install_drain_notify(&wake);
        {
            let mut inner = self.inner.lock().await;
            if let Some(previous) = inner.endpoint_wakes.insert(endpoint, Arc::clone(&wake)) {
                previous.notify_one();
            }
            inner.endpoints.insert(endpoint, sender);
        }
        let service = Arc::clone(self);
        tokio::spawn(async move {
            service.output_scheduler(state, endpoint, wake).await;
        });
    }

    async fn output_scheduler(
        self: Arc<Self>,
        state: Option<super::AppState>,
        endpoint: u64,
        wake: Arc<Notify>,
    ) {
        let mut last_extension = None;
        loop {
            let sender = {
                let inner = self.inner.lock().await;
                match (
                    inner.endpoints.get(&endpoint),
                    inner.endpoint_wakes.get(&endpoint),
                ) {
                    (Some(sender), Some(current)) if Arc::ptr_eq(current, &wake) => sender.clone(),
                    _ => return,
                }
            };
            if sender.is_closed() {
                return;
            }
            if sender.requires_soft_gate() {
                let Some(state) = state.as_ref() else {
                    return;
                };
                let blocked = {
                    let session = state.session.lock().await;
                    let Some(client) = session.clients.get(&endpoint) else {
                        return;
                    };
                    super::outbox_backpressured(client)
                };
                if blocked {
                    wake.notified().await;
                    continue;
                }
            }
            let outcome = {
                let mut inner = self.inner.lock().await;
                schedule_one_locked(&mut inner, endpoint, last_extension)
            };
            match outcome {
                ScheduleOutcome::Sent(extension_id) => {
                    last_extension = Some(extension_id);
                    tokio::task::yield_now().await;
                }
                ScheduleOutcome::Idle => wake.notified().await,
                ScheduleOutcome::Closed => return,
            }
        }
    }

    pub(crate) async fn unregister_endpoint(
        self: &Arc<Self>,
        endpoint: u64,
        endpoint_generation: u64,
    ) {
        self.upload_tails
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&endpoint);
        let store_io = self.store_io.lock().await;
        let mut to_cancel = Vec::new();
        let mut to_wait = Vec::new();
        let mut upload_cleanups = Vec::new();
        {
            let mut inner = self.inner.lock().await;
            inner.endpoints.remove(&endpoint);
            if let Some(wake) = inner.endpoint_wakes.remove(&endpoint) {
                wake.notify_one();
            }
            inner.commands.close_endpoint(endpoint);
            inner
                .commands
                .invalidate_endpoint(endpoint, endpoint_generation);
            let aborted_uploads = inner.store.as_mut().map_or_else(Vec::new, |store| {
                let (hashes, cleanups) = store.take_endpoint_uploads(endpoint);
                upload_cleanups = cleanups;
                hashes
            });
            notify_need_object_locked(&mut inner, &aborted_uploads, self.output_retain_max);
            let mut changed = Vec::new();
            let mut remove_now = Vec::new();
            let mut invalidate_attempts = Vec::new();
            let mut owned_definitions = Vec::new();
            for definition in inner.definitions.values_mut() {
                definition.followers.remove(&endpoint);
                if definition.owner_endpoint == Some(endpoint) {
                    owned_definitions.push(definition.extension_id);
                    definition.set_flag(EXT_FLAG_DESIRED_RUNNING, false);
                    definition.interrupt = Some(Interrupt::OwnerClosed);
                    definition.generation = definition.generation.saturating_add(1);
                    definition.wake.notify_waiters();
                    // Preserve a wake permit when the supervisor is between
                    // checks so cleanup cannot fall through terminal retain.
                    definition.wake.notify_one();
                    if let Some(control) = definition.control.clone() {
                        definition.phase = EXT_PHASE_STOPPING;
                        definition.task_id = 0;
                        invalidate_attempts.push((
                            definition.extension_id,
                            control.definition_revision,
                            control.attempt,
                        ));
                        to_cancel.push(control);
                        changed.push(definition.extension_id);
                    } else {
                        definition.phase = EXT_PHASE_STOPPED;
                        definition.pending_deadline = None;
                        definition.release_deadline = None;
                        definition.detail = "attached owner disconnected".into();
                        changed.push(definition.extension_id);
                        remove_now.push(definition.extension_id);
                    }
                }
            }
            for extension_id in owned_definitions {
                if let Some(completions) = inner.supervisor_completions.get(&extension_id) {
                    to_wait.extend(completions.iter().cloned());
                }
            }
            for (extension_id, revision, attempt) in invalidate_attempts {
                inner
                    .commands
                    .invalidate_attempt(extension_id, revision, attempt);
            }
            for extension_id in changed {
                emit_lifecycle_locked(&mut inner, extension_id, self.output_retain_max);
            }
            for extension_id in remove_now {
                remove_definition_locked(&mut inner, extension_id);
            }
        }
        let cleanup_results = if upload_cleanups.is_empty() {
            Vec::new()
        } else {
            tokio::task::spawn_blocking(move || {
                upload_cleanups
                    .into_iter()
                    .map(|cleanup| cleanup.finish())
                    .collect::<Vec<_>>()
            })
            .await
            .unwrap_or_default()
        };
        if !cleanup_results.is_empty() {
            let mut inner = self.inner.lock().await;
            if let Some(store) = inner.store.as_mut() {
                for cleanup in cleanup_results {
                    store.commit_upload_cleanup(cleanup);
                }
            }
        }
        drop(store_io);
        for control in to_cancel {
            control.connection.cancel();
            control.host.cancel();
        }
        // A child supervisor completes only after its runtime attempt, logical
        // connection, native jobs, and that connection's own attached
        // children have drained. Waiting outside the service lock therefore
        // forms a recursive cleanup barrier without serializing unrelated
        // extension work.
        for completion in to_wait {
            completion.wait().await;
        }
    }

    pub(crate) async fn restore(self: &Arc<Self>, state: super::AppState) {
        if self
            .maintenance_started
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
        {
            let service = Arc::clone(self);
            tokio::spawn(async move {
                service.maintenance_loop().await;
            });
        }
        let definitions = {
            let inner = self.inner.lock().await;
            if !self.persist_allowed || !self.available || inner.store.is_none() {
                Vec::new()
            } else {
                inner
                    .definitions
                    .values()
                    .filter(|definition| {
                        definition.persistent()
                            && definition.enabled()
                            && definition.desired()
                            && definition.object_pinned
                            && definition.phase != EXT_PHASE_BLOCKED
                    })
                    .map(|definition| (definition.extension_id, definition.next_start_unix_ms))
                    .collect()
            }
        };
        let now = unix_millis_now();
        for (id, next_start_unix_ms) in definitions {
            let delay = Duration::from_millis(next_start_unix_ms.saturating_sub(now));
            if delay.is_zero() {
                self.ensure_supervisor(state.clone(), id).await;
            } else {
                let service = Arc::clone(self);
                let state = state.clone();
                tokio::spawn(async move {
                    tokio::time::sleep(delay).await;
                    service.ensure_supervisor(state, id).await;
                });
            }
        }
    }

    async fn maintenance_loop(self: Arc<Self>) {
        loop {
            tokio::time::sleep(Duration::from_secs(1)).await;
            let _store_io = self.store_io.lock().await;
            let (cleanups, retries, snapshot) = {
                let mut inner = self.inner.lock().await;
                if inner.shutting_down {
                    return;
                }
                let now = Instant::now();
                let (expired_uploads, cleanups) = inner
                    .store
                    .as_mut()
                    .map(|store| store.take_expired_uploads(now))
                    .unwrap_or_default();
                notify_need_object_locked(&mut inner, &expired_uploads, self.output_retain_max);
                expire_pending_locked(
                    &mut inner,
                    now,
                    self.output_retain_max,
                    self.terminal_retain,
                );
                release_expired_pending_locked(&mut inner, now);
                let snapshot = inner
                    .store
                    .as_ref()
                    .and_then(|store| store.lru_snapshot().ok().flatten());
                let retries = inner
                    .store
                    .as_ref()
                    .map(ObjectStore::cleanup_retries)
                    .unwrap_or_default();
                (cleanups, retries, snapshot)
            };
            let cleanup_results = if cleanups.is_empty() {
                Vec::new()
            } else {
                tokio::task::spawn_blocking(move || {
                    cleanups
                        .into_iter()
                        .map(|cleanup| cleanup.finish())
                        .collect::<Vec<_>>()
                })
                .await
                .unwrap_or_default()
            };
            let retry_results = if retries.is_empty() {
                Vec::new()
            } else {
                tokio::task::spawn_blocking(move || {
                    retries
                        .into_iter()
                        .map(|retry| retry.finish())
                        .collect::<Vec<_>>()
                })
                .await
                .unwrap_or_default()
            };
            let (snapshot, persisted) = if let Some(snapshot) = snapshot {
                tokio::task::spawn_blocking(move || {
                    let persisted = snapshot.persist().is_ok();
                    (Some(snapshot), persisted)
                })
                .await
                .unwrap_or((None, false))
            } else {
                (None, false)
            };
            if !cleanup_results.is_empty() || !retry_results.is_empty() || persisted {
                let mut inner = self.inner.lock().await;
                if let Some(store) = inner.store.as_mut() {
                    for cleanup in cleanup_results {
                        store.commit_upload_cleanup(cleanup);
                    }
                    for retry in retry_results {
                        store.commit_cleanup_retry(retry);
                    }
                    if persisted && let Some(snapshot) = snapshot.as_ref() {
                        store.acknowledge_lru_snapshot(snapshot);
                    }
                }
            }
        }
    }

    /// Publish the extension shutdown barrier before global connection
    /// cancellation. This closes the restart/accounting race while allowing
    /// the caller to cancel every connection before waiting for supervisors.
    pub(crate) async fn begin_shutdown(&self) {
        let (controls, wakes) = {
            let mut inner = self.inner.lock().await;
            inner.shutting_down = true;
            let controls = inner
                .definitions
                .values_mut()
                .filter_map(|definition| {
                    definition.interrupt = Some(Interrupt::ServerShutdown);
                    definition.control.clone()
                })
                .collect::<Vec<_>>();
            let wakes = inner
                .definitions
                .values()
                .map(|definition| Arc::clone(&definition.wake))
                .collect::<Vec<_>>();
            (controls, wakes)
        };
        for wake in wakes {
            wake.notify_waiters();
        }
        for control in controls {
            control.connection.cancel();
            control.host.cancel();
        }
    }

    pub(crate) async fn shutdown(&self) {
        self.begin_shutdown().await;
        loop {
            if self.inner.lock().await.supervisors.is_empty() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    pub(crate) fn dispatch<'a>(
        self: &'a Arc<Self>,
        state: super::AppState,
        endpoint: u64,
        origin: &'a super::ConnectionOrigin,
        packet: &'a [u8],
    ) -> Pin<Box<dyn Future<Output = DispatchOutcome> + Send + 'a>> {
        Box::pin(self.dispatch_inner(state, endpoint, origin, packet))
    }

    /// Dispatch an extension-origin request without holding the connection
    /// reader behind module validation. Requests which can validate the CAS
    /// are transferred into the endpoint's bounded native-job lane; cheap
    /// control and acknowledgement traffic remains inline on the reader.
    pub(crate) fn dispatch_owned<'a>(
        self: &'a Arc<Self>,
        state: super::AppState,
        endpoint: u64,
        origin: &'a super::ConnectionOrigin,
        packet: Vec<u8>,
        jobs: EndpointTracker,
    ) -> Pin<Box<dyn Future<Output = DispatchOutcome> + Send + 'a>> {
        Box::pin(self.dispatch_owned_inner(state, endpoint, origin, packet, jobs))
    }

    async fn dispatch_owned_inner(
        self: &Arc<Self>,
        state: super::AppState,
        endpoint: u64,
        origin: &super::ConnectionOrigin,
        packet: Vec<u8>,
        jobs: EndpointTracker,
    ) -> DispatchOutcome {
        let (requires_detached_job, is_put) = if self.advertised() {
            match wire::parse_extension_request(&packet) {
                Ok(Some(ExtensionRequest::Run { flags, args, .. })) => {
                    let persistent = flags & EXT_RUN_PERSIST != 0;
                    (
                        !(persistent
                            && (!self.persist_allowed
                                || args.iter().any(|arg| std::str::from_utf8(arg).is_err()))),
                        false,
                    )
                }
                // Every upload chunk can touch the filesystem. Keep the
                // extension reader available for STATUS/CANCEL while a slow
                // cache device is servicing any part of PUT.
                Ok(Some(ExtensionRequest::Put { .. })) => (true, true),
                _ => (false, false),
            }
        } else {
            (false, false)
        };
        if !requires_detached_job {
            return self.dispatch_inner(state, endpoint, origin, &packet).await;
        }

        let service = Arc::clone(self);
        let worker_jobs = jobs.clone();
        let request_bytes = packet.len();
        let upload_order = is_put.then(|| {
            let current = SupervisorCompletion::new();
            let previous = self
                .upload_tails
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .insert(endpoint, Arc::clone(&current));
            UploadOrder { previous, current }
        });
        match jobs.spawn_async(request_bytes, async move {
            if let Some(order) = upload_order {
                order.wait().await;
                // Keep the completion guard alive through reply publication.
                let _order = order;
                service
                    .dispatch_validation_job(state, endpoint, packet, worker_jobs)
                    .await;
                return;
            }
            service
                .dispatch_validation_job(state, endpoint, packet, worker_jobs)
                .await;
        }) {
            Ok(()) => DispatchOutcome::Continue,
            Err(_) => DispatchOutcome::Close,
        }
    }

    async fn dispatch_validation_job(
        self: Arc<Self>,
        state: super::AppState,
        endpoint: u64,
        packet: Vec<u8>,
        jobs: EndpointTracker,
    ) {
        let Ok(Some(request)) = wire::parse_extension_request(&packet) else {
            return;
        };
        let needs_validation = matches!(request, ExtensionRequest::Run { .. })
            || matches!(
                request,
                ExtensionRequest::Put { flags, .. }
                    if flags & (EXT_PUT_BEGIN | EXT_PUT_FINAL) != 0
            );
        let validation = if needs_validation {
            Some(tokio::select! {
                _ = jobs.cancelled() => return,
                permit = self.validating.clone().acquire_owned() => {
                    let Ok(permit) = permit else {
                        return;
                    };
                    permit
                }
            })
        } else {
            None
        };
        if jobs.is_cancelled() {
            return;
        }
        match request {
            ExtensionRequest::Run {
                nonce,
                flags,
                restart,
                expected_extension_id,
                expected_definition_revision,
                hash,
                name,
                args,
            } => {
                self.handle_run(
                    state,
                    endpoint,
                    nonce,
                    flags,
                    restart,
                    expected_extension_id,
                    expected_definition_revision,
                    hash,
                    name,
                    args,
                    validation,
                    Some(&jobs),
                )
                .await;
            }
            ExtensionRequest::Put {
                nonce,
                flags,
                hash,
                offset,
                total_size,
                data,
            } => {
                self.handle_put(
                    state,
                    endpoint,
                    nonce,
                    flags,
                    hash,
                    offset,
                    total_size,
                    data,
                    validation,
                    Some(&jobs),
                )
                .await;
            }
            _ => unreachable!("only RUN and PUT requests enter the detached lane"),
        }
    }

    async fn dispatch_inner(
        self: &Arc<Self>,
        state: super::AppState,
        endpoint: u64,
        origin: &super::ConnectionOrigin,
        packet: &[u8],
    ) -> DispatchOutcome {
        let request = match wire::parse_extension_request(packet) {
            Ok(Some(request)) => request,
            Ok(None) => return DispatchOutcome::Continue,
            Err(error) => {
                self.reply_decode_error(endpoint, packet, &error).await;
                return DispatchOutcome::Continue;
            }
        };

        if !self.advertised() {
            self.reply_disabled(endpoint, request).await;
            return DispatchOutcome::Continue;
        }

        match request {
            ExtensionRequest::Run {
                nonce,
                flags,
                restart,
                expected_extension_id,
                expected_definition_revision,
                hash,
                name,
                args,
            } => {
                self.handle_run(
                    state,
                    endpoint,
                    nonce,
                    flags,
                    restart,
                    expected_extension_id,
                    expected_definition_revision,
                    hash,
                    name,
                    args,
                    None,
                    None,
                )
                .await;
                DispatchOutcome::Continue
            }
            ExtensionRequest::Put {
                nonce,
                flags,
                hash,
                offset,
                total_size,
                data,
            } => {
                self.handle_put(
                    state, endpoint, nonce, flags, hash, offset, total_size, data, None, None,
                )
                .await;
                DispatchOutcome::Continue
            }
            ExtensionRequest::Control {
                nonce,
                extension_id,
                action,
            } => {
                self.handle_control(state, endpoint, nonce, extension_id, action)
                    .await;
                DispatchOutcome::Continue
            }
            ExtensionRequest::Event { kind, data } => {
                if self.handle_event(origin, kind, data).await {
                    DispatchOutcome::Continue
                } else {
                    DispatchOutcome::Close
                }
            }
            ExtensionRequest::CommandRegister {
                nonce,
                listener_id,
                descriptor,
            } => {
                self.handle_command_register(
                    state,
                    endpoint,
                    origin,
                    nonce,
                    listener_id,
                    descriptor,
                )
                .await;
                DispatchOutcome::Continue
            }
            ExtensionRequest::CommandDiscover {
                nonce,
                directory_revision,
                cursor,
            } => {
                self.handle_command_discover(endpoint, nonce, directory_revision, cursor)
                    .await;
                DispatchOutcome::Continue
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    async fn handle_run(
        self: &Arc<Self>,
        state: super::AppState,
        endpoint: u64,
        nonce: u16,
        run_flags: u8,
        restart: u8,
        expected_id: u64,
        expected_revision: u64,
        hash: ObjectHash,
        name: &str,
        arguments: Vec<&[u8]>,
        admitted_validation: Option<OwnedSemaphorePermit>,
        jobs: Option<&EndpointTracker>,
    ) {
        let update = run_flags & EXT_RUN_UPDATE != 0;
        let persistent = run_flags & EXT_RUN_PERSIST != 0;
        if persistent && !self.persist_allowed {
            self.send(
                endpoint,
                run_error_status(
                    nonce,
                    EXT_STATUS_PERMISSION,
                    hash,
                    "persistent extensions are disabled on this server",
                ),
            )
            .await;
            return;
        }
        if persistent
            && arguments
                .iter()
                .any(|arg| std::str::from_utf8(arg).is_err())
        {
            self.send(
                endpoint,
                run_error_status(
                    nonce,
                    EXT_STATUS_INVALID,
                    hash,
                    "persistent extension arguments must be UTF-8",
                ),
            )
            .await;
            return;
        }
        let argument_charge = encoded_borrowed_argument_bytes(&arguments);
        let Some(argument_reservation) = self.argument_budget.try_reserve(argument_charge) else {
            self.send(
                endpoint,
                run_error_status(
                    nonce,
                    EXT_STATUS_BUDGET,
                    hash,
                    "extension argument store is full",
                ),
            )
            .await;
            return;
        };
        // Admission precedes the only borrowed-to-owned argument copy. The
        // same guard is transferred into a resident definition, so this path
        // never reserves the argument bytes a second time. Extension-origin
        // packet bytes remain independently charged by EndpointTracker.
        let mut argument_reservation = Some(argument_reservation);
        let args = arguments
            .into_iter()
            .map(<[u8]>::to_vec)
            .collect::<Vec<_>>();
        let _validation = match admitted_validation {
            Some(permit) => permit,
            None => match self.validating.clone().acquire_owned().await {
                Ok(permit) => permit,
                Err(_) => {
                    self.send(
                        endpoint,
                        run_error_status(
                            nonce,
                            EXT_STATUS_OTHER,
                            hash,
                            "extension validation service is unavailable",
                        ),
                    )
                    .await;
                    return;
                }
            },
        };
        let (object_read, durability_error) = match self.probe_object(hash, persistent).await {
            ObjectProbe::Hit(read) => (Some(read), None),
            ObjectProbe::Miss => (None, None),
            ObjectProbe::Durability(error) => (None, Some(error)),
        };
        if jobs.is_some_and(EndpointTracker::is_cancelled) {
            return;
        }
        let object_hit = object_read.is_some();
        let _catalog_io = if persistent || update {
            Some(self.catalog_io.lock().await)
        } else {
            None
        };

        let mut start = None;
        let mut cancel = None;
        let mut created = None;
        let mut emit_after_reply = None;
        let response;
        {
            let mut inner = self.inner.lock().await;
            if jobs.is_some_and(EndpointTracker::is_cancelled) {
                return;
            }
            if inner.shutting_down {
                response =
                    run_error_status(nonce, EXT_STATUS_OTHER, hash, "server is shutting down");
            } else if update {
                let Some(current) = inner
                    .definitions
                    .values()
                    .find(|definition| definition.persistent() && definition.name == name)
                    .cloned()
                else {
                    response = run_error_status(
                        nonce,
                        EXT_STATUS_NOT_FOUND,
                        hash,
                        "persistent extension name does not exist",
                    );
                    drop(inner);
                    self.send(endpoint, response).await;
                    return;
                };
                if current.extension_id != expected_id
                    || current.definition_revision != expected_revision
                {
                    response = status_packet(
                        &current,
                        nonce,
                        EXT_STATUS_CONFLICT,
                        None,
                        "extension definition changed",
                    );
                } else if let Some(error) = durability_error.as_ref() {
                    response =
                        status_packet(&current, nonce, EXT_STATUS_OTHER, None, &error.to_string());
                } else if !object_hit {
                    response = update_operation_status(
                        &current,
                        nonce,
                        EXT_STATUS_OK,
                        EXT_PHASE_NEED_OBJECT,
                        hash,
                        restart,
                        "module upload required",
                    );
                } else {
                    let current_arguments = if current.args.is_some() {
                        current.args.clone().ok_or(CatalogError::Unavailable)
                    } else {
                        drop(inner);
                        let loaded = self.definition_arguments(&current).await;
                        inner = self.inner.lock().await;
                        loaded
                    };
                    if let Err(error) = &current_arguments {
                        response = status_packet(
                            &current,
                            nonce,
                            catalog_status(error),
                            None,
                            &error.to_string(),
                        );
                    } else if current.hash == hash
                        && current_arguments
                            .as_ref()
                            .is_ok_and(|stored| stored == &args)
                        && current.restart == restart
                    {
                        match repair_persistent_pin(&mut inner, &current) {
                            Ok(()) => {
                                let current_id = current.extension_id;
                                drop(inner);
                                let cleared = self
                                    .catalog_call(move |catalog| {
                                        catalog.set_lifecycle(
                                            current_id,
                                            None,
                                            None,
                                            None,
                                            None,
                                            None,
                                            Some(0),
                                            Some(BlockedState::Clear),
                                        )
                                    })
                                    .await;
                                inner = self.inner.lock().await;
                                match cleared {
                                    Ok(_) => {
                                        let shutting_down = inner.shutting_down;
                                        if let Some(definition) =
                                            inner.definitions.get_mut(&current.extension_id)
                                        {
                                            if definition.phase == EXT_PHASE_BLOCKED {
                                                definition.detail.clear();
                                                definition.generation =
                                                    definition.generation.saturating_add(1);
                                                definition.wake.notify_waiters();
                                                if shutting_down {
                                                    definition.phase = EXT_PHASE_STOPPED;
                                                } else if definition.enabled()
                                                    && definition.desired()
                                                {
                                                    definition.phase = EXT_PHASE_QUEUED;
                                                    start = Some(definition.extension_id);
                                                } else {
                                                    definition.phase = EXT_PHASE_STOPPED;
                                                }
                                                emit_after_reply = Some(definition.extension_id);
                                            }
                                            response = update_operation_status(
                                                definition,
                                                nonce,
                                                EXT_STATUS_OK,
                                                wire::EXT_PHASE_NONE,
                                                definition.hash,
                                                definition.restart,
                                                "extension definition is unchanged",
                                            );
                                        } else {
                                            response = run_error_status(
                                                nonce,
                                                EXT_STATUS_OTHER,
                                                hash,
                                                "extension disappeared during update",
                                            );
                                        }
                                    }
                                    Err(error) => {
                                        response = status_packet(
                                            &current,
                                            nonce,
                                            catalog_status(&error),
                                            None,
                                            &error.to_string(),
                                        );
                                    }
                                }
                            }
                            Err(error) => {
                                response = status_packet(
                                    &current,
                                    nonce,
                                    catalog_status(&error),
                                    None,
                                    &error.to_string(),
                                );
                            }
                        }
                    } else {
                        drop(inner);
                        let updated = self
                            .commit_catalog_update(&current, hash, args, restart)
                            .await;
                        inner = self.inner.lock().await;
                        match updated {
                            Ok(updated) => {
                                let shutting_down = inner.shutting_down;
                                if let Some(definition) = inner.definitions.get_mut(&expected_id) {
                                    definition.definition_revision = updated.definition_revision;
                                    definition.hash = hash;
                                    release_definition_arguments(definition);
                                    definition.argument_bytes = updated.argument_bytes;
                                    definition.restart = restart;
                                    definition.object_pinned = true;
                                    definition.generation = definition.generation.saturating_add(1);
                                    definition.failure_count = 0;
                                    definition.interrupt = Some(Interrupt::Updated);
                                    definition.detail.clear();
                                    definition.pending_deadline = None;
                                    definition.next_start_unix_ms = 0;
                                    definition.wake.notify_waiters();
                                    cancel = definition.control.clone();
                                    if cancel.is_some() {
                                        definition.phase = EXT_PHASE_STOPPING;
                                        definition.task_id = 0;
                                    } else if shutting_down {
                                        definition.phase = EXT_PHASE_STOPPED;
                                    } else if definition.enabled() && definition.desired() {
                                        definition.phase = EXT_PHASE_QUEUED;
                                        start = Some(expected_id);
                                    } else {
                                        definition.phase = EXT_PHASE_STOPPED;
                                    }
                                    response = update_operation_status(
                                        definition,
                                        nonce,
                                        EXT_STATUS_OK,
                                        wire::EXT_PHASE_NONE,
                                        definition.hash,
                                        definition.restart,
                                        "",
                                    );
                                    emit_after_reply = Some(expected_id);
                                } else {
                                    response = run_error_status(
                                        nonce,
                                        EXT_STATUS_OTHER,
                                        hash,
                                        "extension disappeared during update",
                                    );
                                }
                            }
                            Err(error) => {
                                response = status_packet(
                                    &current,
                                    nonce,
                                    catalog_status(&error),
                                    None,
                                    &error.to_string(),
                                );
                            }
                        }
                    }
                }
            } else {
                let transient_count = inner
                    .definitions
                    .values()
                    .filter(|definition| !definition.persistent())
                    .count();
                let persistent_count = inner
                    .definitions
                    .values()
                    .filter(|definition| definition.persistent())
                    .count();
                let name_conflict = persistent
                    && inner
                        .definitions
                        .values()
                        .any(|definition| definition.persistent() && definition.name == name);
                let follower_capacity = follower_capacity_available(
                    &inner,
                    endpoint,
                    self.follow_max_per_endpoint,
                    self.follow_max,
                );
                if name_conflict {
                    response = run_error_status(
                        nonce,
                        EXT_STATUS_CONFLICT,
                        hash,
                        "persistent extension name already exists",
                    );
                } else if (!persistent && transient_count >= self.max_transient)
                    || (persistent && persistent_count >= self.max_persistent)
                {
                    response = run_error_status(
                        nonce,
                        EXT_STATUS_BUDGET,
                        hash,
                        "extension supervisor capacity exhausted",
                    );
                } else if !follower_capacity {
                    response = run_error_status(
                        nonce,
                        EXT_STATUS_BUDGET,
                        hash,
                        "extension follower capacity exhausted",
                    );
                } else if let Some(error) = durability_error.as_ref() {
                    response = run_error_status(nonce, EXT_STATUS_OTHER, hash, &error.to_string());
                } else {
                    // The reservation exists before argument cloning and ID
                    // admission. Persistent cache hits drop it again as soon
                    // as their redb transaction commits.
                    let Some(extension_id) = allocate_extension_id(&inner) else {
                        response = run_error_status(
                            nonce,
                            EXT_STATUS_BUDGET,
                            hash,
                            "could not allocate an extension ID",
                        );
                        drop(inner);
                        self.send(endpoint, response).await;
                        return;
                    };
                    let hit = object_hit;
                    let flags = (u8::from(run_flags & EXT_RUN_DETACH != 0) * EXT_FLAG_DETACH)
                        | (u8::from(persistent) * EXT_FLAG_PERSIST)
                        | EXT_FLAG_ENABLED
                        | EXT_FLAG_DESIRED_RUNNING;
                    let mut definition = Definition {
                        extension_id,
                        definition_revision: 1,
                        flags,
                        restart,
                        hash,
                        name: name.to_owned(),
                        args: Some(args),
                        argument_bytes: argument_charge,
                        argument_reservation: argument_reservation.take(),
                        owner_endpoint: (run_flags & EXT_RUN_DETACH == 0).then_some(endpoint),
                        phase: if hit {
                            EXT_PHASE_QUEUED
                        } else {
                            EXT_PHASE_NEED_OBJECT
                        },
                        attempt: 0,
                        last_running_attempt: 0,
                        task_id: 0,
                        next_start_unix_ms: 0,
                        detail: if hit {
                            String::new()
                        } else {
                            "module upload required".into()
                        },
                        next_output_sequence: 1,
                        retained: VecDeque::new(),
                        terminal_replay: VecDeque::new(),
                        retained_bytes: 0,
                        followers: HashMap::new(),
                        pending_deadline: (!hit).then_some(Instant::now() + self.pending_timeout),
                        release_deadline: None,
                        generation: 1,
                        failure_count: 0,
                        interrupt: None,
                        control: None,
                        object_pinned: false,
                        catalog_committed: false,
                        wake: Arc::new(Notify::new()),
                    };
                    definition.followers.insert(
                        endpoint,
                        FollowerCursor {
                            next_sequence: 1,
                            replay_through: Some(0),
                        },
                    );
                    let admitted = if hit && persistent {
                        drop(inner);
                        let committed = self.commit_catalog_create(&definition).await;
                        inner = self.inner.lock().await;
                        committed.map(|persistent| {
                            definition.definition_revision = persistent.definition_revision;
                            definition.flags = persistent.flags;
                            definition.argument_bytes = persistent.argument_bytes;
                            definition.catalog_committed = true;
                            definition.object_pinned = true;
                            release_definition_arguments(&mut definition);
                        })
                    } else if hit {
                        commit_transient_create(&mut inner, &mut definition)
                    } else {
                        Ok(())
                    };
                    match admitted {
                        Ok(()) => {
                            if hit && inner.shutting_down {
                                definition.phase = EXT_PHASE_STOPPED;
                            }
                            response = creation_status(&definition, nonce, &definition.detail);
                            created = Some(extension_id);
                            inner.definitions.insert(extension_id, definition);
                            if hit && !inner.shutting_down {
                                start = Some(extension_id);
                            }
                        }
                        Err((status, detail)) => {
                            release_definition_arguments(&mut definition);
                            response = run_error_status(nonce, status, hash, &detail);
                        }
                    }
                }
            }
            if let Some(sender) = inner.endpoints.get(&endpoint) {
                let _ = sender.send(response);
            }
            if created.is_some() {
                wake_endpoint_locked(&inner, endpoint);
            }
            if let Some(extension_id) = emit_after_reply {
                emit_lifecycle_locked(&mut inner, extension_id, self.output_retain_max);
            }
        }
        drop(_catalog_io);
        if let Some(control) = cancel {
            control.connection.cancel();
            control.host.cancel();
        }
        if let Some(extension_id) = start {
            self.ensure_supervisor(state, extension_id).await;
        }
    }

    #[allow(clippy::too_many_arguments)]
    async fn handle_put(
        self: &Arc<Self>,
        state: super::AppState,
        endpoint: u64,
        nonce: u16,
        flags: u8,
        hash: ObjectHash,
        offset: u64,
        total_size: u64,
        data: &[u8],
        admitted_validation: Option<OwnedSemaphorePermit>,
        jobs: Option<&EndpointTracker>,
    ) {
        let validation_request =
            if flags & (EXT_PUT_BEGIN | EXT_PUT_FINAL) != 0 && admitted_validation.is_none() {
                self.validation_request_budget.try_reserve(data.len())
            } else {
                None
            };
        if flags & (EXT_PUT_BEGIN | EXT_PUT_FINAL) != 0
            && admitted_validation.is_none()
            && validation_request.is_none()
        {
            let inner = self.inner.lock().await;
            if let Some(sender) = inner.endpoints.get(&endpoint) {
                let _ = sender.send(put_status(
                    nonce,
                    EXT_STATUS_BUDGET,
                    hash,
                    0,
                    "extension validation request budget exhausted",
                ));
            }
            return;
        }
        let _validation_request = validation_request;
        let _validation = if flags & (EXT_PUT_BEGIN | EXT_PUT_FINAL) != 0 {
            match admitted_validation {
                Some(permit) => Some(permit),
                None => self.validating.clone().acquire_owned().await.ok(),
            }
        } else {
            debug_assert!(admitted_validation.is_none());
            None
        };
        let _begin_read = if flags & EXT_PUT_BEGIN != 0 {
            match self.probe_object(hash, false).await {
                ObjectProbe::Hit(read) => Some(read),
                ObjectProbe::Miss | ObjectProbe::Durability(_) => None,
            }
        } else {
            None
        };
        // Upload tokens temporarily leave ObjectStore while their file work
        // runs. Serialize those transitions without occupying `inner`; status,
        // control, output, and cancellation remain independently available.
        let _store_io = self.store_io.lock().await;
        if jobs.is_some_and(EndpointTracker::is_cancelled) {
            return;
        }
        let begin = if flags & EXT_PUT_BEGIN != 0 {
            if let Err(error) = self.persist_store_lru_in_lane().await {
                Some(Err(error))
            } else {
                Some(loop {
                    let prepared = {
                        let mut inner = self.inner.lock().await;
                        if jobs.is_some_and(EndpointTracker::is_cancelled) {
                            return;
                        }
                        let Some(store) = inner.store.as_mut() else {
                            break Err(ObjectStoreError::NotFound);
                        };
                        store.prepare_begin_upload_after_probe(
                            endpoint,
                            hash,
                            total_size,
                            Instant::now(),
                        )
                    };
                    match prepared {
                        Ok(PreparedBeginUpload::Complete(result)) => break Ok(result),
                        Ok(PreparedBeginUpload::Evict(eviction)) => {
                            let evicted = tokio::task::block_in_place(|| {
                                self.before_storage_io();
                                (*eviction).finish()
                            });
                            let committed = {
                                let mut inner = self.inner.lock().await;
                                inner
                                    .store
                                    .as_mut()
                                    .ok_or(ObjectStoreError::NotFound)
                                    .and_then(|store| store.commit_eviction(evicted))
                            };
                            if let Err(error) = committed {
                                break Err(error);
                            }
                            if let Err(error) = self.persist_store_lru_in_lane().await {
                                break Err(error);
                            }
                        }
                        Ok(PreparedBeginUpload::Create(creation)) => {
                            let created = tokio::task::block_in_place(|| {
                                self.before_storage_io();
                                (*creation).finish()
                            });
                            let committed = {
                                let mut inner = self.inner.lock().await;
                                inner
                                    .store
                                    .as_mut()
                                    .map(|store| store.commit_upload_creation(created))
                            };
                            break match committed {
                                Some(UploadCreationCommit::Complete(result)) => result,
                                Some(UploadCreationCommit::Stale(stale)) => {
                                    let cleanup =
                                        tokio::task::block_in_place(|| (*stale).cleanup());
                                    let mut inner = self.inner.lock().await;
                                    if let Some(store) = inner.store.as_mut() {
                                        store.commit_upload_cleanup(cleanup);
                                    }
                                    Err(ObjectStoreError::Conflict)
                                }
                                None => Err(ObjectStoreError::NotFound),
                            };
                        }
                        Err(error) => break Err(error),
                    }
                })
            }
        } else {
            None
        };

        let result = match begin {
            Some(Ok(BeginUpload::AlreadyHave { size })) => Ok(PutChunk::AlreadyHave { size }),
            Some(Err(error)) => Err(error),
            Some(Ok(BeginUpload::Started)) | None => {
                let prepared = {
                    let mut inner = self.inner.lock().await;
                    inner
                        .store
                        .as_mut()
                        .ok_or(ObjectStoreError::NotFound)
                        .and_then(|store| {
                            store.prepare_put_chunk(
                                endpoint,
                                hash,
                                offset,
                                total_size,
                                data,
                                flags & EXT_PUT_FINAL != 0,
                                Instant::now(),
                            )
                        })
                };
                match prepared {
                    Ok(PreparedPut::Complete(result)) => Ok(result),
                    Ok(PreparedPut::Abort(cleanup, error)) => {
                        let cleaned = tokio::task::block_in_place(|| {
                            self.before_storage_io();
                            (*cleanup).finish()
                        });
                        let mut inner = self.inner.lock().await;
                        if let Some(store) = inner.store.as_mut() {
                            store.commit_upload_cleanup(cleaned);
                        }
                        Err(error)
                    }
                    Ok(PreparedPut::Chunk(upload)) => {
                        let completed = tokio::task::block_in_place(|| {
                            self.before_storage_io();
                            (*upload).finish(data)
                        });
                        let committed = {
                            let mut inner = self.inner.lock().await;
                            inner
                                .store
                                .as_mut()
                                .map(|store| store.commit_chunk_upload(completed))
                        };
                        match committed {
                            Some(ChunkUploadCommit::Complete(result)) => result,
                            Some(ChunkUploadCommit::Stale(stale)) => {
                                let cleanup = tokio::task::block_in_place(|| (*stale).cleanup());
                                let mut inner = self.inner.lock().await;
                                if let Some(store) = inner.store.as_mut() {
                                    store.commit_upload_cleanup(cleanup);
                                }
                                Err(ObjectStoreError::Conflict)
                            }
                            None => Err(ObjectStoreError::NotFound),
                        }
                    }
                    Ok(PreparedPut::Final(upload)) => {
                        let finalized = tokio::task::block_in_place(|| {
                            self.before_storage_io();
                            (*upload).finish(data, |module| self.validate_module(module))
                        });
                        let committed = {
                            let mut inner = self.inner.lock().await;
                            inner
                                .store
                                .as_mut()
                                .ok_or(ObjectStoreError::NotFound)
                                .and_then(|store| store.commit_final_upload(finalized))
                        };
                        match committed {
                            Ok(result) => self.persist_store_lru_in_lane().await.map(|()| result),
                            Err(error) => Err(error),
                        }
                    }
                    Err(error) => Err(error),
                }
            }
        };
        let result = match result {
            Ok(result @ PutChunk::AlreadyHave { .. }) => {
                self.persist_store_lru_in_lane().await.map(|()| result)
            }
            result => result,
        };
        let start = self.apply_put_result(endpoint, nonce, hash, result).await;
        for extension_id in start {
            self.ensure_supervisor(state.clone(), extension_id).await;
        }
    }

    async fn apply_put_result(
        &self,
        endpoint: u64,
        nonce: u16,
        hash: ObjectHash,
        result: Result<PutChunk, ObjectStoreError>,
    ) -> Vec<u64> {
        let (status, received, detail, start, transitioned, notify_need_object) = match result {
            Ok(PutChunk::Accepted { received }) => (
                EXT_STATUS_OK,
                received,
                String::new(),
                Vec::new(),
                Vec::new(),
                false,
            ),
            Ok(PutChunk::Committed { size }) => {
                let (start, transitioned) = self.complete_pending(hash).await;
                (
                    EXT_STATUS_OK,
                    size,
                    String::new(),
                    start,
                    transitioned,
                    false,
                )
            }
            Ok(PutChunk::AlreadyHave { size }) => {
                let (start, transitioned) = self.complete_pending(hash).await;
                (
                    wire::EXT_PUT_ALREADY_HAVE,
                    size,
                    "module already exists".into(),
                    start,
                    transitioned,
                    false,
                )
            }
            Err(error) => {
                let status = object_status(&error);
                let detail = error.to_string();
                let mut inner = self.inner.lock().await;
                let transitioned = if matches!(error, ObjectStoreError::InvalidModule(_)) {
                    stop_invalid_pending_locked(&mut inner, hash, &detail, self.terminal_retain)
                } else {
                    Vec::new()
                };
                (
                    status,
                    0,
                    detail,
                    Vec::new(),
                    transitioned,
                    !matches!(
                        error,
                        ObjectStoreError::Conflict | ObjectStoreError::InvalidModule(_)
                    ),
                )
            }
        };
        let mut inner = self.inner.lock().await;
        if let Some(sender) = inner.endpoints.get(&endpoint) {
            let _ = sender.send(put_status(nonce, status, hash, received, &detail));
        }
        for extension_id in transitioned {
            emit_lifecycle_locked(&mut inner, extension_id, self.output_retain_max);
        }
        if notify_need_object {
            notify_need_object_locked(&mut inner, &[hash], self.output_retain_max);
        }
        start
    }

    async fn complete_pending(&self, hash: ObjectHash) -> (Vec<u64>, Vec<u64>) {
        let ids = {
            let inner = self.inner.lock().await;
            inner
                .definitions
                .values()
                .filter(|definition| {
                    definition.hash == hash && definition.phase == EXT_PHASE_NEED_OBJECT
                })
                .map(|definition| definition.extension_id)
                .collect::<Vec<_>>()
        };
        let has_persistent = {
            let inner = self.inner.lock().await;
            ids.iter().any(|extension_id| {
                inner
                    .definitions
                    .get(extension_id)
                    .is_some_and(Definition::persistent)
            })
        };
        let _catalog_io = if has_persistent {
            Some(self.catalog_io.lock().await)
        } else {
            None
        };
        let mut start = Vec::new();
        let mut changed = Vec::new();
        for extension_id in ids {
            let snapshot = {
                let inner = self.inner.lock().await;
                inner.definitions.get(&extension_id).cloned()
            };
            let Some(snapshot) = snapshot else {
                continue;
            };
            if snapshot.phase != EXT_PHASE_NEED_OBJECT || snapshot.hash != hash {
                continue;
            }
            if snapshot
                .pending_deadline
                .is_some_and(|deadline| Instant::now() >= deadline)
            {
                let mut inner = self.inner.lock().await;
                if let Some(definition) = inner.definitions.get_mut(&extension_id) {
                    definition.phase = EXT_PHASE_STOPPED;
                    definition.set_flag(EXT_FLAG_DESIRED_RUNNING, false);
                    definition.detail = "pending extension creation expired".into();
                    definition.pending_deadline = None;
                    definition.release_deadline = Some(Instant::now() + self.terminal_retain);
                    release_definition_arguments(definition);
                    changed.push(extension_id);
                }
                continue;
            }

            let admitted = if snapshot.persistent() {
                self.commit_catalog_create(&snapshot).await.map(Some)
            } else {
                let mut inner = self.inner.lock().await;
                let Some(mut definition) = inner.definitions.remove(&extension_id) else {
                    continue;
                };
                let admitted = commit_transient_create(&mut inner, &mut definition).map(|()| None);
                inner.definitions.insert(extension_id, definition);
                admitted
            };
            let mut inner = self.inner.lock().await;
            let shutting_down = inner.shutting_down;
            let Some(definition) = inner.definitions.get_mut(&extension_id) else {
                continue;
            };
            match admitted {
                Ok(persistent) => {
                    if let Some(persistent) = persistent {
                        definition.definition_revision = persistent.definition_revision;
                        definition.flags = persistent.flags;
                        definition.argument_bytes = persistent.argument_bytes;
                        definition.catalog_committed = true;
                        definition.object_pinned = true;
                        release_definition_arguments(definition);
                    }
                    definition.phase = if shutting_down {
                        EXT_PHASE_STOPPED
                    } else {
                        EXT_PHASE_QUEUED
                    };
                    definition.pending_deadline = None;
                    definition.release_deadline = None;
                    definition.detail.clear();
                    if !shutting_down {
                        start.push(extension_id);
                    }
                }
                Err((_, detail)) => {
                    definition.phase = EXT_PHASE_STOPPED;
                    definition.pending_deadline = None;
                    definition.release_deadline = Some(Instant::now() + self.terminal_retain);
                    definition.set_flag(EXT_FLAG_DESIRED_RUNNING, false);
                    definition.detail = bounded_detail(&detail);
                    release_definition_arguments(definition);
                }
            }
            changed.push(extension_id);
        }
        (start, changed)
    }

    /// Persist the latest complete LRU image without holding service state.
    /// Callers serialize this with `store_io` so eviction cannot pass a newer
    /// in-memory touch while an older snapshot is being published.
    async fn persist_store_lru(&self) -> Result<(), ObjectStoreError> {
        let _store_io = self.store_io.lock().await;
        self.persist_store_lru_in_lane().await
    }

    async fn persist_store_lru_in_lane(&self) -> Result<(), ObjectStoreError> {
        let snapshot = {
            let inner = self.inner.lock().await;
            inner
                .store
                .as_ref()
                .ok_or(ObjectStoreError::NotFound)?
                .lru_snapshot()?
        };
        let Some(snapshot) = snapshot else {
            return Ok(());
        };
        let (snapshot, persisted) = tokio::task::spawn_blocking(move || {
            let persisted = snapshot.persist();
            (snapshot, persisted)
        })
        .await
        .map_err(|error| ObjectStoreError::Io(std::io::Error::other(error.to_string())))?;
        persisted?;
        let mut inner = self.inner.lock().await;
        inner
            .store
            .as_mut()
            .ok_or(ObjectStoreError::NotFound)?
            .acknowledge_lru_snapshot(&snapshot);
        Ok(())
    }

    async fn handle_control(
        self: &Arc<Self>,
        state: super::AppState,
        endpoint: u64,
        nonce: u16,
        extension_id: u64,
        action: u8,
    ) {
        if matches!(
            action,
            EXT_CONTROL_CANCEL
                | EXT_CONTROL_RESTART
                | EXT_CONTROL_ENABLE
                | EXT_CONTROL_DISABLE
                | EXT_CONTROL_REMOVE
        ) {
            self.handle_mutating_control(state, endpoint, nonce, extension_id, action)
                .await;
            return;
        }
        let mut packets = Vec::new();
        {
            let mut inner = self.inner.lock().await;
            if action == EXT_CONTROL_LIST {
                let mut records = inner
                    .definitions
                    .values()
                    .map(extension_record)
                    .collect::<Vec<_>>();
                records.sort_by(|left, right| {
                    left.name
                        .as_bytes()
                        .cmp(right.name.as_bytes())
                        .then(left.extension_id.cmp(&right.extension_id))
                });
                packets.push(
                    wire::msg_extension_list(nonce, EXT_STATUS_OK, &records).unwrap_or_else(|| {
                        wire::msg_extension_list(nonce, EXT_STATUS_BUDGET, &[])
                            .expect("empty extension catalog response")
                    }),
                );
            } else {
                let Some(current) = inner.definitions.get(&extension_id).cloned() else {
                    let packet = fixed_status(
                        nonce,
                        EXT_STATUS_UNKNOWN_ID,
                        0,
                        0,
                        extension_id,
                        0,
                        [0; 32],
                        "extension ID does not exist",
                    );
                    if let Some(sender) = inner.endpoints.get(&endpoint) {
                        let _ = sender.send(packet);
                    }
                    return;
                };
                match action {
                    EXT_CONTROL_STATUS => packets.push(status_packet(
                        &current,
                        nonce,
                        EXT_STATUS_OK,
                        None,
                        &current.detail,
                    )),
                    EXT_CONTROL_ATTACH => {
                        let already_following = current.followers.contains_key(&endpoint);
                        if !already_following
                            && !follower_capacity_available(
                                &inner,
                                endpoint,
                                self.follow_max_per_endpoint,
                                self.follow_max,
                            )
                        {
                            packets.push(status_packet(
                                &current,
                                nonce,
                                EXT_STATUS_BUDGET,
                                None,
                                "extension follower capacity exhausted",
                            ));
                        } else if let Some(definition) = inner.definitions.get_mut(&extension_id) {
                            let oldest = oldest_replay_sequence(definition);
                            let cursor = definition
                                .followers
                                .get(&endpoint)
                                .map(|follower| follower.next_sequence)
                                .unwrap_or(oldest)
                                .max(oldest);
                            let through = definition.latest_output_sequence();
                            let replay_from =
                                next_replay_sequence(definition, cursor, through).unwrap_or(0);
                            let replay_through = definition
                                .followers
                                .get(&endpoint)
                                .and_then(|follower| follower.replay_through)
                                .map_or(through, |pending| pending.max(through));
                            definition.followers.insert(
                                endpoint,
                                FollowerCursor {
                                    next_sequence: cursor,
                                    replay_through: Some(replay_through),
                                },
                            );
                            packets.push(attach_status_packet(
                                definition,
                                nonce,
                                replay_from,
                                &definition.detail,
                            ));
                        }
                    }
                    EXT_CONTROL_UNFOLLOW => {
                        if let Some(definition) = inner.definitions.get_mut(&extension_id) {
                            definition.followers.remove(&endpoint);
                        }
                        packets.push(status_packet(&current, nonce, EXT_STATUS_OK, None, ""));
                    }
                    _ => packets.push(status_packet(
                        &current,
                        nonce,
                        EXT_STATUS_INVALID,
                        None,
                        "unknown extension control action",
                    )),
                }
            }
            if let Some(sender) = inner.endpoints.get(&endpoint) {
                for packet in packets.drain(..) {
                    if sender.send(packet).is_err() {
                        break;
                    }
                }
            }
            if action == EXT_CONTROL_ATTACH {
                wake_endpoint_locked(&inner, endpoint);
            }
        }
    }

    async fn handle_mutating_control(
        self: &Arc<Self>,
        state: super::AppState,
        endpoint: u64,
        nonce: u16,
        extension_id: u64,
        action: u8,
    ) {
        let initial = {
            let inner = self.inner.lock().await;
            inner.definitions.get(&extension_id).cloned()
        };
        let Some(initial) = initial else {
            self.send(
                endpoint,
                fixed_status(
                    nonce,
                    EXT_STATUS_UNKNOWN_ID,
                    0,
                    0,
                    extension_id,
                    0,
                    [0; 32],
                    "extension ID does not exist",
                ),
            )
            .await;
            return;
        };
        let serialize_catalog = initial.persistent();
        let _catalog_io = if serialize_catalog {
            Some(self.catalog_io.lock().await)
        } else {
            None
        };

        let current = {
            let mut inner = self.inner.lock().await;
            let Some(current) = inner.definitions.get(&extension_id).cloned() else {
                drop(inner);
                self.send(
                    endpoint,
                    fixed_status(
                        nonce,
                        EXT_STATUS_UNKNOWN_ID,
                        0,
                        0,
                        extension_id,
                        0,
                        [0; 32],
                        "extension ID does not exist",
                    ),
                )
                .await;
                return;
            };
            let invalid = match action {
                EXT_CONTROL_RESTART if current.persistent() && !self.persist_allowed => Some((
                    EXT_STATUS_PERMISSION,
                    "persistent extensions are disabled on this server",
                )),
                EXT_CONTROL_RESTART
                    if current
                        .owner_endpoint
                        .is_some_and(|owner| !inner.endpoints.contains_key(&owner)) =>
                {
                    Some((
                        EXT_STATUS_CONFLICT,
                        "attached extension owner is no longer connected",
                    ))
                }
                EXT_CONTROL_RESTART if !current.enabled() => {
                    Some((EXT_STATUS_CONFLICT, "extension is disabled"))
                }
                EXT_CONTROL_ENABLE if !current.persistent() || !self.persist_allowed => Some((
                    EXT_STATUS_PERMISSION,
                    "enable requires persistent-extension permission",
                )),
                EXT_CONTROL_DISABLE if !current.persistent() => Some((
                    EXT_STATUS_PERMISSION,
                    "disable requires a persistent extension",
                )),
                EXT_CONTROL_REMOVE if !current.persistent() => Some((
                    EXT_STATUS_PERMISSION,
                    "remove requires a persistent extension",
                )),
                EXT_CONTROL_REMOVE
                    if current.enabled()
                        || current.control.is_some()
                        || !matches!(current.phase, EXT_PHASE_STOPPED | EXT_PHASE_BLOCKED) =>
                {
                    Some((
                        EXT_STATUS_CONFLICT,
                        "extension must be disabled and quiescent before removal",
                    ))
                }
                _ => None,
            };
            if let Some((status, detail)) = invalid {
                let packet = status_packet(&current, nonce, status, None, detail);
                if let Some(sender) = inner.endpoints.get(&endpoint) {
                    let _ = sender.send(packet);
                }
                return;
            }
            if matches!(action, EXT_CONTROL_RESTART | EXT_CONTROL_ENABLE)
                && current.persistent()
                && let Err(error) = repair_persistent_pin(&mut inner, &current)
            {
                let packet = status_packet(
                    &current,
                    nonce,
                    catalog_status(&error),
                    None,
                    &error.to_string(),
                );
                if let Some(sender) = inner.endpoints.get(&endpoint) {
                    let _ = sender.send(packet);
                }
                return;
            }
            current
        };
        let write_catalog = current.catalog_committed;

        let persisted = if write_catalog {
            if action == EXT_CONTROL_REMOVE {
                self.catalog_call(move |catalog| catalog.remove(extension_id).map(|_| ()))
                    .await
            } else {
                let (enabled, desired) = match action {
                    EXT_CONTROL_CANCEL => (None, Some(false)),
                    EXT_CONTROL_RESTART => (None, Some(true)),
                    EXT_CONTROL_ENABLE => (Some(true), None),
                    EXT_CONTROL_DISABLE => (Some(false), None),
                    _ => unreachable!(),
                };
                self.catalog_call(move |catalog| {
                    catalog
                        .set_lifecycle(
                            extension_id,
                            enabled,
                            desired,
                            None,
                            None,
                            None,
                            Some(0),
                            Some(BlockedState::Clear),
                        )
                        .map(|_| ())
                })
                .await
            }
        } else {
            Ok(())
        };

        let mut cancel = None;
        let mut start = None;
        {
            let mut inner = self.inner.lock().await;
            let persisted_ok = persisted.is_ok();
            let packet = match persisted {
                Err(error) => status_packet(
                    &current,
                    nonce,
                    catalog_status(&error),
                    None,
                    &error.to_string(),
                ),
                Ok(()) if action == EXT_CONTROL_REMOVE => {
                    remove_definition_locked(&mut inner, extension_id);
                    fixed_status(
                        nonce,
                        EXT_STATUS_OK,
                        0,
                        0,
                        extension_id,
                        0,
                        [0; 32],
                        "removed",
                    )
                }
                Ok(()) => {
                    let (enabled, desired, interrupt) = match action {
                        EXT_CONTROL_CANCEL => (None, Some(false), Interrupt::Cancelled),
                        EXT_CONTROL_RESTART => (None, Some(true), Interrupt::Restarted),
                        EXT_CONTROL_ENABLE => (Some(true), None, Interrupt::Restarted),
                        EXT_CONTROL_DISABLE => (Some(false), None, Interrupt::Disabled),
                        _ => unreachable!(),
                    };
                    if mutate_lifecycle_locked(
                        &mut inner,
                        extension_id,
                        enabled,
                        desired,
                        interrupt,
                        self.terminal_retain,
                    )
                    .is_err()
                    {
                        fixed_status(
                            nonce,
                            EXT_STATUS_UNKNOWN_ID,
                            0,
                            0,
                            extension_id,
                            0,
                            [0; 32],
                            "extension ID does not exist",
                        )
                    } else if let Some(definition) = inner.definitions.get(&extension_id) {
                        cancel = definition.control.clone();
                        if matches!(action, EXT_CONTROL_RESTART | EXT_CONTROL_ENABLE)
                            && definition.enabled()
                            && definition.desired()
                            && cancel.is_none()
                        {
                            start = Some(extension_id);
                        }
                        status_packet(definition, nonce, EXT_STATUS_OK, None, "")
                    } else {
                        fixed_status(
                            nonce,
                            EXT_STATUS_UNKNOWN_ID,
                            0,
                            0,
                            extension_id,
                            0,
                            [0; 32],
                            "extension ID does not exist",
                        )
                    }
                }
            };
            if let Some(sender) = inner.endpoints.get(&endpoint) {
                let _ = sender.send(packet);
            }
            if persisted_ok && action != EXT_CONTROL_REMOVE {
                emit_lifecycle_locked(&mut inner, extension_id, self.output_retain_max);
            }
        }
        drop(_catalog_io);
        if let Some(control) = cancel {
            control.connection.cancel();
            control.host.cancel();
        }
        if let Some(extension_id) = start {
            self.ensure_supervisor(state, extension_id).await;
        }
    }

    async fn handle_event(&self, origin: &super::ConnectionOrigin, kind: u8, data: &[u8]) -> bool {
        let Some((extension_id, revision, attempt, task_id)) = origin_identity(origin) else {
            return false;
        };
        let mut inner = self.inner.lock().await;
        let Some(definition) = inner.definitions.get(&extension_id) else {
            return false;
        };
        let valid = definition.control.as_ref().is_some_and(|control| {
            control.definition_revision == revision
                && control.attempt == attempt
                && control.task_id == task_id
        });
        if !valid {
            return false;
        }
        let sequence = definition.next_output_sequence;
        let event = wire::msg_extension_output_event(&ExtensionOutputEvent {
            extension_id,
            definition_revision: revision,
            attempt,
            task_id,
            output_sequence: sequence,
            kind,
            data,
        });
        let Some(packet) = event else {
            return false;
        };
        retain_and_fanout(&mut inner, extension_id, packet, self.output_retain_max);
        true
    }

    #[allow(clippy::too_many_arguments)]
    async fn handle_command_register(
        &self,
        state: super::AppState,
        endpoint: u64,
        origin: &super::ConnectionOrigin,
        nonce: u16,
        listener_id: u32,
        descriptor: &str,
    ) {
        let identity = origin_identity(origin);
        let endpoint_generation = state.boot_generation;
        let captured_listener = if listener_id == 0 {
            None
        } else {
            let session = state.session.lock().await;
            session
                .channels
                .listener_snapshot(endpoint, listener_id)
                .map(|listener| command_listener(endpoint_generation, listener))
        };
        let (prepared, fallback_id, fallback_revision) = {
            let inner = self.inner.lock().await;
            let owner = identity.and_then(|identity| {
                command_owner(&inner, endpoint, endpoint_generation, identity)
            });
            let fallback = owner
                .as_ref()
                .map(|owner| (owner.extension_id, owner.definition_revision))
                .or_else(|| identity.map(|identity| (identity.0, identity.1)))
                .unwrap_or((0, 0));
            (
                inner.commands.prepare_registration(
                    owner.as_ref(),
                    listener_id,
                    descriptor,
                    captured_listener.as_ref(),
                ),
                fallback.0,
                fallback.1,
            )
        };
        let prepared = match prepared {
            Ok(prepared) => prepared,
            Err(error) => {
                self.send(
                    endpoint,
                    command_registered(
                        nonce,
                        error.status(),
                        fallback_id,
                        fallback_revision,
                        error.detail(),
                    ),
                )
                .await;
                return;
            }
        };

        // Hold the channel-registry view through the publication recheck.
        let session = state.session.lock().await;
        let current_listener = if listener_id == 0 {
            None
        } else {
            session
                .channels
                .listener_snapshot(endpoint, listener_id)
                .map(|listener| command_listener(endpoint_generation, listener))
        };
        let (status, extension_id, revision, detail) = {
            let mut inner = self.inner.lock().await;
            let current_owner = identity.and_then(|identity| {
                command_owner(&inner, endpoint, endpoint_generation, identity)
            });
            match inner.commands.commit_registration(
                prepared.clone(),
                current_owner.as_ref(),
                current_listener.as_ref(),
            ) {
                Ok(result) => (
                    EXT_STATUS_OK,
                    result.extension_id,
                    result.definition_revision,
                    "",
                ),
                Err(error) => (
                    error.status(),
                    prepared.extension_id(),
                    prepared.definition_revision(),
                    error.detail(),
                ),
            }
        };
        drop(session);
        self.send(
            endpoint,
            command_registered(nonce, status, extension_id, revision, detail),
        )
        .await;
    }

    async fn handle_command_discover(
        &self,
        endpoint: u64,
        nonce: u16,
        directory_revision: u64,
        cursor: u64,
    ) {
        let page = self.inner.lock().await.commands.discover(
            endpoint,
            directory_revision,
            cursor,
            Instant::now(),
        );
        self.send(endpoint, command_page(nonce, &page)).await;
    }

    pub(crate) async fn invalidate_command_listener(
        &self,
        endpoint_generation: u64,
        listener: crate::channel::ListenerSnapshot,
    ) {
        self.inner
            .lock()
            .await
            .commands
            .invalidate_listener(&command_listener(endpoint_generation, listener));
    }

    async fn reply_decode_error(
        &self,
        endpoint: u64,
        packet: &[u8],
        error: &wire::ExtensionDecodeError,
    ) {
        let detail = error.to_string();
        let reply = match packet.first().copied() {
            Some(wire::EXT_EVENT) => return,
            Some(wire::EXT_RUN) => {
                let nonce = packet_u16(packet, 1);
                let hash = packet_hash(packet, 21);
                run_error_status(nonce, EXT_STATUS_INVALID, hash, &detail)
            }
            Some(wire::EXT_PUT) => put_status(
                packet_u16(packet, 1),
                EXT_STATUS_INVALID,
                packet_hash(packet, 4),
                0,
                &detail,
            ),
            Some(wire::EXT_CONTROL) => fixed_status(
                packet_u16(packet, 1),
                EXT_STATUS_INVALID,
                0,
                0,
                packet_u64(packet, 3),
                0,
                [0; 32],
                &detail,
            ),
            Some(wire::EXT_COMMAND) if packet.get(1) == Some(&wire::EXT_COMMAND_REGISTER) => {
                command_registered(packet_u16(packet, 2), EXT_STATUS_INVALID, 0, 0, &detail)
            }
            Some(wire::EXT_COMMAND) => {
                wire::msg_extension_commands(packet_u16(packet, 2), EXT_STATUS_INVALID, 0, 0, &[])
                    .expect("empty invalid command response")
            }
            _ => return,
        };
        self.send(endpoint, reply).await;
    }

    async fn reply_disabled(&self, endpoint: u64, request: ExtensionRequest<'_>) {
        let detail = self
            .inner
            .lock()
            .await
            .diagnostic
            .clone()
            .unwrap_or_else(|| "extensions are disabled by server policy".into());
        let packet = match request {
            ExtensionRequest::Run { nonce, hash, .. } => {
                run_error_status(nonce, EXT_STATUS_PERMISSION, hash, &detail)
            }
            ExtensionRequest::Control {
                nonce,
                extension_id: _,
                action: EXT_CONTROL_LIST,
            } => wire::msg_extension_list(nonce, EXT_STATUS_PERMISSION, &[])
                .expect("empty disabled extension list response"),
            ExtensionRequest::Control {
                nonce,
                extension_id,
                ..
            } => fixed_status(
                nonce,
                EXT_STATUS_PERMISSION,
                0,
                0,
                extension_id,
                0,
                [0; 32],
                &detail,
            ),
            ExtensionRequest::Put { nonce, hash, .. } => {
                put_status(nonce, EXT_STATUS_PERMISSION, hash, 0, &detail)
            }
            ExtensionRequest::CommandRegister { nonce, .. } => {
                wire::msg_extension_command_registered(&wire::ExtensionCommandRegistered {
                    nonce,
                    status: EXT_STATUS_PERMISSION,
                    extension_id: 0,
                    definition_revision: 0,
                    detail: &detail,
                })
                .expect("bounded disabled command response")
            }
            ExtensionRequest::CommandDiscover { nonce, .. } => {
                wire::msg_extension_commands(nonce, EXT_STATUS_PERMISSION, 0, 0, &[])
                    .expect("bounded disabled command directory response")
            }
            ExtensionRequest::Event { .. } => return,
        };
        self.send(endpoint, packet).await;
    }

    async fn send(&self, endpoint: u64, packet: Vec<u8>) {
        let sender = self.inner.lock().await.endpoints.get(&endpoint).cloned();
        if let Some(sender) = sender {
            let _ = sender.send(packet);
        }
    }
}

impl ExtensionService {
    async fn acquire_running_permit(
        &self,
        extension_id: u64,
        queued_generation: u64,
        wake: Arc<Notify>,
    ) -> Option<tokio::sync::OwnedSemaphorePermit> {
        let acquire = self.running.clone().acquire_owned();
        tokio::pin!(acquire);
        loop {
            tokio::select! {
                permit = &mut acquire => return permit.ok(),
                _ = wake.notified() => {}
                _ = tokio::time::sleep(Duration::from_secs(1)) => {}
            }
            let inner = self.inner.lock().await;
            let still_queued = !inner.shutting_down
                && inner
                    .definitions
                    .get(&extension_id)
                    .is_some_and(|definition| {
                        definition.generation == queued_generation
                            && definition.enabled()
                            && definition.desired()
                    });
            if !still_queued {
                return None;
            }
        }
    }

    async fn ensure_supervisor(self: &Arc<Self>, state: super::AppState, extension_id: u64) {
        let completion = {
            let mut inner = self.inner.lock().await;
            let eligible = inner
                .definitions
                .get(&extension_id)
                .is_some_and(|definition| {
                    definition.enabled()
                        && definition.desired()
                        && definition.phase != EXT_PHASE_NEED_OBJECT
                });
            if !inner.shutting_down && eligible && inner.supervisors.insert(extension_id) {
                let completion = SupervisorCompletion::new();
                let completions = inner
                    .supervisor_completions
                    .entry(extension_id)
                    .or_default();
                completions.retain(|completion| !completion.is_complete());
                completions.push(Arc::clone(&completion));
                Some(completion)
            } else {
                None
            }
        };
        if let Some(completion) = completion {
            let service = Arc::clone(self);
            tokio::spawn(async move {
                let guard = SupervisorCompletionGuard(Arc::clone(&completion));
                Arc::clone(&service).supervise(state, extension_id).await;
                completion.complete();
                let mut inner = service.inner.lock().await;
                let remove_entry = if let Some(completions) =
                    inner.supervisor_completions.get_mut(&extension_id)
                {
                    completions.retain(|current| !Arc::ptr_eq(current, &completion));
                    completions.is_empty()
                } else {
                    false
                };
                if remove_entry {
                    inner.supervisor_completions.remove(&extension_id);
                }
                drop(guard);
            });
        }
    }

    async fn supervise(self: Arc<Self>, state: super::AppState, extension_id: u64) {
        loop {
            let terminal = {
                let mut inner = self.inner.lock().await;
                let shutting_down = inner.shutting_down;
                let Some(definition) = inner.definitions.get_mut(&extension_id) else {
                    inner.supervisors.remove(&extension_id);
                    return;
                };
                if shutting_down
                    || !definition.enabled()
                    || !definition.desired()
                    || definition.phase == EXT_PHASE_NEED_OBJECT
                {
                    if definition.phase != EXT_PHASE_NEED_OBJECT {
                        definition.phase = EXT_PHASE_STOPPED;
                        definition.task_id = 0;
                        definition.next_start_unix_ms = 0;
                    }
                    Some((
                        shutting_down,
                        definition.persistent(),
                        definition.generation,
                        Arc::clone(&definition.wake),
                    ))
                } else {
                    definition.phase = EXT_PHASE_QUEUED;
                    definition.task_id = 0;
                    definition.next_start_unix_ms = 0;
                    None
                }
            };
            if let Some((shutting_down, persistent, generation, wake)) = terminal {
                if shutting_down {
                    let mut inner = self.inner.lock().await;
                    inner.supervisors.remove(&extension_id);
                    if !persistent {
                        remove_definition_locked(&mut inner, extension_id);
                    }
                    return;
                }
                if persistent {
                    self.inner.lock().await.supervisors.remove(&extension_id);
                    return;
                }
                tokio::select! {
                    _ = tokio::time::sleep(self.terminal_retain) => {
                        self.release_transient(extension_id, generation, true).await;
                        return;
                    }
                    _ = wake.notified() => continue,
                }
            }
            {
                let mut inner = self.inner.lock().await;
                emit_lifecycle_locked(&mut inner, extension_id, self.output_retain_max);
            }
            let Some((queued_generation, wake)) = self
                .inner
                .lock()
                .await
                .definitions
                .get(&extension_id)
                .map(|definition| (definition.generation, Arc::clone(&definition.wake)))
            else {
                break;
            };
            let permit = self
                .acquire_running_permit(extension_id, queued_generation, wake)
                .await;
            let Some(permit) = permit else {
                continue;
            };

            let validation = match self.validating.clone().acquire_owned().await {
                Ok(permit) => permit,
                Err(_) => break,
            };
            let prepared = self.prepare_attempt(extension_id).await;
            let (
                mut attempt,
                generation,
                attempt_number,
                name,
                args,
                flags,
                revision,
                hash,
                loaded_argument_reservation,
            ) = match prepared {
                Ok(value) => value,
                Err(PrepareAttemptError::ArgumentBudget(wake)) => {
                    // Contention is admission pressure, not an attempt or a
                    // durable failure. Release execution permits before
                    // waiting so resident transient work can drain.
                    drop(validation);
                    drop(permit);
                    tokio::select! {
                        _ = self.argument_budget.notify.notified() => {}
                        _ = wake.notified() => {}
                        _ = tokio::time::sleep(Duration::from_secs(1)) => {}
                    }
                    continue;
                }
                Err(PrepareAttemptError::Superseded) => {
                    drop(validation);
                    drop(permit);
                    continue;
                }
                Err(PrepareAttemptError::Failed(error)) => {
                    self.block_definition(extension_id, error).await;
                    drop(validation);
                    drop(permit);
                    if self.wait_blocked_or_restart(extension_id).await {
                        continue;
                    }
                    return;
                }
            };

            let connection = super::ConnectionCancellation::default();
            let host = attempt.cancellation();
            let preparation_installed = {
                let mut inner = self.inner.lock().await;
                let valid = !inner.shutting_down
                    && inner
                        .definitions
                        .get(&extension_id)
                        .is_some_and(|definition| {
                            definition.generation == generation
                                && definition.definition_revision == revision
                                && definition.enabled()
                                && definition.desired()
                        });
                if valid && let Some(definition) = inner.definitions.get_mut(&extension_id) {
                    definition.control = Some(AttemptControl {
                        definition_revision: revision,
                        attempt: attempt_number,
                        task_id: 0,
                        host: host.clone(),
                        connection: connection.clone(),
                    });
                    definition.interrupt = None;
                }
                valid
            };
            if !preparation_installed {
                attempt.cancel();
                let _ = attempt.join().await;
                drop(validation);
                drop(permit);
                continue;
            }

            if let Err(error) = attempt.wait_prepared().await {
                attempt.cancel();
                let _ = attempt.join().await;
                self.block_definition(extension_id, error).await;
                drop(validation);
                drop(permit);
                if self.wait_blocked_or_restart(extension_id).await {
                    continue;
                }
                return;
            }
            drop(validation);

            let task_id = {
                let mut inner = self.inner.lock().await;
                let Some(current) = inner.definitions.get(&extension_id) else {
                    attempt.cancel();
                    drop(inner);
                    let _ = attempt.join().await;
                    drop(permit);
                    break;
                };
                if inner.shutting_down
                    || current.generation != generation
                    || !current.enabled()
                    || !current.desired()
                    || !current.control.as_ref().is_some_and(|control| {
                        control.definition_revision == revision
                            && control.attempt == attempt_number
                            && control.task_id == 0
                    })
                {
                    attempt.cancel();
                    drop(inner);
                    let _ = attempt.join().await;
                    drop(permit);
                    continue;
                }
                let Some(task_id) = allocate_task_id(&inner) else {
                    attempt.cancel();
                    drop(inner);
                    let _ = attempt.join().await;
                    drop(permit);
                    self.block_definition(
                        extension_id,
                        AttemptFailure {
                            kind: FailureKind::HostFailure,
                            detail: "could not allocate a task ID".into(),
                        },
                    )
                    .await;
                    return;
                };
                inner.task_ids.insert(task_id);
                if let Some(definition) = inner.definitions.get_mut(&extension_id)
                    && let Some(control) = definition.control.as_mut()
                {
                    control.task_id = task_id;
                }
                Ok(task_id)
            };
            let task_id = match task_id {
                Ok(task_id) => task_id,
                Err(error) => {
                    attempt.cancel();
                    let _ = attempt.join().await;
                    drop(permit);
                    self.block_definition(extension_id, error).await;
                    return;
                }
            };

            let init_args = args.iter().map(Vec::as_slice).collect::<Vec<_>>();
            let init = wire::ExtensionInit {
                extension_id,
                definition_revision: revision,
                attempt: attempt_number,
                task_id,
                flags,
                hash,
                name: &name,
                args: init_args,
            };
            let (init_reserved_tx, init_reserved_rx) = oneshot::channel();
            let (commit_init_tx, commit_init_rx) = oneshot::channel();
            let options = super::ConnectionOptions::extension_with_barrier(
                &init,
                connection.clone(),
                Some(super::ExtensionBootstrapBarrier {
                    init_reserved: init_reserved_tx,
                    commit_init: commit_init_rx,
                }),
            );
            // ConnectionOptions now owns the serialized INIT. The durable
            // argument load and its reservation are no longer needed.
            drop(init);
            drop(args);
            drop(loaded_argument_reservation);
            let publication = AttemptPublication {
                service: Arc::clone(&self),
                extension_id,
                generation,
                definition_revision: revision,
                attempt: attempt_number,
                task_id,
            };
            let driven = drive_attempt(
                state.clone(),
                options,
                init_reserved_rx,
                commit_init_tx,
                attempt,
                connection.clone(),
                publication,
            )
            .await;
            let running_for = driven.running_for;

            let decision = self
                .finish_attempt(
                    extension_id,
                    generation,
                    revision,
                    attempt_number,
                    task_id,
                    driven,
                    running_for,
                )
                .await;
            drop(permit);

            match decision {
                NextAttempt::Stop => {
                    let terminal = {
                        let mut inner = self.inner.lock().await;
                        inner.supervisors.remove(&extension_id);
                        inner.definitions.get(&extension_id).map(|definition| {
                            (
                                definition.persistent(),
                                definition.generation,
                                Arc::clone(&definition.wake),
                            )
                        })
                    };
                    let Some((persistent, generation, wake)) = terminal else {
                        // Owner-loss teardown removes the transient definition
                        // synchronously. Never extend the recursive cleanup
                        // barrier merely to wait out its replay lease.
                        return;
                    };
                    if persistent {
                        return;
                    }
                    tokio::select! {
                        _ = tokio::time::sleep(self.terminal_retain) => {
                            self.release_transient(extension_id, generation, true).await;
                        }
                        _ = wake.notified() => {}
                    }
                    return;
                }
                NextAttempt::Immediate => continue,
                NextAttempt::Backoff { duration, wake } => {
                    tokio::select! {
                        _ = tokio::time::sleep(duration) => {}
                        _ = wake.notified() => {}
                    }
                }
            }
        }
        self.inner.lock().await.supervisors.remove(&extension_id);
    }

    async fn prepare_attempt(
        &self,
        extension_id: u64,
    ) -> Result<PreparedAttempt, PrepareAttemptError> {
        let (snapshot, loaded_argument_reservation, attempt_number, object_read) = {
            let mut inner = self.inner.lock().await;
            let snapshot = inner
                .definitions
                .get(&extension_id)
                .cloned()
                .ok_or_else(|| AttemptFailure {
                    kind: FailureKind::HostFailure,
                    detail: "extension disappeared before validation".into(),
                })?;
            if !snapshot.enabled() || !snapshot.desired() {
                return Err(PrepareAttemptError::Superseded);
            }
            let loaded_argument_reservation = if snapshot.persistent() && snapshot.catalog_committed
            {
                if snapshot.argument_bytes > self.argument_budget.max {
                    return Err(AttemptFailure {
                        kind: FailureKind::HostFailure,
                        detail: "persistent extension arguments exceed the argument-store budget"
                            .into(),
                    }
                    .into());
                }
                Some(
                    self.argument_budget
                        .try_reserve(snapshot.argument_bytes)
                        .ok_or_else(|| {
                            PrepareAttemptError::ArgumentBudget(Arc::clone(&snapshot.wake))
                        })?,
                )
            } else {
                None
            };
            let attempt_number = snapshot
                .attempt
                .checked_add(1)
                .ok_or_else(|| AttemptFailure {
                    kind: FailureKind::HostFailure,
                    detail: "extension attempt counter exhausted".into(),
                })?;
            let object_read = inner
                .store
                .as_mut()
                .ok_or(ObjectStoreError::NotFound)
                .and_then(|store| store.reserve_read(&snapshot.hash))
                .map_err(|error| AttemptFailure {
                    kind: FailureKind::Validation,
                    detail: error.to_string(),
                })?;
            (
                snapshot,
                loaded_argument_reservation,
                attempt_number,
                object_read,
            )
        };
        let args = {
            let _catalog_io = if snapshot.args.is_none() {
                Some(self.catalog_io.lock().await)
            } else {
                None
            };
            self.definition_arguments(&snapshot)
                .await
                .map_err(|error| AttemptFailure {
                    kind: FailureKind::HostFailure,
                    detail: error.to_string(),
                })?
        };
        let module = match tokio::task::block_in_place(|| object_read.read_verified()) {
            Ok(module) => module,
            Err(error) => {
                let missing = matches!(
                    &error,
                    ObjectStoreError::Io(io) if io.kind() == std::io::ErrorKind::NotFound
                );
                let corrupt = matches!(&error, ObjectStoreError::HashMismatch);
                let removed = missing || (corrupt && object_read.remove_file().is_ok());
                let mut inner = self.inner.lock().await;
                if removed {
                    if let Some(store) = inner.store.as_mut() {
                        store.forget_removed(&snapshot.hash);
                    }
                    mark_hash_unpinned(&mut inner, &snapshot.hash);
                }
                return Err(AttemptFailure {
                    kind: FailureKind::Validation,
                    detail: error.to_string(),
                }
                .into());
            }
        };
        self.persist_store_lru()
            .await
            .map_err(|error| AttemptFailure {
                kind: FailureKind::Validation,
                detail: error.to_string(),
            })?;
        let _catalog_io = if snapshot.persistent() {
            Some(self.catalog_io.lock().await)
        } else {
            None
        };
        let still_current = {
            let inner = self.inner.lock().await;
            !inner.shutting_down
                && inner
                    .definitions
                    .get(&extension_id)
                    .is_some_and(|definition| {
                        definition.generation == snapshot.generation
                            && definition.definition_revision == snapshot.definition_revision
                            && definition.hash == snapshot.hash
                            && definition.attempt == snapshot.attempt
                            && definition.enabled()
                            && definition.desired()
                    })
        };
        if !still_current {
            return Err(PrepareAttemptError::Superseded);
        }
        self.persist_attempt_counters_catalog(
            extension_id,
            attempt_number,
            snapshot.last_running_attempt,
            snapshot.persistent(),
        )
        .await
        .map_err(|error| AttemptFailure {
            kind: FailureKind::HostFailure,
            detail: error.to_string(),
        })?;
        let mut inner = self.inner.lock().await;
        let still_current = !inner.shutting_down
            && inner
                .definitions
                .get(&extension_id)
                .is_some_and(|definition| {
                    definition.generation == snapshot.generation
                        && definition.definition_revision == snapshot.definition_revision
                        && definition.hash == snapshot.hash
                        && definition.attempt == snapshot.attempt
                        && definition.enabled()
                        && definition.desired()
                });
        if !still_current {
            return Err(PrepareAttemptError::Superseded);
        }
        if let Some(definition) = inner.definitions.get_mut(&extension_id) {
            definition.phase = EXT_PHASE_VALIDATING;
            definition.attempt = attempt_number;
            definition.task_id = 0;
            definition.detail.clear();
        }
        emit_lifecycle_locked(&mut inner, extension_id, self.output_retain_max);
        drop(inner);
        drop(_catalog_io);

        let label = (!snapshot.name.is_empty()).then_some(snapshot.name.clone());
        let attempt = if is_wasm_module(&module) {
            RuntimeAttempt::Wasmi(
                wasmi_host::spawn_attempt(WasmiAttemptSpec {
                    module: Arc::<[u8]>::from(module),
                    module_hash: snapshot.hash,
                    extension_id,
                    label,
                    config: self.host_config.clone(),
                })
                .map_err(|error| AttemptFailure {
                    kind: FailureKind::HostFailure,
                    detail: error.to_string(),
                })?,
            )
        } else {
            RuntimeAttempt::QuickJs(
                quickjs_host::spawn_attempt(quickjs_host::AttemptSpec {
                    source: Arc::<[u8]>::from(module),
                    module_hash: snapshot.hash,
                    extension_id,
                    label,
                    config: self.host_config.clone(),
                })
                .map_err(|error| AttemptFailure {
                    kind: FailureKind::HostFailure,
                    detail: error.to_string(),
                })?,
            )
        };
        if std::env::var_os("BLIT_EXT_THREAD_DEBUG").is_some() {
            eprintln!(
                "blit-server: prepared extension thread {} ({})",
                attempt.thread_names().logical,
                attempt.thread_names().os
            );
        }
        Ok((
            attempt,
            snapshot.generation,
            attempt_number,
            snapshot.name,
            args,
            snapshot.flags,
            snapshot.definition_revision,
            snapshot.hash,
            loaded_argument_reservation,
        ))
    }

    async fn block_definition(&self, extension_id: u64, error: AttemptFailure) {
        let persistent = {
            let inner = self.inner.lock().await;
            inner
                .definitions
                .get(&extension_id)
                .is_some_and(Definition::persistent)
        };
        let _catalog_io = if persistent {
            Some(self.catalog_io.lock().await)
        } else {
            None
        };
        let (should_block, persistent) = {
            let inner = self.inner.lock().await;
            let shutting_down = inner.shutting_down;
            (
                inner
                    .definitions
                    .get(&extension_id)
                    .is_some_and(|definition| {
                        !shutting_down && definition.enabled() && definition.desired()
                    }),
                inner
                    .definitions
                    .get(&extension_id)
                    .is_some_and(Definition::persistent),
            )
        };
        let mut detail = bounded_detail(&error.detail);
        if should_block && persistent {
            let durable_detail = detail.clone();
            let persisted = self
                .catalog_call(move |catalog| {
                    catalog.set_lifecycle(
                        extension_id,
                        None,
                        None,
                        None,
                        None,
                        None,
                        Some(0),
                        Some(BlockedState::Set(&durable_detail)),
                    )
                })
                .await;
            if let Err(persist_error) = persisted {
                detail = bounded_detail(&format!(
                    "{detail}; could not persist blocked state: {persist_error}"
                ));
            }
        }
        let mut inner = self.inner.lock().await;
        if let Some(definition) = inner.definitions.get_mut(&extension_id) {
            definition.phase = if should_block {
                EXT_PHASE_BLOCKED
            } else {
                EXT_PHASE_STOPPED
            };
            definition.task_id = 0;
            definition.control = None;
            definition.next_start_unix_ms = 0;
            definition.detail = detail;
        }
        emit_lifecycle_locked(&mut inner, extension_id, self.output_retain_max);
    }

    async fn wait_blocked_or_restart(&self, extension_id: u64) -> bool {
        let Some((persistent, generation, wake, owner_endpoint)) = ({
            let mut inner = self.inner.lock().await;
            let state = inner.definitions.get(&extension_id).map(|definition| {
                (
                    definition.persistent(),
                    definition.generation,
                    Arc::clone(&definition.wake),
                    definition.owner_endpoint,
                )
            });
            if state
                .as_ref()
                .is_some_and(|(persistent, _, _, _)| *persistent)
            {
                inner.supervisors.remove(&extension_id);
            }
            state
        }) else {
            return false;
        };
        if persistent {
            return false;
        }
        if let Some(owner_endpoint) = owner_endpoint {
            let owner_live = self
                .inner
                .lock()
                .await
                .endpoints
                .contains_key(&owner_endpoint);
            if !owner_live {
                self.release_transient(extension_id, generation, false)
                    .await;
                return false;
            }
            wake.notified().await;
            let state = self
                .inner
                .lock()
                .await
                .definitions
                .get(&extension_id)
                .map(|definition| {
                    (
                        definition.enabled() && definition.desired(),
                        definition.generation,
                    )
                });
            if let Some((true, _)) = state {
                return true;
            }
            if let Some((false, current_generation)) = state {
                self.release_transient(extension_id, current_generation, false)
                    .await;
            }
            return false;
        }
        tokio::select! {
            _ = tokio::time::sleep(self.terminal_retain) => {
                self.release_transient(extension_id, generation, true).await;
                false
            }
            _ = wake.notified() => {
                let inner = self.inner.lock().await;
                let state = inner.definitions.get(&extension_id).map(|definition| {
                    (
                        definition.enabled() && definition.desired(),
                        definition.generation,
                    )
                });
                drop(inner);
                if let Some((false, current_generation)) = state {
                    self.release_transient(extension_id, current_generation, false)
                        .await;
                }
                state.is_some_and(|(eligible, _)| eligible)
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    async fn finish_attempt(
        &self,
        extension_id: u64,
        generation: u64,
        attempt_revision: u64,
        attempt_number: u64,
        task_id: u32,
        driven: DrivenAttempt,
        running_for: Duration,
    ) -> NextAttempt {
        let persistent = {
            let inner = self.inner.lock().await;
            inner
                .definitions
                .get(&extension_id)
                .is_some_and(Definition::persistent)
        };
        let _catalog_io = if persistent {
            Some(self.catalog_io.lock().await)
        } else {
            None
        };
        let mut inner = self.inner.lock().await;
        inner.task_ids.remove(&task_id);
        let Some(mut definition) = inner.definitions.remove(&extension_id) else {
            return NextAttempt::Stop;
        };
        let visible_definition = definition.clone();
        inner
            .commands
            .invalidate_attempt(extension_id, attempt_revision, attempt_number);
        definition.control = None;
        definition.task_id = 0;
        definition.next_start_unix_ms = 0;
        let interrupt = definition.interrupt.take();
        let (mut reason, mut code, mut detail, failure) = classify_outcome(&driven, interrupt);
        if running_for >= Duration::from_secs(60) {
            definition.failure_count = 0;
        }
        if failure {
            definition.failure_count = definition.failure_count.saturating_add(1);
        } else if reason == EXT_EXIT_RETURNED && code == 0 {
            definition.failure_count = 0;
        }

        let explicit_replace = matches!(interrupt, Some(Interrupt::Updated | Interrupt::Restarted));
        let suppressed = matches!(
            interrupt,
            Some(
                Interrupt::Cancelled
                    | Interrupt::Disabled
                    | Interrupt::OwnerClosed
                    | Interrupt::ServerShutdown
            )
        );
        let automatic = !suppressed
            && definition.enabled()
            && definition.desired()
            && (definition.restart == EXT_RESTART_ALWAYS
                || failure && definition.restart == EXT_RESTART_ON_FAILURE);
        let mut restart = explicit_replace || automatic;
        let mut backoff = restart && !explicit_replace;
        let mut duration = if backoff {
            backoff_duration(definition.failure_count.max(1))
        } else {
            Duration::ZERO
        };
        if restart {
            if backoff {
                definition.phase = EXT_PHASE_BACKOFF;
                definition.next_start_unix_ms = unix_millis_after(duration);
            } else {
                definition.phase = EXT_PHASE_QUEUED;
            }
        } else {
            definition.phase = EXT_PHASE_STOPPED;
            if !suppressed && !explicit_replace {
                definition.set_flag(EXT_FLAG_DESIRED_RUNNING, false);
            }
        }
        definition.detail = bounded_detail(&detail);
        if definition.generation == generation && explicit_replace {
            definition.generation = definition.generation.saturating_add(1);
        }
        let persisted = if persistent {
            inner.definitions.insert(extension_id, visible_definition);
            drop(inner);
            let persisted = self.persist_terminal_catalog(&definition).await;
            inner = self.inner.lock().await;
            let _ = inner.definitions.remove(&extension_id);
            persisted
        } else {
            Ok(())
        };
        if let Err(error) = persisted {
            restart = false;
            backoff = false;
            duration = Duration::ZERO;
            definition.phase = EXT_PHASE_BLOCKED;
            definition.next_start_unix_ms = 0;
            detail = error.to_string();
            definition.detail = bounded_detail(&detail);
            definition.failure_count = definition.failure_count.saturating_add(1);
            reason = EXT_EXIT_HOST_FAILURE;
            code = 0;
        }

        let owner_lost = !definition.persistent()
            && definition
                .owner_endpoint
                .is_some_and(|owner| !inner.endpoints.contains_key(&owner));
        let compact_terminal = !definition.persistent()
            && matches!(definition.phase, EXT_PHASE_STOPPED | EXT_PHASE_BLOCKED);
        let next_start = definition.next_start_unix_ms;
        let sequence = definition.next_output_sequence;
        inner.definitions.insert(extension_id, definition);
        if let Some(packet) = wire::msg_extension_exit(&ExtensionExit {
            extension_id,
            definition_revision: attempt_revision,
            attempt: attempt_number,
            task_id,
            output_sequence: sequence,
            reason,
            code,
            next_start_unix_ms: next_start,
            detail: &bounded_detail(&detail),
        }) {
            if compact_terminal {
                retain_terminal_and_fanout(
                    &mut inner,
                    extension_id,
                    packet,
                    self.output_retain_max,
                    TerminalRecordKind::Exit,
                );
            } else {
                retain_and_fanout(&mut inner, extension_id, packet, self.output_retain_max);
            }
        }
        emit_lifecycle_locked(&mut inner, extension_id, self.output_retain_max);
        if owner_lost {
            remove_definition_locked(&mut inner, extension_id);
            return NextAttempt::Stop;
        }
        let wake = inner
            .definitions
            .get(&extension_id)
            .map(|definition| Arc::clone(&definition.wake))
            .unwrap_or_else(|| Arc::new(Notify::new()));
        if !restart {
            NextAttempt::Stop
        } else if backoff {
            NextAttempt::Backoff { duration, wake }
        } else {
            NextAttempt::Immediate
        }
    }

    async fn release_transient(
        &self,
        extension_id: u64,
        generation: u64,
        force_terminal_replay: bool,
    ) {
        let mut inner = self.inner.lock().await;
        let removable = inner
            .definitions
            .get(&extension_id)
            .is_some_and(|definition| {
                !definition.persistent()
                    && definition.generation == generation
                    && definition.control.is_none()
                    && matches!(definition.phase, EXT_PHASE_STOPPED | EXT_PHASE_BLOCKED)
            });
        if !removable {
            return;
        }
        if force_terminal_replay {
            force_terminal_replay_locked(&mut inner, extension_id);
        }
        remove_definition_locked(&mut inner, extension_id);
    }
}

type PreparedAttempt = (
    RuntimeAttempt,
    u64,
    u64,
    String,
    Vec<Vec<u8>>,
    u8,
    u64,
    ObjectHash,
    Option<Arc<ArgumentReservation>>,
);

#[derive(Debug)]
enum RuntimeAttempt {
    Wasmi(wasmi_host::WasmiAttempt),
    QuickJs(quickjs_host::QuickJsAttempt),
}

impl RuntimeAttempt {
    fn thread_names(&self) -> &crate::thread_name::ThreadNames {
        match self {
            Self::Wasmi(attempt) => attempt.thread_names(),
            Self::QuickJs(attempt) => attempt.thread_names(),
        }
    }

    fn cancellation(&self) -> AttemptCancellation {
        match self {
            Self::Wasmi(attempt) => attempt.cancellation(),
            Self::QuickJs(attempt) => attempt.cancellation(),
        }
    }

    fn bridge(&self) -> wasmi_host::HostBridge {
        match self {
            Self::Wasmi(attempt) => attempt.bridge(),
            Self::QuickJs(attempt) => attempt.bridge(),
        }
    }

    async fn wait_prepared(&mut self) -> Result<(), AttemptFailure> {
        match self {
            Self::Wasmi(attempt) => attempt.wait_prepared().await,
            Self::QuickJs(attempt) => attempt.wait_prepared().await,
        }
    }

    fn start(&mut self) -> Result<(), wasmi_host::LifecycleError> {
        match self {
            Self::Wasmi(attempt) => attempt.start(),
            Self::QuickJs(attempt) => attempt.start(),
        }
    }

    fn cancel(&self) {
        match self {
            Self::Wasmi(attempt) => attempt.cancel(),
            Self::QuickJs(attempt) => attempt.cancel(),
        }
    }

    async fn join(self) -> Result<AttemptOutcome, wasmi_host::LifecycleError> {
        match self {
            Self::Wasmi(attempt) => attempt.join().await,
            Self::QuickJs(attempt) => attempt.join().await,
        }
    }
}

enum PrepareAttemptError {
    ArgumentBudget(Arc<Notify>),
    Superseded,
    Failed(AttemptFailure),
}

impl From<AttemptFailure> for PrepareAttemptError {
    fn from(error: AttemptFailure) -> Self {
        Self::Failed(error)
    }
}

enum NextAttempt {
    Stop,
    Immediate,
    Backoff {
        duration: Duration,
        wake: Arc<Notify>,
    },
}

struct AttemptPublication {
    service: Arc<ExtensionService>,
    extension_id: u64,
    generation: u64,
    definition_revision: u64,
    attempt: u64,
    task_id: u32,
}

impl AttemptPublication {
    async fn publish_running(&self) -> Result<(), AttemptFailure> {
        let persistent = {
            let inner = self.service.inner.lock().await;
            inner
                .definitions
                .get(&self.extension_id)
                .is_some_and(Definition::persistent)
        };
        let _catalog_io = if persistent {
            Some(self.service.catalog_io.lock().await)
        } else {
            None
        };
        let is_valid = |inner: &ServiceState| {
            !inner.shutting_down
                && inner
                    .definitions
                    .get(&self.extension_id)
                    .is_some_and(|definition| {
                        definition.generation == self.generation
                            && definition.definition_revision == self.definition_revision
                            && definition.enabled()
                            && definition.desired()
                            && definition.control.as_ref().is_some_and(|control| {
                                control.definition_revision == self.definition_revision
                                    && control.attempt == self.attempt
                                    && control.task_id == self.task_id
                            })
                    })
        };
        {
            let inner = self.service.inner.lock().await;
            if !is_valid(&inner) {
                return Err(AttemptFailure {
                    kind: FailureKind::HostFailure,
                    detail: "extension attempt was superseded during bootstrap".into(),
                });
            }
        }
        self.service
            .persist_attempt_counters_catalog(
                self.extension_id,
                self.attempt,
                self.attempt,
                persistent,
            )
            .await
            .map_err(|error| AttemptFailure {
                kind: FailureKind::HostFailure,
                detail: error.to_string(),
            })?;
        let mut inner = self.service.inner.lock().await;
        if !is_valid(&inner) {
            return Err(AttemptFailure {
                kind: FailureKind::HostFailure,
                detail: "extension attempt was superseded during bootstrap".into(),
            });
        }
        if let Some(definition) = inner.definitions.get_mut(&self.extension_id) {
            definition.phase = EXT_PHASE_RUNNING;
            definition.task_id = self.task_id;
            definition.last_running_attempt = self.attempt;
            definition.detail.clear();
        }
        emit_lifecycle_locked(
            &mut inner,
            self.extension_id,
            self.service.output_retain_max,
        );
        Ok(())
    }

    async fn publish_stopping(&self) {
        let mut inner = self.service.inner.lock().await;
        let matches = inner
            .definitions
            .get(&self.extension_id)
            .and_then(|definition| definition.control.as_ref())
            .is_some_and(|control| {
                control.definition_revision == self.definition_revision
                    && control.attempt == self.attempt
                    && control.task_id == self.task_id
            });
        if matches
            && let Some(definition) = inner.definitions.get_mut(&self.extension_id)
            && definition.phase == EXT_PHASE_RUNNING
        {
            definition.phase = EXT_PHASE_STOPPING;
            definition.task_id = 0;
            emit_lifecycle_locked(
                &mut inner,
                self.extension_id,
                self.service.output_retain_max,
            );
        }
    }
}

struct DrivenAttempt {
    outcome: AttemptOutcome,
    handler_closed_first: bool,
    connection_failure: Option<super::ConnectionFailure>,
    running_for: Duration,
}

async fn drive_attempt(
    state: super::AppState,
    options: Option<super::ConnectionOptions>,
    init_reserved_rx: oneshot::Receiver<()>,
    commit_init_tx: oneshot::Sender<()>,
    mut attempt: RuntimeAttempt,
    connection: super::ConnectionCancellation,
    publication: AttemptPublication,
) -> DrivenAttempt {
    let Some(options) = options else {
        attempt.cancel();
        return DrivenAttempt {
            outcome: attempt.join().await.unwrap_or_else(|error| {
                AttemptOutcome::Failed(AttemptFailure {
                    kind: FailureKind::HostFailure,
                    detail: error.to_string(),
                })
            }),
            handler_closed_first: true,
            connection_failure: connection.failure(),
            running_for: Duration::ZERO,
        };
    };
    let bridge = attempt.bridge();
    let host_cancel = attempt.cancellation();
    let (server_stream, client_stream) =
        tokio::io::duplex(wasmi_host::PACKET_MAX_BYTES.saturating_add(4));
    let (mut from_server, mut to_server) = tokio::io::split(client_stream);

    let handler_state = state.clone();
    let mut handler = tokio::spawn(async move {
        super::handle_client_with_options(server_stream, handler_state, options).await;
    });

    let outbound_bridge = bridge.clone();
    let mut outbound = tokio::spawn(async move {
        while let Some(lease) = outbound_bridge.recv_from_guest().await {
            let packet = lease.packet();
            let length = packet.len() as u32;
            if to_server.write_all(&length.to_le_bytes()).await.is_err()
                || to_server.write_all(packet).await.is_err()
            {
                outbound_bridge.close_from_guest();
                return false;
            }
            lease.acknowledge();
        }
        to_server.shutdown().await.is_ok()
    });

    let inbound_bridge = bridge.clone();
    let mut inbound = tokio::spawn(async move {
        loop {
            let mut length = [0; 4];
            if from_server.read_exact(&mut length).await.is_err() {
                inbound_bridge.close_to_guest();
                return true;
            }
            let length = u32::from_le_bytes(length) as usize;
            if length == 0 || length > wasmi_host::PACKET_MAX_BYTES {
                inbound_bridge.cancel();
                return false;
            }
            match inbound_bridge.reserve_to_guest(length).await {
                Ok(reservation) => {
                    let mut packet = vec![0; length];
                    if from_server.read_exact(&mut packet).await.is_err() {
                        inbound_bridge.close_to_guest();
                        return false;
                    }
                    if reservation.commit(packet).is_err() {
                        inbound_bridge.close_to_guest();
                        return false;
                    }
                }
                Err(wasmi_host::PacketSendError::Closed) => {
                    let mut remaining = length;
                    let mut scratch = [0_u8; 16 * 1024];
                    while remaining > 0 {
                        let chunk = remaining.min(scratch.len());
                        if from_server.read_exact(&mut scratch[..chunk]).await.is_err() {
                            return true;
                        }
                        remaining -= chunk;
                    }
                }
                Err(_) => {
                    inbound_bridge.cancel();
                    return false;
                }
            }
        }
    });

    // Let the guest drain HELLO and the initial snapshot while the public
    // lifecycle remains VALIDATING. Its send ABI still traps until it has
    // actually received INIT.
    if let Err(error) = attempt.start() {
        connection.cancel();
        host_cancel.cancel();
        let outcome = attempt.join().await.unwrap_or_else(|_| {
            AttemptOutcome::Failed(AttemptFailure {
                kind: FailureKind::HostFailure,
                detail: error.to_string(),
            })
        });
        let _ = handler.await;
        let _ = outbound.await;
        let _ = inbound.await;
        return DrivenAttempt {
            outcome,
            handler_closed_first: false,
            connection_failure: connection.failure(),
            running_for: Duration::ZERO,
        };
    }

    let mut join = Box::pin(attempt.join());
    let bootstrap_ended = tokio::select! {
        biased;
        result = &mut join => {
            let outcome = result.unwrap_or_else(|error| AttemptOutcome::Failed(AttemptFailure {
                kind: FailureKind::HostFailure,
                detail: error.to_string(),
            }));
            Some((outcome, false))
        }
        _ = &mut handler => {
            host_cancel.cancel();
            let outcome = (&mut join).await.unwrap_or_else(|error| AttemptOutcome::Failed(AttemptFailure {
                kind: FailureKind::HostFailure,
                detail: error.to_string(),
            }));
            Some((outcome, true))
        }
        reserved = init_reserved_rx => {
            if reserved.is_ok() {
                None
            } else {
                connection.cancel();
                host_cancel.cancel();
                let outcome = (&mut join).await.unwrap_or_else(|error| AttemptOutcome::Failed(AttemptFailure {
                    kind: FailureKind::HostFailure,
                    detail: error.to_string(),
                }));
                Some((outcome, true))
            }
        }
    };
    if let Some((outcome, handler_closed_first)) = bootstrap_ended {
        connection.cancel();
        host_cancel.cancel();
        let handler_closed_first =
            handler_closed_first && !matches!(outcome, AttemptOutcome::Returned(_));
        let _ = (&mut outbound).await;
        if !handler.is_finished() {
            let _ = (&mut handler).await;
        }
        let _ = (&mut inbound).await;
        return DrivenAttempt {
            outcome,
            handler_closed_first,
            connection_failure: connection.failure(),
            running_for: Duration::ZERO,
        };
    }

    if let Err(error) = publication.publish_running().await {
        connection.cancel();
        host_cancel.cancel();
        let _ = join.await;
        if !handler.is_finished() {
            let _ = (&mut handler).await;
        }
        let _ = (&mut outbound).await;
        let _ = (&mut inbound).await;
        return DrivenAttempt {
            outcome: AttemptOutcome::Failed(error),
            handler_closed_first: false,
            connection_failure: connection.failure(),
            running_for: Duration::ZERO,
        };
    }
    let started_at = Instant::now();

    if commit_init_tx.send(()).is_err() {
        connection.cancel();
        host_cancel.cancel();
        let outcome = join.await.unwrap_or_else(|error| {
            AttemptOutcome::Failed(AttemptFailure {
                kind: FailureKind::HostFailure,
                detail: error.to_string(),
            })
        });
        publication.publish_stopping().await;
        let running_for = started_at.elapsed();
        if !handler.is_finished() {
            let _ = (&mut handler).await;
        }
        let _ = (&mut outbound).await;
        let _ = (&mut inbound).await;
        return DrivenAttempt {
            outcome,
            handler_closed_first: true,
            connection_failure: connection.failure(),
            running_for,
        };
    }

    let (outcome, handler_closed_first) = tokio::select! {
        result = &mut join => {
            let outcome = result.unwrap_or_else(|error| AttemptOutcome::Failed(AttemptFailure {
                kind: FailureKind::HostFailure,
                detail: error.to_string(),
            }));
            (outcome, false)
        }
        _ = &mut handler => {
            host_cancel.cancel();
            let outcome = join.await.unwrap_or_else(|error| AttemptOutcome::Failed(AttemptFailure {
                kind: FailureKind::HostFailure,
                detail: error.to_string(),
            }));
            (outcome, true)
        }
    };
    // A normal guest return seals its send side before the thread result is
    // delivered. The handler may consequently observe the orderly EOF and win
    // this select even though it did not fail first.
    let handler_closed_first =
        handler_closed_first && !matches!(outcome, AttemptOutcome::Returned(_));
    publication.publish_stopping().await;
    let running_for = started_at.elapsed();

    if !matches!(outcome, AttemptOutcome::Returned(_)) || handler_closed_first {
        connection.cancel();
        host_cancel.cancel();
    }
    let _ = (&mut outbound).await;
    if !handler.is_finished() {
        let _ = (&mut handler).await;
    }
    let _ = (&mut inbound).await;
    DrivenAttempt {
        outcome,
        handler_closed_first,
        connection_failure: connection.failure(),
        running_for,
    }
}

fn classify_outcome(
    driven: &DrivenAttempt,
    interrupt: Option<Interrupt>,
) -> (u8, i32, String, bool) {
    if let Some(interrupt) = interrupt {
        return match interrupt {
            Interrupt::Updated | Interrupt::Restarted => (
                EXT_EXIT_UPDATED,
                0,
                "extension definition replaced".into(),
                false,
            ),
            Interrupt::ServerShutdown => (
                EXT_EXIT_SERVER_SHUTDOWN,
                0,
                "server is shutting down".into(),
                false,
            ),
            Interrupt::Cancelled | Interrupt::Disabled | Interrupt::OwnerClosed => {
                (EXT_EXIT_CANCELLED, 0, "extension cancelled".into(), false)
            }
        };
    }
    if driven.connection_failure == Some(super::ConnectionFailure::SlowConsumer) {
        return (
            EXT_EXIT_SLOW_CONSUMER,
            0,
            "extension did not drain its output".into(),
            true,
        );
    }
    if driven.connection_failure == Some(super::ConnectionFailure::ResourceLimit) {
        return (
            EXT_EXIT_RESOURCE_LIMIT,
            0,
            "extension native-job resource limit exceeded".into(),
            true,
        );
    }
    if driven.handler_closed_first {
        return (
            EXT_EXIT_PROTOCOL_VIOLATION,
            0,
            "logical client connection closed before the guest returned".into(),
            true,
        );
    }
    match &driven.outcome {
        AttemptOutcome::Returned(code) => (EXT_EXIT_RETURNED, *code, String::new(), *code != 0),
        AttemptOutcome::Cancelled => (EXT_EXIT_CANCELLED, 0, "extension cancelled".into(), false),
        AttemptOutcome::Failed(error) => match error.kind {
            FailureKind::AbiMisuse => (EXT_EXIT_PROTOCOL_VIOLATION, 0, error.detail.clone(), true),
            FailureKind::Trap => (EXT_EXIT_TRAPPED, 0, error.detail.clone(), true),
            FailureKind::Validation | FailureKind::Instantiation | FailureKind::HostFailure => {
                (EXT_EXIT_HOST_FAILURE, 0, error.detail.clone(), true)
            }
        },
    }
}

fn allocate_task_id(inner: &ServiceState) -> Option<u32> {
    for _ in 0..64 {
        let mut bytes = [0; 4];
        getrandom::fill(&mut bytes).ok()?;
        let task_id = u32::from_le_bytes(bytes);
        if task_id != 0 && !inner.task_ids.contains(&task_id) {
            return Some(task_id);
        }
    }
    None
}

fn backoff_duration(failure_count: u32) -> Duration {
    let exponent = failure_count.saturating_sub(1).min(16);
    let cap = BACKOFF_BASE
        .checked_mul(1_u32 << exponent)
        .unwrap_or(BACKOFF_MAX)
        .min(BACKOFF_MAX);
    let range = u64::try_from(cap.as_nanos())
        .unwrap_or(u64::MAX - 1)
        .saturating_add(1);
    let rejection_floor = u64::MAX - (u64::MAX % range);
    loop {
        let mut random = [0_u8; 8];
        if getrandom::fill(&mut random).is_err() {
            return cap;
        }
        let sample = u64::from_le_bytes(random);
        if sample < rejection_floor {
            return Duration::from_nanos(sample % range);
        }
    }
}

fn definition_from_persistent(value: PersistentDefinition) -> Definition {
    let waiting_for_backoff = !value.blocked
        && value.next_start_unix_ms > unix_millis_now()
        && value.flags & (EXT_FLAG_ENABLED | EXT_FLAG_DESIRED_RUNNING)
            == EXT_FLAG_ENABLED | EXT_FLAG_DESIRED_RUNNING;
    Definition {
        extension_id: value.extension_id,
        definition_revision: value.definition_revision,
        flags: value.flags,
        restart: value.restart,
        hash: value.hash,
        name: value.name,
        args: None,
        argument_bytes: value.argument_bytes,
        argument_reservation: None,
        owner_endpoint: None,
        phase: if value.blocked {
            EXT_PHASE_BLOCKED
        } else if waiting_for_backoff {
            EXT_PHASE_BACKOFF
        } else {
            EXT_PHASE_STOPPED
        },
        attempt: value.attempt,
        last_running_attempt: value.last_running_attempt,
        task_id: 0,
        next_start_unix_ms: if waiting_for_backoff {
            value.next_start_unix_ms
        } else {
            0
        },
        detail: if value.blocked {
            value.blocked_detail
        } else {
            String::new()
        },
        next_output_sequence: 1,
        retained: VecDeque::new(),
        terminal_replay: VecDeque::new(),
        retained_bytes: 0,
        followers: HashMap::new(),
        pending_deadline: None,
        release_deadline: None,
        generation: 1,
        failure_count: value.failure_count,
        interrupt: None,
        control: None,
        object_pinned: false,
        catalog_committed: true,
        wake: Arc::new(Notify::new()),
    }
}

fn release_definition_arguments(definition: &mut Definition) {
    definition.args = None;
    definition.argument_reservation = None;
}

fn allocate_extension_id(inner: &ServiceState) -> Option<u64> {
    for _ in 0..64 {
        let mut bytes = [0; 8];
        getrandom::fill(&mut bytes).ok()?;
        let extension_id = u64::from_le_bytes(bytes);
        if extension_id != 0 && !inner.definitions.contains_key(&extension_id) {
            return Some(extension_id);
        }
    }
    None
}

fn commit_transient_create(
    inner: &mut ServiceState,
    definition: &mut Definition,
) -> Result<(), (u8, String)> {
    let store = inner
        .store
        .as_mut()
        .ok_or((EXT_STATUS_OTHER, "object store is unavailable".into()))?;
    store
        .pin(&definition.hash)
        .map_err(|error| (object_status(&error), error.to_string()))?;
    definition.object_pinned = true;
    if definition.persistent() {
        store.unpin(&definition.hash);
        definition.object_pinned = false;
        return Err((
            EXT_STATUS_OTHER,
            "persistent creation requires the catalog lane".into(),
        ));
    }
    Ok(())
}

fn repair_persistent_pin(
    inner: &mut ServiceState,
    current: &Definition,
) -> Result<(), CatalogError> {
    if current.object_pinned {
        return Ok(());
    }
    let store = inner.store.as_mut().ok_or(CatalogError::Unavailable)?;
    store
        .pin(&current.hash)
        .map_err(|error| CatalogError::Storage(error.to_string()))?;
    let definition = inner
        .definitions
        .get_mut(&current.extension_id)
        .ok_or(CatalogError::NotFound)?;
    definition.object_pinned = true;
    Ok(())
}

fn stop_invalid_pending_locked(
    inner: &mut ServiceState,
    hash: ObjectHash,
    detail: &str,
    terminal_retain: Duration,
) -> Vec<u64> {
    let ids = inner
        .definitions
        .values()
        .filter(|definition| definition.hash == hash && definition.phase == EXT_PHASE_NEED_OBJECT)
        .map(|definition| definition.extension_id)
        .collect::<Vec<_>>();
    let now = Instant::now();
    for extension_id in &ids {
        if let Some(definition) = inner.definitions.get_mut(extension_id) {
            definition.phase = EXT_PHASE_STOPPED;
            definition.set_flag(EXT_FLAG_DESIRED_RUNNING, false);
            definition.pending_deadline = None;
            definition.release_deadline = Some(now + terminal_retain);
            definition.generation = definition.generation.saturating_add(1);
            definition.detail = bounded_detail(detail);
            definition.wake.notify_waiters();
            release_definition_arguments(definition);
        }
    }
    ids
}

fn notify_need_object_locked(
    inner: &mut ServiceState,
    hashes: &[ObjectHash],
    output_retain_max: usize,
) {
    if hashes.is_empty() {
        return;
    }
    let ids = inner
        .definitions
        .values()
        .filter(|definition| {
            definition.phase == EXT_PHASE_NEED_OBJECT && hashes.contains(&definition.hash)
        })
        .map(|definition| definition.extension_id)
        .collect::<Vec<_>>();
    for extension_id in ids {
        emit_lifecycle_locked(inner, extension_id, output_retain_max);
    }
}

fn expire_pending_locked(
    inner: &mut ServiceState,
    now: Instant,
    output_retain_max: usize,
    terminal_retain: Duration,
) {
    let ids = inner
        .definitions
        .values()
        .filter(|definition| {
            definition.phase == EXT_PHASE_NEED_OBJECT
                && definition
                    .pending_deadline
                    .is_some_and(|deadline| now >= deadline)
        })
        .map(|definition| definition.extension_id)
        .collect::<Vec<_>>();
    for extension_id in ids {
        if let Some(definition) = inner.definitions.get_mut(&extension_id) {
            definition.phase = EXT_PHASE_STOPPED;
            definition.set_flag(EXT_FLAG_DESIRED_RUNNING, false);
            definition.pending_deadline = None;
            definition.release_deadline = Some(now + terminal_retain);
            definition.generation = definition.generation.saturating_add(1);
            definition.detail = "pending extension creation expired".into();
            definition.wake.notify_waiters();
            release_definition_arguments(definition);
        }
        emit_lifecycle_locked(inner, extension_id, output_retain_max);
    }
}

fn release_expired_pending_locked(inner: &mut ServiceState, now: Instant) {
    let ids = inner
        .definitions
        .values()
        .filter(|definition| {
            definition.control.is_none()
                && definition
                    .release_deadline
                    .is_some_and(|deadline| now >= deadline)
        })
        .map(|definition| definition.extension_id)
        .collect::<Vec<_>>();
    for extension_id in ids {
        force_terminal_replay_locked(inner, extension_id);
        remove_definition_locked(inner, extension_id);
    }
}

/// At the replay lease boundary, bypass the network soft production gate once
/// for the compact terminal pair and its marker. Extension-origin followers
/// still pass through their hard outbox reservation, which cancels a slow
/// consumer instead of admitting unbounded retained output.
fn force_terminal_replay_locked(inner: &mut ServiceState, extension_id: u64) {
    let endpoints = inner
        .definitions
        .get(&extension_id)
        .map(|definition| definition.followers.keys().copied().collect::<Vec<_>>())
        .unwrap_or_default();
    for endpoint in endpoints {
        let Some(sender) = inner.endpoints.get(&endpoint).cloned() else {
            continue;
        };
        let Some((through, cursor, mut records)) =
            inner.definitions.get(&extension_id).and_then(|definition| {
                let follower = definition.followers.get(&endpoint)?;
                let through = follower.replay_through?;
                let records = definition
                    .terminal_replay
                    .iter()
                    .filter(|record| {
                        record.sequence >= follower.next_sequence && record.sequence <= through
                    })
                    .map(|record| (record.sequence, Arc::clone(&record.packet)))
                    .collect::<Vec<_>>();
                Some((through, follower.next_sequence, records))
            })
        else {
            continue;
        };
        records.sort_by_key(|(sequence, _)| *sequence);
        records.dedup_by_key(|(sequence, _)| *sequence);
        let mut next_sequence = cursor;
        let mut open = true;
        for (sequence, packet) in records {
            if !sender.send_retained(&packet) {
                open = false;
                break;
            }
            next_sequence = sequence.saturating_add(1);
        }
        if open {
            let marker = wire::msg_extension_replay_done(extension_id, through)
                .expect("valid replay marker");
            open = sender.send(marker).is_ok();
        }
        if open
            && let Some(follower) = inner
                .definitions
                .get_mut(&extension_id)
                .and_then(|definition| definition.followers.get_mut(&endpoint))
        {
            follower.next_sequence = next_sequence.max(through.saturating_add(1));
            follower.replay_through = None;
        }
    }
}

fn remove_definition_locked(inner: &mut ServiceState, extension_id: u64) {
    if let Some(mut definition) = inner.definitions.remove(&extension_id) {
        if definition.object_pinned
            && let Some(store) = inner.store.as_mut()
        {
            store.unpin(&definition.hash);
        }
        release_definition_arguments(&mut definition);
        inner.retained_bytes = inner
            .retained_bytes
            .saturating_sub(definition.retained_bytes);
        inner.commands.invalidate_extension(extension_id);
    }
    inner.supervisors.remove(&extension_id);
}

fn follower_capacity_available(
    inner: &ServiceState,
    endpoint: u64,
    per_endpoint_limit: usize,
    global_limit: usize,
) -> bool {
    let mut endpoint_count = 0usize;
    let mut global_count = 0usize;
    for definition in inner.definitions.values() {
        global_count = global_count.saturating_add(definition.followers.len());
        endpoint_count = endpoint_count
            .saturating_add(usize::from(definition.followers.contains_key(&endpoint)));
    }
    endpoint_count < per_endpoint_limit && global_count < global_limit
}

fn mutate_lifecycle_locked(
    inner: &mut ServiceState,
    extension_id: u64,
    enabled: Option<bool>,
    desired: Option<bool>,
    interrupt: Interrupt,
    terminal_retain: Duration,
) -> Result<(), CatalogError> {
    let Some(current) = inner.definitions.get(&extension_id).cloned() else {
        return Err(CatalogError::NotFound);
    };
    let pending_creation = current.phase == EXT_PHASE_NEED_OBJECT && !current.catalog_committed;
    if let Some(definition) = inner.definitions.get_mut(&extension_id) {
        if let Some(enabled) = enabled {
            definition.set_flag(EXT_FLAG_ENABLED, enabled);
        }
        if let Some(desired) = desired {
            definition.set_flag(EXT_FLAG_DESIRED_RUNNING, desired);
        }
        definition.generation = definition.generation.saturating_add(1);
        definition.interrupt = Some(interrupt);
        definition.next_start_unix_ms = 0;
        definition.wake.notify_waiters();
        if pending_creation {
            if !definition.enabled() || !definition.desired() {
                definition.phase = EXT_PHASE_STOPPED;
                definition.pending_deadline = None;
                definition.release_deadline = Some(Instant::now() + terminal_retain);
                definition.task_id = 0;
                release_definition_arguments(definition);
            } else {
                definition.phase = EXT_PHASE_NEED_OBJECT;
            }
        } else if definition.control.is_some() {
            definition.phase = EXT_PHASE_STOPPING;
            definition.task_id = 0;
        } else if definition.enabled() && definition.desired() {
            definition.phase = EXT_PHASE_QUEUED;
        } else {
            definition.phase = EXT_PHASE_STOPPED;
        }
    }
    if let Some(control) = current.control.as_ref() {
        inner.commands.invalidate_attempt(
            extension_id,
            control.definition_revision,
            control.attempt,
        );
    }
    if enabled == Some(false) {
        inner
            .commands
            .invalidate_definition(extension_id, current.definition_revision);
    }
    Ok(())
}

fn retain_and_fanout(
    inner: &mut ServiceState,
    extension_id: u64,
    packet: Vec<u8>,
    global_limit: usize,
) -> Option<RetainedRecord> {
    retain_record(inner, extension_id, packet, global_limit, false)
}

fn retain_record(
    inner: &mut ServiceState,
    extension_id: u64,
    packet: Vec<u8>,
    global_limit: usize,
    compact_terminal: bool,
) -> Option<RetainedRecord> {
    debug_assert_eq!(inner.output_budget.max, global_limit);
    let bytes = packet.len();
    let (sequence, clock) = {
        let definition = inner.definitions.get_mut(&extension_id)?;
        let Some(next_sequence) = definition.next_output_sequence.checked_add(1) else {
            definition.phase = EXT_PHASE_BLOCKED;
            definition.detail = "extension output sequence exhausted".into();
            return None;
        };
        let sequence = definition.next_output_sequence;
        definition.next_output_sequence = next_sequence;
        inner.retention_clock = inner.retention_clock.saturating_add(1);
        while bytes <= OUTPUT_RETAIN_PER_EXTENSION
            && definition.retained_bytes.saturating_add(bytes) > OUTPUT_RETAIN_PER_EXTENSION
        {
            let Some(evicted) = definition.retained.pop_front() else {
                break;
            };
            definition.retained_bytes = definition
                .retained_bytes
                .saturating_sub(evicted.packet.len());
            inner.retained_bytes = inner.retained_bytes.saturating_sub(evicted.packet.len());
        }
        (sequence, inner.retention_clock)
    };

    let reservation = if bytes <= OUTPUT_RETAIN_PER_EXTENSION {
        loop {
            if let Some(reservation) = inner.output_budget.try_reserve(bytes) {
                break Some(reservation);
            }
            if !evict_oldest_history(inner) {
                break None;
            }
        }
    } else {
        None
    };
    if reservation.is_none() && !compact_terminal {
        return None;
    }
    let retained = reservation.is_some();
    let record = RetainedRecord {
        sequence,
        clock,
        packet: Arc::new(RetainedPacket {
            bytes: packet,
            _reservation: reservation,
        }),
    };
    if retained {
        let definition = inner.definitions.get_mut(&extension_id)?;
        definition.retained.push_back(record.clone());
        definition.retained_bytes = definition.retained_bytes.saturating_add(bytes);
        inner.retained_bytes = inner.retained_bytes.saturating_add(bytes);
    }
    wake_followers_locked(inner, extension_id);
    Some(record)
}

#[derive(Clone, Copy)]
enum TerminalRecordKind {
    Exit,
    Status,
}

fn retain_terminal_and_fanout(
    inner: &mut ServiceState,
    extension_id: u64,
    packet: Vec<u8>,
    global_limit: usize,
    kind: TerminalRecordKind,
) -> Option<u64> {
    let persistent = inner.definitions.get(&extension_id)?.persistent();
    let record = retain_record(inner, extension_id, packet, global_limit, !persistent)?;
    let definition = inner.definitions.get_mut(&extension_id)?;
    if persistent {
        return Some(record.sequence);
    }
    match kind {
        TerminalRecordKind::Exit => definition.terminal_replay.clear(),
        TerminalRecordKind::Status => definition
            .terminal_replay
            .retain(|record| record.packet.first() == Some(&wire::EXT_EXIT)),
    }
    definition.terminal_replay.push_back(record);
    while definition.terminal_replay.len() > 2 {
        definition.terminal_replay.pop_front();
    }
    Some(
        definition
            .terminal_replay
            .back()
            .expect("just inserted compact terminal record")
            .sequence,
    )
}

fn oldest_replay_sequence(definition: &Definition) -> u64 {
    definition
        .retained
        .iter()
        .chain(definition.terminal_replay.iter())
        .map(|record| record.sequence)
        .min()
        .unwrap_or(definition.next_output_sequence)
}

#[cfg(test)]
fn merged_replay(definition: &Definition, cursor: u64, through: u64) -> Vec<(u64, Vec<u8>)> {
    let mut records = definition
        .retained
        .iter()
        .chain(definition.terminal_replay.iter())
        .filter(|record| record.sequence >= cursor && record.sequence <= through)
        .map(|record| (record.sequence, record.packet.to_vec()))
        .collect::<Vec<_>>();
    records.sort_by_key(|(sequence, _)| *sequence);
    records.dedup_by_key(|(sequence, _)| *sequence);
    records
}

fn fanout_replay_done(inner: &mut ServiceState, extension_id: u64, through: u64) {
    if let Some(definition) = inner.definitions.get_mut(&extension_id) {
        for follower in definition.followers.values_mut() {
            follower.replay_through = Some(
                follower
                    .replay_through
                    .map_or(through, |pending| pending.max(through)),
            );
        }
    }
    wake_followers_locked(inner, extension_id);
}

fn evict_oldest_history(inner: &mut ServiceState) -> bool {
    let oldest = inner
        .definitions
        .iter()
        .filter_map(|(extension_id, definition)| {
            definition
                .retained
                .front()
                .map(|record| (*extension_id, record.clock))
        })
        .min_by_key(|(_, clock)| *clock)
        .map(|(extension_id, _)| extension_id);
    let Some(extension_id) = oldest else {
        return false;
    };
    let Some(definition) = inner.definitions.get_mut(&extension_id) else {
        return false;
    };
    let Some(evicted) = definition.retained.pop_front() else {
        return false;
    };
    definition.retained_bytes = definition
        .retained_bytes
        .saturating_sub(evicted.packet.len());
    inner.retained_bytes = inner.retained_bytes.saturating_sub(evicted.packet.len());
    true
}

fn next_replay_record(
    definition: &Definition,
    cursor: u64,
    through: u64,
) -> Option<&RetainedRecord> {
    definition
        .retained
        .iter()
        .chain(definition.terminal_replay.iter())
        .filter(|record| record.sequence >= cursor && record.sequence <= through)
        .min_by_key(|record| record.sequence)
}

fn next_replay_sequence(definition: &Definition, cursor: u64, through: u64) -> Option<u64> {
    next_replay_record(definition, cursor, through).map(|record| record.sequence)
}

fn wake_endpoint_locked(inner: &ServiceState, endpoint: u64) {
    if let Some(wake) = inner.endpoint_wakes.get(&endpoint) {
        wake.notify_one();
    }
}

fn wake_followers_locked(inner: &ServiceState, extension_id: u64) {
    let Some(definition) = inner.definitions.get(&extension_id) else {
        return;
    };
    for endpoint in definition.followers.keys() {
        wake_endpoint_locked(inner, *endpoint);
    }
}

enum ScheduleOutcome {
    Sent(u64),
    Idle,
    Closed,
}

fn schedule_one_locked(
    inner: &mut ServiceState,
    endpoint: u64,
    last_extension: Option<u64>,
) -> ScheduleOutcome {
    let Some(sender) = inner.endpoints.get(&endpoint).cloned() else {
        return ScheduleOutcome::Closed;
    };
    let mut followed = inner
        .definitions
        .iter()
        .filter(|(_, definition)| definition.followers.contains_key(&endpoint))
        .map(|(extension_id, _)| *extension_id)
        .collect::<Vec<_>>();
    followed.sort_unstable();
    let start = last_extension
        .and_then(|last| {
            followed
                .iter()
                .position(|extension_id| *extension_id > last)
        })
        .unwrap_or(0);
    for offset in 0..followed.len() {
        let extension_id = followed[(start + offset) % followed.len()];
        let Some(definition) = inner.definitions.get_mut(&extension_id) else {
            continue;
        };
        let Some(mut follower) = definition.followers.get(&endpoint).copied() else {
            continue;
        };
        follower.next_sequence = follower
            .next_sequence
            .max(oldest_replay_sequence(definition));
        let through = follower.replay_through.unwrap_or(u64::MAX);
        if let Some(record) = next_replay_record(definition, follower.next_sequence, through) {
            let sequence = record.sequence;
            if !sender.send_retained(&record.packet) {
                return ScheduleOutcome::Closed;
            }
            follower.next_sequence = sequence.saturating_add(1);
            definition.followers.insert(endpoint, follower);
            return ScheduleOutcome::Sent(extension_id);
        }
        if let Some(through) = follower.replay_through {
            let marker = wire::msg_extension_replay_done(extension_id, through)
                .expect("valid replay marker");
            if sender.send(marker).is_err() {
                return ScheduleOutcome::Closed;
            }
            follower.next_sequence = follower.next_sequence.max(through.saturating_add(1));
            follower.replay_through = None;
            definition.followers.insert(endpoint, follower);
            return ScheduleOutcome::Sent(extension_id);
        }
    }
    ScheduleOutcome::Idle
}

fn emit_lifecycle_locked(inner: &mut ServiceState, extension_id: u64, global_limit: usize) {
    let Some(definition) = inner.definitions.get(&extension_id) else {
        return;
    };
    let compact_terminal = !definition.persistent()
        && matches!(definition.phase, EXT_PHASE_STOPPED | EXT_PHASE_BLOCKED);
    let sequence = definition.next_output_sequence;
    let packet = wire::msg_extension_info_status(&ExtensionInfoStatus {
        extension_id,
        definition_revision: definition.definition_revision,
        phase: definition.phase,
        flags: definition.flags,
        restart: definition.restart,
        attempt: definition.attempt,
        last_running_attempt: definition.last_running_attempt,
        task_id: definition.task_id,
        output_sequence: sequence,
        next_start_unix_ms: definition.next_start_unix_ms,
        hash: definition.hash,
        detail: &bounded_detail(&definition.detail),
    });
    if let Some(packet) = packet {
        if compact_terminal {
            if let Some(through) = retain_terminal_and_fanout(
                inner,
                extension_id,
                packet,
                global_limit,
                TerminalRecordKind::Status,
            ) {
                fanout_replay_done(inner, extension_id, through);
            }
        } else {
            if let Some(definition) = inner.definitions.get_mut(&extension_id) {
                definition.terminal_replay.clear();
            }
            retain_and_fanout(inner, extension_id, packet, global_limit);
        }
    }
}

fn status_packet(
    definition: &Definition,
    nonce: u16,
    status: u8,
    phase_override: Option<u8>,
    detail: &str,
) -> Vec<u8> {
    status_packet_with_replay(definition, nonce, status, phase_override, 0, detail)
}

fn attach_status_packet(
    definition: &Definition,
    nonce: u16,
    replay_from_sequence: u64,
    detail: &str,
) -> Vec<u8> {
    status_packet_with_replay(
        definition,
        nonce,
        EXT_STATUS_OK,
        None,
        replay_from_sequence,
        detail,
    )
}

fn status_packet_with_replay(
    definition: &Definition,
    nonce: u16,
    status: u8,
    phase_override: Option<u8>,
    replay_from_sequence: u64,
    detail: &str,
) -> Vec<u8> {
    let phase = phase_override.unwrap_or(definition.phase);
    let task_id = if phase == EXT_PHASE_RUNNING {
        definition.task_id
    } else {
        0
    };
    let next_start_unix_ms = if phase == EXT_PHASE_BACKOFF {
        definition.next_start_unix_ms
    } else {
        0
    };
    let detail = bounded_detail(detail);
    wire::msg_extension_status(&ExtensionStatus {
        nonce,
        status,
        phase,
        flags: definition.flags,
        restart: definition.restart,
        extension_id: definition.extension_id,
        definition_revision: definition.definition_revision,
        attempt: definition.attempt,
        last_running_attempt: definition.last_running_attempt,
        task_id,
        replay_from_sequence,
        output_sequence: definition.latest_output_sequence(),
        next_start_unix_ms,
        hash: definition.hash,
        detail: &detail,
    })
    .expect("internally valid extension status")
}

#[allow(clippy::too_many_arguments)]
fn fixed_status(
    nonce: u16,
    status: u8,
    flags: u8,
    restart: u8,
    extension_id: u64,
    definition_revision: u64,
    hash: ObjectHash,
    detail: &str,
) -> Vec<u8> {
    let detail = bounded_detail(detail);
    wire::msg_extension_status(&ExtensionStatus {
        nonce,
        status,
        phase: wire::EXT_PHASE_NONE,
        flags,
        restart,
        extension_id,
        definition_revision,
        attempt: 0,
        last_running_attempt: 0,
        task_id: 0,
        replay_from_sequence: 0,
        output_sequence: 0,
        next_start_unix_ms: 0,
        hash,
        detail: &detail,
    })
    .expect("internally valid empty extension status")
}

fn run_error_status(nonce: u16, status: u8, hash: ObjectHash, detail: &str) -> Vec<u8> {
    fixed_status(nonce, status, 0, 0, 0, 0, hash, detail)
}

fn creation_status(definition: &Definition, nonce: u16, detail: &str) -> Vec<u8> {
    let detail = bounded_detail(detail);
    wire::msg_extension_status(&ExtensionStatus {
        nonce,
        status: EXT_STATUS_OK,
        phase: definition.phase,
        flags: definition.flags,
        restart: definition.restart,
        extension_id: definition.extension_id,
        definition_revision: definition.definition_revision,
        attempt: 0,
        last_running_attempt: 0,
        task_id: 0,
        replay_from_sequence: 0,
        output_sequence: 0,
        next_start_unix_ms: 0,
        hash: definition.hash,
        detail: &detail,
    })
    .expect("internally valid extension creation status")
}

fn update_operation_status(
    definition: &Definition,
    nonce: u16,
    status: u8,
    phase: u8,
    hash: ObjectHash,
    restart: u8,
    detail: &str,
) -> Vec<u8> {
    let detail = bounded_detail(detail);
    wire::msg_extension_status(&ExtensionStatus {
        nonce,
        status,
        phase,
        flags: definition.flags,
        restart,
        extension_id: definition.extension_id,
        definition_revision: definition.definition_revision,
        attempt: 0,
        last_running_attempt: 0,
        task_id: 0,
        replay_from_sequence: 0,
        output_sequence: 0,
        next_start_unix_ms: 0,
        hash,
        detail: &detail,
    })
    .expect("internally valid extension update status")
}

fn put_status(nonce: u16, status: u8, hash: ObjectHash, received: u64, detail: &str) -> Vec<u8> {
    let detail = bounded_detail(detail);
    wire::msg_extension_put_status(&ExtensionPutStatus {
        nonce,
        status,
        hash,
        received,
        detail: &detail,
    })
    .expect("internally valid extension upload status")
}

fn extension_record(definition: &Definition) -> ExtensionRecord<'_> {
    ExtensionRecord {
        extension_id: definition.extension_id,
        definition_revision: definition.definition_revision,
        phase: definition.phase,
        flags: definition.flags,
        restart: definition.restart,
        attempt: definition.attempt,
        last_running_attempt: definition.last_running_attempt,
        task_id: definition.task_id,
        output_sequence: definition.latest_output_sequence(),
        next_start_unix_ms: definition.next_start_unix_ms,
        hash: definition.hash,
        name: &definition.name,
    }
}

fn bounded_detail(detail: &str) -> String {
    if detail.len() <= wire::EXT_MAX_DETAIL {
        return detail.to_owned();
    }
    let mut end = wire::EXT_MAX_DETAIL;
    while !detail.is_char_boundary(end) {
        end -= 1;
    }
    detail[..end].to_owned()
}

fn catalog_status(error: &CatalogError) -> u8 {
    match error {
        CatalogError::Unavailable => EXT_STATUS_PERMISSION,
        CatalogError::Invalid(_) => EXT_STATUS_INVALID,
        CatalogError::Conflict => EXT_STATUS_CONFLICT,
        CatalogError::NotFound => EXT_STATUS_NOT_FOUND,
        CatalogError::Budget => EXT_STATUS_BUDGET,
        CatalogError::Storage(_) => EXT_STATUS_OTHER,
    }
}

fn mark_hash_unpinned(inner: &mut ServiceState, hash: &ObjectHash) {
    for definition in inner
        .definitions
        .values_mut()
        .filter(|definition| &definition.hash == hash)
    {
        definition.object_pinned = false;
    }
}

fn object_status(error: &ObjectStoreError) -> u8 {
    match error {
        ObjectStoreError::InvalidConfig(_) | ObjectStoreError::InvalidUpload(_) => {
            EXT_STATUS_INVALID
        }
        ObjectStoreError::NotFound => EXT_STATUS_NOT_FOUND,
        ObjectStoreError::Conflict => EXT_STATUS_CONFLICT,
        ObjectStoreError::TooLarge => EXT_STATUS_TOO_LARGE,
        ObjectStoreError::Budget => EXT_STATUS_BUDGET,
        ObjectStoreError::HashMismatch | ObjectStoreError::InvalidModule(_) => EXT_STATUS_INVALID,
        ObjectStoreError::Io(_) => EXT_STATUS_OTHER,
    }
}

fn origin_identity(origin: &super::ConnectionOrigin) -> Option<(u64, u64, u64, u32)> {
    let super::ConnectionOrigin::Extension {
        extension_id,
        definition_revision,
        attempt,
        task_id,
        ..
    } = origin
    else {
        return None;
    };
    Some((*extension_id, *definition_revision, *attempt, *task_id))
}

fn command_owner(
    inner: &ServiceState,
    endpoint: u64,
    endpoint_generation: u64,
    identity: (u64, u64, u64, u32),
) -> Option<CommandOwner> {
    let definition = inner.definitions.get(&identity.0)?;
    let control = definition.control.as_ref()?;
    Some(CommandOwner {
        endpoint_id: endpoint,
        endpoint_generation,
        extension_id: definition.extension_id,
        definition_revision: definition.definition_revision,
        attempt: identity.2,
        hash: definition.hash,
        name: definition.name.clone(),
        persistent: definition.persistent(),
        enabled: definition.enabled(),
        running: definition.phase == EXT_PHASE_RUNNING
            && definition.definition_revision == identity.1
            && control.attempt == identity.2
            && control.task_id == identity.3,
    })
}

fn command_listener(
    endpoint_generation: u64,
    listener: crate::channel::ListenerSnapshot,
) -> CommandListener {
    CommandListener {
        endpoint_id: listener.endpoint,
        endpoint_generation,
        listener_id: listener.channel_id,
        listener_generation: listener.generation,
        name: listener.name,
        token: listener.token,
    }
}

fn command_registered(
    nonce: u16,
    status: u8,
    extension_id: u64,
    definition_revision: u64,
    detail: &str,
) -> Vec<u8> {
    let detail = bounded_detail(detail);
    wire::msg_extension_command_registered(&wire::ExtensionCommandRegistered {
        nonce,
        status,
        extension_id,
        definition_revision,
        detail: &detail,
    })
    .expect("internally valid command registration response")
}

fn command_page(nonce: u16, page: &DiscoveryPage) -> Vec<u8> {
    let records = page
        .records
        .iter()
        .map(|record| record.as_wire())
        .collect::<Vec<_>>();
    wire::msg_extension_commands(
        nonce,
        page.status.status(),
        page.directory_revision,
        page.next_cursor,
        &records,
    )
    .expect("command directory enforces response bounds")
}

fn host_running_default() -> usize {
    std::thread::available_parallelism()
        .map(|cpus| cpus.get().saturating_sub(1).clamp(1, DEFAULT_MAX_RUNNING))
        .unwrap_or(1)
}

fn packet_u16(packet: &[u8], offset: usize) -> u16 {
    packet
        .get(offset..offset.saturating_add(2))
        .and_then(|bytes| bytes.try_into().ok())
        .map(u16::from_le_bytes)
        .unwrap_or(0)
}

fn packet_u64(packet: &[u8], offset: usize) -> u64 {
    packet
        .get(offset..offset.saturating_add(8))
        .and_then(|bytes| bytes.try_into().ok())
        .map(u64::from_le_bytes)
        .unwrap_or(0)
}

fn packet_hash(packet: &[u8], offset: usize) -> ObjectHash {
    packet
        .get(offset..offset.saturating_add(32))
        .and_then(|bytes| bytes.try_into().ok())
        .unwrap_or([0; 32])
}

fn encoded_argument_bytes(args: &[Vec<u8>]) -> usize {
    args.iter().fold(2usize, |total, argument| {
        total.saturating_add(4 + argument.len())
    })
}

fn encoded_borrowed_argument_bytes(args: &[&[u8]]) -> usize {
    args.iter().fold(2usize, |total, argument| {
        total.saturating_add(4 + argument.len())
    })
}

fn unix_millis_after(duration: Duration) -> u64 {
    unix_millis_now().saturating_add(duration.as_millis().min(u64::MAX as u128) as u64)
}

fn unix_millis_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(u64::MAX as u128) as u64
}

#[cfg(test)]
mod tests {
    use super::*;
    use blit_remote::extension::{
        EXT_EXIT_RETURNED, EXT_PHASE_NEED_OBJECT, ExtensionMessage, ExtensionRunRequest,
    };

    fn temporary_root(label: &str) -> std::path::PathBuf {
        let mut random = [0; 8];
        getrandom::fill(&mut random).unwrap();
        std::env::temp_dir().join(format!(
            "blit-extension-service-{label}-{:016x}",
            u64::from_le_bytes(random)
        ))
    }

    fn test_service(root: &std::path::Path) -> Arc<ExtensionService> {
        test_service_with_output_retain(root, 8 * 1024 * 1024)
    }

    fn test_service_with_output_retain(
        root: &std::path::Path,
        output_retain_max: usize,
    ) -> Arc<ExtensionService> {
        test_service_with_limits(
            root,
            output_retain_max,
            8 * 1024 * 1024,
            DEFAULT_TERMINAL_RETAIN,
            2,
        )
    }

    fn test_service_with_limits(
        root: &std::path::Path,
        output_retain_max: usize,
        argument_store_max: usize,
        terminal_retain: Duration,
        max_running: usize,
    ) -> Arc<ExtensionService> {
        let store = ObjectStore::open(ObjectStoreConfig {
            root: root.join("cache"),
            module_max: wire::EXT_MAX_MODULE,
            cache_max: 128 * 1024 * 1024,
            entry_max: 32,
            upload_max: 4,
            upload_max_per_endpoint: 2,
            upload_timeout: Duration::from_secs(30),
            allocation_quantum: 4096,
        })
        .unwrap();
        let catalog = ExtensionCatalog::open(Some(root.join("extensions.redb")), 8).unwrap();
        Arc::new(ExtensionService {
            enabled: true,
            available: true,
            persist_allowed: true,
            max_transient: 8,
            max_persistent: 8,
            follow_max_per_endpoint: 8,
            follow_max: 32,
            argument_budget: ArgumentBudget::new(argument_store_max),
            validation_request_budget: ArgumentBudget::new(
                usize::try_from(wire::EXT_MAX_MODULE).unwrap(),
            ),
            output_retain_max,
            pending_timeout: Duration::from_secs(30),
            terminal_retain,
            host_config: WasmiHostConfig::default(),
            running: Arc::new(Semaphore::new(max_running)),
            validating: Arc::new(Semaphore::new(1)),
            store_io: Mutex::new(()),
            catalog_io: Mutex::new(()),
            catalog: Arc::new(std::sync::Mutex::new(Some(catalog))),
            upload_tails: std::sync::Mutex::new(HashMap::new()),
            maintenance_started: AtomicBool::new(false),
            validation_hook: std::sync::Mutex::new(None),
            storage_hook: std::sync::Mutex::new(None),
            catalog_hook: Arc::new(std::sync::Mutex::new(None)),
            inner: Mutex::new(ServiceState {
                store: Some(store),
                diagnostic: None,
                definitions: HashMap::new(),
                endpoints: HashMap::new(),
                endpoint_wakes: HashMap::new(),
                supervisors: HashSet::new(),
                supervisor_completions: HashMap::new(),
                task_ids: HashSet::new(),
                retained_bytes: 0,
                output_budget: OutputBudget::new(output_retain_max),
                retention_clock: 0,
                shutting_down: false,
                commands: CommandDirectory::new(Default::default()),
            }),
        })
    }

    fn test_state(extensions: Arc<ExtensionService>) -> super::super::AppState {
        let boot_generation = 73;
        Arc::new(super::super::AppStateInner {
            config: super::super::Config {
                name: crate::ServerName::default(),
                shell: "/bin/sh".into(),
                shell_flags: String::new(),
                scrollback: 100,
                ipc_path: "unused".into(),
                surface_encoders: Vec::new(),
                surface_encoding: super::super::SurfaceEncoding::default(),
                chroma: super::super::ChromaSubsampling::default(),
                media_codecs: super::super::MediaCodecPolicy::default(),
                vaapi_device: String::new(),
                #[cfg(unix)]
                fd_channel: None,
                verbose: false,
                processes: false,
                max_connections: 0,
                max_ptys: 0,
                ping_interval: Duration::ZERO,
                skip_compositor: true,
                export_sock: false,
                inject_path: false,
                allow_forward: Vec::new(),
                allow_forward_insecure: false,
                allow_persistent_extensions: true,
            },
            #[cfg(any(unix, windows))]
            process_server: super::super::process::Server::new(false, false),
            boot_generation,
            session: Mutex::new(super::super::Session::new_with_boot_generation(
                boot_generation,
            )),
            pty_fds: Arc::new(std::sync::RwLock::new(HashMap::default())),
            delivery_notify: Arc::new(Notify::new()),
            shutdown_notify: Arc::new(Notify::new()),
            supervisor_notify: Arc::new(Notify::new()),
            active_connections: std::sync::atomic::AtomicUsize::new(0),
            connections: Arc::new(super::super::ConnectionRegistry::default()),
            extension_jobs: super::super::extension_jobs::GlobalTracker::from_env(),
            extensions,
        })
    }

    fn output_definition(extension_id: u64, endpoint: u64) -> Definition {
        let mut definition = definition_from_persistent(PersistentDefinition {
            extension_id,
            definition_revision: 1,
            flags: EXT_FLAG_PERSIST,
            restart: wire::EXT_RESTART_NEVER,
            attempt: 0,
            last_running_attempt: 0,
            failure_count: 0,
            next_start_unix_ms: 0,
            blocked: false,
            blocked_detail: String::new(),
            hash: [extension_id as u8; 32],
            name: format!("output-{extension_id}"),
            argument_bytes: 0,
        });
        definition.followers.insert(
            endpoint,
            FollowerCursor {
                next_sequence: 1,
                replay_through: None,
            },
        );
        definition
    }

    fn attached_definition(extension_id: u64, owner_endpoint: u64) -> Definition {
        let mut definition = output_definition(extension_id, owner_endpoint);
        definition.flags = EXT_FLAG_ENABLED | EXT_FLAG_DESIRED_RUNNING;
        definition.owner_endpoint = Some(owner_endpoint);
        definition.phase = EXT_PHASE_QUEUED;
        definition.followers.clear();
        definition
    }

    #[tokio::test]
    async fn attached_cleanup_waits_recursively_for_descendant_supervisors() {
        let root = temporary_root("recursive-cleanup");
        let service = test_service(&root);
        let parent_endpoint = 701;
        let child_endpoint = 702;
        let child_id = 801;
        let grandchild_id = 802;
        let child_done = SupervisorCompletion::new();
        let grandchild_done = SupervisorCompletion::new();
        {
            let mut inner = service.inner.lock().await;
            inner
                .definitions
                .insert(child_id, attached_definition(child_id, parent_endpoint));
            inner.definitions.insert(
                grandchild_id,
                attached_definition(grandchild_id, child_endpoint),
            );
            inner
                .supervisor_completions
                .insert(child_id, vec![Arc::clone(&child_done)]);
            inner
                .supervisor_completions
                .insert(grandchild_id, vec![Arc::clone(&grandchild_done)]);
        }

        // Model the child connection's cleanup: its supervisor cannot finish
        // until unregistering the child endpoint has drained the grandchild.
        let child_cleanup = {
            let service = Arc::clone(&service);
            let child_done = Arc::clone(&child_done);
            tokio::spawn(async move {
                service.unregister_endpoint(child_endpoint, 73).await;
                child_done.complete();
            })
        };
        let mut parent_cleanup = {
            let service = Arc::clone(&service);
            tokio::spawn(async move {
                service.unregister_endpoint(parent_endpoint, 73).await;
            })
        };

        assert!(
            tokio::time::timeout(Duration::from_millis(20), &mut parent_cleanup)
                .await
                .is_err(),
            "parent cleanup returned before the child supervisor completed"
        );
        assert!(!child_done.is_complete());
        grandchild_done.complete();
        tokio::time::timeout(Duration::from_secs(1), child_cleanup)
            .await
            .expect("child cleanup observed grandchild completion")
            .unwrap();
        tokio::time::timeout(Duration::from_secs(1), parent_cleanup)
            .await
            .expect("parent cleanup observed child completion")
            .unwrap();
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn owner_teardown_wakes_terminal_replay_wait_without_waiting_for_lease() {
        let root = temporary_root("owner-terminal-wake");
        let service = test_service_with_limits(
            &root,
            8 * 1024 * 1024,
            8 * 1024 * 1024,
            Duration::from_secs(30),
            1,
        );
        let module = returning_module(0);
        let hash = tokio::task::block_in_place(|| insert_module(&service, &module));
        let state = test_state(Arc::clone(&service));
        let endpoint = 703;
        let mut receiver = register_test_endpoint(&service, endpoint).await;
        service
            .dispatch(
                state,
                endpoint,
                &super::super::ConnectionOrigin::Network,
                &run_packet(31, hash),
            )
            .await;
        let extension_id = loop {
            let packet = receiver.recv().await.unwrap();
            if let Some(ExtensionMessage::Status(status)) =
                wire::parse_extension_message(&packet).unwrap()
                && status.nonce == 31
            {
                break status.extension_id;
            }
        };
        let _ = wait_for_exit(&mut receiver).await;
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                let waiting = {
                    let inner = service.inner.lock().await;
                    !inner.supervisors.contains(&extension_id)
                        && inner.definitions.contains_key(&extension_id)
                        && inner.supervisor_completions.get(&extension_id).is_some_and(
                            |completions| {
                                completions
                                    .iter()
                                    .any(|completion| !completion.is_complete())
                            },
                        )
                };
                if waiting {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("supervisor entered terminal replay wait");

        tokio::time::timeout(
            Duration::from_millis(500),
            service.unregister_endpoint(endpoint, 73),
        )
        .await
        .expect("owner teardown observed terminal wake instead of replay lease");
        assert!(
            !service
                .inner
                .lock()
                .await
                .definitions
                .contains_key(&extension_id)
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn stalled_network_follower_keeps_global_charge_after_ring_eviction() {
        let root = temporary_root("stalled-output");
        let service = test_service_with_output_retain(&root, 4);
        let endpoint = 71;
        let frames = Arc::new(AtomicUsize::new(0));
        let bytes = Arc::new(AtomicUsize::new(0));
        let tracking = Arc::new(super::super::OutboxTracking {
            queued_frames: frames,
            queued_bytes: bytes,
            extension_limit: None,
            drain_notify: std::sync::Mutex::new(None),
        });
        let (tx, mut rx) = mpsc::unbounded_channel();
        let sender = super::super::TrackedOutboxSender::ordered(tx, tracking);
        let budget = {
            let mut inner = service.inner.blocking_lock();
            inner.endpoints.insert(endpoint, sender);
            inner.definitions.insert(1, output_definition(1, endpoint));
            assert!(retain_and_fanout(&mut inner, 1, vec![1; 4], 4).is_some());
            assert!(matches!(
                schedule_one_locked(&mut inner, endpoint, None),
                ScheduleOutcome::Sent(1)
            ));
            assert_eq!(inner.output_budget.used(), 4);

            // The only discoverable record can be evicted, but its shared
            // writer clone still owns the global reservation. The next
            // sequence is allocated and dropped instead of overcommitting.
            assert!(retain_and_fanout(&mut inner, 1, vec![2; 4], 4).is_none());
            assert!(inner.definitions[&1].retained.is_empty());
            assert_eq!(inner.definitions[&1].next_output_sequence, 3);
            assert_eq!(inner.output_budget.used(), 4);
            Arc::clone(&inner.output_budget)
        };
        let queued = rx
            .try_recv()
            .expect("first record reached the writer outbox");
        assert_eq!(queued.packet(), &[1; 4]);
        drop(queued);
        assert_eq!(budget.used(), 0);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn terminal_lease_expiry_force_flushes_permanently_soft_gated_network_follower() {
        let root = temporary_root("terminal-force-flush");
        let service = test_service_with_output_retain(&root, 0);
        let endpoint = 711;
        let tracking = Arc::new(super::super::OutboxTracking {
            queued_frames: Arc::new(AtomicUsize::new(
                super::super::OUTBOX_SOFT_QUEUE_MIN_FRAMES + 1,
            )),
            queued_bytes: Arc::new(AtomicUsize::new(
                super::super::OUTBOX_SOFT_QUEUE_LIMIT_BYTES + 1,
            )),
            extension_limit: None,
            drain_notify: std::sync::Mutex::new(None),
        });
        let (tx, mut rx) = mpsc::unbounded_channel();
        let sender = super::super::TrackedOutboxSender::ordered(tx, tracking);
        assert!(sender.requires_soft_gate());
        {
            let mut inner = service.inner.blocking_lock();
            inner.endpoints.insert(endpoint, sender);
            let mut definition = output_definition(1, endpoint);
            definition.flags = 0;
            definition.phase = EXT_PHASE_STOPPED;
            inner.definitions.insert(1, definition);
            let exit_sequence = retain_terminal_and_fanout(
                &mut inner,
                1,
                vec![wire::EXT_EXIT],
                0,
                TerminalRecordKind::Exit,
            )
            .unwrap();
            let through = retain_terminal_and_fanout(
                &mut inner,
                1,
                vec![wire::EXT_INFO],
                0,
                TerminalRecordKind::Status,
            )
            .unwrap();
            assert!(exit_sequence < through);
            fanout_replay_done(&mut inner, 1, through);

            // The normal scheduler would remain behind the soft gate. Lease
            // expiry gets one bounded bypass before the definition vanishes.
            force_terminal_replay_locked(&mut inner, 1);
            remove_definition_locked(&mut inner, 1);
        }
        assert_eq!(rx.try_recv().unwrap().packet(), &[wire::EXT_EXIT]);
        assert_eq!(rx.try_recv().unwrap().packet(), &[wire::EXT_INFO]);
        let marker = rx.try_recv().unwrap();
        assert!(matches!(
            wire::parse_extension_message(marker.packet()).unwrap(),
            Some(ExtensionMessage::Info(wire::ExtensionInfo::ReplayDone {
                extension_id: 1,
                through_sequence: 2,
            }))
        ));
        assert!(rx.try_recv().is_err());
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn extension_hard_gate_rejects_before_cloning_retained_record() {
        let root = temporary_root("hard-output-gate");
        let service = test_service_with_output_retain(&root, 8);
        let endpoint = 72;
        let cancellation = super::super::ConnectionCancellation::default();
        let limit = Arc::new(super::super::ExtensionOutboxLimit::with_limits(
            cancellation.clone(),
            3,
            1,
        ));
        let tracking = Arc::new(super::super::OutboxTracking {
            queued_frames: Arc::new(AtomicUsize::new(0)),
            queued_bytes: Arc::new(AtomicUsize::new(0)),
            extension_limit: Some(limit),
            drain_notify: std::sync::Mutex::new(None),
        });
        let (tx, mut rx) = mpsc::unbounded_channel();
        let sender = super::super::TrackedOutboxSender::ordered(tx, tracking);
        let mut inner = service.inner.blocking_lock();
        inner.endpoints.insert(endpoint, sender);
        inner.definitions.insert(1, output_definition(1, endpoint));
        assert!(retain_and_fanout(&mut inner, 1, vec![1; 4], 8).is_some());
        let before = Arc::strong_count(&inner.definitions[&1].retained[0].packet);
        assert!(matches!(
            schedule_one_locked(&mut inner, endpoint, None),
            ScheduleOutcome::Closed
        ));
        assert_eq!(
            Arc::strong_count(&inner.definitions[&1].retained[0].packet),
            before
        );
        assert!(rx.try_recv().is_err());
        assert_eq!(
            cancellation.failure(),
            Some(super::super::ConnectionFailure::SlowConsumer)
        );
        drop(inner);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn cursor_scheduler_round_robins_without_reordering_each_extension() {
        let root = temporary_root("output-fairness");
        let service = test_service_with_output_retain(&root, 32);
        let endpoint = 73;
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut inner = service.inner.blocking_lock();
        inner.endpoints.insert(endpoint, tx.into());
        inner
            .definitions
            .insert(10, output_definition(10, endpoint));
        inner
            .definitions
            .insert(20, output_definition(20, endpoint));
        for (extension_id, packet) in [
            (10, vec![10, 1]),
            (10, vec![10, 2]),
            (20, vec![20, 1]),
            (20, vec![20, 2]),
        ] {
            assert!(retain_and_fanout(&mut inner, extension_id, packet, 32).is_some());
        }
        let mut last = None;
        for expected in [10, 20, 10, 20] {
            let ScheduleOutcome::Sent(extension_id) =
                schedule_one_locked(&mut inner, endpoint, last)
            else {
                panic!("scheduler stopped with retained records available");
            };
            assert_eq!(extension_id, expected);
            last = Some(extension_id);
        }
        drop(inner);
        let packets = std::iter::from_fn(|| rx.try_recv().ok()).collect::<Vec<_>>();
        assert_eq!(
            packets,
            vec![vec![10, 1], vec![20, 1], vec![10, 2], vec![20, 2]]
        );
        let _ = std::fs::remove_dir_all(root);
    }

    fn returning_module(code: i32) -> Vec<u8> {
        wat::parse_str(format!(
            "(module (memory (export \"memory\") 1) \
             (func (export \"blit_main\") (result i32) i32.const {code}))"
        ))
        .unwrap()
    }

    #[test]
    fn native_job_admission_failure_is_a_resource_limit_exit() {
        let driven = DrivenAttempt {
            outcome: AttemptOutcome::Cancelled,
            handler_closed_first: true,
            connection_failure: Some(super::super::ConnectionFailure::ResourceLimit),
            running_for: Duration::ZERO,
        };
        let (reason, code, detail, failure) = classify_outcome(&driven, None);
        assert_eq!(reason, EXT_EXIT_RESOURCE_LIMIT);
        assert_eq!(code, 0);
        assert!(detail.contains("resource limit"));
        assert!(failure);
    }

    #[test]
    fn startup_catalog_metadata_does_not_reserve_persistent_arguments() {
        let root = temporary_root("lazy-startup-arguments");
        let path = root.join("extensions.redb");
        let argument = "x".repeat(wire::EXT_MAX_ARG);
        let mut catalog = ExtensionCatalog::open(Some(path.clone()), 8).unwrap();
        for index in 0..8 {
            catalog
                .create_with_id(
                    index + 1,
                    [index as u8 + 1; 32],
                    format!("persistent-{index}"),
                    vec![argument.clone(); 8],
                    wire::EXT_RESTART_NEVER,
                )
                .unwrap();
        }
        drop(catalog);

        let reopened = ExtensionCatalog::open(Some(path), 8).unwrap();
        let definitions = reopened
            .list()
            .into_iter()
            .map(definition_from_persistent)
            .collect::<Vec<_>>();
        let budget = ArgumentBudget::new(1);
        assert_eq!(budget.used(), 0);
        assert!(definitions.iter().all(|definition| {
            definition.args.is_none()
                && definition.argument_reservation.is_none()
                && definition.argument_bytes > 0
        }));
        let _ = std::fs::remove_dir_all(root);
    }

    fn insert_module(service: &ExtensionService, module: &[u8]) -> ObjectHash {
        let hash = *blake3::hash(module).as_bytes();
        let mut inner = service.inner.blocking_lock();
        let store = inner.store.as_mut().unwrap();
        store
            .begin_upload(900, hash, module.len() as u64, Instant::now())
            .unwrap();
        assert_eq!(
            store
                .put_chunk(
                    900,
                    hash,
                    0,
                    module.len() as u64,
                    module,
                    true,
                    Instant::now(),
                    |bytes| {
                        validate_extension_object(bytes, &WasmiHostConfig::default())
                            .map_err(|error| error.to_string())
                    },
                )
                .unwrap(),
            PutChunk::Committed {
                size: module.len() as u64
            }
        );
        hash
    }

    async fn register_test_endpoint(
        service: &Arc<ExtensionService>,
        endpoint: u64,
    ) -> mpsc::UnboundedReceiver<Vec<u8>> {
        let (sender, receiver) = mpsc::unbounded_channel();
        service
            .register_untracked_endpoint(endpoint, sender.into())
            .await;
        receiver
    }

    fn run_packet(nonce: u16, hash: ObjectHash) -> Vec<u8> {
        wire::msg_extension_run(&ExtensionRunRequest {
            nonce,
            flags: 0,
            restart: wire::EXT_RESTART_NEVER,
            expected_extension_id: 0,
            expected_definition_revision: 0,
            hash,
            name: "test",
            args: Vec::new(),
        })
        .unwrap()
    }

    async fn wait_for_exit(
        receiver: &mut mpsc::UnboundedReceiver<Vec<u8>>,
    ) -> wire::ExtensionExit<'static> {
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let packet = receiver
                    .recv()
                    .await
                    .expect("extension endpoint stayed live");
                if let Some(ExtensionMessage::Exit(exit)) =
                    wire::parse_extension_message(&packet).unwrap()
                {
                    return wire::ExtensionExit {
                        extension_id: exit.extension_id,
                        definition_revision: exit.definition_revision,
                        attempt: exit.attempt,
                        task_id: exit.task_id,
                        output_sequence: exit.output_sequence,
                        reason: exit.reason,
                        code: exit.code,
                        next_start_unix_ms: exit.next_start_unix_ms,
                        detail: Box::leak(exit.detail.to_owned().into_boxed_str()),
                    };
                }
            }
        })
        .await
        .expect("extension produced a terminal record")
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn cache_hit_runs_through_generic_handler_and_drains_before_exit() {
        let root = temporary_root("hit");
        let service = test_service(&root);
        let module = returning_module(7);
        let hash = tokio::task::block_in_place(|| insert_module(&service, &module));
        let state = test_state(service.clone());
        let endpoint = 41;
        let mut receiver = register_test_endpoint(&service, endpoint).await;
        assert_eq!(
            service
                .dispatch(
                    state,
                    endpoint,
                    &super::super::ConnectionOrigin::Network,
                    &run_packet(1, hash),
                )
                .await,
            DispatchOutcome::Continue
        );
        let first = receiver.recv().await.unwrap();
        let Some(ExtensionMessage::Status(status)) = wire::parse_extension_message(&first).unwrap()
        else {
            panic!("run did not receive its correlated status")
        };
        assert_eq!(status.status, EXT_STATUS_OK);
        let exit = wait_for_exit(&mut receiver).await;
        assert_eq!(exit.reason, EXT_EXIT_RETURNED);
        assert_eq!(exit.code, 7);
        assert_ne!(exit.task_id, 0);
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn quickjs_source_runs_through_generic_handler() {
        let root = temporary_root("quickjs-hit");
        let service = test_service(&root);
        let source = b"export default function () { return 17; }";
        let hash = tokio::task::block_in_place(|| insert_module(&service, source));
        let state = test_state(service.clone());
        let endpoint = 411;
        let mut receiver = register_test_endpoint(&service, endpoint).await;
        assert_eq!(
            service
                .dispatch(
                    state,
                    endpoint,
                    &super::super::ConnectionOrigin::Network,
                    &run_packet(101, hash),
                )
                .await,
            DispatchOutcome::Continue
        );
        let first = receiver.recv().await.unwrap();
        let Some(ExtensionMessage::Status(status)) = wire::parse_extension_message(&first).unwrap()
        else {
            panic!("run did not receive its correlated status")
        };
        assert_eq!(status.status, EXT_STATUS_OK);
        let exit = wait_for_exit(&mut receiver).await;
        assert_eq!(exit.reason, EXT_EXIT_RETURNED);
        assert_eq!(exit.code, 17);
        assert_ne!(exit.task_id, 0);
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn cache_miss_upload_acknowledges_then_starts_pending_creation() {
        let root = temporary_root("miss");
        let service = test_service(&root);
        let state = test_state(service.clone());
        let module = returning_module(0);
        let hash = *blake3::hash(&module).as_bytes();
        let endpoint = 42;
        let mut receiver = register_test_endpoint(&service, endpoint).await;
        service
            .dispatch(
                state.clone(),
                endpoint,
                &super::super::ConnectionOrigin::Network,
                &run_packet(2, hash),
            )
            .await;
        let status_packet = receiver.recv().await.unwrap();
        let Some(ExtensionMessage::Status(status)) =
            wire::parse_extension_message(&status_packet).unwrap()
        else {
            panic!("missing run status")
        };
        assert_eq!(status.phase, EXT_PHASE_NEED_OBJECT);

        let put = wire::msg_extension_put(&wire::ExtensionPutRequest {
            nonce: 3,
            flags: EXT_PUT_BEGIN | EXT_PUT_FINAL,
            hash,
            offset: 0,
            total_size: module.len() as u64,
            data: &module,
        })
        .unwrap();
        service
            .dispatch(
                state,
                endpoint,
                &super::super::ConnectionOrigin::Network,
                &put,
            )
            .await;
        let mut saw_put = false;
        let mut saw_exit = false;
        tokio::time::timeout(Duration::from_secs(5), async {
            while !saw_exit {
                let packet = receiver.recv().await.unwrap();
                match wire::parse_extension_message(&packet).unwrap() {
                    Some(ExtensionMessage::PutStatus(status)) => {
                        assert_eq!(status.status, EXT_STATUS_OK);
                        saw_put = true;
                    }
                    Some(ExtensionMessage::Exit(exit)) => {
                        assert!(
                            saw_put,
                            "upload acknowledgement must precede automatic start"
                        );
                        // The detail is the whole diagnosis and the bare
                        // comparison threw it away: this has failed in CI as
                        // `left: 5, right: 0` — a protocol violation — which
                        // says nothing about which of the two ways to reach
                        // one it took, and it does not reproduce locally.
                        assert_eq!(
                            exit.reason, EXT_EXIT_RETURNED,
                            "guest exited {} instead of returning: {}",
                            exit.reason, exit.detail
                        );
                        saw_exit = true;
                    }
                    _ => {}
                }
            }
        })
        .await
        .expect("pending extension started after upload");
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn same_endpoint_control_bypasses_tracked_final_upload_validation() {
        let root = temporary_root("validation-unlocked-control");
        let service = test_service(&root);
        let state = test_state(service.clone());
        let module = returning_module(0);
        let hash = *blake3::hash(&module).as_bytes();
        let owner = 142;
        let mut owner_rx = register_test_endpoint(&service, owner).await;

        service
            .dispatch(
                state.clone(),
                owner,
                &super::super::ConnectionOrigin::Network,
                &run_packet(20, hash),
            )
            .await;
        let extension_id = loop {
            let packet = owner_rx.recv().await.unwrap();
            if let Some(ExtensionMessage::Status(status)) =
                wire::parse_extension_message(&packet).unwrap()
            {
                assert_eq!(status.phase, EXT_PHASE_NEED_OBJECT);
                break status.extension_id;
            }
        };

        let entered = Arc::new(Notify::new());
        let release = Arc::new((std::sync::Mutex::new(false), std::sync::Condvar::new()));
        struct ValidationRelease(Arc<(std::sync::Mutex<bool>, std::sync::Condvar)>);
        impl ValidationRelease {
            fn release(&self) {
                let (released, wake) = &*self.0;
                *released
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner) = true;
                wake.notify_all();
            }
        }
        impl Drop for ValidationRelease {
            fn drop(&mut self) {
                self.release();
            }
        }
        let release_guard = ValidationRelease(Arc::clone(&release));
        let hook_entered = Arc::clone(&entered);
        let hook_release = Arc::clone(&release);
        *service
            .validation_hook
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(Arc::new(move || {
            hook_entered.notify_one();
            let (released, wake) = &*hook_release;
            let mut released = released
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            while !*released {
                released = wake
                    .wait(released)
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
            }
        }));

        let put = wire::msg_extension_put(&wire::ExtensionPutRequest {
            nonce: 21,
            flags: EXT_PUT_BEGIN | EXT_PUT_FINAL,
            hash,
            offset: 0,
            total_size: module.len() as u64,
            data: &module,
        })
        .unwrap();
        let cancellation = super::super::ConnectionCancellation::default();
        let jobs = state.extension_jobs.endpoint(cancellation);
        assert_eq!(
            tokio::time::timeout(
                Duration::from_millis(250),
                service.dispatch_owned(
                    state.clone(),
                    owner,
                    &super::super::ConnectionOrigin::Network,
                    put,
                    jobs.clone(),
                ),
            )
            .await
            .expect("reader returned immediately after upload admission"),
            DispatchOutcome::Continue
        );
        tokio::time::timeout(Duration::from_secs(2), entered.notified())
            .await
            .expect("upload reached detached validation");

        for (nonce, action) in [(22, EXT_CONTROL_STATUS), (23, EXT_CONTROL_CANCEL)] {
            let packet = wire::msg_extension_control(nonce, extension_id, action).unwrap();
            tokio::time::timeout(
                Duration::from_millis(250),
                service.dispatch_owned(
                    state.clone(),
                    owner,
                    &super::super::ConnectionOrigin::Network,
                    packet,
                    jobs.clone(),
                ),
            )
            .await
            .expect("control request was not blocked by module validation");
            let status = tokio::time::timeout(Duration::from_millis(250), async {
                loop {
                    let response = owner_rx.recv().await.unwrap();
                    if let Some(ExtensionMessage::Status(status)) =
                        wire::parse_extension_message(&response).unwrap()
                        && status.nonce == nonce
                    {
                        break status.status;
                    }
                }
            })
            .await
            .expect("control response was not blocked by module validation");
            assert_eq!(status, EXT_STATUS_OK);
        }

        let draining_jobs = jobs.clone();
        let mut draining = Box::pin(async move { draining_jobs.cancel_and_drain().await });
        assert!(
            tokio::time::timeout(Duration::from_millis(100), &mut draining)
                .await
                .is_err(),
            "cleanup detached an active non-cancellable validation"
        );
        release_guard.release();
        tokio::time::timeout(Duration::from_secs(5), draining)
            .await
            .expect("cleanup joined validation after release");
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn catalog_commit_keeps_control_transient_startup_and_shutdown_live() {
        let root = temporary_root("catalog-unlocked-control");
        let service = test_service(&root);
        let state = test_state(Arc::clone(&service));
        let module = returning_module(0);
        let hash = tokio::task::block_in_place(|| insert_module(&service, &module));
        let extension_id = 779;
        let persistent = service
            .catalog_call(move |catalog| {
                catalog.create_with_id(
                    extension_id,
                    hash,
                    "catalog-stall".into(),
                    Vec::new(),
                    wire::EXT_RESTART_NEVER,
                )
            })
            .await
            .unwrap();
        {
            let mut inner = service.inner.lock().await;
            inner.store.as_mut().unwrap().pin(&hash).unwrap();
            let mut definition = definition_from_persistent(persistent);
            definition.object_pinned = true;
            inner.definitions.insert(extension_id, definition);
        }
        let endpoint = 146;
        let mut receiver = register_test_endpoint(&service, endpoint).await;

        let entered = Arc::new(Notify::new());
        let release = Arc::new((std::sync::Mutex::new(false), std::sync::Condvar::new()));
        struct CatalogRelease(Arc<(std::sync::Mutex<bool>, std::sync::Condvar)>);
        impl CatalogRelease {
            fn release(&self) {
                let (released, wake) = &*self.0;
                *released
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner) = true;
                wake.notify_all();
            }
        }
        impl Drop for CatalogRelease {
            fn drop(&mut self) {
                self.release();
            }
        }
        let release_guard = CatalogRelease(Arc::clone(&release));
        let hook_entered = Arc::clone(&entered);
        let hook_release = Arc::clone(&release);
        *service
            .catalog_hook
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(Arc::new(move || {
            hook_entered.notify_one();
            let (released, wake) = &*hook_release;
            let mut released = released
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            while !*released {
                released = wake
                    .wait(released)
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
            }
        }));

        let disabling = {
            let service = Arc::clone(&service);
            let state = state.clone();
            tokio::spawn(async move {
                service
                    .handle_control(state, endpoint, 50, extension_id, EXT_CONTROL_DISABLE)
                    .await;
            })
        };
        tokio::time::timeout(Duration::from_secs(2), entered.notified())
            .await
            .expect("durable control reached the catalog lane");

        tokio::time::timeout(
            Duration::from_millis(250),
            service.handle_control(
                state.clone(),
                endpoint,
                51,
                extension_id,
                EXT_CONTROL_STATUS,
            ),
        )
        .await
        .expect("STATUS acquired service state while catalog I/O was stalled");
        let status = tokio::time::timeout(Duration::from_millis(250), async {
            loop {
                let packet = receiver.recv().await.unwrap();
                if let Some(ExtensionMessage::Status(status)) =
                    wire::parse_extension_message(&packet).unwrap()
                    && status.nonce == 51
                {
                    break status.status;
                }
            }
        })
        .await
        .expect("STATUS response was not blocked by catalog I/O");
        assert_eq!(status, EXT_STATUS_OK);

        service
            .dispatch(
                state.clone(),
                endpoint,
                &super::super::ConnectionOrigin::Network,
                &run_packet(52, hash),
            )
            .await;
        let transient_exit =
            tokio::time::timeout(Duration::from_millis(250), wait_for_exit(&mut receiver))
                .await
                .expect("transient startup was not blocked by catalog I/O");
        assert_eq!(transient_exit.reason, EXT_EXIT_RETURNED);

        tokio::time::timeout(Duration::from_millis(250), service.begin_shutdown())
            .await
            .expect("shutdown admission was not blocked by catalog I/O");

        release_guard.release();
        tokio::time::timeout(Duration::from_secs(2), disabling)
            .await
            .expect("durable control completed after catalog release")
            .unwrap();
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn network_final_validation_holds_exact_request_byte_charge() {
        let root = temporary_root("network-validation-charge");
        let service = test_service(&root);
        let state = test_state(Arc::clone(&service));
        let module = returning_module(0);
        let hash = *blake3::hash(&module).as_bytes();
        let endpoint = 144;
        let _receiver = register_test_endpoint(&service, endpoint).await;
        let entered = Arc::new(Notify::new());
        let release = Arc::new((std::sync::Mutex::new(false), std::sync::Condvar::new()));
        struct ReleaseOnDrop(Arc<(std::sync::Mutex<bool>, std::sync::Condvar)>);
        impl ReleaseOnDrop {
            fn release(&self) {
                let (released, wake) = &*self.0;
                *released
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner) = true;
                wake.notify_all();
            }
        }
        impl Drop for ReleaseOnDrop {
            fn drop(&mut self) {
                self.release();
            }
        }
        let release_guard = ReleaseOnDrop(Arc::clone(&release));
        let hook_entered = Arc::clone(&entered);
        let hook_release = Arc::clone(&release);
        *service
            .validation_hook
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(Arc::new(move || {
            hook_entered.notify_one();
            let (released, wake) = &*hook_release;
            let mut released = released
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            while !*released {
                released = wake
                    .wait(released)
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
            }
        }));
        let put = wire::msg_extension_put(&wire::ExtensionPutRequest {
            nonce: 45,
            flags: EXT_PUT_BEGIN | EXT_PUT_FINAL,
            hash,
            offset: 0,
            total_size: module.len() as u64,
            data: &module,
        })
        .unwrap();
        let dispatch = {
            let service = Arc::clone(&service);
            tokio::spawn(async move {
                service
                    .dispatch(
                        state,
                        endpoint,
                        &super::super::ConnectionOrigin::Network,
                        &put,
                    )
                    .await
            })
        };
        tokio::time::timeout(Duration::from_secs(2), entered.notified())
            .await
            .expect("network final reached validation");
        assert_eq!(service.validation_request_budget.used(), module.len());
        release_guard.release();
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(5), dispatch)
                .await
                .expect("network validation completed")
                .unwrap(),
            DispatchOutcome::Continue
        );
        assert_eq!(service.validation_request_budget.used(), 0);
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn same_endpoint_control_bypasses_stalled_chunk_filesystem_io() {
        let root = temporary_root("storage-unlocked-control");
        let service = test_service(&root);
        let state = test_state(Arc::clone(&service));
        let module = returning_module(0);
        let hash = *blake3::hash(&module).as_bytes();
        let owner = 143;
        let mut owner_rx = register_test_endpoint(&service, owner).await;
        service
            .dispatch(
                state.clone(),
                owner,
                &super::super::ConnectionOrigin::Network,
                &run_packet(40, hash),
            )
            .await;
        let extension_id = loop {
            let packet = owner_rx.recv().await.unwrap();
            if let Some(ExtensionMessage::Status(status)) =
                wire::parse_extension_message(&packet).unwrap()
                && status.nonce == 40
            {
                break status.extension_id;
            }
        };

        let begin = wire::msg_extension_put(&wire::ExtensionPutRequest {
            nonce: 41,
            flags: EXT_PUT_BEGIN,
            hash,
            offset: 0,
            total_size: module.len() as u64,
            data: &[],
        })
        .unwrap();
        service
            .dispatch(
                state.clone(),
                owner,
                &super::super::ConnectionOrigin::Network,
                &begin,
            )
            .await;
        loop {
            let packet = owner_rx.recv().await.unwrap();
            if let Some(ExtensionMessage::PutStatus(status)) =
                wire::parse_extension_message(&packet).unwrap()
                && status.nonce == 41
            {
                assert_eq!(status.status, EXT_STATUS_OK);
                break;
            }
        }

        let entered = Arc::new(Notify::new());
        let release = Arc::new((std::sync::Mutex::new(false), std::sync::Condvar::new()));
        let hook_entered = Arc::clone(&entered);
        let hook_release = Arc::clone(&release);
        *service
            .storage_hook
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(Arc::new(move || {
            hook_entered.notify_one();
            let (released, wake) = &*hook_release;
            let mut released = released
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            while !*released {
                released = wake
                    .wait(released)
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
            }
        }));
        let split = module.len() / 2;
        let chunk = wire::msg_extension_put(&wire::ExtensionPutRequest {
            nonce: 42,
            flags: 0,
            hash,
            offset: 0,
            total_size: module.len() as u64,
            data: &module[..split],
        })
        .unwrap();
        let cancellation = super::super::ConnectionCancellation::default();
        let jobs = state.extension_jobs.endpoint(cancellation);
        assert_eq!(
            service
                .dispatch_owned(
                    state.clone(),
                    owner,
                    &super::super::ConnectionOrigin::Network,
                    chunk,
                    jobs.clone(),
                )
                .await,
            DispatchOutcome::Continue
        );
        tokio::time::timeout(Duration::from_secs(2), entered.notified())
            .await
            .expect("chunk reached detached filesystem lane");

        for (nonce, action) in [(43, EXT_CONTROL_STATUS), (44, EXT_CONTROL_CANCEL)] {
            let packet = wire::msg_extension_control(nonce, extension_id, action).unwrap();
            tokio::time::timeout(
                Duration::from_millis(250),
                service.dispatch_owned(
                    state.clone(),
                    owner,
                    &super::super::ConnectionOrigin::Network,
                    packet,
                    jobs.clone(),
                ),
            )
            .await
            .expect("control dispatch remained independent of stalled storage");
            tokio::time::timeout(Duration::from_millis(250), async {
                loop {
                    let packet = owner_rx.recv().await.unwrap();
                    if let Some(ExtensionMessage::Status(status)) =
                        wire::parse_extension_message(&packet).unwrap()
                        && status.nonce == nonce
                    {
                        assert_eq!(status.status, EXT_STATUS_OK);
                        break;
                    }
                }
            })
            .await
            .expect("control response remained independent of stalled storage");
        }

        {
            let (released, wake) = &*release;
            *released
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = true;
            wake.notify_all();
        }
        tokio::time::timeout(Duration::from_secs(5), jobs.cancel_and_drain())
            .await
            .expect("detached chunk completed after storage release");
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn endpoint_cleanup_cancels_validation_waiter_before_launch() {
        let root = temporary_root("validation-pending-cancel");
        let service = test_service(&root);
        let state = test_state(service.clone());
        let endpoint = 144;
        let mut receiver = register_test_endpoint(&service, endpoint).await;
        let held_validation = service.validating.clone().acquire_owned().await.unwrap();
        let cancellation = super::super::ConnectionCancellation::default();
        let jobs = state.extension_jobs.endpoint(cancellation);

        assert_eq!(
            service
                .dispatch_owned(
                    state,
                    endpoint,
                    &super::super::ConnectionOrigin::Network,
                    run_packet(24, [7; 32]),
                    jobs.clone(),
                )
                .await,
            DispatchOutcome::Continue
        );
        tokio::task::yield_now().await;
        tokio::time::timeout(Duration::from_millis(250), jobs.cancel_and_drain())
            .await
            .expect("cleanup cancelled a validation waiter");
        drop(held_validation);
        assert!(
            tokio::time::timeout(Duration::from_millis(50), receiver.recv())
                .await
                .is_err(),
            "cancelled validation waiter dispatched a reply"
        );
        assert!(service.inner.lock().await.definitions.is_empty());
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn running_waiter_keeps_fifo_position_across_wake_checks() {
        let root = temporary_root("running-fifo");
        let service = test_service_with_limits(
            &root,
            DEFAULT_OUTPUT_RETAIN_MAX,
            DEFAULT_ARGUMENT_STORE_MAX,
            Duration::from_millis(10),
            1,
        );
        let state = test_state(service.clone());
        let endpoint = 144;
        let mut receiver = register_test_endpoint(&service, endpoint).await;
        let hash = [77; 32];
        service
            .dispatch(
                state,
                endpoint,
                &super::super::ConnectionOrigin::Network,
                &run_packet(24, hash),
            )
            .await;
        let extension_id = loop {
            let packet = receiver.recv().await.unwrap();
            if let Some(ExtensionMessage::Status(status)) =
                wire::parse_extension_message(&packet).unwrap()
            {
                break status.extension_id;
            }
        };
        let generation = service
            .inner
            .lock()
            .await
            .definitions
            .get(&extension_id)
            .unwrap()
            .generation;
        let held = service.running.clone().acquire_owned().await.unwrap();
        let first_wake = Arc::new(Notify::new());
        let second_wake = Arc::new(Notify::new());
        let (winner_tx, mut winner_rx) = mpsc::unbounded_channel();

        let first = {
            let service = Arc::clone(&service);
            let wake = Arc::clone(&first_wake);
            let winner_tx = winner_tx.clone();
            tokio::spawn(async move {
                let permit = service
                    .acquire_running_permit(extension_id, generation, wake)
                    .await
                    .unwrap();
                winner_tx.send(1).unwrap();
                drop(permit);
            })
        };
        tokio::time::sleep(Duration::from_millis(20)).await;
        let second = {
            let service = Arc::clone(&service);
            let wake = Arc::clone(&second_wake);
            let winner_tx = winner_tx.clone();
            tokio::spawn(async move {
                let permit = service
                    .acquire_running_permit(extension_id, generation, wake)
                    .await
                    .unwrap();
                winner_tx.send(2).unwrap();
                drop(permit);
            })
        };
        tokio::time::sleep(Duration::from_millis(20)).await;
        first_wake.notify_one();
        tokio::task::yield_now().await;
        drop(held);

        assert_eq!(
            tokio::time::timeout(Duration::from_secs(1), winner_rx.recv())
                .await
                .unwrap(),
            Some(1),
            "eligibility wakes must not move the oldest semaphore waiter to the tail"
        );
        first.await.unwrap();
        second.await.unwrap();
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn argument_contention_releases_running_permit_for_resident_transient() {
        let root = temporary_root("argument-starvation");
        let service = test_service_with_limits(
            &root,
            8 * 1024 * 1024,
            encoded_argument_bytes(&[]),
            Duration::from_millis(10),
            1,
        );
        let state = test_state(service.clone());
        let endpoint = 43;
        let mut receiver = register_test_endpoint(&service, endpoint).await;

        // A pending transient owns the complete two-byte empty-argument
        // encoding, filling the deliberately tiny store.
        let transient_module = returning_module(1);
        let transient_hash = *blake3::hash(&transient_module).as_bytes();
        service
            .dispatch(
                state.clone(),
                endpoint,
                &super::super::ConnectionOrigin::Network,
                &run_packet(4, transient_hash),
            )
            .await;
        let status_packet = receiver.recv().await.unwrap();
        let Some(ExtensionMessage::Status(transient_status)) =
            wire::parse_extension_message(&status_packet).unwrap()
        else {
            panic!("missing pending transient status")
        };
        assert_eq!(transient_status.phase, EXT_PHASE_NEED_OBJECT);
        assert_eq!(service.argument_budget.used(), 2);

        // Install a committed persistent definition without resident args and
        // make it contend first for the sole running permit.
        let persistent_module = returning_module(2);
        let persistent_hash =
            tokio::task::block_in_place(|| insert_module(&service, &persistent_module));
        let persistent_id = 777;
        let persistent = service
            .catalog_call(move |catalog| {
                catalog.create_with_id(
                    persistent_id,
                    persistent_hash,
                    "persistent".into(),
                    Vec::new(),
                    wire::EXT_RESTART_NEVER,
                )
            })
            .await
            .unwrap();
        {
            let mut inner = service.inner.lock().await;
            inner.store.as_mut().unwrap().pin(&persistent_hash).unwrap();
            let mut definition = definition_from_persistent(persistent);
            definition.object_pinned = true;
            inner.definitions.insert(persistent_id, definition);
        }
        service
            .ensure_supervisor(state.clone(), persistent_id)
            .await;
        tokio::time::timeout(Duration::from_secs(2), async {
            while service.argument_budget.contentions() == 0
                || service.running.available_permits() == 0
            {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("persistent attempt reached argument admission");
        assert_eq!(
            service.running.available_permits(),
            1,
            "argument waiter must not retain the running permit"
        );

        // Once its object arrives, the resident transient must be able to use
        // that permit, terminate, and release the budget so persistence can
        // retry without ever recording a failed attempt for contention.
        let put = wire::msg_extension_put(&wire::ExtensionPutRequest {
            nonce: 5,
            flags: EXT_PUT_BEGIN | EXT_PUT_FINAL,
            hash: transient_hash,
            offset: 0,
            total_size: transient_module.len() as u64,
            data: &transient_module,
        })
        .unwrap();
        service
            .dispatch(
                state,
                endpoint,
                &super::super::ConnectionOrigin::Network,
                &put,
            )
            .await;
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let finished = service
                    .inner
                    .lock()
                    .await
                    .definitions
                    .get(&persistent_id)
                    .is_some_and(|definition| {
                        definition.attempt == 1 && definition.phase == EXT_PHASE_STOPPED
                    });
                if finished && service.argument_budget.used() == 0 {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("transient drained and persistent attempt completed");
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn persistent_arguments_over_budget_block_before_attempt_allocation() {
        let root = temporary_root("argument-over-budget");
        let service =
            test_service_with_limits(&root, 8 * 1024 * 1024, 1, Duration::from_millis(10), 1);
        let state = test_state(service.clone());
        let module = returning_module(3);
        let hash = tokio::task::block_in_place(|| insert_module(&service, &module));
        let extension_id = 778;
        let persistent = service
            .catalog_call(move |catalog| {
                catalog.create_with_id(
                    extension_id,
                    hash,
                    "over-budget".into(),
                    Vec::new(),
                    wire::EXT_RESTART_NEVER,
                )
            })
            .await
            .unwrap();
        {
            let mut inner = service.inner.lock().await;
            inner.store.as_mut().unwrap().pin(&hash).unwrap();
            let mut definition = definition_from_persistent(persistent);
            definition.object_pinned = true;
            inner.definitions.insert(extension_id, definition);
        }
        service.ensure_supervisor(state, extension_id).await;
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                let blocked = service
                    .inner
                    .lock()
                    .await
                    .definitions
                    .get(&extension_id)
                    .is_some_and(|definition| definition.phase == EXT_PHASE_BLOCKED);
                if blocked {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("oversized persistent arguments became blocked");
        let inner = service.inner.lock().await;
        assert_eq!(inner.definitions.get(&extension_id).unwrap().attempt, 0);
        assert_eq!(service.argument_budget.used(), 0);
        drop(inner);
        let durable = service
            .catalog_call(move |catalog| Ok(catalog.get(extension_id).cloned()))
            .await
            .unwrap()
            .unwrap();
        assert!(durable.blocked);
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn compact_terminal_replay_survives_zero_retention_and_late_attach() {
        let root = temporary_root("terminal-replay");
        let service = test_service_with_output_retain(&root, 0);
        let module = returning_module(7);
        let hash = tokio::task::block_in_place(|| insert_module(&service, &module));
        let state = test_state(service.clone());
        let owner = 51;
        let mut owner_rx = register_test_endpoint(&service, owner).await;
        service
            .dispatch(
                state.clone(),
                owner,
                &super::super::ConnectionOrigin::Network,
                &run_packet(11, hash),
            )
            .await;

        let correlated = owner_rx.recv().await.unwrap();
        let Some(ExtensionMessage::Status(created)) =
            wire::parse_extension_message(&correlated).unwrap()
        else {
            panic!("run did not receive its correlated status");
        };
        let extension_id = created.extension_id;
        let (exit_sequence, first_terminal_status) =
            tokio::time::timeout(Duration::from_secs(5), async {
                let mut exit_sequence = None;
                let mut terminal_status = None;
                loop {
                    let packet = owner_rx.recv().await.unwrap();
                    match wire::parse_extension_message(&packet).unwrap() {
                        Some(ExtensionMessage::Exit(exit)) if exit.extension_id == extension_id => {
                            exit_sequence = Some(exit.output_sequence);
                        }
                        Some(ExtensionMessage::Info(wire::ExtensionInfo::Status(status)))
                            if status.extension_id == extension_id
                                && status.phase == EXT_PHASE_STOPPED =>
                        {
                            assert!(
                                exit_sequence.is_some(),
                                "current follower saw terminal STATUS before EXIT"
                            );
                            terminal_status = Some(status.output_sequence);
                        }
                        Some(ExtensionMessage::Info(wire::ExtensionInfo::ReplayDone {
                            extension_id: marker_id,
                            through_sequence,
                        })) if marker_id == extension_id
                            && terminal_status == Some(through_sequence) =>
                        {
                            break (exit_sequence.unwrap(), through_sequence);
                        }
                        _ => {}
                    }
                }
            })
            .await
            .expect("current follower received terminal records and marker");
        assert!(exit_sequence < first_terminal_status);

        // Re-emitting an equivalent terminal snapshot replaces the compact
        // STATUS instead of growing the supervisor reserve. The normal ring is
        // empty at a zero-byte global limit, so the late replay below can only
        // come from the compact records.
        let final_terminal_status = {
            let mut inner = service.inner.lock().await;
            emit_lifecycle_locked(&mut inner, extension_id, 0);
            emit_lifecycle_locked(&mut inner, extension_id, 0);
            let definition = inner.definitions.get(&extension_id).unwrap();
            assert!(definition.retained.is_empty());
            assert_eq!(definition.retained_bytes, 0);
            assert_eq!(inner.retained_bytes, 0);
            assert_eq!(definition.terminal_replay.len(), 2);
            assert_eq!(definition.terminal_replay[0].sequence, exit_sequence);
            assert_eq!(
                definition.terminal_replay[0].packet.first(),
                Some(&wire::EXT_EXIT)
            );
            let mut overlap = definition.clone();
            overlap.retained = overlap.terminal_replay.clone();
            let merged = merged_replay(
                &overlap,
                exit_sequence,
                definition.terminal_replay[1].sequence,
            );
            assert_eq!(
                merged
                    .iter()
                    .map(|(sequence, _)| *sequence)
                    .collect::<Vec<_>>(),
                vec![exit_sequence, definition.terminal_replay[1].sequence]
            );
            definition.terminal_replay[1].sequence
        };
        assert!(final_terminal_status > first_terminal_status);

        let late = 52;
        let mut late_rx = register_test_endpoint(&service, late).await;
        let attach = wire::msg_extension_control(12, extension_id, EXT_CONTROL_ATTACH).unwrap();
        service
            .dispatch(
                state,
                late,
                &super::super::ConnectionOrigin::Network,
                &attach,
            )
            .await;

        let reply = late_rx.recv().await.unwrap();
        let Some(ExtensionMessage::Status(status)) = wire::parse_extension_message(&reply).unwrap()
        else {
            panic!("attach did not receive its correlated status");
        };
        assert_eq!(status.nonce, 12);
        assert_eq!(status.replay_from_sequence, exit_sequence);
        assert_eq!(status.output_sequence, final_terminal_status);

        let exit = late_rx.recv().await.unwrap();
        assert!(matches!(
            wire::parse_extension_message(&exit).unwrap(),
            Some(ExtensionMessage::Exit(exit))
                if exit.output_sequence == exit_sequence && exit.code == 7
        ));
        let terminal = late_rx.recv().await.unwrap();
        assert!(matches!(
            wire::parse_extension_message(&terminal).unwrap(),
            Some(ExtensionMessage::Info(wire::ExtensionInfo::Status(status)))
                if status.output_sequence == final_terminal_status
                    && status.phase == EXT_PHASE_STOPPED
        ));
        let marker = late_rx.recv().await.unwrap();
        assert!(matches!(
            wire::parse_extension_message(&marker).unwrap(),
            Some(ExtensionMessage::Info(wire::ExtensionInfo::ReplayDone {
                extension_id: marker_id,
                through_sequence,
            })) if marker_id == extension_id && through_sequence == final_terminal_status
        ));
        assert!(
            late_rx.try_recv().is_err(),
            "late attach replay duplicated a record"
        );
        let _ = std::fs::remove_dir_all(root);
    }
}
