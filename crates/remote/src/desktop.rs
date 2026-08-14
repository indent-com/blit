//! Tray icon and desktop notification wire protocol.
//!
//! Both streams are revisioned state, not fire-and-forget events. A client
//! stages `RESET` chunks and swaps them into view at `SYNC`; live records
//! apply directly. All integers are little-endian.

use std::collections::BTreeMap;

/// `S2C_HELLO` feature bit: the compositor desktop bus has a live bridge.
pub const FEATURE_DESKTOP: u32 = 1 << 21;

pub const C2S_DESKTOP_SUBSCRIBE: u8 = 0x3B;
pub const C2S_TRAY_EVENT: u8 = 0x3C;
pub const C2S_NOTIFICATION_EVENT: u8 = 0x3D;

pub const S2C_TRAY_UPDATE: u8 = 0x32;
pub const S2C_TRAY_MENU: u8 = 0x33;
pub const S2C_NOTIFICATION_UPDATE: u8 = 0x34;

/// Per-message ceiling after desktop LZ4 decompression.
pub const DESKTOP_MAX_DECOMPRESSED: usize = 16 * 1024 * 1024;

pub const DESKTOP_SUBSCRIBE_TRAY: u8 = 1 << 0;
pub const DESKTOP_SUBSCRIBE_NOTIFICATIONS: u8 = 1 << 1;
pub const DESKTOP_SUBSCRIBE_ALL: u8 = DESKTOP_SUBSCRIBE_TRAY | DESKTOP_SUBSCRIBE_NOTIFICATIONS;

pub const DESKTOP_UPDATE_RESET: u8 = 1 << 0;
pub const DESKTOP_UPDATE_SYNC: u8 = 1 << 1;
pub const DESKTOP_UPDATE_REPLAY: u8 = 1 << 2;
pub const DESKTOP_UPDATE_KNOWN: u8 =
    DESKTOP_UPDATE_RESET | DESKTOP_UPDATE_SYNC | DESKTOP_UPDATE_REPLAY;

pub const TRAY_EVENT_ACTIVATE: u8 = 0;
pub const TRAY_EVENT_SECONDARY_ACTIVATE: u8 = 1;
pub const TRAY_EVENT_OPEN_MENU: u8 = 2;
pub const TRAY_EVENT_SCROLL: u8 = 3;
pub const TRAY_EVENT_MENU_ITEM: u8 = 4;
pub const TRAY_EVENT_SCROLL_HORIZONTAL: u8 = 1 << 0;

pub const NOTIFICATION_EVENT_DEFAULT: u8 = 0;
pub const NOTIFICATION_EVENT_ACTION: u8 = 1;
pub const NOTIFICATION_EVENT_DISMISS: u8 = 2;

pub const TRAY_RECORD_UPSERT: u8 = 0x01;
pub const TRAY_RECORD_DELETE: u8 = 0x02;
pub const NOTIFICATION_RECORD_UPSERT: u8 = 0x01;
pub const NOTIFICATION_RECORD_DELETE: u8 = 0x02;

pub const TRAY_STATUS_PASSIVE: u8 = 0;
pub const TRAY_STATUS_ACTIVE: u8 = 1;
pub const TRAY_STATUS_NEEDS_ATTENTION: u8 = 2;

pub const TRAY_CATEGORY_APPLICATION_STATUS: u8 = 0;
pub const TRAY_CATEGORY_COMMUNICATIONS: u8 = 1;
pub const TRAY_CATEGORY_SYSTEM_SERVICE: u8 = 2;
pub const TRAY_CATEGORY_HARDWARE: u8 = 3;
pub const TRAY_CATEGORY_UNKNOWN: u8 = 255;

pub const TRAY_HAS_MENU: u8 = 1 << 0;
pub const TRAY_ITEM_IS_MENU: u8 = 1 << 1;

pub const TRAY_MENU_OK: u8 = 0;
pub const TRAY_MENU_NONE: u8 = 1;
pub const TRAY_MENU_UNAVAILABLE: u8 = 2;
pub const TRAY_MENU_STALE: u8 = 3;

pub const MENU_NODE_VISIBLE: u16 = 1 << 0;
pub const MENU_NODE_ENABLED: u16 = 1 << 1;
pub const MENU_NODE_SEPARATOR: u16 = 1 << 2;
pub const MENU_NODE_SUBMENU: u16 = 1 << 3;
pub const MENU_NODE_CHECKMARK: u16 = 1 << 4;
pub const MENU_NODE_RADIO: u16 = 1 << 5;

