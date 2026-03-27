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
    padding: "1px 5px",
    backgroundColor: "rgba(88,136,255,0.3)",
    color: "inherit",
    flexShrink: 0,
  },
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
  disconnected: {
    position: "absolute",
    inset: 0,
    display: "flex",
    alignItems: "center",
    justifyContent: "center",
    fontSize: 32,
    fontWeight: 700,
    color: "#e33",
    pointerEvents: "none",
  },
};
