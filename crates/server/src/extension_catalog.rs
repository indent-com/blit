//! Durable desired-state catalog for persistent extensions.

use blit_remote::extension::{
    EXT_FLAG_DESIRED_RUNNING, EXT_FLAG_DETACH, EXT_FLAG_ENABLED, EXT_FLAG_PERSIST, EXT_FLAGS,
    EXT_MAX_ARG, EXT_MAX_ARGS, EXT_MAX_ARGUMENT_BYTES, EXT_MAX_DETAIL, EXT_MAX_NAME,
    EXT_RESTART_ALWAYS,
};
use redb::ReadableTable;
use std::collections::HashMap;
use std::path::PathBuf;

const DEFINITIONS: redb::TableDefinition<u64, &[u8]> =
    redb::TableDefinition::new("extension_definitions_v1");
const DEFAULT_MAX_PERSISTENT: usize = 128;
const RECORD_VERSION_V1: u8 = 1;
const RECORD_VERSION_V2: u8 = 2;
const RECORD_VERSION: u8 = 3;

#[derive(Clone, Copy, Debug)]
pub enum BlockedState<'a> {
    Set(&'a str),
    Clear,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PersistentDefinition {
    pub extension_id: u64,
    pub definition_revision: u64,
    pub flags: u8,
    pub restart: u8,
    pub attempt: u64,
    pub last_running_attempt: u64,
    pub failure_count: u32,
    pub next_start_unix_ms: u64,
    pub blocked: bool,
    pub blocked_detail: String,
    pub hash: [u8; 32],
    pub name: String,
    /// Exact encoded argument bytes (count, lengths, and UTF-8 payloads).
    /// The argument values themselves remain in redb until explicitly loaded.
    pub argument_bytes: usize,
}

#[derive(Debug)]
pub enum CatalogError {
    Unavailable,
    Invalid(&'static str),
    Conflict,
    NotFound,
    Budget,
    Storage(String),
}

impl std::fmt::Display for CatalogError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unavailable => f.write_str("persistent extension storage is unavailable"),
            Self::Invalid(detail) => f.write_str(detail),
            Self::Conflict => f.write_str("persistent extension definition changed"),
            Self::NotFound => f.write_str("persistent extension was not found"),
            Self::Budget => f.write_str("persistent extension definition budget exhausted"),
            Self::Storage(detail) => write!(f, "persistent extension storage failed: {detail}"),
        }
    }
}

impl std::error::Error for CatalogError {}

/// In-memory index backed by immediate redb transactions. Mutations update the
/// index only after the durable transaction commits.
pub struct ExtensionCatalog {
    db: Option<redb::Database>,
    records: HashMap<u64, PersistentDefinition>,
    names: HashMap<String, u64>,
    max_persistent: usize,
}

impl ExtensionCatalog {
    pub fn open(path: Option<PathBuf>, max_persistent: usize) -> Result<Self, CatalogError> {
        let Some(path) = path else {
            return Ok(Self {
                db: None,
                records: HashMap::new(),
                names: HashMap::new(),
                max_persistent,
            });
        };
        if let Some(parent) = path.parent() {
            // Narrow permissions belong to directories we bring into being.
            // A path the operator chose may sit in one we do not own -- point
            // BLIT_EXTENSION_PATH at /tmp/x.redb and chmod'ing the parent
            // fails with EPERM, which used to disable the whole extension
            // subsystem over a directory nobody asked us to secure.
            let existed = parent.is_dir();
            std::fs::create_dir_all(parent)
                .map_err(|error| CatalogError::Storage(error.to_string()))?;
            if !existed {
                set_owner_directory(parent)?;
            }
        }
        let db = redb::Database::create(&path)
            .map_err(|error| CatalogError::Storage(error.to_string()))?;
        set_owner_file(&path)?;
        let mut records = HashMap::new();
        let mut names = HashMap::new();
        let read = db
            .begin_read()
            .map_err(|error| CatalogError::Storage(error.to_string()))?;
        match read.open_table(DEFINITIONS) {
            Ok(table) => {
                let iter = table
                    .iter()
                    .map_err(|error| CatalogError::Storage(error.to_string()))?;
                for item in iter {
                    let (id, bytes) =
                        item.map_err(|error| CatalogError::Storage(error.to_string()))?;
                    let definition = decode_definition(bytes.value())?.0;
                    if definition.extension_id != id.value()
                        || records.insert(id.value(), definition.clone()).is_some()
                        || names
                            .insert(definition.name.clone(), definition.extension_id)
                            .is_some()
                    {
                        return Err(CatalogError::Storage(
                            "duplicate or inconsistent extension definition".to_owned(),
                        ));
                    }
                }
            }
            Err(redb::TableError::TableDoesNotExist(_)) => {}
            Err(error) => return Err(CatalogError::Storage(error.to_string())),
        }
        Ok(Self {
            db: Some(db),
            records,
            names,
            max_persistent,
        })
    }

