export const PANE_TOOL_CORNERS = [
  "top-right",
  "bottom-right",
  "bottom-left",
  "top-left",
] as const;

export type PaneToolCorner = (typeof PANE_TOOL_CORNERS)[number];

export interface PaneToolBounds {
  left: number;
  right: number;
  top: number;
  bottom: number;
}

/** The corner represented by the quarter of the pane containing a point. */
export function paneToolCornerAtPoint(
  rect: PaneToolBounds,
  x: number,
  y: number,
): PaneToolCorner | null {
  if (x < rect.left || x > rect.right || y < rect.top || y > rect.bottom)
    return null;

  const horizontal = x < (rect.left + rect.right) / 2 ? "left" : "right";
  const vertical = y < (rect.top + rect.bottom) / 2 ? "top" : "bottom";
  return `${vertical}-${horizontal}`;
}
