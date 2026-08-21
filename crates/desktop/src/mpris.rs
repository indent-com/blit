//! Bounded MPRIS discovery and semantic action bridge.

use super::{
    Common, DBUS_CALL_TIMEOUT, Event, ImageResolver, MAX_ARTWORK_SOURCE_DIMENSION,
    MAX_SOURCE_IMAGE_BYTES, PngImage, clip_text,
};
use blit_remote::media::{
    LoopStatus, MPRIS_ARTIST_MAX, MPRIS_ARTWORK_MAX, MPRIS_CAN_CONTROL, MPRIS_CAN_GO_NEXT,
    MPRIS_CAN_GO_PREVIOUS, MPRIS_CAN_PAUSE, MPRIS_CAN_PLAY, MPRIS_CAN_RAISE, MPRIS_CAN_SEEK,
    MPRIS_CAN_SET_LOOP_STATUS, MPRIS_CAN_SET_RATE, MPRIS_CAN_SET_SHUFFLE, MPRIS_CAN_SET_VOLUME,
    MPRIS_PLAYER_MAX, MPRIS_STRING_MAX, MprisAction, MprisActionKind, MprisActionResult,
    MprisArtwork, MprisPlayer, MprisRecord, PlaybackStatus, artwork_url_allowed,
};
use blit_remote::{
    STATUS_BUDGET, STATUS_CONFLICT, STATUS_INVALID, STATUS_OK, STATUS_OTHER, STATUS_UNKNOWN_ID,
    STATUS_WRONG_TYPE,
};
use futures_util::StreamExt;
use std::collections::{HashMap, HashSet, VecDeque, hash_map::Entry};
use std::future::Future;
use std::sync::{Arc, Mutex as StdMutex};
use std::time::{Duration, Instant};
use tokio::sync::{Mutex, Notify, Semaphore};
use zbus::fdo::DBusProxy;
use zbus::names::{BusName, OwnedBusName};
use zbus::zvariant::{OwnedObjectPath, OwnedValue};
use zbus::{Connection, Proxy};

const PREFIX: &str = "org.mpris.MediaPlayer2.";
const PATH: &str = "/org/mpris/MediaPlayer2";
const BASE_INTERFACE: &str = "org.mpris.MediaPlayer2";
const PLAYER_INTERFACE: &str = "org.mpris.MediaPlayer2.Player";
const PROPERTIES_INTERFACE: &str = "org.freedesktop.DBus.Properties";
const ARTWORK_SIZE: u32 = 512;
/// Descending sizes tried until the re-encoded PNG fits `MPRIS_ARTWORK_MAX`.
/// See `fit_artwork`.
const ARTWORK_FIT_SIZES: [u32; 3] = [ARTWORK_SIZE, 384, 256];
const ARTWORK_BUDGET: usize = 8 * 1024 * 1024;
const PROPERTY_CALL_LIMIT: usize = 4;
const ACTION_CALL_LIMIT: usize = 4;
const ACTION_QUEUE_LIMIT: usize = 64;
const MONITOR_RETRY_DELAY: Duration = Duration::from_secs(5);

#[derive(Clone)]
struct Target {
    owner: String,
    aliases: HashSet<String>,
    player: MprisPlayer,
    track_path: Option<OwnedObjectPath>,
    last_playing: u64,
    last_active: u64,
    last_artwork: u64,
    position_observed_at: Instant,
}

struct DeferredTarget {
    owner: String,
    retry_at: Option<Instant>,
}

fn player_at(target: &Target, observed_at: Instant) -> MprisPlayer {
    let mut player = target.player.clone();
    if player.playback_status == PlaybackStatus::Playing {
        let elapsed_us = observed_at
            .saturating_duration_since(target.position_observed_at)
            .as_micros()
            .min(i64::MAX as u128) as i128;
        let delta = elapsed_us.saturating_mul(i128::from(player.rate_ppm)) / 1_000_000;
        player.position_us =
            (i128::from(player.position_us) + delta).clamp(0, i128::from(i64::MAX)) as i64;
    } else {
        player.position_us = player.position_us.max(0);
    }
    if player.length_us >= 0 {
        player.position_us = player.position_us.min(player.length_us);
    }
    player
}

pub(super) struct State {
    next_player_id: u32,
    activity: u64,
    players: HashMap<u32, Target>,
    by_owner: HashMap<String, u32>,
    by_alias: HashMap<String, u32>,
    deferred: HashMap<String, DeferredTarget>,
    active: Option<u32>,
    artwork_bytes: usize,
    property_calls: Arc<Semaphore>,
    capacity_changed: Arc<Notify>,
}

impl Default for State {
    fn default() -> Self {
        Self {
            next_player_id: 0,
            activity: 0,
            players: HashMap::new(),
            by_owner: HashMap::new(),
            by_alias: HashMap::new(),
            deferred: HashMap::new(),
            active: None,
            artwork_bytes: 0,
            property_calls: Arc::new(Semaphore::new(PROPERTY_CALL_LIMIT)),
            capacity_changed: Arc::new(Notify::new()),
        }
    }
}

impl State {
    fn allocate_id(&mut self) -> u32 {
        loop {
            self.next_player_id = self.next_player_id.wrapping_add(1).max(1);
            if !self.players.contains_key(&self.next_player_id) {
                return self.next_player_id;
            }
        }
    }

    fn tick_activity(&mut self) -> u64 {
        self.activity = self.activity.wrapping_add(1).max(1);
        self.activity
    }

    fn set_active(&mut self, player_id: u32) -> Vec<MprisRecord> {
        if self.active == Some(player_id) || !self.players.contains_key(&player_id) {
            return Vec::new();
        }
        let activity = self.tick_activity();
        let observed_at = Instant::now();
        let mut changed = Vec::with_capacity(2);
        if let Some(old_id) = self.active
            && let Some(old) = self.players.get_mut(&old_id)
        {
            old.player.active = false;
            old.player.revision = old.player.revision.wrapping_add(1).max(1);
            changed.push(MprisRecord::Upsert(player_at(old, observed_at)));
        }
        self.active = Some(player_id);
        if let Some(target) = self.players.get_mut(&player_id) {
            target.player.active = true;
            target.player.revision = target.player.revision.wrapping_add(1).max(1);
            target.last_active = activity;
            changed.push(MprisRecord::Upsert(player_at(target, observed_at)));
        }
        changed
    }

    fn choose_active(&self) -> Option<u32> {
        self.players
            .iter()
            .filter(|(_, target)| target.player.playback_status == PlaybackStatus::Playing)
            .max_by_key(|(_, target)| target.last_playing)
            .map(|(id, _)| *id)
            .or_else(|| {
                self.players
                    .iter()
                    .filter(|(_, target)| target.last_active != 0)
                    .max_by_key(|(_, target)| target.last_active)
                    .map(|(id, _)| *id)
            })
            .or_else(|| self.players.keys().copied().min())
    }

