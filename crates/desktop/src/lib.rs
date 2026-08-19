//! Desktop services hosted on a compositor-scoped private D-Bus session.
//!
//! D-Bus values are normalized here. The server receives bounded state
//! events and sends semantic commands; it never handles arbitrary variants,
//! bus names, object paths, or remote image bytes.

use blit_remote::desktop::{
    MENU_NODE_CHECKMARK, MENU_NODE_ENABLED, MENU_NODE_RADIO, MENU_NODE_SEPARATOR,
    MENU_NODE_SUBMENU, MENU_NODE_VISIBLE, MenuNode, NOTIFICATION_CLOSED_BY_CALLER,
    NOTIFICATION_CLOSED_DISMISSED, NOTIFICATION_CLOSED_EXPIRED, NOTIFICATION_CLOSED_UNDEFINED,
    NOTIFICATION_EVENT_ACTION, NOTIFICATION_EVENT_DEFAULT, NOTIFICATION_EVENT_DISMISS,
    NOTIFICATION_RESIDENT, NOTIFICATION_TRANSIENT, NOTIFICATION_URGENCY_CRITICAL,
    NOTIFICATION_URGENCY_NORMAL, Notification, NotificationAction, NotificationEvent,
    NotificationRecord, PngImage, TRAY_CATEGORY_APPLICATION_STATUS, TRAY_CATEGORY_COMMUNICATIONS,
    TRAY_CATEGORY_HARDWARE, TRAY_CATEGORY_SYSTEM_SERVICE, TRAY_CATEGORY_UNKNOWN,
    TRAY_EVENT_ACTIVATE, TRAY_EVENT_MENU_ITEM, TRAY_EVENT_OPEN_MENU, TRAY_EVENT_SCROLL,
    TRAY_EVENT_SCROLL_HORIZONTAL, TRAY_EVENT_SECONDARY_ACTIVATE, TRAY_HAS_MENU, TRAY_ITEM_IS_MENU,
    TRAY_MENU_NONE, TRAY_MENU_OK, TRAY_MENU_STALE, TRAY_MENU_UNAVAILABLE, TRAY_STATUS_ACTIVE,
    TRAY_STATUS_NEEDS_ATTENTION, TRAY_STATUS_PASSIVE, TrayEvent, TrayItem, TrayMenu, TrayRecord,
};
use futures_util::StreamExt;
use image::imageops::FilterType;
use image::{DynamicImage, GenericImageView, ImageBuffer, Rgba, RgbaImage};
use std::collections::{HashMap, HashSet, VecDeque};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::Mutex as StdMutex;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};
use tokio::sync::{Mutex, Notify, mpsc};
use zbus::fdo::DBusProxy;
use zbus::message::Header;
use zbus::names::{BusName, OwnedBusName, OwnedInterfaceName, OwnedUniqueName};
use zbus::object_server::SignalEmitter;
use zbus::proxy::{Builder as ProxyBuilder, CacheProperties};
use zbus::zvariant::{OwnedObjectPath, OwnedValue};
use zbus::{Connection, Proxy, fdo, interface};

mod mpris;
mod portal;

/// Loopback HTTP origin and image fixtures, shared by the unit tests in
/// `mpris` and the full-bridge tests below. Lives at the crate root so both
/// module-private test modules can reach it.
#[cfg(test)]
mod test_http {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// Serves one canned response per connection and keeps accepting, so a
    /// caching test can prove a second fetch never reached the wire: the hit
    /// counter, not a closed port, is what the assertions read.
    ///
    /// `declared_len` overrides Content-Length so a body can overrun what it
    /// advertised, which is the only way to exercise the streamed size cap
    /// rather than the header precheck.
    pub(crate) fn serve(
        body: Vec<u8>,
        status: &str,
        declared_len: Option<usize>,
    ) -> (String, Arc<AtomicUsize>) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind loopback");
        let port = listener.local_addr().expect("local addr").port();
        let hits = Arc::new(AtomicUsize::new(0));
        let counter = hits.clone();
        let status = status.to_string();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { return };
                counter.fetch_add(1, Ordering::SeqCst);
                // Drain the request head so the client sees a well-formed
                // exchange rather than a reset peer.
                let mut request = [0u8; 1024];
                let _ = std::io::Read::read(&mut stream, &mut request);
                let length = declared_len.unwrap_or(body.len());
                let header = format!(
                    "HTTP/1.1 {status}\r\nContent-Type: image/jpeg\r\nContent-Length: {length}\r\nConnection: close\r\n\r\n"
                );
                let _ = std::io::Write::write_all(&mut stream, header.as_bytes());
                let _ = std::io::Write::write_all(&mut stream, &body);
                let _ = std::io::Write::flush(&mut stream);
            }
        });
        (format!("http://127.0.0.1:{port}/cover"), hits)
    }

    /// A cover whose PNG re-encode does not fit the MPRIS artwork cap at full
    /// size. Near-incompressible by construction, standing in for unusually
    /// detailed art; measured at ~768 KiB once re-encoded to 512×512.
    pub(crate) fn incompressible_cover(size: u32) -> Vec<u8> {
        let mut buf = image::RgbImage::new(size, size);
        let mut seed = 12_345u32;
        for y in 0..size {
            for x in 0..size {
                seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                buf.put_pixel(
                    x,
                    y,
                    image::Rgb([
                        (seed >> 16) as u8,
                        (seed >> 8) as u8,
                        seed.rotate_left(x % 13) as u8,
                    ]),
                );
            }
        }
        // PNG, not JPEG: a JPEG round trip would smooth away exactly the detail
        // this fixture exists to preserve.
        let mut png = std::io::Cursor::new(Vec::new());
        image::DynamicImage::ImageRgb8(buf)
            .write_to(&mut png, image::ImageFormat::Png)
            .expect("encode png");
        png.into_inner()
    }

    /// A JPEG of the requested size, matching how catalogues actually publish
    /// covers: photographic, lossy, and larger than the icon ceiling.
    pub(crate) fn cover_jpeg(width: u32, height: u32) -> Vec<u8> {
        let image = image::DynamicImage::ImageRgb8(image::RgbImage::from_pixel(
            width,
            height,
            image::Rgb([12, 40, 120]),
        ));
        let mut jpeg = std::io::Cursor::new(Vec::new());
        image
            .write_to(&mut jpeg, image::ImageFormat::Jpeg)
            .expect("encode jpeg");
        jpeg.into_inner()
    }
}

const NOTIFICATION_PATH: &str = "/org/freedesktop/Notifications";
const WATCHER_PATH: &str = "/StatusNotifierWatcher";
const EVENT_CAPACITY: usize = 512;
const COMMAND_CAPACITY: usize = 128;
const MAX_TRAY_ITEMS: usize = 128;
const MAX_NOTIFICATIONS: usize = 256;
const MAX_ACTIONS: usize = 32;
const MAX_STRING_BYTES: usize = 64 * 1024;
const MAX_MENU_NODES: usize = 2_048;
const MAX_MENU_DEPTH: usize = 16;
const MAX_SOURCE_IMAGE_BYTES: usize = 4 * 1024 * 1024;
const MAX_SOURCE_IMAGE_DIMENSION: u32 = 512;
/// Source ceiling for cover art, which is photographic and arrives at whatever
/// size a catalogue publishes: Spotify serves 640×640, and 1000–2000 px is
/// common elsewhere. An *icon* that large means something is wrong, so icons stay
/// at `MAX_SOURCE_IMAGE_DIMENSION`; for a cover it is ordinary, and refusing it
/// drops the art altogether where downscaling was the whole intent.
const MAX_ARTWORK_SOURCE_DIMENSION: u32 = 2048;
/// Decode allowance that matches the ceiling above, so a legal cover is not
/// rejected by an allocation limit tuned for icons. Transient: the decoded
/// surface is downscaled to the caller's target and dropped immediately.
const MAX_ARTWORK_DECODE_BYTES: u64 =
    (MAX_ARTWORK_SOURCE_DIMENSION as u64) * (MAX_ARTWORK_SOURCE_DIMENSION as u64) * 4;
const MAX_FINAL_PNG_BYTES: usize = 1024 * 1024;
const TRAY_ICON_SIZE: u32 = 64;
const NOTIFICATION_IMAGE_SIZE: u32 = 512;
const NOTIFICATION_RATE_BURST: f64 = 20.0;
const NOTIFICATION_RATE_REFILL: f64 = 2.0;
const NOTIFICATION_REPLACE_COST: f64 = 0.25;
const DBUS_CALL_TIMEOUT: Duration = Duration::from_secs(2);

type StatusNotifierPixmap = (i32, i32, Vec<u8>);
type StatusNotifierTooltip = (String, Vec<StatusNotifierPixmap>, String, String);
type DBusMenuLayout = (i32, HashMap<String, OwnedValue>, Vec<OwnedValue>);
type NotificationImageData = (i32, i32, i32, bool, i32, i32, Vec<u8>);

#[derive(Clone, Debug, Hash, PartialEq, Eq)]
struct ImageCacheKey {
    path: PathBuf,
    modified_ns: u128,
    target: u32,
    source_max: u32,
}

#[derive(Clone)]
struct ImageResolver {
    theme: Arc<str>,
    roots: Arc<Vec<PathBuf>>,
    cache: Arc<StdMutex<HashMap<ImageCacheKey, PngImage>>>,
}

impl ImageResolver {
    fn new() -> Self {
        let mut roots = Vec::new();
        if let Some(home) = std::env::var_os("XDG_DATA_HOME") {
            roots.push(PathBuf::from(home).join("icons"));
        } else if let Some(home) = std::env::var_os("HOME") {
            roots.push(PathBuf::from(home).join(".local/share/icons"));
        }
        for root in std::env::var_os("XDG_DATA_DIRS")
            .unwrap_or_else(|| "/usr/local/share:/usr/share".into())
            .to_string_lossy()
            .split(':')
            .filter(|root| !root.is_empty())
        {
            roots.push(PathBuf::from(root).join("icons"));
        }
        roots.sort();
        roots.dedup();
        Self {
            theme: std::env::var("BLIT_ICON_THEME")
                .unwrap_or_else(|_| "hicolor".into())
                .into(),
            roots: Arc::new(roots),
            cache: Arc::new(StdMutex::new(HashMap::new())),
        }
    }

    async fn resolve(
        &self,
        source: String,
        item_theme_path: Option<PathBuf>,
        target: u32,
        source_max: u32,
    ) -> Option<PngImage> {
        let resolver = self.clone();
        tokio::task::spawn_blocking(move || {
            resolver.resolve_blocking(&source, item_theme_path.as_deref(), target, source_max)
        })
        .await
        .ok()
        .flatten()
    }

    async fn encoded(&self, bytes: Vec<u8>, target: u32, source_max: u32) -> Option<PngImage> {
        tokio::task::spawn_blocking(move || normalize_encoded_image(&bytes, target, source_max))
            .await
            .ok()
            .flatten()
    }

    async fn pixels(&self, data: NotificationImageData, target: u32) -> Option<PngImage> {
        tokio::task::spawn_blocking(move || normalize_notification_pixels(data, target))
            .await
            .ok()
            .flatten()
    }

    async fn pixmaps(&self, pixmaps: Vec<StatusNotifierPixmap>, target: u32) -> Option<PngImage> {
        tokio::task::spawn_blocking(move || best_pixmap_png(&pixmaps, target))
            .await
            .ok()
            .flatten()
    }

    async fn overlay(&self, base: PngImage, overlay: PngImage) -> Option<PngImage> {
        tokio::task::spawn_blocking(move || composite_overlay(&base, &overlay))
            .await
            .ok()
            .flatten()
    }

    fn resolve_blocking(
        &self,
        source: &str,
        item_theme_path: Option<&Path>,
        target: u32,
        source_max: u32,
    ) -> Option<PngImage> {
        if source.is_empty() || target == 0 || target > MAX_SOURCE_IMAGE_DIMENSION {
            return None;
        }
        let path = if source.starts_with('/') {
            PathBuf::from(source)
        } else if let Some(path) = source.strip_prefix("file://") {
            if !path.starts_with('/') || path.contains('%') {
                return None;
            }
            PathBuf::from(path)
        } else {
            find_theme_icon(source, target, &self.theme, &self.roots, item_theme_path)?
        };
        let path = path.canonicalize().ok()?;
        let metadata = fs::metadata(&path).ok()?;
        if !metadata.is_file() || metadata.len() > MAX_SOURCE_IMAGE_BYTES as u64 {
            return None;
        }
        let modified_ns = metadata
            .modified()
            .ok()
            .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
            .map_or(0, |duration| duration.as_nanos());
        let key = ImageCacheKey {
            path: path.clone(),
            modified_ns,
            target,
            source_max,
        };
        if let Some(image) = self.cache.lock().ok()?.get(&key).cloned() {
            return Some(image);
        }
        let bytes = fs::read(path).ok()?;
        let image = normalize_encoded_image(&bytes, target, source_max)?;
        if let Ok(mut cache) = self.cache.lock() {
            if cache.len() >= 512 {
                cache.clear();
            }
            cache.insert(key, image.clone());
        }
        Some(image)
    }
}

#[derive(Clone, Debug)]
pub enum Event {
    Tray(TrayRecord),
    TrayMenu(TrayMenu),
    Notification(NotificationRecord),
    Mpris(Vec<blit_remote::media::MprisRecord>),
    MprisAction {
        requester: u64,
        result: blit_remote::media::MprisActionResult,
    },
    Portal {
        request: blit_remote::media::PortalRequest,
        parent_window: String,
    },
    PortalCancel(u32),
    PortalSessionClosed(u32),
}

#[derive(Clone, Debug)]
pub struct PortalStream {
    pub surface_id: u16,
    pub node_id: u32,
    pub pipewire_serial: u64,
    pub width: u16,
    pub height: u16,
}

#[derive(Clone, Debug)]
pub enum Command {
    Tray(TrayEvent),
    Notification(NotificationEvent),
    Mpris {
        requester: u64,
        action: blit_remote::media::MprisAction,
    },
    PortalReply(blit_remote::media::PortalReply),
    PortalScreenCastStarted {
        request_id: u32,
        session_id: u32,
        streams: Vec<PortalStream>,
    },
    PortalSessionClosed(u32),
}

#[derive(Clone, Copy, Debug)]
pub struct Config {
    pub default_timeout: Duration,
    pub minimum_timeout: Duration,
    pub maximum_timeout: Duration,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            default_timeout: Duration::from_secs(10),
            minimum_timeout: Duration::from_secs(1),
            maximum_timeout: Duration::from_secs(24 * 60 * 60),
        }
    }
}

pub struct Bridge {
    _connection: Connection,
    events: Arc<EventPipe>,
    commands: mpsc::Sender<Command>,
}

#[derive(Default)]
struct EventQueue {
    queued: VecDeque<Event>,
}

impl EventQueue {
    fn coalesce_key(event: &Event) -> Option<(u8, u32)> {
        match event {
            Event::Tray(TrayRecord::Upsert(item)) => Some((0, item.tray_id)),
            Event::TrayMenu(menu) => Some((1, menu.tray_id)),
            Event::Notification(NotificationRecord::Upsert(item)) => {
                Some((2, item.notification_id))
            }
            Event::Tray(TrayRecord::Delete { .. })
            | Event::Notification(NotificationRecord::Delete { .. })
            | Event::Mpris(_)
            | Event::MprisAction { .. }
            | Event::Portal { .. }
            | Event::PortalCancel(_)
            | Event::PortalSessionClosed(_) => None,
        }
    }

    fn push(&mut self, event: Event) -> Option<Event> {
        if let Some(key) = Self::coalesce_key(&event)
            && let Some(existing) = self
                .queued
                .iter_mut()
                .rev()
                .take_while(|queued| match (key.0, queued) {
                    (0, Event::Tray(TrayRecord::Delete { tray_id })) => *tray_id != key.1,
                    (
                        2,
                        Event::Notification(NotificationRecord::Delete {
                            notification_id, ..
                        }),
                    ) => *notification_id != key.1,
                    _ => true,
                })
                .find(|queued| Self::coalesce_key(queued) == Some(key))
        {
            *existing = event;
            return None;
        }
        if self.queued.len() >= EVENT_CAPACITY {
            return Some(event);
        }
        self.queued.push_back(event);
        None
    }
}

#[derive(Default)]
struct EventPipe {
    queue: StdMutex<EventQueue>,
    space: Notify,
}

