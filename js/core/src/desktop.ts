import { fsCompressLiteral, fsDecompress } from "./fs";
import { Notifier, type ReactiveStore } from "./reactive";

/** `S2C_HELLO` feature bit: the compositor desktop bus has a live bridge. */
export const FEATURE_DESKTOP = 1 << 21;

export const C2S_DESKTOP_SUBSCRIBE = 0x3b;
export const C2S_TRAY_EVENT = 0x3c;
export const C2S_NOTIFICATION_EVENT = 0x3d;
export const S2C_TRAY_UPDATE = 0x32;
export const S2C_TRAY_MENU = 0x33;
export const S2C_NOTIFICATION_UPDATE = 0x34;
export const DESKTOP_MAX_DECOMPRESSED = 16 * 1024 * 1024;

export const DESKTOP_SUBSCRIBE_TRAY = 1 << 0;
export const DESKTOP_SUBSCRIBE_NOTIFICATIONS = 1 << 1;
export const DESKTOP_SUBSCRIBE_ALL =
  DESKTOP_SUBSCRIBE_TRAY | DESKTOP_SUBSCRIBE_NOTIFICATIONS;

export const DESKTOP_UPDATE_RESET = 1 << 0;
export const DESKTOP_UPDATE_SYNC = 1 << 1;
export const DESKTOP_UPDATE_REPLAY = 1 << 2;

export const TRAY_EVENT_ACTIVATE = 0;
export const TRAY_EVENT_SECONDARY_ACTIVATE = 1;
export const TRAY_EVENT_OPEN_MENU = 2;
export const TRAY_EVENT_SCROLL = 3;
export const TRAY_EVENT_MENU_ITEM = 4;
export const TRAY_EVENT_SCROLL_HORIZONTAL = 1 << 0;

export const NOTIFICATION_EVENT_DEFAULT = 0;
export const NOTIFICATION_EVENT_ACTION = 1;
export const NOTIFICATION_EVENT_DISMISS = 2;

export const TRAY_STATUS_PASSIVE = 0;
export const TRAY_STATUS_ACTIVE = 1;
export const TRAY_STATUS_NEEDS_ATTENTION = 2;
export const TRAY_HAS_MENU = 1 << 0;
export const TRAY_ITEM_IS_MENU = 1 << 1;

export const TRAY_MENU_OK = 0;
export const TRAY_MENU_NONE = 1;
export const TRAY_MENU_UNAVAILABLE = 2;
export const TRAY_MENU_STALE = 3;

export const MENU_NODE_VISIBLE = 1 << 0;
export const MENU_NODE_ENABLED = 1 << 1;
export const MENU_NODE_SEPARATOR = 1 << 2;
export const MENU_NODE_SUBMENU = 1 << 3;
export const MENU_NODE_CHECKMARK = 1 << 4;
export const MENU_NODE_RADIO = 1 << 5;

export const NOTIFICATION_RESIDENT = 1 << 0;
export const NOTIFICATION_TRANSIENT = 1 << 1;

export const NOTIFICATION_CLOSED_EXPIRED = 1;
export const NOTIFICATION_CLOSED_DISMISSED = 2;
export const NOTIFICATION_CLOSED_BY_CALLER = 3;
export const NOTIFICATION_CLOSED_UNDEFINED = 4;

export interface DesktopImage {
  width: number;
  height: number;
  png: Uint8Array;
}

export interface TrayItem {
  trayId: number;
  revision: number;
  status: number;
  category: number;
  flags: number;
  appId: string;
  title: string;
  tooltipTitle: string;
  tooltipBody: string;
  icon: DesktopImage;
}

export interface TrayMenuNode {
  id: number;
  parentId: number;
  position: number;
  flags: number;
  toggleState: number;
  label: string;
  icon: DesktopImage;
}

export interface TrayMenu {
  trayId: number;
  trayRevision: number;
  menuRevision: number;
  status: number;
  nodes: readonly TrayMenuNode[];
}

export interface NotificationAction {
  key: string;
  label: string;
}

export interface DesktopNotification {
  notificationId: number;
  revision: number;
  urgency: number;
  flags: number;
  timeoutMs: number;
  appName: string;
  desktopEntry: string;
  summary: string;
  body: string;
  icon: DesktopImage;
  image: DesktopImage;
  actions: readonly NotificationAction[];
}

export type TrayRecord =
  | { kind: "upsert"; item: TrayItem }
  | { kind: "delete"; trayId: number };