pub const NOTIFICATION_RESIDENT: u8 = 1 << 0;
pub const NOTIFICATION_TRANSIENT: u8 = 1 << 1;

pub const NOTIFICATION_URGENCY_LOW: u8 = 0;
pub const NOTIFICATION_URGENCY_NORMAL: u8 = 1;
pub const NOTIFICATION_URGENCY_CRITICAL: u8 = 2;

pub const NOTIFICATION_CLOSED_EXPIRED: u8 = 1;
pub const NOTIFICATION_CLOSED_DISMISSED: u8 = 2;
pub const NOTIFICATION_CLOSED_BY_CALLER: u8 = 3;
pub const NOTIFICATION_CLOSED_UNDEFINED: u8 = 4;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PngImage {
    pub width: u16,
    pub height: u16,
    pub png: Vec<u8>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TrayItem {
    pub tray_id: u32,
    pub revision: u32,
    pub status: u8,
    pub category: u8,
    pub flags: u8,
    pub app_id: String,
    pub title: String,
    pub tooltip_title: String,
    pub tooltip_body: String,
    pub icon: PngImage,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TrayRecord {
    Upsert(TrayItem),
    Delete { tray_id: u32 },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MenuNode {
    pub id: i32,
    pub parent_id: i32,
    pub position: u16,
    pub flags: u16,
    pub toggle_state: i8,
    pub label: String,
    pub icon: PngImage,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TrayMenu {
    pub tray_id: u32,
    pub tray_revision: u32,
    pub menu_revision: u32,
    pub status: u8,
    pub nodes: Vec<MenuNode>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NotificationAction {
    pub key: String,
    pub label: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Notification {
    pub notification_id: u32,
    pub revision: u32,
    pub urgency: u8,
    pub flags: u8,
    pub timeout_ms: u32,
    pub app_name: String,
    pub desktop_entry: String,
    pub summary: String,
    pub body: String,
    pub icon: PngImage,
    pub image: PngImage,
    pub actions: Vec<NotificationAction>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum NotificationRecord {
    Upsert(Notification),
    Delete {
        notification_id: u32,
        revision: u32,
        reason: u8,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TrayEvent {
    pub tray_id: u32,
    pub kind: u8,
    pub menu_revision: u32,
    pub value: i32,
    pub flags: u8,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NotificationEvent {
    pub notification_id: u32,
    pub revision: u32,
    pub kind: u8,
    pub key: String,
}

pub fn msg_desktop_subscribe(flags: u8) -> Vec<u8> {
    vec![C2S_DESKTOP_SUBSCRIBE, flags & DESKTOP_SUBSCRIBE_ALL]
}

pub fn parse_desktop_subscribe(msg: &[u8]) -> Option<u8> {
    (msg.len() == 2 && msg[0] == C2S_DESKTOP_SUBSCRIBE && msg[1] & !DESKTOP_SUBSCRIBE_ALL == 0)
        .then_some(msg[1])
}

pub fn msg_tray_event(event: TrayEvent) -> Vec<u8> {
    let mut msg = Vec::with_capacity(15);
    msg.push(C2S_TRAY_EVENT);
    msg.extend_from_slice(&event.tray_id.to_le_bytes());
    msg.push(event.kind);
    msg.extend_from_slice(&event.menu_revision.to_le_bytes());
    msg.extend_from_slice(&event.value.to_le_bytes());
    msg.push(event.flags);
    msg
}

pub fn parse_tray_event(msg: &[u8]) -> Option<TrayEvent> {
    if msg.len() != 15 || msg[0] != C2S_TRAY_EVENT || msg[5] > TRAY_EVENT_MENU_ITEM {
        return None;
    }
    Some(TrayEvent {
        tray_id: u32::from_le_bytes(msg[1..5].try_into().ok()?),
        kind: msg[5],
        menu_revision: u32::from_le_bytes(msg[6..10].try_into().ok()?),
        value: i32::from_le_bytes(msg[10..14].try_into().ok()?),
        flags: msg[14],
    })
}

pub fn msg_notification_event(event: &NotificationEvent) -> Vec<u8> {
    let key = clip_u16(event.key.as_bytes(), &event.key);
    let mut msg = Vec::with_capacity(12 + key.len());
    msg.push(C2S_NOTIFICATION_EVENT);
    msg.extend_from_slice(&event.notification_id.to_le_bytes());
    msg.extend_from_slice(&event.revision.to_le_bytes());
    msg.push(event.kind);
    msg.extend_from_slice(&(key.len() as u16).to_le_bytes());
    msg.extend_from_slice(key);
    msg
}

pub fn parse_notification_event(msg: &[u8]) -> Option<NotificationEvent> {
    if msg.len() < 12 || msg[0] != C2S_NOTIFICATION_EVENT || msg[9] > NOTIFICATION_EVENT_DISMISS {
        return None;
    }
    let len = u16::from_le_bytes([msg[10], msg[11]]) as usize;
    if msg.len() != 12 + len {
        return None;
    }
    let key = std::str::from_utf8(&msg[12..]).ok()?.to_string();
    if (msg[9] == NOTIFICATION_EVENT_ACTION) != !key.is_empty() {
        return None;
    }
    Some(NotificationEvent {
        notification_id: u32::from_le_bytes(msg[1..5].try_into().ok()?),
        revision: u32::from_le_bytes(msg[5..9].try_into().ok()?),
        kind: msg[9],
        key,
    })
}

fn clip_u16<'a>(bytes: &'a [u8], original: &'a str) -> &'a [u8] {
    if bytes.len() <= u16::MAX as usize {
        return bytes;
    }
    let mut end = u16::MAX as usize;
    while !original.is_char_boundary(end) {
        end -= 1;
    }
    &bytes[..end]
}

fn push_str16(out: &mut Vec<u8>, value: &str) {
    let bytes = clip_u16(value.as_bytes(), value);
    out.extend_from_slice(&(bytes.len() as u16).to_le_bytes());
    out.extend_from_slice(bytes);
}

fn push_str32(out: &mut Vec<u8>, value: &str) {
    let bytes = value.as_bytes();
    let len = bytes.len().min(u32::MAX as usize);
    let mut end = len;
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    out.extend_from_slice(&(end as u32).to_le_bytes());
    out.extend_from_slice(&bytes[..end]);
}

fn push_bytes32(out: &mut Vec<u8>, value: &[u8]) {
    let len = value.len().min(u32::MAX as usize);
    out.extend_from_slice(&(len as u32).to_le_bytes());
    out.extend_from_slice(&value[..len]);
}

fn push_image(out: &mut Vec<u8>, image: &PngImage) {
    out.extend_from_slice(&image.width.to_le_bytes());
    out.extend_from_slice(&image.height.to_le_bytes());
    push_bytes32(out, &image.png);
}

fn push_record(out: &mut Vec<u8>, kind: u8, body: &[u8]) {
    out.push(kind);
    out.extend_from_slice(&(body.len() as u32).to_le_bytes());
    out.extend_from_slice(body);
}

fn encode_tray_records(records: &[TrayRecord]) -> Vec<u8> {
    let count = records.len().min(u16::MAX as usize);
    let mut out = Vec::new();
    out.extend_from_slice(&(count as u16).to_le_bytes());
    for record in &records[..count] {
        match record {
            TrayRecord::Upsert(item) => {
                let mut body = Vec::new();
                body.extend_from_slice(&item.tray_id.to_le_bytes());
                body.extend_from_slice(&item.revision.to_le_bytes());
                body.extend_from_slice(&[item.status, item.category, item.flags]);
                push_str16(&mut body, &item.app_id);
                push_str16(&mut body, &item.title);
                push_str16(&mut body, &item.tooltip_title);
                push_str16(&mut body, &item.tooltip_body);
                push_image(&mut body, &item.icon);
                push_record(&mut out, TRAY_RECORD_UPSERT, &body);
            }
            TrayRecord::Delete { tray_id } => {
                push_record(&mut out, TRAY_RECORD_DELETE, &tray_id.to_le_bytes());
            }
        }
    }
    out
}

fn encode_notification_records(records: &[NotificationRecord]) -> Vec<u8> {
    let count = records.len().min(u16::MAX as usize);
    let mut out = Vec::new();
    out.extend_from_slice(&(count as u16).to_le_bytes());
    for record in &records[..count] {
        match record {
            NotificationRecord::Upsert(item) => {
                let mut body = Vec::new();
                body.extend_from_slice(&item.notification_id.to_le_bytes());
                body.extend_from_slice(&item.revision.to_le_bytes());
                body.extend_from_slice(&[item.urgency, item.flags]);
                body.extend_from_slice(&item.timeout_ms.to_le_bytes());
                push_str16(&mut body, &item.app_name);
                push_str16(&mut body, &item.desktop_entry);
                push_str16(&mut body, &item.summary);
                push_str32(&mut body, &item.body);
                push_image(&mut body, &item.icon);
                push_image(&mut body, &item.image);
                let action_count = item.actions.len().min(u8::MAX as usize);
                body.push(action_count as u8);
                for action in &item.actions[..action_count] {
                    push_str16(&mut body, &action.key);
                    push_str16(&mut body, &action.label);
                }
                push_record(&mut out, NOTIFICATION_RECORD_UPSERT, &body);
            }
            NotificationRecord::Delete {
                notification_id,
                revision,
                reason,
            } => {
                let mut body = Vec::with_capacity(9);
                body.extend_from_slice(&notification_id.to_le_bytes());
                body.extend_from_slice(&revision.to_le_bytes());
                body.push(*reason);
                push_record(&mut out, NOTIFICATION_RECORD_DELETE, &body);
            }
        }
    }
    out
}

pub fn msg_tray_update(flags: u8, records: &[TrayRecord]) -> Vec<u8> {
    let raw = encode_tray_records(records);
    let compressed = lz4_flex::compress_prepend_size(&raw);
    let mut msg = Vec::with_capacity(2 + compressed.len());
    msg.extend_from_slice(&[S2C_TRAY_UPDATE, flags & DESKTOP_UPDATE_KNOWN]);
    msg.extend_from_slice(&compressed);
    msg
}

pub fn msg_notification_update(flags: u8, records: &[NotificationRecord]) -> Vec<u8> {
    let raw = encode_notification_records(records);
    let compressed = lz4_flex::compress_prepend_size(&raw);
    let mut msg = Vec::with_capacity(2 + compressed.len());
    msg.extend_from_slice(&[S2C_NOTIFICATION_UPDATE, flags & DESKTOP_UPDATE_KNOWN]);
    msg.extend_from_slice(&compressed);
    msg
}

fn snapshot_messages<T>(
    records: &[T],
    encode: fn(&[T]) -> Vec<u8>,
    build: fn(u8, &[T]) -> Vec<u8>,
) -> Option<Vec<Vec<u8>>> {
    if records.is_empty() {
        return Some(vec![build(
            DESKTOP_UPDATE_RESET | DESKTOP_UPDATE_SYNC | DESKTOP_UPDATE_REPLAY,
            records,
        )]);
    }

    let mut ranges = Vec::new();
    let mut start = 0;
    let mut chunk_len = 2; // record count
    for index in 0..records.len() {
        let encoded = encode(&records[index..=index]);
        let record_len = encoded.len().checked_sub(2)?;
        if 2 + record_len > DESKTOP_MAX_DECOMPRESSED {
            return None;
        }
        if chunk_len + record_len > DESKTOP_MAX_DECOMPRESSED {
            ranges.push(start..index);
            start = index;
            chunk_len = 2;
        }
        chunk_len += record_len;
    }
    ranges.push(start..records.len());

    let last = ranges.len() - 1;
    Some(
        ranges
            .into_iter()
            .enumerate()
            .map(|(index, range)| {
                let mut flags = DESKTOP_UPDATE_REPLAY;
                if index == 0 {
                    flags |= DESKTOP_UPDATE_RESET;
                }
                if index == last {
                    flags |= DESKTOP_UPDATE_SYNC;
                }
                build(flags, &records[range])
            })
            .collect(),
    )
}

/// Build a bounded staged tray snapshot, splitting before the desktop
/// decompression ceiling when necessary.
pub fn msg_tray_snapshot(records: &[TrayRecord]) -> Option<Vec<Vec<u8>>> {
    snapshot_messages(records, encode_tray_records, msg_tray_update)
}

/// Build a bounded staged notification snapshot, splitting before the
/// desktop decompression ceiling when necessary.
pub fn msg_notification_snapshot(records: &[NotificationRecord]) -> Option<Vec<Vec<u8>>> {
    snapshot_messages(
        records,
        encode_notification_records,
        msg_notification_update,
    )
}

pub fn msg_tray_menu(menu: &TrayMenu) -> Vec<u8> {
    let count = menu.nodes.len().min(u16::MAX as usize);
    let mut raw = Vec::new();
    raw.extend_from_slice(&(count as u16).to_le_bytes());
    for node in &menu.nodes[..count] {
        raw.extend_from_slice(&node.id.to_le_bytes());
        raw.extend_from_slice(&node.parent_id.to_le_bytes());
        raw.extend_from_slice(&node.position.to_le_bytes());
        raw.extend_from_slice(&node.flags.to_le_bytes());
        raw.push(node.toggle_state as u8);
        push_str16(&mut raw, &node.label);
        push_image(&mut raw, &node.icon);
    }
    let compressed = lz4_flex::compress_prepend_size(&raw);
    let mut msg = Vec::with_capacity(14 + compressed.len());
    msg.push(S2C_TRAY_MENU);
    msg.extend_from_slice(&menu.tray_id.to_le_bytes());
    msg.extend_from_slice(&menu.tray_revision.to_le_bytes());
    msg.extend_from_slice(&menu.menu_revision.to_le_bytes());
    msg.push(menu.status);
    msg.extend_from_slice(&compressed);
    msg
}

fn decompress_guarded(data: &[u8]) -> Option<Vec<u8>> {
    let declared = u32::from_le_bytes(data.get(..4)?.try_into().ok()?) as usize;
    if declared > DESKTOP_MAX_DECOMPRESSED {
        return None;
    }
    lz4_flex::decompress_size_prepended(data).ok()
}

struct Reader<'a> {
    data: &'a [u8],
    offset: usize,
}

impl<'a> Reader<'a> {
    fn new(data: &'a [u8]) -> Self {
        Self { data, offset: 0 }
    }

    fn take(&mut self, len: usize) -> Option<&'a [u8]> {
        let end = self.offset.checked_add(len)?;
        let value = self.data.get(self.offset..end)?;
        self.offset = end;
        Some(value)
    }

    fn u8(&mut self) -> Option<u8> {
        Some(self.take(1)?[0])
    }

    fn i8(&mut self) -> Option<i8> {
        Some(self.u8()? as i8)
    }

    fn u16(&mut self) -> Option<u16> {
        Some(u16::from_le_bytes(self.take(2)?.try_into().ok()?))
    }

    fn u32(&mut self) -> Option<u32> {
        Some(u32::from_le_bytes(self.take(4)?.try_into().ok()?))
    }

    fn i32(&mut self) -> Option<i32> {
        Some(i32::from_le_bytes(self.take(4)?.try_into().ok()?))
    }

    fn str16(&mut self) -> Option<String> {
        let len = self.u16()? as usize;
        Some(std::str::from_utf8(self.take(len)?).ok()?.to_string())
    }

    fn str32(&mut self) -> Option<String> {
        let len = self.u32()? as usize;
        Some(std::str::from_utf8(self.take(len)?).ok()?.to_string())
    }

    fn bytes32(&mut self) -> Option<Vec<u8>> {
        let len = self.u32()? as usize;
        Some(self.take(len)?.to_vec())
    }

    fn image(&mut self) -> Option<PngImage> {
        Some(PngImage {
            width: self.u16()?,
            height: self.u16()?,
            png: self.bytes32()?,
        })
    }

    fn done(&self) -> bool {
        self.offset == self.data.len()
    }
}

fn decode_records<T>(
    data: &[u8],
    mut decode: impl FnMut(u8, &[u8]) -> Option<Option<T>>,
) -> Option<Vec<T>> {
    let mut r = Reader::new(data);
    let count = r.u16()? as usize;
    let mut records = Vec::with_capacity(count.min(4096));
    for _ in 0..count {
        let kind = r.u8()?;
        let len = r.u32()? as usize;
        let body = r.take(len)?;
        if let Some(record) = decode(kind, body)? {
            records.push(record);
        }
    }
    r.done().then_some(records)
}

fn decode_tray_record(kind: u8, body: &[u8]) -> Option<Option<TrayRecord>> {
    let mut r = Reader::new(body);
    let record = match kind {
        TRAY_RECORD_UPSERT => Some(TrayRecord::Upsert(TrayItem {
            tray_id: r.u32()?,
            revision: r.u32()?,
            status: r.u8()?,
            category: r.u8()?,
            flags: r.u8()?,
            app_id: r.str16()?,
            title: r.str16()?,
            tooltip_title: r.str16()?,
            tooltip_body: r.str16()?,
            icon: r.image()?,
        })),
        TRAY_RECORD_DELETE => Some(TrayRecord::Delete { tray_id: r.u32()? }),
        _ => return Some(None),
    };
    r.done().then_some(record)
}

fn decode_notification_record(kind: u8, body: &[u8]) -> Option<Option<NotificationRecord>> {
    let mut r = Reader::new(body);
    let record = match kind {
        NOTIFICATION_RECORD_UPSERT => {
            let notification_id = r.u32()?;
            let revision = r.u32()?;
            let urgency = r.u8()?;
            let flags = r.u8()?;
            let timeout_ms = r.u32()?;
            let app_name = r.str16()?;
            let desktop_entry = r.str16()?;
            let summary = r.str16()?;
            let body = r.str32()?;
            let icon = r.image()?;
            let image = r.image()?;
            let action_count = r.u8()? as usize;
            let mut actions = Vec::with_capacity(action_count);
            for _ in 0..action_count {
                actions.push(NotificationAction {
                    key: r.str16()?,
                    label: r.str16()?,
                });
            }
            Some(NotificationRecord::Upsert(Notification {
                notification_id,
                revision,
                urgency,
                flags,
                timeout_ms,
                app_name,
                desktop_entry,
                summary,
                body,
                icon,
                image,
                actions,
            }))
        }
        NOTIFICATION_RECORD_DELETE => Some(NotificationRecord::Delete {
            notification_id: r.u32()?,
            revision: r.u32()?,
            reason: r.u8()?,
        }),
        _ => return Some(None),
    };
    r.done().then_some(record)
}

pub fn parse_tray_update(msg: &[u8]) -> Option<(u8, Vec<TrayRecord>)> {
    if msg.len() < 6 || msg[0] != S2C_TRAY_UPDATE || msg[1] & !DESKTOP_UPDATE_KNOWN != 0 {
        return None;
    }
    let raw = decompress_guarded(&msg[2..])?;
    Some((msg[1], decode_records(&raw, decode_tray_record)?))
}

pub fn parse_notification_update(msg: &[u8]) -> Option<(u8, Vec<NotificationRecord>)> {
    if msg.len() < 6 || msg[0] != S2C_NOTIFICATION_UPDATE || msg[1] & !DESKTOP_UPDATE_KNOWN != 0 {
        return None;
    }
    let raw = decompress_guarded(&msg[2..])?;
    Some((msg[1], decode_records(&raw, decode_notification_record)?))
}

pub fn parse_tray_menu(msg: &[u8]) -> Option<TrayMenu> {
    if msg.len() < 18 || msg[0] != S2C_TRAY_MENU {
        return None;
    }
    let raw = decompress_guarded(&msg[14..])?;
    let mut r = Reader::new(&raw);
    let count = r.u16()? as usize;
    let mut nodes = Vec::with_capacity(count.min(2048));
    for _ in 0..count {
        nodes.push(MenuNode {
            id: r.i32()?,
            parent_id: r.i32()?,
            position: r.u16()?,
            flags: r.u16()?,
            toggle_state: r.i8()?,
            label: r.str16()?,
            icon: r.image()?,
        });
    }
    if !r.done() {
        return None;
    }
    Some(TrayMenu {
        tray_id: u32::from_le_bytes(msg[1..5].try_into().ok()?),
        tray_revision: u32::from_le_bytes(msg[5..9].try_into().ok()?),
        menu_revision: u32::from_le_bytes(msg[9..13].try_into().ok()?),
        status: msg[13],
        nodes,
    })
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DesktopMirror {
    pub tray: BTreeMap<u32, TrayItem>,
    pub notifications: BTreeMap<u32, Notification>,
    tray_staging: Option<BTreeMap<u32, TrayItem>>,
    notification_staging: Option<BTreeMap<u32, Notification>>,
}

impl DesktopMirror {
    pub fn apply_tray_update(&mut self, msg: &[u8]) -> Option<u8> {
        let (flags, records) = parse_tray_update(msg)?;
        if flags & DESKTOP_UPDATE_RESET != 0 {
            self.tray_staging = Some(BTreeMap::new());
        }
        let target = self.tray_staging.as_mut().unwrap_or(&mut self.tray);
        for record in records {
            match record {
                TrayRecord::Upsert(item) => {
                    target.insert(item.tray_id, item);
                }
                TrayRecord::Delete { tray_id } => {
                    target.remove(&tray_id);
                }
            }
        }
        if flags & DESKTOP_UPDATE_SYNC != 0
            && let Some(staged) = self.tray_staging.take()
        {
            self.tray = staged;
        }
        Some(flags)
    }

    pub fn apply_notification_update(&mut self, msg: &[u8]) -> Option<u8> {
        let (flags, records) = parse_notification_update(msg)?;
        if flags & DESKTOP_UPDATE_RESET != 0 {
            self.notification_staging = Some(BTreeMap::new());
        }
        let target = self
            .notification_staging
            .as_mut()
            .unwrap_or(&mut self.notifications);
        for record in records {
            match record {
                NotificationRecord::Upsert(item) => {
                    target.insert(item.notification_id, item);
                }
                NotificationRecord::Delete {
                    notification_id, ..
                } => {
                    target.remove(&notification_id);
                }
            }
        }
        if flags & DESKTOP_UPDATE_SYNC != 0
            && let Some(staged) = self.notification_staging.take()
        {
            self.notifications = staged;
        }
        Some(flags)
    }

    pub fn reset(&mut self) {
        self.tray.clear();
        self.notifications.clear();
        self.tray_staging = None;
        self.notification_staging = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn icon(bytes: &[u8]) -> PngImage {
        PngImage {
            width: 2,
            height: 3,
            png: bytes.to_vec(),
        }
    }

    fn tray(id: u32) -> TrayItem {
        TrayItem {
            tray_id: id,
            revision: 7,
            status: TRAY_STATUS_ACTIVE,
            category: TRAY_CATEGORY_COMMUNICATIONS,
            flags: TRAY_HAS_MENU,
            app_id: "chat".into(),
            title: "Chat".into(),
            tooltip_title: "Unread".into(),
            tooltip_body: "Two messages".into(),
            icon: icon(&[1, 2, 3]),
        }
    }

    fn notification(id: u32) -> Notification {
        Notification {
            notification_id: id,
            revision: 9,
            urgency: NOTIFICATION_URGENCY_NORMAL,
            flags: NOTIFICATION_RESIDENT,
            timeout_ms: 10_000,
            app_name: "Chat".into(),
            desktop_entry: "chat.desktop".into(),
            summary: "Message".into(),
            body: "Hello".into(),
            icon: icon(&[4, 5]),
            image: PngImage::default(),
            actions: vec![NotificationAction {
                key: "default".into(),
                label: "Open".into(),
            }],
        }
    }

    #[test]
    fn client_messages_roundtrip_and_reject_bad_keys() {
        assert_eq!(
            parse_desktop_subscribe(&msg_desktop_subscribe(DESKTOP_SUBSCRIBE_ALL)),
            Some(DESKTOP_SUBSCRIBE_ALL)
        );
        assert_eq!(parse_desktop_subscribe(&[C2S_DESKTOP_SUBSCRIBE, 4]), None);

        let tray = TrayEvent {
            tray_id: 12,
            kind: TRAY_EVENT_SCROLL,
            menu_revision: 4,
            value: -120,
            flags: TRAY_EVENT_SCROLL_HORIZONTAL,
        };
        assert_eq!(parse_tray_event(&msg_tray_event(tray)), Some(tray));

        let action = NotificationEvent {
            notification_id: 3,
            revision: 8,
            kind: NOTIFICATION_EVENT_ACTION,
            key: "reply".into(),
        };
        assert_eq!(
            parse_notification_event(&msg_notification_event(&action)),
            Some(action)
        );
        let mut bad = msg_notification_event(&NotificationEvent {
            notification_id: 3,
            revision: 8,
            kind: NOTIFICATION_EVENT_DEFAULT,
            key: String::new(),
        });
        bad.extend_from_slice(b"x");
        assert_eq!(parse_notification_event(&bad), None);
    }

    #[test]
    fn update_codecs_roundtrip_unknown_records_and_guard_sizes() {
        let tray_records = vec![
            TrayRecord::Upsert(tray(2)),
            TrayRecord::Delete { tray_id: 1 },
        ];
        let msg = msg_tray_update(DESKTOP_UPDATE_REPLAY, &tray_records);
        assert_eq!(
            parse_tray_update(&msg),
            Some((DESKTOP_UPDATE_REPLAY, tray_records))
        );

        let notification_records = vec![
            NotificationRecord::Upsert(notification(4)),
            NotificationRecord::Delete {
                notification_id: 3,
                revision: 2,
                reason: NOTIFICATION_CLOSED_EXPIRED,
            },
        ];
        let msg = msg_notification_update(0, &notification_records);
        assert_eq!(
            parse_notification_update(&msg),
            Some((0, notification_records))
        );

        // Unknown length-framed records are skipped without hiding later ones.
        let mut raw = Vec::new();
        raw.extend_from_slice(&2u16.to_le_bytes());
        push_record(&mut raw, 0x7f, &[1, 2, 3]);
        push_record(&mut raw, TRAY_RECORD_DELETE, &44u32.to_le_bytes());
        let compressed = lz4_flex::compress_prepend_size(&raw);
        let mut msg = vec![S2C_TRAY_UPDATE, 0];
        msg.extend_from_slice(&compressed);
        assert_eq!(
            parse_tray_update(&msg),
            Some((0, vec![TrayRecord::Delete { tray_id: 44 }]))
        );

        let mut oversized = vec![S2C_TRAY_UPDATE, 0];
        oversized.extend_from_slice(&(DESKTOP_MAX_DECOMPRESSED as u32 + 1).to_le_bytes());
        oversized.push(0);
        assert_eq!(parse_tray_update(&oversized), None);
    }

    #[test]
    fn menu_roundtrip() {
        let menu = TrayMenu {
            tray_id: 8,
            tray_revision: 4,
            menu_revision: 3,
            status: TRAY_MENU_OK,
            nodes: vec![MenuNode {
                id: -2,
                parent_id: 0,
                position: 1,
                flags: MENU_NODE_VISIBLE | MENU_NODE_ENABLED | MENU_NODE_CHECKMARK,
                toggle_state: 1,
                label: "_Pause".into(),
                icon: icon(&[9]),
            }],
        };
        assert_eq!(parse_tray_menu(&msg_tray_menu(&menu)), Some(menu));
    }

    #[test]
    fn notification_snapshots_chunk_at_the_desktop_limit() {
        let mut first = notification(1);
        first.body = "a".repeat(DESKTOP_MAX_DECOMPRESSED / 2 + 1);
        let mut second = notification(2);
        second.body = "b".repeat(DESKTOP_MAX_DECOMPRESSED / 2 + 1);
        let messages = msg_notification_snapshot(&[
            NotificationRecord::Upsert(first),
            NotificationRecord::Upsert(second),
        ])
        .unwrap();
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0][1], DESKTOP_UPDATE_RESET | DESKTOP_UPDATE_REPLAY);
        assert_eq!(messages[1][1], DESKTOP_UPDATE_SYNC | DESKTOP_UPDATE_REPLAY);
        assert!(messages.iter().all(|message| {
            u32::from_le_bytes(message[2..6].try_into().unwrap()) as usize
                <= DESKTOP_MAX_DECOMPRESSED
        }));
    }

    #[test]
    fn mirror_stages_snapshots_and_applies_live_updates() {
        let mut mirror = DesktopMirror::default();
        mirror.tray.insert(99, tray(99));
        mirror.apply_tray_update(&msg_tray_update(
            DESKTOP_UPDATE_RESET | DESKTOP_UPDATE_REPLAY,
            &[TrayRecord::Upsert(tray(1))],
        ));
        assert!(mirror.tray.contains_key(&99));
        mirror.apply_tray_update(&msg_tray_update(
            DESKTOP_UPDATE_SYNC | DESKTOP_UPDATE_REPLAY,
            &[TrayRecord::Upsert(tray(2))],
        ));
        assert_eq!(mirror.tray.keys().copied().collect::<Vec<_>>(), vec![1, 2]);

        mirror.apply_notification_update(&msg_notification_update(
            DESKTOP_UPDATE_RESET | DESKTOP_UPDATE_SYNC | DESKTOP_UPDATE_REPLAY,
            &[NotificationRecord::Upsert(notification(7))],
        ));
        mirror.apply_notification_update(&msg_notification_update(
            0,
            &[NotificationRecord::Delete {
                notification_id: 7,
                revision: 9,
                reason: NOTIFICATION_CLOSED_DISMISSED,
            }],
        ));
        assert!(mirror.notifications.is_empty());
    }
}
