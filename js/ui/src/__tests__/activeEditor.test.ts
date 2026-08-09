import { describe, expect, it } from "vitest";
import type { PreviewController } from "../ide/activeEditor";
import { activeEditor, setActiveEditorFocused } from "../ide/activeEditor";

function controller(path: string): PreviewController {
  return {
    kind: "preview",
    connectionId: "main",
    path,
  };
}

describe("focused tile chrome", () => {
  it("follows pane focus without a background pane clearing the winner", () => {
    const first = controller("/first.ts");
    const second = controller("/second.ts");

    setActiveEditorFocused(first, true);
    expect(activeEditor()).toBe(first);

    // Mounting a background BSP tile must not take or clear the bar.
    setActiveEditorFocused(second, false);
    expect(activeEditor()).toBe(first);

    setActiveEditorFocused(second, true);
    expect(activeEditor()).toBe(second);

    // Solid may run the old pane's focus effect after the new pane's. It may
    // only release its own controller, not the newly focused controller.
    setActiveEditorFocused(first, false);
    expect(activeEditor()).toBe(second);

    setActiveEditorFocused(second, false);
    expect(activeEditor()).toBeNull();
  });
});
