import { createContext, useContext, type JSX } from "solid-js";
import type { BlitWorkspace, TerminalPalette } from "@blit-sh/core";

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
  children: JSX.Element;
}

export function BlitWorkspaceProvider(props: BlitProviderProps) {
  return (
    <BlitContext.Provider
      value={{
        get workspace() {
          return props.workspace;
        },
        get palette() {
          return props.palette;
        },
        get fontFamily() {
          return props.fontFamily;
        },
        get fontSize() {
          return props.fontSize;
        },
        get advanceRatio() {
          return props.advanceRatio;
        },
      }}
    >
      {props.children}
    </BlitContext.Provider>
  );
}

export function useRequiredBlitWorkspace(): BlitWorkspace {
  const ctx = useBlitContext();
  if (!ctx.workspace) {
    throw new Error("Blit components require a BlitWorkspaceProvider ancestor");
  }
  return ctx.workspace;
}
