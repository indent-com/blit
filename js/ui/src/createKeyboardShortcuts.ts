import { onMount, onCleanup } from "solid-js";
import type {
  BlitWorkspace,
  BlitSession,
  SessionId,
  BSPAssignments,
} from "@blit-sh/core";
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
  /** Close / request-close the focused surface */
  closeSurface: (surfaceId: number) => void;
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
      if (
        e.key === "Enter" &&
        !mod &&
        !e.shiftKey &&
        !h.overlay() &&
        !h.activeLayout()
      ) {
        // Enter on an exited session restarts/closes it.
        // When a surface is focused, Enter is not special.
        if (h.focusedSurfaceId() != null) return;
        const fid = h.focusedSessionId();
        const focused = fid ? h.sessions().find((s) => s.id === fid) : null;
        if ((focused && focused.state === "exited") || fid == null) {
          e.preventDefault();
          h.handleRestartOrClose();
          return;
        }
      }
      if (e.ctrlKey && e.shiftKey && e.key === "Q") {
        if (h.overlay()) return;
        e.preventDefault();
        const sid = h.focusedSurfaceId();
        if (sid != null) {
          h.closeSurface(sid);
          return;
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
        const fs = h.focusedSession();
        if (fs?.state === "exited") {
          e.preventDefault();
          void h.workspace.closeSession(fs.id);
        }
      }
    };

    window.addEventListener("keydown", handler, true);
    onCleanup(() => window.removeEventListener("keydown", handler, true));
  });
}
