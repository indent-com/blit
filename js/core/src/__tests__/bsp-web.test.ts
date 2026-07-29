import { describe, expect, it } from "vitest";
import {
  isContentAssignment,
  isSurfaceAssignment,
  isTileAssignment,
  isWebAssignment,
  parseWebAssignment,
  webAssignment,
} from "../bsp/layout";

describe("web pane assignments", () => {
  it("round-trips a URL containing colons and slashes", () => {
    const value = webAssignment("hound", "https://localhost:3000");
    expect(parseWebAssignment(value)).toEqual({
      connectionId: "hound",
      url: "https://localhost:3000",
    });
  });

  it("keeps a path in the URL verbatim", () => {
    const value = webAssignment("local", "http://h:8080/a/b?c=1");
    expect(parseWebAssignment(value)?.url).toBe("http://h:8080/a/b?c=1");
  });

  it("is distinguishable from every other assignment shape", () => {
    const web = webAssignment("local", "http://h:80");
    expect(isWebAssignment(web)).toBe(true);
    expect(isSurfaceAssignment(web)).toBe(false);
    expect(isTileAssignment(web)).toBe(false);
    expect(isWebAssignment("surface:local:1")).toBe(false);
    expect(isWebAssignment("editor:local:/tmp/x")).toBe(false);
    expect(isWebAssignment(null)).toBe(false);
  });

  it("counts as pane content, so sessions never claim it", () => {
    // A web pane appearing in session assignment would have the workspace try
    // to treat a URL as a PTY id.
    expect(isContentAssignment(webAssignment("local", "http://h:80"))).toBe(
      true,
    );
    expect(isContentAssignment("surface:local:1")).toBe(true);
    expect(isContentAssignment("editor:local:/x")).toBe(true);
    expect(isContentAssignment("local:7")).toBe(false);
    expect(isContentAssignment(null)).toBe(false);
  });

  it("rejects malformed values rather than half-parsing them", () => {
    expect(parseWebAssignment("web:")).toBeNull();
    expect(parseWebAssignment("web:local")).toBeNull();
    expect(parseWebAssignment("web:local:")).toBeNull();
    expect(parseWebAssignment("web::http://h")).toBeNull();
    expect(parseWebAssignment(null)).toBeNull();
  });
});
