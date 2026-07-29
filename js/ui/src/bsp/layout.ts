import { parseDSL } from "@blit-sh/core/bsp";
import type { BSPLayout } from "@blit-sh/core/bsp";

export type {
  BSPLayout,
  BSPPane,
  BSPAssignments,
  TileAssignment,
} from "@blit-sh/core/bsp";
export {
  enumeratePanes,
  assignSessionsToPanes,
  buildCandidateOrder,
  reconcileAssignments,
  adjustWeights,
  layoutFromDSL,
  leafCount,
  PRESETS,
  surfaceAssignment,
  isSurfaceAssignment,
  parseSurfaceAssignment,
  editorAssignment,
  diffAssignment,
  parseDiffArg,
  isTileAssignment,
  parseTileAssignment,
  webAssignment,
  isWebAssignment,
  parseWebAssignment,
  isContentAssignment,
} from "@blit-sh/core/bsp";

import { readStorage, writeStorage } from "../storage";

const LAYOUT_KEY = "blit.layout";
const LAYOUT_HISTORY_KEY = "blit.layouts";
type StoredRecentLayout = string | { name: string; dsl: string };

function parseHash(): Record<string, string> {
  const hash = typeof location !== "undefined" ? location.hash.slice(1) : "";
  if (!hash) return {};
  const result: Record<string, string> = {};
  for (const part of hash.split("&")) {
    const eq = part.indexOf("=");
    if (eq > 0)
      result[decodeURIComponent(part.slice(0, eq))] = decodeURIComponent(
        part.slice(eq + 1),
      );
  }
  return result;
}

function layoutFromDSLString(dsl: string, name?: string): BSPLayout | null {
  try {
    const { root, weight } = parseDSL(dsl);
    return { name: name ?? dsl, dsl, root, weight };
  } catch {
    return null;
  }
}

/** The layout named by the URL hash's `l=` param, or null. Never consults
 *  localStorage — the hashchange handler must not resurrect a stored
 *  layout the current hash doesn't carry. */
export function loadLayoutFromHash(): BSPLayout | null {
  const hash = parseHash();
  if (!hash.l) return null;
  // Format: "name:dsl" when name differs from dsl, otherwise just "dsl".
  const colonIdx = hash.l.indexOf(":");
  if (colonIdx > 0) {
    const name = hash.l.slice(0, colonIdx);
    const dsl = hash.l.slice(colonIdx + 1);
    const layout = layoutFromDSLString(dsl, name);
    if (layout) return layout;
  }
  return layoutFromDSLString(hash.l);
}

export function loadActiveLayout(): BSPLayout | null {
  const fromHash = loadLayoutFromHash();
  if (fromHash) return fromHash;

  // A hash that carries app state but no `l=` is an explicit "no layout" —
  // the app strips `l` when the layout is cleared. Falling back to the
  // stored layout here resurrected it on every remount (page load, PWA
  // relaunch, and each dev-server HMR remount) and on every hashchange:
  // the "screen suddenly splits" bug. The stored layout only seeds a
  // genuinely fresh entry (empty hash or connect-only params).
  const hash = parseHash();
  for (const key of ["t", "s", "a", "p", "tile"]) {
    if (hash[key] !== undefined) return null;
  }

  try {
    const raw = readStorage(LAYOUT_KEY);
    if (!raw) return null;
    const saved = JSON.parse(raw) as { name: string; dsl: string };
    return layoutFromDSLString(saved.dsl, saved.name);
  } catch {
    const dsl = readStorage(LAYOUT_KEY);
    return dsl ? layoutFromDSLString(dsl) : null;
  }
}

export function loadFocusedPaneFromHash(): string | null {
  return parseHash().p || null;
}

/**
 * The non-BSP "focused tile" persisted in the hash as `tile=<encoded>`.
 * parseHash already URL-decodes, so this returns the raw assignment string
 * (editor:/diff:/commit:) or null. The caller validates with isTileAssignment.
 */
