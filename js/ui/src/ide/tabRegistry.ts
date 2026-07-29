/**
 * Server-side tab registry (docs/design/kv.md): every opened tab — editors,
 * diffs, commits, and web panes — is tracked under `tabs/<id>` in the host's
 * KV store, and URL hashes reference the short id instead of a URL-encoded
 * assignment string. A web pane's URL is exactly the kind of long value that
 * motivated this: encoded in a hash it dwarfs the rest of the layout.
 *
 * The id is deterministic — FNV-1a 64 over the CONNECTION-LESS assignment,
 * 8 base36 chars — so two clients opening the same file on the same host
 * mint the same id and NO_CAS puts dedupe idempotently. The stored value is
 * the bare assignment (`editor:/abs/path`, `diff:staged:/abs/path`,
 * `commit:<oid>:<repo>`): connection names are client-local labels and
 * never go server-side (kv.md § First consumer). Collision math: 36^8 ≈
 * 2.8e12; at human tab counts the birthday probability is ~1e-9, accepted.
 *
 * Ids are never all-digits (char 0 is forced to "z" in that ~3.6e-5 case):
 * inside the hash's `t:<conn>:<seg>` grammar a digit-only seg is a terminal
 * ptyId and anything else is a tab id — that partition must stay total.
 *
 * Registration is fire-and-forget crash insurance, the serverState.ts
 * posture: failures (no FEATURE_KV, transport down) are silent and the
 * URL-hash fallback still carries the id.
 */

import type { BlitWorkspace, ConnectionId } from "@blit-sh/core";
import {
  parseTileAssignment,
  isTileAssignment,
  isWebAssignment,
  parseWebAssignment,
} from "@blit-sh/core/bsp";

const textEncoder = new TextEncoder();
const textDecoder = new TextDecoder();

const FNV_OFFSET = 0xcbf29ce484222325n;
const FNV_PRIME = 0x100000001b3n;
const U64 = 0xffffffffffffffffn;

/** Short id for a bare (connection-less) assignment. */
export function tabId(bare: string): string {
  let h = FNV_OFFSET;
  for (const byte of textEncoder.encode(bare)) {
    h = ((h ^ BigInt(byte)) * FNV_PRIME) & U64;
  }
  let id = (h % 36n ** 8n).toString(36).padStart(8, "0");
  if (/^\d+$/.test(id)) id = `z${id.slice(1)}`;
  return id;
}

/** Split an assignment into its connection and the bare server-side form. */
export function stripConn(
  assignment: string,
): { connectionId: ConnectionId; bare: string } | null {
  const web = parseWebAssignment(assignment);
  if (web) {
    return {
      connectionId: web.connectionId as ConnectionId,
      bare: `web:${web.url}`,
    };
  }
  const t = parseTileAssignment(assignment);
  if (!t) return null;
  return {
    connectionId: t.connectionId as ConnectionId,
    bare: `${t.kind}:${t.arg}`,
  };
}

/** Re-insert a connection into a bare assignment; null when malformed. */
export function withConn(
  bare: string,
  connectionId: ConnectionId,
): string | null {
  const colon = bare.indexOf(":");
  if (colon <= 0) return null;
  const full = `${bare.slice(0, colon)}:${connectionId}:${bare.slice(colon + 1)}`;
  return isTileAssignment(full) || isWebAssignment(full) ? full : null;
}

const tabKey = (id: string): string => `tabs/${id}`;

/** Fire-and-forget registration; idempotent (deterministic id ⇒ same value). */
export function registerTab(
  workspace: BlitWorkspace,
  assignment: string,
): void {
  const s = stripConn(assignment);
  if (!s) return;
  workspace
    .kvPut(s.connectionId, tabKey(tabId(s.bare)), textEncoder.encode(s.bare))
    .catch(() => {});
}

/** Fire-and-forget deletion (the tab left every pane, the active view, and
 *  the background dock). */
export function unregisterTab(
  workspace: BlitWorkspace,
  assignment: string,
): void {
  const s = stripConn(assignment);
  if (!s) return;
  workspace.kvDelete(s.connectionId, tabKey(tabId(s.bare))).catch(() => {});
}

/** Resolve a short id back to a full assignment for `connectionId`.
 *  null = DEFINITIVELY absent or malformed (give up); transport/feature
 *  failures THROW so callers can retry — a boot-time re-establish rejects
 *  in-flight fetches, and treating that as "absent" would silently drop
 *  hash-restored tiles. */
export async function resolveTab(
  workspace: BlitWorkspace,
  connectionId: ConnectionId,
  id: string,
): Promise<string | null> {
  const res = await workspace.kvFetch(connectionId, tabKey(id));
  if (!res) return null;
  return withConn(textDecoder.decode(res.value), connectionId);
}

/** A hash `t:` segment names a terminal ptyId iff it is all digits; tab ids
 *  never are (see module header). Strict regex — parseInt("12ab") === 12
 *  would misroute a tab id as a ptyId. */
export function isPtyRef(seg: string): boolean {
  return /^\d+$/.test(seg);
}
