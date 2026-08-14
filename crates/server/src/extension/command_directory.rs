//! Live command advertisements contributed by persistent extensions.
//!
//! The directory deliberately owns no channel or supervisor locks. Registration
//! is split into validation and publication so the caller can capture the
//! channel listener, acquire the directory in its global lock order, and pass a
//! fresh owner/listener view for the final generation recheck. Discovery
//! snapshots are immutable and endpoint-scoped.

use blit_remote::channel::CHANNEL_MAX_NAME;
use blit_remote::extension::{
    CommandRecord, EXT_MAX_COMMAND_RECORDS, EXT_MAX_COMMANDS_PACKET, EXT_MAX_DESCRIPTOR,
    EXT_MAX_NAME, EXT_STATUS_BUDGET, EXT_STATUS_CONFLICT, EXT_STATUS_INVALID, EXT_STATUS_NOT_FOUND,
    EXT_STATUS_OK, EXT_STATUS_PERMISSION,
};
use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

const COMMAND_RECORD_FIXED_BYTES: usize = 72;
const COMMANDS_PACKET_FIXED_BYTES: usize = 23;
const DEFAULT_STORE_BYTES: usize = 64 * 1024 * 1024;
const DEFAULT_SNAPSHOT_COUNT: usize = 256;
const SNAPSHOT_IDLE_TIMEOUT: Duration = Duration::from_secs(30);

/// Server-global command-directory limits sampled at startup.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct CommandDirectoryLimits {
    pub store_bytes: usize,
    pub snapshots: usize,
    pub snapshot_idle_timeout: Duration,
}

impl Default for CommandDirectoryLimits {
    fn default() -> Self {
        Self {
            store_bytes: DEFAULT_STORE_BYTES,
            snapshots: DEFAULT_SNAPSHOT_COUNT,
            snapshot_idle_timeout: SNAPSHOT_IDLE_TIMEOUT,
        }
    }
}

impl CommandDirectoryLimits {
    pub(crate) fn from_env() -> Self {
        let defaults = Self::default();
        Self {
            store_bytes: crate::deployment_usize(
                "BLIT_EXT_COMMAND_STORE_MAX",
                defaults.store_bytes,
            ),
            snapshots: crate::deployment_usize("BLIT_EXT_COMMAND_SNAPSHOT_MAX", defaults.snapshots),
            snapshot_idle_timeout: defaults.snapshot_idle_timeout,
        }
    }
}

/// Immutable identity of the live extension endpoint issuing `REGISTER`.
///
/// Supplying this value is an authority assertion by the supervisor, not a
/// claim decoded from a guest packet.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct CommandOwner {
    pub endpoint_id: u64,
    pub endpoint_generation: u64,
    pub extension_id: u64,
    pub definition_revision: u64,
    pub attempt: u64,
    pub hash: [u8; 32],
    pub name: String,
    pub persistent: bool,
    pub enabled: bool,
    pub running: bool,
}

/// Live channel-listener identity captured from the channel registry.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct CommandListener {
    pub endpoint_id: u64,
    pub endpoint_generation: u64,
    pub listener_id: u32,
    pub listener_generation: u64,
    pub name: String,
    pub token: [u8; 16],
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct OwnerFence {
    endpoint_id: u64,
    endpoint_generation: u64,
    extension_id: u64,
    definition_revision: u64,
    attempt: u64,
    hash: [u8; 32],
    name: String,
}

impl From<&CommandOwner> for OwnerFence {
    fn from(owner: &CommandOwner) -> Self {
        Self {
            endpoint_id: owner.endpoint_id,
            endpoint_generation: owner.endpoint_generation,
            extension_id: owner.extension_id,
            definition_revision: owner.definition_revision,
            attempt: owner.attempt,
            hash: owner.hash,
            name: owner.name.clone(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct ListenerFence {
    endpoint_id: u64,
    endpoint_generation: u64,
    listener_id: u32,
    listener_generation: u64,
    name: String,
    token: [u8; 16],
}

impl From<&CommandListener> for ListenerFence {
    fn from(listener: &CommandListener) -> Self {
        Self {
            endpoint_id: listener.endpoint_id,
            endpoint_generation: listener.endpoint_generation,
            listener_id: listener.listener_id,
            listener_generation: listener.listener_generation,
            name: listener.name.clone(),
            token: listener.token,
        }
    }
}

/// A validated registration awaiting the non-awaiting publication recheck.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct PreparedRegistration {
    owner: OwnerFence,
    operation: RegistrationOperation,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum RegistrationOperation {
    Unregister,
    Publish {
        listener: ListenerFence,
        descriptor: String,
    },
}

impl PreparedRegistration {
    pub(crate) fn extension_id(&self) -> u64 {
        self.owner.extension_id
    }

    pub(crate) fn definition_revision(&self) -> u64 {
        self.owner.definition_revision
    }
}

/// Deterministic family-local registration failures.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RegistrationError {
    Permission,
    NotFound,
    Invalid,
    Conflict,
    Budget,
}

impl RegistrationError {
    pub(crate) fn status(self) -> u8 {
        match self {
            Self::Permission => EXT_STATUS_PERMISSION,
            Self::NotFound => EXT_STATUS_NOT_FOUND,
            Self::Invalid => EXT_STATUS_INVALID,
            Self::Conflict => EXT_STATUS_CONFLICT,
            Self::Budget => EXT_STATUS_BUDGET,
        }
    }

    pub(crate) fn detail(self) -> &'static str {
        match self {
            Self::Permission => "extension command registration is not permitted",
            Self::NotFound => "extension command listener was not found",
            Self::Invalid => "extension command descriptor is invalid",
            Self::Conflict => "extension command owner or listener changed",
            Self::Budget => "extension command directory budget exhausted",
        }
    }
}

/// Result of a successful publish or unregister operation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct RegistrationResult {
    pub extension_id: u64,
    pub definition_revision: u64,
    pub directory_revision: u64,
    pub changed: bool,
}

/// Owned form of one wire `command_record`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct AdvertisedCommand {
    pub extension_id: u64,
    pub definition_revision: u64,
    pub hash: [u8; 32],
    pub name: String,
    pub listener_name: String,
    pub listener_token: [u8; 16],
    pub descriptor: String,
}