    fn remove_player(&mut self, player_id: u32) -> Vec<MprisRecord> {
        let Some(target) = self.players.remove(&player_id) else {
            return Vec::new();
        };
        self.by_owner.remove(&target.owner);
        self.artwork_bytes = self
            .artwork_bytes
            .saturating_sub(target.player.artwork.png_len());
        for alias in target.aliases {
            self.by_alias.remove(&alias);
        }
        self.capacity_changed.notify_one();
        let mut records = vec![MprisRecord::Delete { player_id }];
        if self.active == Some(player_id) {
            self.active = None;
            if let Some(next) = self.choose_active() {
                records.extend(self.set_active(next));
            }
        }
        records
    }

    fn remove_player_for_retry(&mut self, player_id: u32, retry_at: Instant) -> Vec<MprisRecord> {
        let Some(target) = self.players.get(&player_id) else {
            return Vec::new();
        };
        let owner = target.owner.clone();
        let aliases = target.aliases.iter().cloned().collect::<Vec<_>>();
        let records = self.remove_player(player_id);
        for alias in aliases {
            self.deferred.insert(
                alias,
                DeferredTarget {
                    owner: owner.clone(),
                    retry_at: Some(retry_at),
                },
            );
        }
        records
    }

    fn deferred_candidate(&self, limit: usize, now: Instant) -> Option<(String, String)> {
        let ready = |target: &&DeferredTarget| target.retry_at.is_none_or(|at| now >= at);
        self.deferred
            .iter()
            .filter(|(_, target)| {
                ready(target) && self.by_owner.contains_key(target.owner.as_str())
            })
            .min_by_key(|(alias, _)| alias.as_str())
            .map(|(alias, target)| (alias.clone(), target.owner.clone()))
            .or_else(|| {
                (self.players.len() < limit)
                    .then(|| {
                        self.deferred
                            .iter()
                            .filter(|(_, target)| ready(target))
                            .min_by_key(|(alias, _)| alias.as_str())
                            .map(|(alias, target)| (alias.clone(), target.owner.clone()))
                    })
                    .flatten()
            })
    }
}

#[derive(Clone)]
pub(super) struct ActionDispatcher {
    slots: Arc<Semaphore>,
    queues: Arc<StdMutex<ActionQueues>>,
    connection: Connection,
    state: Arc<Mutex<State>>,
    common: Common,
}

struct ActionJob {
    requester: u64,
    action: MprisAction,
}

#[derive(Default)]
struct ActionQueues {
    players: HashMap<u32, VecDeque<ActionJob>>,
    pending: usize,
}

impl ActionQueues {
    fn enqueue(&mut self, job: ActionJob) -> Result<bool, ActionJob> {
        if self.pending >= ACTION_QUEUE_LIMIT {
            return Err(job);
        }
        self.pending += 1;
        match self.players.entry(job.action.player_id) {
            Entry::Occupied(mut entry) => {
                entry.get_mut().push_back(job);
                Ok(false)
            }
            Entry::Vacant(entry) => {
                entry.insert(VecDeque::from([job]));
                Ok(true)
            }
        }
    }

    fn take_next(&mut self, player_id: u32) -> Option<ActionJob> {
        let queue = self.players.get_mut(&player_id)?;
        let job = queue.pop_front();
        if job.is_none() {
            self.players.remove(&player_id);
        }
        job
    }

    fn complete_one(&mut self) {
        self.pending = self.pending.saturating_sub(1);
    }
}

impl ActionDispatcher {
    pub(super) fn new(connection: Connection, state: Arc<Mutex<State>>, common: Common) -> Self {
        Self {
            slots: Arc::new(Semaphore::new(ACTION_CALL_LIMIT)),
            queues: Arc::new(StdMutex::new(ActionQueues::default())),
            connection,
            state,
            common,
        }
    }

    pub(super) fn dispatch(&self, requester: u64, action: MprisAction) {
        let player_id = action.player_id;
        let job = ActionJob { requester, action };
        let queued = self
            .queues
            .lock()
            .expect("MPRIS action queue mutex poisoned")
            .enqueue(job);
        let start_worker = match queued {
            Ok(start_worker) => start_worker,
            Err(job) => {
                let common = self.common.clone();
                tokio::spawn(async move {
                    send_result(&common, job.requester, job.action, STATUS_BUDGET, 0).await;
                });
                return;
            }
        };
        if !start_worker {
            return;
        }
        let dispatcher = self.clone();
        tokio::spawn(async move {
            dispatcher.run_player(player_id).await;
        });
    }

    async fn run_player(self, player_id: u32) {
        loop {
            let job = self
                .queues
                .lock()
                .expect("MPRIS action queue mutex poisoned")
                .take_next(player_id);
            let Some(job) = job else {
                return;
            };
            let permit = self
                .slots
                .acquire()
                .await
                .expect("MPRIS action semaphore is never closed");
            handle_action(
                job.requester,
                job.action,
                &self.connection,
                &self.state,
                &self.common,
            )
            .await;
            drop(permit);
            self.queues
                .lock()
                .expect("MPRIS action queue mutex poisoned")
                .complete_one();
        }
    }
}

pub(super) async fn watch(
    connection: Connection,
    state: std::sync::Arc<Mutex<State>>,
    common: Common,
) {
    let Ok(bus) = DBusProxy::new(&connection).await else {
        return;
    };
    // Subscribe before enumerating. Owner changes that race the startup scan
    // are then queued and reconciled after the initial, sorted registrations.
    let Ok(mut changes) = bus.receive_name_owner_changed().await else {
        return;
    };
    let mut names = match bus.list_names().await {
        Ok(names) => names
            .into_iter()
            .map(|name| name.to_string())
            .filter(|name| name.starts_with(PREFIX))
            .collect::<Vec<_>>(),
        Err(_) => Vec::new(),
    };
    names.sort();
    for alias in names {
        if let Ok(name) = BusName::try_from(alias.as_str())
            && let Ok(owner) = bus.get_name_owner(name).await
        {
            register_alias(&connection, &state, &common, alias, owner.to_string()).await;
        }
    }

    let capacity_changed = state.lock().await.capacity_changed.clone();
    loop {
        let signal = tokio::select! {
            signal = changes.next() => {
                let Some(signal) = signal else { return; };
                signal
            }
            () = capacity_changed.notified() => {
                retry_deferred(&connection, &state, &common).await;
                continue;
            }
        };
        let Ok(args) = signal.args() else {
            continue;
        };
        let name = args.name().to_string();
        let old_owner = args.old_owner().as_ref().map(ToString::to_string);
        let new_owner = args.new_owner().as_ref().map(ToString::to_string);
        if name.starts_with(PREFIX) {
            if let Some(old_owner) = old_owner.as_deref() {
                remove_alias(&state, &common, &name, old_owner).await;
            }
            if let Some(new_owner) = new_owner {
                register_alias(&connection, &state, &common, name, new_owner).await;
            }
        } else if name.starts_with(':') && new_owner.is_none() {
            remove_owner(&state, &common, &name).await;
        }
    }
}

