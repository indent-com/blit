export { BlitTerminal } from "./BlitTerminal.jsx";
export type { BlitTerminalProps } from "./BlitTerminal.jsx";

export { BlitSurfaceView } from "./BlitSurfaceView.jsx";
export type { BlitSurfaceViewProps } from "./BlitSurfaceView.jsx";

export { useBlitConnection } from "./hooks/useBlitConnection";
export { createBlitSessions } from "./hooks/createBlitSessions";
export {
  createBlitWorkspace,
  createBlitWorkspaceState,
} from "./hooks/createBlitWorkspace";
export { useBlitSession, useBlitFocusedSession } from "./hooks/useBlitSession";
export { createBlitWorkspaceConnection } from "./hooks/createBlitWorkspaceConnection";

export { BlitWorkspaceProvider } from "./BlitContext.jsx";
export type { BlitContextValue, BlitProviderProps } from "./BlitContext.jsx";