impl AdvertisedCommand {
    pub(crate) fn as_wire(&self) -> CommandRecord<'_> {
        CommandRecord {
            extension_id: self.extension_id,
            definition_revision: self.definition_revision,
            hash: self.hash,
            name: &self.name,
            listener_name: &self.listener_name,
            listener_token: self.listener_token,
            descriptor: &self.descriptor,
        }
    }

    pub(crate) fn encoded_len(&self) -> usize {
        COMMAND_RECORD_FIXED_BYTES
            + self.name.len()
            + self.listener_name.len()
            + self.descriptor.len()
    }

    fn encode(&self) -> Vec<u8> {
        let mut bytes = Vec::with_capacity(self.encoded_len());
        bytes.extend_from_slice(&self.extension_id.to_le_bytes());
        bytes.extend_from_slice(&self.definition_revision.to_le_bytes());
        bytes.extend_from_slice(&self.hash);
        bytes.extend_from_slice(&(self.name.len() as u16).to_le_bytes());
        bytes.extend_from_slice(self.name.as_bytes());
        bytes.extend_from_slice(&(self.listener_name.len() as u16).to_le_bytes());
        bytes.extend_from_slice(self.listener_name.as_bytes());
        bytes.extend_from_slice(&self.listener_token);
        bytes.extend_from_slice(&(self.descriptor.len() as u32).to_le_bytes());
        bytes.extend_from_slice(self.descriptor.as_bytes());
        bytes
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum DiscoveryStatus {
    Ok,
    Budget,
    Conflict,
}

impl DiscoveryStatus {
    pub(crate) fn status(self) -> u8 {
        match self {
            Self::Ok => EXT_STATUS_OK,
            Self::Budget => EXT_STATUS_BUDGET,
            Self::Conflict => EXT_STATUS_CONFLICT,
        }
    }
}

/// One immutable discovery-snapshot page.
#[derive(Clone, Debug)]
pub(crate) struct DiscoveryPage {
    pub status: DiscoveryStatus,
    pub directory_revision: u64,
    pub next_cursor: u64,
    pub records: Vec<AdvertisedCommand>,
    // The snapshot admission must outlive the page clone used to encode the
    // response. In particular, one-page and final-page results no longer
    // release their global slot and bytes while their records are still live.
    _reservation: Option<SnapshotReservation>,
}

impl PartialEq for DiscoveryPage {
    fn eq(&self, other: &Self) -> bool {
        self.status == other.status
            && self.directory_revision == other.directory_revision
            && self.next_cursor == other.next_cursor
            && self.records == other.records
    }
}

impl Eq for DiscoveryPage {}

impl DiscoveryPage {
    #[cfg(test)]
    pub(crate) fn encoded_len(&self) -> usize {
        COMMANDS_PACKET_FIXED_BYTES
            + self
                .records
                .iter()
                .map(AdvertisedCommand::encoded_len)
                .sum::<usize>()
    }
}

#[derive(Clone, Debug)]
struct Registration {
    owner: OwnerFence,
    listener: ListenerFence,
    record: AdvertisedCommand,
    encoded: Vec<u8>,
}

#[derive(Debug)]
struct DiscoverySnapshot {
    revision: u64,
    cursor: u64,
    next_index: usize,
    records: Vec<AdvertisedCommand>,
    expires_at: Instant,
    reservation: SnapshotReservation,
}

#[derive(Debug, Default)]
struct SnapshotBudget {
    state: Mutex<SnapshotBudgetState>,
}

#[derive(Debug, Default)]
struct SnapshotBudgetState {
    bytes: usize,
    count: usize,
}

impl SnapshotBudget {
    fn state(&self) -> MutexGuard<'_, SnapshotBudgetState> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn try_reserve(
        self: &Arc<Self>,
        record_bytes: usize,
        snapshot_bytes: usize,
        limits: CommandDirectoryLimits,
    ) -> Option<SnapshotReservation> {
        let mut state = self.state();
        let stored_bytes = record_bytes
            .checked_add(state.bytes)?
            .checked_add(snapshot_bytes)?;
        if state.count >= limits.snapshots || stored_bytes > limits.store_bytes {
            return None;
        }
        state.bytes += snapshot_bytes;
        state.count += 1;
        drop(state);
        Some(SnapshotReservation {
            _inner: Arc::new(SnapshotReservationInner {
                budget: Arc::clone(self),
                bytes: snapshot_bytes,
            }),
        })
    }
}

#[derive(Clone, Debug)]
struct SnapshotReservation {
    _inner: Arc<SnapshotReservationInner>,
}

#[derive(Debug)]
struct SnapshotReservationInner {
    budget: Arc<SnapshotBudget>,
    bytes: usize,
}

impl Drop for SnapshotReservationInner {
    fn drop(&mut self) {
        let mut state = self.budget.state();
        state.bytes = state
            .bytes
            .checked_sub(self.bytes)
            .expect("live command snapshot bytes remained charged");
        state.count = state
            .count
            .checked_sub(1)
            .expect("live command snapshot slot remained charged");
    }
}

/// Live command records and endpoint-local immutable discovery snapshots.
pub(crate) struct CommandDirectory {
    limits: CommandDirectoryLimits,
    revision: u64,
    next_cursor: u64,
    records: BTreeMap<String, Registration>,
    record_bytes: usize,
    snapshots: HashMap<u64, DiscoverySnapshot>,
    snapshot_budget: Arc<SnapshotBudget>,
}

impl Default for CommandDirectory {
    fn default() -> Self {
        Self::new(CommandDirectoryLimits::from_env())
    }
}

impl CommandDirectory {
    pub(crate) fn new(limits: CommandDirectoryLimits) -> Self {
        Self {
            limits,
            revision: 1,
            next_cursor: 1,
            records: BTreeMap::new(),
            record_bytes: 0,
            snapshots: HashMap::new(),
            snapshot_budget: Arc::new(SnapshotBudget::default()),
        }
    }

    #[cfg(test)]
    pub(crate) fn revision(&self) -> u64 {
        self.revision
    }

    #[cfg(test)]
    pub(crate) fn record_count(&self) -> usize {
        self.records.len()
    }

    #[cfg(test)]
    pub(crate) fn active_snapshot_count(&self) -> usize {
        self.snapshot_budget.state().count
    }

    pub(crate) fn stored_bytes(&self) -> usize {
        self.record_bytes
            .saturating_add(self.snapshot_budget.state().bytes)
    }

    /// Validate origin, field combinations, descriptor JSON, and the captured
    /// listener without mutating the previous registration.
    pub(crate) fn prepare_registration(
        &self,
        owner: Option<&CommandOwner>,
        listener_id: u32,
        descriptor: &str,
        listener: Option<&CommandListener>,
    ) -> Result<PreparedRegistration, RegistrationError> {
        let owner = owner
            .filter(|owner| valid_owner(owner))
            .ok_or(RegistrationError::Permission)?;
        let owner_fence = OwnerFence::from(owner);

        if listener_id == 0 && descriptor.is_empty() {
            return Ok(PreparedRegistration {
                owner: owner_fence,
                operation: RegistrationOperation::Unregister,
            });
        }
        if listener_id == 0 || descriptor.is_empty() {
            return Err(RegistrationError::Invalid);
        }

        if listener.is_some_and(|listener| {
            listener.endpoint_id != owner.endpoint_id
                || listener.endpoint_generation != owner.endpoint_generation
        }) {
            return Err(RegistrationError::Permission);
        }
        let listener = listener
            .filter(|listener| listener.listener_id == listener_id)
            .ok_or(RegistrationError::NotFound)?;
        if !valid_listener(listener) {
            return Err(RegistrationError::NotFound);
        }
        validate_descriptor(descriptor).map_err(|_| RegistrationError::Invalid)?;

        Ok(PreparedRegistration {
            owner: owner_fence,
            operation: RegistrationOperation::Publish {
                listener: ListenerFence::from(listener),
                descriptor: descriptor.to_owned(),
            },
        })
    }

    /// Recheck the exact owner/listener generations and atomically publish.
    /// The caller must invoke this while its supervisor/channel registry views
    /// cannot change.
    pub(crate) fn commit_registration(
        &mut self,
        prepared: PreparedRegistration,
        current_owner: Option<&CommandOwner>,
        current_listener: Option<&CommandListener>,
    ) -> Result<RegistrationResult, RegistrationError> {
        let current_owner = current_owner.ok_or(RegistrationError::Conflict)?;
        if !valid_owner(current_owner) || OwnerFence::from(current_owner) != prepared.owner {
            return Err(RegistrationError::Conflict);
        }

        match prepared.operation {
            RegistrationOperation::Unregister => self.unregister(&prepared.owner),
            RegistrationOperation::Publish {
                listener,
                descriptor,
            } => {
                let current_listener = current_listener.ok_or(RegistrationError::Conflict)?;
                if !valid_listener(current_listener)
                    || ListenerFence::from(current_listener) != listener
                {
                    return Err(RegistrationError::Conflict);
                }
                self.publish(prepared.owner, listener, descriptor)
            }
        }
    }