async fn register_alias(
    connection: &Connection,
    state: &std::sync::Arc<Mutex<State>>,
    common: &Common,
    alias: String,
    owner: String,
) {
    let player_id = {
        let mut state = state.lock().await;
        if state.by_alias.contains_key(&alias) {
            state.deferred.remove(&alias);
            return;
        }
        if let Some(player_id) = state.by_owner.get(&owner).copied() {
            state.deferred.remove(&alias);
            state.by_alias.insert(alias.clone(), player_id);
            if let Some(target) = state.players.get_mut(&player_id) {
                target.aliases.insert(alias);
            }
            return;
        }
        if state.players.len() >= player_limit() {
            state.deferred.insert(
                alias,
                DeferredTarget {
                    owner,
                    retry_at: None,
                },
            );
            return;
        }
        state.deferred.remove(&alias);
        let player_id = state.allocate_id();
        let player = empty_player(player_id);
        state.players.insert(
            player_id,
            Target {
                owner: owner.clone(),
                aliases: HashSet::from([alias.clone()]),
                player,
                track_path: None,
                last_playing: 0,
                last_active: 0,
                last_artwork: 0,
                position_observed_at: Instant::now(),
            },
        );
        state.by_owner.insert(owner, player_id);
        state.by_alias.insert(alias, player_id);
        player_id
    };
    tokio::spawn(monitor_player(
        connection.clone(),
        state.clone(),
        common.clone(),
        player_id,
    ));
}

async fn remove_alias(
    state: &std::sync::Arc<Mutex<State>>,
    common: &Common,
    alias: &str,
    owner: &str,
) {
    let records = {
        let mut state = state.lock().await;
        let player_id = state.by_alias.get(alias).copied().filter(|player_id| {
            state
                .players
                .get(player_id)
                .is_some_and(|target| target.owner == owner)
        });
        if let Some(player_id) = player_id {
            state.by_alias.remove(alias);
            let target = state
                .players
                .get_mut(&player_id)
                .expect("alias player checked above");
            target.aliases.remove(alias);
            if target.aliases.is_empty() {
                state.remove_player(player_id)
            } else {
                Vec::new()
            }
        } else {
            if state
                .deferred
                .get(alias)
                .is_some_and(|value| value.owner == owner)
            {
                state.deferred.remove(alias);
            }
            Vec::new()
        }
    };
    send_records(common, records).await;
}

async fn remove_owner(state: &std::sync::Arc<Mutex<State>>, common: &Common, owner: &str) {
    let records = {
        let mut state = state.lock().await;
        state.deferred.retain(|_, value| value.owner != owner);
        state
            .by_owner
            .get(owner)
            .copied()
            .map(|id| state.remove_player(id))
            .unwrap_or_default()
    };
    send_records(common, records).await;
}

async fn remove_monitored_player(
    state: &std::sync::Arc<Mutex<State>>,
    common: &Common,
    player_id: u32,
    owner: &str,
) {
    let removed = {
        let mut state = state.lock().await;
        if state.by_owner.get(owner) == Some(&player_id) {
            Some(state.remove_player_for_retry(player_id, Instant::now() + MONITOR_RETRY_DELAY))
        } else {
            None
        }
    };
    let Some(records) = removed else {
        return;
    };
    send_records(common, records).await;
    let capacity_changed = state.lock().await.capacity_changed.clone();
    tokio::spawn(async move {
        tokio::time::sleep(MONITOR_RETRY_DELAY).await;
        capacity_changed.notify_one();
    });
}

async fn retry_deferred(
    connection: &Connection,
    state: &std::sync::Arc<Mutex<State>>,
    common: &Common,
) {
    loop {
        let candidate = state
            .lock()
            .await
            .deferred_candidate(player_limit(), Instant::now());
        let Some((alias, owner)) = candidate else {
            return;
        };
        register_alias(connection, state, common, alias, owner).await;
    }
}

