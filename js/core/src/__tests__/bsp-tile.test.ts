import { describe, it, expect } from "vitest";
import {
  editorAssignment,
  diffAssignment,
  parseDiffArg,
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

  it("does not treat sessions or surfaces as tiles", () => {
    expect(isTileAssignment("surface:local:3")).toBe(false);
    expect(isTileAssignment("local:5")).toBe(false);
    expect(isTileAssignment(null)).toBe(false);
    expect(parseTileAssignment("surface:local:3")).toBeNull();
    expect(parseTileAssignment(null)).toBeNull();
  });
});