export type NotificationRecord =
  | { kind: "upsert"; item: DesktopNotification }
  | {
      kind: "delete";
      notificationId: number;
      revision: number;
      reason: number;
    };

const utf8 = new TextDecoder("utf-8", { fatal: true });
const encoder = new TextEncoder();

function encodeString16(value: string): Uint8Array {
  const encoded = encoder.encode(value);
  let end = Math.min(encoded.length, 0xffff);
  while (end > 0 && end < encoded.length && (encoded[end] & 0xc0) === 0x80) {
    end--;
  }
  return encoded.subarray(0, end);
}

function decompressDesktop(data: Uint8Array): Uint8Array | null {
  if (data.length < 4) return null;
  const declared =
    (data[0] | (data[1] << 8) | (data[2] << 16) | (data[3] << 24)) >>> 0;
  if (declared > DESKTOP_MAX_DECOMPRESSED) return null;
  return fsDecompress(data);
}

class Reader {
  readonly #data: Uint8Array;
  readonly #view: DataView;
  offset = 0;

  constructor(data: Uint8Array) {
    this.#data = data;
    this.#view = new DataView(data.buffer, data.byteOffset, data.byteLength);
  }

  take(length: number): Uint8Array {
    const end = this.offset + length;
    if (!Number.isSafeInteger(end) || length < 0 || end > this.#data.length) {
      throw new Error("desktop record overrun");
    }
    const value = this.#data.subarray(this.offset, end);
    this.offset = end;
    return value;
  }

  u8(): number {
    return this.take(1)[0]!;
  }

  i8(): number {
    const value = this.u8();
    return value > 127 ? value - 256 : value;
  }

  u16(): number {
    const value = this.#view.getUint16(this.offset, true);
    this.take(2);
    return value;
  }

  u32(): number {
    const value = this.#view.getUint32(this.offset, true);
    this.take(4);
    return value;
  }

  i32(): number {
    const value = this.#view.getInt32(this.offset, true);
    this.take(4);
    return value;
  }

  string16(): string {
    return utf8.decode(this.take(this.u16()));
  }

  string32(): string {
    return utf8.decode(this.take(this.u32()));
  }

  bytes32(): Uint8Array {
    return this.take(this.u32()).slice();
  }

  image(): DesktopImage {
    return { width: this.u16(), height: this.u16(), png: this.bytes32() };
  }

  get done(): boolean {
    return this.offset === this.#data.length;
  }
}

function parseRecords<T>(
  data: Uint8Array,
  decode: (kind: number, body: Reader) => T | null,
): T[] | null {
  try {
    const reader = new Reader(data);
    const count = reader.u16();
    const records: T[] = [];
    for (let i = 0; i < count; i++) {
      const kind = reader.u8();
      const body = new Reader(reader.take(reader.u32()));
      const record = decode(kind, body);
      // Unknown kinds are skipped by their outer record length. Known kinds
      // must consume exactly their body so trailing junk cannot shift fields.
      if (record !== null) {
        if (!body.done) return null;
        records.push(record);
      }
    }
    return reader.done ? records : null;
  } catch {
    return null;
  }
}

function parseTrayRecords(data: Uint8Array): TrayRecord[] | null {
  return parseRecords(data, (kind, body) => {
    if (kind === 1) {
      return {
        kind: "upsert",
        item: {
          trayId: body.u32(),
          revision: body.u32(),
          status: body.u8(),
          category: body.u8(),
          flags: body.u8(),
          appId: body.string16(),
          title: body.string16(),
          tooltipTitle: body.string16(),
          tooltipBody: body.string16(),
          icon: body.image(),
        },
      };
    }
    if (kind === 2) return { kind: "delete", trayId: body.u32() };
    return null;
  });
}

function parseNotificationRecords(
  data: Uint8Array,
): NotificationRecord[] | null {
  return parseRecords(data, (kind, body) => {
    if (kind === 1) {
      const item: DesktopNotification = {
        notificationId: body.u32(),
        revision: body.u32(),
        urgency: body.u8(),
        flags: body.u8(),
        timeoutMs: body.u32(),
        appName: body.string16(),
        desktopEntry: body.string16(),
        summary: body.string16(),
        body: body.string32(),
        icon: body.image(),
        image: body.image(),
        actions: [],
      };
      const actions: NotificationAction[] = [];
      const count = body.u8();
      for (let i = 0; i < count; i++) {
        actions.push({ key: body.string16(), label: body.string16() });
      }
      item.actions = actions;
      return { kind: "upsert", item };
    }
    if (kind === 2) {
      return {
        kind: "delete",
        notificationId: body.u32(),
        revision: body.u32(),
        reason: body.u8(),
      };
    }
    return null;
  });
}

