import type { JSX } from "solid-js";
import { createSignal, onCleanup, onMount, Show } from "solid-js";
import { Portal } from "solid-js/web";
import type { TerminalPalette } from "@blit-sh/core";
import {
  layout,
  overlayChromeStyles,
  scrollbarStyle,
  themeFor,
  uiScale,
} from "./theme";
import { t } from "./i18n";

/**
 * The visible band of the page. `position: fixed` and vh units follow the
 * layout viewport, which on iOS does NOT shrink when the software keyboard
 * opens — the visual viewport shrinks and pans (offsetTop) instead. An
 * overlay anchored to inset:0 would sit half under the keyboard and half
 * above the screen edge, so the backdrop tracks the visual viewport.
 */
function useViewportBand() {
  const [band, setBand] = createSignal({
    top: 0,
    left: 0,
    width: window.innerWidth,
    height: window.innerHeight,
  });
  onMount(() => {
    const vv = window.visualViewport;
    const update = () =>
      setBand(
        vv
          ? {
              top: vv.offsetTop,
              left: vv.offsetLeft,
              width: vv.width,
              height: vv.height,
            }
          : {
              top: 0,
              left: 0,
              width: window.innerWidth,
              height: window.innerHeight,
            },
      );
    update();
    // scroll: the keyboard pans the page (offsetTop) without a resize.
    vv?.addEventListener("resize", update);
    vv?.addEventListener("scroll", update);
    window.addEventListener("resize", update);
    onCleanup(() => {
      vv?.removeEventListener("resize", update);
      vv?.removeEventListener("scroll", update);
      window.removeEventListener("resize", update);
    });
  });
  return band;
}

export function OverlayBackdrop(props: {
  palette: TerminalPalette;
  label: string;
  onClose?: () => void;
  dismissOnBackdrop?: boolean;
  children: JSX.Element;
  style?: JSX.CSSProperties;
}) {
  const dark = () => props.palette.dark;
  const styles = () => overlayChromeStyles(themeFor(props.palette), dark());
  const band = useViewportBand();
  // The band is smaller than the layout viewport ⇒ software keyboard is up
  // (or the window is being covered). Space is scarce: fill the band
  // instead of floating at 80% with wide margins.
  const cramped = () => band().height < window.innerHeight - 1;

  return (
    // Portal to <body>: while the software keyboard is up, Workspace pins
    // <main> with a translateY transform, and a transformed ancestor becomes
    // the containing block for position:fixed — a backdrop left inside it
    // would add its band offset on top of <main>'s and land off-screen.
    <Portal mount={document.body}>
      <div
        role="dialog"
        aria-modal="true"
        aria-label={props.label}
        style={{
          ...layout.overlay,
          top: `${band().top}px`,
          left: `${band().left}px`,
          width: `${band().width}px`,
          height: `${band().height}px`,
          "max-width": "none",
          "max-height": "none",
          ...styles().overlay,
          // Consumed by layout.panel's max-height; inherits to every overlay.
          "--overlay-panel-cap": cramped() ? "100%" : "80%",
          ...(cramped() ? { padding: "8px" } : {}),
          ...props.style,
        }}
        onClick={props.dismissOnBackdrop !== false ? props.onClose : undefined}
      >
        {props.children}
      </div>
    </Portal>
  );
}

export function OverlayPanel(props: {
  ref?: HTMLDivElement | ((el: HTMLDivElement) => void);
  palette: TerminalPalette;
  fontSize?: number;
  style?: JSX.CSSProperties;
  onClick?: (e: MouseEvent) => void;
  children?: JSX.Element;
}) {
  const dark = () => props.palette.dark;
  const scale = () => uiScale(props.fontSize ?? 13);
  const styles = () =>
    overlayChromeStyles(themeFor(props.palette), dark(), scale());

  return (
    <div
      ref={props.ref}
      style={{
        ...layout.panel,
        ...styles().panel,
        ...scrollbarStyle(themeFor(props.palette)),
        "font-size": `${scale().md}px`,
        ...props.style,
      }}
      onClick={(e) => {
        e.stopPropagation();
        props.onClick?.(e);
      }}
    >
      {props.children}
    </div>
  );
}

export function OverlayHeader(props: {
  palette: TerminalPalette;
  title: JSX.Element;
  subtitle?: JSX.Element;
  actions?: JSX.Element;
  onClose?: () => void;
  closeLabel?: string;
  fontSize?: number;
}) {
  const dark = () => props.palette.dark;
  const scale = () => uiScale(props.fontSize ?? 13);
  const styles = () =>
    overlayChromeStyles(themeFor(props.palette), dark(), scale());

  return (
    <header style={styles().header}>
      <div style={styles().headerCopy}>
        <h2 style={styles().title}>{props.title}</h2>
        <Show when={props.subtitle}>
          {(sub) => <p style={styles().subtitle}>{sub()}</p>}
        </Show>
      </div>
      <Show when={props.actions || props.onClose}>
        <div style={styles().headerActions}>
          {props.actions}
          <Show when={props.onClose}>
            {(close) => (
              <button
                type="button"
                style={styles().closeButton}
                onClick={close()}
              >
                {props.closeLabel ?? t("overlay.close")}
              </button>
            )}
          </Show>
        </div>
      </Show>
    </header>
  );
}
