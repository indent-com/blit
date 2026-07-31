/**
 * PaneClose — the ✕ that closes whatever a pane is showing.
 *
 * One component for both render paths (BSP leaf panes in BSPContainer and the
 * non-BSP focused view in Workspace) so the two can't drift, the same reason
 * BlitTile is shared.
 *
 * Visibility is pointer-dependent. With a pointer it appears on hover, keeping
 * the corner of the grid clear the rest of the time. On a touch device there is
 * no hover to reveal it and no way to type the Ctrl+Alt+Shift+Q chord that
 * closes a pane — MobileToolbar offers Ctrl and Alt but no Shift — so there it
 * is always shown. Without that, a terminal opened on Android cannot be closed
 * from the pane at all.
 */

import { Show } from "solid-js";
import type { Theme, UIScale } from "./theme";
import { t } from "./i18n";

export function PaneClose(props: {
  theme: Theme;
  scale: UIScale;
  /** No hover to reveal it (touch): keep it on screen. */
  alwaysVisible: boolean;
  /** The pointer is over the pane. */
  hovered: boolean;
  onClose: () => void;
}) {
  return (
    <Show when={props.alwaysVisible || props.hovered}>
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
        style={{
          position: "absolute",
          top: `${props.scale.tightGap}px`,
          right: `${props.scale.tightGap}px`,
          // Above the pane's content and above the tile-drag highlight, which
          // sits at 5 and is pointer-events:none.
          "z-index": 6,
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
          cursor: "pointer",
          "touch-action": "manipulation",
        }}
      >
        {"✕"}
      </button>
    </Show>
  );
}
