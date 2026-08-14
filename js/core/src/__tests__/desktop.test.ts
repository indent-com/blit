import { describe, expect, it, vi } from "vitest";
import {
  DESKTOP_SUBSCRIBE_ALL,
  DESKTOP_UPDATE_REPLAY,
  DESKTOP_UPDATE_RESET,
  DESKTOP_UPDATE_SYNC,
  DesktopStore,
  NOTIFICATION_EVENT_ACTION,
  NOTIFICATION_RESIDENT,
  S2C_TRAY_UPDATE,
  TRAY_EVENT_SCROLL,
  TRAY_EVENT_SCROLL_HORIZONTAL,
  TRAY_HAS_MENU,
  TRAY_STATUS_ACTIVE,
  buildDesktopSubscribeMessage,
  buildNotificationEventMessage,
  buildNotificationUpdateMessage,
  buildTrayEventMessage,
  buildTrayUpdateMessage,
  parseNotificationUpdate,
  parseTrayUpdate,
  type DesktopNotification,
  type TrayItem,
} from "../desktop";

const icon = (
  bytes: number[] = [],
): { width: number; height: number; png: Uint8Array } => ({
  width: bytes.length ? 2 : 0,
  height: bytes.length ? 3 : 0,
  png: new Uint8Array(bytes),
});

const tray = (trayId: number): TrayItem => ({
  trayId,
  revision: 7,
  status: TRAY_STATUS_ACTIVE,
  category: 1,
  flags: TRAY_HAS_MENU,
  appId: "chat",
  title: "Chat",
  tooltipTitle: "Unread",
  tooltipBody: "Two messages",
  icon: icon([1, 2, 3]),
});

const notification = (notificationId: number): DesktopNotification => ({
  notificationId,
  revision: 9,
  urgency: 1,
  flags: NOTIFICATION_RESIDENT,
  timeoutMs: 10_000,
  appName: "Chat",
  desktopEntry: "chat.desktop",
  summary: "Message",
  body: "Hello",
  icon: icon([4, 5]),
  image: icon(),
  actions: [{ key: "default", label: "Open" }],
});

describe("desktop wire format", () => {
  it("builds the locked C2S layouts", () => {
    expect(
      Array.from(buildDesktopSubscribeMessage(DESKTOP_SUBSCRIBE_ALL)),
    ).toEqual([0x3b, 3]);
    expect(
      Array.from(
        buildTrayEventMessage(
          12,
          TRAY_EVENT_SCROLL,
          4,
          -120,
          TRAY_EVENT_SCROLL_HORIZONTAL,
        ),
      ),
    ).toEqual([0x3c, 12, 0, 0, 0, 3, 4, 0, 0, 0, 0x88, 0xff, 0xff, 0xff, 1]);
    expect(
      Array.from(
        buildNotificationEventMessage(3, 8, NOTIFICATION_EVENT_ACTION, "reply"),
      ),
    ).toEqual([
      0x3d, 3, 0, 0, 0, 8, 0, 0, 0, 1, 5, 0, 0x72, 0x65, 0x70, 0x6c, 0x79,
    ]);
  });

  it("roundtrips tray and notification records", () => {
    const trayMessage = buildTrayUpdateMessage(DESKTOP_UPDATE_REPLAY, [
      { kind: "upsert", item: tray(2) },
      { kind: "delete", trayId: 1 },
    ]);
    expect(parseTrayUpdate(trayMessage)).toEqual({
      flags: DESKTOP_UPDATE_REPLAY,
      records: [
        { kind: "upsert", item: tray(2) },
        { kind: "delete", trayId: 1 },
      ],
    });

    const notificationMessage = buildNotificationUpdateMessage(0, [
      { kind: "upsert", item: notification(4) },
      { kind: "delete", notificationId: 3, revision: 2, reason: 1 },
    ]);
    expect(parseNotificationUpdate(notificationMessage)).toEqual({
      flags: 0,
      records: [
        { kind: "upsert", item: notification(4) },
        { kind: "delete", notificationId: 3, revision: 2, reason: 1 },
      ],
    });
  });

  it("rejects malformed or oversized compressed records", () => {
    const invalidFlags = buildTrayUpdateMessage(0, []);
    invalidFlags[1] = 0x80;
    expect(parseTrayUpdate(invalidFlags)).toBeNull();

    const oversized = new Uint8Array([
      S2C_TRAY_UPDATE,
      0,
      1,
      0,
      0,
      1, // declared 16 MiB + 1
      0,
    ]);
    expect(parseTrayUpdate(oversized)).toBeNull();
  });
});

describe("DesktopStore", () => {
  it("stages snapshots and never raises replayed notifications", () => {
    const store = new DesktopStore();
    const changed = vi.fn();
    const raised = vi.fn();
    store.subscribe(changed);
    store.onNotificationRaised(raised);

    store.handleTrayUpdate(
      buildTrayUpdateMessage(DESKTOP_UPDATE_RESET | DESKTOP_UPDATE_REPLAY, [
        { kind: "upsert", item: tray(1) },
      ]),
    );
    expect(store.tray.size).toBe(0);
    expect(changed).not.toHaveBeenCalled();
    store.handleTrayUpdate(
      buildTrayUpdateMessage(DESKTOP_UPDATE_SYNC | DESKTOP_UPDATE_REPLAY, [
        { kind: "upsert", item: tray(2) },
      ]),
    );
    expect([...store.tray.keys()]).toEqual([1, 2]);

    store.handleNotificationUpdate(
      buildNotificationUpdateMessage(
        DESKTOP_UPDATE_RESET | DESKTOP_UPDATE_SYNC | DESKTOP_UPDATE_REPLAY,
        [{ kind: "upsert", item: notification(7) }],
      ),
    );
    expect(store.notifications.get(7)?.summary).toBe("Message");
    expect(raised).not.toHaveBeenCalled();

    store.handleNotificationUpdate(
      buildNotificationUpdateMessage(0, [
        { kind: "upsert", item: notification(8) },
      ]),
    );
    expect(raised).toHaveBeenCalledOnce();
    expect(raised).toHaveBeenCalledWith(notification(8));
  });

  it("sends semantic events through its injected connection sender", () => {
    const store = new DesktopStore();
    const sent: Uint8Array[] = [];
    store.setSender((message) => sent.push(message));
    store.subscribeDesktop();
    store.scroll(12, -120, true);
    store.invokeAction(3, 8, "reply");
    expect(sent).toEqual([
      buildDesktopSubscribeMessage(DESKTOP_SUBSCRIBE_ALL),
      buildTrayEventMessage(
        12,
        TRAY_EVENT_SCROLL,
        0,
        -120,
        TRAY_EVENT_SCROLL_HORIZONTAL,
      ),
      buildNotificationEventMessage(3, 8, NOTIFICATION_EVENT_ACTION, "reply"),
    ]);
  });
});
