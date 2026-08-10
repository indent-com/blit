import { describe, expect, it } from "vitest";
import {
  debugPanelOpenFromHash,
  withDebugPanelState,
} from "../workspaceUrl";

describe("workspace debug URL state", () => {
  it("recognizes the established bare debug flag", () => {
    expect(debugPanelOpenFromHash("#t=local%3A1&debug")).toBe(true);
    expect(debugPanelOpenFromHash("#t=local%3A1")).toBe(false);
  });

  it("accepts parameter-shaped and encoded debug flags", () => {
    expect(debugPanelOpenFromHash("debug=1")).toBe(true);
    expect(debugPanelOpenFromHash("%64ebug")).toBe(true);
    expect(debugPanelOpenFromHash("debugger")).toBe(false);
  });

  it("adds one canonical flag without rewriting other URL state", () => {
    expect(withDebugPanelState("l=two:a|b&t=local:1", true)).toBe(
      "l=two:a|b&t=local:1&debug",
    );
    expect(withDebugPanelState("secret&debug=1", true)).toBe(
      "secret&debug",
    );
  });

  it("removes the flag without disturbing other URL state", () => {
    expect(withDebugPanelState("secret&debug&a=0:t:local:1", false)).toBe(
      "secret&a=0:t:local:1",
    );
  });
});
