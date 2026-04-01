import { createContext, useContext, useMemo, type ReactNode } from "react";
import type { TerminalPalette } from "@blit-sh/core";
import type { BlitWorkspace } from "@blit-sh/core";

export interface BlitContextValue {
  workspace?: BlitWorkspace;
  palette?: TerminalPalette;
  fontFamily?: string;
  fontSize?: number;
  advanceRatio?: number;
}

const BlitContext = createContext<BlitContextValue>({});

export function useBlitContext(): BlitContextValue {
  return useContext(BlitContext);
}

export interface BlitProviderProps extends BlitContextValue {
  children: ReactNode;
}

export function BlitWorkspaceProvider({
  children,
  workspace,
  palette,
  fontFamily,
  fontSize,
  advanceRatio,
}: BlitProviderProps) {
  const value = useMemo(
    () => ({ workspace, palette, fontFamily, fontSize, advanceRatio }),
    [workspace, palette, fontFamily, fontSize, advanceRatio],
  );
  return <BlitContext.Provider value={value}>{children}</BlitContext.Provider>;
}

export function useRequiredBlitWorkspace(): BlitWorkspace {
  const workspace = useBlitContext().workspace;
  if (!workspace) {
    throw new Error("Blit components require a BlitWorkspaceProvider ancestor");
  }
  return workspace;
}