    pub fn from_env(name: &crate::ServerName) -> Result<Self, CatalogError> {
        Self::open(
            catalog_path(name),
            crate::deployment_usize("BLIT_EXT_MAX_PERSISTENT", DEFAULT_MAX_PERSISTENT),
        )
    }

    pub fn list(&self) -> Vec<PersistentDefinition> {
        let mut records: Vec<_> = self.records.values().cloned().collect();
        records.sort_by(|left, right| left.name.as_bytes().cmp(right.name.as_bytes()));
        records
    }

    pub fn get(&self, extension_id: u64) -> Option<&PersistentDefinition> {
        self.records.get(&extension_id)
    }

    pub fn by_name(&self, name: &str) -> Option<&PersistentDefinition> {
        self.names
            .get(name)
            .and_then(|extension_id| self.records.get(extension_id))
    }

    pub fn create(
        &mut self,
        hash: [u8; 32],
        name: String,
        args: Vec<String>,
        restart: u8,
    ) -> Result<PersistentDefinition, CatalogError> {
        let extension_id = self.allocate_id()?;
        self.create_with_id(extension_id, hash, name, args, restart)
    }

    /// Commit a definition whose ID was reserved before a cache-miss upload.
    /// The extension service collision-checks the process-wide transient and
    /// pending registries before exposing this ID to the client; the catalog
    /// repeats its durable collision check in the commit path.
    pub fn create_with_id(
        &mut self,
        extension_id: u64,
        hash: [u8; 32],
        name: String,
        args: Vec<String>,
        restart: u8,
    ) -> Result<PersistentDefinition, CatalogError> {
        validate_definition_fields(&name, &args, restart)?;
        if extension_id == 0
            || self.names.contains_key(&name)
            || self.records.contains_key(&extension_id)
        {
            return Err(CatalogError::Conflict);
        }
        if self.records.len() >= self.max_persistent {
            return Err(CatalogError::Budget);
        }
        let definition = PersistentDefinition {
            extension_id,
            definition_revision: 1,
            flags: EXT_FLAG_DETACH | EXT_FLAG_PERSIST | EXT_FLAG_ENABLED | EXT_FLAG_DESIRED_RUNNING,
            restart,
            attempt: 0,
            last_running_attempt: 0,
            failure_count: 0,
            next_start_unix_ms: 0,
            blocked: false,
            blocked_detail: String::new(),
            hash,
            name,
            argument_bytes: encoded_argument_bytes(&args)?,
        };
        self.commit_with_arguments(&definition, &args)?;
        self.names
            .insert(definition.name.clone(), definition.extension_id);
        self.records
            .insert(definition.extension_id, definition.clone());
        Ok(definition)
    }

