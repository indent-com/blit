/**
 * Solid glue over the core {@link ReactiveStore} surface.
 *
 * The fs/git/lsp handles are framework-agnostic reactive stores (`subscribe`
 * + `revision`). These two helpers are the *only* place the UI bridges them
 * into Solid — panels and tiles never wire mirror callbacks by hand.
 */

import { createEffect, createSignal, onCleanup, type Accessor } from "solid-js";
import type { ReactiveStore, BlitWorkspaceSnapshot } from "@blit-sh/core";

/**
 * True for an open/request failure that only means "the transport moved under
 * us" — the connection dropped, is mid-handshake, or was re-established. These
 * are never worth showing: a re-establish resets fs/git/lsp *after* re-emitting
 * its snapshot, and an open registers its pending entry synchronously, so an
 * open issued during that emit is rejected before the retry-driving generation
 * has settled. Callers retry instead of surfacing a dead end.
 */
export function isTransientConnError(e: unknown): boolean {
  const msg = e instanceof Error ? e.message : String(e);
  return /transport is|re-established|shutting down|not connected/i.test(msg);
}

/** Ceiling on consecutive transient retries — a real reconnect needs one;
 *  the bound keeps a synchronously-rejecting open out of a microtask loop. */
const MAX_OPEN_RETRIES = 20;

/**
 * True when `connectionId` can serve the given capability. A tile can be opened
 * on a connection that has completed its handshake (features negotiated) but is
 * still "authenticating" — e.g. a declared root whose remote has no terminals,
 * so it never receives a session LIST and never reaches "connected". syncFs /
 * openRepo work as soon as the transport is up AND the feature was negotiated,
 * so gate on the capability flag, not on status === "connected".
 */
export function isConnReady(
  snapshot: BlitWorkspaceSnapshot,
  connectionId: string,
  capability: "supportsFsSync" | "supportsGit",
): boolean {
  const c = snapshot.connections.find((x) => x.id === connectionId);
  return (
    !!c &&
    c[capability] &&
    (c.status === "connected" || c.status === "authenticating")
  );
}

/** A connection's reset generation (bumps on every transport drop AND server
 *  re-establish). Reading it inside a tile's open effect makes the tile
 *  re-open its fs/git handle after a reset even when the transport never left
 *  "connected" — those handles don't survive a reset. */
export function connGeneration(
  snapshot: BlitWorkspaceSnapshot,
  connectionId: string,
): number {
  return (
    snapshot.connections.find((x) => x.id === connectionId)?.generation ?? 0
  );
}

/**
 * Track a store's `revision` as a Solid accessor: reading it inside a
 * memo/effect subscribes that computation to every store emit. Read it,
 * then read the store's own data (the mirror). Auto-unsubscribes on
 * cleanup, so call it under a reactive owner (component or `createRoot`).
 */
export function trackStore(store: ReactiveStore): Accessor<number> {
  const [rev, setRev] = createSignal(store.revision);
  onCleanup(store.subscribe(() => setRev(store.revision)));
  return rev;
}

export interface OwnedHandle<H> {
  /** The handle once opened; `null` while opening or after an error. */
  handle: Accessor<H | null>;
  /** Open error, if any. */
  error: Accessor<string | null>;
  /** Bumps on every store emit; `0` until the handle opens. */
  version: Accessor<number>;
}

/**
 * Own a single handle for the lifetime of a self-contained tile (editor,
 * diff): open on mount, track its store, tear down on cleanup. Unlike the
 * shared IdeSession registry, this makes no attempt to coalesce or keep warm.
 *
 * `open` is reactive: it runs inside an effect, so any signal it reads (e.g.
 * the connection's status) re-opens the handle when it changes. Return `null`
 * from `open` to mean "not ready yet" (e.g. the connection is still
 * connecting) — no handle or error is produced, and the handle reopens once a
 * dependency changes. This is what lets a tile restored on reload wait for its
 * transport instead of erroring out permanently.
 */
export function useOwnedHandle<H extends ReactiveStore>(
  open: () => Promise<H> | null,
  teardown: (handle: H) => void,
): OwnedHandle<H> {
  const [handle, setHandle] = createSignal<H | null>(null);
  const [error, setError] = createSignal<string | null>(null);
  const [version, setVersion] = createSignal(0);
  // Re-attempt after a transient rejection. The generation the caller tracks
  // may already have bumped when the open was rejected, so nothing else would
  // re-run this effect and the tile would sit on a stale error forever.
  const [retry, setRetry] = createSignal(0);
  let retries = 0;

  createEffect(() => {
    retry();
    const pending = open();
    // Clear any prior handle/error before (re)opening so consumers never read
    // a torn-down handle during the gap.
    setHandle(null);
    setError(null);
    if (!pending) return; // not ready — wait for a dependency to change
    let disposed = false;
    let opened: H | null = null;
    let unsub: () => void = () => {};
    pending
      .then((h) => {
        if (disposed) {
          teardown(h);
          return;
        }
        retries = 0;
        opened = h;
        unsub = h.subscribe(() => setVersion((v) => v + 1));
        setHandle(() => h);
      })
      .catch((e: unknown) => {
        if (disposed) return;
        // A transport that moved under us is not a tile error: stay
        // handle-less (consumers read that as loading) and re-attempt.
        if (isTransientConnError(e) && retries++ < MAX_OPEN_RETRIES) {
          setRetry((n) => n + 1);
          return;
        }
        setError(e instanceof Error ? e.message : String(e));
      });
    onCleanup(() => {
      disposed = true;
      unsub();
      if (opened) teardown(opened);
    });
  });

  return { handle, error, version };
}