export function parseTrayUpdate(
  message: Uint8Array,
): { flags: number; records: TrayRecord[] } | null {
  if (message.length < 6 || message[0] !== S2C_TRAY_UPDATE) return null;
  const flags = message[1]!;
  if (
    flags &
    ~(DESKTOP_UPDATE_RESET | DESKTOP_UPDATE_SYNC | DESKTOP_UPDATE_REPLAY)
  ) {
    return null;
  }
  const data = decompressDesktop(message.subarray(2));
  if (!data) return null;
  const records = parseTrayRecords(data);
  return records ? { flags, records } : null;
}

export function parseNotificationUpdate(
  message: Uint8Array,
): { flags: number; records: NotificationRecord[] } | null {
  if (message.length < 6 || message[0] !== S2C_NOTIFICATION_UPDATE) return null;
  const flags = message[1]!;
  if (
    flags &
    ~(DESKTOP_UPDATE_RESET | DESKTOP_UPDATE_SYNC | DESKTOP_UPDATE_REPLAY)
  ) {
    return null;
  }
  const data = decompressDesktop(message.subarray(2));
  if (!data) return null;
  const records = parseNotificationRecords(data);
  return records ? { flags, records } : null;
}

export function parseTrayMenu(message: Uint8Array): TrayMenu | null {
  if (message.length < 18 || message[0] !== S2C_TRAY_MENU) return null;
  try {
    const view = new DataView(
      message.buffer,
      message.byteOffset,
      message.byteLength,
    );
    const data = decompressDesktop(message.subarray(14));
    if (!data) return null;
    const reader = new Reader(data);
    const count = reader.u16();
    const nodes: TrayMenuNode[] = [];
    for (let i = 0; i < count; i++) {
      nodes.push({
        id: reader.i32(),
        parentId: reader.i32(),
        position: reader.u16(),
        flags: reader.u16(),
        toggleState: reader.i8(),
        label: reader.string16(),
        icon: reader.image(),
      });
    }
    if (!reader.done) return null;
    return {
      trayId: view.getUint32(1, true),
      trayRevision: view.getUint32(5, true),
      menuRevision: view.getUint32(9, true),
      status: message[13]!,
      nodes,
    };
  } catch {
    return null;
  }
}

export function buildDesktopSubscribeMessage(flags: number): Uint8Array {
  return new Uint8Array([C2S_DESKTOP_SUBSCRIBE, flags & DESKTOP_SUBSCRIBE_ALL]);
}

export function buildTrayEventMessage(
  trayId: number,
  kind: number,
  menuRevision = 0,
  value = 0,
  flags = 0,
): Uint8Array {
  const message = new Uint8Array(15);
  const view = new DataView(message.buffer);
  message[0] = C2S_TRAY_EVENT;
  view.setUint32(1, trayId, true);
  message[5] = kind;
  view.setUint32(6, menuRevision, true);
  view.setInt32(10, value, true);
  message[14] = flags;
  return message;
}

export function buildNotificationEventMessage(
  notificationId: number,
  revision: number,
  kind: number,
  key = "",
): Uint8Array {
  const keyBytes = encodeString16(key);
  const message = new Uint8Array(12 + keyBytes.length);
  const view = new DataView(message.buffer);
  message[0] = C2S_NOTIFICATION_EVENT;
  view.setUint32(1, notificationId, true);
  view.setUint32(5, revision, true);
  message[9] = kind;
  view.setUint16(10, keyBytes.length, true);
  message.set(keyBytes, 12);
  return message;
}

// Mock-server helpers. Production servers use the Rust codec's full LZ4
// encoder; literal-only blocks are wire-compatible and keep tests readable.
function pushU16(out: number[], value: number): void {
  out.push(value & 0xff, (value >>> 8) & 0xff);
}

function pushU32(out: number[], value: number): void {
  out.push(
    value & 0xff,
    (value >>> 8) & 0xff,
    (value >>> 16) & 0xff,
    (value >>> 24) & 0xff,
  );
}

function pushString16(out: number[], value: string): void {
  const bytes = encodeString16(value);
  pushU16(out, bytes.length);
  out.push(...bytes);
}

