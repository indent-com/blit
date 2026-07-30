/**
 * dragReorder — pointer-driven list reordering, shared by {@link RemotesOverlay}
 * and {@link RootsOverlay}.
 *
 * HTML5 drag-and-drop never fires on touch, so on a phone or tablet these
 * lists could not be reordered at all. Pointer events cover mouse, pen and
 * touch through one path: the drag handle captures the pointer, the insertion
 * gap is derived from the live row rects, and the list edge-scrolls while a
 * captured pointer rests near its top or bottom — a captured touch can no
 * longer pan the container itself, so without that a long list would be
 * unreorderable past the visible rows.
 *
 * Handles must carry `touch-action: none`; preventDefault() on pointerdown
 * does not by itself stop a touch from scrolling the container.
 */

import { createSignal } from "solid-js";

/** Travel required before a press on a row *body* becomes a drag. */
const DRAG_THRESHOLD_PX = 4;
/** Band at the container's edges that auto-scrolls during a drag. */
const EDGE_BAND_PX = 28;
/** Auto-scroll speed inside that band. */
const EDGE_SPEED_PX_PER_S = 600;
/** Frame clamp, so a backgrounded tab doesn't resume with one huge jump. */
const MAX_FRAME_S = 0.05;

/** Vertical extent of one row; a `DOMRect` satisfies it. */
export interface RowExtent {
  readonly top: number;
  readonly bottom: number;
}

/**
 * Insertion gap for a pointer at `y`, given the rows' extents in list order.
 * Gap `i` means "before row i"; `extents.length` means "after the last row".
 * Crossing a row's midpoint moves into the gap below it, so the result is
 * monotonic in `y` even when rows are unequal heights or have space between
 * them.
 */
export function gapAt(y: number, extents: readonly RowExtent[]): number {
  for (let i = 0; i < extents.length; i++) {
    const { top, bottom } = extents[i];
    if (y < top + (bottom - top) / 2) return i;
  }
  return extents.length;
}

/**
 * `items` with the entry at `from` moved into insertion gap `gap`, or null if
 * that is a no-op (dropping onto either side of where it already sits) or the
 * indices don't address the list.
 */
export function reorderTo<T>(
  items: readonly T[],
  from: number,
  gap: number,
): T[] | null {
  if (from < 0 || from >= items.length || gap < 0 || gap > items.length)
    return null;
  // A gap below the source closes up by one once the source is removed.
  const to = gap > from ? gap - 1 : gap;
  if (to === from) return null;
  const next = items.slice();
  const [moved] = next.splice(from, 1);
  next.splice(to, 0, moved);
  return next;
}

export interface DragReorder {
  /** Row currently being dragged, or null. Reactive. */
  sourceIndex: () => number | null;
  /** Insertion gap under the pointer, or null when not dragging. Reactive. */
  dropGap: () => number | null;
  /** Whether dropping at `gap` would actually move the dragged row. */
  wouldMove: (gap: number) => boolean;
  /** `ref` for row `index`, in list order. */
  rowRef: (index: number) => (el: HTMLElement) => void;
  /** `ref` for the scrolling list container; enables edge auto-scroll. */
  containerRef: (el: HTMLElement) => void;
  /** `onPointerDown` for a drag handle — starts a drag for any pointer type. */
  onHandlePointerDown: (e: PointerEvent, index: number) => void;
  /**
   * `onPointerDown` for a row body — mouse only, and only past
   * {@link DRAG_THRESHOLD_PX}, so taps, text selection and one-finger panning
   * of the list keep working on touch.
   */
  onRowPointerDown: (e: PointerEvent, index: number) => void;
}

