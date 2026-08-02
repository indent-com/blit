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

/** Mark a drag as carrying a tile assignment. Attach to `onDragStart`. */
export function startTileDrag(e: DragEvent, assignment: string): void {
  const dt = e.dataTransfer;
  if (!dt) return;
  dt.setData(TILE_DND_MIME, assignment);
  dt.setData("text/plain", assignment); // harmless fallback for other targets
  dt.effectAllowed = "copyMove";
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
 * Drag a pane's content with a finger.
 *
 * HTML5 drag-and-drop never fires from touch, so on Android the grip could be
 * tapped (which cycles the toolbar's corner) but not dragged — every pane
 * move and every park was mouse-only. `dragReorder` hit this first and solved
 * it for lists; panes need the same, but they cannot borrow that solution:
 * their drop targets are scattered across BSPContainer, Workspace's main view
 * and the dock, each already wired to `dragover`/`drop`.
 *
 * So rather than a second drop protocol, this synthesises the first: one
 * `DataTransfer` carried across real `DragEvent`s dispatched at whatever
 * `elementFromPoint` reports under the finger. Every existing handler runs
 * unchanged, including the window-level listeners that reveal the dock as a
 * park target — they cannot tell the difference, which is the point.
 *
 * Mouse and pen keep the native path (real drag image, edge autoscroll); this
 * takes over only for touch. The handle must carry `touch-action: none`, or
 * the browser pans the page instead of reporting the move.
 */
export function startPaneTouchDrag(
  e: PointerEvent,
  assignment: string,
  sourcePaneId: string,
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
  data.setData(TILE_DND_MIME, assignment);
  data.setData("text/plain", assignment);
  data.setData(PANE_SOURCE_DND_MIME, sourcePaneId);
  data.effectAllowed = "copyMove";

  const startX = e.clientX;
  const startY = e.clientY;
  let dragging = false;
  let over: Element | null = null;

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

  const onMove = (ev: PointerEvent) => {
    if (ev.pointerId !== e.pointerId) return;
    if (!dragging) {
      const far =
        Math.abs(ev.clientX - startX) > TOUCH_DRAG_THRESHOLD_PX ||
        Math.abs(ev.clientY - startY) > TOUCH_DRAG_THRESHOLD_PX;
      if (!far) return;
      dragging = true;
      handle.setPointerCapture?.(ev.pointerId);
      fire(handle, "dragstart", ev);
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
    window.removeEventListener("pointermove", onMove);
    window.removeEventListener("pointerup", onUp);
    window.removeEventListener("pointercancel", onCancel);
  }

  window.addEventListener("pointermove", onMove);
  window.addEventListener("pointerup", onUp);
  window.addEventListener("pointercancel", onCancel);
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

/** Mark a drag as carrying an explorer path (alongside any tile payload). */
export function addFsMoveDrag(e: DragEvent, payload: FsMovePayload): void {
  e.dataTransfer?.setData(FS_MOVE_DND_MIME, JSON.stringify(payload));
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