function pushString32(out: number[], value: string): void {
  const bytes = encoder.encode(value);
  pushU32(out, bytes.length);
  out.push(...bytes);
}

function pushImage(out: number[], image: DesktopImage): void {
  pushU16(out, image.width);
  pushU16(out, image.height);
  pushU32(out, image.png.length);
  out.push(...image.png);
}

function pushRecord(out: number[], kind: number, body: number[]): void {
  out.push(kind);
  pushU32(out, body.length);
  out.push(...body);
}

export function buildTrayUpdateMessage(
  flags: number,
  records: readonly TrayRecord[],
): Uint8Array {
  const raw: number[] = [];
  pushU16(raw, records.length);
  for (const record of records) {
    const body: number[] = [];
    if (record.kind === "upsert") {
      const item = record.item;
      pushU32(body, item.trayId);
      pushU32(body, item.revision);
      body.push(item.status, item.category, item.flags);
      pushString16(body, item.appId);
      pushString16(body, item.title);
      pushString16(body, item.tooltipTitle);
      pushString16(body, item.tooltipBody);
      pushImage(body, item.icon);
      pushRecord(raw, 1, body);
    } else {
      pushU32(body, record.trayId);
      pushRecord(raw, 2, body);
    }
  }
  const compressed = fsCompressLiteral(new Uint8Array(raw));
  const message = new Uint8Array(2 + compressed.length);
  message.set([S2C_TRAY_UPDATE, flags], 0);
  message.set(compressed, 2);
  return message;
}

export function buildNotificationUpdateMessage(
  flags: number,
  records: readonly NotificationRecord[],
): Uint8Array {
  const raw: number[] = [];
  pushU16(raw, records.length);
  for (const record of records) {
    const body: number[] = [];
    if (record.kind === "upsert") {
      const item = record.item;
      pushU32(body, item.notificationId);
      pushU32(body, item.revision);
      body.push(item.urgency, item.flags);
      pushU32(body, item.timeoutMs);
      pushString16(body, item.appName);
      pushString16(body, item.desktopEntry);
      pushString16(body, item.summary);
      pushString32(body, item.body);
      pushImage(body, item.icon);
      pushImage(body, item.image);
      const actions = item.actions.slice(0, 0xff);
      body.push(actions.length);
      for (const action of actions) {
        pushString16(body, action.key);
        pushString16(body, action.label);
      }
      pushRecord(raw, 1, body);
    } else {
      pushU32(body, record.notificationId);
      pushU32(body, record.revision);
      body.push(record.reason);
      pushRecord(raw, 2, body);
    }
  }
  const compressed = fsCompressLiteral(new Uint8Array(raw));
  const message = new Uint8Array(2 + compressed.length);
  message.set([S2C_NOTIFICATION_UPDATE, flags], 0);
  message.set(compressed, 2);
  return message;
}

/** Framework-neutral state and semantic input API for one blit connection. */
export class DesktopStore implements ReactiveStore {
  readonly #notifier = new Notifier();
  readonly #tray = new Map<number, TrayItem>();
  readonly #notifications = new Map<number, DesktopNotification>();
  #trayStaging: Map<number, TrayItem> | null = null;
  #notificationStaging: Map<number, DesktopNotification> | null = null;
  #sender: ((message: Uint8Array) => void) | null = null;
  readonly #raised = new Set<(notification: DesktopNotification) => void>();
  readonly #menus = new Set<(menu: TrayMenu) => void>();

  get revision(): number {
    return this.#notifier.revision;
  }

  subscribe = (listener: () => void): (() => void) =>
    this.#notifier.subscribe(listener);

  get tray(): ReadonlyMap<number, TrayItem> {
    return this.#tray;
  }

  get notifications(): ReadonlyMap<number, DesktopNotification> {
    return this.#notifications;
  }

  setSender(sender: ((message: Uint8Array) => void) | null): void {
    this.#sender = sender;
  }

  onNotificationRaised(
    listener: (notification: DesktopNotification) => void,
  ): () => void {
    this.#raised.add(listener);
    return () => this.#raised.delete(listener);
  }

  onTrayMenu(listener: (menu: TrayMenu) => void): () => void {
    this.#menus.add(listener);
    return () => this.#menus.delete(listener);
  }

