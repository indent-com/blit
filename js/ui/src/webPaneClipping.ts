export interface ClipInsets {
  top: number;
  right: number;
  bottom: number;
  left: number;
}

interface RectEdges {
  top: number;
  right: number;
  bottom: number;
  left: number;
}

/** Insets needed to clip a fixed overlay to ancestor viewport bounds. */
export function clipInsetsFor(
  rect: RectEdges,
  clips: readonly RectEdges[],
): ClipInsets {
  let top = rect.top;
  let right = rect.right;
  let bottom = rect.bottom;
  let left = rect.left;
  for (const clip of clips) {
    top = Math.max(top, clip.top);
    right = Math.min(right, clip.right);
    bottom = Math.min(bottom, clip.bottom);
    left = Math.max(left, clip.left);
  }
  return {
    top: Math.max(0, Math.min(rect.bottom - rect.top, top - rect.top)),
    right: Math.max(0, Math.min(rect.right - rect.left, rect.right - right)),
    bottom: Math.max(0, Math.min(rect.bottom - rect.top, rect.bottom - bottom)),
    left: Math.max(0, Math.min(rect.right - rect.left, left - rect.left)),
  };
}