    pub fn update(
        &mut self,
        expected_id: u64,
        expected_revision: u64,
        name: &str,
        hash: [u8; 32],
        args: Vec<String>,
        restart: u8,
    ) -> Result<PersistentDefinition, CatalogError> {
        validate_definition_fields(name, &args, restart)?;
        let current = self.by_name(name).ok_or(CatalogError::NotFound)?;
        if current.extension_id != expected_id || current.definition_revision != expected_revision {
            return Err(CatalogError::Conflict);
        }
        let mut updated = current.clone();
        updated.definition_revision = updated
            .definition_revision
            .checked_add(1)
            .ok_or(CatalogError::Budget)?;
        updated.hash = hash;
        updated.argument_bytes = encoded_argument_bytes(&args)?;
        updated.restart = restart;
        updated.failure_count = 0;
        updated.next_start_unix_ms = 0;
        updated.blocked = false;
        updated.blocked_detail.clear();
        self.commit_with_arguments(&updated, &args)?;
        self.records.insert(expected_id, updated.clone());
        Ok(updated)
    }

    #[allow(clippy::too_many_arguments)]
    pub fn set_lifecycle(
        &mut self,
        extension_id: u64,
        enabled: Option<bool>,
        desired_running: Option<bool>,
        attempt: Option<u64>,
        last_running_attempt: Option<u64>,
        failure_count: Option<u32>,
        next_start_unix_ms: Option<u64>,
        blocked: Option<BlockedState<'_>>,
    ) -> Result<PersistentDefinition, CatalogError> {
        let mut updated = self
            .records
            .get(&extension_id)
            .cloned()
            .ok_or(CatalogError::NotFound)?;
        set_flag(&mut updated.flags, EXT_FLAG_ENABLED, enabled);
        set_flag(
            &mut updated.flags,
            EXT_FLAG_DESIRED_RUNNING,
            desired_running,
        );
        if let Some(attempt) = attempt {
            if attempt < updated.attempt {
                return Err(CatalogError::Invalid("attempt counter cannot decrease"));
            }
            updated.attempt = attempt;
        }
        if let Some(last_running) = last_running_attempt {
            if last_running < updated.last_running_attempt || last_running > updated.attempt {
                return Err(CatalogError::Invalid(
                    "last-running attempt must be monotonic and not exceed attempt",
                ));
            }
            updated.last_running_attempt = last_running;
        }
        if let Some(failure_count) = failure_count {
            updated.failure_count = failure_count;
        }
        if let Some(next_start_unix_ms) = next_start_unix_ms {
            updated.next_start_unix_ms = next_start_unix_ms;
        }
        match blocked {
            Some(BlockedState::Set(detail)) => {
                if detail.len() > EXT_MAX_DETAIL {
                    return Err(CatalogError::Invalid(
                        "persistent blocked diagnostic exceeds protocol limits",
                    ));
                }
                updated.blocked = true;
                updated.blocked_detail = detail.to_owned();
            }
            Some(BlockedState::Clear) => {
                updated.blocked = false;
                updated.blocked_detail.clear();
            }
            None => {}
        }
        self.commit_metadata(&updated)?;
        self.records.insert(extension_id, updated.clone());
        Ok(updated)
    }

    pub fn remove(&mut self, extension_id: u64) -> Result<PersistentDefinition, CatalogError> {
        let definition = self
            .records
            .get(&extension_id)
            .cloned()
            .ok_or(CatalogError::NotFound)?;
        self.commit_remove(extension_id)?;
        self.records.remove(&extension_id);
        self.names.remove(&definition.name);
        Ok(definition)
    }

    fn allocate_id(&self) -> Result<u64, CatalogError> {
        for _ in 0..64 {
            let mut bytes = [0; 8];
            getrandom::fill(&mut bytes)
                .map_err(|error| CatalogError::Storage(error.to_string()))?;
            let id = u64::from_le_bytes(bytes);
            if id != 0 && !self.records.contains_key(&id) {
                return Ok(id);
            }
        }
        Err(CatalogError::Budget)
    }

