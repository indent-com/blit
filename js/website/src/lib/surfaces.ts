import { createSignal, onCleanup, type Accessor } from "solid-js";
import type { BlitSurface, BlitWorkspace, ConnectionId } from "@blit-sh/core";

/**
 * Reactive accessor for top-level surfaces on a single connection. Subsurfaces
 * (parentId !== 0) are composited into their parent and excluded.
 *
 * The returned array is replaced wholesale on every store change so a
 * surface entry whose width/height/title was mutated in place by
 * SurfaceStore picks up the new values when the consumer reads them.
 */
export function createSurfaces(
  workspace: BlitWorkspace,
  connectionId: ConnectionId,
): Accessor<readonly BlitSurface[]> {
  const [surfaces, setSurfaces] = createSignal<readonly BlitSurface[]>([]);

  const sync = () => {
    const conn = workspace.getConnection(connectionId);
    if (!conn) {
      setSurfaces([]);
      return;
    }
    const next: BlitSurface[] = [];
    for (const s of conn.surfaceStore.getSurfaces().values()) {
      if (s.parentId !== 0) continue;
      next.push({ ...s });
    }
    setSurfaces(next);
  };

  // Re-attach the surfaceStore listener whenever the connection set
  // changes — the connection may not exist yet at first call, and a
  // reconnect could swap the SurfaceStore instance.
  let storeUnsub: (() => void) | null = null;
  let attachedConn: ReturnType<BlitWorkspace["getConnection"]> | null = null;

  const ensureStoreSub = () => {
    const conn = workspace.getConnection(connectionId);
    if (conn === attachedConn) return;
    storeUnsub?.();
    storeUnsub = null;
    attachedConn = conn;
    if (conn) {
      storeUnsub = conn.surfaceStore.onChange(sync);
    }
  };

  ensureStoreSub();
  sync();

  const wsUnsub = workspace.subscribe(() => {
    ensureStoreSub();
    sync();
  });

  onCleanup(() => {
    storeUnsub?.();
    wsUnsub();
  });

  return surfaces;
}