    fn publish(
        &mut self,
        owner: OwnerFence,
        listener: ListenerFence,
        descriptor: String,
    ) -> Result<RegistrationResult, RegistrationError> {
        if self.records.iter().any(|(name, registration)| {
            (name == &owner.name && registration.owner.extension_id != owner.extension_id)
                || (name != &owner.name && registration.owner.extension_id == owner.extension_id)
        }) {
            return Err(RegistrationError::Conflict);
        }

        let record = AdvertisedCommand {
            extension_id: owner.extension_id,
            definition_revision: owner.definition_revision,
            hash: owner.hash,
            name: owner.name.clone(),
            listener_name: listener.name.clone(),
            listener_token: listener.token,
            descriptor,
        };
        let encoded = record.encode();

        if let Some(previous) = self.records.get_mut(&owner.name)
            && previous.encoded == encoded
        {
            previous.owner = owner.clone();
            previous.listener = listener;
            return Ok(self.registration_result(&owner, false));
        }

        // Reserve the complete replacement before releasing the old record.
        // This makes a failed replacement leave the previous advertisement
        // intact and charged.
        let reserved = self
            .stored_bytes()
            .checked_add(encoded.len())
            .ok_or(RegistrationError::Budget)?;
        if reserved > self.limits.store_bytes {
            return Err(RegistrationError::Budget);
        }

        let registration = Registration {
            owner: owner.clone(),
            listener,
            record,
            encoded,
        };
        let replaced = self.records.insert(owner.name.clone(), registration);
        self.record_bytes = self
            .record_bytes
            .checked_add(self.records[&owner.name].encoded.len())
            .expect("admission checked command bytes");
        if let Some(replaced) = replaced {
            self.record_bytes -= replaced.encoded.len();
        }
        self.bump_revision();
        Ok(self.registration_result(&owner, true))
    }

    fn unregister(&mut self, owner: &OwnerFence) -> Result<RegistrationResult, RegistrationError> {
        if self
            .records
            .get(&owner.name)
            .is_some_and(|registration| registration.owner.extension_id != owner.extension_id)
        {
            return Err(RegistrationError::Conflict);
        }
        let changed = self.remove_name(&owner.name);
        Ok(self.registration_result(owner, changed))
    }

    fn registration_result(&self, owner: &OwnerFence, changed: bool) -> RegistrationResult {
        RegistrationResult {
            extension_id: owner.extension_id,
            definition_revision: owner.definition_revision,
            directory_revision: self.revision,
            changed,
        }
    }

    /// Attempt cleanup cannot remove a replacement attempt's advertisement.
    pub(crate) fn invalidate_attempt(
        &mut self,
        extension_id: u64,
        definition_revision: u64,
        attempt: u64,
    ) -> bool {
        self.remove_matching(|registration| {
            registration.owner.extension_id == extension_id
                && registration.owner.definition_revision == definition_revision
                && registration.owner.attempt == attempt
        }) != 0
    }

    /// Disable/update cleanup is fenced by definition revision.
    pub(crate) fn invalidate_definition(
        &mut self,
        extension_id: u64,
        definition_revision: u64,
    ) -> bool {
        self.remove_matching(|registration| {
            registration.owner.extension_id == extension_id
                && registration.owner.definition_revision == definition_revision
        }) != 0
    }

    /// A durably removed definition can no longer have a successor under this
    /// ID, so removal deliberately ignores generation fields.
    pub(crate) fn invalidate_extension(&mut self, extension_id: u64) -> bool {
        self.remove_matching(|registration| registration.owner.extension_id == extension_id) != 0
    }

    /// Remove records for the exact endpoint incarnation being torn down.
    pub(crate) fn invalidate_endpoint(
        &mut self,
        endpoint_id: u64,
        endpoint_generation: u64,
    ) -> usize {
        self.remove_matching(|registration| {
            registration.owner.endpoint_id == endpoint_id
                && registration.owner.endpoint_generation == endpoint_generation
        })
    }

    /// Listener-close cleanup is fenced against reuse of the channel ID.
    pub(crate) fn invalidate_listener(&mut self, listener: &CommandListener) -> bool {
        let fence = ListenerFence::from(listener);
        self.remove_matching(|registration| registration.listener == fence) != 0
    }

    /// Capture or continue the caller's one immutable snapshot.
    pub(crate) fn discover(
        &mut self,
        endpoint_id: u64,
        requested_revision: u64,
        cursor: u64,
        now: Instant,
    ) -> DiscoveryPage {
        self.expire_snapshots(now);
        if requested_revision == 0 && cursor == 0 {
            self.release_snapshot(endpoint_id);
            return self.start_snapshot(endpoint_id, now);
        }
        if requested_revision == 0 || cursor == 0 {
            return self.discovery_error(DiscoveryStatus::Conflict);
        }
        self.continue_snapshot(endpoint_id, requested_revision, cursor, now)
    }

    fn start_snapshot(&mut self, endpoint_id: u64, now: Instant) -> DiscoveryPage {
        let Some(encoded_bytes) = self
            .records
            .values()
            .try_fold(0usize, |total, registration| {
                total.checked_add(registration.record.encoded_len())
            })
        else {
            return self.discovery_error(DiscoveryStatus::Budget);
        };
        // Charge the complete immutable view and its global slot before any
        // record is cloned. The reservation is shared by the endpoint cursor
        // and every page which still owns records from that view.
        let Some(reservation) =
            self.snapshot_budget
                .try_reserve(self.record_bytes, encoded_bytes, self.limits)
        else {
            return self.discovery_error(DiscoveryStatus::Budget);
        };
        let records: Vec<_> = self
            .records
            .values()
            .map(|registration| registration.record.clone())
            .collect();

        let end = page_end(&records, 0);
        let page_records = records[..end].to_vec();
        if end == records.len() {
            return DiscoveryPage {
                status: DiscoveryStatus::Ok,
                directory_revision: self.revision,
                next_cursor: 0,
                records: page_records,
                _reservation: Some(reservation),
            };
        }

        let next_cursor = self.allocate_cursor();
        let snapshot = DiscoverySnapshot {
            revision: self.revision,
            cursor: next_cursor,
            next_index: end,
            records,
            expires_at: now + self.limits.snapshot_idle_timeout,
            reservation: reservation.clone(),
        };
        self.snapshots.insert(endpoint_id, snapshot);
        DiscoveryPage {
            status: DiscoveryStatus::Ok,
            directory_revision: self.revision,
            next_cursor,
            records: page_records,
            _reservation: Some(reservation),
        }
    }

