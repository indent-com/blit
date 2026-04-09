import { onMount, onCleanup } from "solid-js";
import type {
  BlitWorkspace,
  BlitSession,
  SessionId,
  ConnectionId,
  BSPAssignments,
} from "@blit-sh/core";
import { isSurfaceAssignment, parseSurfaceAssignment } from "./bsp/layout";
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
 */
export function createKeyboardShortcuts(h: KeyboardShortcutHandlers): void {
  onMount(() => {
    const handler = (e: KeyboardEvent) => {
      const mod = e.metaKey || e.ctrlKey;

      if (mod && !e.shiftKey && e.key === "k") {
        e.preventDefault();
        h.toggleOverlay("expose");
        return;
      }
      if (e.ctrlKey && e.shiftKey && (e.key === "?" || e.code === "Slash")) {
        e.preventDefault();
        h.toggleOverlay("help");
        return;
      }
      if (e.ctrlKey && e.shiftKey && (e.key === "~" || e.key === "`")) {
        e.preventDefault();
        h.toggleDebug();
        return;
      }
      if (e.ctrlKey && e.shiftKey && e.key === "B") {
        e.preventDefault();
        h.togglePreviewPanel();
        return;
      }
      // Ctrl+Shift+A: reset audio pipeline (recover from stalled audio).
      if (e.ctrlKey && e.shiftKey && !e.altKey && !e.metaKey && e.key === "A") {
        e.preventDefault();
        h.resetAudio();
        return;
      }
      if (mod && !e.shiftKey && e.key === "Enter") {
        e.preventDefault();
        if (h.overlay()) {
          // Let the overlay handle it.
          return;
        }
        if (h.activeLayout() && h.bspFocusedPaneId()) {
          if (h.connectionCount() <= 1) {
            void h.createInPane(h.bspFocusedPaneId()!);
          } else {
            h.openNewTerminalPicker(h.bspFocusedPaneId()!);
          }
        } else if (h.connectionCount() <= 1) {
          void h.createAndFocus();
        } else {
          h.openNewTerminalPicker();
        }
        return;
      }
      if (mod && e.shiftKey && e.key === "Enter") {
        e.preventDefault();
        if (h.activeLayout() && h.bspFocusedPaneId()) {
          void h.createInPane(h.bspFocusedPaneId()!);
        } else {
          void h.createAndFocus();
        }
        return;
      }
      if (e.key === "Enter" && !mod && !e.shiftKey && !h.overlay()) {
        // Enter on an exited session restarts/closes it (works in BSP layouts too).
        // When a surface is focused, Enter is not special.
        if (h.focusedSurfaceId() != null) return;
        // In BSP mode, the focused pane may hold a surface assignment rather
        // than a session.  Don't intercept Enter in that case either.
        const fpId = h.bspFocusedPaneId();
        if (fpId) {
          const assign = h.layoutAssignments()?.assignments[fpId] ?? null;
          if (isSurfaceAssignment(assign)) return;
        }
        // Don't intercept Enter when an input/textarea/canvas is focused (e.g.
        // the EmptyPane command input handles Enter itself, and the surface
        // canvas forwards keys to the Wayland compositor).
        const tag = document.activeElement?.tagName;
        if (
          tag === "INPUT" ||
          tag === "TEXTAREA" ||
          tag === "CANVAS" ||
          tag === "BUTTON"
        )
          return;
        const fid = h.focusedSessionId();
        const focused = fid ? h.sessions().find((s) => s.id === fid) : null;
        if ((focused && focused.state === "exited") || fid == null) {
          e.preventDefault();
          h.handleRestartOrClose();
          return;
        }
      }
      // Ctrl+Shift+Q: remove the current term/surface from the focused BSP pane
      // (unassign without closing).
      if (e.ctrlKey && e.shiftKey && !e.altKey && !e.metaKey && e.key === "Q") {
        if (h.overlay()) return;
        // Non-BSP surface focus: unfocus the surface (return to terminal view).
        if (h.focusedSurfaceId() != null) {
          e.preventDefault();
          h.unfocusSurface();
          return;
        }
        if (!h.activeLayout() || !h.bspFocusedPaneId()) return;
        e.preventDefault();
        h.clearFocusedPaneAssignment();
        return;
      }
      // Ctrl+Alt+Shift+Q: close the focused terminal or surface entirely.
      // Check e.code because Alt on Mac transforms the key value.
      if (
        e.ctrlKey &&
        e.altKey &&
        e.shiftKey &&
        (e.key === "Q" || e.code === "KeyQ")
      ) {
        if (h.overlay()) return;
        e.preventDefault();
        // Non-BSP surface focus.
        const sid = h.focusedSurfaceId();
        const sConnId = h.focusedSurfaceConnId();
        if (sid != null && sConnId != null) {
          h.closeSurface(sConnId, sid);
          return;
        }
        // BSP pane may hold a surface assignment.
        const fpId = h.bspFocusedPaneId();
        if (fpId) {
          const assign = h.layoutAssignments()?.assignments[fpId] ?? null;
          if (assign && isSurfaceAssignment(assign)) {
            const parsed = parseSurfaceAssignment(assign);
            if (parsed != null) {
              h.closeSurface(parsed.connectionId, parsed.surfaceId);
              return;
            }
          }
        }
        const fid = h.focusedSessionId();
        if (fid) void h.workspace.closeSession(fid);
        return;
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
        e.preventDefault();
        // When a surface is focused, cycling leaves the surface first.
        if (h.focusedSurfaceId() != null) {
          h.unfocusSurface();
          return;
        }
        const all = h
          .sessions()
          .filter((s) => s.state !== "closed")
          .map((s) => s.id);
        if (all.length === 0) return;
        const currentId = h.focusedSessionId();
        const la = h.layoutAssignments();
        const fpId = h.bspFocusedPaneId();
        if (la && fpId) {
          // BSP layout active: rotate sessions within the focused pane.
          // The candidate pool is sessions not locked into a *different* pane.
          const assignedElsewhere = new Set(
            Object.entries(la.assignments)
              .filter(([pid, sid]) => pid !== fpId && sid != null)
              .map(([, sid]) => sid as string),
          );
          const candidates = all.filter((id) => !assignedElsewhere.has(id));
          if (candidates.length === 0) return;
          const index = currentId ? candidates.indexOf(currentId) : -1;
          if (index >= 0 && candidates.length < 2) return;
          let nextId: string;
          if (index < 0) {
            nextId =
              e.code === "BracketRight"
                ? candidates[0]
                : candidates[candidates.length - 1];
          } else {
            nextId =
              e.code === "BracketRight"
                ? candidates[(index + 1) % candidates.length]
                : candidates[
                    (index - 1 + candidates.length) % candidates.length
                  ];
          }
          h.focusBySession(nextId);
        } else {
          // No layout: simple global cycle.
          const index = currentId ? all.indexOf(currentId) : -1;
          if (index >= 0 && all.length < 2) return;
          let nextId: string;
          if (index < 0) {
            nextId = e.code === "BracketRight" ? all[0] : all[all.length - 1];
          } else {
            nextId =
              e.code === "BracketRight"
                ? all[(index + 1) % all.length]
                : all[(index - 1 + all.length) % all.length];
          }
          h.focusBySession(nextId);
        }
        return;
      }
      if (e.key === "Escape") {
        if (h.overlay()) {
          e.preventDefault();
          h.cancelOverlay();
          return;
        }
        // Escape while a surface is focused returns to the terminal view.
        if (h.focusedSurfaceId() != null) {
          e.preventDefault();
          h.unfocusSurface();
          return;
        }
        // When a BSP layout is active, BSPContainer handles Escape on
        // exited sessions itself (it needs to clear the pane assignment
        // before closing).  If we close here on the capture phase the
        // session state flips to "closed" synchronously, which
        // invalidates the BSPContainer effect before its bubble-phase
        // handler can fire.
        if (!h.activeLayout()) {
          const fs = h.focusedSession();
          if (fs?.state === "exited") {
            e.preventDefault();
            void h.workspace.closeSession(fs.id);
          }
        }
      }
    };

    window.addEventListener("keydown", handler, true);
    onCleanup(() => window.removeEventListener("keydown", handler, true));
  });
}