  handleTrayUpdate(message: Uint8Array): boolean {
    const update = parseTrayUpdate(message);
    if (!update) return false;
    if (update.flags & DESKTOP_UPDATE_RESET) this.#trayStaging = new Map();
    const target = this.#trayStaging ?? this.#tray;
    for (const record of update.records) {
      if (record.kind === "upsert") target.set(record.item.trayId, record.item);
      else target.delete(record.trayId);
    }
    if (update.flags & DESKTOP_UPDATE_SYNC && this.#trayStaging) {
      this.#tray.clear();
      for (const [id, item] of this.#trayStaging) this.#tray.set(id, item);
      this.#trayStaging = null;
      this.#notifier.emit();
    } else if (!this.#trayStaging && update.records.length > 0) {
      this.#notifier.emit();
    }
    return true;
  }

  handleNotificationUpdate(message: Uint8Array): boolean {
    const update = parseNotificationUpdate(message);
    if (!update) return false;
    if (update.flags & DESKTOP_UPDATE_RESET) {
      this.#notificationStaging = new Map();
    }
    const target = this.#notificationStaging ?? this.#notifications;
    const raised: DesktopNotification[] = [];
    for (const record of update.records) {
      if (record.kind === "upsert") {
        target.set(record.item.notificationId, record.item);
        if (!(update.flags & DESKTOP_UPDATE_REPLAY)) raised.push(record.item);
      } else {
        target.delete(record.notificationId);
      }
    }
    if (update.flags & DESKTOP_UPDATE_SYNC && this.#notificationStaging) {
      this.#notifications.clear();
      for (const [id, item] of this.#notificationStaging) {
        this.#notifications.set(id, item);
      }
      this.#notificationStaging = null;
      this.#notifier.emit();
    } else if (!this.#notificationStaging && update.records.length > 0) {
      this.#notifier.emit();
    }
    for (const item of raised) {
      for (const listener of [...this.#raised]) listener(item);
    }
    return true;
  }

  handleTrayMenu(message: Uint8Array): boolean {
    const menu = parseTrayMenu(message);
    if (!menu) return false;
    for (const listener of [...this.#menus]) listener(menu);
    return true;
  }

  subscribeDesktop(flags = DESKTOP_SUBSCRIBE_ALL): void {
    this.#sender?.(buildDesktopSubscribeMessage(flags));
  }

  activate(trayId: number): void {
    this.#sender?.(buildTrayEventMessage(trayId, TRAY_EVENT_ACTIVATE));
  }

  secondaryActivate(trayId: number): void {
    this.#sender?.(
      buildTrayEventMessage(trayId, TRAY_EVENT_SECONDARY_ACTIVATE),
    );
  }

  openMenu(trayId: number, menuRevision = 0, parentId = 0): void {
    this.#sender?.(
      buildTrayEventMessage(
        trayId,
        TRAY_EVENT_OPEN_MENU,
        menuRevision,
        parentId,
      ),
    );
  }

  scroll(trayId: number, delta: number, horizontal = false): void {
    this.#sender?.(
      buildTrayEventMessage(
        trayId,
        TRAY_EVENT_SCROLL,
        0,
        delta,
        horizontal ? TRAY_EVENT_SCROLL_HORIZONTAL : 0,
      ),
    );
  }

  clickMenuItem(trayId: number, menuRevision: number, itemId: number): void {
    this.#sender?.(
      buildTrayEventMessage(trayId, TRAY_EVENT_MENU_ITEM, menuRevision, itemId),
    );
  }

  invokeDefault(notificationId: number, revision: number): void {
    this.#sender?.(
      buildNotificationEventMessage(
        notificationId,
        revision,
        NOTIFICATION_EVENT_DEFAULT,
      ),
    );
  }

  invokeAction(notificationId: number, revision: number, key: string): void {
    if (!key) return;
    this.#sender?.(
      buildNotificationEventMessage(
        notificationId,
        revision,
        NOTIFICATION_EVENT_ACTION,
        key,
      ),
    );
  }

  dismiss(notificationId: number, revision: number): void {
    this.#sender?.(
      buildNotificationEventMessage(
        notificationId,
        revision,
        NOTIFICATION_EVENT_DISMISS,
      ),
    );
  }

  reset(): void {
    const changed = this.#tray.size > 0 || this.#notifications.size > 0;
    this.#tray.clear();
    this.#notifications.clear();
    this.#trayStaging = null;
    this.#notificationStaging = null;
    if (changed) this.#notifier.emit();
  }
}
