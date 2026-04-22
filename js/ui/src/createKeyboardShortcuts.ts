import { onMount, onCleanup } from "solid-js";
import type {
  BlitWorkspace,
  BlitSession,
  SessionId,
  ConnectionId,
  BSPAssignments,
} from "@blit-sh/core";
import { installKeyboardShortcuts } from "@blit-sh/core";
import type { ShortcutContext, ShortcutActions } from "@blit-sh/core";
import type { Overlay } from "./Workspace";

export interface KeyboardShortcutHandlers {
  workspace: BlitWorkspace;
  /** Current overlay accessor */
  overlay: () => Overlay;
  /** Currently active BSP layout (null = single terminal) */
  activeLayout: () => unknown | null;
  /** Currently focused BSP pane ID */
  bspFocusedPaneId: () => string | null;
  /** Current BSP pane→session assignments (null when no layout active) */
  layoutAssignments: () => BSPAssignments | null;
  /** Focused session accessor */
  focusedSession: () => BlitSession | null;
  /** All sessions accessor */
  sessions: () => readonly BlitSession[];
  /** Focused session ID accessor */
  focusedSessionId: () => SessionId | null;
  /** Connection supports restart */
  supportsRestart: () => boolean;
  /** Currently focused surface ID (null when a terminal is focused) */
  focusedSurfaceId: () => number | null;
  /** Connection ID of the currently focused surface */
  focusedSurfaceConnId: () => ConnectionId | null;
  /** Close / request-close the focused surface */
  closeSurface: (connectionId: ConnectionId, surfaceId: number) => void;
  /** Unfocus the surface and return to the terminal view */
  unfocusSurface: () => void;

  toggleOverlay: (target: Overlay) => void;
  cancelOverlay: () => void;
  toggleDebug: () => void;
  togglePreviewPanel: () => void;
  createAndFocus: () => Promise<void>;
  createInPane: (paneId: string) => Promise<void>;
  openNewTerminalPicker: (paneId?: string) => void;
  handleRestartOrClose: () => void;
  connectionCount: () => number;
  /** Focus a session by ID, updating BSP pane focus if a layout is active */
  focusBySession: (sessionId: SessionId) => void;
  /** Clear the assignment for the focused BSP pane (remove term without closing) */
  clearFocusedPaneAssignment: () => void;
  /** Reset the audio pipeline on all connections to recover from stalled audio */
  resetAudio: () => void;
}

/**
 * Installs global keyboard shortcuts for the workspace.
 * Must be called inside a Solid component (uses onMount/onCleanup).
 *
 * This is a thin Solid wrapper over `@blit-sh/core`'s
 * `installKeyboardShortcuts`.  The shortcut matching logic lives
 * entirely in core so it can be reused by `@blit-sh/react` and
 * other framework bindings.
 */
export function createKeyboardShortcuts(h: KeyboardShortcutHandlers): void {
  const ctx: ShortcutContext = {
    overlay: () => h.overlay() != null,
    hasActiveLayout: () => h.activeLayout() != null,
    bspFocusedPaneId: h.bspFocusedPaneId,
    layoutAssignments: h.layoutAssignments,
    focusedSession: h.focusedSession,
    sessions: h.sessions,
    focusedSessionId: h.focusedSessionId,
    supportsRestart: h.supportsRestart,
    focusedSurfaceId: h.focusedSurfaceId,
    focusedSurfaceConnId: h.focusedSurfaceConnId,
    connectionCount: h.connectionCount,
    activeElementTag: () => document.activeElement?.tagName ?? null,
  };

  const act: ShortcutActions = {
    toggleOverlay: (target) => h.toggleOverlay(target as Overlay),
    cancelOverlay: h.cancelOverlay,
    toggleDebug: h.toggleDebug,
    togglePreviewPanel: h.togglePreviewPanel,
    createAndFocus: () => void h.createAndFocus(),
    createInPane: (paneId) => void h.createInPane(paneId),
    openNewTerminalPicker: h.openNewTerminalPicker,
    handleRestartOrClose: h.handleRestartOrClose,
    focusBySession: h.focusBySession,
    focusSession: (id) => h.workspace.focusSession(id),
    closeSurface: h.closeSurface,
    closeSession: (id) => void h.workspace.closeSession(id),
    unfocusSurface: h.unfocusSurface,
    clearFocusedPaneAssignment: h.clearFocusedPaneAssignment,
    resetAudio: h.resetAudio,
  };

  onMount(() => {
    onCleanup(installKeyboardShortcuts(ctx, act));
  });
}
