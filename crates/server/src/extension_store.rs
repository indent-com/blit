//! Persistent, bounded content-addressed storage for extension Wasm objects.

use std::collections::{HashMap, HashSet};
use std::fs::{self, File, OpenOptions};
use std::io::{ErrorKind, Write};
use std::path::{Path, PathBuf};
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use std::time::{Duration, Instant};

pub type ObjectHash = [u8; 32];

const DEFAULT_MODULE_MAX: u64 = 64 * 1024 * 1024;
const DEFAULT_CACHE_MAX: u64 = 2 * 1024 * 1024 * 1024;
const DEFAULT_ENTRY_MAX: usize = 4096;
const DEFAULT_UPLOAD_MAX: usize = 32;
const DEFAULT_UPLOAD_MAX_PER_ENDPOINT: usize = 4;
const DEFAULT_UPLOAD_TIMEOUT: Duration = Duration::from_secs(5 * 60);
const MIN_QUANTUM: u64 = 4096;
const LRU_METADATA_FILE: &str = "lru-v1";
const LRU_METADATA_TEMP: &str = ".lru-v1.tmp";
const LRU_METADATA_MAGIC: &[u8; 8] = b"BLITLRU1";
const LRU_METADATA_HEADER: usize = 8 + 8 + 8;
const LRU_METADATA_RECORD: usize = 32 + 8;
const LRU_METADATA_CHECKSUM: usize = 32;

#[derive(Clone, Debug)]
pub struct ObjectStoreConfig {
    pub root: PathBuf,
    pub module_max: u64,
    pub cache_max: u64,
    pub entry_max: usize,
    pub upload_max: usize,
    pub upload_max_per_endpoint: usize,
    pub upload_timeout: Duration,
    pub allocation_quantum: u64,
}

