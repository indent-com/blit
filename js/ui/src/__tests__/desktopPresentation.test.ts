import { describe, expect, it } from "vitest";
import {
  TRAY_HAS_MENU,
  TRAY_ITEM_IS_MENU,
  type DesktopNotification,
} from "@blit-sh/core";
import {
  desktopDelivery,
  desktopNativeTag,
  matchesDesktopNotification,
  trayPrimaryOpensMenu,
} from "../desktopPresentation";

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
});
