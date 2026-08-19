import { describe, expect, it } from "vitest";
import {
  TRAY_HAS_MENU,
  TRAY_ITEM_IS_MENU,
  type DesktopImage,
  type DesktopNotification,
} from "@blit-sh/core";
import {
  desktopDelivery,
  desktopNativeTag,
  canRaiseMpris,
  canSeekMpris,
  desktopNotificationHasDetail,
  matchesDesktopNotification,
  mprisHasProgress,
  mprisSeekTargetUs,
  popupViewportShift,
  portalDialogFocusTarget,
  reconcileMprisSubscriptions,
  samePortalPresentationEntry,
  selectMediaSessionEntry,
  trayPrimaryGesture,
  trayPrimaryOpensMenu,
  type MprisSubscriptionTarget,
} from "../desktopPresentation";

function png(bytes: number): DesktopImage {
  return { width: bytes, height: bytes, png: new Uint8Array(bytes) };
}

function notification(
  overrides: Partial<DesktopNotification> = {},
): DesktopNotification {
  return {
    notificationId: 1,
    revision: 1,
    urgency: 1,
    flags: 0,
    timeoutMs: 0,
    appName: "Brave",
    desktopEntry: "brave",
    summary: "Anniversaire Jo",
    body: "",
    icon: png(0),
    image: png(0),
    actions: [],
    ...overrides,
  };
}