export function createDragReorder(opts: {
  /** Current row count. */
  count: () => number;
  /** When true, drags don't start (e.g. the config WS isn't up yet). */
  disabled?: () => boolean;
  /** Commit a drop: move `from` into insertion gap `gap`. */
  onDrop: (from: number, gap: number) => void;
}): DragReorder {
  const [sourceIndex, setSourceIndex] = createSignal<number | null>(null);
  const [dropGap, setDropGap] = createSignal<number | null>(null);

  const rows: (HTMLElement | undefined)[] = [];
  let container: HTMLElement | undefined;

  /** Live extents of the mounted rows, in list order. */
  function extents(): RowExtent[] {
    const out: RowExtent[] = [];
    for (let i = 0; i < opts.count(); i++) {
      const el = rows[i];
      if (el) out.push(el.getBoundingClientRect());
    }
    return out;
  }

  function begin(e: PointerEvent, index: number, capture: HTMLElement) {
    let settled = false;
    let raf = 0;
    let lastFrame = 0;
    let pointerY = e.clientY;

    capture.setPointerCapture(e.pointerId);
    setSourceIndex(index);
    setDropGap(gapAt(pointerY, extents()));

    // Auto-scroll while the pointer sits in an edge band. Runs off rAF rather
    // than off pointermove so it keeps scrolling with the finger held still.
    const tick = (now: number) => {
      raf = 0;
      if (!container) return;
      const dt = lastFrame
        ? Math.min(MAX_FRAME_S, (now - lastFrame) / 1000)
        : 0;
      lastFrame = now;
      const box = container.getBoundingClientRect();
      const dir =
        pointerY < box.top + EDGE_BAND_PX
          ? -1
          : pointerY > box.bottom - EDGE_BAND_PX
            ? 1
            : 0;
      if (dir === 0) {
        lastFrame = 0;
        return;
      }
      const before = container.scrollTop;
      container.scrollTop = before + dir * EDGE_SPEED_PX_PER_S * dt;
      // The first frame only establishes `lastFrame` (dt is 0), so keep the
      // chain alive for it; a later frame that fails to move means the scroll
      // range is exhausted and holding there can't help.
      if (dt === 0 || container.scrollTop !== before) {
        // Rows moved under the pointer, so the gap has to be recomputed.
        setDropGap(gapAt(pointerY, extents()));
        raf = requestAnimationFrame(tick);
      } else {
        lastFrame = 0;
      }
    };

    const pump = () => {
      if (raf || !container) return;
      const box = container.getBoundingClientRect();
      if (
        pointerY < box.top + EDGE_BAND_PX ||
        pointerY > box.bottom - EDGE_BAND_PX
      ) {
        lastFrame = 0;
        raf = requestAnimationFrame(tick);
      }
    };

    const onMove = (me: PointerEvent) => {
      if (me.pointerId !== e.pointerId) return;
      me.preventDefault();
      pointerY = me.clientY;
      setDropGap(gapAt(pointerY, extents()));
      pump();
    };

    const settle = (commit: boolean) => {
      if (settled) return;
      settled = true;
      capture.removeEventListener("pointermove", onMove);
      capture.removeEventListener("pointerup", onUp);
      capture.removeEventListener("pointercancel", onAbort);
      capture.removeEventListener("lostpointercapture", onAbort);
      if (raf) cancelAnimationFrame(raf);
      raf = 0;
      const from = sourceIndex();
      const gap = dropGap();
      setSourceIndex(null);
      setDropGap(null);
      if (commit && from !== null && gap !== null) opts.onDrop(from, gap);
    };

    const onUp = (ue: PointerEvent) => {
      if (ue.pointerId !== e.pointerId) return;
      settle(true);
    };
    // pointercancel, or capture lost because the row unmounted mid-drag.
    const onAbort = () => settle(false);

    capture.addEventListener("pointermove", onMove);
    capture.addEventListener("pointerup", onUp);
    capture.addEventListener("pointercancel", onAbort);
    capture.addEventListener("lostpointercapture", onAbort);
  }

  const onHandlePointerDown = (e: PointerEvent, index: number) => {
    if (opts.disabled?.() || sourceIndex() !== null) return;
    if (e.pointerType === "mouse" && e.button !== 0) return;
    // The handle does nothing but reorder, so claim the gesture outright.
    e.preventDefault();
    begin(e, index, e.currentTarget as HTMLElement);
  };

  const onRowPointerDown = (e: PointerEvent, index: number) => {
    if (opts.disabled?.() || sourceIndex() !== null) return;
    // Touch keeps row-body gestures for scrolling; only the handle drags.
    if (e.pointerType !== "mouse" || e.button !== 0) return;
    const target = e.target as HTMLElement | null;
    if (target?.closest("button, a, input, select, textarea, [role=button]"))
      return;
    const row = e.currentTarget as HTMLElement;
    const startX = e.clientX;
    const startY = e.clientY;

    const watch = (me: PointerEvent) => {
      if (me.pointerId !== e.pointerId) return;
      const travel =
        Math.abs(me.clientX - startX) + Math.abs(me.clientY - startY);
      if (travel < DRAG_THRESHOLD_PX) return;
      stop();
      begin(me, index, row);
    };
    const stop = () => {
      row.removeEventListener("pointermove", watch);
      row.removeEventListener("pointerup", stop);
      row.removeEventListener("pointercancel", stop);
    };
    row.addEventListener("pointermove", watch);
    row.addEventListener("pointerup", stop);
    row.addEventListener("pointercancel", stop);
  };

  return {
    sourceIndex,
    dropGap,
    wouldMove: (gap: number) => {
      const from = sourceIndex();
      return from !== null && gap !== from && gap !== from + 1;
    },
    rowRef: (index: number) => (el: HTMLElement) => {
      rows[index] = el;
    },
    containerRef: (el: HTMLElement) => {
      container = el;
    },
    onHandlePointerDown,
    onRowPointerDown,
  };
}