impl Bridge {
    pub async fn start(
        address: &str,
        config: Config,
        notify: Arc<dyn Fn() + Send + Sync>,
    ) -> Result<Self, String> {
        let events = Arc::new(EventPipe::default());
        let (commands, command_rx) = mpsc::channel(COMMAND_CAPACITY);
        let notification_state = Arc::new(Mutex::new(NotificationState::new(config)));
        let watcher_state = Arc::new(Mutex::new(WatcherState::default()));
        let mpris_state = Arc::new(Mutex::new(mpris::State::default()));
        let portal_state = Arc::new(Mutex::new(portal::State::default()));
        let common = Common {
            events: events.clone(),
            notify,
            images: ImageResolver::new(),
        };

        let notification_service = NotificationService {
            state: notification_state.clone(),
            common: common.clone(),
        };
        let kde_watcher = KdeWatcher {
            state: watcher_state.clone(),
            common: common.clone(),
        };
        let freedesktop_watcher = FreedesktopWatcher {
            state: watcher_state.clone(),
            common: common.clone(),
        };
        let portal_service = portal::AccessService {
            state: portal_state.clone(),
            common: common.clone(),
            timeout: std::env::var("BLIT_PORTAL_ACCESS_TIMEOUT_MS")
                .ok()
                .and_then(|value| value.parse::<u64>().ok())
                .filter(|value| *value > 0)
                .map(|value| value.min(60_000))
                .map(Duration::from_millis)
                .unwrap_or(Duration::from_secs(60)),
        };
        let screencast_service = portal::ScreenCastService {
            state: portal_state.clone(),
            common: common.clone(),
            timeout: std::env::var("BLIT_PORTAL_SCREENCAST_TIMEOUT_MS")
                .ok()
                .and_then(|value| value.parse::<u64>().ok())
                .filter(|value| *value > 0)
                .map(|value| value.min(120_000))
                .map(Duration::from_millis)
                .unwrap_or(Duration::from_secs(120)),
        };
        let host_name = format!(
            "org.freedesktop.StatusNotifierHost.blit.p{}",
            std::process::id()
        );
        let connection = zbus::connection::Builder::address(address)
            .map_err(|e| e.to_string())?
            .name("org.freedesktop.Notifications")
            .map_err(|e| e.to_string())?
            .name("org.kde.StatusNotifierWatcher")
            .map_err(|e| e.to_string())?
            .name("org.freedesktop.StatusNotifierWatcher")
            .map_err(|e| e.to_string())?
            .name(host_name)
            .map_err(|e| e.to_string())?
            .serve_at(NOTIFICATION_PATH, notification_service)
            .map_err(|e| e.to_string())?
            .serve_at(WATCHER_PATH, kde_watcher)
            .map_err(|e| e.to_string())?
            .serve_at(WATCHER_PATH, freedesktop_watcher)
            .map_err(|e| e.to_string())?
            .name("org.freedesktop.impl.portal.desktop.blit")
            .map_err(|e| e.to_string())?
            .serve_at("/org/freedesktop/portal/desktop", portal_service)
            .map_err(|e| e.to_string())?
            .serve_at("/org/freedesktop/portal/desktop", screencast_service)
            .map_err(|e| e.to_string())?
            .build()
            .await
            .map_err(|e| format!("connect desktop services to private bus: {e}"))?;

        let notification_emitter = SignalEmitter::new(&connection, NOTIFICATION_PATH)
            .map_err(|e| e.to_string())?
            .into_owned();
        let mpris_actions =
            mpris::ActionDispatcher::new(connection.clone(), mpris_state.clone(), common.clone());
        tokio::spawn(command_loop(
            command_rx,
            connection.clone(),
            notification_emitter,
            notification_state,
            watcher_state.clone(),
            mpris_actions,
            portal_state,
            common.clone(),
        ));
        tokio::spawn(watch_owner_changes(
            connection.clone(),
            watcher_state,
            common.clone(),
        ));
        if std::env::var("BLIT_MPRIS").map_or(true, |value| value != "0") {
            tokio::spawn(mpris::watch(connection.clone(), mpris_state, common));
        }

        Ok(Self {
            _connection: connection,
            events,
            commands,
        })
    }

    pub fn try_recv(&mut self) -> Option<Event> {
        let event = self.events.queue.lock().ok()?.queued.pop_front();
        if event.is_some() {
            self.events.space.notify_one();
        }
        event
    }

    pub fn try_command(&self, command: Command) -> bool {
        self.commands.try_send(command).is_ok()
    }
}

#[derive(Clone)]
struct Common {
    events: Arc<EventPipe>,
    notify: Arc<dyn Fn() + Send + Sync>,
    images: ImageResolver,
}

impl Common {
    async fn send(&self, event: Event) -> fdo::Result<()> {
        let mut event = Some(event);
        loop {
            let space = self.events.space.notified();
            let pushed = self
                .events
                .queue
                .lock()
                .map_err(|_| fdo::Error::Failed("desktop event queue is poisoned".into()))?
                .push(event.take().expect("event retained while waiting"));
            match pushed {
                None => break,
                Some(retained) => event = Some(retained),
            }
            space.await;
        }
        (self.notify)();
        Ok(())
    }
}

#[derive(Clone)]
struct StoredNotification {
    item: Notification,
    resident: bool,
    owner: String,
}

struct NotificationState {
    config: Config,
    next_id: u32,
    next_revision: u32,
    active: HashMap<u32, StoredNotification>,
    order: VecDeque<u32>,
    rate: HashMap<String, RateBucket>,
}

struct RateBucket {
    tokens: f64,
    updated: Instant,
}

impl NotificationState {
    fn new(config: Config) -> Self {
        Self {
            config,
            next_id: 1,
            next_revision: 1,
            active: HashMap::new(),
            order: VecDeque::new(),
            rate: HashMap::new(),
        }
    }

    fn allocate_id(&mut self) -> u32 {
        loop {
            let id = self.next_id.max(1);
            self.next_id = self.next_id.wrapping_add(1).max(1);
            if !self.active.contains_key(&id) {
                return id;
            }
        }
    }

    fn revision(&mut self) -> u32 {
        let revision = self.next_revision.max(1);
        self.next_revision = self.next_revision.wrapping_add(1).max(1);
        revision
    }

    fn remove(&mut self, id: u32, revision: u32) -> Option<StoredNotification> {
        if self
            .active
            .get(&id)
            .is_none_or(|stored| stored.item.revision != revision)
        {
            return None;
        }
        self.order.retain(|candidate| *candidate != id);
        self.active.remove(&id)
    }

    fn effective_timeout(&self, requested_ms: i32, urgency: u8) -> Duration {
        if requested_ms == 0 || (requested_ms < 0 && urgency == NOTIFICATION_URGENCY_CRITICAL) {
            return Duration::ZERO;
        }
        if requested_ms < 0 {
            return self.config.default_timeout;
        }
        Duration::from_millis(requested_ms as u64)
            .clamp(self.config.minimum_timeout, self.config.maximum_timeout)
    }

    fn charge(&mut self, owner: &str, replacement: bool) -> bool {
        let now = Instant::now();
        if self.rate.len() >= 1_024 && !self.rate.contains_key(owner) {
            self.rate
                .retain(|_, bucket| now.duration_since(bucket.updated) < Duration::from_secs(300));
            if self.rate.len() >= 1_024 {
                return false;
            }
        }
        let bucket = self.rate.entry(owner.to_string()).or_insert(RateBucket {
            tokens: NOTIFICATION_RATE_BURST,
            updated: now,
        });
        bucket.tokens = (bucket.tokens
            + now.duration_since(bucket.updated).as_secs_f64() * NOTIFICATION_RATE_REFILL)
            .min(NOTIFICATION_RATE_BURST);
        bucket.updated = now;
        let cost = if replacement {
            NOTIFICATION_REPLACE_COST
        } else {
            1.0
        };
        if bucket.tokens < cost {
            return false;
        }
        bucket.tokens -= cost;
        true
    }
}

struct NotificationService {
    state: Arc<Mutex<NotificationState>>,
    common: Common,
}

#[interface(name = "org.freedesktop.Notifications")]
impl NotificationService {
    async fn get_capabilities(&self) -> Vec<&str> {
        vec!["actions", "body", "icon-static"]
    }

    #[allow(clippy::too_many_arguments)]
    async fn notify(
        &self,
        app_name: &str,
        replaces_id: u32,
        app_icon: &str,
        summary: &str,
        body: &str,
        actions: Vec<String>,
        hints: HashMap<String, OwnedValue>,
        expire_timeout: i32,
        #[zbus(header)] header: Header<'_>,
        #[zbus(signal_emitter)] emitter: SignalEmitter<'_>,
    ) -> fdo::Result<u32> {
        let owner = header
            .sender()
            .map(ToString::to_string)
            .ok_or_else(|| fdo::Error::Failed("notification call has no sender".into()))?;
        let urgency = hints
            .get("urgency")
            .and_then(|value| u8::try_from(value).ok())
            .unwrap_or(NOTIFICATION_URGENCY_NORMAL)
            .min(NOTIFICATION_URGENCY_CRITICAL);
        let resident = hint_bool(&hints, "resident");
        let transient = hint_bool(&hints, "transient");
        let desktop_entry = hints
            .get("desktop-entry")
            .and_then(|value| <&str>::try_from(value).ok())
            .unwrap_or_default();
        {
            let mut state = self.state.lock().await;
            let replacement = replaces_id != 0
                && state
                    .active
                    .get(&replaces_id)
                    .is_some_and(|stored| stored.owner == owner);
            if !state.charge(&owner, replacement) {
                return Err(fdo::Error::LimitsExceeded(
                    "notification rate limit exceeded".into(),
                ));
            }
        }
        let normalized_actions = actions
            .chunks_exact(2)
            .take(MAX_ACTIONS)
            .map(|pair| NotificationAction {
                key: clip_text(&pair[0], 4096),
                label: strip_markup(&pair[1], 4096),
            })
            .collect::<Vec<_>>();
        let icon = self
            .common
            .images
            .resolve(
                app_icon.to_string(),
                None,
                TRAY_ICON_SIZE,
                MAX_SOURCE_IMAGE_DIMENSION,
            )
            .await
            .unwrap_or_default();
        let image = if let Some(data) = notification_image_data(&hints, "image-data") {
            self.common
                .images
                .pixels(data, NOTIFICATION_IMAGE_SIZE)
                .await
        } else if let Some(path) = hint_string(&hints, "image-path") {
            self.common
                .images
                .resolve(
                    path.to_string(),
                    None,
                    NOTIFICATION_IMAGE_SIZE,
                    MAX_SOURCE_IMAGE_DIMENSION,
                )
                .await
        } else if let Some(data) = notification_image_data(&hints, "icon_data") {
            self.common
                .images
                .pixels(data, NOTIFICATION_IMAGE_SIZE)
                .await
        } else {
            None
        }
        .unwrap_or_default();

        let (item, replaced, evicted, timeout) = {
            let mut state = self.state.lock().await;
            let replaced = (replaces_id != 0
                && state
                    .active
                    .get(&replaces_id)
                    .is_some_and(|stored| stored.owner == owner))
            .then_some(replaces_id);
            let id = replaced.unwrap_or_else(|| state.allocate_id());
            let revision = state.revision();
            let timeout = state.effective_timeout(expire_timeout, urgency);
            let flags = (u8::from(resident) * NOTIFICATION_RESIDENT)
                | (u8::from(transient) * NOTIFICATION_TRANSIENT);
            let item = Notification {
                notification_id: id,
                revision,
                urgency,
                flags,
                timeout_ms: timeout.as_millis().min(u32::MAX as u128) as u32,
                app_name: strip_markup(app_name, 4096),
                desktop_entry: clip_text(desktop_entry, 4096),
                summary: strip_markup(summary, 4096),
                body: strip_markup(body, MAX_STRING_BYTES),
                icon,
                image,
                actions: normalized_actions,
            };
            if replaced.is_some() {
                state.order.retain(|candidate| *candidate != id);
            }
            let evicted = if state.active.len() >= MAX_NOTIFICATIONS && replaced.is_none() {
                state
                    .order
                    .iter()
                    .copied()
                    .find(|candidate| {
                        state.active.get(candidate).is_some_and(|stored| {
                            stored.item.urgency != NOTIFICATION_URGENCY_CRITICAL
                        })
                    })
                    .and_then(|candidate| {
                        let old = state.active.remove(&candidate)?;
                        state.order.retain(|id| *id != candidate);
                        Some(old.item)
                    })
            } else {
                None
            };
            if state.active.len() >= MAX_NOTIFICATIONS && replaced.is_none() {
                return Err(fdo::Error::LimitsExceeded(
                    "all active notifications are critical".into(),
                ));
            }
            state.order.push_back(id);
            state.active.insert(
                id,
                StoredNotification {
                    item: item.clone(),
                    resident,
                    owner,
                },
            );
            (item, replaced, evicted, timeout)
        };

        if let Some(old) = evicted {
            self.common
                .send(Event::Notification(NotificationRecord::Delete {
                    notification_id: old.notification_id,
                    revision: old.revision,
                    reason: NOTIFICATION_CLOSED_UNDEFINED,
                }))
                .await?;
            Self::notification_closed(&emitter, old.notification_id, 4)
                .await
                .map_err(fdo::Error::from)?;
        }
        let _ = replaced;
        self.common
            .send(Event::Notification(NotificationRecord::Upsert(
                item.clone(),
            )))
            .await?;
        if !timeout.is_zero() {
            let state = self.state.clone();
            let common = self.common.clone();
            let emitter = emitter.to_owned();
            let id = item.notification_id;
            let revision = item.revision;
            tokio::spawn(async move {
                tokio::time::sleep(timeout).await;
                let removed = state.lock().await.remove(id, revision);
                if removed.is_some() {
                    let _ = common
                        .send(Event::Notification(NotificationRecord::Delete {
                            notification_id: id,
                            revision,
                            reason: NOTIFICATION_CLOSED_EXPIRED,
                        }))
                        .await;
                    let _ = NotificationService::notification_closed(&emitter, id, 1).await;
                }
            });
        }
        Ok(item.notification_id)
    }

    async fn close_notification(
        &self,
        id: u32,
        #[zbus(header)] header: Header<'_>,
        #[zbus(signal_emitter)] emitter: SignalEmitter<'_>,
    ) -> fdo::Result<()> {
        let owner = header
            .sender()
            .map(ToString::to_string)
            .ok_or_else(|| fdo::Error::Failed("notification call has no sender".into()))?;
        let removed = {
            let mut state = self.state.lock().await;
            let Some(revision) = state
                .active
                .get(&id)
                .filter(|stored| stored.owner == owner)
                .map(|stored| stored.item.revision)
            else {
                return Err(fdo::Error::InvalidArgs("unknown notification id".into()));
            };
            state.remove(id, revision).map(|stored| stored.item)
        };
        if let Some(item) = removed {
            self.common
                .send(Event::Notification(NotificationRecord::Delete {
                    notification_id: id,
                    revision: item.revision,
                    reason: NOTIFICATION_CLOSED_BY_CALLER,
                }))
                .await?;
            Self::notification_closed(&emitter, id, 3)
                .await
                .map_err(fdo::Error::from)?;
        }
        Ok(())
    }

    #[zbus(out_args("name", "vendor", "version", "spec_version"))]
    async fn get_server_information(&self) -> (&str, &str, &str, &str) {
        ("blit", "Indent", env!("CARGO_PKG_VERSION"), "1.3")
    }

    #[zbus(signal)]
    async fn notification_closed(
        emitter: &SignalEmitter<'_>,
        id: u32,
        reason: u32,
    ) -> zbus::Result<()>;

    #[zbus(signal)]
    async fn action_invoked(
        emitter: &SignalEmitter<'_>,
        id: u32,
        action_key: &str,
    ) -> zbus::Result<()>;
}

fn hint_bool(hints: &HashMap<String, OwnedValue>, name: &str) -> bool {
    hints
        .get(name)
        .and_then(|value| bool::try_from(value).ok())
        .unwrap_or(false)
}

fn hint_string<'a>(hints: &'a HashMap<String, OwnedValue>, name: &str) -> Option<&'a str> {
    hints
        .get(name)
        .and_then(|value| <&str>::try_from(value).ok())
}