    /// Load one definition's arguments on demand. Startup/listing never call
    /// this path, so catalog cardinality does not multiply resident argument
    /// memory.
    pub fn load_arguments(
        &self,
        extension_id: u64,
        expected_revision: u64,
    ) -> Result<Vec<String>, CatalogError> {
        let db = self.db.as_ref().ok_or(CatalogError::Unavailable)?;
        let read = db
            .begin_read()
            .map_err(|error| CatalogError::Storage(error.to_string()))?;
        let table = read
            .open_table(DEFINITIONS)
            .map_err(|error| CatalogError::Storage(error.to_string()))?;
        let record = table
            .get(extension_id)
            .map_err(|error| CatalogError::Storage(error.to_string()))?
            .ok_or(CatalogError::NotFound)?;
        let bytes = record.value();
        let (definition, arguments_offset) = decode_definition(bytes)?;
        if definition.extension_id != extension_id {
            return Err(CatalogError::Storage(
                "inconsistent extension definition identity".to_owned(),
            ));
        }
        if definition.definition_revision != expected_revision {
            return Err(CatalogError::Conflict);
        }
        decode_arguments(&bytes[arguments_offset..], true).map(|(_, arguments)| arguments)
    }

    fn commit_with_arguments(
        &self,
        definition: &PersistentDefinition,
        arguments: &[String],
    ) -> Result<(), CatalogError> {
        let encoded = encode_definition(definition, arguments)?;
        self.commit_put(definition.extension_id, encoded)
    }

    fn commit_metadata(&self, definition: &PersistentDefinition) -> Result<(), CatalogError> {
        let db = self.db.as_ref().ok_or(CatalogError::Unavailable)?;
        let mut write = db
            .begin_write()
            .map_err(|error| CatalogError::Storage(error.to_string()))?;
        write.set_durability(redb::Durability::Immediate);
        {
            let mut table = write
                .open_table(DEFINITIONS)
                .map_err(|error| CatalogError::Storage(error.to_string()))?;
            let raw_arguments = {
                let current = table
                    .get(definition.extension_id)
                    .map_err(|error| CatalogError::Storage(error.to_string()))?
                    .ok_or(CatalogError::NotFound)?;
                let bytes = current.value();
                let (stored, arguments_offset) = decode_definition(bytes)?;
                if stored.extension_id != definition.extension_id
                    || stored.definition_revision != definition.definition_revision
                    || stored.hash != definition.hash
                    || stored.name != definition.name
                    || stored.restart != definition.restart
                    || stored.argument_bytes != definition.argument_bytes
                {
                    return Err(CatalogError::Storage(
                        "inconsistent extension definition metadata".to_owned(),
                    ));
                }
                bytes[arguments_offset..].to_vec()
            };
            let encoded = encode_definition_raw(definition, &raw_arguments)?;
            table
                .insert(definition.extension_id, encoded.as_slice())
                .map_err(|error| CatalogError::Storage(error.to_string()))?;
        }
        write
            .commit()
            .map_err(|error| CatalogError::Storage(error.to_string()))
    }

    fn commit_put(&self, extension_id: u64, encoded: Vec<u8>) -> Result<(), CatalogError> {
        let db = self.db.as_ref().ok_or(CatalogError::Unavailable)?;
        let mut write = db
            .begin_write()
            .map_err(|error| CatalogError::Storage(error.to_string()))?;
        write.set_durability(redb::Durability::Immediate);
        {
            let mut table = write
                .open_table(DEFINITIONS)
                .map_err(|error| CatalogError::Storage(error.to_string()))?;
            table
                .insert(extension_id, encoded.as_slice())
                .map_err(|error| CatalogError::Storage(error.to_string()))?;
        }
        write
            .commit()
            .map_err(|error| CatalogError::Storage(error.to_string()))
    }

