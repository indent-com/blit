/**
 * One animation frame for every terminal surface, split into a read phase
 * and a write phase.
 *
 * Each surface used to own its own `requestAnimationFrame`, and each frame
 * it read layout (the canvas's box, for device-pixel snapping) and then
 * wrote layout (canvas size, scroll spacer, transform). One surface doing
 * that is fine — the read happens while layout is clean. Several surfaces
 * doing it in sequence is not: pane 2's read is forced to lay out the whole
 * document again because pane 1 just wrote to it, and so on down the line.
 * A profile of a window drag put ~69% of the time in Layout with almost no
 * script self-time, which is what that looks like.
 *
 * So the frame is staged instead. Every registered surface measures first,
 * then every surface writes. The browser lays out at most once per frame
 * regardless of how many panes are on screen, because no read ever follows
 * a write within the frame.
 *
 * The contract is the whole value here, and it is not enforceable by types:
 * `measureFrame` must not touch the DOM in any way that invalidates layout,
 * and `paintFrame` must not read it back. Both halves are ordinary methods,
 * so the only thing keeping the phases honest is that they stay small
 * enough to check by eye.
 */

export interface FrameParticipant {
  /** Read layout. Must not write anything that invalidates it. */
  measureFrame(): void;
  /** Write DOM and paint. Must not read layout back. */
  paintFrame(): void;
}

const pending = new Set<FrameParticipant>();
let raf = 0;

/** Ask for `p` to be measured and painted on the next frame. */
export function scheduleFrame(p: FrameParticipant): void {
  pending.add(p);
  if (raf !== 0) return;
  raf = requestAnimationFrame(runFrame);
}

/** Drop `p` from the next frame — call on teardown. */
export function cancelFrame(p: FrameParticipant): void {
  pending.delete(p);
  if (pending.size === 0 && raf !== 0) {
    cancelAnimationFrame(raf);
    raf = 0;
  }
}

function runFrame(): void {
  raf = 0;
  // Snapshot before running anything: a paint routinely schedules the next
  // frame, and a surface can be disposed from inside its own paint, either
  // of which would mutate the set mid-iteration.
  const participants = [...pending];
  pending.clear();

  for (const p of participants) {
    try {
      p.measureFrame();
    } catch {
      // One surface's failure must not cost every other surface its frame.
    }
  }
  for (const p of participants) {
    try {
      p.paintFrame();
    } catch {
      // As above: a dead pane should not freeze the live ones.
    }
  }
}

/** Test seam: how many surfaces are waiting on the next frame. */
export function pendingFrameCount(): number {
  return pending.size;
}
