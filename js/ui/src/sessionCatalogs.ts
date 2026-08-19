/**
 * Every connected server's application catalog, held open for the page.
 *
 * The Manage panel opens a `blit.session.v1` channel when a viewer expands a
 * remote and closes it when they leave, which is right for a panel: it costs
 * nothing while nobody is looking. The switcher cannot work that way. It has to
 * filter a thousand applications from the first keystroke instead of fetching
 * one when it opens.
 *
 * So this holds one channel per connected server for the life of the page. The
 * standing cost is small and one-sided: the catalog rides the greeting once,
 * and after that the supervisor only speaks when an application's state
 * changes. Icons are still asked for a screenful at a time, by whoever is
 * drawing them.
 *
 * Shaped like {@link ./ide/rootsStore.ts}: a module-scope map plus a version
 * signal. It is armed from an effect over the connection snapshots, but it
 * also follows `CHANNEL_WATCH` for `blit.session.v1` so that installing the
 * session extension after connect (or uninstalling and reinstalling it) opens
 * or closes the catalog without requiring a reconnect.
 */

import { createSignal } from "solid-js";
import type { BlitWorkspace, ConnectionId } from "@blit-sh/core";
import { followChannelNames } from "./channelPresence";
import {
  openSession,
  SESSION_CHANNEL,
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
  /** Stops the `CHANNEL_WATCH` follow for this connection. */
  stopChannelWatch: (() => void) | null;
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
 * connection generation, which re-arms after a re-establish. The channel itself
 * is opened only while the server's registry reports `blit.session.v1` as
 * present, so uninstalling the session extension closes the catalog and
 * reinstalling it reopens it without requiring a reconnect.
 */
export function ensureSessionCatalog(
  workspace: BlitWorkspace,
  connectionId: ConnectionId,
  generation: number,
): void {
  const existing = opens.get(connectionId);
  if (existing && existing.generation === generation) return;
  // Superseded by a new generation, or the first call for this connection.
  existing?.stopChannelWatch?.();
  existing?.handle?.close();

  const connection = workspace.getConnection(connectionId);
  if (!connection) return;
  const state: OpenState = {
    handle: null,
    generation,
    stopChannelWatch: null,
  };
  opens.set(connectionId, state);

  let live = true;
  let stopWatch: (() => void) | null = null;
  state.stopChannelWatch = () => {
    live = false;
    stopWatch?.();
    state.handle?.close();
    state.handle = null;
    if (opens.get(connectionId) === state) opens.delete(connectionId);
  };

  void followChannelNames(connection, [SESSION_CHANNEL], (present) => {
    if (!live) return;
    if (present.has(SESSION_CHANNEL)) {
      if (state.handle) return;
      void openSession(connection, {
        onClosed: () => {
          if (!live) return;
          // The channel went away (extension stopped). Close the handle so the
          // icon timer and object URLs are released, but keep the slot and the
          // watch alive: a reinstall will make the channel present again and
          // we'll reopen.
          state.handle?.close();
          state.handle = null;
          bump();
        },
      })
        .then((handle) => {
          if (!live || opens.get(connectionId) !== state) {
            handle.close();
            return;
          }
          state.handle = handle;
          handle.subscribe(bump);
          bump();
        })
        .catch(() => {
          // A refused open is transient while the channel is flapping; the
          // next presence update will retry.
        });
    } else {
      if (state.handle) {
        state.handle.close();
        state.handle = null;
        bump();
      }
    }
  }).then((release) => {
    if (live) stopWatch = release;
    else release();
  });
}

/** Close a server's channel and stop watching its presence. */
export function dropSessionCatalog(connectionId: ConnectionId): void {
  const state = opens.get(connectionId);
  if (!state) return;
  state.stopChannelWatch?.();
  // stopChannelWatch deletes the state and closes the handle.
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
 * connection drops or the channel is not currently served.
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
