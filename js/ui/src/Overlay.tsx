import type { JSX } from "solid-js";
import { Show } from "solid-js";
import type { TerminalPalette } from "@blit-sh/core";
import { layout, overlayChromeStyles, themeFor, uiScale } from "./theme";
import { t } from "./i18n";

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

  return (
    <div
      role="dialog"
      aria-modal="true"
      aria-label={props.label}
      style={{
        ...layout.overlay,
        ...styles().overlay,
        ...props.style,
      }}
      onClick={props.dismissOnBackdrop !== false ? props.onClose : undefined}
    >
      {props.children}
    </div>
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
