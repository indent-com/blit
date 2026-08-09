/**
 * URL-hash wire form for side-panel chrome state: which side panels are open
 * and which left-dock sections are expanded. Two keys in the app's
 * `&`-joined hash (alongside l/p/a/s/t/tile):
 *
 *   d=l,r        open panels: l = left dock, r = right preview panel;
 *                "d=" means both closed
 *   x=explorer,log   expanded left-dock sections; "x=" means all collapsed
 *
 * Both keys are always written, so a present key is authoritative; an absent
 * key falls back to localStorage/defaults (older links keep working).
 */

import { LEFT_PANELS, type LeftPanel } from "./dockSections";

export interface PanelsState {
  left: boolean;
  preview: boolean;
}

/** Format the `d=` value: comma list of open panels. */
export function formatPanelsHash(
  leftOpen: boolean,
  previewOpen: boolean,
): string {
  const open: string[] = [];
  if (leftOpen) open.push("l");
  if (previewOpen) open.push("r");
  return open.join(",");
}

/** Parse a `d=` value, or null when the key is absent from the hash. A
 *  present key is authoritative: panels not listed are closed. */
export function parsePanelsHash(value: string | null): PanelsState | null {
  if (value === null) return null;
  const tokens = new Set(value.split(",").filter(Boolean));
  return { left: tokens.has("l"), preview: tokens.has("r") };
}

/** Format the `x=` value: comma list of expanded (non-collapsed) sections. */
export function formatExpandedHash(collapsed: ReadonlySet<LeftPanel>): string {
  return LEFT_PANELS.filter((panel) => !collapsed.has(panel)).join(",");
}

/** Parse an `x=` value into the collapsed set, or null when the key is
 *  absent from the hash. Unknown section ids are dropped. */
export function parseExpandedHash(value: string | null): Set<LeftPanel> | null {
  if (value === null) return null;
  const expanded = new Set(
    value
      .split(",")
      .filter((token): token is LeftPanel =>
        (LEFT_PANELS as string[]).includes(token),
      ),
  );
  return new Set(LEFT_PANELS.filter((panel) => !expanded.has(panel)));
}
