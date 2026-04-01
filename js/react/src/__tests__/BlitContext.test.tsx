import { describe, it, expect } from "vitest";
import { renderHook } from "@testing-library/react";
import type { ReactNode } from "react";
import { BlitWorkspaceProvider, useBlitContext } from "../BlitContext";
import type { BlitContextValue } from "../BlitContext";
import type { BlitWorkspace } from "@blit-sh/core";

function wrapper(value: BlitContextValue) {
  return function Wrapper({ children }: { children: ReactNode }) {
    return <BlitWorkspaceProvider {...value}>{children}</BlitWorkspaceProvider>;
  };
}

describe("BlitContext", () => {
  it("returns empty object without provider", () => {
    const { result } = renderHook(() => useBlitContext());
    expect(result.current).toEqual({});
  });

  it("provides workspace", () => {
    const workspace = {} as BlitWorkspace;
    const { result } = renderHook(() => useBlitContext(), {
      wrapper: wrapper({ workspace }),
    });
    expect(result.current.workspace).toBe(workspace);
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
    const { result } = renderHook(() => useBlitContext(), {
      wrapper: wrapper({ palette }),
    });
    expect(result.current.palette).toBe(palette);
  });

  it("provides fontFamily and fontSize", () => {
    const { result } = renderHook(() => useBlitContext(), {
      wrapper: wrapper({ fontFamily: "monospace", fontSize: 14 }),
    });
    expect(result.current.fontFamily).toBe("monospace");
    expect(result.current.fontSize).toBe(14);
  });

  it("provides undefined for omitted values", () => {
    const { result } = renderHook(() => useBlitContext(), {
      wrapper: wrapper({}),
    });
    expect(result.current.workspace).toBeUndefined();
    expect(result.current.palette).toBeUndefined();
    expect(result.current.fontFamily).toBeUndefined();
    expect(result.current.fontSize).toBeUndefined();
  });
});
