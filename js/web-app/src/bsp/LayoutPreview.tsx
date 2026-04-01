import { leafCount, type BSPNode } from "@blit-sh/core/bsp";

/**
 * Tiny visual preview of a BSP layout as nested rectangles.
 * Renders at the given width/height with 1px gaps between panes.
 */
export function LayoutPreview({
  node,
  width = 48,
  height = 32,
  color = "currentColor",
  bg,
  highlightIndex,
}: {
  node: BSPNode;
  width?: number;
  height?: number;
  color?: string;
  /** Background color for gaps between panes. */
  bg?: string;
  /** Leaf index to highlight (filled) in the preview. */
  highlightIndex?: number;
}) {
  const rects: Array<{
    x: number;
    y: number;
    w: number;
    h: number;
    leafIndex: number;
  }> = [];
  let leafCounter = 0;
  const gap = 1;

  function layout(n: BSPNode, x: number, y: number, w: number, h: number) {
    if (n.type === "leaf") {
      rects.push({ x, y, w, h, leafIndex: leafCounter++ });
      return;
    }

    if (n.direction === "tabs") {
      const tabH = Math.max(2, Math.round(h * 0.1));
      const tabW = Math.max(
        2,
        Math.round((w - gap * (n.children.length - 1)) / n.children.length),
      );

      // Figure out which tab child contains the highlighted leaf.
      let activeChild = 0;
      if (highlightIndex != null) {
        let leafStart = leafCounter;
        for (let i = 0; i < n.children.length; i++) {
          const count = leafCount(n.children[i].node);
          if (
            highlightIndex >= leafStart &&
            highlightIndex < leafStart + count
          ) {
            activeChild = i;
            break;
          }
          leafStart += count;
        }
      }

      for (let i = 0; i < n.children.length; i++) {
        rects.push({
          x: x + i * (tabW + gap),
          y,
          w: tabW,
          h: tabH,
          leafIndex: i === activeChild ? (highlightIndex ?? -1) : -1,
        });
      }

      // Skip leaf counters for inactive children, layout only the active one.
      for (let i = 0; i < n.children.length; i++) {
        if (i === activeChild) {
          layout(n.children[i].node, x, y + tabH + gap, w, h - tabH - gap);
        } else {
          leafCounter += leafCount(n.children[i].node);
        }
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
      const actualSize =
        i === n.children.length - 1
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
      {bg && <rect width={width} height={height} fill={bg} rx={1} />}
      {rects.map((r, i) => {
        const highlighted = r.leafIndex >= 0 && r.leafIndex === highlightIndex;
        return (
          <rect
            key={i}
            x={r.x + 0.5}
            y={r.y + 0.5}
            width={Math.max(0, r.w - 1)}
            height={Math.max(0, r.h - 1)}
            fill={highlighted ? color : (bg ?? "none")}
            stroke={color}
            rx={1}
          />
        );
      })}
    </svg>
  );
}
