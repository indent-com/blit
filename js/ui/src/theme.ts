import type { JSX } from "solid-js";
import type { BlitSession, TerminalPalette } from "@blit-sh/core";

export function sessionName(s: BlitSession): string {
  const label = s.title ?? s.command;
  if (label && s.tag && label !== s.tag) return `${s.tag}: ${label}`;
  return label ?? s.tag ?? "Terminal";
}

export interface Theme {
  bg: string;
  fg: string;
  dimFg: string;
  panelBg: string;
  solidPanelBg: string;
  inputBg: string;
  solidInputBg: string;
  border: string;
  subtleBorder: string;
  hoverBg: string;
  selectedBg: string;
  accent: string;
  error: string;
  errorText: string;
  success: string;
  warning: string;
}

export interface UIScale {
  xs: number;
  sm: number;
  md: number;
  lg: number;
  xl: number;
  tightGap: number;
  gap: number;
  panelPadding: number;
  controlY: number;
  controlX: number;
  icon: number;
}

export function uiScale(baseFontSize: number): UIScale {
  const base = Math.max(10, Math.round(baseFontSize || 13));
  const max = Math.round(base * 1.25);
  const scaled = (multiplier: number, floor: number) =>
    Math.max(floor, Math.min(max, Math.round(base * multiplier)));

  return {
    xs: scaled(0.78, 9),
    sm: scaled(0.88, 10),
    md: scaled(1, base),
    lg: scaled(1.08, base),
    xl: scaled(1.18, base),
    tightGap: Math.max(4, Math.round(base * 0.3)),
    gap: Math.max(6, Math.round(base * 0.45)),
    panelPadding: Math.max(8, Math.round(base * 0.6)),
    controlY: Math.max(3, Math.round(base * 0.32)),
    controlX: Math.max(6, Math.round(base * 0.55)),
    icon: Math.max(44, Math.round(base * 3.7)),
  };
}

export const darkTheme: Theme = {
  bg: "#1a1a1a",
  fg: "#e0e0e0",
  dimFg: "rgba(255,255,255,0.5)",
  panelBg: "rgba(0,0,0,0.85)",
  solidPanelBg: "#1e1e1e",
  inputBg: "rgba(255,255,255,0.08)",
  solidInputBg: "#2a2a2a",
  border: "rgba(255,255,255,0.15)",
  subtleBorder: "rgba(255,255,255,0.1)",
  hoverBg: "rgba(255,255,255,0.06)",
  selectedBg: "rgba(255,255,255,0.1)",
  accent: "#58f",
  error: "#a44",
  errorText: "#f55",
  success: "#4a4",
  warning: "#da3",
};

export const lightTheme: Theme = {
  bg: "#f5f5f5",
  fg: "#333",
  dimFg: "rgba(0,0,0,0.5)",
  panelBg: "rgba(255,255,255,0.9)",
  solidPanelBg: "#f5f5f5",
  inputBg: "rgba(0,0,0,0.05)",
  solidInputBg: "#fff",
  border: "rgba(0,0,0,0.15)",
  subtleBorder: "rgba(0,0,0,0.1)",
  hoverBg: "rgba(0,0,0,0.04)",
  selectedBg: "rgba(0,0,0,0.08)",
  accent: "#58f",
  error: "#a44",
  errorText: "#f55",
  success: "#4a4",
  warning: "#da3",
};

function rgb([r, g, b]: [number, number, number]): string {
  return `rgb(${r}, ${g}, ${b})`;
}

function rgba([r, g, b]: [number, number, number], alpha: number): string {
  return `rgba(${r}, ${g}, ${b}, ${alpha})`;
}

function mix(
  from: [number, number, number],
  to: [number, number, number],
  amount: number,
): [number, number, number] {
  return [
    Math.round(from[0] + (to[0] - from[0]) * amount),
    Math.round(from[1] + (to[1] - from[1]) * amount),
    Math.round(from[2] + (to[2] - from[2]) * amount),
  ];
}