    fn commit_remove(&self, extension_id: u64) -> Result<(), CatalogError> {
        let db = self.db.as_ref().ok_or(CatalogError::Unavailable)?;
        let mut write = db
            .begin_write()
            .map_err(|error| CatalogError::Storage(error.to_string()))?;
        write.set_durability(redb::Durability::Immediate);
        {
            let mut table = write
                .open_table(DEFINITIONS)
                .map_err(|error| CatalogError::Storage(error.to_string()))?;
            table
                .remove(extension_id)
                .map_err(|error| CatalogError::Storage(error.to_string()))?;
        }
        write
            .commit()
            .map_err(|error| CatalogError::Storage(error.to_string()))
    }
}

fn set_flag(flags: &mut u8, bit: u8, value: Option<bool>) {
    match value {
        Some(true) => *flags |= bit,
        Some(false) => *flags &= !bit,
        None => {}
    }
}

fn validate_definition_fields(
    name: &str,
    args: &[String],
    restart: u8,
) -> Result<(), CatalogError> {
    if name.is_empty()
        || name.len() > EXT_MAX_NAME
        || name.chars().any(char::is_control)
        || restart > EXT_RESTART_ALWAYS
        || args.len() > EXT_MAX_ARGS
    {
        return Err(CatalogError::Invalid(
            "persistent extension name, restart policy, or argument count is invalid",
        ));
    }
    let mut total = 0usize;
    for arg in args {
        total = total.checked_add(arg.len()).ok_or(CatalogError::Budget)?;
        if arg.len() > EXT_MAX_ARG || total > EXT_MAX_ARGUMENT_BYTES {
            return Err(CatalogError::Invalid(
                "persistent extension arguments exceed protocol limits",
            ));
        }
    }
    Ok(())
}

fn encoded_argument_bytes(arguments: &[String]) -> Result<usize, CatalogError> {
    arguments.iter().try_fold(2usize, |total, argument| {
        total
            .checked_add(4 + argument.len())
            .ok_or(CatalogError::Budget)
    })
}

fn validate_definition_identity(definition: &PersistentDefinition) -> Result<(), CatalogError> {
    if definition.extension_id == 0
        || definition.definition_revision == 0
        || definition.flags & !EXT_FLAGS != 0
        || definition.flags & (EXT_FLAG_DETACH | EXT_FLAG_PERSIST)
            != EXT_FLAG_DETACH | EXT_FLAG_PERSIST
        || definition.last_running_attempt > definition.attempt
        || definition.blocked_detail.len() > EXT_MAX_DETAIL
        || !definition.blocked && !definition.blocked_detail.is_empty()
    {
        return Err(CatalogError::Invalid(
            "persistent extension identity is invalid",
        ));
    }
    Ok(())
}

fn encode_definition(
    definition: &PersistentDefinition,
    arguments: &[String],
) -> Result<Vec<u8>, CatalogError> {
    validate_definition_fields(&definition.name, arguments, definition.restart)?;
    let argument_bytes = encoded_argument_bytes(arguments)?;
    if definition.argument_bytes != argument_bytes {
        return Err(CatalogError::Invalid(
            "persistent extension argument metadata is inconsistent",
        ));
    }
    let mut raw_arguments = Vec::with_capacity(argument_bytes);
    raw_arguments.extend_from_slice(&(arguments.len() as u16).to_le_bytes());
    for argument in arguments {
        raw_arguments.extend_from_slice(&(argument.len() as u32).to_le_bytes());
        raw_arguments.extend_from_slice(argument.as_bytes());
    }
    encode_definition_raw(definition, &raw_arguments)
}

