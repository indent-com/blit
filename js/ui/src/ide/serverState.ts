/**
 * Editor state in the server KV store (docs/design/kv.md § First consumer):
 * parked dirty buffers under `editor/buf/<abs-path>`, so unsaved edits
 * survive reload, crash, and cross-client reconnects.
 *
 * Value envelope: `[ver:1][base:16 LE][content…]` — `ver` = 0, `base` = the
 * disk content hash (`FsNode.hash`) the buffer diverged from, `content` =
 * the full buffer bytes. Puts are debounced and CAS-chained off the
 * previous put's returned hash (zero on first divergence); the chain is
 * per (connection, path) module state, mirroring `lastWrittenHash` on the
 * fs side. On CONFLICT (another client parks the same file) the put adopts
 * the returned hash and retries once — last-writer-wins between tabs,
 * disclosed in the RFC.
 */

import type { BlitWorkspace, ConnectionId } from "@blit-sh/core";
import { FsConflictError } from "@blit-sh/core";

const PARK_DEBOUNCE_MS = 1000;

const bufKey = (path: string): string => `editor/buf/${path}`;
const chainKey = (connectionId: ConnectionId, path: string): string =>
  `${connectionId} ${path}`;

/** CAS chain: the hash our latest put returned, per (connection, path).
 *  0n = we have not parked (create-exclusive next). */
const chain = new Map<string, bigint>();
const timers = new Map<string, ReturnType<typeof setTimeout>>();

/** Parking is best-effort by design, but a silently inert store makes
 *  "doesn't work" undebuggable — warn once per connection. */
const warned = new Set<ConnectionId>();
function warnOnce(connectionId: ConnectionId, e: unknown): void {
  if (warned.has(connectionId)) return;
  warned.add(connectionId);
  console.warn(
    `blit kv: buffer parking unavailable on "${connectionId}" ` +
      `(old server without the kv store? restart it): ` +
      (e instanceof Error ? e.message : String(e)),
  );
}

/** Encode the buffer envelope. */
export function encodeParkedBuffer(
  base: bigint,
  content: Uint8Array,
): Uint8Array {
  const out = new Uint8Array(17 + content.length);
  const v = new DataView(out.buffer);
  out[0] = 0; // ver
  v.setBigUint64(1, base & 0xffffffffffffffffn, true);
  v.setBigUint64(9, (base >> 64n) & 0xffffffffffffffffn, true);
  out.set(content, 17);
  return out;
}

/** Decode the buffer envelope; null on unknown version / malformed. */
export function decodeParkedBuffer(
  raw: Uint8Array,
): { base: bigint; content: Uint8Array } | null {
  if (raw.length < 17 || raw[0] !== 0) return null;
  const v = new DataView(raw.buffer, raw.byteOffset, raw.byteLength);
  const base = v.getBigUint64(1, true) | (v.getBigUint64(9, true) << 64n);
  return { base, content: raw.slice(17) };
}

async function putNow(
  workspace: BlitWorkspace,
  connectionId: ConnectionId,
  path: string,
  envelope: Uint8Array,
): Promise<void> {
  const ck = chainKey(connectionId, path);
  const prev = chain.get(ck) ?? 0n;
  try {
    const res = await workspace.kvPut(connectionId, bufKey(path), envelope, {
      ifHash: prev,
    });
    chain.set(ck, res.hash);
  } catch (e) {
    if (e instanceof FsConflictError) {
      // Another writer parked this file; adopt its hash and retry once
      // (last-writer-wins between tabs — docs/design/kv.md).
      try {
        const res = await workspace.kvPut(
          connectionId,
          bufKey(path),
          envelope,
          { ifHash: e.hash },
        );
        chain.set(ck, res.hash);
      } catch (e2) {
        // Still conflicting / transport down: parking is best-effort crash
        // insurance, never in the editing path.
        warnOnce(connectionId, e2);
      }
    } else {
      warnOnce(connectionId, e);
    }
  }
}

/** Debounced park of a dirty buffer. Call on every doc change; the write
 *  coalesces to one put per second per file. */
export function parkBuffer(
  workspace: BlitWorkspace,
  connectionId: ConnectionId,
  path: string,
  content: () => Uint8Array,
  base: () => bigint,
): void {
  const ck = chainKey(connectionId, path);
  clearTimeout(timers.get(ck));
  timers.set(
    ck,
    setTimeout(() => {
      timers.delete(ck);
      void putNow(
        workspace,
        connectionId,
        path,
        encodeParkedBuffer(base(), content()),
      );
    }, PARK_DEBOUNCE_MS),
  );
}

/** Flush a pending debounced park immediately (autosave triggers: blur,
 *  tab hide, teardown). */
export function flushParkedBuffer(
  workspace: BlitWorkspace,
  connectionId: ConnectionId,
  path: string,
  content: () => Uint8Array,
  base: () => bigint,
): void {
  const ck = chainKey(connectionId, path);
  const timer = timers.get(ck);
  if (timer !== undefined) {
    clearTimeout(timer);
    timers.delete(ck);
    void putNow(
      workspace,
      connectionId,
      path,
      encodeParkedBuffer(base(), content()),
    );
  }
}

/** The buffer reached disk (save landed) or was discarded: drop the parked
 *  copy. CAS'd on our chain so another tab's newer park survives. */
export function clearParkedBuffer(
  workspace: BlitWorkspace,
  connectionId: ConnectionId,
  path: string,
): void {
  const ck = chainKey(connectionId, path);
  clearTimeout(timers.get(ck));
  timers.delete(ck);
  const prev = chain.get(ck);
  chain.delete(ck);
  if (prev === undefined || prev === 0n) return; // nothing of ours parked
  workspace.kvDelete(connectionId, bufKey(path), { ifHash: prev }).catch(() => {
    // Conflict = another writer re-parked; theirs stands. Other failures:
    // an orphan entry, reconciled on next restore.
  });
}

/** Recall a parked buffer on editor mount. Also primes the CAS chain so a
 *  subsequent park replaces (rather than conflicts with) the entry. */
export async function recallParkedBuffer(
  workspace: BlitWorkspace,
  connectionId: ConnectionId,
  path: string,
): Promise<{ base: bigint; content: Uint8Array } | null> {
  try {
    const res = await workspace.kvFetch(connectionId, bufKey(path));
    if (!res) return null;
    const decoded = decodeParkedBuffer(res.value);
    if (decoded) chain.set(chainKey(connectionId, path), res.hash);
    return decoded;
  } catch {
    return null; // no FEATURE_KV / transport down: behave as before the store
  }
}
