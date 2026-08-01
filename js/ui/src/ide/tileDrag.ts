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
