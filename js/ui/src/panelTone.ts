import type { Theme } from "./theme";

/** Semantic status, mapped to a colour by {@link pillColor}. */
export type PanelTone = "ok" | "warn" | "bad" | "idle";

export function pillColor(theme: Theme, tone: PanelTone): string {
  switch (tone) {
    case "ok":
      return theme.accent;
    case "warn":
      return theme.warning;
    case "bad":
      return theme.error;
    case "idle":
      return theme.dimFg;
  }
}