async fn monitor_player(
    connection: Connection,
    state: std::sync::Arc<Mutex<State>>,
    common: Common,
    player_id: u32,
) {
    let (owner, property_calls) = {
        let state = state.lock().await;
        let Some(target) = state.players.get(&player_id) else {
            return;
        };
        (target.owner.clone(), state.property_calls.clone())
    };
    let Ok(player_proxy) = proxy(&connection, &owner, PLAYER_INTERFACE).await else {
        remove_monitored_player(&state, &common, player_id, &owner).await;
        return;
    };
    let Ok(properties_proxy) = proxy(&connection, &owner, PROPERTIES_INTERFACE).await else {
        remove_monitored_player(&state, &common, player_id, &owner).await;
        return;
    };
    let Ok(mut changed) = properties_proxy.receive_signal("PropertiesChanged").await else {
        remove_monitored_player(&state, &common, player_id, &owner).await;
        return;
    };
    let Ok(mut seeked) = player_proxy.receive_signal("Seeked").await else {
        remove_monitored_player(&state, &common, player_id, &owner).await;
        return;
    };
    let mut failures = 0u8;
    loop {
        if state.lock().await.by_owner.get(&owner) != Some(&player_id) {
            return;
        }
        let refresh = tokio::time::timeout(
            DBUS_CALL_TIMEOUT,
            read_player(&connection, &owner, player_id, &property_calls),
        )
        .await;
        match refresh {
            Ok(Ok(mut snapshot)) => {
                failures = 0;
                attach_artwork(&mut snapshot, &common.images).await;
                let records = install_snapshot(&state, snapshot).await;
                send_records(&common, records).await;
            }
            Ok(Err(_)) | Err(_) => {
                failures = failures.saturating_add(1);
                if failures >= 3 {
                    remove_monitored_player(&state, &common, player_id, &owner).await;
                    return;
                }
                // A failed refresh cannot rely on another signal arriving.
                // Retry until the three-consecutive-timeout contract is
                // resolved, with a small delay to avoid a tight error loop.
                tokio::time::sleep(Duration::from_millis(50)).await;
                continue;
            }
        }
        let signal_alive = tokio::select! {
            signal = changed.next() => signal.is_some(),
            signal = seeked.next() => signal.is_some(),
        };
        if !signal_alive {
            remove_monitored_player(&state, &common, player_id, &owner).await;
            return;
        }
        // Coalesce bursts of invalidations into one bounded property refresh.
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

struct Snapshot {
    player: MprisPlayer,
    /// Carried rather than resolved in place: see `attach_artwork`.
    art_url: String,
    track_path: Option<OwnedObjectPath>,
    position_observed_at: Instant,
}

async fn install_snapshot(
    state: &std::sync::Arc<Mutex<State>>,
    mut snapshot: Snapshot,
) -> Vec<MprisRecord> {
    let mut state = state.lock().await;
    let Some(old) = state.players.get(&snapshot.player.player_id) else {
        return Vec::new();
    };
    snapshot.player.track_revision = if track_changed(old, &snapshot) {
        old.player.track_revision.wrapping_add(1).max(1)
    } else {
        old.player.track_revision
    };
    snapshot.player.revision = old.player.revision.wrapping_add(1).max(1);
    snapshot.player.active = old.player.active;
    let became_playing = old.player.playback_status != PlaybackStatus::Playing
        && snapshot.player.playback_status == PlaybackStatus::Playing;
    // Only covers carried as bytes occupy the budget, so a player whose art is
    // a URL is never a candidate for eviction and never forces one.
    state.artwork_bytes = state
        .artwork_bytes
        .saturating_sub(old.player.artwork.png_len());
    let artwork_budget = artwork_budget();
    let mut records = Vec::new();
    while snapshot.player.artwork.png_len() > 0
        && state.artwork_bytes + snapshot.player.artwork.png_len() > artwork_budget
    {
        let Some(evict_id) = state
            .players
            .iter()
            .filter(|(id, target)| {
                **id != snapshot.player.player_id && target.player.artwork.png_len() > 0
            })
            .min_by_key(|(_, target)| target.last_artwork)
            .map(|(id, _)| *id)
        else {
            snapshot.player.artwork = MprisArtwork::None;
            break;
        };
        if let Some((bytes, record)) = state.players.get_mut(&evict_id).map(|evicted| {
            let bytes = evicted.player.artwork.png_len();
            evicted.player.artwork = MprisArtwork::None;
            evicted.player.revision = evicted.player.revision.wrapping_add(1).max(1);
            evicted.last_artwork = 0;
            (
                bytes,
                MprisRecord::Upsert(player_at(evicted, Instant::now())),
            )
        }) {
            state.artwork_bytes = state.artwork_bytes.saturating_sub(bytes);
            records.push(record);
        }
    }
    let artwork_activity = (snapshot.player.artwork.png_len() > 0).then(|| state.tick_activity());
    state.artwork_bytes += snapshot.player.artwork.png_len();
    let activity = became_playing.then(|| state.tick_activity());
    let player_id = snapshot.player.player_id;
    if let Some(target) = state.players.get_mut(&player_id) {
        target.player = snapshot.player.clone();
        target.track_path = snapshot.track_path;
        target.position_observed_at = snapshot.position_observed_at;
        if let Some(activity) = activity {
            target.last_playing = activity;
        }
        target.last_artwork = artwork_activity.unwrap_or(0);
    }
    if (became_playing || state.active.is_none()) && state.active != Some(player_id) {
        records.extend(state.set_active(player_id));
    } else if let Some(target) = state.players.get(&player_id) {
        records.push(MprisRecord::Upsert(player_at(target, Instant::now())));
    }
    records
}

fn track_changed(old: &Target, snapshot: &Snapshot) -> bool {
    old.track_path != snapshot.track_path
        || old.player.title != snapshot.player.title
        || old.player.album != snapshot.player.album
        || old.player.artists != snapshot.player.artists
        || old.player.length_us != snapshot.player.length_us
}

async fn handle_action(
    requester: u64,
    action: MprisAction,
    connection: &Connection,
    state: &std::sync::Arc<Mutex<State>>,
    common: &Common,
) {
    if action.kind == MprisActionKind::SelectActive {
        let (status, revision, records) = {
            let mut state = state.lock().await;
            if !state.players.contains_key(&action.player_id) {
                (STATUS_UNKNOWN_ID, 0, Vec::new())
            } else {
                let records = state.set_active(action.player_id);
                let revision = state
                    .players
                    .get(&action.player_id)
                    .map_or(0, |target| target.player.revision);
                (STATUS_OK, revision, records)
            }
        };
        send_records(common, records).await;
        send_result(common, requester, action, status, revision).await;
        return;
    }

    // The keyed dispatcher serializes this with earlier actions for the same
    // player, so the state read observes their completed refreshes.
    let target = {
        let state = state.lock().await;
        state
            .players
            .get(&action.player_id)
            .cloned()
            .map(|target| (target, state.property_calls.clone()))
    };
    let Some((target, property_calls)) = target else {
        send_result(common, requester, action, STATUS_UNKNOWN_ID, 0).await;
        return;
    };
    let status = match tokio::time::timeout(
        DBUS_CALL_TIMEOUT,
        execute_action(connection, &target, action, &property_calls),
    )
    .await
    {
        Ok(Ok(())) => STATUS_OK,
        Ok(Err(status)) => status,
        Err(_) => STATUS_OTHER,
    };
    let mut revision = target.player.revision;
    if status == STATUS_OK
        && let Ok(mut snapshot) = tokio::time::timeout(
            DBUS_CALL_TIMEOUT,
            read_player(connection, &target.owner, action.player_id, &property_calls),
        )
        .await
        .unwrap_or(Err("timeout"))
    {
        attach_artwork(&mut snapshot, &common.images).await;
        let mut records = install_snapshot(state, snapshot).await;
        {
            let mut state = state.lock().await;
            records.extend(state.set_active(action.player_id));
            revision = state
                .players
                .get(&action.player_id)
                .map_or(0, |target| target.player.revision);
        }
        send_records(common, records).await;
    }
    send_result(common, requester, action, status, revision).await;
}

async fn execute_action(
    connection: &Connection,
    target: &Target,
    action: MprisAction,
    property_calls: &Semaphore,
) -> Result<(), u8> {
    if action.kind == MprisActionKind::Raise {
        let base = proxy(connection, &target.owner, BASE_INTERFACE)
            .await
            .map_err(|_| STATUS_OTHER)?;
        require_bool(&base, property_calls, "CanRaise").await?;
        return base
            .call::<_, _, ()>("Raise", &())
            .await
            .map_err(|_| STATUS_OTHER);
    }
    let player = proxy(connection, &target.owner, PLAYER_INTERFACE)
        .await
        .map_err(|_| STATUS_OTHER)?;
    require_bool(&player, property_calls, "CanControl").await?;
    match action.kind {
        MprisActionKind::Play => {
            require_bool(&player, property_calls, "CanPlay").await?;
            player
                .call::<_, _, ()>("Play", &())
                .await
                .map_err(|_| STATUS_OTHER)
        }
        MprisActionKind::Pause => {
            require_bool(&player, property_calls, "CanPause").await?;
            player
                .call::<_, _, ()>("Pause", &())
                .await
                .map_err(|_| STATUS_OTHER)
        }
        MprisActionKind::PlayPause => {
            let playback_status = get_string_property(&player, property_calls, "PlaybackStatus")
                .await
                .map_err(|_| STATUS_OTHER)?;
            match playback_status.as_str() {
                "Playing" => require_bool(&player, property_calls, "CanPause").await?,
                "Paused" | "Stopped" => require_bool(&player, property_calls, "CanPlay").await?,
                _ => return Err(STATUS_OTHER),
            }
            player
                .call::<_, _, ()>("PlayPause", &())
                .await
                .map_err(|_| STATUS_OTHER)
        }
        MprisActionKind::Stop => player
            .call::<_, _, ()>("Stop", &())
            .await
            .map_err(|_| STATUS_OTHER),
        MprisActionKind::Next => {
            require_bool(&player, property_calls, "CanGoNext").await?;
            player
                .call::<_, _, ()>("Next", &())
                .await
                .map_err(|_| STATUS_OTHER)
        }
        MprisActionKind::Previous => {
            require_bool(&player, property_calls, "CanGoPrevious").await?;
            player
                .call::<_, _, ()>("Previous", &())
                .await
                .map_err(|_| STATUS_OTHER)
        }
        MprisActionKind::Seek => {
            require_bool(&player, property_calls, "CanSeek").await?;
            player
                .call::<_, _, ()>("Seek", &(action.value,))
                .await
                .map_err(|_| STATUS_OTHER)
        }
        MprisActionKind::SetPosition => {
            require_bool(&player, property_calls, "CanSeek").await?;
            if action.track_revision == 0 || action.track_revision != target.player.track_revision {
                return Err(STATUS_CONFLICT);
            }
            let metadata = get_metadata_property(&player, property_calls, "Metadata")
                .await
                .map_err(|_| STATUS_OTHER)?;
            let current_path = metadata
                .get("mpris:trackid")
                .and_then(|value| value.try_clone().ok())
                .and_then(|value| OwnedObjectPath::try_from(value).ok());
            let Some(path) = target.track_path.as_ref() else {
                return Err(STATUS_CONFLICT);
            };
            if current_path.as_ref() != Some(path)
                || path.as_str() == "/org/mpris/MediaPlayer2/TrackList/NoTrack"
                || !metadata_matches_target(&metadata, target)
            {
                return Err(STATUS_CONFLICT);
            }
            let length = metadata
                .get("mpris:length")
                .and_then(|value| i64::try_from(value).ok())
                .filter(|value| *value >= 0);
            if action.value < 0 || length.is_some_and(|length| action.value >= length) {
                return Err(STATUS_INVALID);
            }
            player
                .call::<_, _, ()>("SetPosition", &(path, action.value))
                .await
                .map_err(|_| STATUS_OTHER)
        }
        MprisActionKind::Volume => {
            if !(0..=4_000_000).contains(&action.value) {
                return Err(STATUS_INVALID);
            }
            get_f64_property(&player, property_calls, "Volume")
                .await
                .map_err(|_| STATUS_OTHER)?;
            set_f64_property(
                &player,
                property_calls,
                "Volume",
                action.value as f64 / 1_000_000.0,
            )
            .await
            .map_err(|_| STATUS_OTHER)
        }
        MprisActionKind::Shuffle => {
            if !(0..=1).contains(&action.value) {
                return Err(STATUS_INVALID);
            }
            get_bool_property(&player, property_calls, "Shuffle")
                .await
                .map_err(|_| STATUS_OTHER)?;
            set_bool_property(&player, property_calls, "Shuffle", action.value == 1)
                .await
                .map_err(|_| STATUS_OTHER)
        }
        MprisActionKind::LoopStatus => {
            let value = match action.value {
                0 => "None",
                1 => "Track",
                2 => "Playlist",
                _ => return Err(STATUS_INVALID),
            };
            get_string_property(&player, property_calls, "LoopStatus")
                .await
                .map_err(|_| STATUS_OTHER)?;
            set_str_property(&player, property_calls, "LoopStatus", value)
                .await
                .map_err(|_| STATUS_OTHER)
        }
        MprisActionKind::Rate => {
            get_f64_property(&player, property_calls, "Rate")
                .await
                .map_err(|_| STATUS_OTHER)?;
            let minimum = get_f64_property(&player, property_calls, "MinimumRate")
                .await
                .map_err(|_| STATUS_OTHER)?;
            let maximum = get_f64_property(&player, property_calls, "MaximumRate")
                .await
                .map_err(|_| STATUS_OTHER)?;
            let value = action.value as f64 / 1_000_000.0;
            if action.value == 0
                || !minimum.is_finite()
                || !maximum.is_finite()
                || value < minimum
                || value > maximum
            {
                return Err(STATUS_INVALID);
            }
            set_f64_property(&player, property_calls, "Rate", value)
                .await
                .map_err(|_| STATUS_OTHER)
        }
        MprisActionKind::Raise | MprisActionKind::SelectActive => Ok(()),
    }
}

async fn require_bool(
    proxy: &Proxy<'_>,
    property_calls: &Semaphore,
    property: &str,
) -> Result<(), u8> {
    match get_bool_property(proxy, property_calls, property).await {
        Ok(true) => Ok(()),
        Ok(false) => Err(STATUS_WRONG_TYPE),
        Err(_) => Err(STATUS_OTHER),
    }
}

fn metadata_matches_target(metadata: &HashMap<String, OwnedValue>, target: &Target) -> bool {
    let artists = metadata
        .get("xesam:artist")
        .and_then(|value| value.try_clone().ok())
        .and_then(|value| Vec::<String>::try_from(value).ok())
        .unwrap_or_default()
        .into_iter()
        .take(MPRIS_ARTIST_MAX)
        .map(|artist| clip_text(&artist, MPRIS_STRING_MAX))
        .collect::<Vec<_>>();
    let length = metadata
        .get("mpris:length")
        .and_then(|value| i64::try_from(value).ok())
        .map(|value| value.max(0))
        .unwrap_or(-1);
    metadata_string(metadata, "xesam:title") == target.player.title
        && metadata_string(metadata, "xesam:album") == target.player.album
        && artists == target.player.artists
        && length == target.player.length_us
}

async fn read_player(
    connection: &Connection,
    owner: &str,
    player_id: u32,
    property_calls: &Semaphore,
) -> Result<Snapshot, &'static str> {
    let properties = proxy(connection, owner, PROPERTIES_INTERFACE)
        .await
        .map_err(|_| "properties proxy")?;
    let (base_properties, player_properties) = tokio::join!(
        get_all_properties(&properties, property_calls, BASE_INTERFACE),
        async {
            let properties =
                get_all_properties(&properties, property_calls, PLAYER_INTERFACE).await;
            (properties, Instant::now())
        }
    );
    let base_properties = base_properties.map_err(|_| "base properties")?;
    let (player_properties, position_observed_at) = player_properties;
    let player_properties = player_properties.map_err(|_| "player properties")?;
    let identity = map_string(&base_properties, "Identity").ok_or("identity")?;
    let playback_status = match map_string(&player_properties, "PlaybackStatus").as_deref() {
        Some("Playing") => PlaybackStatus::Playing,
        Some("Paused") => PlaybackStatus::Paused,
        Some("Stopped") => PlaybackStatus::Stopped,
        _ => return Err("playback status"),
    };
    let metadata = player_properties
        .get("Metadata")
        .and_then(|value| value.try_clone().ok())
        .and_then(|value| HashMap::<String, OwnedValue>::try_from(value).ok())
        .ok_or("metadata")?;
    let can_control = map_bool(&player_properties, "CanControl").unwrap_or(false);
    let mut capabilities = 0u16;
    if can_control {
        capabilities |= MPRIS_CAN_CONTROL;
        capabilities |=
            u16::from(map_bool(&player_properties, "CanPlay").unwrap_or(false)) * MPRIS_CAN_PLAY;
        capabilities |=
            u16::from(map_bool(&player_properties, "CanPause").unwrap_or(false)) * MPRIS_CAN_PAUSE;
        capabilities |= u16::from(map_bool(&player_properties, "CanGoNext").unwrap_or(false))
            * MPRIS_CAN_GO_NEXT;
        capabilities |= u16::from(map_bool(&player_properties, "CanGoPrevious").unwrap_or(false))
            * MPRIS_CAN_GO_PREVIOUS;
        capabilities |=
            u16::from(map_bool(&player_properties, "CanSeek").unwrap_or(false)) * MPRIS_CAN_SEEK;
        capabilities |=
            u16::from(map_finite(&player_properties, "Volume").is_some()) * MPRIS_CAN_SET_VOLUME;
        capabilities |=
            u16::from(map_bool(&player_properties, "Shuffle").is_some()) * MPRIS_CAN_SET_SHUFFLE;
        capabilities |= u16::from(map_string(&player_properties, "LoopStatus").is_some())
            * MPRIS_CAN_SET_LOOP_STATUS;
        capabilities |=
            u16::from(map_finite(&player_properties, "Rate").is_some()) * MPRIS_CAN_SET_RATE;
    }
    capabilities |=
        u16::from(map_bool(&base_properties, "CanRaise").unwrap_or(false)) * MPRIS_CAN_RAISE;

    let loop_status = match map_string(&player_properties, "LoopStatus").as_deref() {
        Some("Track") => LoopStatus::Track,
        Some("Playlist") => LoopStatus::Playlist,
        _ => LoopStatus::None,
    };
    let rate = map_finite(&player_properties, "Rate").unwrap_or(1.0);
    let minimum_rate = map_finite(&player_properties, "MinimumRate").unwrap_or(1.0);
    let maximum_rate = map_finite(&player_properties, "MaximumRate").unwrap_or(1.0);
    let volume = map_finite(&player_properties, "Volume")
        .unwrap_or(1.0)
        .clamp(0.0, 4.0);
    let position = map_i64(&player_properties, "Position")
        .unwrap_or_default()
        .max(0);
    let title = metadata_string(&metadata, "xesam:title");
    let album = metadata_string(&metadata, "xesam:album");
    let artists = metadata
        .get("xesam:artist")
        .and_then(|value| value.try_clone().ok())
        .and_then(|value| Vec::<String>::try_from(value).ok())
        .unwrap_or_default()
        .into_iter()
        .take(MPRIS_ARTIST_MAX)
        .map(|artist| clip_text(&artist, MPRIS_STRING_MAX))
        .collect();
    let length_us = metadata
        .get("mpris:length")
        .and_then(|value| i64::try_from(value).ok())
        .map(|value| value.max(0))
        .unwrap_or(-1);
    let track_path = metadata
        .get("mpris:trackid")
        .and_then(|value| value.try_clone().ok())
        .and_then(|value| OwnedObjectPath::try_from(value).ok());
    let art_url = metadata_string(&metadata, "mpris:artUrl");
    Ok(Snapshot {
        player: MprisPlayer {
            player_id,
            revision: 0,
            track_revision: 0,
            active: false,
            playback_status,
            loop_status,
            shuffle: map_bool(&player_properties, "Shuffle").unwrap_or(false),
            capability_flags: capabilities,
            rate_ppm: ppm(rate),
            minimum_rate_ppm: ppm(minimum_rate),
            maximum_rate_ppm: ppm(maximum_rate),
            volume_ppm: (volume * 1_000_000.0).round() as u32,
            position_us: position,
            length_us,
            identity: clip_text(&identity, MPRIS_STRING_MAX),
            desktop_entry: clip_text(
                &map_string(&base_properties, "DesktopEntry").unwrap_or_default(),
                MPRIS_STRING_MAX,
            ),
            title,
            album,
            artists,
            artwork: MprisArtwork::None,
        },
        art_url,
        track_path,
        position_observed_at,
    })
}

/// Resolves the cover a snapshot named, outside the D-Bus refresh budget.
///
/// `read_player` is wrapped in `DBUS_CALL_TIMEOUT` by both of its callers, and
/// three consecutive expiries deregister the player. A remote cover costs
/// nothing to resolve now that it is only forwarded, but a local one still
/// reads and decodes a file up to three times looking for a size that fits, so
/// it stays out of a window whose overrun would cost the player its
/// registration rather than just its art.
async fn attach_artwork(snapshot: &mut Snapshot, images: &ImageResolver) {
    snapshot.player.artwork = artwork(images, &snapshot.art_url).await;
}

/// Turns an `mpris:artUrl` into something a viewer can display.
///
/// A URL the viewer can reach is forwarded untouched, which is both cheaper and
/// better: the browser fetches it off the UI thread, caches it by URL across
/// track changes, and the server never spends a fetch, a decode, a resize or a
/// re-encode on it. Art that exists only as local bytes cannot be named to a
/// browser, so it is normalized and carried instead.
async fn artwork(images: &ImageResolver, url: &str) -> MprisArtwork {
    if artwork_url_allowed(url) {
        return MprisArtwork::Url(url.to_string());
    }
    let png = if url.starts_with("file:") {
        let mut fitted = None;
        for size in ARTWORK_FIT_SIZES {
            let Some(image) = images
                .resolve(url.to_string(), None, size, MAX_ARTWORK_SOURCE_DIMENSION)
                .await
            else {
                break;
            };
            if image.png.len() <= MPRIS_ARTWORK_MAX {
                fitted = Some(image);
                break;
            }
        }
        fitted
    } else if url.starts_with("data:image/") {
        data_artwork(images, url).await
    } else {
        None
    };
    match png {
        Some(image) => MprisArtwork::Png(image.png),
        None => MprisArtwork::None,
    }
}

async fn data_artwork(images: &ImageResolver, url: &str) -> Option<PngImage> {
    let data = data_url::DataUrl::process(url).ok()?;
    if !data.mime_type().type_.eq_ignore_ascii_case("image") {
        return None;
    }
    let (bytes, _) = data.decode_to_vec().ok()?;
    if bytes.len() > MAX_SOURCE_IMAGE_BYTES {
        return None;
    }
    fit_artwork(images, bytes).await
}

/// Normalizes `bytes` to the largest size whose PNG fits the transport cap.
///
/// A 512×512 re-encode of unusually detailed art can exceed `MPRIS_ARTWORK_MAX`
/// even though the source was perfectly legal — measured at ~768 KiB against a
/// 512 KiB cap for near-incompressible input. The encoder omits an over-cap
/// cover rather than truncating it, so without stepping down the viewer would
/// see a coverless player and no sign that art had been found and discarded.
async fn fit_artwork(images: &ImageResolver, bytes: Vec<u8>) -> Option<PngImage> {
    for size in ARTWORK_FIT_SIZES {
        let image = images
            .encoded(bytes.clone(), size, MAX_ARTWORK_SOURCE_DIMENSION)
            .await?;
        if image.png.len() <= MPRIS_ARTWORK_MAX {
            return Some(image);
        }
    }
    None
}

async fn proxy(
    connection: &Connection,
    owner: &str,
    interface: &'static str,
) -> zbus::Result<Proxy<'static>> {
    let destination = OwnedBusName::try_from(owner.to_string())?;
    Proxy::new(connection, destination, PATH, interface).await
}