    fn continue_snapshot(
        &mut self,
        endpoint_id: u64,
        requested_revision: u64,
        cursor: u64,
        now: Instant,
    ) -> DiscoveryPage {
        let Some(snapshot) = self.snapshots.get(&endpoint_id) else {
            return self.discovery_error(DiscoveryStatus::Conflict);
        };
        if snapshot.revision != requested_revision || snapshot.cursor != cursor {
            return self.discovery_error(DiscoveryStatus::Conflict);
        }

        let revision = snapshot.revision;
        let start = snapshot.next_index;
        let end = page_end(&snapshot.records, start);
        let records = snapshot.records[start..end].to_vec();
        if end == snapshot.records.len() {
            let snapshot = self
                .snapshots
                .remove(&endpoint_id)
                .expect("validated command snapshot remained live");
            return DiscoveryPage {
                status: DiscoveryStatus::Ok,
                directory_revision: revision,
                next_cursor: 0,
                records,
                _reservation: Some(snapshot.reservation),
            };
        }

        let next_cursor = self.allocate_cursor();
        let snapshot = self
            .snapshots
            .get_mut(&endpoint_id)
            .expect("validated snapshot remained live");
        snapshot.next_index = end;
        snapshot.cursor = next_cursor;
        snapshot.expires_at = now + self.limits.snapshot_idle_timeout;
        DiscoveryPage {
            status: DiscoveryStatus::Ok,
            directory_revision: revision,
            next_cursor,
            records,
            _reservation: Some(snapshot.reservation.clone()),
        }
    }

    pub(crate) fn close_endpoint(&mut self, endpoint_id: u64) {
        self.release_snapshot(endpoint_id);
    }

    pub(crate) fn expire_snapshots(&mut self, now: Instant) -> usize {
        let expired: Vec<_> = self
            .snapshots
            .iter()
            .filter_map(|(&endpoint, snapshot)| (now >= snapshot.expires_at).then_some(endpoint))
            .collect();
        for endpoint in &expired {
            self.release_snapshot(*endpoint);
        }
        expired.len()
    }

    fn discovery_error(&self, status: DiscoveryStatus) -> DiscoveryPage {
        DiscoveryPage {
            status,
            directory_revision: self.revision,
            next_cursor: 0,
            records: Vec::new(),
            _reservation: None,
        }
    }

    fn allocate_cursor(&mut self) -> u64 {
        let cursor = self.next_cursor;
        self.next_cursor = self.next_cursor.checked_add(1).unwrap_or(1);
        cursor
    }

    fn release_snapshot(&mut self, endpoint_id: u64) {
        self.snapshots.remove(&endpoint_id);
    }

    fn remove_matching(&mut self, predicate: impl Fn(&Registration) -> bool) -> usize {
        let names: Vec<_> = self
            .records
            .iter()
            .filter(|(_, registration)| predicate(registration))
            .map(|(name, _)| name.clone())
            .collect();
        for name in &names {
            let removed = self
                .records
                .remove(name)
                .expect("collected command registration remained live");
            self.record_bytes -= removed.encoded.len();
        }
        if !names.is_empty() {
            self.bump_revision();
        }
        names.len()
    }

    fn remove_name(&mut self, name: &str) -> bool {
        let Some(removed) = self.records.remove(name) else {
            return false;
        };
        self.record_bytes -= removed.encoded.len();
        self.bump_revision();
        true
    }

    fn bump_revision(&mut self) {
        // Exhaustion is not reachable in practice. Keeping zero reserved also
        // makes a theoretical wrap fail old continuations by cursor.
        self.revision = self.revision.checked_add(1).unwrap_or(1);
    }
}

fn page_end(records: &[AdvertisedCommand], start: usize) -> usize {
    let mut bytes = COMMANDS_PACKET_FIXED_BYTES;
    let mut end = start;
    while end < records.len() && end - start < EXT_MAX_COMMAND_RECORDS {
        let Some(next) = bytes.checked_add(records[end].encoded_len()) else {
            break;
        };
        if next > EXT_MAX_COMMANDS_PACKET {
            break;
        }
        bytes = next;
        end += 1;
    }
    debug_assert!(end > start || start == records.len());
    end
}

fn valid_owner(owner: &CommandOwner) -> bool {
    owner.endpoint_id != 0
        && owner.endpoint_generation != 0
        && owner.extension_id != 0
        && owner.definition_revision != 0
        && owner.attempt != 0
        && owner.persistent
        && owner.enabled
        && owner.running
        && valid_name(&owner.name, EXT_MAX_NAME)
}

fn valid_listener(listener: &CommandListener) -> bool {
    listener.endpoint_id != 0
        && listener.endpoint_generation != 0
        && listener.listener_id != 0
        && listener.listener_generation != 0
        && valid_name(&listener.name, CHANNEL_MAX_NAME)
}

fn valid_name(name: &str, max: usize) -> bool {
    !name.is_empty() && name.len() <= max && !name.chars().any(char::is_control)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct DescriptorError;

/// Validate JSON syntax and the v1 discovery envelope without interpreting the
/// extension's application-specific command and option content.
pub(crate) fn validate_descriptor(descriptor: &str) -> Result<(), DescriptorError> {
    if descriptor.is_empty() || descriptor.len() > EXT_MAX_DESCRIPTOR {
        return Err(DescriptorError);
    }
    DescriptorParser::new(descriptor).parse()
}

struct DescriptorParser<'a> {
    source: &'a str,
    offset: usize,
}

impl<'a> DescriptorParser<'a> {
    fn new(source: &'a str) -> Self {
        Self { source, offset: 0 }
    }

    fn parse(mut self) -> Result<(), DescriptorError> {
        self.whitespace();
        self.expect(b'{')?;
        self.whitespace();
        let mut protocol = None;
        let mut summary = false;
        let mut commands = false;
        if self.consume(b'}') {
            return Err(DescriptorError);
        }

        loop {
            let key = self.string(true)?.expect("requested decoded JSON string");
            self.whitespace();
            self.expect(b':')?;
            self.whitespace();
            match key.as_str() {
                "protocol" => {
                    if self.peek() != Some(b'"') {
                        return Err(DescriptorError);
                    }
                    protocol = self.string(true)?;
                }
                "summary" => {
                    if self.peek() != Some(b'"') {
                        return Err(DescriptorError);
                    }
                    self.string(false)?;
                    summary = true;
                }
                "commands" => {
                    if self.peek() != Some(b'[') {
                        return Err(DescriptorError);
                    }
                    self.value()?;
                    commands = true;
                }
                _ => self.value()?,
            }
            self.whitespace();
            if self.consume(b'}') {
                break;
            }
            self.expect(b',')?;
            self.whitespace();
            if self.peek() == Some(b'}') {
                return Err(DescriptorError);
            }
        }
        self.whitespace();
        if self.offset != self.source.len()
            || protocol.as_deref() != Some("blit.cli.v1")
            || !summary
            || !commands
        {
            return Err(DescriptorError);
        }
        Ok(())
    }

    /// Consume one arbitrary JSON value with an explicit heap stack so a
    /// hostile 64-KiB descriptor cannot overflow the server thread's stack.
    fn value(&mut self) -> Result<(), DescriptorError> {
        let mut frames = Vec::new();
        self.value_start(&mut frames)?;
        while let Some(frame) = frames.pop() {
            self.whitespace();
            match frame {
                JsonFrame::ArrayFirstOrEnd => {
                    if !self.consume(b']') {
                        frames.push(JsonFrame::ArrayCommaOrEnd);
                        self.value_start(&mut frames)?;
                    }
                }
                JsonFrame::ArrayValue => {
                    frames.push(JsonFrame::ArrayCommaOrEnd);
                    self.value_start(&mut frames)?;
                }
                JsonFrame::ArrayCommaOrEnd => {
                    if self.consume(b',') {
                        frames.push(JsonFrame::ArrayValue);
                    } else {
                        self.expect(b']')?;
                    }
                }
                JsonFrame::ObjectFirstKeyOrEnd => {
                    if !self.consume(b'}') {
                        self.object_member(&mut frames)?;
                    }
                }
                JsonFrame::ObjectKey => self.object_member(&mut frames)?,
                JsonFrame::ObjectCommaOrEnd => {
                    if self.consume(b',') {
                        frames.push(JsonFrame::ObjectKey);
                    } else {
                        self.expect(b'}')?;
                    }
                }
            }
        }
        Ok(())
    }