fn notification_image_data(
    hints: &HashMap<String, OwnedValue>,
    name: &str,
) -> Option<NotificationImageData> {
    hints.get(name)?.try_clone().ok()?.try_into().ok()
}

fn clip_text(value: &str, max: usize) -> String {
    if value.len() <= max {
        return value.to_string();
    }
    let mut end = max;
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    value[..end].to_string()
}

/// Notification markup is a tiny XML-ish subset. Unknown tags are stripped,
/// `<br>` becomes a newline, and the five XML entities plus numeric entities
/// are decoded. Malformed markup remains harmless plain text.
pub fn strip_markup(value: &str, max: usize) -> String {
    let mut out = String::with_capacity(value.len().min(max));
    let mut chars = value.char_indices().peekable();
    while let Some((_, ch)) = chars.next() {
        if out.len() >= max {
            break;
        }
        if ch == '<' {
            let mut tag = String::new();
            let mut closed = false;
            for (_, next) in chars.by_ref() {
                if next == '>' {
                    closed = true;
                    break;
                }
                if tag.len() < 32 {
                    tag.push(next);
                }
            }
            if closed && tag.trim().trim_end_matches('/').eq_ignore_ascii_case("br") {
                out.push('\n');
            }
            continue;
        }
        if ch == '&' {
            let mut entity = String::new();
            let mut closed = false;
            while let Some(&(_, next)) = chars.peek() {
                chars.next();
                if next == ';' {
                    closed = true;
                    break;
                }
                if entity.len() >= 12 {
                    break;
                }
                entity.push(next);
            }
            if closed {
                let decoded = match entity.as_str() {
                    "amp" => Some('&'),
                    "lt" => Some('<'),
                    "gt" => Some('>'),
                    "quot" => Some('"'),
                    "apos" => Some('\''),
                    _ if entity.starts_with("#x") => u32::from_str_radix(&entity[2..], 16)
                        .ok()
                        .and_then(char::from_u32),
                    _ if entity.starts_with('#') => {
                        entity[1..].parse::<u32>().ok().and_then(char::from_u32)
                    }
                    _ => None,
                };
                if let Some(decoded) = decoded {
                    out.push(decoded);
                    continue;
                }
                out.push('&');
                out.push_str(&entity);
                out.push(';');
                continue;
            }
            out.push('&');
            out.push_str(&entity);
            continue;
        }
        out.push(ch);
    }
    clip_text(&out, max)
}

#[derive(Clone, Debug, Hash, PartialEq, Eq)]
struct ItemKey {
    owner: OwnedUniqueName,
    path: OwnedObjectPath,
}

#[derive(Clone, Debug)]
struct ItemTarget {
    tray_id: u32,
    revision: u32,
    interface: String,
    flags: u8,
    menu_path: Option<OwnedObjectPath>,
    menu_revision: u32,
    next_menu_revision: u32,
    menu_items: HashMap<i32, u16>,
    monitored_menu_path: Option<OwnedObjectPath>,
    failures: u8,
}

#[derive(Default)]
struct WatcherState {
    next_id: AtomicU32,
    items: HashMap<ItemKey, ItemTarget>,
    registered: Vec<String>,
}

impl WatcherState {
    fn id(&self) -> u32 {
        self.next_id
            .fetch_add(1, Ordering::Relaxed)
            .wrapping_add(1)
            .max(1)
    }
}

struct KdeWatcher {
    state: Arc<Mutex<WatcherState>>,
    common: Common,
}

struct FreedesktopWatcher {
    state: Arc<Mutex<WatcherState>>,
    common: Common,
}

macro_rules! watcher_interface {
    ($ty:ident, $name:literal) => {
        #[interface(name = $name)]
        impl $ty {
            async fn register_status_notifier_item(
                &self,
                service: &str,
                #[zbus(header)] header: Header<'_>,
                #[zbus(connection)] connection: &Connection,
                #[zbus(signal_emitter)] emitter: SignalEmitter<'_>,
            ) -> fdo::Result<()> {
                register_item(
                    &self.state,
                    &self.common,
                    service,
                    &header,
                    connection,
                    &emitter,
                )
                .await
            }

            async fn register_status_notifier_host(
                &self,
                _service: &str,
                #[zbus(signal_emitter)] emitter: SignalEmitter<'_>,
            ) -> fdo::Result<()> {
                KdeWatcher::status_notifier_host_registered(&emitter)
                    .await
                    .map_err(fdo::Error::from)?;
                FreedesktopWatcher::status_notifier_host_registered(&emitter)
                    .await
                    .map_err(fdo::Error::from)
            }

            #[zbus(property)]
            async fn registered_status_notifier_items(&self) -> Vec<String> {
                self.state.lock().await.registered.clone()
            }

            #[zbus(property)]
            async fn is_status_notifier_host_registered(&self) -> bool {
                true
            }

            #[zbus(property)]
            async fn protocol_version(&self) -> i32 {
                0
            }

            #[zbus(signal)]
            async fn status_notifier_item_registered(
                emitter: &SignalEmitter<'_>,
                service: &str,
            ) -> zbus::Result<()>;

            #[zbus(signal)]
            async fn status_notifier_item_unregistered(
                emitter: &SignalEmitter<'_>,
                service: &str,
            ) -> zbus::Result<()>;

            #[zbus(signal)]
            async fn status_notifier_host_registered(
                emitter: &SignalEmitter<'_>,
            ) -> zbus::Result<()>;
        }
    };
}

watcher_interface!(KdeWatcher, "org.kde.StatusNotifierWatcher");
watcher_interface!(FreedesktopWatcher, "org.freedesktop.StatusNotifierWatcher");

async fn register_item(
    state: &Arc<Mutex<WatcherState>>,
    common: &Common,
    service: &str,
    header: &Header<'_>,
    connection: &Connection,
    emitter: &SignalEmitter<'_>,
) -> fdo::Result<()> {
    let (owner, path) = if service.starts_with('/') {
        let owner = header
            .sender()
            .and_then(|sender| OwnedUniqueName::try_from(sender.as_str()).ok())
            .ok_or_else(|| fdo::Error::InvalidArgs("registration has no sender".into()))?;
        let path = OwnedObjectPath::try_from(service)
            .map_err(|_| fdo::Error::InvalidArgs("invalid item object path".into()))?;
        (owner, path)
    } else {
        let name = BusName::try_from(service)
            .map_err(|_| fdo::Error::InvalidArgs("invalid item bus name".into()))?;
        let owner = DBusProxy::new(connection)
            .await
            .map_err(fdo::Error::from)?
            .get_name_owner(name)
            .await?;
        (
            owner,
            OwnedObjectPath::try_from("/StatusNotifierItem").unwrap(),
        )
    };
    let key = ItemKey { owner, path };
    let registered_name = format!("{}{}", key.owner, key.path);
    {
        let mut watcher = state.lock().await;
        if watcher.items.contains_key(&key) {
            return Ok(());
        }
        if watcher.items.len() >= MAX_TRAY_ITEMS {
            return Err(fdo::Error::LimitsExceeded("too many tray items".into()));
        }
        let tray_id = watcher.id();
        watcher.items.insert(
            key.clone(),
            ItemTarget {
                tray_id,
                revision: 0,
                interface: "org.kde.StatusNotifierItem".into(),
                flags: 0,
                menu_path: None,
                menu_revision: 0,
                next_menu_revision: 0,
                menu_items: HashMap::new(),
                monitored_menu_path: None,
                failures: 0,
            },
        );
        watcher.registered.push(registered_name.clone());
    }
    KdeWatcher::status_notifier_item_registered(emitter, &registered_name)
        .await
        .map_err(fdo::Error::from)?;
    FreedesktopWatcher::status_notifier_item_registered(emitter, &registered_name)
        .await
        .map_err(fdo::Error::from)?;

    let connection = connection.clone();
    let state = state.clone();
    let common = common.clone();
    tokio::spawn(async move {
        monitor_item(connection, state, common, key).await;
    });
    Ok(())
}

async fn item_proxy(
    connection: &Connection,
    key: &ItemKey,
    interface: &str,
) -> zbus::Result<Proxy<'static>> {
    let interface = OwnedInterfaceName::try_from(interface.to_string())?;
    // Never cache properties. zbus refreshes a property cache from
    // `PropertiesChanged` alone, and StatusNotifierItem does not use it: an
    // application announces a repainted icon, a new status, or a new tooltip
    // with the interface's own `NewIcon`/`NewStatus`/`NewToolTip` signal.
    // Chromium — so every Electron tray — emits only those, so a cached proxy
    // answers every re-read with the snapshot taken when the item registered
    // and the icon freezes with whatever badge it was wearing.
    ProxyBuilder::new(connection)
        .destination(OwnedBusName::from(BusName::from(key.owner.clone())))?
        .path(key.path.clone())?
        .interface(interface)?
        .cache_properties(CacheProperties::No)
        .build()
        .await
}

async fn read_item(
    kde: &Proxy<'_>,
    freedesktop: &Proxy<'_>,
    tray_id: u32,
    revision: u32,
    images: &ImageResolver,
) -> zbus::Result<(TrayItem, String, Option<OwnedObjectPath>)> {
    let (proxy, interface) = if kde.get_property::<String>("Id").await.is_ok() {
        (kde, "org.kde.StatusNotifierItem")
    } else {
        (freedesktop, "org.freedesktop.StatusNotifierItem")
    };
    let app_id = proxy.get_property::<String>("Id").await?;
    let status = proxy.get_property::<String>("Status").await?;
    let title = proxy
        .get_property::<String>("Title")
        .await
        .unwrap_or_default();
    let category = proxy
        .get_property::<String>("Category")
        .await
        .unwrap_or_default();
    let item_is_menu = proxy
        .get_property::<bool>("ItemIsMenu")
        .await
        .unwrap_or(false);
    let menu_path = proxy
        .get_property::<OwnedObjectPath>("Menu")
        .await
        .ok()
        .filter(|path| path.as_str() != "/");
    let tooltip = proxy
        .get_property::<StatusNotifierTooltip>("ToolTip")
        .await
        .unwrap_or_default();
    let attention = status == "NeedsAttention";
    let theme_path = proxy
        .get_property::<String>("IconThemePath")
        .await
        .ok()
        .filter(|path| path.starts_with('/'))
        .map(PathBuf::from);
    let icon_name_property = if attention {
        "AttentionIconName"
    } else {
        "IconName"
    };
    let icon_pixmap_property = if attention {
        "AttentionIconPixmap"
    } else {
        "IconPixmap"
    };
    let icon_name = proxy
        .get_property::<String>(icon_name_property)
        .await
        .unwrap_or_default();
    let pixmaps = proxy
        .get_property::<Vec<StatusNotifierPixmap>>(icon_pixmap_property)
        .await
        .unwrap_or_default();
    let mut icon = images
        .resolve(
            icon_name,
            theme_path.clone(),
            TRAY_ICON_SIZE,
            MAX_SOURCE_IMAGE_DIMENSION,
        )
        .await
        .or(images.pixmaps(pixmaps, TRAY_ICON_SIZE).await);
    if icon.is_none() && attention {
        let normal_name = proxy
            .get_property::<String>("IconName")
            .await
            .unwrap_or_default();
        let normal_pixmaps = proxy
            .get_property::<Vec<StatusNotifierPixmap>>("IconPixmap")
            .await
            .unwrap_or_default();
        icon = images
            .resolve(
                normal_name,
                theme_path.clone(),
                TRAY_ICON_SIZE,
                MAX_SOURCE_IMAGE_DIMENSION,
            )
            .await
            .or(images.pixmaps(normal_pixmaps, TRAY_ICON_SIZE).await);
    }
    let overlay_name = proxy
        .get_property::<String>("OverlayIconName")
        .await
        .unwrap_or_default();
    let overlay_pixmaps = proxy
        .get_property::<Vec<StatusNotifierPixmap>>("OverlayIconPixmap")
        .await
        .unwrap_or_default();
    let overlay = images
        .resolve(
            overlay_name,
            theme_path,
            TRAY_ICON_SIZE / 2,
            MAX_SOURCE_IMAGE_DIMENSION,
        )
        .await
        .or(images.pixmaps(overlay_pixmaps, TRAY_ICON_SIZE / 2).await);
    if let (Some(base), Some(overlay)) = (icon.as_ref(), overlay) {
        icon = images.overlay(base.clone(), overlay).await.or(icon);
    }
    let icon = icon.unwrap_or_default();
    Ok((
        TrayItem {
            tray_id,
            revision,
            status: match status.as_str() {
                "Passive" => TRAY_STATUS_PASSIVE,
                "NeedsAttention" => TRAY_STATUS_NEEDS_ATTENTION,
                _ => TRAY_STATUS_ACTIVE,
            },
            category: match category.as_str() {
                "ApplicationStatus" => TRAY_CATEGORY_APPLICATION_STATUS,
                "Communications" => TRAY_CATEGORY_COMMUNICATIONS,
                "SystemServices" => TRAY_CATEGORY_SYSTEM_SERVICE,
                "Hardware" => TRAY_CATEGORY_HARDWARE,
                _ => TRAY_CATEGORY_UNKNOWN,
            },
            flags: (u8::from(menu_path.is_some()) * TRAY_HAS_MENU)
                | (u8::from(item_is_menu) * TRAY_ITEM_IS_MENU),
            app_id: clip_text(&app_id, 4096),
            title: strip_markup(&title, 4096),
            tooltip_title: strip_markup(&tooltip.2, 4096),
            tooltip_body: strip_markup(&tooltip.3, MAX_STRING_BYTES),
            icon,
        },
        interface.into(),
        menu_path,
    ))
}

async fn monitor_item(
    connection: Connection,
    state: Arc<Mutex<WatcherState>>,
    common: Common,
    key: ItemKey,
) {
    let tray_id = {
        let watcher = state.lock().await;
        let Some(target) = watcher.items.get(&key) else {
            return;
        };
        target.tray_id
    };
    // Install every match rule before the initial property snapshot. A notifier
    // is allowed to change an icon through an interface signal rather than
    // PropertiesChanged; subscribing after read_item would lose a change in
    // that gap and leave the stale snapshot visible until another signal.
    let Ok(kde) = item_proxy(&connection, &key, "org.kde.StatusNotifierItem").await else {
        return;
    };
    let Ok(freedesktop) = item_proxy(&connection, &key, "org.freedesktop.StatusNotifierItem").await
    else {
        return;
    };
    let Ok(properties) = item_proxy(&connection, &key, "org.freedesktop.DBus.Properties").await
    else {
        return;
    };
    let Ok(mut kde_signals) = kde.receive_all_signals().await else {
        return;
    };
    let Ok(mut freedesktop_signals) = freedesktop.receive_all_signals().await else {
        return;
    };
    let Ok(mut property_signals) = properties.receive_signal("PropertiesChanged").await else {
        return;
    };
    loop {
        let revision = {
            let mut watcher = state.lock().await;
            let Some(target) = watcher.items.get_mut(&key) else {
                return;
            };
            target.revision = target.revision.wrapping_add(1).max(1);
            target.revision
        };
        let read = tokio::time::timeout(
            DBUS_CALL_TIMEOUT,
            read_item(&kde, &freedesktop, tray_id, revision, &common.images),
        )
        .await;
        let interface = match read {
            Ok(Ok((item, used_interface, menu_path))) => {
                let monitor_path = {
                    let mut watcher = state.lock().await;
                    let Some(target) = watcher.items.get_mut(&key) else {
                        return;
                    };
                    target.failures = 0;
                    target.interface = used_interface.clone();
                    target.flags = item.flags;
                    if target.menu_path != menu_path {
                        target.menu_path = menu_path.clone();
                        target.menu_revision = 0;
                        target.menu_items.clear();
                    }
                    if target.monitored_menu_path != menu_path {
                        target.monitored_menu_path = menu_path.clone();
                        menu_path.clone()
                    } else {
                        None
                    }
                };
                if let Some(path) = monitor_path {
                    tokio::spawn(monitor_menu(
                        connection.clone(),
                        state.clone(),
                        key.clone(),
                        path,
                    ));
                }
                let _ = common.send(Event::Tray(TrayRecord::Upsert(item))).await;
                used_interface
            }
            Ok(Err(_)) | Err(_) => {
                let remove = {
                    let mut watcher = state.lock().await;
                    let Some(target) = watcher.items.get_mut(&key) else {
                        return;
                    };
                    target.failures = target.failures.saturating_add(1);
                    target.failures >= 3
                };
                if remove {
                    remove_item(&connection, &state, &common, &key).await;
                    return;
                }
                tokio::time::sleep(Duration::from_millis(250)).await;
                continue;
            }
        };
        if interface == "org.kde.StatusNotifierItem" {
            tokio::select! {
                signal = kde_signals.next() => if signal.is_none() { return; },
                signal = property_signals.next() => if signal.is_none() { return; },
            }
        } else {
            tokio::select! {
                signal = freedesktop_signals.next() => if signal.is_none() { return; },
                signal = property_signals.next() => if signal.is_none() { return; },
            }
        }
    }
}