impl ObjectStoreConfig {
    pub fn from_env() -> Option<Self> {
        let root = object_root()?;
        Some(Self {
            allocation_quantum: filesystem_allocation_quantum_near(&root),
            root,
            module_max: crate::deployment_u64("BLIT_EXT_MODULE_MAX", DEFAULT_MODULE_MAX)
                .min(DEFAULT_MODULE_MAX),
            cache_max: crate::deployment_u64("BLIT_EXT_OBJECT_CACHE_MAX", DEFAULT_CACHE_MAX),
            entry_max: crate::deployment_usize(
                "BLIT_EXT_OBJECT_CACHE_MAX_ENTRIES",
                DEFAULT_ENTRY_MAX,
            ),
            upload_max: crate::deployment_usize("BLIT_EXT_UPLOAD_MAX_ACTIVE", DEFAULT_UPLOAD_MAX),
            upload_max_per_endpoint: crate::deployment_usize(
                "BLIT_EXT_UPLOAD_MAX_PER_CLIENT",
                DEFAULT_UPLOAD_MAX_PER_ENDPOINT,
            ),
            upload_timeout: Duration::from_secs(crate::deployment_u64(
                "BLIT_EXT_UPLOAD_TIMEOUT",
                DEFAULT_UPLOAD_TIMEOUT.as_secs(),
            )),
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BeginUpload {
    Started,
    AlreadyHave { size: u64 },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PutChunk {
    Accepted { received: u64 },
    Committed { size: u64 },
    AlreadyHave { size: u64 },
}

/// Result of the lock-local portion of an upload chunk.
pub enum PreparedPut {
    Complete(PutChunk),
    Chunk(Box<ChunkUpload>),
    Final(Box<FinalUpload>),
    Abort(Box<UploadCleanup>, ObjectStoreError),
}

/// Lock-local outcome of BEGIN admission. Filesystem mutation is represented by
/// an owned token so the service can drop its state mutex before doing I/O.
pub enum PreparedBeginUpload {
    Complete(BeginUpload),
    Evict(Box<ObjectEviction>),
    Create(Box<UploadCreation>),
}

/// A temporary CAS pin and immutable path snapshot for detached reads.
///
/// Dropping the snapshot releases the temporary pin without reacquiring the
/// object-store owner. This lets hash, validation, and durability work run
/// outside higher-level service locks without opening an eviction window.
pub struct ObjectRead {
    hash: ObjectHash,
    path: PathBuf,
    size: u64,
    pins: Arc<AtomicUsize>,
}

impl ObjectRead {
    pub fn read_verified(&self) -> Result<Vec<u8>, ObjectStoreError> {
        if fs::metadata(&self.path)?.len() != self.size {
            return Err(ObjectStoreError::HashMismatch);
        }
        let bytes = fs::read(&self.path)?;
        if blake3::hash(&bytes).as_bytes() != &self.hash {
            return Err(ObjectStoreError::HashMismatch);
        }
        Ok(bytes)
    }

    pub fn sync(&self) -> Result<(), ObjectStoreError> {
        File::open(&self.path)?.sync_all()?;
        #[cfg(unix)]
        if let Some(parent) = self.path.parent() {
            File::open(parent)?.sync_all()?;
            if let Some(objects) = parent.parent() {
                File::open(objects)?.sync_all()?;
            }
        }
        Ok(())
    }

    pub fn remove_file(&self) -> Result<(), ObjectStoreError> {
        match fs::remove_file(&self.path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error.into()),
        }
    }
}

impl Drop for ObjectRead {
    fn drop(&mut self) {
        let previous = self.pins.fetch_sub(1, Ordering::AcqRel);
        debug_assert!(previous > 0);
    }
}

/// Exclusive ownership of a final upload after its metadata transition.
pub struct FinalUpload {
    session: u64,
    endpoint: u64,
    hash: ObjectHash,
    total_size: u64,
    charge: u64,
    path: PathBuf,
    final_path: PathBuf,
    file: File,
    hasher: blake3::Hasher,
}

/// Exclusive ownership of a non-final upload write.
pub struct ChunkUpload {
    session: u64,
    hash: ObjectHash,
    endpoint: u64,
    total_size: u64,
    received: u64,
    charge: u64,
    path: PathBuf,
    file: File,
    hasher: blake3::Hasher,
    last_activity: Instant,
}

pub struct ChunkUploadResult {
    session: u64,
    endpoint: u64,
    hash: ObjectHash,
    charge: u64,
    upload: Option<Upload>,
    release_reservation: bool,
    cleanup_path: Option<PathBuf>,
    result: Result<PutChunk, ObjectStoreError>,
}

/// A reserved BEGIN whose temporary file is created outside the store owner.
pub struct UploadCreation {
    session: u64,
    endpoint: u64,
    hash: ObjectHash,
    total_size: u64,
    charge: u64,
    root: PathBuf,
    now: Instant,
}

pub struct UploadCreationResult {
    session: u64,
    endpoint: u64,
    hash: ObjectHash,
    charge: u64,
    upload: Option<Upload>,
    release_reservation: bool,
    cleanup_path: Option<PathBuf>,
    result: Result<(), ObjectStoreError>,
}

pub enum UploadCreationCommit {
    Complete(Result<BeginUpload, ObjectStoreError>),
    Stale(Box<UploadCreationResult>),
}

pub enum ChunkUploadCommit {
    Complete(Result<PutChunk, ObjectStoreError>),
    Stale(Box<ChunkUploadResult>),
}

/// An object selected under the in-memory LRU and pinned-state lock. The file
/// deletion happens after that lock is released.
pub struct ObjectEviction {
    hash: ObjectHash,
    path: PathBuf,
    entry: ObjectEntry,
}

pub struct ObjectEvictionResult {
    eviction: ObjectEviction,
    removed: bool,
    result: Result<(), ObjectStoreError>,
}

/// An upload removed from admission state and awaiting detached file cleanup.
pub struct UploadCleanup {
    session: u64,
    endpoint: u64,
    hash: ObjectHash,
    path: PathBuf,
    charge: u64,
    file: Option<File>,
}

pub struct UploadCleanupResult {
    session: u64,
    endpoint: u64,
    hash: ObjectHash,
    charge: u64,
    path: Option<PathBuf>,
    removed: bool,
}

#[derive(Clone)]
struct PendingCleanup {
    session: u64,
    endpoint: u64,
    hash: ObjectHash,
    charge: u64,
    path: Option<PathBuf>,
}

pub struct CleanupRetry {
    pending: PendingCleanup,
}

pub struct CleanupRetryResult {
    pending: PendingCleanup,
    removed: bool,
}

/// Immutable durable-recency image. Encoding is lock-local; file publication is
/// performed by the caller after releasing the extension-service mutex.
pub struct LruSnapshot {
    root: PathBuf,
    revision: u64,
    bytes: Vec<u8>,
}

/// Detached finalization result consumed by [`ObjectStore::commit_final_upload`].
pub struct FinalUploadResult {
    session: u64,
    endpoint: u64,
    hash: ObjectHash,
    total_size: u64,
    charge: u64,
    release_reservation: bool,
    cleanup_path: Option<PathBuf>,
    result: Result<(), ObjectStoreError>,
}

impl FinalUpload {
    pub fn finish(
        self,
        data: &[u8],
        validate: impl FnOnce(&[u8]) -> Result<(), String>,
    ) -> FinalUploadResult {
        let Self {
            session,
            endpoint,
            hash,
            total_size,
            charge,
            path,
            final_path,
            file,
            mut hasher,
        } = self;
        let mut file = Some(file);
        let mut installed = false;
        let result = (|| -> Result<(), ObjectStoreError> {
            let upload_file = file.as_mut().expect("final upload file remained owned");
            upload_file.write_all(data)?;
            hasher.update(data);
            upload_file.flush()?;
            // Final uploads are made durable even for transient callers. In
            // addition to keeping all finalization I/O detached, this lets a
            // compatible pending persistent definition commit immediately
            // after the CAS transition without doing filesystem work under
            // the extension-service lock.
            upload_file.sync_all()?;
            drop(file.take());
            if hasher.finalize().as_bytes() != &hash {
                return Err(ObjectStoreError::HashMismatch);
            }
            let bytes = fs::read(&path)?;
            validate(&bytes).map_err(ObjectStoreError::InvalidModule)?;
            let parent = final_path
                .parent()
                .expect("object path always has a shard directory");
            fs::create_dir_all(parent)?;
            set_owner_directory(parent)?;
            #[cfg(unix)]
            fs::rename(&path, &final_path)?;
            #[cfg(not(unix))]
            {
                if final_path.exists() {
                    fs::remove_file(&final_path)?;
                }
                fs::rename(&path, &final_path)?;
            }
            installed = true;
            set_owner_file(&final_path)?;
            #[cfg(unix)]
            {
                File::open(parent)?.sync_all()?;
                if let Some(objects) = parent.parent() {
                    File::open(objects)?.sync_all()?;
                }
            }
            Ok(())
        })();
        // Close before cleanup on every error path, including write/fsync
        // failures. This is required for retryable deletion on Windows.
        drop(file.take());
        let (release_reservation, cleanup_path) = if result.is_err() {
            let cleanup = if installed {
                final_path.clone()
            } else {
                path.clone()
            };
            (remove_file_if_present(&cleanup).is_ok(), Some(cleanup))
        } else {
            (false, None)
        };
        FinalUploadResult {
            session,
            endpoint,
            hash,
            total_size,
            charge,
            release_reservation,
            cleanup_path,
            result,
        }
    }
}

impl ChunkUpload {
    pub fn finish(mut self, data: &[u8]) -> ChunkUploadResult {
        let result = self
            .file
            .write_all(data)
            .map_err(ObjectStoreError::from)
            .map(|()| {
                self.hasher.update(data);
                PutChunk::Accepted {
                    received: self.received,
                }
            });
        if result.is_ok() {
            return ChunkUploadResult {
                session: self.session,
                endpoint: self.endpoint,
                hash: self.hash,
                charge: self.charge,
                upload: Some(Upload {
                    session: self.session,
                    endpoint: self.endpoint,
                    total_size: self.total_size,
                    received: self.received,
                    charge: self.charge,
                    path: self.path,
                    file: self.file,
                    hasher: self.hasher,
                    last_activity: self.last_activity,
                }),
                release_reservation: false,
                cleanup_path: None,
                result,
            };
        }
        drop(self.file);
        let cleanup_path = self.path;
        let release_reservation = remove_file_if_present(&cleanup_path).is_ok();
        ChunkUploadResult {
            session: self.session,
            endpoint: self.endpoint,
            hash: self.hash,
            charge: self.charge,
            upload: None,
            release_reservation,
            cleanup_path: Some(cleanup_path),
            result,
        }
    }
}

impl UploadCreation {
    pub fn finish(self) -> UploadCreationResult {
        let mut created_path = None;
        let result = (|| -> Result<Upload, ObjectStoreError> {
            let (path, file) = create_upload_file(&self.root, self.endpoint, &self.hash)?;
            created_path = Some(path.clone());
            Ok(Upload {
                session: self.session,
                endpoint: self.endpoint,
                total_size: self.total_size,
                received: 0,
                charge: self.charge,
                path,
                file,
                hasher: blake3::Hasher::new(),
                last_activity: self.now,
            })
        })();
        match result {
            Ok(upload) => UploadCreationResult {
                session: self.session,
                endpoint: self.endpoint,
                hash: self.hash,
                charge: self.charge,
                upload: Some(upload),
                release_reservation: false,
                cleanup_path: None,
                result: Ok(()),
            },
            Err(error) => {
                let release_reservation = created_path
                    .as_deref()
                    .is_none_or(|path| remove_file_if_present(path).is_ok());
                UploadCreationResult {
                    session: self.session,
                    endpoint: self.endpoint,
                    hash: self.hash,
                    charge: self.charge,
                    upload: None,
                    release_reservation,
                    cleanup_path: created_path,
                    result: Err(error),
                }
            }
        }
    }
}

impl UploadCreationResult {
    pub fn cleanup(mut self) -> UploadCleanupResult {
        if let Some(upload) = self.upload.take() {
            return UploadCleanup {
                session: self.session,
                endpoint: self.endpoint,
                hash: self.hash,
                path: upload.path,
                charge: self.charge,
                file: Some(upload.file),
            }
            .finish();
        }
        UploadCleanupResult {
            session: self.session,
            endpoint: self.endpoint,
            hash: self.hash,
            charge: self.charge,
            path: self.cleanup_path,
            removed: self.release_reservation,
        }
    }
}

impl ChunkUploadResult {
    pub fn cleanup(mut self) -> UploadCleanupResult {
        if let Some(upload) = self.upload.take() {
            return UploadCleanup {
                session: self.session,
                endpoint: self.endpoint,
                hash: self.hash,
                path: upload.path,
                charge: self.charge,
                file: Some(upload.file),
            }
            .finish();
        }
        UploadCleanupResult {
            session: self.session,
            endpoint: self.endpoint,
            hash: self.hash,
            charge: self.charge,
            path: self.cleanup_path,
            removed: self.release_reservation,
        }
    }
}

impl ObjectEviction {
    pub fn finish(self) -> ObjectEvictionResult {
        let result = remove_file_if_present(&self.path);
        ObjectEvictionResult {
            eviction: self,
            removed: result.is_ok(),
            result,
        }
    }
}

impl UploadCleanup {
    pub fn finish(mut self) -> UploadCleanupResult {
        drop(self.file.take());
        let removed = remove_file_if_present(&self.path).is_ok();
        UploadCleanupResult {
            session: self.session,
            endpoint: self.endpoint,
            hash: self.hash,
            charge: self.charge,
            path: Some(self.path),
            removed,
        }
    }
}

impl CleanupRetry {
    pub fn finish(self) -> CleanupRetryResult {
        let removed = self
            .pending
            .path
            .as_deref()
            .is_some_and(|path| remove_file_if_present(path).is_ok());
        CleanupRetryResult {
            pending: self.pending,
            removed,
        }
    }
}

impl LruSnapshot {
    pub fn persist(&self) -> Result<(), ObjectStoreError> {
        write_lru_metadata_atomic(&self.root, &self.bytes)
    }
}

#[derive(Debug)]
pub enum ObjectStoreError {
    InvalidConfig(&'static str),
    InvalidUpload(&'static str),
    NotFound,
    Conflict,
    TooLarge,
    Budget,
    HashMismatch,
    InvalidModule(String),
    Io(std::io::Error),
}

impl std::fmt::Display for ObjectStoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidConfig(detail) | Self::InvalidUpload(detail) => f.write_str(detail),
            Self::NotFound => f.write_str("extension object or upload was not found"),
            Self::Conflict => f.write_str("another endpoint owns this object upload"),
            Self::TooLarge => f.write_str("extension module exceeds the configured limit"),
            Self::Budget => f.write_str("extension object cache budget exhausted"),
            Self::HashMismatch => f.write_str("extension object BLAKE3 digest does not match"),
            Self::InvalidModule(detail) => write!(f, "invalid extension module: {detail}"),
            Self::Io(error) => write!(f, "extension object storage failed: {error}"),
        }
    }
}

impl std::error::Error for ObjectStoreError {}

impl From<std::io::Error> for ObjectStoreError {
    fn from(value: std::io::Error) -> Self {
        Self::Io(value)
    }
}

#[derive(Clone, Debug)]
struct ObjectEntry {
    size: u64,
    charge: u64,
    last_used: u64,
    pins: Arc<AtomicUsize>,
    executable: bool,
}

#[derive(Clone, Copy)]
struct FinalizingUpload {
    endpoint: u64,
    session: u64,
}

struct Upload {
    session: u64,
    endpoint: u64,
    total_size: u64,
    received: u64,
    charge: u64,
    path: PathBuf,
    file: File,
    hasher: blake3::Hasher,
    last_activity: Instant,
}

/// One-process owner of the immutable raw-object cache and active uploads.
pub struct ObjectStore {
    config: ObjectStoreConfig,
    objects: HashMap<ObjectHash, ObjectEntry>,
    uploads: HashMap<ObjectHash, Upload>,
    finalizing: HashMap<ObjectHash, FinalizingUpload>,
    pending_cleanups: Vec<PendingCleanup>,
    charged_bytes: u64,
    charged_entries: usize,
    lru_clock: u64,
    lru_revision: u64,
    lru_persisted_revision: u64,
    lru_dirty: bool,
    next_upload_session: u64,
}

impl ObjectStore {
    pub fn open(config: ObjectStoreConfig) -> Result<Self, ObjectStoreError> {
        validate_config(&config)?;
        let objects_dir = config.root.join("objects");
        let tmp_dir = config.root.join("tmp");
        fs::create_dir_all(&objects_dir)?;
        fs::create_dir_all(&tmp_dir)?;
        set_owner_directory(&config.root)?;
        set_owner_directory(&objects_dir)?;
        set_owner_directory(&tmp_dir)?;

        let mut store = Self {
            config,
            objects: HashMap::new(),
            uploads: HashMap::new(),
            finalizing: HashMap::new(),
            pending_cleanups: Vec::new(),
            charged_bytes: 0,
            charged_entries: 0,
            lru_clock: 0,
            lru_revision: 0,
            lru_persisted_revision: 0,
            lru_dirty: false,
            next_upload_session: 1,
        };
        store.scan_objects(&objects_dir)?;
        store.load_or_rebuild_lru_metadata()?;
        Ok(store)
    }

    pub fn contains(&mut self, hash: &ObjectHash) -> bool {
        self.read(hash).is_ok()
    }

    pub fn object_path(&self, hash: &ObjectHash) -> PathBuf {
        let hex = encode_hash(hash);
        self.config
            .root
            .join("objects")
            .join(&hex[..2])
            .join(format!("{}.wasm", &hex[2..]))
    }

    pub fn read(&mut self, hash: &ObjectHash) -> Result<Vec<u8>, ObjectStoreError> {
        let read = self.reserve_read(hash)?;
        let result = read.read_verified();
        if let Err(error) = &result {
            self.reconcile_read_error(hash, error);
        }
        result
    }

    pub fn reserve_read(&mut self, hash: &ObjectHash) -> Result<ObjectRead, ObjectStoreError> {
        let Some(entry) = self.objects.get(hash) else {
            return Err(ObjectStoreError::NotFound);
        };
        if entry.size == 0 || entry.size > self.config.module_max {
            return Err(ObjectStoreError::TooLarge);
        }
        entry
            .pins
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |pins| {
                pins.checked_add(1)
            })
            .map_err(|_| ObjectStoreError::Budget)?;
        let pins = Arc::clone(&entry.pins);
        let size = entry.size;
        let path = self.object_path(hash);
        self.touch(hash);
        Ok(ObjectRead {
            hash: *hash,
            path,
            size,
            pins,
        })
    }

    /// Reconcile metadata after detached read/hash work has established that
    /// an object is absent or corrupt. File removal happens through the read
    /// token before this metadata-only transition.
    pub fn forget_removed(&mut self, hash: &ObjectHash) {
        if let Some(entry) = self.objects.remove(hash) {
            self.release(entry.charge, 1);
            self.mark_lru_dirty();
        }
    }

    pub fn mark_executable(&mut self, hash: &ObjectHash) {
        if let Some(entry) = self.objects.get_mut(hash) {
            entry.executable = true;
        }
    }

    pub fn reconcile_read_error(&mut self, hash: &ObjectHash, error: &ObjectStoreError) {
        match error {
            ObjectStoreError::Io(error) if error.kind() == std::io::ErrorKind::NotFound => {
                self.forget_removed(hash);
            }
            ObjectStoreError::HashMismatch if fs::remove_file(self.object_path(hash)).is_ok() => {
                self.forget_removed(hash);
            }
            _ => {}
        }
    }

    pub fn is_usable(&self, hash: &ObjectHash) -> bool {
        self.objects
            .get(hash)
            .is_some_and(|entry| entry.size > 0 && entry.size <= self.config.module_max)
    }

    pub fn pin(&mut self, hash: &ObjectHash) -> Result<(), ObjectStoreError> {
        let entry = self.objects.get(hash).ok_or(ObjectStoreError::NotFound)?;
        entry
            .pins
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |pins| {
                pins.checked_add(1)
            })
            .map_err(|_| ObjectStoreError::Budget)?;
        Ok(())
    }

    pub fn unpin(&mut self, hash: &ObjectHash) {
        if let Some(entry) = self.objects.get(hash) {
            let _ = entry
                .pins
                .fetch_update(Ordering::AcqRel, Ordering::Acquire, |pins| {
                    Some(pins.saturating_sub(1))
                });
        }
    }

    pub fn discard_invalid(&mut self, hash: &ObjectHash) -> Result<(), ObjectStoreError> {
        self.remove_corrupt(hash)
    }

    /// Durability barrier required before committing a persistent definition.
    /// The caller must hold a temporary pin across this call and its redb
    /// transaction so eviction cannot open a reference-to-missing-object gap.
    pub fn sync_object(&mut self, hash: &ObjectHash) -> Result<(), ObjectStoreError> {
        self.reserve_read(hash)?.sync()
    }

    pub fn begin_upload(
        &mut self,
        endpoint: u64,
        hash: ObjectHash,
        total_size: u64,
        now: Instant,
    ) -> Result<BeginUpload, ObjectStoreError> {
        if let Some(size) = self.objects.get(&hash).map(|entry| entry.size) {
            match self.read(&hash) {
                Ok(_) => return Ok(BeginUpload::AlreadyHave { size }),
                Err(ObjectStoreError::NotFound | ObjectStoreError::HashMismatch) => {}
                Err(error) => return Err(error),
            }
        }
        loop {
            match self.prepare_begin_upload_after_probe(endpoint, hash, total_size, now)? {
                PreparedBeginUpload::Complete(result) => return Ok(result),
                PreparedBeginUpload::Evict(eviction) => {
                    let result = (*eviction).finish();
                    self.commit_eviction(result)?;
                }
                PreparedBeginUpload::Create(creation) => {
                    let result = (*creation).finish();
                    return match self.commit_upload_creation(result) {
                        UploadCreationCommit::Complete(result) => result,
                        UploadCreationCommit::Stale(stale) => {
                            let cleanup = (*stale).cleanup();
                            self.commit_upload_cleanup(cleanup);
                            Err(ObjectStoreError::Conflict)
                        }
                    };
                }
            }
        }
    }

    /// Begin after the service performed detached hash and Wasmi validation.
    /// This variant never opens or hashes an object while the service state is
    /// locked. A concurrently committed object is known executable and can be
    /// returned idempotently.
    pub fn prepare_begin_upload_after_probe(
        &mut self,
        endpoint: u64,
        hash: ObjectHash,
        total_size: u64,
        now: Instant,
    ) -> Result<PreparedBeginUpload, ObjectStoreError> {
        if total_size == 0 || total_size > self.config.module_max {
            return Err(ObjectStoreError::TooLarge);
        }
        if let Some((size, executable)) = self
            .objects
            .get(&hash)
            .map(|entry| (entry.size, entry.executable))
        {
            if executable {
                self.touch(&hash);
                return Ok(PreparedBeginUpload::Complete(BeginUpload::AlreadyHave {
                    size,
                }));
            }
            return Err(ObjectStoreError::Io(std::io::Error::other(
                "cached extension object could not be verified",
            )));
        }
        if self.uploads.contains_key(&hash)
            || self.finalizing.contains_key(&hash)
            || self
                .pending_cleanups
                .iter()
                .any(|cleanup| cleanup.hash == hash)
        {
            return Err(ObjectStoreError::Conflict);
        }
        let endpoint_uploads = self
            .uploads
            .values()
            .filter(|upload| upload.endpoint == endpoint)
            .count()
            + self
                .finalizing
                .values()
                .filter(|upload| upload.endpoint == endpoint)
                .count();
        if self.uploads.len().saturating_add(self.finalizing.len()) >= self.config.upload_max
            || endpoint_uploads >= self.config.upload_max_per_endpoint
        {
            return Err(ObjectStoreError::Budget);
        }
        let charge = round_charge(total_size, self.config.allocation_quantum)?;
        if self.charged_bytes.saturating_add(charge) > self.config.cache_max
            || self.charged_entries.saturating_add(1) > self.config.entry_max
        {
            let victim = self.oldest_unpinned().ok_or(ObjectStoreError::Budget)?;
            let entry = self
                .objects
                .remove(&victim)
                .expect("LRU victim remained registered");
            self.mark_lru_dirty();
            return Ok(PreparedBeginUpload::Evict(Box::new(ObjectEviction {
                hash: victim,
                path: self.object_path(&victim),
                entry,
            })));
        }
        let session = self.allocate_upload_session()?;
        self.charged_bytes += charge;
        self.charged_entries += 1;
        self.finalizing
            .insert(hash, FinalizingUpload { endpoint, session });
        Ok(PreparedBeginUpload::Create(Box::new(UploadCreation {
            session,
            endpoint,
            hash,
            total_size,
            charge,
            root: self.config.root.clone(),
            now,
        })))
    }

    pub fn commit_eviction(
        &mut self,
        result: ObjectEvictionResult,
    ) -> Result<(), ObjectStoreError> {
        if result.removed {
            self.release(result.eviction.entry.charge, 1);
            self.mark_lru_dirty();
            result.result
        } else {
            self.objects
                .insert(result.eviction.hash, result.eviction.entry);
            result.result
        }
    }

    pub fn commit_upload_creation(
        &mut self,
        mut created: UploadCreationResult,
    ) -> UploadCreationCommit {
        let current = self
            .finalizing
            .get(&created.hash)
            .is_some_and(|upload| upload.session == created.session);
        if !current {
            return UploadCreationCommit::Stale(Box::new(created));
        }
        self.finalizing.remove(&created.hash);
        match created.result {
            Ok(()) => {
                let upload = created
                    .upload
                    .take()
                    .expect("successful upload creation owns its file");
                self.uploads.insert(created.hash, upload);
                UploadCreationCommit::Complete(Ok(BeginUpload::Started))
            }
            Err(error) => {
                self.commit_upload_cleanup(UploadCleanupResult {
                    session: created.session,
                    endpoint: created.endpoint,
                    hash: created.hash,
                    charge: created.charge,
                    path: created.cleanup_path,
                    removed: created.release_reservation,
                });
                UploadCreationCommit::Complete(Err(error))
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub fn put_chunk(
        &mut self,
        endpoint: u64,
        hash: ObjectHash,
        offset: u64,
        total_size: u64,
        data: &[u8],
        final_chunk: bool,
        now: Instant,
        validate: impl FnOnce(&[u8]) -> Result<(), String>,
    ) -> Result<PutChunk, ObjectStoreError> {
        match self.prepare_put_chunk(endpoint, hash, offset, total_size, data, final_chunk, now)? {
            PreparedPut::Complete(result) => Ok(result),
            PreparedPut::Chunk(upload) => {
                let result = (*upload).finish(data);
                match self.commit_chunk_upload(result) {
                    ChunkUploadCommit::Complete(result) => result,
                    ChunkUploadCommit::Stale(stale) => {
                        let cleanup = (*stale).cleanup();
                        self.commit_upload_cleanup(cleanup);
                        Err(ObjectStoreError::Conflict)
                    }
                }
            }
            PreparedPut::Final(upload) => {
                let result = (*upload).finish(data, validate);
                self.commit_final_upload(result)
            }
            PreparedPut::Abort(cleanup, error) => {
                let result = (*cleanup).finish();
                self.commit_upload_cleanup(result);
                Err(error)
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub fn prepare_put_chunk(
        &mut self,
        endpoint: u64,
        hash: ObjectHash,
        offset: u64,
        total_size: u64,
        data: &[u8],
        final_chunk: bool,
        now: Instant,
    ) -> Result<PreparedPut, ObjectStoreError> {
        let Some(upload) = self.uploads.get_mut(&hash) else {
            if let Some(size) = self.objects.get(&hash).map(|entry| entry.size) {
                self.touch(&hash);
                return Ok(PreparedPut::Complete(PutChunk::AlreadyHave { size }));
            }
            if self.finalizing.contains_key(&hash) {
                return Err(ObjectStoreError::Conflict);
            }
            if self
                .pending_cleanups
                .iter()
                .any(|cleanup| cleanup.hash == hash)
            {
                return Err(ObjectStoreError::Conflict);
            }
            return Err(ObjectStoreError::NotFound);
        };
        if upload.endpoint != endpoint {
            return Err(ObjectStoreError::Conflict);
        }
        let end = offset
            .checked_add(data.len() as u64)
            .ok_or(ObjectStoreError::InvalidUpload("upload offset overflow"))?;
        if offset != upload.received
            || total_size != upload.total_size
            || end > total_size
            || final_chunk != (end == total_size)
        {
            let cleanup = self
                .take_upload_cleanup(&hash)
                .expect("invalid active upload remained registered");
            return Ok(PreparedPut::Abort(
                Box::new(cleanup),
                ObjectStoreError::InvalidUpload(
                    "extension upload chunks must be contiguous and end exactly",
                ),
            ));
        }

        let upload = self
            .uploads
            .remove(&hash)
            .expect("active upload remained registered");
        let Upload {
            session,
            endpoint,
            total_size,
            charge,
            path,
            file,
            hasher,
            ..
        } = upload;
        self.finalizing
            .insert(hash, FinalizingUpload { endpoint, session });
        if !final_chunk {
            return Ok(PreparedPut::Chunk(Box::new(ChunkUpload {
                session,
                hash,
                endpoint,
                total_size,
                received: end,
                charge,
                path,
                file,
                hasher,
                last_activity: now,
            })));
        }
        Ok(PreparedPut::Final(Box::new(FinalUpload {
            session,
            endpoint,
            hash,
            total_size,
            charge,
            final_path: self.object_path(&hash),
            path,
            file,
            hasher,
        })))
    }

    pub fn commit_chunk_upload(&mut self, mut completed: ChunkUploadResult) -> ChunkUploadCommit {
        let current = self
            .finalizing
            .get(&completed.hash)
            .is_some_and(|upload| upload.session == completed.session);
        if !current {
            return ChunkUploadCommit::Stale(Box::new(completed));
        }
        self.finalizing.remove(&completed.hash);
        match completed.result {
            Ok(result) => {
                self.uploads.insert(
                    completed.hash,
                    completed
                        .upload
                        .take()
                        .expect("successful chunk write owns its upload"),
                );
                ChunkUploadCommit::Complete(Ok(result))
            }
            Err(error) => {
                self.commit_upload_cleanup(UploadCleanupResult {
                    session: completed.session,
                    endpoint: completed.endpoint,
                    hash: completed.hash,
                    charge: completed.charge,
                    path: completed.cleanup_path,
                    removed: completed.release_reservation,
                });
                ChunkUploadCommit::Complete(Err(error))
            }
        }
    }

    pub fn commit_final_upload(
        &mut self,
        finalized: FinalUploadResult,
    ) -> Result<PutChunk, ObjectStoreError> {
        let FinalUploadResult {
            session,
            endpoint,
            hash,
            total_size,
            charge,
            release_reservation,
            cleanup_path,
            result,
        } = finalized;
        if !self
            .finalizing
            .get(&hash)
            .is_some_and(|upload| upload.session == session)
        {
            return Err(ObjectStoreError::Conflict);
        }
        if let Err(error) = result {
            self.finalizing.remove(&hash);
            self.commit_upload_cleanup(UploadCleanupResult {
                session,
                endpoint,
                hash,
                charge,
                path: cleanup_path,
                removed: release_reservation,
            });
            return Err(error);
        }
        self.finalizing.remove(&hash);
        self.lru_clock = self.lru_clock.saturating_add(1);
        self.objects.insert(
            hash,
            ObjectEntry {
                size: total_size,
                charge,
                last_used: self.lru_clock,
                pins: Arc::new(AtomicUsize::new(0)),
                executable: true,
            },
        );
        self.mark_lru_dirty();
        Ok(PutChunk::Committed { size: total_size })
    }

    pub fn take_endpoint_uploads(
        &mut self,
        endpoint: u64,
    ) -> (Vec<ObjectHash>, Vec<UploadCleanup>) {
        let hashes: Vec<_> = self
            .uploads
            .iter()
            .filter_map(|(hash, upload)| (upload.endpoint == endpoint).then_some(*hash))
            .collect();
        let cleanups = hashes
            .iter()
            .filter_map(|hash| self.take_upload_cleanup(hash))
            .collect();
        (hashes, cleanups)
    }

    pub fn take_expired_uploads(&mut self, now: Instant) -> (Vec<ObjectHash>, Vec<UploadCleanup>) {
        let timeout = self.config.upload_timeout;
        let hashes: Vec<_> = self
            .uploads
            .iter()
            .filter_map(|(hash, upload)| {
                now.saturating_duration_since(upload.last_activity)
                    .ge(&timeout)
                    .then_some(*hash)
            })
            .collect();
        let cleanups = hashes
            .iter()
            .filter_map(|hash| self.take_upload_cleanup(hash))
            .collect();
        (hashes, cleanups)
    }

    pub fn commit_upload_cleanup(&mut self, cleanup: UploadCleanupResult) {
        if cleanup.removed {
            self.release(cleanup.charge, 1);
        } else {
            self.pending_cleanups.push(PendingCleanup {
                session: cleanup.session,
                endpoint: cleanup.endpoint,
                hash: cleanup.hash,
                charge: cleanup.charge,
                path: cleanup.path,
            });
        }
    }

    pub fn cleanup_retries(&self) -> Vec<CleanupRetry> {
        self.pending_cleanups
            .iter()
            .cloned()
            .map(|pending| CleanupRetry { pending })
            .collect()
    }

    pub fn commit_cleanup_retry(&mut self, result: CleanupRetryResult) -> Option<ObjectHash> {
        if !result.removed {
            return None;
        }
        let index = self.pending_cleanups.iter().position(|pending| {
            pending.session == result.pending.session
                && pending.endpoint == result.pending.endpoint
                && pending.hash == result.pending.hash
                && pending.charge == result.pending.charge
                && pending.path == result.pending.path
        })?;
        let cleanup = self.pending_cleanups.remove(index);
        self.release(cleanup.charge, 1);
        Some(cleanup.hash)
    }

    pub fn finish_startup_gc(&mut self) -> Result<(), ObjectStoreError> {
        // Opening the cache is intentionally non-destructive. The caller
        // invokes this only after the durable catalog has loaded and every
        // referenced object has been pinned, so an unreadable catalog can
        // never cause startup cleanup to delete potentially recoverable data.
        remove_orphan_temporaries(&self.config.root.join("tmp"))?;
        self.persist_lru_metadata()?;
        while self.charged_bytes > self.config.cache_max
            || self.charged_entries > self.config.entry_max
        {
            let Some(hash) = self.oldest_unpinned() else {
                // Pinned objects survive a limit reduction. New reservations
                // remain blocked until usage falls below both ceilings.
                break;
            };
            self.evict_object(hash)?;
        }
        Ok(())
    }

    pub fn usage(&self) -> (u64, usize, usize) {
        (
            self.charged_bytes,
            self.charged_entries,
            self.uploads.len().saturating_add(self.finalizing.len()),
        )
    }

    fn scan_objects(&mut self, objects_dir: &Path) -> Result<(), ObjectStoreError> {
        for shard in fs::read_dir(objects_dir)? {
            let shard = shard?;
            if !shard.file_type()?.is_dir() {
                return Err(ObjectStoreError::InvalidConfig(
                    "extension object cache contains an unexpected entry",
                ));
            }
            let shard_name = shard.file_name();
            let Some(shard_name) = shard_name.to_str() else {
                return Err(ObjectStoreError::InvalidConfig(
                    "extension object cache contains a non-UTF-8 shard name",
                ));
            };
            if shard_name.len() != 2 {
                return Err(ObjectStoreError::InvalidConfig(
                    "extension object cache contains an invalid shard name",
                ));
            }
            for file in fs::read_dir(shard.path())? {
                let file = file?;
                if !file.file_type()?.is_file() {
                    return Err(ObjectStoreError::InvalidConfig(
                        "extension object cache contains an unexpected object entry",
                    ));
                }
                set_owner_file(&file.path())?;
                let file_name = file.file_name();
                let Some(file_name) = file_name.to_str() else {
                    return Err(ObjectStoreError::InvalidConfig(
                        "extension object cache contains a non-UTF-8 object name",
                    ));
                };
                let Some(tail) = file_name.strip_suffix(".wasm") else {
                    return Err(ObjectStoreError::InvalidConfig(
                        "extension object cache contains an invalid object name",
                    ));
                };
                let Some(expected) = decode_hash(&format!("{shard_name}{tail}")) else {
                    return Err(ObjectStoreError::InvalidConfig(
                        "extension object cache contains an invalid object digest",
                    ));
                };
                // Startup reconstructs accounting and pins from names and
                // metadata only. Content is hashed on every read before it is
                // validated or executed, so recovery mode never opens stored
                // modules merely to expose the durable catalog.
                let size = file.metadata()?.len();
                let charge = round_charge(size, self.config.allocation_quantum)?;
                self.charged_bytes = self
                    .charged_bytes
                    .checked_add(charge)
                    .ok_or(ObjectStoreError::Budget)?;
                self.charged_entries = self
                    .charged_entries
                    .checked_add(1)
                    .ok_or(ObjectStoreError::Budget)?;
                self.objects.insert(
                    expected,
                    ObjectEntry {
                        size,
                        charge,
                        last_used: 0,
                        pins: Arc::new(AtomicUsize::new(0)),
                        executable: false,
                    },
                );
            }
        }
        Ok(())
    }

    fn touch(&mut self, hash: &ObjectHash) {
        self.lru_clock = self.lru_clock.saturating_add(1);
        if let Some(entry) = self.objects.get_mut(hash) {
            entry.last_used = self.lru_clock;
        }
        self.mark_lru_dirty();
    }

    fn load_or_rebuild_lru_metadata(&mut self) -> Result<(), ObjectStoreError> {
        let temporary = self.config.root.join(LRU_METADATA_TEMP);
        match fs::remove_file(temporary) {
            Ok(()) => {}
            Err(error) if error.kind() == ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
        let path = self.config.root.join(LRU_METADATA_FILE);
        let loaded = match fs::read(&path) {
            Ok(bytes) => decode_lru_metadata(&bytes, &self.objects),
            Err(error) if error.kind() == ErrorKind::NotFound => None,
            Err(error) => return Err(error.into()),
        };
        if let Some((clock, recency)) = loaded {
            self.lru_clock = clock;
            for (hash, last_used) in recency {
                self.objects
                    .get_mut(&hash)
                    .expect("validated LRU record remained present")
                    .last_used = last_used;
            }
            self.lru_revision = 1;
            self.lru_persisted_revision = 1;
            self.lru_dirty = false;
            set_owner_file(&path)?;
            return Ok(());
        }

        // Missing, torn, stale, or corrupt metadata never authorizes a
        // deletion. Rebuild a complete deterministic order and durably
        // publish it while ObjectStore::open is still non-destructive; the
        // caller reconstructs durable pins before invoking startup GC.
        let mut hashes = self.objects.keys().copied().collect::<Vec<_>>();
        hashes.sort_unstable();
        self.lru_clock = 0;
        for hash in hashes {
            self.lru_clock = self.lru_clock.saturating_add(1);
            self.objects
                .get_mut(&hash)
                .expect("scanned object remained present")
                .last_used = self.lru_clock;
        }
        self.mark_lru_dirty();
        self.persist_lru_metadata()
    }

    pub fn lru_snapshot(&self) -> Result<Option<LruSnapshot>, ObjectStoreError> {
        if !self.lru_dirty || self.lru_revision == self.lru_persisted_revision {
            return Ok(None);
        }
        Ok(Some(LruSnapshot {
            root: self.config.root.clone(),
            revision: self.lru_revision,
            bytes: encode_lru_metadata(self.lru_clock, &self.objects)?,
        }))
    }

    pub fn acknowledge_lru_snapshot(&mut self, snapshot: &LruSnapshot) {
        self.lru_persisted_revision = self.lru_persisted_revision.max(snapshot.revision);
        if self.lru_persisted_revision == self.lru_revision {
            self.lru_dirty = false;
        }
    }

    fn mark_lru_dirty(&mut self) {
        self.lru_revision = self.lru_revision.saturating_add(1);
        self.lru_dirty = true;
    }

    fn persist_lru_metadata(&mut self) -> Result<(), ObjectStoreError> {
        let Some(snapshot) = self.lru_snapshot()? else {
            return Ok(());
        };
        snapshot.persist()?;
        self.acknowledge_lru_snapshot(&snapshot);
        Ok(())
    }

    fn allocate_upload_session(&mut self) -> Result<u64, ObjectStoreError> {
        for _ in 0..64 {
            let session = self.next_upload_session;
            self.next_upload_session = self.next_upload_session.wrapping_add(1).max(1);
            if session != 0
                && !self
                    .uploads
                    .values()
                    .any(|upload| upload.session == session)
                && !self
                    .finalizing
                    .values()
                    .any(|upload| upload.session == session)
                && !self
                    .pending_cleanups
                    .iter()
                    .any(|cleanup| cleanup.session == session)
            {
                return Ok(session);
            }
        }
        Err(ObjectStoreError::Budget)
    }

    fn oldest_unpinned(&self) -> Option<ObjectHash> {
        self.objects
            .iter()
            .filter(|(_, entry)| entry.pins.load(Ordering::Acquire) == 0)
            .min_by_key(|(hash, entry)| (entry.last_used, **hash))
            .map(|(hash, _)| *hash)
    }

    fn evict_object(&mut self, hash: ObjectHash) -> Result<(), ObjectStoreError> {
        let entry = self.objects.remove(&hash).expect("LRU entry remained live");
        match fs::remove_file(self.object_path(&hash)) {
            Ok(()) => {
                self.release(entry.charge, 1);
                self.mark_lru_dirty();
                self.persist_lru_metadata()
            }
            Err(error) => {
                self.objects.insert(hash, entry);
                Err(error.into())
            }
        }
    }

    fn abort_upload(&mut self, hash: &ObjectHash) {
        if let Some(cleanup) = self.take_upload_cleanup(hash) {
            let result = cleanup.finish();
            self.commit_upload_cleanup(result);
        }
    }

    fn take_upload_cleanup(&mut self, hash: &ObjectHash) -> Option<UploadCleanup> {
        let upload = self.uploads.remove(hash)?;
        Some(UploadCleanup {
            session: upload.session,
            endpoint: upload.endpoint,
            hash: *hash,
            path: upload.path,
            charge: upload.charge,
            file: Some(upload.file),
        })
    }

    fn release(&mut self, bytes: u64, entries: usize) {
        self.charged_bytes = self.charged_bytes.saturating_sub(bytes);
        self.charged_entries = self.charged_entries.saturating_sub(entries);
    }

    fn remove_corrupt(&mut self, hash: &ObjectHash) -> Result<(), ObjectStoreError> {
        if let Some(entry) = self.objects.remove(hash) {
            if let Err(error) = fs::remove_file(self.object_path(hash)) {
                self.objects.insert(*hash, entry);
                return Err(error.into());
            }
            self.release(entry.charge, 1);
            self.mark_lru_dirty();
            self.persist_lru_metadata()?;
        }
        Ok(())
    }
}

impl Drop for ObjectStore {
    fn drop(&mut self) {
        let _ = self.persist_lru_metadata();
        let hashes: Vec<_> = self.uploads.keys().copied().collect();
        for hash in hashes {
            self.abort_upload(&hash);
        }
    }
}

fn encode_lru_metadata(
    clock: u64,
    objects: &HashMap<ObjectHash, ObjectEntry>,
) -> Result<Vec<u8>, ObjectStoreError> {
    let count = u64::try_from(objects.len()).map_err(|_| ObjectStoreError::Budget)?;
    let records_bytes = objects
        .len()
        .checked_mul(LRU_METADATA_RECORD)
        .ok_or(ObjectStoreError::Budget)?;
    let payload_bytes = LRU_METADATA_HEADER
        .checked_add(records_bytes)
        .ok_or(ObjectStoreError::Budget)?;
    let mut bytes = Vec::with_capacity(
        payload_bytes
            .checked_add(LRU_METADATA_CHECKSUM)
            .ok_or(ObjectStoreError::Budget)?,
    );
    bytes.extend_from_slice(LRU_METADATA_MAGIC);
    bytes.extend_from_slice(&clock.to_le_bytes());
    bytes.extend_from_slice(&count.to_le_bytes());
    let mut records = objects.iter().collect::<Vec<_>>();
    records.sort_unstable_by_key(|(hash, _)| **hash);
    for (hash, entry) in records {
        bytes.extend_from_slice(hash);
        bytes.extend_from_slice(&entry.last_used.to_le_bytes());
    }
    let checksum = blake3::hash(&bytes);
    bytes.extend_from_slice(checksum.as_bytes());
    Ok(bytes)
}

fn decode_lru_metadata(
    bytes: &[u8],
    objects: &HashMap<ObjectHash, ObjectEntry>,
) -> Option<(u64, Vec<(ObjectHash, u64)>)> {
    if bytes.len() < LRU_METADATA_HEADER + LRU_METADATA_CHECKSUM
        || &bytes[..LRU_METADATA_MAGIC.len()] != LRU_METADATA_MAGIC
    {
        return None;
    }
    let payload_len = bytes.len().checked_sub(LRU_METADATA_CHECKSUM)?;
    if blake3::hash(&bytes[..payload_len]).as_bytes() != &bytes[payload_len..] {
        return None;
    }
    let clock = u64::from_le_bytes(bytes[8..16].try_into().ok()?);
    let count = usize::try_from(u64::from_le_bytes(bytes[16..24].try_into().ok()?)).ok()?;
    let expected = LRU_METADATA_HEADER
        .checked_add(count.checked_mul(LRU_METADATA_RECORD)?)?
        .checked_add(LRU_METADATA_CHECKSUM)?;
    if expected != bytes.len() || count != objects.len() {
        return None;
    }
    let mut hashes = HashSet::with_capacity(count);
    let mut recency = Vec::with_capacity(count);
    for record in bytes[LRU_METADATA_HEADER..payload_len].chunks_exact(LRU_METADATA_RECORD) {
        let hash: ObjectHash = record[..32].try_into().ok()?;
        let last_used = u64::from_le_bytes(record[32..].try_into().ok()?);
        if last_used == 0
            || last_used > clock
            || !objects.contains_key(&hash)
            || !hashes.insert(hash)
        {
            return None;
        }
        recency.push((hash, last_used));
    }
    Some((clock, recency))
}

fn write_lru_metadata_atomic(root: &Path, bytes: &[u8]) -> Result<(), ObjectStoreError> {
    let path = root.join(LRU_METADATA_FILE);
    let temporary = root.join(LRU_METADATA_TEMP);
    match fs::remove_file(&temporary) {
        Ok(()) => {}
        Err(error) if error.kind() == ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    let result = (|| -> Result<(), ObjectStoreError> {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open_owner_only()
            .open(&temporary)?;
        file.write_all(bytes)?;
        file.flush()?;
        file.sync_all()?;
        drop(file);
        set_owner_file(&temporary)?;
        #[cfg(unix)]
        fs::rename(&temporary, &path)?;
        #[cfg(not(unix))]
        {
            match fs::remove_file(&path) {
                Ok(()) => {}
                Err(error) if error.kind() == ErrorKind::NotFound => {}
                Err(error) => return Err(error.into()),
            }
            fs::rename(&temporary, &path)?;
        }
        set_owner_file(&path)?;
        File::open(&path)?.sync_all()?;
        #[cfg(unix)]
        File::open(root)?.sync_all()?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

fn create_upload_file(
    root: &Path,
    endpoint: u64,
    hash: &ObjectHash,
) -> Result<(PathBuf, File), ObjectStoreError> {
    for _ in 0..16 {
        let mut random = [0; 8];
        getrandom::fill(&mut random).map_err(|error| {
            std::io::Error::other(format!("extension upload entropy failed: {error}"))
        })?;
        let path = root.join("tmp").join(format!(
            "upload-{endpoint:016x}-{}-{:016x}.part",
            &encode_hash(hash)[..12],
            u64::from_le_bytes(random)
        ));
        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .open_owner_only()
            .open(&path)
        {
            Ok(file) => {
                if let Err(error) = set_owner_file(&path) {
                    drop(file);
                    let _ = remove_file_if_present(&path);
                    return Err(error);
                }
                return Ok((path, file));
            }
            Err(error) if error.kind() == ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error.into()),
        }
    }
    Err(ObjectStoreError::Io(std::io::Error::new(
        ErrorKind::AlreadyExists,
        "could not allocate a unique extension upload temporary",
    )))
}

fn remove_file_if_present(path: &Path) -> Result<(), ObjectStoreError> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}

fn validate_config(config: &ObjectStoreConfig) -> Result<(), ObjectStoreError> {
    if config.module_max == 0 || config.module_max > DEFAULT_MODULE_MAX {
        return Err(ObjectStoreError::InvalidConfig(
            "BLIT_EXT_MODULE_MAX must be in 1..=64 MiB",
        ));
    }
    if config.cache_max == 0
        || config.entry_max == 0
        || config.upload_max == 0
        || config.upload_max_per_endpoint == 0
        || config.allocation_quantum < MIN_QUANTUM
        || !config.allocation_quantum.is_power_of_two()
    {
        return Err(ObjectStoreError::InvalidConfig(
            "extension object cache limits must be positive and allocation quantum a >=4 KiB power of two",
        ));
    }
    Ok(())
}

fn round_charge(size: u64, quantum: u64) -> Result<u64, ObjectStoreError> {
    size.max(1)
        .checked_add(quantum - 1)
        .map(|value| value / quantum * quantum)
        .ok_or(ObjectStoreError::Budget)
}

fn filesystem_allocation_quantum_near(path: &Path) -> u64 {
    let existing = path
        .ancestors()
        .find(|candidate| candidate.exists())
        .unwrap_or_else(|| Path::new("."));
    filesystem_allocation_quantum(existing)
}

#[cfg(unix)]
#[allow(clippy::unnecessary_cast)]
fn filesystem_allocation_quantum(path: &Path) -> u64 {
    use std::ffi::CString;
    use std::mem::MaybeUninit;
    use std::os::unix::ffi::OsStrExt;

    let Ok(path) = CString::new(path.as_os_str().as_bytes()) else {
        return MIN_QUANTUM;
    };
    let mut stat = MaybeUninit::<libc::statvfs>::uninit();
    // SAFETY: `path` is NUL-terminated and `stat` points to writable storage
    // for the complete `statvfs` result.
    if unsafe { libc::statvfs(path.as_ptr(), stat.as_mut_ptr()) } != 0 {
        return MIN_QUANTUM;
    }
    // SAFETY: a zero return initialized the result.
    let stat = unsafe { stat.assume_init() };
    let reported = if stat.f_frsize == 0 {
        stat.f_bsize as u64
    } else {
        stat.f_frsize as u64
    };
    reported
        .max(MIN_QUANTUM)
        .checked_next_power_of_two()
        .unwrap_or(MIN_QUANTUM)
}

#[cfg(not(unix))]
fn filesystem_allocation_quantum(_path: &Path) -> u64 {
    MIN_QUANTUM
}

fn remove_orphan_temporaries(tmp: &Path) -> Result<(), ObjectStoreError> {
    for entry in fs::read_dir(tmp)? {
        let entry = entry?;
        if !entry.file_type()?.is_file() {
            return Err(ObjectStoreError::InvalidConfig(
                "extension upload directory contains an unexpected entry",
            ));
        }
        fs::remove_file(entry.path())?;
    }
    Ok(())
}

trait OwnerOnlyOpenOptions {
    fn open_owner_only(&mut self) -> &mut Self;
}

#[cfg(unix)]
impl OwnerOnlyOpenOptions for OpenOptions {
    fn open_owner_only(&mut self) -> &mut Self {
        use std::os::unix::fs::OpenOptionsExt;
        self.mode(0o600)
    }
}

#[cfg(not(unix))]
impl OwnerOnlyOpenOptions for OpenOptions {
    fn open_owner_only(&mut self) -> &mut Self {
        self
    }
}

#[cfg(unix)]
fn set_owner_directory(path: &Path) -> Result<(), ObjectStoreError> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    Ok(())
}

#[cfg(not(unix))]
fn set_owner_directory(_path: &Path) -> Result<(), ObjectStoreError> {
    Ok(())
}

#[cfg(unix)]
fn set_owner_file(path: &Path) -> Result<(), ObjectStoreError> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    Ok(())
}

#[cfg(not(unix))]
fn set_owner_file(_path: &Path) -> Result<(), ObjectStoreError> {
    Ok(())
}

fn object_root() -> Option<PathBuf> {
    if let Some(path) = std::env::var_os("BLIT_WASM_CACHE") {
        return Some(PathBuf::from(path));
    }
    #[cfg(target_os = "macos")]
    let base = std::env::var_os("HOME").map(|home| PathBuf::from(home).join("Library/Caches"));
    #[cfg(all(unix, not(target_os = "macos")))]
    let base = std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".cache")));
    #[cfg(windows)]
    let base = std::env::var_os("LOCALAPPDATA").map(PathBuf::from);
    base.map(|base| base.join("blit/wasm"))
}

fn encode_hash(hash: &ObjectHash) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut result = String::with_capacity(64);
    for byte in hash {
        result.push(HEX[(byte >> 4) as usize] as char);
        result.push(HEX[(byte & 0x0f) as usize] as char);
    }
    result
}

fn decode_hash(value: &str) -> Option<ObjectHash> {
    if value.len() != 64 {
        return None;
    }
    let mut hash = [0; 32];
    for (index, pair) in value.as_bytes().chunks_exact(2).enumerate() {
        let high = hex_nibble(pair[0])?;
        let low = hex_nibble(pair[1])?;
        hash[index] = high << 4 | low;
    }
    Some(hash)
}

fn hex_nibble(value: u8) -> Option<u8> {
    match value {
        b'0'..=b'9' => Some(value - b'0'),
        b'a'..=b'f' => Some(value - b'a' + 10),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_root(label: &str) -> PathBuf {
        let mut random = [0; 8];
        getrandom::fill(&mut random).unwrap();
        std::env::temp_dir().join(format!(
            "blit-extension-store-{label}-{:016x}",
            u64::from_le_bytes(random)
        ))
    }

    fn config(root: PathBuf) -> ObjectStoreConfig {
        ObjectStoreConfig {
            root,
            module_max: 1024 * 1024,
            cache_max: 3 * MIN_QUANTUM,
            entry_max: 3,
            upload_max: 2,
            upload_max_per_endpoint: 1,
            upload_timeout: Duration::from_secs(1),
            allocation_quantum: MIN_QUANTUM,
        }
    }

    fn put(store: &mut ObjectStore, endpoint: u64, bytes: &[u8]) -> ObjectHash {
        let hash = *blake3::hash(bytes).as_bytes();
        let now = Instant::now();
        assert_eq!(
            store
                .begin_upload(endpoint, hash, bytes.len() as u64, now)
                .unwrap(),
            BeginUpload::Started
        );
        assert_eq!(
            store
                .put_chunk(
                    endpoint,
                    hash,
                    0,
                    bytes.len() as u64,
                    bytes,
                    true,
                    now,
                    |_| Ok(())
                )
                .unwrap(),
            PutChunk::Committed {
                size: bytes.len() as u64
            }
        );
        hash
    }

    #[test]
    fn upload_commits_atomically_and_cache_hits_do_not_reupload() {
        let root = temp_root("commit");
        let mut store = ObjectStore::open(config(root.clone())).unwrap();
        let bytes = b"\0asm extension";
        let hash = put(&mut store, 7, bytes);
        assert_eq!(store.read(&hash).unwrap(), bytes);
        assert_eq!(
            store
                .begin_upload(9, hash, bytes.len() as u64, Instant::now())
                .unwrap(),
            BeginUpload::AlreadyHave {
                size: bytes.len() as u64
            }
        );
        drop(store);
        let mut reopened = ObjectStore::open(config(root.clone())).unwrap();
        assert_eq!(reopened.read(&hash).unwrap(), bytes);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn bad_digest_aborts_and_releases_the_complete_reservation() {
        let root = temp_root("digest");
        let mut store = ObjectStore::open(config(root.clone())).unwrap();
        let claimed = [9; 32];
        let now = Instant::now();
        store.begin_upload(1, claimed, 3, now).unwrap();
        assert!(matches!(
            store.put_chunk(1, claimed, 0, 3, b"bad", true, now, |_| Ok(())),
            Err(ObjectStoreError::HashMismatch)
        ));
        assert_eq!(store.usage(), (0, 0, 0));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn pinned_objects_survive_lru_pressure() {
        let root = temp_root("lru");
        let mut cfg = config(root.clone());
        cfg.cache_max = 2 * MIN_QUANTUM;
        cfg.entry_max = 2;
        let mut store = ObjectStore::open(cfg).unwrap();
        let first = put(&mut store, 1, b"first");
        let second = put(&mut store, 1, b"second");
        store.pin(&first).unwrap();
        let third = put(&mut store, 1, b"third");
        assert!(store.contains(&first));
        assert!(!store.contains(&second));
        assert!(store.contains(&third));
        store.unpin(&first);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn startup_defers_orphan_deletion_until_catalog_pins_are_reconstructed() {
        let root = temp_root("orphan");
        fs::create_dir_all(root.join("tmp")).unwrap();
        fs::write(root.join("tmp/orphan.part"), b"partial").unwrap();
        let mut store = ObjectStore::open(config(root.clone())).unwrap();
        assert_eq!(store.usage(), (0, 0, 0));
        assert_eq!(fs::read_dir(root.join("tmp")).unwrap().count(), 1);
        store.finish_startup_gc().unwrap();
        assert_eq!(fs::read_dir(root.join("tmp")).unwrap().count(), 0);
        drop(store);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn lowered_module_limit_never_deletes_a_later_pinned_object() {
        let root = temp_root("lowered-limit");
        let mut store = ObjectStore::open(config(root.clone())).unwrap();
        let bytes = b"previously-valid-object";
        let hash = put(&mut store, 1, bytes);
        let path = store.object_path(&hash);
        drop(store);

        let mut lowered = config(root.clone());
        lowered.module_max = 4;
        let mut reopened = ObjectStore::open(lowered).unwrap();
        reopened.pin(&hash).unwrap();
        reopened.finish_startup_gc().unwrap();
        assert!(path.is_file());
        assert!(matches!(
            reopened.read(&hash),
            Err(ObjectStoreError::TooLarge)
        ));
        assert!(matches!(
            reopened.begin_upload(2, hash, bytes.len() as u64, Instant::now()),
            Err(ObjectStoreError::TooLarge)
        ));
        drop(reopened);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn persisted_recency_controls_eviction_after_restart() {
        let root = temp_root("restart-lru");
        let mut store = ObjectStore::open(config(root.clone())).unwrap();
        let first = put(&mut store, 1, b"first");
        let second = put(&mut store, 1, b"second");
        let third = put(&mut store, 1, b"third");
        assert_eq!(store.read(&first).unwrap(), b"first");
        let first_path = store.object_path(&first);
        let second_path = store.object_path(&second);
        let third_path = store.object_path(&third);
        drop(store);

        let mut lowered = config(root.clone());
        lowered.cache_max = 2 * MIN_QUANTUM;
        lowered.entry_max = 2;
        let mut reopened = ObjectStore::open(lowered).unwrap();
        reopened.finish_startup_gc().unwrap();
        assert!(first_path.is_file(), "the last read object stays hot");
        assert!(
            !second_path.exists(),
            "the durable oldest object is evicted"
        );
        assert!(third_path.is_file());
        drop(reopened);
        fs::remove_dir_all(root).unwrap();
    }

    fn rebuilt_metadata_preserves_pins(root: &Path, remove_metadata: bool) {
        let mut store = ObjectStore::open(config(root.to_owned())).unwrap();
        let first = put(&mut store, 1, b"first");
        let pinned = put(&mut store, 1, b"pinned");
        let third = put(&mut store, 1, b"third");
        let paths = [
            store.object_path(&first),
            store.object_path(&pinned),
            store.object_path(&third),
        ];
        drop(store);
        let metadata = root.join(LRU_METADATA_FILE);
        if remove_metadata {
            fs::remove_file(&metadata).unwrap();
        } else {
            fs::write(&metadata, b"torn metadata").unwrap();
        }

        let mut lowered = config(root.to_owned());
        lowered.cache_max = MIN_QUANTUM;
        lowered.entry_max = 1;
        let mut reopened = ObjectStore::open(lowered).unwrap();
        assert!(
            paths.iter().all(|path| path.is_file()),
            "open/rebuild is non-destructive until durable pins are restored"
        );
        reopened.pin(&pinned).unwrap();
        reopened.finish_startup_gc().unwrap();
        assert!(reopened.object_path(&pinned).is_file());
        assert_eq!(reopened.usage().0, MIN_QUANTUM);
        let bytes = fs::read(metadata).unwrap();
        assert!(decode_lru_metadata(&bytes, &reopened.objects).is_some());
        assert!(!root.join(LRU_METADATA_TEMP).exists());
    }

    #[test]
    fn corrupt_recency_is_rebuilt_before_pinned_startup_gc() {
        let root = temp_root("corrupt-lru");
        rebuilt_metadata_preserves_pins(&root, false);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn missing_recency_is_rebuilt_before_pinned_startup_gc() {
        let root = temp_root("missing-lru");
        rebuilt_metadata_preserves_pins(&root, true);
        fs::remove_dir_all(root).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn recency_metadata_is_owner_only() {
        use std::os::unix::fs::PermissionsExt;

        let root = temp_root("lru-permissions");
        let mut store = ObjectStore::open(config(root.clone())).unwrap();
        put(&mut store, 1, b"object");
        let mode = fs::metadata(root.join(LRU_METADATA_FILE))
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600);
        drop(store);
        fs::remove_dir_all(root).unwrap();
    }
}