    fn value_start(&mut self, frames: &mut Vec<JsonFrame>) -> Result<(), DescriptorError> {
        self.whitespace();
        match self.peek().ok_or(DescriptorError)? {
            b'"' => {
                self.string(false)?;
            }
            b'{' => {
                self.offset += 1;
                frames.push(JsonFrame::ObjectFirstKeyOrEnd);
            }
            b'[' => {
                self.offset += 1;
                frames.push(JsonFrame::ArrayFirstOrEnd);
            }
            b't' => self.literal(b"true")?,
            b'f' => self.literal(b"false")?,
            b'n' => self.literal(b"null")?,
            b'-' | b'0'..=b'9' => self.number()?,
            _ => return Err(DescriptorError),
        }
        Ok(())
    }

    fn object_member(&mut self, frames: &mut Vec<JsonFrame>) -> Result<(), DescriptorError> {
        self.whitespace();
        if self.peek() != Some(b'"') {
            return Err(DescriptorError);
        }
        self.string(false)?;
        self.whitespace();
        self.expect(b':')?;
        frames.push(JsonFrame::ObjectCommaOrEnd);
        self.value_start(frames)
    }

    fn string(&mut self, decode: bool) -> Result<Option<String>, DescriptorError> {
        self.expect(b'"')?;
        let mut decoded = decode.then(String::new);
        loop {
            let byte = self.peek().ok_or(DescriptorError)?;
            match byte {
                b'"' => {
                    self.offset += 1;
                    return Ok(decoded);
                }
                b'\\' => {
                    self.offset += 1;
                    let escaped = self.peek().ok_or(DescriptorError)?;
                    self.offset += 1;
                    let value = match escaped {
                        b'"' => '"',
                        b'\\' => '\\',
                        b'/' => '/',
                        b'b' => '\u{0008}',
                        b'f' => '\u{000c}',
                        b'n' => '\n',
                        b'r' => '\r',
                        b't' => '\t',
                        b'u' => self.unicode_escape()?,
                        _ => return Err(DescriptorError),
                    };
                    if let Some(decoded) = &mut decoded {
                        decoded.push(value);
                    }
                }
                0x00..=0x1f => return Err(DescriptorError),
                _ => {
                    let tail = &self.source[self.offset..];
                    let value = tail.chars().next().ok_or(DescriptorError)?;
                    self.offset += value.len_utf8();
                    if let Some(decoded) = &mut decoded {
                        decoded.push(value);
                    }
                }
            }
        }
    }

    fn unicode_escape(&mut self) -> Result<char, DescriptorError> {
        let first = self.hex_quad()?;
        let scalar = if (0xd800..=0xdbff).contains(&first) {
            if self.take_byte() != Some(b'\\') || self.take_byte() != Some(b'u') {
                return Err(DescriptorError);
            }
            let second = self.hex_quad()?;
            if !(0xdc00..=0xdfff).contains(&second) {
                return Err(DescriptorError);
            }
            0x1_0000 + (((first - 0xd800) as u32) << 10) + (second - 0xdc00) as u32
        } else if (0xdc00..=0xdfff).contains(&first) {
            return Err(DescriptorError);
        } else {
            first as u32
        };
        char::from_u32(scalar).ok_or(DescriptorError)
    }

    fn hex_quad(&mut self) -> Result<u16, DescriptorError> {
        let mut value = 0u16;
        for _ in 0..4 {
            let digit = match self.take_byte().ok_or(DescriptorError)? {
                b'0'..=b'9' => self.source.as_bytes()[self.offset - 1] - b'0',
                b'a'..=b'f' => self.source.as_bytes()[self.offset - 1] - b'a' + 10,
                b'A'..=b'F' => self.source.as_bytes()[self.offset - 1] - b'A' + 10,
                _ => return Err(DescriptorError),
            };
            value = (value << 4) | u16::from(digit);
        }
        Ok(value)
    }

    fn number(&mut self) -> Result<(), DescriptorError> {
        self.consume(b'-');
        match self.take_byte().ok_or(DescriptorError)? {
            b'0' => {
                if self.peek().is_some_and(|byte| byte.is_ascii_digit()) {
                    return Err(DescriptorError);
                }
            }
            b'1'..=b'9' => {
                while self.peek().is_some_and(|byte| byte.is_ascii_digit()) {
                    self.offset += 1;
                }
            }
            _ => return Err(DescriptorError),
        }
        if self.consume(b'.') {
            if !self.peek().is_some_and(|byte| byte.is_ascii_digit()) {
                return Err(DescriptorError);
            }
            while self.peek().is_some_and(|byte| byte.is_ascii_digit()) {
                self.offset += 1;
            }
        }
        if self.consume(b'e') || self.consume(b'E') {
            if !self.consume(b'+') {
                self.consume(b'-');
            }
            if !self.peek().is_some_and(|byte| byte.is_ascii_digit()) {
                return Err(DescriptorError);
            }
            while self.peek().is_some_and(|byte| byte.is_ascii_digit()) {
                self.offset += 1;
            }
        }
        Ok(())
    }

    fn literal(&mut self, literal: &[u8]) -> Result<(), DescriptorError> {
        let end = self
            .offset
            .checked_add(literal.len())
            .ok_or(DescriptorError)?;
        if self.source.as_bytes().get(self.offset..end) != Some(literal) {
            return Err(DescriptorError);
        }
        self.offset = end;
        Ok(())
    }

    fn whitespace(&mut self) {
        while self
            .peek()
            .is_some_and(|byte| matches!(byte, b' ' | b'\t' | b'\n' | b'\r'))
        {
            self.offset += 1;
        }
    }

    fn expect(&mut self, expected: u8) -> Result<(), DescriptorError> {
        if self.consume(expected) {
            Ok(())
        } else {
            Err(DescriptorError)
        }
    }

    fn consume(&mut self, expected: u8) -> bool {
        if self.peek() == Some(expected) {
            self.offset += 1;
            true
        } else {
            false
        }
    }

    fn take_byte(&mut self) -> Option<u8> {
        let byte = self.peek()?;
        self.offset += 1;
        Some(byte)
    }

    fn peek(&self) -> Option<u8> {
        self.source.as_bytes().get(self.offset).copied()
    }
}

#[derive(Clone, Copy, Debug)]
enum JsonFrame {
    ArrayFirstOrEnd,
    ArrayValue,
    ArrayCommaOrEnd,
    ObjectFirstKeyOrEnd,
    ObjectKey,
    ObjectCommaOrEnd,
}

#[cfg(test)]
mod tests {
    use super::*;

    const DESCRIPTOR: &str = r#"{"protocol":"blit.cli.v1","summary":"Build","commands":[]}"#;

    fn limits() -> CommandDirectoryLimits {
        CommandDirectoryLimits {
            store_bytes: DEFAULT_STORE_BYTES,
            snapshots: DEFAULT_SNAPSHOT_COUNT,
            snapshot_idle_timeout: Duration::from_secs(30),
        }
    }

    fn owner_named(name: &str, id: u64, attempt: u64) -> CommandOwner {
        CommandOwner {
            endpoint_id: id + 100,
            endpoint_generation: attempt + 10,
            extension_id: id,
            definition_revision: 1,
            attempt,
            hash: [id as u8; 32],
            name: name.to_owned(),
            persistent: true,
            enabled: true,
            running: true,
        }
    }

