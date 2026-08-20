/**
 * URL-hash wire form for side-panel chrome state: which side panels are open,
 * which left-dock sections are expanded, the Explorer's selected project,
 * and whether the right dock's Muster group is expanded. Four keys in the
 * app's `&`-joined hash (alongside l/p/a/s/t/tile):
 *
 *   d=l,r        open panels: l = left dock, r = right preview panel;
 *                "d=" means both closed
 *   x=explorer,log   expanded left-dock sections; "x=" means all collapsed
 *   r=f          selected Explorer project: focused pane, declared root, or
 *                a worktree (the latter two carry their identity as JSON)
 *   m=0          whether the right-dock Muster group is expanded (1) or
 *                collapsed (0)
 *
 * All four keys are always written, so a present key is authoritative; an
 * absent key falls back to localStorage/defaults (older links keep working).
 */

import { LEFT_PANELS, type LeftPanel } from "./dockSections";

export interface PanelsState {
  left: boolean;
  preview: boolean;
}

export type ProjectSelection =
  | { kind: "focused" }
  | { kind: "declared"; name: string }
  | {
      kind: "worktree";
      connectionId: string;
      path: string;
      label: string;
    };

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

/** Format the `r=` value. The caller URI-encodes this whole value before
 *  putting it in the hash; JSON keeps arbitrary root names and paths from
 *  colliding with the hash's separators. */
export function formatProjectHash(selection: ProjectSelection): string {
  if (selection.kind === "focused") return "f";
  if (selection.kind === "declared")
    return `d:${JSON.stringify(selection.name)}`;
  return `w:${JSON.stringify([
    selection.connectionId,
    selection.path,
    selection.label,
  ])}`;
}

/** Parse an `r=` value after URLSearchParams has URI-decoded it. Invalid or
 *  absent values fall back to the focused pane. */
export function parseProjectHash(
  value: string | null,
): ProjectSelection | null {
  if (value === null) return null;
  if (value === "f") return { kind: "focused" };
  try {
    if (value.startsWith("d:")) {
      const name: unknown = JSON.parse(value.slice(2));
      return typeof name === "string" && name
        ? { kind: "declared", name }
        : null;
    }
    if (value.startsWith("w:")) {
      const fields: unknown = JSON.parse(value.slice(2));
      if (
        Array.isArray(fields) &&
        fields.length === 3 &&
        fields.every((field) => typeof field === "string") &&
        fields[0] &&
        fields[1]
      ) {
        return {
          kind: "worktree",
          connectionId: fields[0],
          path: fields[1],
          label: fields[2],
        };
      }
    }
  } catch {
    // A hand-edited or stale hash is not fatal; use the normal default.
  }
  return null;
}

/** Format and parse the `m=` bit. A missing or malformed value is left to
 *  the caller's default (collapsed on first load). */
export function formatMusterExpandedHash(expanded: boolean): string {
  return expanded ? "1" : "0";
}

export function parseMusterExpandedHash(value: string | null): boolean | null {
  if (value === "1") return true;
  if (value === "0") return false;
  return null;
}
