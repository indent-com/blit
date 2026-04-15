import type { SessionId } from "@blit-sh/core";
import { parseDSL } from "@blit-sh/core/bsp";
import type { BSPLayout } from "@blit-sh/core/bsp";

export type { BSPLayout, BSPPane, BSPAssignments } from "@blit-sh/core/bsp";
export {
  enumeratePanes,
  assignSessionsToPanes,
  buildCandidateOrder,
  reconcileAssignments,
  adjustWeights,
  layoutFromDSL,
  PRESETS,
  surfaceAssignment,
  isSurfaceAssignment,
  parseSurfaceAssignment,
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

export function loadActiveLayout(): BSPLayout | null {
  const hash = parseHash();
  if (hash.l) {
    // Format: "name:dsl" when name differs from dsl, otherwise just "dsl".
    const colonIdx = hash.l.indexOf(":");
    if (colonIdx > 0) {
      const name = hash.l.slice(0, colonIdx);
      const dsl = hash.l.slice(colonIdx + 1);
      const layout = layoutFromDSLString(dsl, name);
      if (layout) return layout;
    }
    const layout = layoutFromDSLString(hash.l);
    if (layout) return layout;
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
 * Parse BSP pane assignments from the URL hash.
 *
 * New format (t:/s: prefixed):
 *   a=0:t:hound:28,1.0:s:hound:42
 *     → { "0": "t:hound:28", "1.0": "s:hound:42" }
 *
 * Legacy format (no prefix — treated as terminal):
 *   a=0:hound:28,1:local:3
 *     → { "0": "t:hound:28", "1": "t:local:3" }
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
    if (rest.startsWith("t:") || rest.startsWith("s:")) {
      // New format: "t:connectionId:ptyId" or "s:connectionId:surfaceId"
      result[paneId] = rest;
    } else {
      // Legacy format: "connectionId:ptyId" — treat as terminal
      result[paneId] = `t:${rest}`;
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
