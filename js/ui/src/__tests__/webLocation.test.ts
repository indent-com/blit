import { describe, expect, it } from "vitest";
import {
  normalizeLocation,
  parsePlainLocation,
  plainLocation,
  splitLocation,
  webLocationLabel,
} from "../preview";

const HERE = "http://localhost:3000";

describe("plain-iframe locations", () => {
  it("marks and parses back", () => {
    expect(plainLocation("https://example.com")).toBe(
      "plain:https://example.com",
    );
    expect(parsePlainLocation("plain:https://example.com")).toBe(
      "https://example.com",
    );
    expect(parsePlainLocation("https://example.com")).toBeNull();
  });

  it("fills a bare host with https, not the relayed flow's http", () => {
    expect(plainLocation("example.com")).toBe("plain:https://example.com");
  });

  it("survives normalizeLocation untouched", () => {
    // "plain" is not an http(s) scheme, so the normalizer must treat the
    // whole string as opaque — this is what lets the marker ride the
    // remembered-locations list and the web assignment's URL slot.
    expect(normalizeLocation("plain:https://example.com")).toBe(
      "plain:https://example.com",
    );
  });

  it("labels drop the marker and keep everything else", () => {
    expect(webLocationLabel("plain:https://example.com")).toBe(
      "https://example.com",
    );
    expect(webLocationLabel("http://localhost:3000")).toBe(
      "http://localhost:3000",
    );
  });
});

describe("splitLocation", () => {
  // The status bar edits the whole location, so this decides whether a
  // committed edit navigates within the pane's target or re-points it at a
  // different one.
  it("keeps the current origin for a bare path", () => {
    expect(splitLocation("/admin", HERE)).toEqual({
      origin: HERE,
      path: "/admin",
    });
    expect(splitLocation("/a?b=1#c", HERE)).toEqual({
      origin: HERE,
      path: "/a?b=1#c",
    });
  });

  it("splits a full URL on the same origin into a path", () => {
    expect(splitLocation("http://localhost:3000/next", HERE)).toEqual({
      origin: HERE,
      path: "/next",
    });
    // No path given means the root, not the empty string.
    expect(splitLocation("http://localhost:3000", HERE)?.path).toBe("/");
  });

  it("reports a different origin so the pane can be re-pointed", () => {
    expect(splitLocation("http://localhost:5173/x", HERE)).toEqual({
      origin: "http://localhost:5173",
      path: "/x",
    });
    expect(splitLocation("https://localhost:3000/x", HERE)?.origin).toBe(
      "https://localhost:3000",
    );
  });

  it("fills in a missing scheme, as the web overlay does", () => {
    // `localhost:5173` is what people type.
    expect(splitLocation("localhost:5173/x", HERE)).toEqual({
      origin: "http://localhost:5173",
      path: "/x",
    });
  });

  it("normalizes an implicit port so it compares equal", () => {
    // Both name the same origin; neither should read as a retarget.
    expect(
      splitLocation("http://example.com:80/a", "http://example.com"),
    ).toEqual({ origin: "http://example.com", path: "/a" });
    expect(
      splitLocation("https://example.com:443/a", "https://example.com")?.origin,
    ).toBe("https://example.com");
  });

  it("handles an IPv6 literal", () => {
    expect(splitLocation("http://[::1]:8080/x", HERE)).toEqual({
      origin: "http://[::1]:8080",
      path: "/x",
    });
  });

  it("refuses what is not an http(s) location", () => {
    for (const value of [
      "",
      "   ",
      "file:///etc/passwd",
      "javascript:alert(1)",
      "not a url",
    ]) {
      expect(splitLocation(value, HERE), value).toBeNull();
    }
  });
});