export function loadFocusedTileFromHash(): string | null {
  return parseHash().tile || null;
}

/**
 * Parse BSP pane assignments from the URL hash.
 *
 *   a=0:t:hound:28,1.0:s:hound:42,2:t:hound:0k3vq8za
 *     → { "0": "t:hound:28", "1.0": "s:hound:42", "2": "t:hound:0k3vq8za" }
 *
 * A `t:` segment that is all digits is a terminal ptyId; anything else is a
 * server-side tab id (docs/design/kv.md — tab ids are never all-digits).
 * `s:` is a compositor surface. `w:` is a web pane, written for legibility and
 * read as a `t:` tab ref — both resolve through the KV tab registry. The
 * resolver classifies; this just splits.
 */
export function loadAssignmentsFromHash(): Record<string, string> | null {
  const a = parseHash().a;
  if (!a) return null;
  const result: Record<string, string> = {};
  for (const pair of a.split(",")) {
    const colon = pair.indexOf(":");
    if (colon <= 0) continue;
    const paneId = pair.slice(0, colon);
    const rest = pair.slice(colon + 1);
    if (rest.startsWith("w:")) {
      // A web pane is a tab like any other (tabs/<id> in KV); `w:` exists only
      // so a hash reads legibly. Normalize to the tab form so there is exactly
      // one resolution path.
      result[paneId] = `t:${rest.slice(2)}`;
    } else if (rest.startsWith("t:") || rest.startsWith("s:")) {
      result[paneId] = rest;
    }
  }
  return Object.keys(result).length > 0 ? result : null;
}

export function saveActiveLayout(layout: BSPLayout | null): void {
  if (layout) {
    writeStorage(
      LAYOUT_KEY,
      JSON.stringify({ name: layout.name, dsl: layout.dsl }),
    );
  } else {
    try {
      localStorage.removeItem(LAYOUT_KEY);
    } catch {}
  }
}

export function saveToHistory(layout: BSPLayout | string): void {
  pushRecentLayout(layout);
}

/** Remove a layout from the recent history by its DSL string. */
export function removeFromHistory(dsl: string): void {
  try {
    const raw = readStorage(LAYOUT_HISTORY_KEY);
    if (!raw) return;
    const existing: StoredRecentLayout[] = JSON.parse(raw);
    const next = existing.filter((entry) => {
      const d = typeof entry === "string" ? entry : entry.dsl;
      return d !== dsl;
    });
    writeStorage(LAYOUT_HISTORY_KEY, JSON.stringify(next));
  } catch {}
}

export function loadRecentLayouts(): BSPLayout[] {
  try {
    const raw = readStorage(LAYOUT_HISTORY_KEY);
    if (!raw) return [];
    const stored: StoredRecentLayout[] = JSON.parse(raw);
    return stored.flatMap((entry) => {
      const record =
        typeof entry === "string" ? { name: entry, dsl: entry } : entry;
      try {
        const { root, weight } = parseDSL(record.dsl);
        return [{ name: record.name, dsl: record.dsl, root, weight }];
      } catch {
        return [];
      }
    });
  } catch {
    return [];
  }
}

function pushRecentLayout(layout: BSPLayout | string): void {
  const record =
    typeof layout === "string"
      ? { name: layout, dsl: layout }
      : { name: layout.name, dsl: layout.dsl };
  try {
    const raw = readStorage(LAYOUT_HISTORY_KEY);
    const existing: StoredRecentLayout[] = raw ? JSON.parse(raw) : [];
    const next = [
      record,
      ...existing.filter((entry) => {
        const dsl = typeof entry === "string" ? entry : entry.dsl;
        return dsl !== record.dsl;
      }),
    ].slice(0, 10);
    writeStorage(LAYOUT_HISTORY_KEY, JSON.stringify(next));
  } catch {}
}
