/**
 * Drag-and-drop of BSP pane assignments.
 *
 * Any element that opens a tile on click (an explorer file, a changed file, a
 * problem, a commit, a reference) can also be *dragged* onto a BSP pane to open
 * there instead of the default target. Sources call {@link startTileDrag} with
 * the same assignment they'd pass to `onOpenTile`; pane drop zones read it with
 * {@link tileDragAssignment} and route it to that specific pane.
 *
 * The payload is a pane assignment, not strictly an IDE tile: the preview
 * panel's parked cards drag terminals (a bare session id) and surfaces the
 * same way, since a pane assignment holds any of them and BSPContainer's
 * moveToPane is agnostic. Hence the deliberately dumb payload — one opaque
 * string, interpreted only where it lands.
 */

/** Custom MIME so we only accept our own tile drags (not arbitrary text). */
export const TILE_DND_MIME = "application/x-blit-tile";

/** Write a tile assignment into a transfer. Shared by the native path and the
 *  touch bridge, which has a `DataTransfer` but no `DragEvent`. */
export function fillTileDrag(dt: DataTransfer, assignment: string): void {
  dt.setData(TILE_DND_MIME, assignment);
  dt.setData("text/plain", assignment); // harmless fallback for other targets
  dt.effectAllowed = "copyMove";
}

/** Mark a drag as carrying a tile assignment. Attach to `onDragStart`. */
export function startTileDrag(e: DragEvent, assignment: string): void {
  if (e.dataTransfer) fillTileDrag(e.dataTransfer, assignment);
}

/** The tile assignment a drag carries, or null if it isn't one of ours. */
export function tileDragAssignment(e: DragEvent): string | null {
  return e.dataTransfer?.getData(TILE_DND_MIME) || null;
}

/** True if the drag carries a tile assignment (checked in `onDragOver`, where
 *  the payload isn't readable — so we look at the offered types instead). */
export function isTileDrag(e: DragEvent): boolean {
  return e.dataTransfer?.types.includes(TILE_DND_MIME) ?? false;
}

/** Custom MIME naming the BSP pane a drag started from. A drag whose source
 *  is a pane is a *move* of that pane's content, not another open: the drop
 *  swaps the two panes' assignments, so the content lands in exactly one
 *  place (a swap with an empty pane is a plain move). */
export const PANE_SOURCE_DND_MIME = "application/x-blit-pane-source";

/** Mark a drag as carrying a pane's own assignment: the tile payload plus
 *  the pane it is leaving. Attach to `onDragStart`. */
export function startPaneTileDrag(
  e: DragEvent,
  assignment: string,
  sourcePaneId: string,
): void {
  startTileDrag(e, assignment);
  e.dataTransfer?.setData(PANE_SOURCE_DND_MIME, sourcePaneId);
}

/** The pane a drag left, or null when the drag is not a pane's content
 *  (readable only on `drop`). */
export function paneDragSource(e: DragEvent): string | null {
  return e.dataTransfer?.getData(PANE_SOURCE_DND_MIME) || null;
}

/** True if the drag is a pane's content (checked in `onDragOver`, where the
 *  payload isn't readable — so we look at the offered types instead). */
export function isPaneDrag(e: DragEvent): boolean {
  return e.dataTransfer?.types.includes(PANE_SOURCE_DND_MIME) ?? false;
}

/** The `paneDragSource` of the non-BSP main view. BSP pane ids are index
 *  paths ("0", "1.0") from enumeratePanes, so this cannot collide. */
export const MAIN_PANE_SOURCE = "main-view";

/**
 * Travel before a press becomes a drag rather than a tap.
 *
 * Deliberately looser than dragReorder's 4px, which measures a press on a
 * whole list row. This handle is one ~24px button whose tap means something
 * else — cycle the toolbar's corner — and a drag swallows that click, so
 * treating a wobbling finger as a drag makes the tap look broken. Erring
 * toward "that was a tap" is the cheaper mistake here.
 */
const TOUCH_DRAG_THRESHOLD_PX = 6;

/**
 * How a touch drag starts.
 *
 * `move` suits a dedicated handle: it can carry `touch-action: none` and has
 * no competing gesture, so any movement means the drag.
 *
 * `long-press` suits anything living in a scrollable list — explorer rows,
 * commits, dock cards. Those must keep scrolling (and, for a dock card,
 * swiping to dismiss), so a drag cannot begin on movement without stealing
 * it. Holding still is the one gesture none of them claim.
 */