function themeFromPalette(palette: TerminalPalette): Theme {
  const panelSurface = mix(palette.bg, palette.fg, palette.dark ? 0.08 : 0.04);
  const inputSurface = mix(palette.bg, palette.fg, palette.dark ? 0.14 : 0.08);
  const accent = palette.ansi[12] ?? palette.ansi[4] ?? palette.fg;
  const error = palette.ansi[1] ?? palette.fg;
  const errorText = palette.ansi[9] ?? error;
  const success = palette.ansi[2] ?? palette.fg;
  const warning = palette.ansi[3] ?? palette.fg;

  return {
    bg: rgb(palette.bg),
    fg: rgb(palette.fg),
    dimFg: rgba(palette.fg, palette.dark ? 0.62 : 0.68),
    panelBg: rgba(palette.bg, palette.dark ? 0.88 : 0.94),
    solidPanelBg: rgb(panelSurface),
    inputBg: rgba(palette.fg, palette.dark ? 0.1 : 0.06),
    solidInputBg: rgb(inputSurface),
    border: rgba(palette.fg, 0.18),
    subtleBorder: rgba(palette.fg, palette.dark ? 0.12 : 0.1),
    hoverBg: rgba(palette.fg, palette.dark ? 0.06 : 0.05),
    selectedBg: rgba(palette.fg, palette.dark ? 0.11 : 0.09),
    accent: rgb(accent),
    error: rgb(error),
    errorText: rgb(errorText),
    success: rgb(success),
    warning: rgb(warning),
  };
}

export function themeFor(source: boolean | TerminalPalette): Theme {
  if (typeof source === "boolean") {
    return source ? darkTheme : lightTheme;
  }
  return themeFromPalette(source);
}

export const sidebarWidth = "20em";

/** Centralized z-index scale (increments of 10 for easy insertion). */
export const z = {
  exitedBanner: 10,
  overlay: 20,
  disconnected: 30,
  debugPanel: 40,
} as const;

// Layout styles that don't depend on the theme.
export const layout: Record<string, JSX.CSSProperties> = {
  overlay: {
    position: "fixed",
    inset: 0,
    display: "flex",
    "align-items": "center",
    "justify-content": "center",
    "background-color": "rgba(0,0,0,0.5)",
    "backdrop-filter": "blur(1px)",
    "-webkit-backdrop-filter": "blur(1px)",
    "z-index": z.overlay,
    width: "100%",
    height: "100%",
    "max-width": "100%",
    "max-height": "100%",
    padding: 0,
    margin: 0,
  },
  workspace: {
    display: "flex",
    "flex-direction": "column",
    height: "100%",
    width: "100%",
  },
  statusBar: {
    display: "flex",
    "align-items": "center",
    "border-top": "1px solid",
    "flex-shrink": 0,
    "user-select": "none",
  },
  termContainer: {
    flex: 1,
    overflow: "hidden",
    position: "relative",
  },
  panel: {
    padding: "16px",
    "max-height": "80vh",
    overflow: "auto",
  },
};

// Reusable component styles.
export const ui: Record<string, JSX.CSSProperties> = {
  btn: {
    background: "none",
    border: "none",
    color: "inherit",
    cursor: "pointer",
    "font-size": "12px",
    "font-family": "inherit",
    opacity: 0.7,
    padding: "2px 6px",
  },
  input: {
    flex: 1,
    padding: "6px 10px",
    "font-size": "14px",
    border: "1px solid rgba(128,128,128,0.3)",
    outline: "none",
    "font-family": "inherit",
  },
  badge: {
    "font-size": "10px",
    padding: "1px 6px",
    "background-color": "rgba(88,136,255,0.25)",
    color: "inherit",
    "flex-shrink": 0,
    "line-height": 1.5,
  } as JSX.CSSProperties,
  swatch: {
    display: "inline-block",
    width: "14px",
    height: "14px",
  },
  kbd: {
    display: "inline-block",
    padding: "2px 6px",
    "font-size": "12px",
    "font-family": "inherit",
    border: "1px solid rgba(128,128,128,0.4)",
    "white-space": "nowrap",
  },
};

