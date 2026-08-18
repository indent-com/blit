import { describe, expect, it } from "vitest";
import type { BlitClientInfo, BlitClientOrigin } from "@blit-sh/core";
import {
  formatClientAge,
  formatClientBandwidth,
  formatClientLabel,
  formatClientOriginTag,
  formatClientSubscription,
  formatExtensionAttempt,
  formatExtensionTitle,
  formatKickAction,
  formatSurfaceViewSize,
  formatTerminalViewSize,
} from "../clientDisplay";

describe("client subscription sizes", () => {
  it("formats terminal dimensions as columns by rows", () => {
    expect(formatTerminalViewSize(120, 40)).toBe("120×40");
    expect(formatTerminalViewSize(null, null)).toBe("size not reported");
  });

  it("formats surface dimensions and fractional scale", () => {
    expect(formatSurfaceViewSize(1920, 1080, 120)).toBe("1920×1080 @ 1×");
    expect(formatSurfaceViewSize(1280, 720, 180)).toBe("1280×720 @ 1.5×");
    expect(formatSurfaceViewSize(800, 600, null)).toBe("800×600");
    expect(formatSurfaceViewSize(null, null, null)).toBe("size not reported");
  });

  it("keeps trailing zeros that belong to a scale's integer part", () => {
    // Trimming a trailing "0" after stripping ".00" turned 10× into 1×.
    expect(formatSurfaceViewSize(640, 480, 1200)).toBe("640×480 @ 10×");
    expect(formatSurfaceViewSize(640, 480, 2400)).toBe("640×480 @ 20×");
    // Fractional zeros should still go.
    expect(formatSurfaceViewSize(640, 480, 132)).toBe("640×480 @ 1.1×");
    expect(formatSurfaceViewSize(640, 480, 240)).toBe("640×480 @ 2×");
    // Sub-1× scales round to 2dp rather than disappearing.
    expect(formatSurfaceViewSize(640, 480, 100)).toBe("640×480 @ 0.83×");
  });

  it("formats client age and outbound bandwidth", () => {
    expect(formatClientAge(45)).toBe("45s");
    expect(formatClientAge(125)).toBe("2m 5s");
    expect(formatClientAge(7_380)).toBe("2h 3m");
    expect(formatClientAge(183_600)).toBe("2d 3h");
    expect(formatClientBandwidth(0)).toBe("0 B/s");
    expect(formatClientBandwidth(1_500)).toBe("1.5 kB/s");
    expect(formatClientBandwidth(1_500_000)).toBe("1.5 MB/s");
  });

  it("labels every auxiliary subscription family", () => {
    expect(formatClientSubscription(1, 0)).toBe("Audio");
    expect(formatClientSubscription(2, 3)).toBe("Filesystem #3");
    expect(formatClientSubscription(3, 4)).toBe("Git #4");
    expect(formatClientSubscription(4, 5)).toBe("LSP #5");
    expect(formatClientSubscription(5, 6)).toBe("KV #6");
    expect(formatClientSubscription(6, 7)).toBe("Network #7");
    expect(formatClientSubscription(99, 8)).toBe("Unknown 99 #8");
  });
});

describe("client identity", () => {
  function client(origin: BlitClientOrigin | null): BlitClientInfo {
    return {
      id: 7n,
      ageSeconds: 11,
      outboundBytesPerSecond: 0,
      inboundBytesPerSecond: 0,
      subscriptions: [],
      terminals: [],
      surfaces: [],
      origin,
    };
  }

  const extension: BlitClientOrigin = {
    kind: "extension",
    extensionId: 0x05a3415a2dd1ef9bn,
    definitionRevision: 2n,
    attempt: 3n,
    taskId: 4,
    name: "systemd",
  };

  it("names an extension by its definition, not its connection id", () => {
    expect(formatClientLabel(client(extension))).toBe("systemd");
    expect(formatClientOriginTag(client(extension))).toBe("extension");
    expect(formatExtensionAttempt(client(extension))).toBe("attempt 3");
    // The task id is a random 32-bit handle, not an ordinal, so it stays in
    // the tooltip where it costs no attention until someone wants it.
    expect(formatExtensionTitle(client(extension))).toBe(
      "Extension id:05a3415a2dd1ef9b · revision 2 · task 4",
    );
  });

  it("falls back to the id an unnamed transient run is addressed by", () => {
    // The same handle the extensions panel shows and `ext status` accepts.
    expect(formatClientLabel(client({ ...extension, name: "" }))).toBe(
      "id:05a3415a2dd1ef9b",
    );
  });

  it("leaves an ordinary client, and an unasked one, unadorned", () => {
    for (const origin of [{ kind: "network" } as const, null]) {
      expect(formatClientLabel(client(origin))).toBe("Client 7");
      expect(formatClientOriginTag(client(origin))).toBeNull();
      expect(formatExtensionAttempt(client(origin))).toBeNull();
      expect(formatKickAction(client(origin)).idle).toBe("Kick");
    }
  });

  it("says a kind it cannot name is not an ordinary client", () => {
    const unknown = client({ kind: "unknown", originKind: 200 });
    expect(formatClientLabel(unknown)).toBe("Client 7");
    expect(formatClientOriginTag(unknown)).toBe("unrecognized");
  });

  it("tells the viewer that kicking an extension ends its attempt", () => {
    expect(formatKickAction(client(extension))).toEqual({
      idle: "Stop attempt",
      confirm: "Confirm stop",
      busy: "Stopping…",
    });
  });
});
