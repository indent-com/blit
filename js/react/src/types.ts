import type React from "react";
import type { SessionId, TerminalPalette } from "@blit-sh/core";

/** Options for the BlitTerminal component. */
export interface BlitTerminalProps {
  /** Session ID to display. If null, the component renders an empty surface. */
  sessionId: SessionId | null;
  /** CSS font family for the terminal. */
  fontFamily?: string;
  /** Font size in CSS pixels used for cell measurement. */
  fontSize?: number;
  /** Additional CSS class name for the container. */
  className?: string;
  /** Additional inline styles for the container. */
  style?: React.CSSProperties;
  /** Color palette applied to the terminal. */
  palette?: TerminalPalette;
  /** When true, the terminal renders but never sends resize, input, or scroll commands. */
  readOnly?: boolean;
  /** When false, the cursor is hidden. Default: true. */
  showCursor?: boolean;
  /** Called after each render frame. Receives the render duration in ms. */
  onRender?: (renderMs: number) => void;
  /** Scrollbar indicator color (CSS color string). Default: "rgba(255,255,255,0.3)" */
  scrollbarColor?: string;
  /** Scrollbar indicator width in pixels. Default: 4 */
  scrollbarWidth?: number;
  /** Font advance-width / units-per-em ratio from font tables for native-accurate cell width. */
  advanceRatio?: number;
}