fn encode_definition_raw(
    definition: &PersistentDefinition,
    raw_arguments: &[u8],
) -> Result<Vec<u8>, CatalogError> {
    validate_definition_identity(definition)?;
    validate_definition_fields(&definition.name, &[], definition.restart)?;
    let (argument_bytes, _) = decode_arguments(raw_arguments, false)?;
    if definition.argument_bytes != argument_bytes {
        return Err(CatalogError::Invalid(
            "persistent extension argument metadata is inconsistent",
        ));
    }
    let mut bytes = Vec::with_capacity(
        94 + definition.blocked_detail.len() + definition.name.len() + raw_arguments.len(),
    );
    bytes.push(RECORD_VERSION);
    bytes.extend_from_slice(&definition.extension_id.to_le_bytes());
    bytes.extend_from_slice(&definition.definition_revision.to_le_bytes());
    bytes.push(definition.flags);
    bytes.push(definition.restart);
    bytes.extend_from_slice(&definition.attempt.to_le_bytes());
    bytes.extend_from_slice(&definition.last_running_attempt.to_le_bytes());
    bytes.extend_from_slice(&definition.failure_count.to_le_bytes());
    bytes.extend_from_slice(&definition.next_start_unix_ms.to_le_bytes());
    bytes.push(u8::from(definition.blocked));
    bytes.extend_from_slice(&(definition.blocked_detail.len() as u16).to_le_bytes());
    bytes.extend_from_slice(definition.blocked_detail.as_bytes());
    bytes.extend_from_slice(&definition.hash);
    bytes.extend_from_slice(&(definition.name.len() as u16).to_le_bytes());
    bytes.extend_from_slice(definition.name.as_bytes());
    bytes.extend_from_slice(raw_arguments);
    Ok(bytes)
}

fn decode_definition(bytes: &[u8]) -> Result<(PersistentDefinition, usize), CatalogError> {
    let mut decoder = Decoder::new(bytes);
    let version = decoder.u8()?;
    if !matches!(
        version,
        RECORD_VERSION_V1 | RECORD_VERSION_V2 | RECORD_VERSION
    ) {
        return Err(CatalogError::Storage(
            "unsupported extension definition record version".to_owned(),
        ));
    }
    let extension_id = decoder.u64()?;
    let definition_revision = decoder.u64()?;
    let flags = decoder.u8()?;
    let restart = decoder.u8()?;
    let attempt = decoder.u64()?;
    let last_running_attempt = decoder.u64()?;
    let (failure_count, next_start_unix_ms) = if version >= RECORD_VERSION_V2 {
        (decoder.u32()?, decoder.u64()?)
    } else {
        (0, 0)
    };
    let (blocked, blocked_detail) = if version >= RECORD_VERSION {
        let blocked = match decoder.u8()? {
            0 => false,
            1 => true,
            _ => {
                return Err(CatalogError::Storage(
                    "extension blocked-state marker is invalid".to_owned(),
                ));
            }
        };
        let len = decoder.u16()? as usize;
        let detail = std::str::from_utf8(decoder.take(len)?)
            .map_err(|_| CatalogError::Storage("extension detail is not UTF-8".to_owned()))?
            .to_owned();
        (blocked, detail)
    } else {
        (false, String::new())
    };
    let hash = decoder.take(32)?.try_into().expect("fixed hash length");
    let name_len = decoder.u16()? as usize;
    let name = std::str::from_utf8(decoder.take(name_len)?)
        .map_err(|_| CatalogError::Storage("extension name is not UTF-8".to_owned()))?
        .to_owned();
    let arguments_offset = decoder.offset;
    let (argument_bytes, _) = decode_arguments(decoder.rest(), false)?;
    let definition = PersistentDefinition {
        extension_id,
        definition_revision,
        flags,
        restart,
        attempt,
        last_running_attempt,
        failure_count,
        next_start_unix_ms,
        blocked,
        blocked_detail,
        hash,
        name,
        argument_bytes,
    };
    validate_definition_identity(&definition).map_err(|error| {
        CatalogError::Storage(format!("invalid persistent extension definition: {error}"))
    })?;
    validate_definition_fields(&definition.name, &[], definition.restart).map_err(|error| {
        CatalogError::Storage(format!("invalid persistent extension definition: {error}"))
    })?;
    Ok((definition, arguments_offset))
}

