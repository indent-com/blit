export * from "@blit-sh/core";

export { BlitTerminal } from "./BlitTerminal";
export type { BlitTerminalHandle } from "./BlitTerminal";

export type { BlitTerminalProps } from "./types";

export { useBlitConnection } from "./hooks/useBlitConnection";
export { useBlitSessions } from "./hooks/useBlitSessions";
export {
  useBlitWorkspace,
  useBlitWorkspaceState,
} from "./hooks/useBlitWorkspace";
export { useBlitSession, useBlitFocusedSession } from "./hooks/useBlitSession";

export { BlitWorkspaceProvider } from "./BlitContext";
export type { BlitContextValue, BlitProviderProps } from "./BlitContext";
