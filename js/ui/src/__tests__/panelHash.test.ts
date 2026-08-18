import { describe, expect, it } from "vitest";
import {
  formatExpandedHash,
  formatPanelsHash,
  parseExpandedHash,
  parsePanelsHash,
} from "../panelHash";

describe("panelHash panels (d=)", () => {
  it("round-trips both panels open", () => {
    expect(formatPanelsHash(true, true)).toBe("l,r");
    expect(parsePanelsHash("l,r")).toEqual({ left: true, preview: true });
  });

  it("round-trips left-only and right-only", () => {
    expect(formatPanelsHash(true, false)).toBe("l");
    expect(formatPanelsHash(false, true)).toBe("r");
    expect(parsePanelsHash("l")).toEqual({ left: true, preview: false });
    expect(parsePanelsHash("r")).toEqual({ left: false, preview: true });
  });

  it("encodes both closed as an empty value", () => {
    expect(formatPanelsHash(false, false)).toBe("");
    expect(parsePanelsHash("")).toEqual({ left: false, preview: false });
  });

  it("returns null when the key is absent from the hash", () => {
    expect(parsePanelsHash(null)).toBeNull();
  });

  it("ignores unknown tokens", () => {
    expect(parsePanelsHash("l,foo,z")).toEqual({ left: true, preview: false });
  });
});

describe("panelHash expanded sections (x=)", () => {
  it("round-trips all sections expanded", () => {
    expect(formatExpandedHash(new Set())).toBe(
      "explorer,branches,log,problems",
    );
    expect(parseExpandedHash("explorer,branches,log,problems")).toEqual(
      new Set(),
    );
  });

  it("round-trips a subset", () => {
    const collapsed = new Set(["branches", "problems"] as const);
    expect(formatExpandedHash(collapsed)).toBe("explorer,log");
    expect(parseExpandedHash("explorer,log")).toEqual(
      new Set(["branches", "problems"]),
    );
  });

  it("encodes all collapsed as an empty value", () => {
    const collapsed = new Set([
      "explorer",
      "branches",
      "log",
      "problems",
    ] as const);
    expect(formatExpandedHash(collapsed)).toBe("");
    expect(parseExpandedHash("")).toEqual(
      new Set(["explorer", "branches", "log", "problems"]),
    );
  });

  it("returns null when the key is absent from the hash", () => {
    expect(parseExpandedHash(null)).toBeNull();
  });

  it("drops unknown section ids", () => {
    expect(parseExpandedHash("explorer,nope")).toEqual(
      new Set(["branches", "log", "problems"]),
    );
  });

  /** A section id in a shared link outlives the release that coined it, so an
   *  old `x=` must still mean what it said. A panel added later is simply not
   *  named by it, and lands collapsed rather than corrupting the parse. */
  it("keeps an older link's meaning when a section is added", () => {
    expect(parseExpandedHash("explorer,log,problems")).toEqual(
      new Set(["branches"]),
    );
  });
});
