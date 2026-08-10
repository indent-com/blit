/**
 * Editor preferences shared by every open editor, so toggling one applies
 * everywhere instead of per tile — the same reasoning as the palette and
 * font size, which are workspace-wide rather than per-pane.
 *
 * Module-level signals: an editor reads them in an effect and reconfigures
 * its compartment, so a change reaches tiles that already exist.
 */
import { createSignal } from "solid-js";
import {
  EDITOR_WRAP_KEY,
  onConfigChange,
  readStorage,
  writeStorage,
} from "../storage";

const [lineWrap, setLineWrapSignal] = createSignal(
  // Default on: most of what gets opened here is prose-ish or long-lined
  // config, and a horizontal scrollbar in a narrow tiled pane is worse
  // than a wrapped line.
  readStorage(EDITOR_WRAP_KEY) !== "0",
);

export { lineWrap };

// Unlike component-owned preferences, editor wrap lives in a module signal.
// Keep it attached to the shared config stream so a toggle in any frontend
// reconfigures editors that are already open everywhere else.
onConfigChange((key, value) => {
  if (key === EDITOR_WRAP_KEY) setLineWrapSignal(value !== "0");
});

export function setLineWrap(on: boolean): void {
  setLineWrapSignal(on);
  writeStorage(EDITOR_WRAP_KEY, on ? "1" : "0");
}

export function toggleLineWrap(): void {
  setLineWrap(!lineWrap());
}