async fn bounded_property_call<F, T>(property_calls: &Semaphore, future: F) -> T
where
    F: Future<Output = T>,
{
    let _permit = property_calls
        .acquire()
        .await
        .expect("MPRIS property semaphore is never closed");
    future.await
}

async fn get_all_properties(
    proxy: &Proxy<'_>,
    property_calls: &Semaphore,
    interface: &str,
) -> zbus::Result<HashMap<String, OwnedValue>> {
    bounded_property_call(
        property_calls,
        proxy.call::<_, _, HashMap<String, OwnedValue>>("GetAll", &(interface,)),
    )
    .await
}

async fn get_bool_property(
    proxy: &Proxy<'_>,
    property_calls: &Semaphore,
    property: &str,
) -> zbus::Result<bool> {
    bounded_property_call(property_calls, proxy.get_property::<bool>(property)).await
}

async fn get_f64_property(
    proxy: &Proxy<'_>,
    property_calls: &Semaphore,
    property: &str,
) -> zbus::Result<f64> {
    bounded_property_call(property_calls, proxy.get_property::<f64>(property)).await
}

async fn get_string_property(
    proxy: &Proxy<'_>,
    property_calls: &Semaphore,
    property: &str,
) -> zbus::Result<String> {
    bounded_property_call(property_calls, proxy.get_property::<String>(property)).await
}

