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
  /** Send Ctrl-K to the terminal or Wayland surface in the focused pane. */
  forwardCtrlK: () => void;
  cancelOverlay: () => void;
  toggleDebug: () => void;
  togglePreviewPanel: () => void;
  toggleLeftPanel: (panel: "explorer" | "log" | "problems") => void;
  /** Show/hide the project-search top pane. */
  toggleSearch: () => void;
  createAndFocus: () => Promise<void>;
  createInPane: (paneId: string) => Promise<void>;
  openNewTerminalPicker: (paneId?: string) => void;
  handleRestartOrClose: () => void;
  connectionCount: () => number;
  /**
   * Everything open, as pane assignments, in a stable order: terminals, then
   * Wayland surfaces, then tabs (editors, diffs, commits, web panes). This is
   * the ring Alt+Shift+[ / ] walks.
   */
  cycleRing: () => readonly string[];
  /** What the focused slot (BSP pane, or the single main view) is showing. */
  focusedAssignment: () => string | null;
  /** Show an assignment of any kind in the focused slot, and focus it. */
  focusAssignment: (assignment: string) => void;
  /** Clear the assignment for the focused BSP pane (remove term without closing) */
  clearFocusedPaneAssignment: () => void;
  /**
   * Send the focused IDE tile (non-BSP focused tile, or a tile occupying the
   * focused BSP pane) to the recoverable background list. Returns true if a
   * tile was backgrounded (so the caller stops handling the key).
   */
  backgroundFocusedTile: () => boolean;
  /** Close the focused IDE tile outright (no dock parking). */
  closeFocusedTile: () => boolean;
  /** Navigate the focused tile pane's history back / forward (like a browser). */
  navigateBack: () => void;
  navigateForward: () => void;
}

type SurfaceFocusHandlers = Pick<
  KeyboardShortcutHandlers,
  "focusedSurfaceId" | "bspFocusedPaneId" | "layoutAssignments"
>;

/** Whether keyboard input currently belongs to a Wayland surface. */
export function hasFocusedWaylandSurface(h: SurfaceFocusHandlers): boolean {
  if (h.focusedSurfaceId() != null) return true;
  const paneId = h.bspFocusedPaneId();
  if (!paneId) return false;
  const assignment = h.layoutAssignments()?.assignments[paneId] ?? null;
  return isSurfaceAssignment(assignment);
}

export function shouldHandleNewTerminalShortcut(
  h: SurfaceFocusHandlers,
): boolean {
  return !hasFocusedWaylandSurface(h);
}

/** The second Ctrl-K closes the switcher and belongs to the pane underneath. */
export function shouldForwardClosingSwitcherCtrlK(
  current: Overlay,
  event: Pick<KeyboardEvent, "ctrlKey" | "metaKey">,
): boolean {
  return current === "expose" && event.ctrlKey && !event.metaKey;
}

/** Both Ctrl-K and Cmd-K open the switcher on every platform. */
export function isSwitcherShortcut(
  event: Pick<KeyboardEvent, "ctrlKey" | "metaKey" | "shiftKey" | "key">,
): boolean {
  return (
    (event.ctrlKey || event.metaKey) && !event.shiftKey && event.key === "k"
  );
}

/**
 * The next thing Alt+Shift+[ / ] should show in the focused slot, or null when
 * there is nothing to move to.
 *
 * `ring` is everything open; `displayedElsewhere` is what the OTHER BSP panes
 * are already showing, which is excluded — the chord rotates the focused pane's
 * occupant, tiling-WM style, and pulling in a window that is already on screen
 * beside it would only shuffle the two. In single-pane mode nothing is
 * elsewhere, so the ring is walked whole.
 *
 * `current` outside the ring (nothing focused, or a parked view) enters at the
 * near end rather than skipping the first step.
 */
export function nextCycleTarget(
  ring: readonly string[],
  current: string | null,
  direction: 1 | -1,
  displayedElsewhere: ReadonlySet<string> = new Set(),
): string | null {
  const candidates = ring.filter((a) => !displayedElsewhere.has(a));
  if (candidates.length === 0) return null;
  const index = current == null ? -1 : candidates.indexOf(current);
  if (index < 0) {
    return direction === 1 ? candidates[0] : candidates[candidates.length - 1];
  }
  if (candidates.length < 2) return null;
  return candidates[
    (index + direction + candidates.length) % candidates.length
  ];
}

/**
 * Installs global keyboard shortcuts for the workspace.
 * Must be called inside a Solid component (uses onMount/onCleanup).
 */
