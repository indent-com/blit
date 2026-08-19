import { describe, it, expect } from "vitest";
import {
  editorAssignment,
  diffAssignment,
  manageAssignment,
  parseDiffArg,
  isContentAssignment,
  isTileAssignment,
  parseTileAssignment,
} from "../bsp/layout";

describe("tile assignments (docs/ide-plan.md PR-6)", () => {
  it("round-trips a diff assignment whose path contains ':' and '/'", () => {
    // The critical case parseSurfaceAssignment's lastIndexOf would corrupt.
    const a = diffAssignment("local", "/a/b:c/engine.rs");
    expect(isTileAssignment(a)).toBe(true);
    expect(parseTileAssignment(a)).toEqual({
      kind: "diff",
      connectionId: "local",
      arg: "/a/b:c/engine.rs",
    });
  });

  it("round-trips an editor assignment", () => {
    const e = editorAssignment("rabbit", "src/main.rs");
    expect(isTileAssignment(e)).toBe(true);
    expect(parseTileAssignment(e)).toEqual({
      kind: "editor",
      connectionId: "rabbit",
      arg: "src/main.rs",
    });
  });

  it("round-trips diff sides (unstaged / staged / untracked)", () => {
    const p = "/a/b:c/new.rs"; // path contains ':' — must survive
    for (const side of ["unstaged", "staged", "untracked"] as const) {
      const a = diffAssignment("local", p, side);
      const tile = parseTileAssignment(a);
      expect(tile?.kind).toBe("diff");
      expect(parseDiffArg(tile!.arg)).toEqual({
        side,
        staged: side === "staged",
        path: p,
      });
    }
  });

  // A manage tile's address is its connection and nothing else. The trailing
  // colon is load-bearing: parseTileAssignment splits on the first ":" after
  // the prefix, so "manage:hound" (no colon left to split on) parses as
  // nothing at all — and a tile that fails to parse renders an empty pane.
  it("round-trips a manage assignment, arg and all", () => {
    const m = manageAssignment("hound");
    expect(isTileAssignment(m)).toBe(true);
    expect(isContentAssignment(m)).toBe(true);
    expect(parseTileAssignment(m)).toEqual({
      kind: "manage",
      connectionId: "hound",
      arg: "",
    });
    // What the tab registry does to it and back (stripConn/withConn).
    expect(parseTileAssignment(`manage:hound:`)).not.toBeNull();
    expect(parseTileAssignment("manage:hound")).toBeNull();
  });

  it("does not treat sessions or surfaces as tiles", () => {
    expect(isTileAssignment("surface:local:3")).toBe(false);
    expect(isTileAssignment("local:5")).toBe(false);
    expect(isTileAssignment(null)).toBe(false);
    expect(parseTileAssignment("surface:local:3")).toBeNull();
    expect(parseTileAssignment(null)).toBeNull();
  });
});
