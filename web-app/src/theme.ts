import type React from "react";
import type { TerminalPalette } from "blit-react";

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

/** Centralized z-index scale (increments of 10 for easy insertion). */
export const z = {
  exitedBanner: 10,
  overlay: 20,
  disconnected: 30,
  debugPanel: 40,
} as const;

// Layout styles that don't depend on the theme.
export const layout: Record<string, React.CSSProperties> = {
  overlay: {
    position: "fixed",
    inset: 0,
    display: "flex",
    alignItems: "center",
    justifyContent: "center",
    backgroundColor: "rgba(0,0,0,0.5)",
    backdropFilter: "blur(4px)",
    WebkitBackdropFilter: "blur(4px)",
    zIndex: z.overlay,
    width: "100%",
    height: "100%",
    maxWidth: "100%",
    maxHeight: "100%",
    padding: 0,
    margin: 0,
  },
  workspace: {
    display: "flex",
    flexDirection: "column",
    height: "100%",
    width: "100%",
  },
  statusBar: {
    display: "flex",
    alignItems: "center",
    height: 28,
    padding: "0 8px",
    fontSize: 12,
    gap: 8,
    borderTop: "1px solid",
    flexShrink: 0,
    userSelect: "none",
  },
  termContainer: {
    flex: 1,
    overflow: "hidden",
    position: "relative",
  },
  panel: {
    padding: 16,
    maxHeight: "80vh",
    overflow: "auto",
  },
};

// Reusable component styles.
export const ui: Record<string, React.CSSProperties> = {
  btn: {
    background: "none",
    border: "none",
    color: "inherit",
    cursor: "pointer",
    fontSize: 12,
    fontFamily: "inherit",
    opacity: 0.7,
    padding: "2px 6px",
  },
  input: {
    flex: 1,
    padding: "6px 10px",
    fontSize: 14,
    border: "1px solid rgba(128,128,128,0.3)",
    outline: "none",
    fontFamily: "inherit",
  },
  badge: {
    fontSize: 10,
    padding: "1px 6px",
    backgroundColor: "rgba(88,136,255,0.25)",
    color: "inherit",
    flexShrink: 0,
    lineHeight: 1.5,
  } as React.CSSProperties,
  swatch: {
    display: "inline-block",
    width: 14,
    height: 14,
  },
  kbd: {
    display: "inline-block",
    padding: "2px 6px",
    fontSize: 12,
    border: "1px solid rgba(128,128,128,0.4)",
    whiteSpace: "nowrap",
  },
};

export interface OverlayChromeStyles {
  overlay: React.CSSProperties;
  panel: React.CSSProperties;
  header: React.CSSProperties;
  headerCopy: React.CSSProperties;
  title: React.CSSProperties;
  subtitle: React.CSSProperties;
  headerActions: React.CSSProperties;
  closeButton: React.CSSProperties;
  footer: React.CSSProperties;
  actionButton: React.CSSProperties;
}

export function overlayChromeStyles(
  theme: Theme,
  dark: boolean,
): OverlayChromeStyles {
  return {
    overlay: {
      padding: 16,
    },
    panel: {
      backgroundColor: theme.solidPanelBg,
      color: theme.fg,
      border: `1px solid ${theme.border}`,
      boxShadow: dark
        ? "0 18px 60px rgba(0,0,0,0.45)"
        : "0 18px 60px rgba(0,0,0,0.12)",
      outline: "none",
    },
    header: {
      display: "flex",
      justifyContent: "space-between",
      alignItems: "flex-start",
      gap: 12,
      flexWrap: "wrap",
      marginBottom: 12,
    },
    headerCopy: {
      display: "grid",
      gap: 4,
      minWidth: 0,
    },
    title: {
      margin: 0,
      fontSize: 16,
      lineHeight: 1.2,
      fontWeight: 600,
    },
    subtitle: {
      margin: 0,
      fontSize: 12,
      lineHeight: 1.4,
      color: theme.dimFg,
    },
    headerActions: {
      display: "flex",
      alignItems: "center",
      gap: 8,
      marginLeft: "auto",
    },
    closeButton: {
      ...ui.btn,
      opacity: 0.6,
      padding: "4px 8px",
      border: `1px solid ${theme.subtleBorder}`,
      backgroundColor: theme.inputBg,
      whiteSpace: "nowrap",
    },
    footer: {
      display: "flex",
      justifyContent: "space-between",
      alignItems: "center",
      gap: 14,
      flexWrap: "wrap",
    },
    actionButton: {
      appearance: "none",
      border: `1px solid ${theme.subtleBorder}`,
      backgroundColor: theme.inputBg,
      color: theme.fg,
      padding: "10px 14px",
      fontSize: 12,
      fontFamily: "inherit",
      cursor: "pointer",
    },
  };
}

export interface DisconnectedStyles extends OverlayChromeStyles {
  card: React.CSSProperties;
  content: React.CSSProperties;
  title: React.CSSProperties;
  reloadButton: React.CSSProperties;
}

export function disconnectedStyles(
  theme: Theme,
  dark: boolean,
): DisconnectedStyles {
  const chrome = overlayChromeStyles(theme, dark);

  return {
    ...chrome,
    card: {
      ...chrome.panel,
      width: "min(24em, calc(100vw - 2em))",
      maxWidth: "100%",
      background: dark ? theme.solidPanelBg : theme.panelBg,
      padding: 0,
    },
    content: {
      display: "grid",
      gap: "0.75em",
      justifyItems: "center",
      padding: "1.2em 1.4em 1em",
    },
    title: {
      margin: 0,
      fontSize: "1.2em",
      lineHeight: 1.2,
      fontWeight: 600,
    },
    reloadButton: {
      ...chrome.actionButton,
      padding: "0.5em 0.75em",
    },
  };
}
