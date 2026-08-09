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
    expect(formatExpandedHash(new Set())).toBe("explorer,log,problems");
    expect(parseExpandedHash("explorer,log,problems")).toEqual(new Set());
  });

  it("round-trips a subset", () => {
    const collapsed = new Set(["problems"] as const);
    expect(formatExpandedHash(collapsed)).toBe("explorer,log");
    expect(parseExpandedHash("explorer,log")).toEqual(new Set(["problems"]));
  });

  it("encodes all collapsed as an empty value", () => {
    const collapsed = new Set(["explorer", "log", "problems"] as const);
    expect(formatExpandedHash(collapsed)).toBe("");
    expect(parseExpandedHash("")).toEqual(
      new Set(["explorer", "log", "problems"]),
    );
  });

  it("returns null when the key is absent from the hash", () => {
    expect(parseExpandedHash(null)).toBeNull();
  });

  it("drops unknown section ids", () => {
    expect(parseExpandedHash("explorer,nope")).toEqual(
      new Set(["log", "problems"]),
    );
  });
});
