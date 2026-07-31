/**
 * The host-wide open-tab list, mirrored from each server's `tabs/` KV prefix
 * (docs/design/kv.md).
 *
 * `tabRegistry.ts` already writes every opened tab to `tabs/<id>`; until now
 * that record was only ever point-read, by `resolveTab`, to expand one hash
 * ref. Watching the prefix instead is what makes the RFC's actual claim true —
 * open files become "always-on server-backed state that a client _views_, not
 * state a client _owns_": every frontend on a host sees the same set of tabs,
 * and a tile that leaves one client's viewport is still listed everywhere,
 * rather than surviving only in that client's session-local dock signal.
 *
 * Values are the bare (connection-less) assignment, so each mirror's entries
 * are re-tagged with the connection they came from. Ordering is by `mtimeNs` —
 * registration is a NO_CAS put on every open, so newest-touched sorts first,
 * which is the recency order the dock already wanted.
 *
 * Degradation matches the rest of the kv consumers: a server without
 * `FEATURE_KV` (or a transport that refuses the watch) simply contributes no
 * entries, and the caller's local fallback list carries the dock alone.
 */

import { createEffect, createMemo, createSignal, onCleanup } from "solid-js";
import type { BlitWorkspace, ConnectionId, KvWatchHandle } from "@blit-sh/core";
import { TAB_PREFIX, withConn } from "./tabRegistry";

const textDecoder = new TextDecoder();

/** One registered tab, tagged with the connection whose store it came from. */
export interface OpenTab {
  /** Full assignment, connection re-inserted (`editor:<conn>:/abs/path`). */
  readonly assignment: string;
  readonly mtimeNs: bigint;
}

/** The connection fields the watch is gated on — `wsState().connections`. */
interface ConnectionInfo {
  readonly id: string;
  readonly ready: boolean;
  readonly supportsKv: boolean;
}

/** A server-initiated close (`S2C_KV_CLOSED`, e.g. a resource limit) is worth
 *  retrying a few times; a dropped connection isn't retried here at all, since
 *  `ready` flips false and re-running on reconnect is the effect's job. */
const MAX_REOPEN_ATTEMPTS = 5;

interface Watch {
  handle: KvWatchHandle | null;
  /** Set from `onUpdate`/the resolved handle; null until the first arrives. */
  live: ReadonlyMap<string, { value: Uint8Array | null; mtimeNs: bigint }>;
}

export function createOpenTabs(
  workspace: BlitWorkspace,
  connections: () => readonly ConnectionInfo[],
): () => OpenTab[] {
  // KvMirror.live is a plain Map mutated outside Solid's graph, so the memo
  // below is driven by this counter rather than by the map itself.
  const [version, setVersion] = createSignal(0);
  const bump = () => setVersion((n) => n + 1);
  const watches = new Map<string, Watch>();
  const reopenAttempts = new Map<string, number>();
  const [reopenTick, setReopenTick] = createSignal(0);
  let disposed = false;

  createEffect(() => {
    reopenTick();
    const wanted = new Set<string>();
    for (const c of connections()) {
      if (!c.ready || !c.supportsKv) continue;
      wanted.add(c.id);
      if (watches.has(c.id)) continue;
      const watch: Watch = { handle: null, live: new Map() };
      watches.set(c.id, watch);
      void workspace
        .watchKv(c.id as ConnectionId, TAB_PREFIX, {
          onUpdate: (mirror) => {
            watch.live = mirror.live;
            bump();
          },
          onClosed: () => {
            if (watches.get(c.id) !== watch) return;
            watches.delete(c.id);
            bump();
            const attempts = (reopenAttempts.get(c.id) ?? 0) + 1;
            reopenAttempts.set(c.id, attempts);
            if (disposed || attempts > MAX_REOPEN_ATTEMPTS) return;
            setTimeout(() => setReopenTick((n) => n + 1), 250 * attempts);
          },
        })
        .then((handle) => {
          // Disposed, or superseded by a re-open, while the open was in
          // flight: the handle is ours to close, not to install.
          if (disposed || watches.get(c.id) !== watch) {
            handle.close();
            return;
          }
          watch.handle = handle;
          watch.live = handle.mirror.live;
          reopenAttempts.delete(c.id);
          bump();
        })
        .catch(() => {
          // No kv on this server, or the transport refused: contribute
          // nothing. The effect retries when the connection list changes.
          if (watches.get(c.id) === watch) watches.delete(c.id);
        });
    }
    for (const [id, watch] of [...watches]) {
      if (wanted.has(id)) continue;
      watch.handle?.close();
      watches.delete(id);
      reopenAttempts.delete(id);
      bump();
    }
  });

  onCleanup(() => {
    disposed = true;
    for (const watch of watches.values()) watch.handle?.close();
    watches.clear();
  });

  return createMemo<OpenTab[]>(() => {
    version();
    const tabs: OpenTab[] = [];
    for (const [connectionId, watch] of watches) {
      for (const [key, entry] of watch.live) {
        if (!key.startsWith(TAB_PREFIX)) continue;
        // Tab values are a handful of bytes, so they always arrive inline;
        // a metadata-only entry would mean someone else wrote the key.
        if (!entry.value) continue;
        const assignment = withConn(
          textDecoder.decode(entry.value),
          connectionId as ConnectionId,
        );
        if (assignment) tabs.push({ assignment, mtimeNs: entry.mtimeNs });
      }
    }
    tabs.sort((a, b) =>
      a.mtimeNs === b.mtimeNs ? 0 : a.mtimeNs > b.mtimeNs ? -1 : 1,
    );
    return tabs;
  });
}
