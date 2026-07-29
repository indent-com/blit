/**
 * Workspace roots in the server KV store (docs/design/kv.md § Second
 * consumer): one CAS'd `roots` key per server holding the ordered
 * `name = /path` list (`#`-prefixed = disabled) — the gateway's blit.roots
 * format minus the remote prefix, which is implicit in which server holds
 * the key.
 *
 * Per-server scoping is the feature and the conceded cost in one: every
 * client of a host sees the same roots, and the picker lists the union
 * over CONNECTED servers. The gateway list stays authoritative for servers
 * without the kv store, and seeds a server's key on first contact.
 *
 * Every mutation is read-modify-write CAS'd on the current value hash and
 * retried once on conflict — at human edit rates the retry is invisible,
 * and two clients editing simultaneously converge instead of one side
 * silently losing (the gateway scheme's last-writer-wins hazard).
 */

import { createSignal } from "solid-js";
import type { BlitWorkspace, ConnectionId, KvWatchHandle } from "@blit-sh/core";
import { FsConflictError } from "@blit-sh/core";
import { parseRootsText, type Root } from "../storage";

const textEncoder = new TextEncoder();
const textDecoder = new TextDecoder();

const ROOTS_KEY = "roots";

function serialize(roots: readonly Root[]): string {
  return roots
    .map((r) => `${r.disabled ? "# " : ""}${r.name} = ${r.path}`)
    .join("\n");
}

/** Parse a server `roots` document; `remote` is stamped with the owning
 *  connection so downstream remote→connection resolution keeps working. */
function parse(text: string, connectionId: ConnectionId): Root[] {
  return parseRootsText(text).map((r) => ({ ...r, remote: connectionId }));
}

type WatchState = {
  handle: KvWatchHandle | null;
  generation: number;
  hash: bigint;
};

const watches = new Map<ConnectionId, WatchState>();
const [serverRoots, setServerRoots] = createSignal<Map<ConnectionId, Root[]>>(
  new Map(),
  { equals: false },
);

/** Roots stored on connected servers, in per-server document order. */
export function allServerRoots(): Root[] {
  const out: Root[] = [];
  for (const roots of serverRoots().values()) out.push(...roots);
  return out;
}

/** True while `connectionId`'s roots come from its server (a live watch). */
export function hasServerRoots(connectionId: ConnectionId): boolean {
  return watches.has(connectionId);
}

/**
 * Idempotently watch one server's `roots` key, seeding it from the gateway
 * list on first contact (create-exclusive: a concurrent client seeding too
 * conflicts harmlessly). Re-call freely — e.g. from an effect over the
 * connection snapshots with the connection generation, which re-arms the
 * watch after a re-establish (subscriptions don't survive one).
 */
export function ensureServerRoots(
  workspace: BlitWorkspace,
  connectionId: ConnectionId,
  generation: number,
  seed: () => Root[],
): void {
  const existing = watches.get(connectionId);
  if (existing && existing.generation === generation) return;
  existing?.handle?.close();
  const state: WatchState = { handle: null, generation, hash: 0n };
  watches.set(connectionId, state);
  workspace
    .watchKv(connectionId, ROOTS_KEY, {
      onUpdate: (mirror) => {
        const entry = mirror.live.get(ROOTS_KEY);
        state.hash = entry?.hash ?? 0n;
        if (entry?.value) {
          const roots = parse(textDecoder.decode(entry.value), connectionId);
          setServerRoots((m) => (m.set(connectionId, roots), m));
        } else {
          setServerRoots((m) => (m.set(connectionId, []), m));
          if (mirror.snapshotDone && entry === undefined) {
            // First contact: seed from the gateway's entries for this
            // server, then the store is authoritative.
            const mine = seed();
            if (mine.length > 0) {
              workspace
                .kvPut(
                  connectionId,
                  ROOTS_KEY,
                  textEncoder.encode(serialize(mine)),
                  { create: true },
                )
                .catch(() => {});
            }
          }
        }
      },
      onClosed: () => {
        // Connection lost/re-established: drop so the ensure-effect re-arms,
        // and stop advertising this server's roots meanwhile.
        if (watches.get(connectionId) === state) watches.delete(connectionId);
        setServerRoots((m) => (m.delete(connectionId), m));
      },
    })
    .then((handle) => {
      const cur = watches.get(connectionId);
      if (cur !== state) {
        handle.close(); // superseded while opening
        return;
      }
      state.handle = handle;
    })
    .catch(() => {
      // Transient open failure: drop so the ensure-effect retries.
      if (watches.get(connectionId) === state) watches.delete(connectionId);
    });
}

/** Read-modify-write one server's roots under CAS; one conflict retry. */
async function mutate(
  workspace: BlitWorkspace,
  connectionId: ConnectionId,
  transform: (roots: Root[]) => Root[],
): Promise<void> {
  const attempt = async (): Promise<void> => {
    const cur = await workspace.kvFetch(connectionId, ROOTS_KEY);
    const roots = cur ? parse(textDecoder.decode(cur.value), connectionId) : [];
    const next = serialize(transform(roots));
    await workspace.kvPut(
      connectionId,
      ROOTS_KEY,
      textEncoder.encode(next),
      cur ? { ifHash: cur.hash } : { create: true },
    );
  };
  try {
    await attempt();
  } catch (e) {
    if (e instanceof FsConflictError) await attempt();
    // Anything else: best-effort, the watch keeps the UI truthful.
  }
}

export function addServerRoot(
  workspace: BlitWorkspace,
  connectionId: ConnectionId,
  name: string,
  path: string,
): void {
  void mutate(workspace, connectionId, (roots) => [
    ...roots.filter((r) => r.name !== name),
    { name, remote: connectionId, path, disabled: false },
  ]);
}

export function removeServerRoot(
  workspace: BlitWorkspace,
  connectionId: ConnectionId,
  name: string,
): void {
  void mutate(workspace, connectionId, (roots) =>
    roots.filter((r) => r.name !== name),
  );
}

export function toggleServerRoot(
  workspace: BlitWorkspace,
  connectionId: ConnectionId,
  name: string,
): void {
  void mutate(workspace, connectionId, (roots) =>
    roots.map((r) => (r.name === name ? { ...r, disabled: !r.disabled } : r)),
  );
}

/** Reorder this server's roots to match `names` (unknown names keep their
 *  relative order at the end — a concurrent add survives a reorder). */
export function reorderServerRoots(
  workspace: BlitWorkspace,
  connectionId: ConnectionId,
  names: readonly string[],
): void {
  void mutate(workspace, connectionId, (roots) => {
    const byName = new Map(roots.map((r) => [r.name, r]));
    const ordered: Root[] = [];
    for (const name of names) {
      const r = byName.get(name);
      if (r) {
        ordered.push(r);
        byName.delete(name);
      }
    }
    return [...ordered, ...byName.values()];
  });
}
