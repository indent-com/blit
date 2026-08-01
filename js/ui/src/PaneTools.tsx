/**
 * PaneTools — the multitool in a pane's corner: a grip that drags the pane's
 * content out (drop on another pane to move it there, or on the dock to park
 * it), clicks to send the toolbar itself to another corner, and the ✕ that
 * closes the content.
 *
 * One component for both render paths (BSP leaf panes in BSPContainer and the
 * non-BSP focused view in Workspace) so the two can't drift, the same reason
 * BlitTile is shared.
 *
 * It is the one piece of chrome floating above every pane kind, which is what
 * makes it the drag handle: a surface's canvas swallows the pointer, so
 * without the grip a surface pane has nothing to grab.
 *
 * Visibility is pointer-dependent. With a pointer it appears on hover, keeping
 * the corner of the grid clear the rest of the time. On a touch device there is
 * no hover to reveal it and no way to type the Ctrl+Alt+Shift+Q chord that
 * closes a pane — MobileToolbar offers Ctrl and Alt but no Shift — so there it
 * is always shown. Without that, a terminal opened on Android cannot be closed
 * from the pane at all.
 */

import { createSignal, Show, type JSX } from "solid-js";
import type { Theme, UIScale } from "./theme";
import { t } from "./i18n";
import { startPaneTileDrag } from "./ide/tileDrag";

/** Toolbar corners, in click-to-cycle order from the default. */
const CORNERS = ["top-right", "bottom-right", "bottom-left", "top-left"] as const;

export function PaneTools(props: {
  theme: Theme;
  scale: UIScale;
  /** No hover to reveal it (touch): keep it on screen. */
  alwaysVisible: boolean;
  /** The pointer is over the pane. */
  hovered: boolean;
  /** When set, the grip is shown, dragging this assignment out of this pane.
   *  Absent in the non-BSP single view, where there is nowhere to drop. */
  drag?: { assignment: string; paneId: string };
  onClose: () => void;
}) {
  // Which corner the toolbar sits in. It floats over the pane's content, and
  // a surface is a real app that may have its own controls exactly under the
  // default top-right — clicking the grip cycles the toolbar to the next
  // corner, out of the way of whatever it is covering. Per pane, not
  // persisted: outliving the hover is what matters, surviving a reload isn't.
  const [corner, setCorner] = createSignal(0);
  const cornerStyle = (): JSX.CSSProperties => {
    const gap = `${props.scale.tightGap}px`;
    switch (CORNERS[corner() % CORNERS.length]) {
      case "top-right":
        return { top: gap, right: gap };
      case "bottom-right":
        return { bottom: gap, right: gap };
      case "bottom-left":
        return { bottom: gap, left: gap };
      case "top-left":
        return { top: gap, left: gap };
    }
  };
  const segment = (): JSX.CSSProperties => ({
    display: "flex",
    "align-items": "center",
    "justify-content": "center",
    "min-width": `${props.scale.md * 2}px`,
    height: `${props.scale.md * 2}px`,
    padding: 0,
    "background-color": props.theme.solidPanelBg,
    border: `1px solid ${props.theme.subtleBorder}`,
    "border-radius": "0",
    color: props.theme.fg,
    "font-family": "inherit",
    "font-size": `${props.scale.sm}px`,
    "line-height": 1,
    opacity: 0.75,
    "touch-action": "manipulation",
  });
  return (
    <Show when={props.alwaysVisible || props.hovered}>
      <div
        style={{
          position: "absolute",
          ...cornerStyle(),
          // Above the pane's content and above the tile-drag highlight, which
          // sits at 5 and is pointer-events:none.
          "z-index": 6,
          display: "flex",
        }}
      >
        <Show when={props.drag}>
          {(drag) => (
            <button
              type="button"
              title={t("bsp.move")}
              aria-label={t("bsp.move")}
              draggable={true}
              onDragStart={(e) =>
                startPaneTileDrag(e, drag().assignment, drag().paneId)
              }
              // A click (no drag happened — the browser suppresses click
              // after a drag) relocates the toolbar itself. Stopped like the
              // ✕'s: the content underneath must not also see it as input.
              onClick={(e) => {
                e.stopPropagation();
                setCorner((c) => (c + 1) % CORNERS.length);
              }}
              style={{
                ...segment(),
                cursor: "grab",
                // The ✕ brings the shared edge; doubling it reads as a gap.
                "border-right": "none",
              }}
            >
              {"⠿"}
            </button>
          )}
        </Show>
        <button
          type="button"
          title={t("bsp.close")}
          aria-label={t("bsp.close")}
          // Let pointerdown reach the pane — focusing what you are about to
          // close is harmless — but keep the click: a terminal or surface
          // underneath must not also see it as input.
          onClick={(e) => {
            e.stopPropagation();
            props.onClose();
          }}
          style={{ ...segment(), cursor: "pointer" }}
        >
          {"✕"}
        </button>
      </div>
    </Show>
  );
}
