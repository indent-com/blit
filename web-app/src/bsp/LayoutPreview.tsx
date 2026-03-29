import type { BSPNode } from "./dsl";

/**
 * Tiny visual preview of a BSP layout as nested rectangles.
 * Renders at the given width/height with 1px gaps between panes.
 */
export function LayoutPreview({
  node,
  width = 48,
  height = 32,
  color = "currentColor",
  opacity = 0.5,
  highlightIndex,
  highlightOpacity,
}: {
  node: BSPNode;
  width?: number;
  height?: number;
  color?: string;
  opacity?: number;
  /** Leaf index to highlight (brighter) in the preview. */
  highlightIndex?: number;
  /** Opacity for the highlighted leaf (defaults to 1). */
  highlightOpacity?: number;
}) {
  const rects: Array<{ x: number; y: number; w: number; h: number; leafIndex: number }> = [];
  let leafCounter = 0;
  const gap = 1;

  function layout(n: BSPNode, x: number, y: number, w: number, h: number) {
    if (n.type === "leaf") {
      rects.push({ x, y, w, h, leafIndex: leafCounter++ });
      return;
    }

    if (n.direction === "tabs") {
      // Show tab indicators at top, first child fills the rest.
      const tabH = Math.max(2, Math.round(h * 0.1));
      const tabW = Math.max(2, Math.round((w - gap * (n.children.length - 1)) / n.children.length));
      for (let i = 0; i < n.children.length; i++) {
        rects.push({ x: x + i * (tabW + gap), y, w: tabW, h: tabH, leafIndex: -1 });
      }
      if (n.children.length > 0) {
        layout(n.children[0].node, x, y + tabH + gap, w, h - tabH - gap);
      }
      return;
    }

    const totalWeight = n.children.reduce((sum, c) => sum + c.weight, 0);
    const isHoriz = n.direction === "horizontal";
    const totalGap = gap * (n.children.length - 1);
    const available = (isHoriz ? w : h) - totalGap;
    let offset = 0;

    for (let i = 0; i < n.children.length; i++) {
      const child = n.children[i];
      const size = Math.round((child.weight / totalWeight) * available);
      const actualSize = i === n.children.length - 1
        ? available - offset // last child gets remainder to avoid rounding gaps
        : size;

      if (isHoriz) {
        layout(child.node, x + offset, y, actualSize, h);
      } else {
        layout(child.node, x, y + offset, w, actualSize);
      }
      offset += actualSize + gap;
    }
  }

  layout(node, 0, 0, width, height);

  return (
    <svg width={width} height={height} style={{ flexShrink: 0 }}>
      {rects.map((r, i) => (
        <rect
          key={i}
          x={r.x}
          y={r.y}
          width={Math.max(0, r.w)}
          height={Math.max(0, r.h)}
          fill={color}
          opacity={r.leafIndex >= 0 && r.leafIndex === highlightIndex ? (highlightOpacity ?? 1) : opacity}
          rx={1}
        />
      ))}
    </svg>
  );
}