async fn monitor_menu(
    connection: Connection,
    state: Arc<Mutex<WatcherState>>,
    key: ItemKey,
    path: OwnedObjectPath,
) {
    let menu_key = ItemKey {
        owner: key.owner.clone(),
        path: path.clone(),
    };
    let Ok(proxy) = item_proxy(&connection, &menu_key, "com.canonical.dbusmenu").await else {
        return;
    };
    let Ok(mut layouts) = proxy.receive_signal("LayoutUpdated").await else {
        return;
    };
    let Ok(mut properties) = proxy.receive_signal("ItemsPropertiesUpdated").await else {
        return;
    };
    loop {
        // A layout change can renumber or repurpose every id, so nothing the
        // client is holding can be trusted afterwards. A property change
        // cannot: it repaints named items and leaves the rest of the menu
        // exactly as the user is reading it. Apps repaint constantly — Zoom
        // re-syncs its language checkmark on every AboutToShow — and voiding
        // the whole menu for that dropped the click the user was in the middle
        // of making, with no way to tell them why.
        let change = tokio::select! {
            signal = layouts.next() => match signal {
                Some(_) => MenuChange::Layout,
                None => return,
            },
            signal = properties.next() => match signal {
                Some(signal) => repainted_menu_items(&signal),
                None => return,
            },
        };
        let mut watcher = state.lock().await;
        let Some(target) = watcher.items.get_mut(&key) else {
            return;
        };
        if target.menu_path.as_ref() != Some(&path) {
            return;
        }
        match change {
            MenuChange::Layout => {
                target.menu_revision = 0;
                target.menu_items.clear();
            }
            // The repainted items are the ones the client's copy now
            // misdescribes, so only those stop being clickable; the menu keeps
            // its revision and every other item stays live.
            MenuChange::Repainted(ids) => target.menu_items.retain(|id, _| !ids.contains(id)),
        }
    }
}

enum MenuChange {
    /// The menu was restructured: every id the client holds may now name a
    /// different item, so none of them can be acted on.
    Layout,
    /// These items were repainted; the rest of the menu is still what the client
    /// is showing.
    Repainted(HashSet<i32>),
}

/// Which items an `ItemsPropertiesUpdated` signal repaints. An unreadable body
/// has to be treated as if it changed everything.
fn repainted_menu_items(signal: &zbus::Message) -> MenuChange {
    type PropertiesUpdated = (
        Vec<(i32, HashMap<String, OwnedValue>)>,
        Vec<(i32, Vec<String>)>,
    );
    let Ok((updated, removed)) = signal.body().deserialize::<PropertiesUpdated>() else {
        return MenuChange::Layout;
    };
    MenuChange::Repainted(
        updated
            .into_iter()
            .map(|(id, _)| id)
            .chain(removed.into_iter().map(|(id, _)| id))
            .collect(),
    )
}

fn menu_string<'a>(properties: &'a HashMap<String, OwnedValue>, name: &str) -> Option<&'a str> {
    properties
        .get(name)
        .and_then(|value| <&str>::try_from(value).ok())
}

fn menu_bool(properties: &HashMap<String, OwnedValue>, name: &str, default: bool) -> bool {
    properties
        .get(name)
        .and_then(|value| bool::try_from(value).ok())
        .unwrap_or(default)
}

fn menu_i32(properties: &HashMap<String, OwnedValue>, name: &str, default: i32) -> i32 {
    properties
        .get(name)
        .and_then(|value| i32::try_from(value).ok())
        .unwrap_or(default)
}

fn menu_label(value: &str) -> String {
    let mut label = String::with_capacity(value.len());
    let mut chars = value.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '_' {
            if chars.peek() == Some(&'_') {
                chars.next();
                label.push('_');
            }
        } else {
            label.push(ch);
        }
    }
    strip_markup(&label, 4096)
}

struct MenuIconSource {
    node: usize,
    name: String,
    data: Vec<u8>,
}

type NormalizedMenu = (Vec<MenuNode>, HashMap<i32, u16>, Vec<MenuIconSource>);

fn normalize_menu_layout(root: DBusMenuLayout) -> Option<NormalizedMenu> {
    fn append(
        children: Vec<OwnedValue>,
        parent_id: i32,
        depth: usize,
        nodes: &mut Vec<MenuNode>,
        items: &mut HashMap<i32, u16>,
        icons: &mut Vec<MenuIconSource>,
        seen: &mut HashSet<i32>,
    ) -> Option<()> {
        if depth > MAX_MENU_DEPTH {
            return None;
        }
        for (position, child) in children.into_iter().enumerate() {
            if nodes.len() >= MAX_MENU_NODES || position > u16::MAX as usize {
                return None;
            }
            let (id, properties, grandchildren): DBusMenuLayout = child.try_into().ok()?;
            if !seen.insert(id) {
                return None;
            }
            let kind = menu_string(&properties, "type").unwrap_or("standard");
            if kind != "standard" && kind != "separator" {
                continue;
            }
            let mut flags = 0;
            if menu_bool(&properties, "visible", true) {
                flags |= MENU_NODE_VISIBLE;
            }
            if menu_bool(&properties, "enabled", true) {
                flags |= MENU_NODE_ENABLED;
            }
            if kind == "separator" {
                flags |= MENU_NODE_SEPARATOR;
            }
            if !grandchildren.is_empty()
                || menu_string(&properties, "children-display") == Some("submenu")
            {
                flags |= MENU_NODE_SUBMENU;
            }
            match menu_string(&properties, "toggle-type") {
                Some("checkmark") => flags |= MENU_NODE_CHECKMARK,
                Some("radio") => flags |= MENU_NODE_RADIO,
                _ => {}
            }
            let toggle_state = menu_i32(&properties, "toggle-state", -1).clamp(-1, 1) as i8;
            let label = if kind == "separator" {
                String::new()
            } else {
                menu_label(menu_string(&properties, "label").unwrap_or_default())
            };
            let icon_name = menu_string(&properties, "icon-name")
                .unwrap_or_default()
                .to_string();
            let icon_data = properties
                .get("icon-data")
                .and_then(|value| value.try_clone().ok())
                .and_then(|value| Vec::<u8>::try_from(value).ok())
                .unwrap_or_default();
            let node = nodes.len();
            nodes.push(MenuNode {
                id,
                parent_id,
                position: position as u16,
                flags,
                toggle_state,
                label,
                icon: PngImage::default(),
            });
            if !icon_name.is_empty() || !icon_data.is_empty() {
                icons.push(MenuIconSource {
                    node,
                    name: icon_name,
                    data: icon_data,
                });
            }
            items.insert(id, flags);
            append(grandchildren, id, depth + 1, nodes, items, icons, seen)?;
        }
        Some(())
    }

    let (root_id, _, children) = root;
    let mut nodes = Vec::new();
    let mut items = HashMap::new();
    let mut icons = Vec::new();
    let mut seen = HashSet::from([root_id]);
    append(
        children, root_id, 1, &mut nodes, &mut items, &mut icons, &mut seen,
    )?;
    Some((nodes, items, icons))
}

async fn menu_status(
    state: &Arc<Mutex<WatcherState>>,
    tray_id: u32,
    status: u8,
) -> Option<TrayMenu> {
    let watcher = state.lock().await;
    let target = watcher
        .items
        .values()
        .find(|target| target.tray_id == tray_id)?;
    Some(TrayMenu {
        tray_id,
        tray_revision: target.revision,
        menu_revision: target.menu_revision,
        status,
        nodes: Vec::new(),
    })
}

async fn refresh_menu(
    connection: &Connection,
    state: &Arc<Mutex<WatcherState>>,
    tray_id: u32,
    parent_id: i32,
    images: &ImageResolver,
) -> TrayMenu {
    let target = {
        let watcher = state.lock().await;
        watcher
            .items
            .iter()
            .find(|(_, target)| target.tray_id == tray_id)
            .map(|(key, target)| (key.clone(), target.clone()))
    };
    let Some((key, target)) = target else {
        return TrayMenu {
            tray_id,
            tray_revision: 0,
            menu_revision: 0,
            status: TRAY_MENU_UNAVAILABLE,
            nodes: Vec::new(),
        };
    };
    let Some(path) = target.menu_path else {
        return TrayMenu {
            tray_id,
            tray_revision: target.revision,
            menu_revision: target.menu_revision,
            status: TRAY_MENU_NONE,
            nodes: Vec::new(),
        };
    };
    let menu_key = ItemKey {
        owner: key.owner.clone(),
        path: path.clone(),
    };
    let result = tokio::time::timeout(DBUS_CALL_TIMEOUT, async {
        let proxy = item_proxy(connection, &menu_key, "com.canonical.dbusmenu").await?;
        let _: bool = proxy
            .call("AboutToShow", &(parent_id,))
            .await
            .unwrap_or(false);
        let properties = vec![
            "type".to_string(),
            "label".to_string(),
            "enabled".to_string(),
            "visible".to_string(),
            "children-display".to_string(),
            "toggle-type".to_string(),
            "toggle-state".to_string(),
            "icon-name".to_string(),
            "icon-data".to_string(),
        ];
        let (_source_revision, layout): (u32, DBusMenuLayout) =
            proxy.call("GetLayout", &(0i32, -1i32, properties)).await?;
        normalize_menu_layout(layout).ok_or(zbus::Error::Failure("invalid DBusMenu layout".into()))
    })
    .await;
    let Ok(Ok((mut nodes, items, icons))) = result else {
        return TrayMenu {
            tray_id,
            tray_revision: target.revision,
            menu_revision: target.menu_revision,
            status: TRAY_MENU_UNAVAILABLE,
            nodes: Vec::new(),
        };
    };
    for source in icons {
        let icon = if source.data.is_empty() {
            None
        } else {
            images
                .encoded(source.data, TRAY_ICON_SIZE, MAX_SOURCE_IMAGE_DIMENSION)
                .await
        }
        .or(images
            .resolve(
                source.name,
                None,
                TRAY_ICON_SIZE,
                MAX_SOURCE_IMAGE_DIMENSION,
            )
            .await);
        if let Some(icon) = icon
            && let Some(node) = nodes.get_mut(source.node)
        {
            node.icon = icon;
        }
    }
    let mut watcher = state.lock().await;
    let Some(live) = watcher.items.get_mut(&key) else {
        return TrayMenu {
            tray_id,
            tray_revision: target.revision,
            menu_revision: 0,
            status: TRAY_MENU_UNAVAILABLE,
            nodes: Vec::new(),
        };
    };
    if live.menu_path.as_ref() != Some(&path) {
        return TrayMenu {
            tray_id,
            tray_revision: live.revision,
            menu_revision: live.menu_revision,
            status: TRAY_MENU_STALE,
            nodes: Vec::new(),
        };
    }
    live.next_menu_revision = live.next_menu_revision.wrapping_add(1).max(1);
    live.menu_revision = live.next_menu_revision;
    live.menu_items = items;
    TrayMenu {
        tray_id,
        tray_revision: live.revision,
        menu_revision: live.menu_revision,
        status: TRAY_MENU_OK,
        nodes,
    }
}

async fn remove_item(
    connection: &Connection,
    state: &Arc<Mutex<WatcherState>>,
    common: &Common,
    key: &ItemKey,
) {
    let removed = {
        let mut watcher = state.lock().await;
        let removed = watcher.items.remove(key);
        if removed.is_some() {
            let name = format!("{}{}", key.owner, key.path);
            watcher.registered.retain(|candidate| candidate != &name);
        }
        removed
    };
    if let Some(target) = removed {
        let _ = common
            .send(Event::Tray(TrayRecord::Delete {
                tray_id: target.tray_id,
            }))
            .await;
        if let Ok(emitter) = SignalEmitter::new(connection, WATCHER_PATH) {
            let name = format!("{}{}", key.owner, key.path);
            let _ = KdeWatcher::status_notifier_item_unregistered(&emitter, &name).await;
            let _ = FreedesktopWatcher::status_notifier_item_unregistered(&emitter, &name).await;
        }
    }
}

