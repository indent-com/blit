import type { BlitSession, SessionId, ConnectionId, BSPAssignments } from "./types";
import { isSurfaceAssignment, parseSurfaceAssignment } from "./bsp/layout";

/**
 * Minimal keyboard event descriptor — the subset of `KeyboardEvent`
 * that the shortcut matcher inspects.  Framework wrappers pass the
 * native event through; non-browser hosts can construct one manually.
 */
export interface ShortcutKeyEvent {
  key: string;
  code: string;
  ctrlKey: boolean;
  shiftKey: boolean;
  altKey: boolean;
  metaKey: boolean;
}

/**
 * Read-only workspace state consumed by the shortcut handler.
 * Each accessor mirrors one Solid signal / React hook; the core
 * module never imports a framework — callers bridge the gap.
 */
export interface ShortcutContext {
  /** Whether an overlay is currently open. */
  overlay: () => boolean;
  /** Whether a BSP layout is active (non-null = active). */
  hasActiveLayout: () => boolean;
  /** Currently focused BSP pane ID (null when no layout or no pane focused). */
  bspFocusedPaneId: () => string | null;
  /** Current BSP pane→session assignments (null when no layout active). */
  layoutAssignments: () => BSPAssignments | null;
  /** The focused session, or null. */
  focusedSession: () => BlitSession | null;
  /** All sessions. */
  sessions: () => readonly BlitSession[];
  /** Focused session ID. */
  focusedSessionId: () => SessionId | null;
  /** Whether the current connection supports restart. */
  supportsRestart: () => boolean;
  /** Currently focused surface ID (null when a terminal is focused). */
  focusedSurfaceId: () => number | null;
  /** Connection ID of the currently focused surface. */
  focusedSurfaceConnId: () => ConnectionId | null;
  /** Number of active connections. */
  connectionCount: () => number;
  /** Tag name of the currently focused DOM element (e.g. "INPUT"), or null. */
  activeElementTag: () => string | null;
}

/** Side-effect callbacks invoked by the shortcut handler. */
export interface ShortcutActions {
  toggleOverlay: (target: string) => void;
  cancelOverlay: () => void;
  toggleDebug: () => void;
  togglePreviewPanel: () => void;
  createAndFocus: () => void;
  createInPane: (paneId: string) => void;
  openNewTerminalPicker: (paneId?: string) => void;
  handleRestartOrClose: () => void;
  focusBySession: (sessionId: SessionId) => void;
  focusSession: (sessionId: SessionId | null) => void;
  closeSurface: (connectionId: ConnectionId, surfaceId: number) => void;
  closeSession: (sessionId: SessionId) => void;
  unfocusSurface: () => void;
  clearFocusedPaneAssignment: () => void;
  resetAudio: () => void;
}

/**
 * Framework-agnostic workspace keyboard shortcut handler.
 *
 * Inspects the key event against the workspace state and dispatches
 * the appropriate action.  Returns `true` when the event was handled
 * (caller should `preventDefault`), `false` otherwise.
 */
