import type React from "react";

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
};

export function themeFor(dark: boolean): Theme {
  return dark ? darkTheme : lightTheme;
}

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
    zIndex: 100,
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
    borderRadius: 9999,
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
      border: `1px solid ${dark ? "rgba(255,255,255,0.14)" : "rgba(48,22,14,0.16)"}`,
      backgroundColor: dark
        ? "rgba(255,255,255,0.05)"
        : "rgba(255,255,255,0.6)",
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
      width: "min(240px, calc(100vw - 32px))",
      maxWidth: "100%",
      background: dark ? theme.solidPanelBg : theme.panelBg,
      padding: 0,
    },
    content: {
      display: "grid",
      gap: 14,
      justifyItems: "center",
      padding: "18px 18px 16px",
    },
    title: {
      margin: 0,
      fontSize: 18,
      lineHeight: 1.2,
      fontWeight: 600,
    },
    reloadButton: {
      ...chrome.actionButton,
      padding: "8px 12px",
    },
  };
}