async fn get_metadata_property(
    proxy: &Proxy<'_>,
    property_calls: &Semaphore,
    property: &str,
) -> zbus::Result<HashMap<String, OwnedValue>> {
    bounded_property_call(
        property_calls,
        proxy.get_property::<HashMap<String, OwnedValue>>(property),
    )
    .await
}

async fn set_bool_property(
    proxy: &Proxy<'_>,
    property_calls: &Semaphore,
    property: &str,
    value: bool,
) -> zbus::fdo::Result<()> {
    bounded_property_call(property_calls, proxy.set_property(property, value)).await
}

async fn set_f64_property(
    proxy: &Proxy<'_>,
    property_calls: &Semaphore,
    property: &str,
    value: f64,
) -> zbus::fdo::Result<()> {
    bounded_property_call(property_calls, proxy.set_property(property, value)).await
}

async fn set_str_property(
    proxy: &Proxy<'_>,
    property_calls: &Semaphore,
    property: &str,
    value: &str,
) -> zbus::fdo::Result<()> {
    bounded_property_call(property_calls, proxy.set_property(property, value)).await
}

fn map_string(properties: &HashMap<String, OwnedValue>, property: &str) -> Option<String> {
    properties
        .get(property)
        .and_then(|value| <&str>::try_from(value).ok())
        .map(ToOwned::to_owned)
}