export function handleWorkspaceShortcut(
  e: ShortcutKeyEvent,
  ctx: ShortcutContext,
  act: ShortcutActions,
): boolean {
  const mod = e.metaKey || e.ctrlKey;

  // Cmd/Ctrl+K → expose overlay
  if (mod && !e.shiftKey && e.key === "k") {
    act.toggleOverlay("expose");
    return true;
  }
  // Ctrl+Shift+? → help overlay
  if (e.ctrlKey && e.shiftKey && (e.key === "?" || e.code === "Slash")) {
    act.toggleOverlay("help");
    return true;
  }
  // Ctrl+Shift+~ → debug toggle
  if (e.ctrlKey && e.shiftKey && (e.key === "~" || e.key === "`")) {
    act.toggleDebug();
    return true;
  }
  // Ctrl+Shift+B → preview panel toggle
  if (e.ctrlKey && e.shiftKey && e.key === "B") {
    act.togglePreviewPanel();
    return true;
  }
  // Ctrl+Shift+A: reset audio pipeline (recover from stalled audio).
  if (e.ctrlKey && e.shiftKey && !e.altKey && !e.metaKey && e.key === "A") {
    act.resetAudio();
    return true;
  }
  // Cmd/Ctrl+Enter → new terminal
  if (mod && !e.shiftKey && e.key === "Enter") {
    if (ctx.overlay()) {
      // Let the overlay handle it.
      return true;
    }
    if (ctx.hasActiveLayout() && ctx.bspFocusedPaneId()) {
      if (ctx.connectionCount() <= 1) {
        act.createInPane(ctx.bspFocusedPaneId()!);
      } else {
        act.openNewTerminalPicker(ctx.bspFocusedPaneId()!);
      }
    } else if (ctx.connectionCount() <= 1) {
      act.createAndFocus();
    } else {
      act.openNewTerminalPicker();
    }
    return true;
  }
  // Cmd/Ctrl+Shift+Enter → new terminal (skip picker)
  if (mod && e.shiftKey && e.key === "Enter") {
    if (ctx.hasActiveLayout() && ctx.bspFocusedPaneId()) {
      act.createInPane(ctx.bspFocusedPaneId()!);
    } else {
      act.createAndFocus();
    }
    return true;
  }
  // Enter on exited session → restart/close
  if (e.key === "Enter" && !mod && !e.shiftKey && !ctx.overlay()) {
    // When a surface is focused, Enter is not special.
    if (ctx.focusedSurfaceId() != null) return false;
    // In BSP mode, the focused pane may hold a surface assignment rather
    // than a session.  Don't intercept Enter in that case either.
    const fpId = ctx.bspFocusedPaneId();
    if (fpId) {
      const assign = ctx.layoutAssignments()?.assignments[fpId] ?? null;
      if (isSurfaceAssignment(assign)) return false;
    }
    // Don't intercept Enter when an input/textarea/canvas is focused (e.g.
    // the EmptyPane command input handles Enter itself, and the surface
    // canvas forwards keys to the Wayland compositor).
    const tag = ctx.activeElementTag();
    if (
      tag === "INPUT" ||
      tag === "TEXTAREA" ||
      tag === "CANVAS" ||
      tag === "BUTTON"
    )
      return false;
    const fid = ctx.focusedSessionId();
    const focused = fid ? ctx.sessions().find((s) => s.id === fid) : null;
    if ((focused && focused.state === "exited") || fid == null) {
      act.handleRestartOrClose();
      return true;
    }
  }
  // Ctrl+Shift+Q: remove the current term/surface from the main view
  // (unassign without closing) so it falls back to the sidebar.  Also
  // accept e.code === "KeyQ" to survive keyboard layouts where Shift+Q
  // resolves e.key to lowercase "q".
  if (
    e.ctrlKey &&
    e.shiftKey &&
    !e.altKey &&
    !e.metaKey &&
    (e.key === "Q" || e.key === "q" || e.code === "KeyQ")
  ) {
    if (ctx.overlay()) return false;
    // Non-BSP surface focus: unfocus the surface (return to terminal view).
    if (ctx.focusedSurfaceId() != null) {
      act.unfocusSurface();
      return true;
    }
    if (ctx.hasActiveLayout() && ctx.bspFocusedPaneId()) {
      act.clearFocusedPaneAssignment();
      return true;
    }
    // Single-terminal mode: unfocus the current session so the main
    // area shows the EmptyState and the terminal lives only in the
    // sidebar.
    if (ctx.focusedSessionId() != null) {
      act.focusSession(null);
      return true;
    }
    return false;
  }
  // Ctrl+Alt+Shift+Q: close the focused terminal or surface entirely.
  // Check e.code because Alt on Mac transforms the key value.
  if (
    e.ctrlKey &&
    e.altKey &&
    e.shiftKey &&
    (e.key === "Q" || e.code === "KeyQ")
  ) {
    if (ctx.overlay()) return false;
    // Non-BSP surface focus.
    const sid = ctx.focusedSurfaceId();
    const sConnId = ctx.focusedSurfaceConnId();
    if (sid != null && sConnId != null) {
      act.closeSurface(sConnId, sid);
      return true;
    }
    // BSP pane may hold a surface assignment.
    const fpId = ctx.bspFocusedPaneId();
    if (fpId) {
      const assign = ctx.layoutAssignments()?.assignments[fpId] ?? null;
      if (assign && isSurfaceAssignment(assign)) {
        const parsed = parseSurfaceAssignment(assign);
        if (parsed != null) {
          act.closeSurface(parsed.connectionId, parsed.surfaceId);
          return true;
        }
      }
    }
    const fid = ctx.focusedSessionId();
    if (fid) act.closeSession(fid);
    return true;
  }
  // Prev/next terminal: Alt+Shift+[ / ] on all platforms.
  // Avoids browser tab-switching (Cmd/Ctrl+Shift+[/]) on Mac and Windows.
  // Use e.code (physical key) rather than e.key because Alt on Mac
  // transforms [ to " and ] to '.
  if (
    e.altKey &&
    e.shiftKey &&
    !e.ctrlKey &&
    !e.metaKey &&
    (e.code === "BracketLeft" || e.code === "BracketRight")
  ) {
    // When a surface is focused, cycling leaves the surface first.
    if (ctx.focusedSurfaceId() != null) {
      act.unfocusSurface();
      return true;
    }
    const forward = e.code === "BracketRight";
    const nextId = cycleSession(ctx, forward);
    if (nextId != null) act.focusBySession(nextId);
    return true;
  }
  // Escape → close overlay
  if (e.key === "Escape" && ctx.overlay()) {
    act.cancelOverlay();
    return true;
  }
  return false;
}

