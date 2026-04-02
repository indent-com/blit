import { createContext, useContext, type JSX } from "solid-js";
import type { BlitWorkspace, TerminalPalette } from "@blit-sh/core";

export interface BlitContextValue {
  workspace: BlitWorkspace;
  palette?: TerminalPalette;
  fontFamily?: string;
  fontSize?: number;
  advanceRatio?: number;
}

const BlitContext = createContext<BlitContextValue>();

export function useBlitContext(): BlitContextValue {
  const ctx = useContext(BlitContext);
  if (!ctx) {
    throw new Error("Blit components require a BlitWorkspaceProvider ancestor");
  }
  return ctx;
}

export interface BlitProviderProps extends BlitContextValue {
  children: JSX.Element;
}

export function BlitWorkspaceProvider(props: BlitProviderProps) {
  const value: BlitContextValue = {
    get workspace() { return props.workspace; },
    get palette() { return props.palette; },
    get fontFamily() { return props.fontFamily; },
    get fontSize() { return props.fontSize; },
    get advanceRatio() { return props.advanceRatio; },
  };
  return (
    <BlitContext.Provider value={value}>
      {props.children}
    </BlitContext.Provider>
  );
}
