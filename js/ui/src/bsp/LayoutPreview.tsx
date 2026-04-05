import { For } from "solid-js";
import { leafCount, type BSPNode } from "@blit-sh/core/bsp";

/**
 * Tiny visual preview of a BSP layout as nested rectangles.
 * Renders at the given width/height with 1px gaps between panes.
 */
export function LayoutPreview(props: {
  node: BSPNode;
  width?: number;
  height?: number;
  color?: string;
  /** Background color for gaps between panes. */
  bg?: string;
  /** Leaf index to highlight (filled) in the preview. */
  highlightIndex?: number;
}) {
  const w = () => props.width ?? 48;
  const h = () => props.height ?? 32;
  const color = () => props.color ?? "currentColor";

  const rects = () => {
    const result: Array<{
      x: number;
      y: number;
      w: number;
      h: number;
      leafIndex: number;
    }> = [];
    let leafCounter = 0;
    const gap = 1;

    function doLayout(
      n: BSPNode,
      x: number,
      y: number,
      rw: number,
      rh: number,
    ) {
      if (n.type === "leaf") {
        result.push({ x, y, w: rw, h: rh, leafIndex: leafCounter++ });
        return;
      }

      if (n.direction === "tabs") {
        const tabH = Math.max(2, Math.round(rh * 0.1));
        const tabW = Math.max(
          2,
          Math.round((rw - gap * (n.children.length - 1)) / n.children.length),
        );

        // Figure out which tab child contains the highlighted leaf.
        let activeChild = 0;
        if (props.highlightIndex != null) {
          let leafStart = leafCounter;
          for (let i = 0; i < n.children.length; i++) {
            const count = leafCount(n.children[i].node);
            if (
              props.highlightIndex >= leafStart &&
              props.highlightIndex < leafStart + count
            ) {
              activeChild = i;
              break;
            }
            leafStart += count;
          }
        }

        for (let i = 0; i < n.children.length; i++) {
          result.push({
            x: x + i * (tabW + gap),
            y,
            w: tabW,
            h: tabH,
            leafIndex: i === activeChild ? (props.highlightIndex ?? -1) : -1,
          });
        }

        // Skip leaf counters for inactive children, layout only the active one.
        for (let i = 0; i < n.children.length; i++) {
          if (i === activeChild) {
            doLayout(
              n.children[i].node,
              x,
              y + tabH + gap,
              rw,
              rh - tabH - gap,
            );
          } else {
            leafCounter += leafCount(n.children[i].node);
          }
        }
        return;
      }

      const totalWeight = n.children.reduce((sum, c) => sum + c.weight, 0);
      const isHoriz = n.direction === "horizontal";
      const totalGap = gap * (n.children.length - 1);
      const available = (isHoriz ? rw : rh) - totalGap;
      let offset = 0;

      for (let i = 0; i < n.children.length; i++) {
        const child = n.children[i];
        const size = Math.round((child.weight / totalWeight) * available);
        const actualSize =
          i === n.children.length - 1 ? available - offset : size;

        if (isHoriz) {
          doLayout(child.node, x + offset, y, actualSize, rh);
        } else {
          doLayout(child.node, x, y + offset, rw, actualSize);
        }
        offset += actualSize + gap;
      }
    }

    doLayout(props.node, 0, 0, w(), h());
    return result;
  };

  return (
    <svg width={w()} height={h()} style={{ "flex-shrink": 0 }}>
      {props.bg && <rect width={w()} height={h()} fill={props.bg} rx={1} />}
      <For each={rects()}>
        {(r) => {
          const highlighted = () =>
            r.leafIndex >= 0 && r.leafIndex === props.highlightIndex;
          return (
            <rect
              x={r.x + 0.5}
              y={r.y + 0.5}
              width={Math.max(0, r.w - 1)}
              height={Math.max(0, r.h - 1)}
              fill={highlighted() ? color() : (props.bg ?? "none")}
              stroke={color()}
              rx={1}
            />
          );
        }}
      </For>
    </svg>
  );
}