fn map_bool(properties: &HashMap<String, OwnedValue>, property: &str) -> Option<bool> {
    properties
        .get(property)
        .and_then(|value| bool::try_from(value).ok())
}

fn map_i64(properties: &HashMap<String, OwnedValue>, property: &str) -> Option<i64> {
    properties
        .get(property)
        .and_then(|value| i64::try_from(value).ok())
}

fn map_finite(properties: &HashMap<String, OwnedValue>, property: &str) -> Option<f64> {
    properties
        .get(property)
        .and_then(|value| f64::try_from(value).ok())
        .filter(|value| value.is_finite())
}

fn metadata_string(metadata: &HashMap<String, OwnedValue>, key: &str) -> String {
    clip_text(
        metadata
            .get(key)
            .and_then(|value| <&str>::try_from(value).ok())
            .unwrap_or_default(),
        MPRIS_STRING_MAX,
    )
}

fn ppm(value: f64) -> i32 {
    (value * 1_000_000.0)
        .round()
        .clamp(i32::MIN as f64, i32::MAX as f64) as i32
}

fn artwork_budget() -> usize {
    std::env::var("BLIT_MPRIS_ARTWORK_BYTES")
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .map(|value| value.min(ARTWORK_BUDGET))
        .unwrap_or(ARTWORK_BUDGET)
}

fn player_limit() -> usize {
    std::env::var("BLIT_MPRIS_MAX_PLAYERS")
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .map(|value| value.min(MPRIS_PLAYER_MAX))
        .unwrap_or(MPRIS_PLAYER_MAX)
}

fn empty_player(player_id: u32) -> MprisPlayer {
    MprisPlayer {
        player_id,
        revision: 0,
        track_revision: 0,
        active: false,
        playback_status: PlaybackStatus::Stopped,
        loop_status: LoopStatus::None,
        shuffle: false,
        capability_flags: 0,
        rate_ppm: 1_000_000,
        minimum_rate_ppm: 1_000_000,
        maximum_rate_ppm: 1_000_000,
        volume_ppm: 1_000_000,
        position_us: 0,
        length_us: -1,
        identity: String::new(),
        desktop_entry: String::new(),
        title: String::new(),
        album: String::new(),
        artists: Vec::new(),
        artwork: MprisArtwork::None,
    }
}

async fn send_records(common: &Common, records: Vec<MprisRecord>) {
    if !records.is_empty() {
        let _ = common.send(Event::Mpris(records)).await;
    }
}