export type TouchDragActivation = "move" | "long-press";

/** Hold before a press on a list row becomes a drag. Long enough not to fire
 *  during a flick, short enough not to feel broken. */
const LONG_PRESS_MS = 450;
/** Movement during the hold that means "this is a scroll, not a drag". */
const LONG_PRESS_SLOP_PX = 10;

/**
 * Drag anything with a finger.
 *
 * HTML5 drag-and-drop never fires from touch, so every `draggable` in this app
 * was mouse-only: pane grips, explorer rows, changed files, search hits,
 * problems, commits, dock cards. `dragReorder` hit this first and solved it
 * for the remotes and roots lists, but the rest cannot borrow that solution —
 * their drop targets are scattered across BSPContainer, Workspace's main view,
 * the dock and the explorer's own directory rows, each already wired to
 * `dragover`/`drop`.
 *
 * So rather than a second drop protocol, this synthesises the first: one
 * `DataTransfer` — filled by the caller, exactly as its `onDragStart` would —
 * carried across real `DragEvent`s dispatched at whatever `elementFromPoint`
 * reports under the finger. Every existing handler runs unchanged, including
 * the window-level listeners that reveal the dock as a park target. They
 * cannot tell the difference, which is the point.
 *
 * Mouse and pen keep the native path (real drag image, edge autoscroll); this
 * takes over only for touch. See {@link TouchDragActivation} for why a handle
 * and a list row start differently.
 */
export function startTouchDrag(
  e: PointerEvent,
  fill: (data: DataTransfer) => void,
  activate: TouchDragActivation = "move",
): void {
  // Touch only, and tested positively rather than by excluding mouse: a pen
  // drives native drag-and-drop in Chromium just as a mouse does, so letting
  // it in here would run both paths at once — and this one's `dragend` would
  // clear the in-flight count and unmount the dock underneath the native
  // drag, which is the failure the enter-before-leave ordering below exists
  // to avoid.
  if (e.pointerType !== "touch") return;
  const handle = e.currentTarget as HTMLElement | null;
  if (!handle || typeof DataTransfer !== "function") return;

  const data = new DataTransfer();
  fill(data);

  const startX = e.clientX;
  const startY = e.clientY;
  let dragging = false;
  let over: Element | null = null;
  let holdTimer: ReturnType<typeof setTimeout> | null = null;
  let last: PointerEvent = e;

  const blockTouch = (ev: Event) => {
    if (!dragging) return;
    if (ev.cancelable) ev.preventDefault();
    ev.stopPropagation();
  };
  // A long press is also how Android offers text selection and a context
  // menu; neither is what the finger asked for once a drag is under way.
  const blockMenu = (ev: Event) => {
    if (dragging) ev.preventDefault();
  };

  const fire = (target: EventTarget | null, type: string, ev: PointerEvent) => {
    target?.dispatchEvent(
      new DragEvent(type, {
        dataTransfer: data,
        bubbles: true,
        cancelable: true,
        clientX: ev.clientX,
        clientY: ev.clientY,
      }),
    );
  };

  const moved = (ev: PointerEvent, by: number) =>
    Math.abs(ev.clientX - startX) > by || Math.abs(ev.clientY - startY) > by;

  const begin = (ev: PointerEvent) => {
    dragging = true;
    handle.setPointerCapture?.(ev.pointerId);
    // Only now does the page stop scrolling under the finger. A row cannot
    // carry `touch-action: none` the way a dedicated handle can — that would
    // cost the list its scrolling — so the block goes on for the drag's
    // duration instead. Capture-phase and non-passive: passive listeners
    // cannot preventDefault, and stopping propagation keeps a row's own touch
    // gestures (the dock card's swipe-to-dismiss) from reading the same move.
    window.addEventListener("touchmove", blockTouch, {
      passive: false,
      capture: true,
    });
    window.addEventListener("contextmenu", blockMenu, { capture: true });
    fire(handle, "dragstart", ev);
  };

  const onMove = (ev: PointerEvent) => {
    if (ev.pointerId !== e.pointerId) return;
    last = ev;
    if (!dragging) {
      if (activate === "long-press") {
        // Moving before the hold completes means this was a scroll or a
        // swipe, which the page is entitled to handle instead.
        if (moved(ev, LONG_PRESS_SLOP_PX)) stop();
        return;
      }
      if (!moved(ev, TOUCH_DRAG_THRESHOLD_PX)) return;
      begin(ev);
    }
    // Capture routes the pointer events here, but hit-testing is ours to do.
    const el = document.elementFromPoint(ev.clientX, ev.clientY);
    if (el !== over) {
      // Enter the new target before leaving the old, as the real sequence
      // does. The order is not cosmetic: listeners depth-count enter/leave to
      // decide a drag is in flight, and leaving first drops that count to
      // zero — which unmounts the dock, the very target being dragged to.
      if (el) fire(el, "dragenter", ev);
      if (over) fire(over, "dragleave", ev);
      over = el;
    }
    if (el) fire(el, "dragover", ev);
  };

  const onUp = (ev: PointerEvent) => {
    if (ev.pointerId !== e.pointerId) return;
    stop();
    if (!dragging) return; // a tap: leave the click alone
    const el = document.elementFromPoint(ev.clientX, ev.clientY);
    if (el) fire(el, "drop", ev);
    fire(handle, "dragend", ev);
    // The release also produces a click, which on this handle means "move the
    // toolbar to the next corner" — not what a completed drag asked for. The
    // native path gets this for free; here it has to be swallowed.
    const swallow = (click: Event) => {
      click.stopPropagation();
      click.preventDefault();
    };
    handle.addEventListener("click", swallow, { capture: true, once: true });
    setTimeout(
      () => handle.removeEventListener("click", swallow, { capture: true }),
      0,
    );
  };

  const onCancel = (ev: PointerEvent) => {
    if (ev.pointerId !== e.pointerId) return;
    stop();
    if (!dragging) return;
    if (over) fire(over, "dragleave", ev);
    fire(handle, "dragend", ev);
  };

  function stop() {
    if (holdTimer !== null) {
      clearTimeout(holdTimer);
      holdTimer = null;
    }
    window.removeEventListener("pointermove", onMove);
    window.removeEventListener("pointerup", onUp);
    window.removeEventListener("pointercancel", onCancel);
    window.removeEventListener("touchmove", blockTouch, { capture: true });
    window.removeEventListener("contextmenu", blockMenu, { capture: true });
  }

  window.addEventListener("pointermove", onMove);
  window.addEventListener("pointerup", onUp);
  window.addEventListener("pointercancel", onCancel);
  if (activate === "long-press") {
    holdTimer = setTimeout(() => {
      holdTimer = null;
      // Still down and still still: the press was a request to drag, not to
      // scroll past. Everything after this point is the same as a handle's.
      if (!dragging) begin(last);
    }, LONG_PRESS_MS);
  }
}

