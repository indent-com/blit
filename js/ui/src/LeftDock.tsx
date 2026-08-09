/**
 * LeftDock — a single left dock, opened/closed only from the status bar,
 * that stacks the IDE sections as an accordion. Every section is always
 * present; its header (chevron + title + optional controls) toggles collapse.
 * Expanded sections share the vertical space by weight, with a drag handle
 * between adjacent expanded sections; a handle on the right edge sizes the
 * dock. An optional header slot (the root picker) sits above the sections.
 * Palette-themed like the right-side PreviewPanel.
 */

import { createSignal, For, Show, type JSX } from "solid-js";
import type { Theme, UIScale } from "./theme";
import { t } from "./i18n";
import { LEFT_PANELS, type LeftPanel } from "./dockSections";

export type { LeftPanel };
export { LEFT_PANELS };

/** Section titles. A function, not a constant map, so the strings are
 *  read through `t()` at render time rather than frozen at module load. */
export const leftPanelTitle = (panel: LeftPanel): string => t(`dock.${panel}`);

const MIN_DOCK_WIDTH = 160;

export function LeftDock(props: {
  collapsed: ReadonlySet<LeftPanel>;
  weights: Record<LeftPanel, number>;
  renderExtra?: (panel: LeftPanel) => JSX.Element;
  renderBody: (panel: LeftPanel) => JSX.Element;
  header?: JSX.Element;
  onToggleCollapse: (panel: LeftPanel) => void;
  onResizeWeight: (a: LeftPanel, b: LeftPanel, deltaWeight: number) => void;
  theme: Theme;
  scale: UIScale;
  isMobileTouch: boolean;
  width: number;
  onResizeWidth: (width: number) => void;
}) {
  const [resizeHover, setResizeHover] = createSignal(false);
  const [resizeActive, setResizeActive] = createSignal(false);
  let column!: HTMLDivElement;

  const expanded = (): LeftPanel[] =>
    LEFT_PANELS.filter((p) => !props.collapsed.has(p));

  function widthPointerDown(e: PointerEvent) {
    e.preventDefault();
    (e.target as HTMLElement).setPointerCapture(e.pointerId);
    setResizeActive(true);
    const startX = e.clientX;
    const startWidth = props.width;
    const maxWidth = Math.max(
      MIN_DOCK_WIDTH,
      Math.floor(window.innerWidth * 0.85),
    );
    const onMove = (me: PointerEvent) => {
      const delta = me.clientX - startX;
      props.onResizeWidth(
        Math.min(maxWidth, Math.max(MIN_DOCK_WIDTH, startWidth + delta)),
      );
    };
    const onUp = () => {
      setResizeActive(false);
      document.removeEventListener("pointermove", onMove);
      document.removeEventListener("pointerup", onUp);
    };
    document.addEventListener("pointermove", onMove);
    document.addEventListener("pointerup", onUp);
  }

  function sectionDividerDown(a: LeftPanel, b: LeftPanel, e: PointerEvent) {
    e.preventDefault();
    (e.target as HTMLElement).setPointerCapture(e.pointerId);
    const exp = expanded();
    const total = exp.reduce((sum, p) => sum + (props.weights[p] ?? 1), 0);
    const height = column?.clientHeight || 1;
    // Track the *incremental* movement since the last event: onResizeWeight
    // adds the delta, so passing the from-start delta each move would
    // accumulate (position turns into speed).
    let lastY = e.clientY;
    const onMove = (me: PointerEvent) => {
      props.onResizeWeight(a, b, ((me.clientY - lastY) / height) * total);
      lastY = me.clientY;
    };
    const onUp = () => {
      document.removeEventListener("pointermove", onMove);
      document.removeEventListener("pointerup", onUp);
    };
    document.addEventListener("pointermove", onMove);
    document.addEventListener("pointerup", onUp);
  }

  const handleWidth = () => (props.isMobileTouch ? 14 : 3);

  const sectionHeader = (panel: LeftPanel) => {
    const isCollapsed = () => props.collapsed.has(panel);
    return (
      <div
        onClick={() => props.onToggleCollapse(panel)}
        style={{
          display: "flex",
          "align-items": "center",
          gap: `${props.scale.tightGap}px`,
          padding: `${props.scale.controlY}px ${props.scale.panelPadding}px`,
          "border-bottom": `1px solid ${props.theme.subtleBorder}`,
          "flex-shrink": 0,
          cursor: "pointer",
          "user-select": "none",
        }}
      >
        <span
          style={{
            width: "10px",
            "flex-shrink": 0,
            color: props.theme.dimFg,
            "text-align": "center",
          }}
        >
          {isCollapsed() ? "▸" : "▾"}
        </span>
        <span
          style={{
            "font-size": `${props.scale.xs}px`,
            "letter-spacing": "0.8px",
            "text-transform": "uppercase",
            color: props.theme.dimFg,
            "font-weight": 600,
          }}
        >
          {leftPanelTitle(panel)}
        </span>
        <Show when={!isCollapsed()}>
          <span
            onClick={(e) => e.stopPropagation()}
            style={{ display: "flex", "margin-left": "auto" }}
          >
            {props.renderExtra?.(panel)}
          </span>
        </Show>
      </div>
    );
  };

  return (
    <div
      style={{
        width: `${props.width}px`,
        "flex-shrink": 0,
        display: "flex",
        "flex-direction": "row",
        overflow: "hidden",
      }}
    >
      <div
        ref={column}
        style={{
          flex: 1,
          "min-width": 0,
          "background-color": props.theme.bg,
          display: "flex",
          "flex-direction": "column",
          overflow: "hidden",
        }}
      >
        <Show when={props.header}>
          <div style={{ "flex-shrink": 0 }}>{props.header}</div>
        </Show>
        <For each={LEFT_PANELS}>
          {(panel) => {
            const isCollapsed = () => props.collapsed.has(panel);
            // A divider sits above this section only when it and the previous
            // section are both expanded (so both can be resized).
            const prevExpanded = (): LeftPanel | null => {
              const exp = expanded();
              const idx = exp.indexOf(panel);
              return idx > 0 ? exp[idx - 1] : null;
            };
            return (
              <>
                <Show when={!isCollapsed() && prevExpanded()}>
                  {(prev) => (
                    <div
                      onPointerDown={(e) =>
                        sectionDividerDown(prev(), panel, e)
                      }
                      role="separator"
                      aria-orientation="horizontal"
                      aria-label="Resize section"
                      style={{
                        height: `${props.isMobileTouch ? 10 : 4}px`,
                        "flex-shrink": 0,
                        cursor: "row-resize",
                        "background-color": props.theme.subtleBorder,
                        "touch-action": "none",
                      }}
                    />
                  )}
                </Show>
                <div
                  style={{
                    flex: isCollapsed()
                      ? "0 0 auto"
                      : `${props.weights[panel] ?? 1} 1 0`,
                    "min-height": isCollapsed() ? undefined : "48px",
                    display: "flex",
                    "flex-direction": "column",
                    overflow: "hidden",
                  }}
                >
                  {sectionHeader(panel)}
                  <Show when={!isCollapsed()}>{props.renderBody(panel)}</Show>
                </div>
              </>
            );
          }}
        </For>
      </div>
      <div
        onPointerDown={widthPointerDown}
        onPointerEnter={() => setResizeHover(true)}
        onPointerLeave={() => setResizeHover(false)}
        role="separator"
        aria-orientation="vertical"
        aria-label="Resize dock"
        style={{
          width: `${handleWidth()}px`,
          "flex-shrink": 0,
          cursor: "col-resize",
          background: resizeActive()
            ? "rgba(128,128,128,0.5)"
            : resizeHover()
              ? "rgba(128,128,128,0.3)"
              : "transparent",
          "border-right": `1px solid ${props.theme.subtleBorder}`,
          transition: "background 0.1s",
          "touch-action": "none",
        }}
      />
    </div>
  );
}
