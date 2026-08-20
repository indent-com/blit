import { describe, expect, it } from "vitest";
import { pillColor } from "../panelTone";
import { darkTheme } from "../theme";

describe("pillColor", () => {
  it("reserves red for failures and uses the warning color for warnings", () => {
    expect(pillColor(darkTheme, "warn")).toBe(darkTheme.warning);
    expect(pillColor(darkTheme, "bad")).toBe(darkTheme.error);
  });
});