/**
 * Compute the next session to cycle to (prev/next).
 * Returns null when cycling should be a no-op.
 */
function cycleSession(
  ctx: ShortcutContext,
  forward: boolean,
): SessionId | null {
  const all = ctx
    .sessions()
    .filter((s) => s.state !== "closed")
    .map((s) => s.id);
  if (all.length === 0) return null;
  const currentId = ctx.focusedSessionId();
  const la = ctx.layoutAssignments();
  const fpId = ctx.bspFocusedPaneId();
  if (la && fpId) {
    // BSP layout active: rotate sessions within the focused pane.
    // The candidate pool is sessions not locked into a *different* pane.
    const assignedElsewhere = new Set(
      Object.entries(la.assignments)
        .filter(([pid, sid]) => pid !== fpId && sid != null)
        .map(([, sid]) => sid as string),
    );
    const candidates = all.filter((id) => !assignedElsewhere.has(id));
    if (candidates.length === 0) return null;
    const index = currentId ? candidates.indexOf(currentId) : -1;
    if (index >= 0 && candidates.length < 2) return null;
    if (index < 0) {
      return forward ? candidates[0] : candidates[candidates.length - 1];
    }
    return forward
      ? candidates[(index + 1) % candidates.length]
      : candidates[(index - 1 + candidates.length) % candidates.length];
  }
  // No layout: simple global cycle.
  const index = currentId ? all.indexOf(currentId) : -1;
  if (index >= 0 && all.length < 2) return null;
  if (index < 0) {
    return forward ? all[0] : all[all.length - 1];
  }
  return forward
    ? all[(index + 1) % all.length]
    : all[(index - 1 + all.length) % all.length];
}

/**
 * Install the workspace keyboard shortcut handler on the window.
 * Returns a cleanup function that removes the listener.
 *
 * This is the recommended entry point for framework wrappers:
 *
 * ```ts
 * // React
 * useEffect(() => installKeyboardShortcuts(ctx, actions), [ctx, actions]);
 *
 * // Solid
 * onMount(() => { onCleanup(installKeyboardShortcuts(ctx, actions)); });
 * ```
 */
export function installKeyboardShortcuts(
  ctx: ShortcutContext,
  act: ShortcutActions,
): () => void {
  const handler = (e: KeyboardEvent) => {
    if (handleWorkspaceShortcut(e, ctx, act)) {
      e.preventDefault();
    }
  };
  window.addEventListener("keydown", handler, true);
  return () => window.removeEventListener("keydown", handler, true);
}