describe("desktop notification presentation", () => {
  it("uses toasts in the foreground and native delivery only when allowed", () => {
    expect(desktopDelivery("visible", "granted")).toBe("toast");
    expect(desktopDelivery("hidden", "granted")).toBe("native");
    expect(desktopDelivery("hidden", "default")).toBe("retain");
    expect(desktopDelivery("hidden", "denied")).toBe("retain");
  });

  it("namespaces native replacement tags by connection and server boot", () => {
    expect(desktopNativeTag("remote:a", 42n, 7)).toBe("blit:remote:a:42:7");
    expect(desktopNativeTag("remote:a", null, 7)).toBeNull();
  });

  it("rejects clicks from a replaced notification revision", () => {
    const item = {
      notificationId: 7,
      revision: 3,
    } as DesktopNotification;
    expect(
      matchesDesktopNotification(item, { notificationId: 7, revision: 3 }),
    ).toBe(true);
    expect(
      matchesDesktopNotification(item, { notificationId: 7, revision: 2 }),
    ).toBe(false);
    expect(
      matchesDesktopNotification(item, { notificationId: 8, revision: 3 }),
    ).toBe(false);
  });

  it("reports detail only when the sender supplied some", () => {
    expect(desktopNotificationHasDetail(notification())).toBe(false);
    expect(desktopNotificationHasDetail(notification({ body: "Aug 15" }))).toBe(
      true,
    );
    expect(desktopNotificationHasDetail(notification({ image: png(1) }))).toBe(
      true,
    );
    expect(
      desktopNotificationHasDetail(
        notification({ actions: [{ key: "settings", label: "Settings" }] }),
      ),
    ).toBe(true);
  });

  it("does not count the default action as detail: it renders no button", () => {
    expect(
      desktopNotificationHasDetail(
        notification({ actions: [{ key: "default", label: "Reply" }] }),
      ),
    ).toBe(false);
    expect(
      desktopNotificationHasDetail(
        notification({
          actions: [
            { key: "default", label: "" },
            { key: "settings", label: "Settings" },
          ],
        }),
      ),
    ).toBe(true);
  });

  it("opens a menu on primary activation only for menu items", () => {
    expect(trayPrimaryOpensMenu(TRAY_HAS_MENU)).toBe(false);
    expect(trayPrimaryOpensMenu(TRAY_ITEM_IS_MENU)).toBe(true);
    expect(trayPrimaryOpensMenu(TRAY_HAS_MENU | TRAY_ITEM_IS_MENU)).toBe(true);
  });

  it("opens an advertised menu directly from a touch tap", () => {
    expect(trayPrimaryOpensMenu(0, true)).toBe(false);
    expect(trayPrimaryOpensMenu(TRAY_HAS_MENU, true)).toBe(true);
    expect(trayPrimaryOpensMenu(TRAY_ITEM_IS_MENU, true)).toBe(true);
  });

  it("ignores the click a long press leaves behind", () => {
    // The press fired `contextmenu` and the menu is already open; activating
    // as well raises the app's window behind the menu being read.
    expect(trayPrimaryGesture(TRAY_HAS_MENU, "touch", true)).toBe("ignore");
    expect(trayPrimaryGesture(TRAY_HAS_MENU, "touch", false)).toBe("menu");
    // A mouse right-click leaves no trailing click behind, so the flag is never
    // set for one and the next real left click still activates.
    expect(trayPrimaryGesture(TRAY_HAS_MENU, "mouse", false)).toBe("activate");
    expect(trayPrimaryGesture(0, null, false)).toBe("activate");
  });

  it("does not toggle stable MPRIS subscriptions on snapshot-only updates", () => {
    const calls: boolean[] = [];
    const first: MprisSubscriptionTarget = {
      subscribe: (enabled) => calls.push(enabled),
    };
    const second: MprisSubscriptionTarget = {
      subscribe: (enabled) => calls.push(enabled),
    };
    const active = new Set<MprisSubscriptionTarget>();

    reconcileMprisSubscriptions(active, [first]);
    reconcileMprisSubscriptions(active, [first]);
    expect(calls).toEqual([true]);

    reconcileMprisSubscriptions(active, [second]);
    expect(calls).toEqual([true, false, true]);
    reconcileMprisSubscriptions(active, []);
    expect(calls).toEqual([true, false, true, false]);
  });

  it("keeps a portal presentation stable only for the same request object", () => {
    const request = {
      kind: "screencast" as const,
      requestId: 7,
      deadlineMs: 10_000,
      parentSurfaceId: null,
      appId: "example",
      multiple: false,
      candidates: [],
    };
    const first = {
      connectionId: "main",
      connectionLabel: "Main",
      readOnly: false,
      request,
    };
    expect(samePortalPresentationEntry(first, { ...first })).toBe(true);
    expect(
      samePortalPresentationEntry(first, {
        ...first,
        request: { ...request },
      }),
    ).toBe(false);
    expect(
      samePortalPresentationEntry(first, { ...first, readOnly: true }),
    ).toBe(false);
  });

  it("cycles portal dialog focus without including disabled controls", () => {
    const dialog = document.createElement("div");
    dialog.tabIndex = -1;
    const first = document.createElement("button");
    const disabled = document.createElement("button");
    disabled.disabled = true;
    const last = document.createElement("select");
    dialog.append(first, disabled, last);

    expect(portalDialogFocusTarget(dialog, dialog, false)).toBe(first);
    expect(portalDialogFocusTarget(dialog, dialog, true)).toBe(last);
    expect(portalDialogFocusTarget(dialog, first, true)).toBe(last);
    expect(portalDialogFocusTarget(dialog, last, false)).toBe(first);
    expect(portalDialogFocusTarget(dialog, first, false)).toBeUndefined();
  });

  it("selects only writable players for the browser Media Session", () => {
    const readOnlyActive = {
      connectionId: "read-only",
      readOnly: true,
      player: { playerId: 1, active: true, playbackStatus: "playing" as const },
    };
    const writableInactive = {
      connectionId: "main",
      readOnly: false,
      player: {
        playerId: 2,
        active: false,
        playbackStatus: "stopped" as const,
      },
    };
    const writableActive = {
      connectionId: "main",
      readOnly: false,
      player: { playerId: 3, active: true, playbackStatus: "paused" as const },
    };
    expect(
      selectMediaSessionEntry([
        readOnlyActive,
        writableInactive,
        writableActive,
      ]),
    ).toBe(writableActive);
    expect(selectMediaSessionEntry([readOnlyActive])).toBeUndefined();
  });

  it("arbitrates Media Session by focus, playing recency, and manual choice", () => {
    const first = {
      connectionId: "first",
      readOnly: false,
      player: { playerId: 1, active: true, playbackStatus: "playing" as const },
    };
    const second = {
      connectionId: "second",
      readOnly: false,
      player: { playerId: 2, active: true, playbackStatus: "playing" as const },
    };
    const order = new Map([
      ["first:1", 2],
      ["second:2", 3],
    ]);

    expect(selectMediaSessionEntry([first, second], "first", order)).toBe(
      first,
    );
    expect(selectMediaSessionEntry([first, second], undefined, order)).toBe(
      second,
    );
    expect(
      selectMediaSessionEntry([first, second], "first", order, "second:2"),
    ).toBe(second);
  });

  it("allows independent CanRaise but never from a read-only connection", () => {
    expect(canRaiseMpris(false, 1 << 6)).toBe(true);
    expect(canRaiseMpris(true, 1 << 6)).toBe(false);
    expect(canRaiseMpris(false, 1 << 0)).toBe(false);
  });
});

