import { describe, it, expect } from "vitest";
import { render } from "@solidjs/testing-library";
import { BlitWorkspaceProvider, useBlitContext } from "../BlitContext";
import type { BlitContextValue } from "../BlitContext";
import type { BlitWorkspace } from "@blit-sh/core";
import type { JSX } from "solid-js";

function renderWithContext(value: BlitContextValue) {
  let captured: BlitContextValue = {};
  render(() => (
    <BlitWorkspaceProvider {...value}>
      {(() => {
        captured = useBlitContext();
        return null;
      })() as unknown as JSX.Element}
    </BlitWorkspaceProvider>
  ));
  return captured;
}

describe("BlitContext", () => {
  it("returns empty object without provider", () => {
    let captured: BlitContextValue = { workspace: {} as BlitWorkspace };
    render(() => {
      captured = useBlitContext();
      return null;
    });
    expect(captured).toEqual({});
  });

  it("provides workspace", () => {
    const workspace = {} as BlitWorkspace;
    const ctx = renderWithContext({ workspace });
    expect(ctx.workspace).toBe(workspace);
  });

  it("provides palette", () => {
    const palette = {
      id: "test",
      name: "Test",
      dark: true,
      fg: [255, 255, 255] as [number, number, number],
      bg: [0, 0, 0] as [number, number, number],
      ansi: Array.from(
        { length: 16 },
        () => [0, 0, 0] as [number, number, number],
      ),
    };
    const ctx = renderWithContext({ palette });
    expect(ctx.palette).toBe(palette);
  });

  it("provides fontFamily and fontSize", () => {
    const ctx = renderWithContext({ fontFamily: "monospace", fontSize: 14 });
    expect(ctx.fontFamily).toBe("monospace");
    expect(ctx.fontSize).toBe(14);
  });

  it("provides undefined for omitted values", () => {
    const ctx = renderWithContext({});
    expect(ctx.workspace).toBeUndefined();
    expect(ctx.palette).toBeUndefined();
    expect(ctx.fontFamily).toBeUndefined();
    expect(ctx.fontSize).toBeUndefined();
  });
});
