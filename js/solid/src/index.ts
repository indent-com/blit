export { BlitTerminal } from "./BlitTerminal";
export type { BlitTerminalProps } from "./BlitTerminal";

export { BlitSurfaceView } from "./BlitSurfaceView";
export type { BlitSurfaceViewProps } from "./BlitSurfaceView";

export { useBlitConnection } from "./hooks/useBlitConnection";
export { createBlitSessions } from "./hooks/createBlitSessions";
export {
  createBlitWorkspace,
  createBlitWorkspaceState,
} from "./hooks/createBlitWorkspace";
export { useBlitSession, useBlitFocusedSession } from "./hooks/useBlitSession";
export { createBlitWorkspaceConnection } from "./hooks/createBlitWorkspaceConnection";

export { BlitWorkspaceProvider } from "./BlitContext";
export type { BlitContextValue, BlitProviderProps } from "./BlitContext";