describe("MPRIS progress", () => {
  const CONTROL = 1 << 0;
  const SEEK = 1 << 5;

  it("draws a bar only for a track whose length is known", () => {
    expect(mprisHasProgress(180_000_000)).toBe(true);
    // A live stream reports -1, and a player that has not loaded one yet 0.
    expect(mprisHasProgress(-1)).toBe(false);
    expect(mprisHasProgress(0)).toBe(false);
  });

  it("takes a drag only when CanSeek accompanies CanControl", () => {
    expect(canSeekMpris(false, CONTROL | SEEK, 180_000_000)).toBe(true);
    // CanSeek without CanControl is advertised by players that expose their
    // position but refuse commands; the bar still draws, it just does not move.
    expect(canSeekMpris(false, SEEK, 180_000_000)).toBe(false);
    expect(canSeekMpris(false, CONTROL, 180_000_000)).toBe(false);
    expect(canSeekMpris(true, CONTROL | SEEK, 180_000_000)).toBe(false);
    expect(canSeekMpris(false, CONTROL | SEEK, -1)).toBe(false);
  });

  it("stops a scrub short of the end the bridge would reject", () => {
    expect(mprisSeekTargetUs(90_000_000, 180_000_000)).toBe(90_000_000);
    // Dragged to the far right: SetPosition at or past mpris:length fails, so
    // the last reachable microsecond has to be inside the track.
    expect(mprisSeekTargetUs(180_000_000, 180_000_000)).toBe(179_999_000);
    expect(mprisSeekTargetUs(999_000_000, 180_000_000)).toBe(179_999_000);
  });

  it("refuses to invent a target the player could not honour", () => {
    expect(mprisSeekTargetUs(-5, 180_000_000)).toBe(0);
    expect(mprisSeekTargetUs(1_000, -1)).toBe(0);
    expect(mprisSeekTargetUs(Number.NaN, 180_000_000)).toBe(0);
    // A track shorter than the millisecond of headroom clamps to its start
    // rather than to a negative position.
    expect(mprisSeekTargetUs(400, 500)).toBe(0);
  });
});

describe("popupViewportShift", () => {
  it("leaves a popup that already fits alone", () => {
    expect(popupViewportShift({ left: 100, right: 400 }, 1280)).toBe(0);
  });

  it("pushes a phone-width popup back off the left edge", () => {
    // The chrome's right edge is not the window's — the status bar's tools
    // sit to its right — so a popup sized to the viewport starts off screen.
    expect(popupViewportShift({ left: -40, right: 350 }, 390)).toBe(48);
  });

  it("pulls a popup back off the right edge", () => {
    expect(popupViewportShift({ left: 60, right: 420 }, 390)).toBe(-38);
  });

  it("pins a popup wider than the window to the left margin", () => {
    // Both edges overflow and only one can be honoured. Content starts at the
    // left, so losing the right is the survivable half.
    const shift = popupViewportShift({ left: -30, right: 500 }, 390);
    expect(shift).toBe(38);
    expect(-30 + shift).toBe(8);
  });

  it("respects a caller's margin", () => {
    expect(popupViewportShift({ left: 0, right: 100 }, 390, 16)).toBe(16);
  });
});
