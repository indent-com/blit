import { describe, expect, it } from "vitest";
import {
  desktopNotificationIdentity,
  desktopNotificationImage,
  desktopNotificationSourceClientId,
  topLevelDesktopSender,
} from "../desktopNotifications";

describe("desktop notification worker boundary", () => {
  it("accepts only unbound top-level window senders", () => {
    const top = { type: "window", frameType: "top-level" };
    expect(topLevelDesktopSender(top, false)).toBe(true);
    expect(topLevelDesktopSender(top, true)).toBe(false);
    expect(
      topLevelDesktopSender({ type: "window", frameType: "nested" }, false),
    ).toBe(false);
    expect(
      topLevelDesktopSender({ type: "worker", frameType: "top-level" }, false),
    ).toBe(false);
  });

  it("bounds and validates click identities", () => {
    expect(
      desktopNotificationIdentity({
        connectionId: "remote",
        bootGeneration: "42",
        notificationId: 7,
        revision: 3,
      }),
    ).toEqual({
      connectionId: "remote",
      bootGeneration: "42",
      notificationId: 7,
      revision: 3,
    });
    expect(
      desktopNotificationIdentity({
        connectionId: "remote",
        bootGeneration: "42",
        notificationId: 0,
        revision: 3,
      }),
    ).toBeNull();
  });

  it("permits only bounded re-encoded PNG data URLs", () => {
    expect(desktopNotificationImage("data:image/png;base64,AA==")).toBe(
      "data:image/png;base64,AA==",
    );
    expect(desktopNotificationImage("https://remote/icon.png")).toBeUndefined();
    expect(
      desktopNotificationImage(
        `data:image/png;base64,${"A".repeat(1_500_000)}`,
      ),
    ).toBeUndefined();
  });

  it("routes clicks only to the top-level client that showed them", () => {
    expect(
      desktopNotificationSourceClientId({ sourceClientId: "window-7" }),
    ).toBe("window-7");
    expect(desktopNotificationSourceClientId({})).toBeNull();
    expect(
      desktopNotificationSourceClientId({ sourceClientId: "" }),
    ).toBeNull();
  });
});