export interface OverlayChromeStyles {
  overlay: JSX.CSSProperties;
  panel: JSX.CSSProperties;
  header: JSX.CSSProperties;
  headerCopy: JSX.CSSProperties;
  title: JSX.CSSProperties;
  subtitle: JSX.CSSProperties;
  headerActions: JSX.CSSProperties;
  closeButton: JSX.CSSProperties;
  footer: JSX.CSSProperties;
  actionButton: JSX.CSSProperties;
}

export function overlayChromeStyles(
  theme: Theme,
  dark: boolean,
  scale: UIScale = uiScale(13),
): OverlayChromeStyles {
  return {
    overlay: {
      padding: `${Math.max(12, scale.panelPadding * 2)}px`,
    },
    panel: {
      "background-color": theme.solidPanelBg,
      color: theme.fg,
      border: `1px solid ${theme.border}`,
      "box-shadow": dark
        ? "0 18px 60px rgba(0,0,0,0.45)"
        : "0 18px 60px rgba(0,0,0,0.12)",
      outline: "none",
    },
    header: {
      display: "flex",
      "justify-content": "space-between",
      "align-items": "flex-start",
      gap: `${scale.gap}px`,
      "flex-wrap": "wrap",
      "margin-bottom": `${scale.gap * 2}px`,
    },
    headerCopy: {
      display: "grid",
      gap: `${scale.tightGap}px`,
      "min-width": 0,
    },
    title: {
      margin: 0,
      "font-size": `${scale.xl}px`,
      "line-height": 1.2,
      "font-weight": 600,
    },
    subtitle: {
      margin: 0,
      "font-size": `${scale.sm}px`,
      "line-height": 1.4,
      color: theme.dimFg,
    },
    headerActions: {
      display: "flex",
      "align-items": "center",
      gap: `${scale.tightGap + 2}px`,
      "margin-left": "auto",
    },
    closeButton: {
      ...ui.btn,
      opacity: 0.6,
      padding: `${scale.controlY}px ${scale.controlX}px`,
      border: `1px solid ${theme.subtleBorder}`,
      "background-color": theme.inputBg,
      "font-size": `${scale.sm}px`,
      "white-space": "nowrap",
    },
    footer: {
      display: "flex",
      "justify-content": "space-between",
      "align-items": "center",
      gap: `${scale.gap}px`,
      "flex-wrap": "wrap",
    },
    actionButton: {
      appearance: "none",
      border: `1px solid ${theme.subtleBorder}`,
      "background-color": theme.inputBg,
      color: theme.fg,
      padding: `${scale.controlY + 2}px ${scale.controlX + 2}px`,
      "font-size": `${scale.sm}px`,
      "font-family": "inherit",
      cursor: "pointer",
    },
  };
}

export interface DisconnectedStyles extends OverlayChromeStyles {
  card: JSX.CSSProperties;
  content: JSX.CSSProperties;
  title: JSX.CSSProperties;
  reloadButton: JSX.CSSProperties;
}

export function disconnectedStyles(
  theme: Theme,
  dark: boolean,
  scale: UIScale = uiScale(13),
): DisconnectedStyles {
  const chrome = overlayChromeStyles(theme, dark, scale);

  return {
    ...chrome,
    card: {
      ...chrome.panel,
      width: "min(24em, calc(100vw - 2em))",
      "max-width": "100%",
      background: dark ? theme.solidPanelBg : theme.panelBg,
      padding: 0,
    },
    content: {
      display: "grid",
      gap: "0.75em",
      "justify-items": "center",
      padding: "1.2em 1.4em 1em",
    },
    title: {
      margin: 0,
      "font-size": "1.2em",
      "line-height": 1.2,
      "font-weight": 600,
    },
    reloadButton: {
      ...chrome.actionButton,
      padding: "0.5em 0.75em",
    },
  };
}