fn decode_arguments(bytes: &[u8], collect: bool) -> Result<(usize, Vec<String>), CatalogError> {
    let mut decoder = Decoder::new(bytes);
    let count = decoder.u16()? as usize;
    if count > EXT_MAX_ARGS {
        return Err(CatalogError::Storage(
            "extension argument count exceeds protocol limit".to_owned(),
        ));
    }
    let mut arguments = if collect {
        Vec::with_capacity(count)
    } else {
        Vec::new()
    };
    let mut payload_bytes = 0usize;
    for _ in 0..count {
        let len = decoder.u32()? as usize;
        payload_bytes = payload_bytes.checked_add(len).ok_or(CatalogError::Budget)?;
        if len > EXT_MAX_ARG || payload_bytes > EXT_MAX_ARGUMENT_BYTES {
            return Err(CatalogError::Storage(
                "extension arguments exceed protocol limits".to_owned(),
            ));
        }
        let argument = std::str::from_utf8(decoder.take(len)?)
            .map_err(|_| CatalogError::Storage("extension argument is not UTF-8".to_owned()))?;
        if collect {
            arguments.push(argument.to_owned());
        }
    }
    if !decoder.rest().is_empty() {
        return Err(CatalogError::Storage(
            "extension definition has trailing bytes".to_owned(),
        ));
    }
    Ok((bytes.len(), arguments))
}

struct Decoder<'a> {
    bytes: &'a [u8],
    offset: usize,
}

impl<'a> Decoder<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, offset: 0 }
    }

    fn take(&mut self, len: usize) -> Result<&'a [u8], CatalogError> {
        let end = self
            .offset
            .checked_add(len)
            .ok_or_else(|| CatalogError::Storage("extension record overflow".to_owned()))?;
        let value = self
            .bytes
            .get(self.offset..end)
            .ok_or_else(|| CatalogError::Storage("extension record is truncated".to_owned()))?;
        self.offset = end;
        Ok(value)
    }

    fn u8(&mut self) -> Result<u8, CatalogError> {
        Ok(self.take(1)?[0])
    }

    fn u16(&mut self) -> Result<u16, CatalogError> {
        Ok(u16::from_le_bytes(self.take(2)?.try_into().unwrap()))
    }

    fn u32(&mut self) -> Result<u32, CatalogError> {
        Ok(u32::from_le_bytes(self.take(4)?.try_into().unwrap()))
    }

    fn u64(&mut self) -> Result<u64, CatalogError> {
        Ok(u64::from_le_bytes(self.take(8)?.try_into().unwrap()))
    }

    fn rest(&mut self) -> &'a [u8] {
        let rest = &self.bytes[self.offset..];
        self.offset = self.bytes.len();
        rest
    }
}

pub fn catalog_path(name: &crate::ServerName) -> Option<PathBuf> {
    if let Some(path) = std::env::var_os("BLIT_EXTENSION_PATH") {
        return Some(PathBuf::from(path));
    }
    if let Some(path) = std::env::var_os("BLIT_EXT_DB") {
        return Some(PathBuf::from(path));
    }
    #[cfg(target_os = "macos")]
    let base = std::env::var_os("HOME")
        .map(|home| PathBuf::from(home).join("Library/Application Support"));
    #[cfg(all(unix, not(target_os = "macos")))]
    let base = std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/state")));
    #[cfg(windows)]
    let base = std::env::var_os("APPDATA").map(PathBuf::from);
    base.map(|base| crate::server_name::server_path(&base, name, "extensions.redb"))
}

#[cfg(unix)]
fn set_owner_directory(path: &std::path::Path) -> Result<(), CatalogError> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))
        .map_err(|error| CatalogError::Storage(error.to_string()))
}

#[cfg(not(unix))]
fn set_owner_directory(_path: &std::path::Path) -> Result<(), CatalogError> {
    Ok(())
}

#[cfg(unix)]
fn set_owner_file(path: &std::path::Path) -> Result<(), CatalogError> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
        .map_err(|error| CatalogError::Storage(error.to_string()))
}