    fn listener(owner: &CommandOwner, id: u32, generation: u64) -> CommandListener {
        CommandListener {
            endpoint_id: owner.endpoint_id,
            endpoint_generation: owner.endpoint_generation,
            listener_id: id,
            listener_generation: generation,
            name: format!("blit.cli.{}.{}", owner.extension_id, generation),
            token: [generation as u8; 16],
        }
    }

    fn register(
        directory: &mut CommandDirectory,
        owner: &CommandOwner,
        listener: &CommandListener,
        descriptor: &str,
    ) -> Result<RegistrationResult, RegistrationError> {
        let prepared = directory.prepare_registration(
            Some(owner),
            listener.listener_id,
            descriptor,
            Some(listener),
        )?;
        directory.commit_registration(prepared, Some(owner), Some(listener))
    }

    #[test]
    fn descriptor_parser_accepts_growth_and_full_json_grammar() {
        let descriptor = concat!(
            r#" { "pro\u0074ocol" : "blit.cli.v1", "summary":"\ud83d\ude80", "commands" : ["#,
            r#"{"path":["build"],"number":-12.5e+2,"values":[true,false,null]}],"future":{"x":[]}} "#,
        );
        assert_eq!(validate_descriptor(descriptor), Ok(()));

        // Explicit heap parsing accepts nesting far beyond a reasonable call
        // stack without recursion.
        let mut deep = String::from(r#"{"protocol":"blit.cli.v1","summary":"x","commands":"#);
        deep.extend(std::iter::repeat_n('[', 4_000));
        deep.push_str("null");
        deep.extend(std::iter::repeat_n(']', 4_000));
        deep.push('}');
        assert_eq!(validate_descriptor(&deep), Ok(()));
    }

    #[test]
    fn descriptor_parser_rejects_malformed_or_missing_envelopes() {
        for invalid in [
            "",
            "[]",
            r#"{"protocol":"wrong","summary":"x","commands":[]}"#,
            r#"{"protocol":"blit.cli.v1","commands":[]}"#,
            r#"{"protocol":"blit.cli.v1","summary":1,"commands":[]}"#,
            r#"{"protocol":"blit.cli.v1","summary":"x","commands":{}}"#,
            r#"{"protocol":"blit.cli.v1","summary":"x","commands":[],}"#,
            r#"{"protocol":"blit.cli.v1","summary":"\ud800","commands":[]}"#,
            r#"{"protocol":"blit.cli.v1","summary":"x","commands":[01]}"#,
            r#"{"protocol":"blit.cli.v1","summary":"x","commands":[1.] }"#,
        ] {
            assert_eq!(
                validate_descriptor(invalid),
                Err(DescriptorError),
                "{invalid}"
            );
        }
    }

    #[test]
    fn registration_permissions_and_failure_precedence_are_deterministic() {
        let directory = CommandDirectory::new(limits());
        let owner = owner_named("builder", 1, 1);
        let owned_listener = listener(&owner, 2, 1);
        assert_eq!(
            directory.prepare_registration(None, 2, "not json", None),
            Err(RegistrationError::Permission)
        );

        let mut transient = owner.clone();
        transient.persistent = false;
        assert_eq!(
            directory.prepare_registration(Some(&transient), 2, DESCRIPTOR, Some(&owned_listener)),
            Err(RegistrationError::Permission)
        );
        let mut disabled = owner.clone();
        disabled.enabled = false;
        assert_eq!(
            directory.prepare_registration(Some(&disabled), 2, DESCRIPTOR, Some(&owned_listener)),
            Err(RegistrationError::Permission)
        );
        assert_eq!(
            directory.prepare_registration(Some(&owner), 2, "not json", None),
            Err(RegistrationError::NotFound)
        );
        assert_eq!(
            directory.prepare_registration(Some(&owner), 0, DESCRIPTOR, None),
            Err(RegistrationError::Invalid)
        );

        let other = owner_named("other", 2, 1);
        let stolen = listener(&other, 2, 1);
        assert_eq!(
            directory.prepare_registration(Some(&owner), 2, DESCRIPTOR, Some(&stolen)),
            Err(RegistrationError::Permission)
        );
    }

    #[test]
    fn publication_captures_derived_fields_and_identical_bytes_are_a_noop() {
        let mut directory = CommandDirectory::new(limits());
        let owner = owner_named("builder", 1, 7);
        let listener = listener(&owner, 2, 9);
        let first = register(&mut directory, &owner, &listener, DESCRIPTOR).unwrap();
        assert!(first.changed);
        assert_eq!(first.directory_revision, 2);
        let record = &directory.records["builder"].record;
        assert_eq!(record.extension_id, owner.extension_id);
        assert_eq!(record.hash, owner.hash);
        assert_eq!(record.name, "builder");
        assert_eq!(record.listener_name, listener.name);
        assert_eq!(record.listener_token, listener.token);
        assert_eq!(record.descriptor, DESCRIPTOR);
        assert_eq!(record.as_wire().listener_token, listener.token);
        let wire = record.as_wire();
        let packet = blit_remote::extension::msg_extension_commands(
            7,
            EXT_STATUS_OK,
            directory.revision(),
            0,
            std::slice::from_ref(&wire),
        )
        .unwrap();
        assert_eq!(
            packet.len(),
            COMMANDS_PACKET_FIXED_BYTES + record.encoded_len()
        );

        let bytes = directory.stored_bytes();
        let second = register(&mut directory, &owner, &listener, DESCRIPTOR).unwrap();
        assert!(!second.changed);
        assert_eq!(second.directory_revision, 2);
        assert_eq!(directory.stored_bytes(), bytes);
    }

    #[test]
    fn stale_owner_or_listener_loses_the_publication_recheck() {
        let mut directory = CommandDirectory::new(limits());
        let owner = owner_named("builder", 1, 1);
        let listener = listener(&owner, 2, 1);
        register(&mut directory, &owner, &listener, DESCRIPTOR).unwrap();

        let prepared = directory
            .prepare_registration(
                Some(&owner),
                2,
                r#"{"protocol":"blit.cli.v1","summary":"new","commands":[]}"#,
                Some(&listener),
            )
            .unwrap();
        let mut replacement_owner = owner.clone();
        replacement_owner.attempt += 1;
        replacement_owner.endpoint_generation += 1;
        assert_eq!(
            directory.commit_registration(prepared, Some(&replacement_owner), None),
            Err(RegistrationError::Conflict)
        );
        assert_eq!(directory.records["builder"].record.descriptor, DESCRIPTOR);

        let prepared = directory
            .prepare_registration(
                Some(&owner),
                2,
                r#"{"protocol":"blit.cli.v1","summary":"new","commands":[]}"#,
                Some(&listener),
            )
            .unwrap();
        let mut replacement_listener = listener.clone();
        replacement_listener.listener_generation += 1;
        replacement_listener.token = [3; 16];
        assert_eq!(
            directory.commit_registration(prepared, Some(&owner), Some(&replacement_listener)),
            Err(RegistrationError::Conflict)
        );
        assert_eq!(directory.records["builder"].record.descriptor, DESCRIPTOR);
    }

    #[test]
    fn replacement_reserves_new_bytes_before_releasing_the_old_record() {
        let mut directory = CommandDirectory::new(limits());
        let owner = owner_named("builder", 1, 1);
        let listener = listener(&owner, 2, 1);
        register(&mut directory, &owner, &listener, DESCRIPTOR).unwrap();
        let old_bytes = directory.record_bytes;
        let replacement = r#"{"protocol":"blit.cli.v1","summary":"replacement","commands":[]}"#;
        let new_bytes =
            COMMAND_RECORD_FIXED_BYTES + owner.name.len() + listener.name.len() + replacement.len();
        directory.limits.store_bytes = old_bytes + new_bytes - 1;
        assert_eq!(
            register(&mut directory, &owner, &listener, replacement),
            Err(RegistrationError::Budget)
        );
        assert_eq!(directory.records["builder"].record.descriptor, DESCRIPTOR);
        assert_eq!(directory.record_bytes, old_bytes);
        assert_eq!(directory.revision(), 2);
    }

    #[test]
    fn discovery_is_sorted_bounded_and_snapshot_immutable() {
        let mut directory = CommandDirectory::new(limits());
        for index in (0..35).rev() {
            let name = format!("command-{index:02}");
            let owner = owner_named(&name, index + 1, 1);
            let listener = listener(&owner, 2, 1);
            register(&mut directory, &owner, &listener, DESCRIPTOR).unwrap();
        }
        let now = Instant::now();
        let first = directory.discover(9_001, 0, 0, now);
        assert_eq!(first.status, DiscoveryStatus::Ok);
        assert_eq!(first.records.len(), EXT_MAX_COMMAND_RECORDS);
        assert_ne!(first.next_cursor, 0);
        assert!(first.encoded_len() <= EXT_MAX_COMMANDS_PACKET);
        assert!(
            first
                .records
                .windows(2)
                .all(|pair| pair[0].name.as_bytes() < pair[1].name.as_bytes())
        );
        let captured_revision = first.directory_revision;

        assert!(directory.invalidate_extension(35));
        let owner = owner_named("command-00", 1, 1);
        let replacement_listener = listener(&owner, 4, 2);
        register(
            &mut directory,
            &owner,
            &replacement_listener,
            r#"{"protocol":"blit.cli.v1","summary":"changed","commands":[]}"#,
        )
        .unwrap();
        assert!(directory.revision() > captured_revision);

        let second = directory.discover(
            9_001,
            captured_revision,
            first.next_cursor,
            now + Duration::from_secs(1),
        );
        assert_eq!(second.status, DiscoveryStatus::Ok);
        assert_eq!(second.directory_revision, captured_revision);
        assert_eq!(second.next_cursor, 0);
        assert_eq!(second.records.len(), 3);
        assert_eq!(second.records.last().unwrap().name, "command-34");
        // The cursor is gone, but both returned pages still share ownership of
        // the immutable snapshot reservation until response encoding ends.
        assert_eq!(directory.snapshots.len(), 0);
        assert_eq!(directory.active_snapshot_count(), 1);
        drop(first);
        drop(second);
        assert_eq!(directory.active_snapshot_count(), 0);

        let fresh = directory.discover(9_001, 0, 0, now + Duration::from_secs(2));
        assert_eq!(fresh.directory_revision, directory.revision());
        assert!(fresh.records[0].descriptor.contains("changed"));
    }

    #[test]
    fn wrong_replaced_and_expired_snapshot_cursors_conflict() {
        let mut configured = limits();
        configured.snapshot_idle_timeout = Duration::from_secs(2);
        let mut directory = CommandDirectory::new(configured);
        for index in 0..33 {
            let owner = owner_named(&format!("x-{index:02}"), index + 1, 1);
            let listener = listener(&owner, 2, 1);
            register(&mut directory, &owner, &listener, DESCRIPTOR).unwrap();
        }
        let now = Instant::now();
        let first = directory.discover(7, 0, 0, now);
        let conflict = directory.discover(7, first.directory_revision, first.next_cursor + 1, now);
        assert_eq!(conflict.status, DiscoveryStatus::Conflict);
        assert_eq!(conflict.directory_revision, directory.revision());
        assert_eq!(directory.active_snapshot_count(), 1);

        let replacement = directory.discover(7, 0, 0, now);
        assert_ne!(replacement.next_cursor, first.next_cursor);
        assert_eq!(
            directory
                .discover(7, first.directory_revision, first.next_cursor, now)
                .status,
            DiscoveryStatus::Conflict
        );
        assert_eq!(
            directory
                .discover(
                    7,
                    replacement.directory_revision,
                    replacement.next_cursor,
                    now + Duration::from_secs(3),
                )
                .status,
            DiscoveryStatus::Conflict
        );
        assert_eq!(directory.snapshots.len(), 0);
        assert_eq!(directory.active_snapshot_count(), 2);
        drop(first);
        drop(replacement);
        assert_eq!(directory.active_snapshot_count(), 0);
    }

    #[test]
    fn snapshot_slots_and_bytes_are_globally_bounded_and_released() {
        let mut directory = CommandDirectory::new(limits());
        for index in 0..33 {
            let owner = owner_named(&format!("x-{index:02}"), index + 1, 1);
            let listener = listener(&owner, 2, 1);
            register(&mut directory, &owner, &listener, DESCRIPTOR).unwrap();
        }
        directory.limits.snapshots = 1;
        let now = Instant::now();
        let first = directory.discover(1_001, 0, 0, now);
        assert_eq!(first.status, DiscoveryStatus::Ok);
        let charged = directory.stored_bytes();
        assert_eq!(directory.active_snapshot_count(), 1);
        assert_eq!(
            directory.discover(1_002, 0, 0, now).status,
            DiscoveryStatus::Budget
        );
        assert_eq!(directory.stored_bytes(), charged);
        directory.close_endpoint(1_001);
        assert_eq!(directory.snapshots.len(), 0);
        assert_eq!(directory.active_snapshot_count(), 1);
        assert_eq!(directory.stored_bytes(), charged);
        assert_eq!(
            directory.discover(1_002, 0, 0, now).status,
            DiscoveryStatus::Budget
        );
        drop(first);
        assert_eq!(directory.active_snapshot_count(), 0);
        assert_eq!(directory.stored_bytes(), directory.record_bytes);
        assert_eq!(
            directory.discover(1_002, 0, 0, now).status,
            DiscoveryStatus::Ok
        );

        directory.close_endpoint(1_002);
        directory.limits.store_bytes = directory.record_bytes * 2 - 1;
        assert_eq!(
            directory.discover(1_003, 0, 0, now).status,
            DiscoveryStatus::Budget
        );
        assert_eq!(directory.active_snapshot_count(), 0);
    }

    #[test]
    fn first_page_admission_uses_the_exact_shared_byte_boundary() {
        let mut directory = CommandDirectory::new(limits());
        let owner = owner_named("builder", 1, 1);
        let listener = listener(&owner, 2, 1);
        register(&mut directory, &owner, &listener, DESCRIPTOR).unwrap();
        let snapshot_bytes = directory.records["builder"].record.encoded_len();
        let exact_limit = directory.record_bytes + snapshot_bytes;
        directory.limits.store_bytes = exact_limit;

        let page = directory.discover(8_001, 0, 0, Instant::now());
        assert_eq!(page.status, DiscoveryStatus::Ok);
        assert_eq!(page.next_cursor, 0);
        assert_eq!(directory.snapshots.len(), 0);
        assert_eq!(directory.active_snapshot_count(), 1);
        assert_eq!(directory.stored_bytes(), exact_limit);
        // Registration records and the response-owned snapshot continue to
        // compete for the same byte ceiling.
        assert_eq!(
            directory.discover(8_002, 0, 0, Instant::now()).status,
            DiscoveryStatus::Budget
        );

        drop(page);
        assert_eq!(directory.active_snapshot_count(), 0);
        assert_eq!(directory.stored_bytes(), directory.record_bytes);
        directory.limits.store_bytes = exact_limit - 1;
        assert_eq!(
            directory.discover(8_003, 0, 0, Instant::now()).status,
            DiscoveryStatus::Budget
        );
    }

    #[test]
    fn concurrent_one_page_response_holds_its_global_slot() {
        let mut configured = limits();
        configured.snapshots = 1;
        let mut directory = CommandDirectory::new(configured);
        let owner = owner_named("builder", 1, 1);
        let listener = listener(&owner, 2, 1);
        register(&mut directory, &owner, &listener, DESCRIPTOR).unwrap();
        let directory = Arc::new(Mutex::new(directory));

        let page = directory
            .lock()
            .unwrap()
            .discover(9_001, 0, 0, Instant::now());
        assert_eq!(page.next_cursor, 0);
        let page_clone = page.clone();
        let (ready_tx, ready_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let dropper = std::thread::spawn(move || {
            ready_tx.send(()).unwrap();
            release_rx.recv().unwrap();
            drop(page);
        });
        ready_rx.recv().unwrap();

        assert_eq!(
            directory
                .lock()
                .unwrap()
                .discover(9_002, 0, 0, Instant::now())
                .status,
            DiscoveryStatus::Budget
        );
        release_tx.send(()).unwrap();
        dropper.join().unwrap();
        assert_eq!(directory.lock().unwrap().active_snapshot_count(), 1);
        drop(page_clone);
        assert_eq!(directory.lock().unwrap().active_snapshot_count(), 0);
        let page = directory
            .lock()
            .unwrap()
            .discover(9_002, 0, 0, Instant::now());
        assert_eq!(page.status, DiscoveryStatus::Ok);
        drop(page);
        assert_eq!(directory.lock().unwrap().active_snapshot_count(), 0);
    }

    #[test]
    fn final_page_transfers_the_reservation_until_every_page_drops() {
        let mut directory = CommandDirectory::new(limits());
        for index in 0..33 {
            let owner = owner_named(&format!("x-{index:02}"), index + 1, 1);
            let listener = listener(&owner, 2, 1);
            register(&mut directory, &owner, &listener, DESCRIPTOR).unwrap();
        }
        let now = Instant::now();
        let first = directory.discover(10_001, 0, 0, now);
        let charged = directory.stored_bytes();
        let final_page = directory.discover(
            10_001,
            first.directory_revision,
            first.next_cursor,
            now + Duration::from_secs(1),
        );
        assert_eq!(final_page.status, DiscoveryStatus::Ok);
        assert_eq!(final_page.next_cursor, 0);
        assert_eq!(directory.snapshots.len(), 0);
        assert_eq!(directory.active_snapshot_count(), 1);
        assert_eq!(directory.stored_bytes(), charged);

        drop(first);
        assert_eq!(directory.active_snapshot_count(), 1);
        directory.close_endpoint(10_001);
        assert_eq!(directory.active_snapshot_count(), 1);
        drop(final_page);
        assert_eq!(directory.active_snapshot_count(), 0);
        assert_eq!(directory.stored_bytes(), directory.record_bytes);
    }

    #[test]
    fn conflict_and_expiry_do_not_release_a_response_owned_snapshot() {
        let mut configured = limits();
        configured.snapshot_idle_timeout = Duration::from_secs(2);
        let mut directory = CommandDirectory::new(configured);
        for index in 0..33 {
            let owner = owner_named(&format!("x-{index:02}"), index + 1, 1);
            let listener = listener(&owner, 2, 1);
            register(&mut directory, &owner, &listener, DESCRIPTOR).unwrap();
        }
        let now = Instant::now();
        let first = directory.discover(11_001, 0, 0, now);
        assert_eq!(directory.active_snapshot_count(), 1);
        assert_eq!(
            directory
                .discover(11_001, first.directory_revision, first.next_cursor + 1, now,)
                .status,
            DiscoveryStatus::Conflict
        );
        assert_eq!(directory.snapshots.len(), 1);
        assert_eq!(directory.active_snapshot_count(), 1);

        assert_eq!(directory.expire_snapshots(now + Duration::from_secs(3)), 1);
        assert_eq!(directory.snapshots.len(), 0);
        assert_eq!(directory.active_snapshot_count(), 1);
        assert_eq!(
            directory
                .discover(
                    11_001,
                    first.directory_revision,
                    first.next_cursor,
                    now + Duration::from_secs(3),
                )
                .status,
            DiscoveryStatus::Conflict
        );
        drop(first);
        assert_eq!(directory.active_snapshot_count(), 0);
    }

    #[test]
    fn stale_attempt_listener_and_endpoint_cleanup_cannot_remove_successors() {
        let mut directory = CommandDirectory::new(limits());
        let old_owner = owner_named("builder", 1, 1);
        let old_listener = listener(&old_owner, 2, 1);
        register(&mut directory, &old_owner, &old_listener, DESCRIPTOR).unwrap();
        assert!(directory.invalidate_attempt(1, 1, 1));

        let mut new_owner = old_owner.clone();
        new_owner.attempt = 2;
        new_owner.endpoint_generation = 12;
        let new_listener = listener(&new_owner, 2, 2);
        register(&mut directory, &new_owner, &new_listener, DESCRIPTOR).unwrap();
        assert!(!directory.invalidate_attempt(1, 1, 1));
        assert!(!directory.invalidate_listener(&old_listener));
        assert_eq!(
            directory.invalidate_endpoint(old_owner.endpoint_id, old_owner.endpoint_generation),
            0
        );
        assert_eq!(directory.record_count(), 1);
        assert!(directory.invalidate_definition(1, 1));
        assert_eq!(directory.record_count(), 0);
    }

    #[test]
    fn unregister_disable_and_remove_invalidate_visible_records_once() {
        let mut directory = CommandDirectory::new(limits());
        let owner = owner_named("builder", 1, 1);
        let listener = listener(&owner, 2, 1);
        register(&mut directory, &owner, &listener, DESCRIPTOR).unwrap();
        let revision = directory.revision();
        let prepared = directory
            .prepare_registration(Some(&owner), 0, "", None)
            .unwrap();
        let result = directory
            .commit_registration(prepared, Some(&owner), None)
            .unwrap();
        assert!(result.changed);
        assert_eq!(directory.revision(), revision + 1);

        let prepared = directory
            .prepare_registration(Some(&owner), 0, "", None)
            .unwrap();
        let result = directory
            .commit_registration(prepared, Some(&owner), None)
            .unwrap();
        assert!(!result.changed);
        assert_eq!(directory.revision(), revision + 1);

        register(&mut directory, &owner, &listener, DESCRIPTOR).unwrap();
        assert!(directory.invalidate_extension(owner.extension_id));
        assert!(!directory.invalidate_extension(owner.extension_id));
    }

    #[test]
    fn registration_status_mapping_is_wire_compatible() {
        assert_eq!(
            RegistrationError::Permission.status(),
            EXT_STATUS_PERMISSION
        );
        assert_eq!(RegistrationError::NotFound.status(), EXT_STATUS_NOT_FOUND);
        assert_eq!(RegistrationError::Invalid.status(), EXT_STATUS_INVALID);
        assert_eq!(RegistrationError::Conflict.status(), EXT_STATUS_CONFLICT);
        assert_eq!(RegistrationError::Budget.status(), EXT_STATUS_BUDGET);
        assert_eq!(DiscoveryStatus::Ok.status(), EXT_STATUS_OK);
    }
}