export function createKeyboardShortcuts(h: KeyboardShortcutHandlers): void {
  onMount(() => {
    const eventElement = (target: EventTarget | null): Element | null => {
      if (target instanceof Element) return target;
      return document.activeElement instanceof Element
        ? document.activeElement
        : null;
    };

    const isTerminalInput = (el: Element | null): boolean =>
      el?.tagName === "TEXTAREA" &&
      el.getAttribute("aria-label") === "Terminal input";

    const isEnterInputEvent = (e: InputEvent): boolean =>
      e.inputType === "insertLineBreak" ||
      e.inputType === "insertParagraph" ||
      e.data === "\n" ||
      e.data === "\r";

    const isEnterKeyEvent = (e: KeyboardEvent): boolean =>
      e.key === "Enter" ||
      e.key === "Return" ||
      e.code === "Enter" ||
      e.code === "NumpadEnter" ||
      e.keyCode === 13;

    const textareaHasLineBreak = (target: Element | null): boolean =>
      target instanceof HTMLTextAreaElement &&
      (target.value.includes("\n") || target.value.includes("\r"));

    const isReservedInputTarget = (el: Element | null): boolean => {
      const tag = el?.tagName;
      return (
        tag === "INPUT" ||
        tag === "TEXTAREA" ||
        tag === "SELECT" ||
        tag === "CANVAS" ||
        tag === "BUTTON"
      );
    };

    const shouldHandleRestartEnter = (target: Element | null): boolean => {
      if (h.overlay()) return false;
      // When a surface is focused, Enter is application input.
      if (hasFocusedWaylandSurface(h)) return false;

      const fid = h.focusedSessionId();
      const focused = fid ? h.sessions().find((s) => s.id === fid) : null;
      if (!((focused && focused.state === "exited") || fid == null)) {
        return false;
      }

      // Don't steal Enter from normal inputs (e.g. EmptyPane command input) or
      // buttons.  The exception is the hidden terminal textarea: iPadOS keeps
      // it focused and sends the software keyboard Return key as text input,
      // so an exited terminal must be allowed to restart from there.
      if (isReservedInputTarget(target)) {
        return (
          isTerminalInput(target) &&
          (focused?.state === "exited" || fid == null)
        );
      }

      return true;
    };

    const handleRestartEnter = (e: Event): boolean => {
      if (!shouldHandleRestartEnter(eventElement(e.target))) return false;
      e.preventDefault();
      // If this is an input/input-like event, stop it before BlitTerminalSurface's
      // textarea listener forwards the inserted newline to the exited PTY.
      e.stopImmediatePropagation();
      h.handleRestartOrClose();
      return true;
    };
    const handler = (e: KeyboardEvent) => {
      const mod = e.metaKey || e.ctrlKey;
      // Wayland applications own their Ctrl+Shift chords (Zed's command
      // palette is Ctrl+Shift+P): while a surface has keyboard focus, blit's
      // dock/overlay shortcuts must not steal them.
      const surfaceOwnsCtrlShift = hasFocusedWaylandSurface(h);

      if (isSwitcherShortcut(e)) {
        const forward = shouldForwardClosingSwitcherCtrlK(h.overlay(), e);
        e.preventDefault();
        h.toggleOverlay("expose");
        if (forward) h.forwardCtrlK();
        return;
      }
      if (
        !surfaceOwnsCtrlShift &&
        e.ctrlKey &&
        e.shiftKey &&
        (e.key === "?" || e.code === "Slash")
      ) {
        e.preventDefault();
        h.toggleOverlay("help");
        return;
      }
      if (
        !surfaceOwnsCtrlShift &&
        e.ctrlKey &&
        e.shiftKey &&
        (e.key === "~" || e.key === "`")
      ) {
        e.preventDefault();
        h.toggleDebug();
        return;
      }
      if (!surfaceOwnsCtrlShift && e.ctrlKey && e.shiftKey && e.key === "B") {
        e.preventDefault();
        h.togglePreviewPanel();
        return;
      }
      // Ctrl+Shift+E/L/P: reveal a left-dock section. (Changes is folded into
      // Files, so its former Ctrl+Shift+G is retired.)
      if (!surfaceOwnsCtrlShift && e.ctrlKey && e.shiftKey && e.key === "E") {
        e.preventDefault();
        h.toggleLeftPanel("explorer");
        return;
      }
      if (!surfaceOwnsCtrlShift && e.ctrlKey && e.shiftKey && e.key === "F") {
        e.preventDefault();
        h.toggleSearch();
        return;
      }
      if (!surfaceOwnsCtrlShift && e.ctrlKey && e.shiftKey && e.key === "L") {
        e.preventDefault();
        h.toggleLeftPanel("log");
        return;
      }
      if (!surfaceOwnsCtrlShift && e.ctrlKey && e.shiftKey && e.key === "P") {
        e.preventDefault();
        h.toggleLeftPanel("problems");
        return;
      }
      // Ctrl+Alt+←/→: navigate the focused tile pane's history (back/forward).
      if (e.ctrlKey && e.altKey && !e.shiftKey && e.key === "ArrowLeft") {
        e.preventDefault();
        h.navigateBack();
        return;
      }
      if (e.ctrlKey && e.altKey && !e.shiftKey && e.key === "ArrowRight") {
        e.preventDefault();
        h.navigateForward();
        return;
      }
      // Ctrl+Shift+O: open a URL as a web pane. (Chrome binds this to its
      // bookmark manager on Windows/Linux; it is one line to change here.)
      if (
        !surfaceOwnsCtrlShift &&
        e.ctrlKey &&
        e.shiftKey &&
        !e.altKey &&
        !e.metaKey &&
        e.key === "O"
      ) {
        e.preventDefault();
        h.toggleOverlay("web");
        return;
      }
      // Workspace roots have no shortcut of their own: they are an entry in
      // the Cmd+K switcher, alongside remotes, palette, and font.
      if (mod && !e.shiftKey && e.key === "Enter") {
        if (h.overlay()) {
          // Let the overlay handle it.
          e.preventDefault();
          return;
        }
        // Wayland applications own their modified Enter chords. In
        // particular, Ctrl+Enter must not create a terminal behind them.
        if (!shouldHandleNewTerminalShortcut(h)) return;
        e.preventDefault();
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
        if (!h.overlay() && !shouldHandleNewTerminalShortcut(h)) return;
        e.preventDefault();
        if (h.activeLayout() && h.bspFocusedPaneId()) {
          void h.createInPane(h.bspFocusedPaneId()!);
        } else {
          void h.createAndFocus();
        }
        return;
      }
      if (isEnterKeyEvent(e) && !mod && !e.shiftKey) {
        // Enter on an exited session restarts/closes it (works in BSP layouts too).
        if (handleRestartEnter(e)) return;
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
        if (h.overlay()) return;
        // IDE tile first: a non-BSP focused tile, or a tile in the focused BSP
        // pane, goes to the recoverable background list (Cmd+K to restore).
        if (h.backgroundFocusedTile()) {
          e.preventDefault();
          return;
        }
        // Non-BSP surface focus: unfocus the surface (return to terminal view).
        if (h.focusedSurfaceId() != null) {
          e.preventDefault();
          h.unfocusSurface();
          return;
        }
        if (h.activeLayout() && h.bspFocusedPaneId()) {
          e.preventDefault();
          h.clearFocusedPaneAssignment();
          return;
        }
        // Single-terminal mode: unfocus the current session so the main
        // area shows the EmptyState and the terminal lives only in the
        // sidebar.
        if (h.focusedSessionId() != null) {
          e.preventDefault();
          h.workspace.focusSession(null);
          return;
        }
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
        // IDE tile first, same precedence as Ctrl+Shift+Q above — otherwise
        // the chord fell through to the focused *session* and closed a
        // terminal out from under an editor that had focus.
        if (h.closeFocusedTile()) return;
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
      // Prev/next window: Alt+Shift+[ / ] on all platforms. "Window" is every
      // kind the workspace holds — terminals, Wayland surfaces, editors, diffs,
      // commits, web panes — not just terminals, so the chord reaches whatever
      // is open rather than stranding you on the one kind it knew about.
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
        const fpId = h.bspFocusedPaneId();
        const la = h.layoutAssignments();
        const elsewhere = new Set<string>();
        if (la && fpId) {
          for (const [pid, value] of Object.entries(la.assignments)) {
            if (pid !== fpId && value != null) elsewhere.add(value);
          }
        }
        const next = nextCycleTarget(
          h.cycleRing(),
          h.focusedAssignment(),
          e.code === "BracketRight" ? 1 : -1,
          elsewhere,
        );
        if (next != null) h.focusAssignment(next);
        return;
      }
      if (e.key === "Escape") {
        if (h.overlay()) {
          e.preventDefault();
          h.cancelOverlay();
          return;
        }
        // Do not capture Escape while a Wayland surface is focused: many
        // apps rely on it, and BlitSurfaceCanvas will forward it if the
        // event is left unhandled here. Use Ctrl+Shift+Q to return to the
        // terminal view without sending input to the surface.
        if (h.focusedSurfaceId() != null) {
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

    const beforeInputHandler = (e: InputEvent) => {
      if (!isEnterInputEvent(e)) return;
      // iPadOS software keyboard Return in a textarea may arrive as
      // beforeinput/input rather than a useful keydown.  Mirror the keydown
      // restart path before the textarea inserts a newline.
      handleRestartEnter(e);
    };

    const inputHandler = (e: Event) => {
      const target = eventElement(e.target);
      if (!isTerminalInput(target)) return;
      const inputEvent =
        typeof InputEvent !== "undefined" && e instanceof InputEvent ? e : null;
      if (!textareaHasLineBreak(target) && !inputEvent?.inputType) return;
      if (
        inputEvent &&
        !isEnterInputEvent(inputEvent) &&
        !textareaHasLineBreak(target)
      ) {
        return;
      }
      if (handleRestartEnter(e) && target instanceof HTMLTextAreaElement) {
        target.value = "";
      }
    };

    window.addEventListener("keydown", handler, true);
    window.addEventListener("beforeinput", beforeInputHandler, true);
    window.addEventListener("input", inputHandler, true);
    onCleanup(() => {
      window.removeEventListener("keydown", handler, true);
      window.removeEventListener("beforeinput", beforeInputHandler, true);
      window.removeEventListener("input", inputHandler, true);
    });
  });
}
