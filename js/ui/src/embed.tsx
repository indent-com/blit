/**
 * Embedding entry point: the full blit workspace as a mountable component,
 * for hosts that are not the app shell — blit.sh's share page is the first.
 *
 * `App` is the shell: it owns same-origin auth, the gateway mux, the remotes
 * list, and the config socket, all of which presume the page is served by a
 * blit server. None of that holds on a marketing site opening a share link.
 * `Workspace` below it never had those assumptions — it takes a list of
 * (id, transport) pairs and renders the whole product — so embedding is a
 * matter of exposing that seam, not of building a second, lesser client.
 * The 900-line reimplementation this replaces on blit.sh/s is the argument
 * for doing it this way: it had drifted from the app it imitated.
 */

import { render } from "solid-js/web";
import { createShareTransport } from "@blit-sh/core";
import type { BlitDebug, BlitTransport, BlitWasmModule } from "@blit-sh/core";
import { Workspace } from "./Workspace";
import { setShellCapabilities } from "./shellCapabilities";
import type { ConnectionSpec } from "./App";
import type { ShellCapabilities } from "./shellCapabilities";

export type { ConnectionSpec };

export interface EmbedOptions {
  wasm: BlitWasmModule;
  /** Connections to drive, static or reactive; each owns its transport. */
  connections: ConnectionSpec[] | (() => ConnectionSpec[]);
  /** Shell affordances the host page can honour. Defaults to none of the
   *  app shell's extras: no remotes management (the host fixes the
   *  connection list) and no preview service worker (there is no sw.js at
   *  the host's origin). */
  capabilities?: Partial<ShellCapabilities>;
  /** A transport authenticated once and then refused — a revoked share
   *  passphrase, an expired link. The host owns the surrounding page, so it
   *  owns the apology. */
  onAuthError?: () => void;
}

/**
 * Mount the workspace into `root` and return a disposer.
 *
 * The container must have a definite height — the workspace fills it. The
 * app shell's global CSS (border-box sizing, `line-height: 1`, no
 * overscroll) is applied to the container here rather than assumed of the
 * page: blit is a terminal first and every pane sits on that tight rhythm,
 * but an embedding page has typography of its own that a global reset
 * would trample.
 */
export function mountBlitWorkspace(
  root: HTMLElement,
  opts: EmbedOptions,
): () => void {
  setShellCapabilities({
    remotes: false,
    previews: false,
    serverRoutes: false,
    ...opts.capabilities,
  });
  root.style.lineHeight = "1";
  root.style.boxSizing = "border-box";
  root.style.overflow = "hidden";
  root.style.overscrollBehavior = "none";
  const dispose = render(
    () => (
      <Workspace
        connections={opts.connections}
        wasm={opts.wasm}
        onAuthError={opts.onAuthError ?? (() => {})}
      />
    ),
    root,
  );
  return dispose;
}

export function shareTransport(
  hubUrl: string,
  passphrase: string,
  debug?: BlitDebug,
): BlitTransport {
  // Re-exported so an embedder needs one import, not a tour of core.
  return createShareTransport(hubUrl, passphrase, debug);
}