async fn send_result(
    common: &Common,
    requester: u64,
    action: MprisAction,
    status: u8,
    revision: u32,
) {
    let _ = common
        .send(Event::MprisAction {
            requester,
            result: MprisActionResult {
                nonce: action.nonce,
                status,
                player_id: action.player_id,
                revision,
            },
        })
        .await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// The Spotify case: a cover named over HTTPS is forwarded verbatim, and the
    /// server neither fetches nor re-encodes it.
    #[tokio::test]
    async fn a_cover_named_over_https_is_forwarded_as_a_url() {
        let images = ImageResolver::new();
        let url = "https://i.scdn.co/image/ab67616d0000b2738ac778cc7d88779f74d33311";

        let resolved = artwork(&images, url).await;

        assert_eq!(resolved, MprisArtwork::Url(url.into()));
        // Costs the retained artwork budget nothing, which is the point.
        assert_eq!(resolved.png_len(), 0);
    }

    #[tokio::test]
    async fn a_url_the_viewer_could_not_load_yields_no_artwork() {
        let images = ImageResolver::new();
        for hostile in [
            "ftp://example.test/cover.png",
            "javascript:alert(1)",
            "/home/user/cover.png",
            "https://",
            "",
        ] {
            assert!(
                artwork(&images, hostile).await.is_none(),
                "{hostile} must not reach a viewer"
            );
        }
    }

    /// A local cover has no URL a browser could resolve, so it must still travel
    /// as normalized bytes — and at 640×640, over the icon source ceiling.
    #[tokio::test]
    async fn a_local_cover_still_travels_as_normalized_bytes() {
        let dir = std::env::temp_dir().join(format!("blit-art-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        let path = dir.join("cover.jpg");
        std::fs::write(&path, crate::test_http::cover_jpeg(640, 640)).expect("write cover");
        let images = ImageResolver::new();

        let resolved = artwork(&images, &format!("file://{}", path.display())).await;

        let MprisArtwork::Png(png) = &resolved else {
            panic!("expected bytes for a local cover, got {resolved:?}")
        };
        assert!(png.starts_with(b"\x89PNG"));
        assert!(png.len() <= MPRIS_ARTWORK_MAX);
        // Downscaled from 640 to the artwork ceiling rather than refused for
        // exceeding the icon-sized source limit.
        let decoded = image::load_from_memory(png).expect("decode normalized cover");
        assert_eq!(
            (decoded.width(), decoded.height()),
            (ARTWORK_SIZE, ARTWORK_SIZE)
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Detailed local art whose 512×512 re-encode overruns the transport cap
    /// must arrive smaller rather than not at all: the encoder omits an over-cap
    /// cover instead of truncating it.
    #[tokio::test]
    async fn art_that_will_not_fit_at_full_size_steps_down_instead_of_vanishing() {
        let images = ImageResolver::new();
        let fitted = fit_artwork(&images, crate::test_http::incompressible_cover(640))
            .await
            .expect("a cover too detailed for 512px must still arrive");

        assert!(fitted.png.len() <= MPRIS_ARTWORK_MAX);
        assert!(
            fitted.width < ARTWORK_SIZE as u16,
            "expected a step down from {ARTWORK_SIZE}, got {}",
            fitted.width
        );
        assert!(ARTWORK_FIT_SIZES.contains(&(fitted.width as u32)));
    }

    fn target(player_id: u32, observed_at: Instant) -> Target {
        Target {
            owner: format!(":1.{player_id}"),
            aliases: HashSet::new(),
            player: empty_player(player_id),
            track_path: None,
            last_playing: 0,
            last_active: 0,
            last_artwork: 0,
            position_observed_at: observed_at,
        }
    }

    #[test]
    fn playing_position_advances_from_its_observation_anchor_and_clamps() {
        let now = Instant::now();
        let mut target = target(1, now - Duration::from_secs(2));
        target.player.playback_status = PlaybackStatus::Playing;
        target.player.position_us = 1_000_000;
        target.player.rate_ppm = 2_000_000;
        target.player.length_us = 4_000_000;

        let player = player_at(&target, now);
        assert_eq!(player.position_us, 4_000_000);
        assert_eq!(target.player.position_us, 1_000_000);
    }

    #[test]
    fn normalized_track_identity_changes_increment_the_track_revision() {
        let now = Instant::now();
        let mut target = target(1, now);
        target.player.title = "old title".into();
        target.track_path = Some(OwnedObjectPath::try_from("/track/1").unwrap());
        let mut player = target.player.clone();
        player.title = "new title".into();
        let snapshot = Snapshot {
            player,
            art_url: String::new(),
            track_path: target.track_path.clone(),
            position_observed_at: now,
        };

        assert!(track_changed(&target, &snapshot));
    }

    #[test]
    fn active_fallback_uses_the_lowest_id_when_nothing_was_active() {
        let now = Instant::now();
        let mut state = State::default();
        state.players.insert(7, target(7, now));
        state.players.insert(3, target(3, now));

        assert_eq!(state.choose_active(), Some(3));
    }

    #[test]
    fn deferred_player_becomes_eligible_when_capacity_opens() {
        let now = Instant::now();
        let mut state = State::default();
        state.players.insert(1, target(1, now));
        state.by_owner.insert(":1.1".into(), 1);
        state.deferred.insert(
            "org.mpris.MediaPlayer2.deferred".into(),
            DeferredTarget {
                owner: ":1.2".into(),
                retry_at: None,
            },
        );

        assert_eq!(state.deferred_candidate(1, now), None);
        state.remove_player(1);
        assert_eq!(
            state.deferred_candidate(1, now),
            Some(("org.mpris.MediaPlayer2.deferred".into(), ":1.2".into()))
        );
    }

    #[test]
    fn failed_monitor_retries_only_after_its_cooldown() {
        let now = Instant::now();
        let retry_at = now + MONITOR_RETRY_DELAY;
        let alias = "org.mpris.MediaPlayer2.retry".to_string();
        let mut state = State::default();
        let mut player = target(1, now);
        player.aliases.insert(alias.clone());
        state.players.insert(1, player);
        state.by_owner.insert(":1.1".into(), 1);
        state.by_alias.insert(alias.clone(), 1);

        let records = state.remove_player_for_retry(1, retry_at);
        assert!(matches!(
            records.first(),
            Some(MprisRecord::Delete { player_id: 1 })
        ));
        assert_eq!(state.deferred_candidate(1, now), None);
        assert_eq!(
            state.deferred_candidate(1, retry_at),
            Some((alias, ":1.1".into()))
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn property_calls_are_strictly_limited_to_four() {
        let limiter = Arc::new(Semaphore::new(PROPERTY_CALL_LIMIT));
        let active = Arc::new(AtomicUsize::new(0));
        let maximum = Arc::new(AtomicUsize::new(0));
        let mut tasks = Vec::new();
        for _ in 0..12 {
            let limiter = limiter.clone();
            let active = active.clone();
            let maximum = maximum.clone();
            tasks.push(tokio::spawn(async move {
                bounded_property_call(&limiter, async {
                    let current = active.fetch_add(1, Ordering::SeqCst) + 1;
                    maximum.fetch_max(current, Ordering::SeqCst);
                    tokio::time::sleep(Duration::from_millis(20)).await;
                    active.fetch_sub(1, Ordering::SeqCst);
                })
                .await;
            }));
        }
        for task in tasks {
            task.await.unwrap();
        }

        assert_eq!(maximum.load(Ordering::SeqCst), PROPERTY_CALL_LIMIT);
        assert_eq!(limiter.available_permits(), PROPERTY_CALL_LIMIT);
    }

    fn action_job(player_id: u32, nonce: u32) -> ActionJob {
        ActionJob {
            requester: u64::from(nonce),
            action: MprisAction {
                nonce,
                player_id,
                kind: MprisActionKind::Play,
                track_revision: 0,
                value: 0,
            },
        }
    }

    #[test]
    fn action_queue_preserves_per_player_order() {
        let mut queues = ActionQueues::default();
        assert!(matches!(queues.enqueue(action_job(7, 1)), Ok(true)));
        assert!(matches!(queues.enqueue(action_job(7, 2)), Ok(false)));
        assert!(matches!(queues.enqueue(action_job(8, 3)), Ok(true)));

        assert_eq!(queues.take_next(7).unwrap().action.nonce, 1);
        queues.complete_one();
        assert_eq!(queues.take_next(7).unwrap().action.nonce, 2);
        queues.complete_one();
        assert_eq!(queues.take_next(8).unwrap().action.nonce, 3);
        queues.complete_one();
        assert_eq!(queues.pending, 0);
    }

    #[test]
    fn action_execution_gate_has_exactly_four_slots() {
        let slots = Arc::new(Semaphore::new(ACTION_CALL_LIMIT));
        let permits = (0..ACTION_CALL_LIMIT)
            .map(|_| slots.clone().try_acquire_owned().unwrap())
            .collect::<Vec<_>>();
        assert!(slots.clone().try_acquire_owned().is_err());
        drop(permits);
        assert!(slots.try_acquire_owned().is_ok());
    }
}
