/**
 * Every connected server's application catalog, held open for the page.
 *
 * The Manage panel opens a `blit.session.v1` channel when a viewer expands a
 * remote and closes it when they leave, which is right for a panel: it costs
 * nothing while nobody is looking. The switcher cannot work that way. It has to
 * filter a thousand applications from the first keystroke, and a catalog
 * fetched when Cmd-K opens arrives a second late — by which time the viewer has
 * typed the name of something the list still does not know about.
 *
 * So this holds one channel per connected server for the life of the page. The
 * standing cost is small and one-sided: the catalog rides the greeting once,
 * and after that the supervisor only speaks when an application's state
 * changes. Icons are still asked for a screenful at a time, by whoever is
 * drawing them.
 *
 * Shaped like {@link ./ide/rootsStore.ts}: a module-scope map plus a version
 * signal, armed idempotently from an effect over the connection snapshots. The
 * generation is what makes a reconnect re-arm — a channel does not survive one,
 * and without it a server that dropped and came back would stay silent.
 */

import { createSignal } from "solid-js";
import type { BlitWorkspace, ConnectionId } from "@blit-sh/core";
import {
  openSession,
  type SessionApp,
  type SessionCatalogEntry,
  type SessionHandle,
} from "./session";

/** One server's applications: what it manages, and what it could run. */
export interface RemoteApplications {
  readonly connectionId: ConnectionId;
  /** Applications the supervisor is managing, running or not. */
  readonly apps: readonly SessionApp[];
  /** Everything installed there, sorted by display name. */
  readonly catalog: readonly SessionCatalogEntry[];
}

type OpenState = {
  handle: SessionHandle | null;
  generation: number;
};

const opens = new Map<ConnectionId, OpenState>();

/** Bumped on every message from any supervisor. Readers touch it to become
 *  reactive, exactly as the file index's consumers touch its version. */
const [version, setVersion] = createSignal(0);
const bump = () => setVersion((n) => n + 1);

/**
 * Idempotently hold one server's supervisor channel open.
 *
 * Re-call freely — from an effect over the connection snapshots, passing the
 * connection generation, which re-arms after a re-establish. A server running
 * no supervisor refuses the channel, which is an answer rather than a failure:
 * the slot is dropped so a later generation can try again.
 */
export function ensureSessionCatalog(
  workspace: BlitWorkspace,
  connectionId: ConnectionId,
  generation: number,
): void {
  const existing = opens.get(connectionId);
  if (existing && existing.generation === generation) return;
  existing?.handle?.close();

  const connection = workspace.getConnection(connectionId);
  if (!connection) return;
  const state: OpenState = { handle: null, generation };
  opens.set(connectionId, state);

  void openSession(connection, {
    onClosed: () => {
      if (opens.get(connectionId) === state) opens.delete(connectionId);
      bump();
    },
  })
    .then((handle) => {
      // Superseded while opening — a reconnect landed, or the connection went.
      if (opens.get(connectionId) !== state) {
        handle.close();
        return;
      }
      state.handle = handle;
      handle.subscribe(bump);
      bump();
    })
    .catch(() => {
      if (opens.get(connectionId) === state) opens.delete(connectionId);
    });
}

/** Close a server's channel, e.g. because the connection went away. */
export function dropSessionCatalog(connectionId: ConnectionId): void {
  const state = opens.get(connectionId);
  if (!state) return;
  opens.delete(connectionId);
  state.handle?.close();
  bump();
}

const ready = (connectionId: ConnectionId): SessionHandle | null =>
  opens.get(connectionId)?.handle ?? null;

/**
 * The live supervisor channel for one server, for a caller that needs the
 * verbs and not just the lists.
 *
 * The Manage panel used to open a channel of its own. Sharing this one is not
 * only fewer channels: each mirror carries its own icon cache, and two of them
 * for the same server means the same artwork fetched and held twice. Callers
 * must not close it — it belongs to the store, and outlives any one panel.
 *
 * Reactive: null until the greeting has been asked for, and again if the
 * connection drops.
 */
export function sessionHandle(
  connectionId: ConnectionId,
): SessionHandle | null {
  version();
  return ready(connectionId);
}

/** One server's applications, or null while it has no supervisor attached. */
export function sessionCatalog(
  connectionId: ConnectionId,
): RemoteApplications | null {
  version();
  const handle = ready(connectionId);
  if (!handle) return null;
  return { connectionId, apps: handle.apps, catalog: handle.catalog };
}

/** Every attached server's applications, in the order asked for. */
export function sessionCatalogs(
  connectionIds: readonly ConnectionId[],
): RemoteApplications[] {
  version();
  const out: RemoteApplications[] = [];
  for (const connectionId of connectionIds) {
    const found = sessionCatalog(connectionId);
    if (found) out.push(found);
  }
  return out;
}

/** Artwork for one application: a data URL, `null` for none, `undefined`
 *  while nobody has asked. Reactive — it lands long after the row is drawn. */
export function applicationIcon(
  connectionId: ConnectionId,
  id: string,
): string | null | undefined {
  version();
  return ready(connectionId)?.icon(id);
}

/** Ask one server for artwork; ids already known or in flight are dropped. */
export function requestApplicationIcons(
  connectionId: ConnectionId,
  ids: readonly string[],
): void {
  ready(connectionId)?.requestIcons(ids);
}

/**
 * Run one application now, without adopting it.
 *
 * `start`, not `enable`: launching something from the switcher is trying it,
 * not choosing it for every session from here on. It appears in the Manage
 * panel as a running row that is not enabled, which is where it can be kept or
 * discarded.
 */
export function startApplication(
  connectionId: ConnectionId,
  id: string,
): boolean {
  const handle = ready(connectionId);
  if (!handle) return false;
  handle.start(id);
  return true;
}