async fn watch_owner_changes(
    connection: Connection,
    state: Arc<Mutex<WatcherState>>,
    common: Common,
) {
    let Ok(proxy) = DBusProxy::new(&connection).await else {
        return;
    };
    let Ok(mut changes) = proxy.receive_name_owner_changed().await else {
        return;
    };
    while let Some(signal) = changes.next().await {
        let Ok(args) = signal.args() else {
            continue;
        };
        if args.new_owner().is_none() {
            let departed = args.old_owner().as_ref().map(ToString::to_string);
            let keys = {
                let watcher = state.lock().await;
                watcher
                    .items
                    .keys()
                    .filter(|key| departed.as_deref() == Some(key.owner.as_str()))
                    .cloned()
                    .collect::<Vec<_>>()
            };
            for key in keys {
                remove_item(&connection, &state, &common, &key).await;
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn command_loop(
    mut commands: mpsc::Receiver<Command>,
    connection: Connection,
    notification_emitter: SignalEmitter<'static>,
    notification_state: Arc<Mutex<NotificationState>>,
    watcher_state: Arc<Mutex<WatcherState>>,
    mpris_actions: mpris::ActionDispatcher,
    portal_state: Arc<Mutex<portal::State>>,
    common: Common,
) {
    while let Some(command) = commands.recv().await {
        match command {
            Command::Notification(event) => {
                handle_notification_command(
                    event,
                    &notification_emitter,
                    &notification_state,
                    &common,
                )
                .await;
            }
            Command::Tray(event) => {
                handle_tray_command(event, &connection, &watcher_state, &common).await;
            }
            Command::Mpris { requester, action } => {
                mpris_actions.dispatch(requester, action);
            }
            Command::PortalReply(reply) => {
                portal_state.lock().await.complete_reply(reply);
            }
            Command::PortalScreenCastStarted {
                request_id,
                session_id,
                streams,
            } => {
                let delivery = portal_state
                    .lock()
                    .await
                    .complete_screencast(request_id, session_id, streams);
                match delivery {
                    portal::ScreenCastDelivery::Accepted => {}
                    portal::ScreenCastDelivery::UnknownRequest => {
                        let _ = common.send(Event::PortalSessionClosed(session_id)).await;
                    }
                    portal::ScreenCastDelivery::ReceiverDropped => {
                        let _ = common.send(Event::PortalSessionClosed(session_id)).await;
                        portal::close_server_session(&connection, &portal_state, session_id).await;
                    }
                }
            }
            Command::PortalSessionClosed(session_id) => {
                portal::close_server_session(&connection, &portal_state, session_id).await;
            }
        }
    }
}

async fn handle_notification_command(
    event: NotificationEvent,
    emitter: &SignalEmitter<'_>,
    state: &Arc<Mutex<NotificationState>>,
    common: &Common,
) {
    if event.kind == NOTIFICATION_EVENT_DISMISS {
        let removed = state
            .lock()
            .await
            .remove(event.notification_id, event.revision)
            .map(|stored| stored.item);
        if let Some(item) = removed {
            let _ = common
                .send(Event::Notification(NotificationRecord::Delete {
                    notification_id: item.notification_id,
                    revision: item.revision,
                    reason: NOTIFICATION_CLOSED_DISMISSED,
                }))
                .await;
            let _ =
                NotificationService::notification_closed(emitter, item.notification_id, 2).await;
        }
        return;
    }

    let action = {
        let state = state.lock().await;
        let Some(stored) = state.active.get(&event.notification_id) else {
            return;
        };
        if stored.item.revision != event.revision {
            return;
        }
        let key = if event.kind == NOTIFICATION_EVENT_DEFAULT {
            "default"
        } else if event.kind == NOTIFICATION_EVENT_ACTION {
            &event.key
        } else {
            return;
        };
        if !stored.item.actions.iter().any(|action| action.key == key) {
            return;
        }
        (key.to_string(), stored.resident)
    };
    let _ = NotificationService::action_invoked(emitter, event.notification_id, &action.0).await;
    if !action.1 {
        let removed = state
            .lock()
            .await
            .remove(event.notification_id, event.revision)
            .map(|stored| stored.item);
        if let Some(item) = removed {
            let _ = common
                .send(Event::Notification(NotificationRecord::Delete {
                    notification_id: item.notification_id,
                    revision: item.revision,
                    reason: NOTIFICATION_CLOSED_DISMISSED,
                }))
                .await;
            let _ =
                NotificationService::notification_closed(emitter, item.notification_id, 2).await;
        }
    }
}

async fn handle_tray_command(
    event: TrayEvent,
    connection: &Connection,
    state: &Arc<Mutex<WatcherState>>,
    common: &Common,
) {
    let target = {
        let watcher = state.lock().await;
        watcher
            .items
            .iter()
            .find(|(_, target)| target.tray_id == event.tray_id)
            .map(|(key, target)| (key.clone(), target.clone()))
    };
    let Some((key, target)) = target else {
        return;
    };
    let Ok(proxy) = item_proxy(connection, &key, &target.interface).await else {
        return;
    };
    match event.kind {
        TRAY_EVENT_ACTIVATE if target.flags & TRAY_ITEM_IS_MENU == 0 => {
            let _: zbus::Result<()> = proxy.call("Activate", &(0i32, 0i32)).await;
        }
        TRAY_EVENT_ACTIVATE | TRAY_EVENT_OPEN_MENU if target.menu_path.is_some() => {
            let menu = refresh_menu(
                connection,
                state,
                event.tray_id,
                event.value,
                &common.images,
            )
            .await;
            let _ = common.send(Event::TrayMenu(menu)).await;
        }
        TRAY_EVENT_ACTIVATE | TRAY_EVENT_OPEN_MENU => {
            let _: zbus::Result<()> = proxy.call("ContextMenu", &(0i32, 0i32)).await;
        }
        TRAY_EVENT_SECONDARY_ACTIVATE => {
            let _: zbus::Result<()> = proxy.call("SecondaryActivate", &(0i32, 0i32)).await;
        }
        TRAY_EVENT_SCROLL => {
            let orientation = if event.flags & TRAY_EVENT_SCROLL_HORIZONTAL != 0 {
                "horizontal"
            } else {
                "vertical"
            };
            let _: zbus::Result<()> = proxy.call("Scroll", &(event.value, orientation)).await;
        }
        TRAY_EVENT_MENU_ITEM => {
            let clickable = target.menu_revision != 0
                && target.menu_revision == event.menu_revision
                && target.menu_items.get(&event.value).is_some_and(|flags| {
                    flags & (MENU_NODE_VISIBLE | MENU_NODE_ENABLED)
                        == MENU_NODE_VISIBLE | MENU_NODE_ENABLED
                        && flags & MENU_NODE_SEPARATOR == 0
                });
            if !clickable {
                if let Some(stale) = menu_status(state, event.tray_id, TRAY_MENU_STALE).await {
                    let _ = common.send(Event::TrayMenu(stale)).await;
                }
                let menu = refresh_menu(connection, state, event.tray_id, 0, &common.images).await;
                let _ = common.send(Event::TrayMenu(menu)).await;
                return;
            }
            let Some(path) = target.menu_path else {
                return;
            };
            let menu_key = ItemKey {
                owner: key.owner,
                path,
            };
            if let Ok(menu_proxy) =
                item_proxy(connection, &menu_key, "com.canonical.dbusmenu").await
            {
                static STARTED: OnceLock<Instant> = OnceLock::new();
                let timestamp = STARTED
                    .get_or_init(Instant::now)
                    .elapsed()
                    .as_millis()
                    .min(u32::MAX as u128) as u32;
                let data = OwnedValue::from(0u32);
                let _: zbus::Result<()> = menu_proxy
                    .call("Event", &(event.value, "clicked", data, timestamp))
                    .await;
                let menu = refresh_menu(connection, state, event.tray_id, 0, &common.images).await;
                let _ = common.send(Event::TrayMenu(menu)).await;
            }
        }
        _ => {}
    }
}

#[derive(Clone, Copy)]
struct ThemeDirectory {
    size: u32,
    min_size: u32,
    max_size: u32,
    threshold: u32,
    scalable: bool,
}

fn valid_icon_name(name: &str) -> Option<&str> {
    let name = name
        .strip_suffix(".png")
        .or_else(|| name.strip_suffix(".svg"))
        .or_else(|| name.strip_suffix(".xpm"))
        .unwrap_or(name);
    (!name.is_empty()
        && name.len() <= 255
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.')))
    .then_some(name)
}

fn parse_theme_index(path: &Path) -> (Vec<String>, Vec<String>, HashMap<String, ThemeDirectory>) {
    let Ok(text) = fs::read_to_string(path.join("index.theme")) else {
        return (Vec::new(), Vec::new(), HashMap::new());
    };
    let mut section = String::new();
    let mut values = HashMap::<String, HashMap<String, String>>::new();
    for raw in text.lines() {
        let line = raw.trim();
        if line.starts_with('[') && line.ends_with(']') {
            section = line[1..line.len() - 1].to_string();
        } else if !line.starts_with('#')
            && let Some((key, value)) = line.split_once('=')
        {
            values
                .entry(section.clone())
                .or_default()
                .insert(key.trim().into(), value.trim().into());
        }
    }
    let root = values.get("Icon Theme");
    let directories = root
        .and_then(|root| root.get("Directories"))
        .into_iter()
        .flat_map(|value| value.split(','))
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
        .collect::<Vec<_>>();
    let inherits = root
        .and_then(|root| root.get("Inherits"))
        .into_iter()
        .flat_map(|value| value.split(','))
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
        .collect::<Vec<_>>();
    let mut metadata = HashMap::new();
    for directory in &directories {
        let Some(section) = values.get(directory) else {
            continue;
        };
        let size = section
            .get("Size")
            .and_then(|value| value.parse().ok())
            .unwrap_or(0);
        if size == 0 {
            continue;
        }
        let scalable = section.get("Type").is_some_and(|value| value == "Scalable");
        metadata.insert(
            directory.clone(),
            ThemeDirectory {
                size,
                min_size: section
                    .get("MinSize")
                    .and_then(|value| value.parse().ok())
                    .unwrap_or(size),
                max_size: section
                    .get("MaxSize")
                    .and_then(|value| value.parse().ok())
                    .unwrap_or(size),
                threshold: section
                    .get("Threshold")
                    .and_then(|value| value.parse().ok())
                    .unwrap_or(2),
                scalable,
            },
        );
    }
    (directories, inherits, metadata)
}

fn theme_distance(directory: ThemeDirectory, target: u32) -> u32 {
    let (min, max) = if directory.scalable {
        (directory.min_size, directory.max_size)
    } else {
        (
            directory.size.saturating_sub(directory.threshold),
            directory.size.saturating_add(directory.threshold),
        )
    };
    if target < min {
        min - target
    } else {
        target.saturating_sub(max)
    }
}

fn theme_candidate(theme_path: &Path, name: &str, target: u32) -> Option<PathBuf> {
    let (directories, _, metadata) = parse_theme_index(theme_path);
    let mut candidates = Vec::new();
    for (position, directory) in directories.iter().enumerate() {
        let Some(info) = metadata.get(directory).copied() else {
            continue;
        };
        for (format, format_rank) in [("png", 0u8), ("svg", 1), ("xpm", 2)] {
            let path = theme_path.join(directory).join(format!("{name}.{format}"));
            if path.is_file() {
                candidates.push((theme_distance(info, target), format_rank, position, path));
            }
        }
    }
    candidates.sort_by_key(|candidate| (candidate.0, candidate.1, candidate.2));
    candidates.into_iter().next().map(|candidate| candidate.3)
}

fn recursive_icon_candidate(root: &Path, name: &str, target: u32) -> Option<PathBuf> {
    fn visit(
        directory: &Path,
        name: &str,
        target: u32,
        depth: usize,
        visited: &mut usize,
        candidates: &mut Vec<(u32, u8, PathBuf)>,
    ) {
        if depth > 4 || *visited >= 4_096 {
            return;
        }
        let Ok(entries) = fs::read_dir(directory) else {
            return;
        };
        for entry in entries.flatten() {
            *visited += 1;
            if *visited >= 4_096 {
                return;
            }
            let path = entry.path();
            if path.is_dir() {
                visit(&path, name, target, depth + 1, visited, candidates);
                continue;
            }
            let Some(stem) = path.file_stem().and_then(|value| value.to_str()) else {
                continue;
            };
            if stem != name {
                continue;
            }
            let rank = match path.extension().and_then(|value| value.to_str()) {
                Some("png") => 0,
                Some("svg") => 1,
                Some("xpm") => 2,
                _ => continue,
            };
            let directory_size = path
                .parent()
                .and_then(Path::file_name)
                .and_then(|value| value.to_str())
                .and_then(|value| value.split('x').next())
                .and_then(|value| value.parse::<u32>().ok())
                .unwrap_or(target);
            candidates.push((directory_size.abs_diff(target), rank, path));
        }
    }
    let mut visited = 0;
    let mut candidates = Vec::new();
    visit(root, name, target, 0, &mut visited, &mut candidates);
    candidates.sort_by_key(|candidate| (candidate.0, candidate.1));
    candidates.into_iter().next().map(|candidate| candidate.2)
}

fn find_theme_icon(
    source: &str,
    target: u32,
    configured_theme: &str,
    roots: &[PathBuf],
    item_theme_path: Option<&Path>,
) -> Option<PathBuf> {
    let name = valid_icon_name(source)?;
    if let Some(path) = item_theme_path.filter(|path| path.is_absolute() && path.is_dir())
        && let Some(candidate) = recursive_icon_candidate(path, name, target)
    {
        return Some(candidate);
    }

    let mut themes = VecDeque::from([configured_theme.to_string()]);
    let mut seen = HashSet::new();
    while let Some(theme) = themes.pop_front() {
        if valid_icon_name(&theme).is_none() || !seen.insert(theme.clone()) || seen.len() > 32 {
            continue;
        }
        for root in roots {
            let theme_path = root.join(&theme);
            if let Some(candidate) = theme_candidate(&theme_path, name, target) {
                return Some(candidate);
            }
            let (_, inherits, _) = parse_theme_index(&theme_path);
            themes.extend(inherits);
        }
    }
    if !seen.contains("hicolor") {
        for root in roots {
            if let Some(candidate) = theme_candidate(&root.join("hicolor"), name, target) {
                return Some(candidate);
            }
        }
    }
    for root in roots {
        let pixmaps = root.parent().map(|root| root.join("pixmaps"));
        if let Some(candidate) = pixmaps
            .as_deref()
            .and_then(|root| recursive_icon_candidate(root, name, target))
        {
            return Some(candidate);
        }
    }
    recursive_icon_candidate(Path::new("/usr/share/pixmaps"), name, target)
}

fn decode_xpm(bytes: &[u8]) -> Option<DynamicImage> {
    let text = std::str::from_utf8(bytes).ok()?;
    let rows = text
        .lines()
        .filter_map(|line| {
            let start = line.find('"')? + 1;
            let end = line[start..].rfind('"')? + start;
            Some(line[start..end].to_string())
        })
        .collect::<Vec<_>>();
    let header = rows.first()?.split_whitespace().collect::<Vec<_>>();
    if header.len() < 4 {
        return None;
    }
    let width = header[0].parse::<u32>().ok()?;
    let height = header[1].parse::<u32>().ok()?;
    let colors = header[2].parse::<usize>().ok()?;
    let chars = header[3].parse::<usize>().ok()?;
    if width == 0
        || height == 0
        || width > MAX_SOURCE_IMAGE_DIMENSION
        || height > MAX_SOURCE_IMAGE_DIMENSION
        || colors > 1_024
        || chars == 0
        || chars > 4
        || rows.len() < 1 + colors + height as usize
    {
        return None;
    }
    let mut palette = HashMap::<Vec<u8>, Rgba<u8>>::new();
    for row in &rows[1..1 + colors] {
        let row = row.as_bytes();
        if row.len() < chars {
            return None;
        }
        let key = row[..chars].to_vec();
        let fields = std::str::from_utf8(&row[chars..])
            .ok()?
            .split_whitespace()
            .collect::<Vec<_>>();
        let value = fields
            .windows(2)
            .find(|pair| pair[0] == "c")
            .map(|pair| pair[1])?;
        let color = if value.eq_ignore_ascii_case("none") {
            Rgba([0, 0, 0, 0])
        } else {
            let hex = value.strip_prefix('#')?;
            let rgb = match hex.len() {
                3 => [
                    u8::from_str_radix(&hex[0..1], 16).ok()? * 17,
                    u8::from_str_radix(&hex[1..2], 16).ok()? * 17,
                    u8::from_str_radix(&hex[2..3], 16).ok()? * 17,
                ],
                6 => [
                    u8::from_str_radix(&hex[0..2], 16).ok()?,
                    u8::from_str_radix(&hex[2..4], 16).ok()?,
                    u8::from_str_radix(&hex[4..6], 16).ok()?,
                ],
                12 => [
                    (u16::from_str_radix(&hex[0..4], 16).ok()? >> 8) as u8,
                    (u16::from_str_radix(&hex[4..8], 16).ok()? >> 8) as u8,
                    (u16::from_str_radix(&hex[8..12], 16).ok()? >> 8) as u8,
                ],
                _ => return None,
            };
            Rgba([rgb[0], rgb[1], rgb[2], 255])
        };
        palette.insert(key, color);
    }
    let mut image = RgbaImage::new(width, height);
    for (y, row) in rows[1 + colors..1 + colors + height as usize]
        .iter()
        .enumerate()
    {
        let row = row.as_bytes();
        if row.len() != width as usize * chars {
            return None;
        }
        for x in 0..width as usize {
            let color = palette.get(&row[x * chars..(x + 1) * chars])?;
            image.put_pixel(x as u32, y as u32, *color);
        }
    }
    Some(DynamicImage::ImageRgba8(image))
}

/// `source_max` bounds the incoming image's own dimensions, independently of
/// `target`, which is only the size the result is scaled down to.
fn decode_image(bytes: &[u8], target: u32, source_max: u32) -> Option<DynamicImage> {
    if bytes.is_empty() || bytes.len() > MAX_SOURCE_IMAGE_BYTES {
        return None;
    }
    if bytes.starts_with(b"/* XPM */") {
        return decode_xpm(bytes);
    }
    if bytes.starts_with(b"<svg")
        || bytes[..4_096.min(bytes.len())]
            .windows(4)
            .any(|part| part == b"<svg")
    {
        let mut options = resvg::usvg::Options {
            image_href_resolver: resvg::usvg::ImageHrefResolver {
                resolve_data: Box::new(|_, _, _| None),
                resolve_string: Box::new(|_, _| None),
            },
            ..resvg::usvg::Options::default()
        };
        options.fontdb_mut().load_system_fonts();
        let tree = resvg::usvg::Tree::from_data(bytes, &options).ok()?;
        let size = tree.size();
        if size.width() > source_max as f32 || size.height() > source_max as f32 {
            return None;
        }
        let scale = (target as f32 / size.width())
            .min(target as f32 / size.height())
            .min(1.0);
        let width = (size.width() * scale).round().max(1.0) as u32;
        let height = (size.height() * scale).round().max(1.0) as u32;
        let mut pixmap = resvg::tiny_skia::Pixmap::new(width, height)?;
        resvg::render(
            &tree,
            resvg::tiny_skia::Transform::from_scale(scale, scale),
            &mut pixmap.as_mut(),
        );
        return RgbaImage::from_raw(width, height, pixmap.take()).map(DynamicImage::ImageRgba8);
    }
    let mut reader = image::ImageReader::new(std::io::Cursor::new(bytes))
        .with_guessed_format()
        .ok()?;
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(source_max);
    limits.max_image_height = Some(source_max);
    // The allocation ceiling has to admit the dimensions just allowed, or the
    // dimension check above would be dead letter for anything icon-sized.
    limits.max_alloc = Some(if source_max > MAX_SOURCE_IMAGE_DIMENSION {
        MAX_ARTWORK_DECODE_BYTES
    } else {
        MAX_SOURCE_IMAGE_BYTES as u64
    });
    reader.limits(limits);
    let image = reader.decode().ok()?;
    (image.width() <= source_max && image.height() <= source_max).then_some(image)
}

fn encode_png(image: DynamicImage, target: u32) -> Option<PngImage> {
    let image = if image.width() > target || image.height() > target {
        image.resize(target, target, FilterType::Lanczos3)
    } else {
        image
    };
    let (width, height) = image.dimensions();
    if width == 0 || height == 0 || width > u16::MAX as u32 || height > u16::MAX as u32 {
        return None;
    }
    let mut png = std::io::Cursor::new(Vec::new());
    image.write_to(&mut png, image::ImageFormat::Png).ok()?;
    let png = png.into_inner();
    (png.len() <= MAX_FINAL_PNG_BYTES).then_some(PngImage {
        width: width as u16,
        height: height as u16,
        png,
    })
}

fn normalize_encoded_image(bytes: &[u8], target: u32, source_max: u32) -> Option<PngImage> {
    encode_png(decode_image(bytes, target, source_max)?, target)
}

fn normalize_notification_pixels(
    (width, height, rowstride, has_alpha, bits, channels, bytes): NotificationImageData,
    target: u32,
) -> Option<PngImage> {
    if width <= 0
        || height <= 0
        || width > MAX_SOURCE_IMAGE_DIMENSION as i32
        || height > MAX_SOURCE_IMAGE_DIMENSION as i32
        || bits != 8
        || channels != if has_alpha { 4 } else { 3 }
        || rowstride < width.checked_mul(channels)?
        || rowstride as usize > MAX_SOURCE_IMAGE_BYTES
        || rowstride as usize * height as usize != bytes.len()
    {
        return None;
    }
    let mut rgba = Vec::with_capacity(width as usize * height as usize * 4);
    for row in bytes.chunks_exact(rowstride as usize) {
        for pixel in row[..width as usize * channels as usize].chunks_exact(channels as usize) {
            rgba.extend_from_slice(&[
                pixel[0],
                pixel[1],
                pixel[2],
                if has_alpha { pixel[3] } else { 255 },
            ]);
        }
    }
    let image = ImageBuffer::from_raw(width as u32, height as u32, rgba)?;
    encode_png(DynamicImage::ImageRgba8(image), target)
}

fn composite_overlay(base: &PngImage, overlay: &PngImage) -> Option<PngImage> {
    let mut base = decode_image(&base.png, TRAY_ICON_SIZE, MAX_SOURCE_IMAGE_DIMENSION)?.to_rgba8();
    let overlay = decode_image(&overlay.png, TRAY_ICON_SIZE, MAX_SOURCE_IMAGE_DIMENSION)?
        .resize(
            (base.width() / 2).max(1),
            (base.height() / 2).max(1),
            FilterType::Lanczos3,
        )
        .to_rgba8();
    let x = base.width().saturating_sub(overlay.width());
    let y = base.height().saturating_sub(overlay.height());
    image::imageops::overlay(&mut base, &overlay, x.into(), y.into());
    encode_png(DynamicImage::ImageRgba8(base), TRAY_ICON_SIZE)
}

/// Convert the best SNI `a(iiay)` ARGB32 pixmap to bounded RGBA PNG.
pub fn best_pixmap_png(pixmaps: &[StatusNotifierPixmap], target: u32) -> Option<PngImage> {
    let (width, height, argb) = pixmaps
        .iter()
        .filter(|(width, height, bytes)| {
            *width > 0
                && *height > 0
                && *width <= 512
                && *height <= 512
                && (*width as usize)
                    .checked_mul(*height as usize)
                    .and_then(|pixels| pixels.checked_mul(4))
                    == Some(bytes.len())
        })
        .min_by_key(|(width, height, _)| {
            let size = (*width).max(*height) as i64;
            (size - target as i64).unsigned_abs()
        })?;
    let width = *width as u32;
    let height = *height as u32;
    let mut rgba = Vec::with_capacity(argb.len());
    for pixel in argb.chunks_exact(4) {
        rgba.extend_from_slice(&[pixel[1], pixel[2], pixel[3], pixel[0]]);
    }
    let image = ImageBuffer::from_raw(width, height, rgba)?;
    encode_png(DynamicImage::ImageRgba8(image), target)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_markup_entities_and_clips_utf8() {
        assert_eq!(
            strip_markup("<b>Hello &amp; <i>world</i></b><br/>next &#x1F642;", 1024),
            "Hello & world\nnext 🙂"
        );
        assert_eq!(strip_markup("ééé", 5), "éé");
        assert_eq!(strip_markup("broken &wat", 1024), "broken &wat");
    }

    #[test]
    fn converts_argb_network_order_to_png() {
        let image = best_pixmap_png(&[(1, 1, vec![0x44, 0x11, 0x22, 0x33])], 64).unwrap();
        assert_eq!((image.width, image.height), (1, 1));
        let decoder = png::Decoder::new(std::io::Cursor::new(image.png));
        let mut reader = decoder.read_info().unwrap();
        let mut bytes = vec![0; reader.output_buffer_size().unwrap()];
        let info = reader.next_frame(&mut bytes).unwrap();
        assert_eq!(&bytes[..info.buffer_size()], &[0x11, 0x22, 0x33, 0x44]);
    }

    #[test]
    fn rejects_bad_pixmap_dimensions() {
        assert!(best_pixmap_png(&[(2, 2, vec![0; 4])], 64).is_none());
        assert!(best_pixmap_png(&[(513, 1, vec![0; 513 * 4])], 64).is_none());
    }

    #[test]
    fn tray_ids_are_nonzero_and_not_reused_immediately() {
        let watcher = WatcherState::default();
        assert_eq!(watcher.id(), 1);
        assert_eq!(watcher.id(), 2);
    }

    #[test]
    fn notification_rate_bucket_bursts_refills_and_discounts_replacements() {
        let mut state = NotificationState::new(Config::default());
        for _ in 0..20 {
            assert!(state.charge(":1.2", false));
        }
        assert!(!state.charge(":1.2", false));
        state.rate.get_mut(":1.2").unwrap().updated = Instant::now() - Duration::from_secs(1);
        for _ in 0..8 {
            assert!(state.charge(":1.2", true));
        }
        assert!(!state.charge(":1.2", true));
    }

    fn tray_item(tray_id: u32, revision: u32) -> TrayItem {
        TrayItem {
            tray_id,
            revision,
            status: TRAY_STATUS_ACTIVE,
            category: TRAY_CATEGORY_UNKNOWN,
            flags: 0,
            app_id: String::new(),
            title: String::new(),
            tooltip_title: String::new(),
            tooltip_body: String::new(),
            icon: PngImage::default(),
        }
    }

    #[test]
    fn event_queue_coalesces_upserts_but_not_across_delete() {
        let mut queue = EventQueue::default();
        assert!(
            queue
                .push(Event::Tray(TrayRecord::Upsert(tray_item(7, 1))))
                .is_none()
        );
        assert!(
            queue
                .push(Event::Tray(TrayRecord::Upsert(tray_item(7, 2))))
                .is_none()
        );
        assert_eq!(queue.queued.len(), 1);
        assert!(
            queue
                .push(Event::Tray(TrayRecord::Delete { tray_id: 7 }))
                .is_none()
        );
        assert!(
            queue
                .push(Event::Tray(TrayRecord::Upsert(tray_item(7, 3))))
                .is_none()
        );
        assert_eq!(queue.queued.len(), 3);
    }

    #[test]
    fn normalizes_notification_pixels_and_rejects_bad_stride() {
        let image =
            normalize_notification_pixels((1, 1, 8, true, 8, 4, vec![1, 2, 3, 4, 0, 0, 0, 0]), 512)
                .unwrap();
        assert_eq!((image.width, image.height), (1, 1));
        assert!(normalize_notification_pixels((2, 1, 4, true, 8, 4, vec![0; 4]), 512).is_none());
    }

    #[test]
    fn decodes_bounded_legacy_xpm() {
        let xpm = br##"/* XPM */
static char *icon[] = {
"2 1 2 1",
". c None",
"X c #123456",
".X"};"##;
        let image = normalize_encoded_image(xpm, 64, MAX_SOURCE_IMAGE_DIMENSION).unwrap();
        assert_eq!((image.width, image.height), (2, 1));
    }

    /// 640×640 is exactly what Spotify publishes, and it sits over the icon
    /// ceiling. Held here because accepting the cover while refusing an icon of
    /// the same size is a deliberate split, not an accident of one constant.
    #[test]
    fn a_cover_sized_source_normalizes_while_an_icon_that_large_is_refused() {
        let source = image::DynamicImage::ImageRgb8(image::RgbImage::from_pixel(
            640,
            640,
            image::Rgb([12, 40, 120]),
        ));
        let mut jpeg = std::io::Cursor::new(Vec::new());
        source
            .write_to(&mut jpeg, image::ImageFormat::Jpeg)
            .expect("encode jpeg");
        let jpeg = jpeg.into_inner();

        let cover = normalize_encoded_image(&jpeg, 512, MAX_ARTWORK_SOURCE_DIMENSION)
            .expect("a 640px cover normalizes");
        assert_eq!((cover.width, cover.height), (512, 512));

        assert!(normalize_encoded_image(&jpeg, 512, MAX_SOURCE_IMAGE_DIMENSION).is_none());
    }

    #[test]
    fn icon_theme_lookup_honors_directory_size_and_format_order() {
        let root = std::env::temp_dir().join(format!(
            "blit-icon-theme-test-{}-{}",
            std::process::id(),
            Instant::now().elapsed().as_nanos()
        ));
        let theme = root.join("fixture");
        fs::create_dir_all(theme.join("64x64/apps")).unwrap();
        fs::write(
            theme.join("index.theme"),
            "[Icon Theme]\nDirectories=64x64/apps\n[64x64/apps]\nSize=64\nType=Fixed\n",
        )
        .unwrap();
        let png = best_pixmap_png(&[(1, 1, vec![255, 1, 2, 3])], 64).unwrap();
        let expected = theme.join("64x64/apps/fixture.png");
        fs::write(&expected, png.png).unwrap();
        assert_eq!(
            find_theme_icon("fixture", 64, "fixture", std::slice::from_ref(&root), None),
            Some(expected)
        );
        fs::remove_dir_all(root).unwrap();
    }
}

#[cfg(all(test, unix))]
mod dbus_tests {
    use super::*;
    use futures_util::StreamExt;
    use std::io::BufRead;
    use std::process::{Child, Command as ProcessCommand, Stdio};
    use std::sync::Mutex as StdMutex;
    use std::sync::atomic::AtomicI32;
    use tokio::time::{sleep, timeout};

    struct TestBus(Child);

    impl TestBus {
        fn spawn() -> (Self, String) {
            let mut child = ProcessCommand::new("dbus-daemon")
                .args(["--session", "--print-address=1", "--nofork"])
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .spawn()
                .unwrap();
            let mut address = String::new();
            std::io::BufReader::new(child.stdout.take().unwrap())
                .read_line(&mut address)
                .unwrap();
            (Self(child), address.trim().to_string())
        }
    }

    impl Drop for TestBus {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    async fn next_event(bridge: &mut Bridge) -> Event {
        timeout(Duration::from_secs(2), async {
            loop {
                if let Some(event) = bridge.try_recv() {
                    return event;
                }
                sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap()
    }

    #[tokio::test]
    async fn notifications_replace_action_and_close_on_the_private_bus() {
        let (_bus, address) = TestBus::spawn();
        let mut bridge = Bridge::start(&address, Config::default(), Arc::new(|| {}))
            .await
            .unwrap();
        let client = zbus::connection::Builder::address(address.as_str())
            .unwrap()
            .build()
            .await
            .unwrap();
        let proxy = Proxy::new(
            &client,
            "org.freedesktop.Notifications",
            NOTIFICATION_PATH,
            "org.freedesktop.Notifications",
        )
        .await
        .unwrap();

        let id: u32 = proxy
            .call(
                "Notify",
                &(
                    "fixture",
                    0u32,
                    "",
                    "<b>Build</b>",
                    "Done &amp; ready",
                    vec!["default", "Open"],
                    HashMap::<String, OwnedValue>::new(),
                    0i32,
                ),
            )
            .await
            .unwrap();
        assert_ne!(id, 0);
        let Event::Notification(NotificationRecord::Upsert(first)) = next_event(&mut bridge).await
        else {
            panic!("expected notification upsert")
        };
        assert_eq!(first.summary, "Build");
        assert_eq!(first.body, "Done & ready");

        // A peer cannot guess another connection's ID and replace or close
        // its notification. Unknown/foreign replacement IDs allocate fresh
        // state, preserving the ordinary notification API for the caller.
        let foreign_client = zbus::connection::Builder::address(address.as_str())
            .unwrap()
            .build()
            .await
            .unwrap();
        let foreign_proxy = Proxy::new(
            &foreign_client,
            "org.freedesktop.Notifications",
            NOTIFICATION_PATH,
            "org.freedesktop.Notifications",
        )
        .await
        .unwrap();
        let foreign_id: u32 = foreign_proxy
            .call(
                "Notify",
                &(
                    "foreign",
                    id,
                    "",
                    "Foreign",
                    "",
                    Vec::<String>::new(),
                    HashMap::<String, OwnedValue>::new(),
                    0i32,
                ),
            )
            .await
            .unwrap();
        assert_ne!(foreign_id, id);
        assert!(matches!(
            next_event(&mut bridge).await,
            Event::Notification(NotificationRecord::Upsert(item))
                if item.notification_id == foreign_id
        ));
        assert!(
            foreign_proxy
                .call::<_, _, ()>("CloseNotification", &(id,))
                .await
                .is_err()
        );

        let replaced: u32 = proxy
            .call(
                "Notify",
                &(
                    "fixture",
                    id,
                    "",
                    "Build",
                    "Really done",
                    vec!["default", "Open"],
                    HashMap::<String, OwnedValue>::new(),
                    0i32,
                ),
            )
            .await
            .unwrap();
        assert_eq!(replaced, id);
        let Event::Notification(NotificationRecord::Upsert(second)) = next_event(&mut bridge).await
        else {
            panic!("expected replacement upsert")
        };
        assert!(second.revision > first.revision);

        let mut actions = proxy.receive_signal("ActionInvoked").await.unwrap();
        let mut closed = proxy.receive_signal("NotificationClosed").await.unwrap();
        assert!(bridge.try_command(Command::Notification(NotificationEvent {
            notification_id: id,
            revision: second.revision,
            kind: NOTIFICATION_EVENT_DEFAULT,
            key: String::new(),
        })));
        let action = timeout(Duration::from_secs(2), actions.next())
            .await
            .unwrap()
            .unwrap();
        let (signal_id, key): (u32, String) = action.body().deserialize().unwrap();
        assert_eq!((signal_id, key.as_str()), (id, "default"));
        let closed_signal = timeout(Duration::from_secs(2), closed.next())
            .await
            .unwrap()
            .unwrap();
        let (signal_id, reason): (u32, u32) = closed_signal.body().deserialize().unwrap();
        assert_eq!((signal_id, reason), (id, 2));
        assert!(matches!(
            next_event(&mut bridge).await,
            Event::Notification(NotificationRecord::Delete {
                notification_id,
                reason: NOTIFICATION_CLOSED_DISMISSED,
                ..
            }) if notification_id == id
        ));
    }

    struct MockMprisBase;

    #[interface(name = "org.mpris.MediaPlayer2")]
    impl MockMprisBase {
        #[zbus(property)]
        fn identity(&self) -> &str {
            "GetAll fixture"
        }

        #[zbus(property)]
        fn desktop_entry(&self) -> &str {
            "fixture"
        }

        #[zbus(property)]
        fn can_raise(&self) -> bool {
            false
        }
    }

    struct MockMprisPlayer {
        calls: Arc<StdMutex<Vec<&'static str>>>,
    }

    #[interface(name = "org.mpris.MediaPlayer2.Player")]
    impl MockMprisPlayer {
        #[zbus(property)]
        fn playback_status(&self) -> &str {
            "Stopped"
        }

        #[zbus(property)]
        fn metadata(&self) -> HashMap<String, OwnedValue> {
            HashMap::new()
        }

        #[zbus(property)]
        fn loop_status(&self) -> &str {
            "None"
        }

        #[zbus(property)]
        fn rate(&self) -> f64 {
            1.0
        }

        #[zbus(property)]
        fn minimum_rate(&self) -> f64 {
            1.0
        }

        #[zbus(property)]
        fn maximum_rate(&self) -> f64 {
            1.0
        }

        #[zbus(property)]
        fn volume(&self) -> f64 {
            1.0
        }

        #[zbus(property)]
        fn position(&self) -> i64 {
            17
        }

        #[zbus(property)]
        fn shuffle(&self) -> bool {
            false
        }

        #[zbus(property)]
        fn can_control(&self) -> bool {
            true
        }

        #[zbus(property)]
        fn can_play(&self) -> bool {
            true
        }

        #[zbus(property)]
        fn can_pause(&self) -> bool {
            true
        }

        #[zbus(property)]
        fn can_go_next(&self) -> bool {
            false
        }

        #[zbus(property)]
        fn can_go_previous(&self) -> bool {
            false
        }

        #[zbus(property)]
        fn can_seek(&self) -> bool {
            false
        }

        fn play(&self) {
            self.calls.lock().unwrap().push("play");
        }

        fn pause(&self) {
            self.calls.lock().unwrap().push("pause");
        }
    }

    #[tokio::test]
    async fn mpris_snapshot_uses_the_properties_get_all_interface() {
        let (_bus, address) = TestBus::spawn();
        let calls = Arc::new(StdMutex::new(Vec::new()));
        let _player = zbus::connection::Builder::address(address.as_str())
            .unwrap()
            .name("org.mpris.MediaPlayer2.get_all_fixture")
            .unwrap()
            .serve_at("/org/mpris/MediaPlayer2", MockMprisBase)
            .unwrap()
            .serve_at(
                "/org/mpris/MediaPlayer2",
                MockMprisPlayer {
                    calls: calls.clone(),
                },
            )
            .unwrap()
            .build()
            .await
            .unwrap();
        let mut bridge = Bridge::start(&address, Config::default(), Arc::new(|| {}))
            .await
            .unwrap();

        let Event::Mpris(records) = next_event(&mut bridge).await else {
            panic!("expected initial MPRIS snapshot")
        };
        let player = records
            .into_iter()
            .find_map(|record| match record {
                blit_remote::media::MprisRecord::Upsert(player) => Some(player),
                blit_remote::media::MprisRecord::Delete { .. } => None,
            })
            .expect("snapshot must contain an upsert");
        assert_eq!(player.identity, "GetAll fixture");
        assert_eq!(player.desktop_entry, "fixture");
        assert_eq!(player.position_us, 17);
    }

    /// A player that names its cover over HTTP, which is the only form Spotify
    /// and other catalogue-backed players ever publish.
    struct MockStreamingPlayer {
        art_url: String,
    }

    #[interface(name = "org.mpris.MediaPlayer2.Player")]
    impl MockStreamingPlayer {
        #[zbus(property)]
        fn playback_status(&self) -> &str {
            "Playing"
        }

        #[zbus(property)]
        fn metadata(&self) -> HashMap<String, OwnedValue> {
            HashMap::from([
                (
                    "mpris:artUrl".to_string(),
                    zbus::zvariant::Value::from(self.art_url.clone())
                        .try_to_owned()
                        .unwrap(),
                ),
                (
                    "xesam:title".to_string(),
                    zbus::zvariant::Value::from("Da Funk")
                        .try_to_owned()
                        .unwrap(),
                ),
            ])
        }

        #[zbus(property)]
        fn loop_status(&self) -> &str {
            "None"
        }

        #[zbus(property)]
        fn rate(&self) -> f64 {
            1.0
        }

        #[zbus(property)]
        fn minimum_rate(&self) -> f64 {
            1.0
        }

        #[zbus(property)]
        fn maximum_rate(&self) -> f64 {
            1.0
        }

        #[zbus(property)]
        fn volume(&self) -> f64 {
            1.0
        }

        #[zbus(property)]
        fn position(&self) -> i64 {
            0
        }

        #[zbus(property)]
        fn shuffle(&self) -> bool {
            false
        }

        #[zbus(property)]
        fn can_control(&self) -> bool {
            true
        }

        #[zbus(property)]
        fn can_play(&self) -> bool {
            true
        }

        #[zbus(property)]
        fn can_pause(&self) -> bool {
            true
        }

        #[zbus(property)]
        fn can_go_next(&self) -> bool {
            false
        }

        #[zbus(property)]
        fn can_go_previous(&self) -> bool {
            false
        }

        #[zbus(property)]
        fn can_seek(&self) -> bool {
            false
        }
    }

    /// The whole path Spotify exercises: a cover the player only names by URL
    /// reaches a client record as that URL, and the server never dereferences
    /// it. The live server is a real listener precisely so the hit counter can
    /// prove no request was made.
    #[tokio::test]
    async fn a_cover_named_over_http_reaches_the_client_as_a_url_unfetched() {
        let (art_url, hits) =
            crate::test_http::serve(crate::test_http::cover_jpeg(640, 640), "200 OK", None);
        let (_bus, address) = TestBus::spawn();
        let _player = zbus::connection::Builder::address(address.as_str())
            .unwrap()
            .name("org.mpris.MediaPlayer2.streaming_fixture")
            .unwrap()
            .serve_at("/org/mpris/MediaPlayer2", MockMprisBase)
            .unwrap()
            .serve_at(
                "/org/mpris/MediaPlayer2",
                MockStreamingPlayer {
                    art_url: art_url.clone(),
                },
            )
            .unwrap()
            .build()
            .await
            .unwrap();
        let mut bridge = Bridge::start(&address, Config::default(), Arc::new(|| {}))
            .await
            .unwrap();

        let Event::Mpris(records) = next_event(&mut bridge).await else {
            panic!("expected initial MPRIS snapshot")
        };
        let player = records
            .into_iter()
            .find_map(|record| match record {
                blit_remote::media::MprisRecord::Upsert(player) => Some(player),
                blit_remote::media::MprisRecord::Delete { .. } => None,
            })
            .expect("snapshot must contain an upsert");

        assert_eq!(player.title, "Da Funk");
        assert_eq!(
            player.artwork,
            blit_remote::media::MprisArtwork::Url(art_url),
            "a cover named by URL must reach the viewer as that URL"
        );
        assert_eq!(
            hits.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "the server must not fetch a cover the viewer can load itself"
        );
    }

    #[tokio::test]
    async fn mpris_actions_leave_the_command_loop_and_stay_ordered_per_player() {
        let (_bus, address) = TestBus::spawn();
        let calls = Arc::new(StdMutex::new(Vec::new()));
        let _player = zbus::connection::Builder::address(address.as_str())
            .unwrap()
            .name("org.mpris.MediaPlayer2.action_fixture")
            .unwrap()
            .serve_at("/org/mpris/MediaPlayer2", MockMprisBase)
            .unwrap()
            .serve_at(
                "/org/mpris/MediaPlayer2",
                MockMprisPlayer {
                    calls: calls.clone(),
                },
            )
            .unwrap()
            .build()
            .await
            .unwrap();
        let mut bridge = Bridge::start(&address, Config::default(), Arc::new(|| {}))
            .await
            .unwrap();
        let Event::Mpris(records) = next_event(&mut bridge).await else {
            panic!("expected initial MPRIS snapshot")
        };
        let player_id = records
            .into_iter()
            .find_map(|record| match record {
                blit_remote::media::MprisRecord::Upsert(player) => Some(player.player_id),
                blit_remote::media::MprisRecord::Delete { .. } => None,
            })
            .unwrap();
        for (nonce, kind) in [
            (1, blit_remote::media::MprisActionKind::Play),
            (2, blit_remote::media::MprisActionKind::Pause),
        ] {
            assert!(bridge.try_command(Command::Mpris {
                requester: 9,
                action: blit_remote::media::MprisAction {
                    nonce,
                    player_id,
                    kind,
                    track_revision: 0,
                    value: 0,
                },
            }));
        }

        let mut results = Vec::new();
        while results.len() < 2 {
            if let Event::MprisAction { requester, result } = next_event(&mut bridge).await {
                assert_eq!(requester, 9);
                assert_eq!(result.status, blit_remote::STATUS_OK);
                results.push(result.nonce);
            }
        }
        assert_eq!(results, vec![1, 2]);
        assert_eq!(*calls.lock().unwrap(), vec!["play", "pause"]);
    }

    #[tokio::test]
    async fn access_portal_roundtrips_normalized_choices() {
        let (_bus, address) = TestBus::spawn();
        let mut bridge = Bridge::start(&address, Config::default(), Arc::new(|| {}))
            .await
            .unwrap();
        let caller = tokio::spawn(async move {
            let client = zbus::connection::Builder::address(address.as_str())
                .unwrap()
                .build()
                .await
                .unwrap();
            let proxy = Proxy::new(
                &client,
                "org.freedesktop.impl.portal.desktop.blit",
                "/org/freedesktop/portal/desktop",
                "org.freedesktop.impl.portal.Access",
            )
            .await
            .unwrap();
            let raw_choices = vec![(
                "mode".to_string(),
                "Mode".to_string(),
                vec![
                    ("a".to_string(), "First".to_string()),
                    ("b".to_string(), "Second".to_string()),
                ],
                "a".to_string(),
            )];
            let mut options = HashMap::<String, OwnedValue>::new();
            options.insert(
                "choices".into(),
                OwnedValue::try_from(zbus::zvariant::Value::from(raw_choices)).unwrap(),
            );
            proxy
                .call::<_, _, (u32, HashMap<String, OwnedValue>)>(
                    "AccessDialog",
                    &(
                        OwnedObjectPath::try_from(
                            "/org/freedesktop/portal/desktop/request/fixture/access",
                        )
                        .unwrap(),
                        "org.example.App",
                        "",
                        "Permission",
                        "",
                        "Choose a mode",
                        options,
                    ),
                )
                .await
                .unwrap()
        });
        let Event::Portal {
            request: blit_remote::media::PortalRequest::Access(request),
            parent_window,
        } = next_event(&mut bridge).await
        else {
            panic!("expected Access request")
        };
        assert!(parent_window.is_empty());
        assert_eq!(request.app_id, "org.example.App");
        assert_eq!(request.choices.len(), 1);
        assert_eq!(request.choices[0].initial_value, "a");
        assert!(
            bridge.try_command(Command::PortalReply(blit_remote::media::PortalReply {
                request_id: request.request_id,
                decision: blit_remote::media::PortalDecision::Grant,
                surface_ids: Vec::new(),
                choices: vec![blit_remote::media::PortalChoiceValue {
                    id: "mode".into(),
                    value: "b".into(),
                }],
            },))
        );
        let (response, mut results) = caller.await.unwrap();
        assert_eq!(response, 0);
        let choices =
            Vec::<(String, String)>::try_from(results.remove("choices").unwrap()).unwrap();
        assert_eq!(choices, vec![("mode".into(), "b".into())]);
    }

    #[tokio::test]
    async fn screencast_portal_roundtrips_session_and_stream_metadata() {
        let (_bus, address) = TestBus::spawn();
        let mut bridge = Bridge::start(&address, Config::default(), Arc::new(|| {}))
            .await
            .unwrap();
        let caller = tokio::spawn(async move {
            let client = zbus::connection::Builder::address(address.as_str())
                .unwrap()
                .build()
                .await
                .unwrap();
            let proxy = Proxy::new(
                &client,
                "org.freedesktop.impl.portal.desktop.blit",
                "/org/freedesktop/portal/desktop",
                "org.freedesktop.impl.portal.ScreenCast",
            )
            .await
            .unwrap();
            let request =
                OwnedObjectPath::try_from("/org/freedesktop/portal/desktop/request/fixture/create")
                    .unwrap();
            let session = OwnedObjectPath::try_from(
                "/org/freedesktop/portal/desktop/session/fixture/session",
            )
            .unwrap();
            let (response, _): (u32, HashMap<String, OwnedValue>) = proxy
                .call(
                    "CreateSession",
                    &(
                        request,
                        session.clone(),
                        "org.example.App",
                        HashMap::<String, OwnedValue>::new(),
                    ),
                )
                .await
                .unwrap();
            assert_eq!(response, 0);
            let mut options = HashMap::<String, OwnedValue>::new();
            options.insert("types".into(), OwnedValue::from(2u32));
            options.insert("cursor_mode".into(), OwnedValue::from(1u32));
            options.insert("multiple".into(), OwnedValue::from(false));
            let (response, _): (u32, HashMap<String, OwnedValue>) = proxy
                .call(
                    "SelectSources",
                    &(
                        OwnedObjectPath::try_from(
                            "/org/freedesktop/portal/desktop/request/fixture/select",
                        )
                        .unwrap(),
                        session.clone(),
                        "org.example.App",
                        options,
                    ),
                )
                .await
                .unwrap();
            assert_eq!(response, 0);
            let result: (u32, HashMap<String, OwnedValue>) = proxy
                .call(
                    "Start",
                    &(
                        OwnedObjectPath::try_from(
                            "/org/freedesktop/portal/desktop/request/fixture/start",
                        )
                        .unwrap(),
                        session.clone(),
                        "org.example.App",
                        "wayland:0123456789abcdef0123456789abcdef",
                        HashMap::<String, OwnedValue>::new(),
                    ),
                )
                .await
                .unwrap();
            let session_proxy = Proxy::new(
                &client,
                "org.freedesktop.impl.portal.desktop.blit",
                session,
                "org.freedesktop.impl.portal.Session",
            )
            .await
            .unwrap();
            session_proxy.call::<_, _, ()>("Close", &()).await.unwrap();
            result
        });
        let Event::Portal {
            request: blit_remote::media::PortalRequest::ScreenCast(request),
            parent_window,
        } = next_event(&mut bridge).await
        else {
            panic!("expected ScreenCast request")
        };
        assert_eq!(request.app_id, "org.example.App");
        assert_eq!(parent_window, "wayland:0123456789abcdef0123456789abcdef");
        assert!(bridge.try_command(Command::PortalScreenCastStarted {
            request_id: request.request_id,
            session_id: 41,
            streams: vec![PortalStream {
                surface_id: 7,
                node_id: 123,
                pipewire_serial: 456,
                width: 1280,
                height: 720,
            }],
        }));
        let (response, mut results) = caller.await.unwrap();
        assert_eq!(response, 0);
        let streams =
            Vec::<(u32, HashMap<String, OwnedValue>)>::try_from(results.remove("streams").unwrap())
                .unwrap();
        assert_eq!(streams.len(), 1);
        assert_eq!(streams[0].0, 123);
        assert_eq!(
            String::try_from(streams[0].1["id"].try_clone().unwrap()).unwrap(),
            "blit-41-0"
        );
        assert_eq!(
            u64::try_from(streams[0].1["pipewire-serial"].try_clone().unwrap()).unwrap(),
            456
        );
        assert!(matches!(
            next_event(&mut bridge).await,
            Event::PortalSessionClosed(41)
        ));
    }

    #[tokio::test]
    async fn screencast_select_sources_is_one_shot_and_reuse_closes_the_session() {
        let (_bus, address) = TestBus::spawn();
        let _bridge = Bridge::start(&address, Config::default(), Arc::new(|| {}))
            .await
            .unwrap();
        let client = zbus::connection::Builder::address(address.as_str())
            .unwrap()
            .build()
            .await
            .unwrap();
        let proxy = Proxy::new(
            &client,
            "org.freedesktop.impl.portal.desktop.blit",
            "/org/freedesktop/portal/desktop",
            "org.freedesktop.impl.portal.ScreenCast",
        )
        .await
        .unwrap();
        let session =
            OwnedObjectPath::try_from("/org/freedesktop/portal/desktop/session/fixture/one_shot")
                .unwrap();
        let (response, _): (u32, HashMap<String, OwnedValue>) = proxy
            .call(
                "CreateSession",
                &(
                    OwnedObjectPath::try_from(
                        "/org/freedesktop/portal/desktop/request/fixture/one_shot_create",
                    )
                    .unwrap(),
                    session.clone(),
                    "org.example.App",
                    HashMap::<String, OwnedValue>::new(),
                ),
            )
            .await
            .unwrap();
        assert_eq!(response, 0);
        let session_proxy = Proxy::new(
            &client,
            "org.freedesktop.impl.portal.desktop.blit",
            session.clone(),
            "org.freedesktop.impl.portal.Session",
        )
        .await
        .unwrap();
        let mut closed = session_proxy.receive_signal("Closed").await.unwrap();
        let select = |request: &str| {
            let mut options = HashMap::<String, OwnedValue>::new();
            options.insert("types".into(), OwnedValue::from(2u32));
            options.insert("cursor_mode".into(), OwnedValue::from(1u32));
            options.insert("multiple".into(), OwnedValue::from(false));
            (OwnedObjectPath::try_from(request).unwrap(), options)
        };
        let (request, options) =
            select("/org/freedesktop/portal/desktop/request/fixture/one_shot_select_first");
        let (response, _): (u32, HashMap<String, OwnedValue>) = proxy
            .call(
                "SelectSources",
                &(request, session.clone(), "org.example.App", options),
            )
            .await
            .unwrap();
        assert_eq!(response, 0);
        let (request, options) =
            select("/org/freedesktop/portal/desktop/request/fixture/one_shot_select_second");
        let (response, _): (u32, HashMap<String, OwnedValue>) = proxy
            .call(
                "SelectSources",
                &(request, session, "org.example.App", options),
            )
            .await
            .unwrap();
        assert_eq!(response, 1);
        timeout(Duration::from_secs(2), closed.next())
            .await
            .unwrap()
            .expect("backend must emit Session.Closed");
    }

    #[tokio::test]
    async fn screencast_session_pressure_closes_the_oldest_prestart_object() {
        let (_bus, address) = TestBus::spawn();
        let _bridge = Bridge::start(&address, Config::default(), Arc::new(|| {}))
            .await
            .unwrap();
        let client = zbus::connection::Builder::address(address.as_str())
            .unwrap()
            .build()
            .await
            .unwrap();
        let proxy = Proxy::new(
            &client,
            "org.freedesktop.impl.portal.desktop.blit",
            "/org/freedesktop/portal/desktop",
            "org.freedesktop.impl.portal.ScreenCast",
        )
        .await
        .unwrap();
        let oldest =
            OwnedObjectPath::try_from("/org/freedesktop/portal/desktop/session/fixture/pressure_0")
                .unwrap();
        let mut closed = None;
        for index in 0..=32 {
            let session = OwnedObjectPath::try_from(format!(
                "/org/freedesktop/portal/desktop/session/fixture/pressure_{index}"
            ))
            .unwrap();
            let request = OwnedObjectPath::try_from(format!(
                "/org/freedesktop/portal/desktop/request/fixture/pressure_{index}"
            ))
            .unwrap();
            let (response, _): (u32, HashMap<String, OwnedValue>) = proxy
                .call(
                    "CreateSession",
                    &(
                        request,
                        session,
                        "org.example.App",
                        HashMap::<String, OwnedValue>::new(),
                    ),
                )
                .await
                .unwrap();
            assert_eq!(response, 0);
            if index == 0 {
                let session_proxy = Proxy::new(
                    &client,
                    "org.freedesktop.impl.portal.desktop.blit",
                    oldest.clone(),
                    "org.freedesktop.impl.portal.Session",
                )
                .await
                .unwrap();
                closed = Some(session_proxy.receive_signal("Closed").await.unwrap());
            }
        }
        timeout(Duration::from_secs(2), closed.unwrap().next())
            .await
            .unwrap()
            .expect("pressure eviction must emit Session.Closed");
    }

    struct MockItem;

    #[interface(name = "org.kde.StatusNotifierItem")]
    impl MockItem {
        #[zbus(property)]
        fn id(&self) -> &str {
            "fixture"
        }

        #[zbus(property)]
        fn status(&self) -> &str {
            "Active"
        }

        #[zbus(property)]
        fn title(&self) -> &str {
            "Fixture"
        }

        #[zbus(property)]
        fn category(&self) -> &str {
            "ApplicationStatus"
        }

        #[zbus(property)]
        fn item_is_menu(&self) -> bool {
            false
        }

        #[zbus(property)]
        fn menu(&self) -> OwnedObjectPath {
            OwnedObjectPath::try_from("/Menu").unwrap()
        }

        #[zbus(property)]
        fn tool_tip(&self) -> StatusNotifierTooltip {
            (
                String::new(),
                Vec::new(),
                "Fixture tip".into(),
                "Plain body".into(),
            )
        }

        #[zbus(property)]
        fn icon_pixmap(&self) -> Vec<StatusNotifierPixmap> {
            vec![(1, 1, vec![0xff, 0x10, 0x20, 0x30])]
        }
    }

    /// Chromium's tray implementation — every Electron application, so Discord,
    /// Slack, and Legcord — announces a repainted icon with the interface's own
    /// `NewIcon` signal and never emits `PropertiesChanged`. The unread badge is
    /// drawn into that pixmap, so the icon is the only thing that changes.
    struct MockRepaintingItem {
        pixel: Arc<AtomicI32>,
    }

    #[interface(name = "org.kde.StatusNotifierItem")]
    impl MockRepaintingItem {
        #[zbus(property)]
        fn id(&self) -> &str {
            "repainter"
        }

        #[zbus(property)]
        fn status(&self) -> &str {
            "Active"
        }

        #[zbus(property)]
        fn category(&self) -> &str {
            "ApplicationStatus"
        }

        #[zbus(property)]
        fn icon_pixmap(&self) -> Vec<StatusNotifierPixmap> {
            let pixel = self.pixel.load(Ordering::Relaxed) as u8;
            vec![(1, 1, vec![0xff, pixel, pixel, pixel])]
        }
    }

    struct MockMenu {
        clicked: Arc<AtomicI32>,
    }

    fn menu_child(
        id: i32,
        label: &str,
        extra: impl IntoIterator<Item = (&'static str, OwnedValue)>,
        children: Vec<OwnedValue>,
    ) -> OwnedValue {
        let string_value = |value: &str| OwnedValue::from(zbus::zvariant::Str::from(value));
        let mut properties = HashMap::from([
            ("label".to_string(), string_value(label)),
            ("visible".to_string(), OwnedValue::from(true)),
            ("enabled".to_string(), OwnedValue::from(true)),
        ]);
        properties.extend(extra.into_iter().map(|(key, value)| (key.into(), value)));
        let structure = zbus::zvariant::Structure::from((id, properties, children));
        OwnedValue::try_from(zbus::zvariant::Value::from(structure)).unwrap()
    }

    #[interface(name = "com.canonical.dbusmenu")]
    impl MockMenu {
        fn about_to_show(&self, _id: i32) -> bool {
            false
        }

        fn get_layout(
            &self,
            _parent_id: i32,
            _recursion_depth: i32,
            _property_names: Vec<String>,
        ) -> (u32, DBusMenuLayout) {
            let checked = menu_child(
                2,
                "_Keep __open",
                [
                    (
                        "toggle-type",
                        OwnedValue::from(zbus::zvariant::Str::from("checkmark")),
                    ),
                    ("toggle-state", OwnedValue::from(1i32)),
                ],
                Vec::new(),
            );
            let submenu = menu_child(
                3,
                "_More",
                [(
                    "children-display",
                    OwnedValue::from(zbus::zvariant::Str::from("submenu")),
                )],
                vec![menu_child(4, "_Child", [], Vec::new())],
            );
            (1, (0, HashMap::new(), vec![checked, submenu]))
        }

        fn event(&self, id: i32, event_id: &str, _data: OwnedValue, _timestamp: u32) {
            if event_id == "clicked" {
                self.clicked.store(id, Ordering::Relaxed);
            }
        }
    }

    #[tokio::test]
    async fn watcher_registers_and_normalizes_a_kde_item() {
        let (_bus, address) = TestBus::spawn();
        let mut bridge = Bridge::start(&address, Config::default(), Arc::new(|| {}))
            .await
            .unwrap();
        let clicked = Arc::new(AtomicI32::new(0));
        let item_connection = zbus::connection::Builder::address(address.as_str())
            .unwrap()
            .name("org.example.BlitFixture")
            .unwrap()
            .serve_at("/StatusNotifierItem", MockItem)
            .unwrap()
            .serve_at(
                "/Menu",
                MockMenu {
                    clicked: clicked.clone(),
                },
            )
            .unwrap()
            .build()
            .await
            .unwrap();
        let watcher = Proxy::new(
            &item_connection,
            "org.kde.StatusNotifierWatcher",
            WATCHER_PATH,
            "org.kde.StatusNotifierWatcher",
        )
        .await
        .unwrap();
        watcher
            .call::<_, _, ()>("RegisterStatusNotifierItem", &("org.example.BlitFixture",))
            .await
            .unwrap();
        let Event::Tray(TrayRecord::Upsert(item)) = next_event(&mut bridge).await else {
            panic!("expected tray upsert")
        };
        assert_eq!(item.app_id, "fixture");
        assert_eq!(item.tooltip_title, "Fixture tip");
        assert_eq!((item.icon.width, item.icon.height), (1, 1));
        assert_ne!(item.flags & TRAY_HAS_MENU, 0);

        assert!(bridge.try_command(Command::Tray(TrayEvent {
            tray_id: item.tray_id,
            kind: TRAY_EVENT_OPEN_MENU,
            menu_revision: 0,
            value: 0,
            flags: 0,
        })));
        let Event::TrayMenu(menu) = next_event(&mut bridge).await else {
            panic!("expected normalized tray menu")
        };
        assert_eq!(menu.status, TRAY_MENU_OK);
        assert_eq!(menu.nodes.len(), 3);
        assert_eq!(menu.nodes[0].label, "Keep _open");
        assert_ne!(menu.nodes[0].flags & MENU_NODE_CHECKMARK, 0);
        assert_eq!(menu.nodes[0].toggle_state, 1);
        assert_ne!(menu.nodes[1].flags & MENU_NODE_SUBMENU, 0);
        assert_eq!(menu.nodes[2].parent_id, 3);

        assert!(bridge.try_command(Command::Tray(TrayEvent {
            tray_id: item.tray_id,
            kind: TRAY_EVENT_MENU_ITEM,
            menu_revision: menu.menu_revision,
            value: 2,
            flags: 0,
        })));
        let Event::TrayMenu(refreshed) = next_event(&mut bridge).await else {
            panic!("expected refreshed tray menu")
        };
        assert_eq!(refreshed.status, TRAY_MENU_OK);
        assert_eq!(clicked.load(Ordering::Relaxed), 2);

        // Keep the connection observably live until normalization completed.
        assert!(item_connection.unique_name().is_some());
    }

    /// Real apps repaint their tray menu while it is on screen — Zoom re-syncs
    /// its language checkmark on every `AboutToShow`, and does so again
    /// whenever its own state changes. The click the user is in the middle of
    /// making has to survive that: the menu they are looking at still names the
    /// item they are pointing at.
    #[tokio::test]
    async fn a_property_update_does_not_swallow_the_click_on_the_open_menu() {
        let (_bus, address) = TestBus::spawn();
        let mut bridge = Bridge::start(&address, Config::default(), Arc::new(|| {}))
            .await
            .unwrap();
        let clicked = Arc::new(AtomicI32::new(0));
        let item_connection = zbus::connection::Builder::address(address.as_str())
            .unwrap()
            .name("org.example.BlitChurn")
            .unwrap()
            .serve_at("/StatusNotifierItem", MockItem)
            .unwrap()
            .serve_at(
                "/Menu",
                MockMenu {
                    clicked: clicked.clone(),
                },
            )
            .unwrap()
            .build()
            .await
            .unwrap();
        let watcher = Proxy::new(
            &item_connection,
            "org.kde.StatusNotifierWatcher",
            WATCHER_PATH,
            "org.kde.StatusNotifierWatcher",
        )
        .await
        .unwrap();
        watcher
            .call::<_, _, ()>("RegisterStatusNotifierItem", &("org.example.BlitChurn",))
            .await
            .unwrap();
        let Event::Tray(TrayRecord::Upsert(item)) = next_event(&mut bridge).await else {
            panic!("expected tray upsert")
        };

        assert!(bridge.try_command(Command::Tray(TrayEvent {
            tray_id: item.tray_id,
            kind: TRAY_EVENT_OPEN_MENU,
            menu_revision: 0,
            value: 0,
            flags: 0,
        })));
        let Event::TrayMenu(menu) = next_event(&mut bridge).await else {
            panic!("expected normalized tray menu")
        };
        assert_eq!(menu.status, TRAY_MENU_OK);

        // The app repaints one item while the menu sits open in front of the
        // user — Zoom's own signal touches its language checkmark, never the
        // Exit the user is reaching for.
        let repaint = async |id: i32| {
            let updated: Vec<(i32, HashMap<String, OwnedValue>)> = vec![(
                id,
                HashMap::from([("toggle-state".to_string(), OwnedValue::from(0i32))]),
            )];
            let removed: Vec<(i32, Vec<String>)> = Vec::new();
            item_connection
                .emit_signal(
                    None::<&str>,
                    "/Menu",
                    "com.canonical.dbusmenu",
                    "ItemsPropertiesUpdated",
                    &(updated, removed),
                )
                .await
                .unwrap();
            sleep(Duration::from_millis(200)).await;
        };

        repaint(4).await;
        assert!(bridge.try_command(Command::Tray(TrayEvent {
            tray_id: item.tray_id,
            kind: TRAY_EVENT_MENU_ITEM,
            menu_revision: menu.menu_revision,
            value: 2,
            flags: 0,
        })));
        let Event::TrayMenu(after) = next_event(&mut bridge).await else {
            panic!("expected a menu event after the click")
        };
        assert_eq!(
            after.status, TRAY_MENU_OK,
            "a repaint elsewhere must not make the open menu stale"
        );
        assert_eq!(
            clicked.load(Ordering::Relaxed),
            2,
            "the click must reach the app"
        );

        // The item that *was* repainted is the one the client now misdescribes,
        // so a click on it is still refused and answered with a fresh menu.
        repaint(4).await;
        assert!(bridge.try_command(Command::Tray(TrayEvent {
            tray_id: item.tray_id,
            kind: TRAY_EVENT_MENU_ITEM,
            menu_revision: after.menu_revision,
            value: 4,
            flags: 0,
        })));
        let Event::TrayMenu(reread) = next_event(&mut bridge).await else {
            panic!("expected a menu event after the refused click")
        };
        assert_eq!(reread.status, TRAY_MENU_OK);
        assert_ne!(reread.menu_revision, after.menu_revision);
        assert_eq!(
            clicked.load(Ordering::Relaxed),
            2,
            "a click on the repainted item must not reach the app"
        );

        assert!(item_connection.unique_name().is_some());
    }

    /// An application which repaints its tray icon — Legcord drawing its unread
    /// badge, Slack its dot — signals `NewIcon` and nothing else. The pixmap the
    /// viewer sees has to follow, so re-reading the item must reach the
    /// application rather than a snapshot taken when it registered.
    #[tokio::test]
    async fn a_new_icon_signal_republishes_the_repainted_pixmap() {
        let (_bus, address) = TestBus::spawn();
        let mut bridge = Bridge::start(&address, Config::default(), Arc::new(|| {}))
            .await
            .unwrap();
        let pixel = Arc::new(AtomicI32::new(0x10));
        let item_connection = zbus::connection::Builder::address(address.as_str())
            .unwrap()
            .name("org.example.BlitRepaint")
            .unwrap()
            .serve_at(
                "/StatusNotifierItem",
                MockRepaintingItem {
                    pixel: pixel.clone(),
                },
            )
            .unwrap()
            .build()
            .await
            .unwrap();
        let watcher = Proxy::new(
            &item_connection,
            "org.kde.StatusNotifierWatcher",
            WATCHER_PATH,
            "org.kde.StatusNotifierWatcher",
        )
        .await
        .unwrap();
        watcher
            .call::<_, _, ()>("RegisterStatusNotifierItem", &("org.example.BlitRepaint",))
            .await
            .unwrap();
        let Event::Tray(TrayRecord::Upsert(first)) = next_event(&mut bridge).await else {
            panic!("expected tray upsert")
        };

        pixel.store(0xf0, Ordering::Relaxed);
        item_connection
            .emit_signal(
                None::<&str>,
                "/StatusNotifierItem",
                "org.kde.StatusNotifierItem",
                "NewIcon",
                &(),
            )
            .await
            .unwrap();
        let Event::Tray(TrayRecord::Upsert(second)) = next_event(&mut bridge).await else {
            panic!("expected a tray upsert for the repainted icon")
        };
        assert_ne!(
            second.icon.png, first.icon.png,
            "NewIcon must republish the pixmap the application now draws"
        );

        assert!(item_connection.unique_name().is_some());
    }
}