#[cfg(not(unix))]
fn set_owner_file(_path: &std::path::Path) -> Result<(), CatalogError> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_db(label: &str) -> PathBuf {
        let mut random = [0; 8];
        getrandom::fill(&mut random).unwrap();
        std::env::temp_dir()
            .join(format!(
                "blit-extension-catalog-{label}-{:016x}",
                u64::from_le_bytes(random)
            ))
            .join("extensions.redb")
    }

    #[test]
    fn create_update_lifecycle_and_reopen_are_durable() {
        let path = temp_db("durable");
        let mut catalog = ExtensionCatalog::open(Some(path.clone()), 2).unwrap();
        let created = catalog
            .create([1; 32], "builder".to_owned(), vec!["one".to_owned()], 2)
            .unwrap();
        let updated = catalog
            .update(
                created.extension_id,
                1,
                "builder",
                [2; 32],
                vec!["two".to_owned()],
                1,
            )
            .unwrap();
        assert_eq!(updated.definition_revision, 2);
        catalog
            .set_lifecycle(
                updated.extension_id,
                Some(false),
                None,
                Some(3),
                Some(2),
                Some(4),
                Some(123_456),
                Some(BlockedState::Set("blocked")),
            )
            .unwrap();
        drop(catalog);

        let reopened = ExtensionCatalog::open(Some(path.clone()), 2).unwrap();
        let record = reopened.by_name("builder").unwrap();
        assert_eq!(record.hash, [2; 32]);
        assert_eq!(record.attempt, 3);
        assert_eq!(record.last_running_attempt, 2);
        assert_eq!(record.failure_count, 4);
        assert_eq!(record.next_start_unix_ms, 123_456);
        assert!(record.blocked);
        assert_eq!(record.blocked_detail, "blocked");
        assert_eq!(record.flags & EXT_FLAG_ENABLED, 0);
        assert!(matches!(
            reopened.load_arguments(record.extension_id, 1),
            Err(CatalogError::Conflict)
        ));
        assert_eq!(
            reopened
                .load_arguments(record.extension_id, record.definition_revision)
                .unwrap(),
            vec!["two"]
        );
        drop(reopened);
        std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    #[test]
    fn conflicts_and_capacity_do_not_mutate_the_catalog() {
        let path = temp_db("conflict");
        let mut catalog = ExtensionCatalog::open(Some(path.clone()), 1).unwrap();
        let first = catalog
            .create([1; 32], "one".to_owned(), Vec::new(), 0)
            .unwrap();
        assert!(matches!(
            catalog.create([2; 32], "two".to_owned(), Vec::new(), 0),
            Err(CatalogError::Budget)
        ));
        assert!(matches!(
            catalog.update(first.extension_id, 99, "one", [3; 32], Vec::new(), 0),
            Err(CatalogError::Conflict)
        ));
        assert_eq!(catalog.by_name("one").unwrap().hash, [1; 32]);
        drop(catalog);
        std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    #[test]
    fn reopening_many_large_argument_records_keeps_only_metadata_resident() {
        let path = temp_db("lazy-arguments");
        let mut catalog = ExtensionCatalog::open(Some(path.clone()), 8).unwrap();
        let argument = "x".repeat(EXT_MAX_ARG);
        let mut ids = Vec::new();
        for index in 0..8 {
            let created = catalog
                .create(
                    [index as u8 + 1; 32],
                    format!("extension-{index}"),
                    vec![argument.clone(); 8],
                    0,
                )
                .unwrap();
            ids.push(created.extension_id);
        }
        drop(catalog);

        let reopened = ExtensionCatalog::open(Some(path.clone()), 8).unwrap();
        let records = reopened.list();
        assert_eq!(records.len(), 8);
        assert!(
            records
                .iter()
                .all(|record| { record.argument_bytes == 2 + 8 * (4 + EXT_MAX_ARG) })
        );
        assert_eq!(
            reopened.load_arguments(ids[3], 1).unwrap(),
            vec![argument; 8]
        );
        drop(reopened);
        std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }
}
