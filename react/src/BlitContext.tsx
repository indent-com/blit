import { createContext, useContext, useMemo, type ReactNode } from "react";
import type { TerminalPalette } from "./types";
import type { BlitWorkspace } from "./BlitWorkspace";

export interface BlitContextValue {
  workspace?: BlitWorkspace;
  palette?: TerminalPalette;
  fontFamily?: string;
  fontSize?: number;
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
}: BlitProviderProps) {
  const value = useMemo(
    () => ({ workspace, palette, fontFamily, fontSize }),
    [workspace, palette, fontFamily, fontSize],
  );
  return <BlitContext.Provider value={value}>{children}</BlitContext.Provider>;
}

export function useRequiredBlitWorkspace(): BlitWorkspace {
  const workspace = useBlitContext().workspace;
  if (!workspace) {
    throw new Error(
      "Blit components require a BlitWorkspaceProvider ancestor",
    );
  }
  return workspace;
}