/**
 * Drag a pane's content with a finger — the grip's own entry point.
 *
 * A dedicated handle, so it activates on movement rather than on a hold: it
 * carries `touch-action: none` and its only other gesture is a tap.
 */
export function startPaneTouchDrag(
  e: PointerEvent,
  assignment: string,
  sourcePaneId: string,
): void {
  startTouchDrag(e, (data) => {
    fillTileDrag(data, assignment);
    data.setData(PANE_SOURCE_DND_MIME, sourcePaneId);
  });
}

/** Custom MIME for explorer move drags: a file/dir dragged onto a directory
 *  row moves it there (docs/design/fs-write.md: rename and move are one op). */
export const FS_MOVE_DND_MIME = "application/x-blit-fs-move";

export interface FsMovePayload {
  connectionId: string;
  /** The session root the path is relative to — drops only apply within
   *  the same tree. */
  root: string;
  relPath: string;
}

/** Write an explorer path into a transfer (see {@link fillTileDrag}). */
export function fillFsMoveDrag(dt: DataTransfer, payload: FsMovePayload): void {
  dt.setData(FS_MOVE_DND_MIME, JSON.stringify(payload));
}

/** Mark a drag as carrying an explorer path (alongside any tile payload). */
export function addFsMoveDrag(e: DragEvent, payload: FsMovePayload): void {
  if (e.dataTransfer) fillFsMoveDrag(e.dataTransfer, payload);
}

/** The move payload a drop carries, or null (readable only on `drop`). */
export function fsMovePayload(e: DragEvent): FsMovePayload | null {
  const raw = e.dataTransfer?.getData(FS_MOVE_DND_MIME);
  if (!raw) return null;
  try {
    return JSON.parse(raw) as FsMovePayload;
  } catch {
    return null;
  }
}

/** True if the drag carries an explorer path (checked in `onDragOver`). */
export function isFsMoveDrag(e: DragEvent): boolean {
  return e.dataTransfer?.types.includes(FS_MOVE_DND_MIME) ?? false;
}
