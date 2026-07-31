/**
 * What the surrounding shell can actually offer.
 *
 * The workspace usually runs inside the app served by a blit server, where
 * the page has a gateway (remotes to add and switch), a config socket, and
 * a same-origin service worker for web-pane previews. Embedded — blit.sh
 * opening a share link — none of that exists: the connection list is fixed
 * by the host, and there is no `sw.js` at the page's origin to register.
 * These flags let the one Workspace serve both lives instead of the embed
 * growing a second, lesser client; the affordances they gate are hidden,
 * not broken, because a menu entry that opens an empty panel is a bug
 * report waiting to be filed.
 *
 * Module state rather than a prop: the flags describe the page, not a
 * component instance, and they are set exactly once before mount.
 */

export interface ShellCapabilities {
  /** The page can manage remotes (gateway mux + config socket). */
  remotes: boolean;
  /** The page can register the preview service worker. */
  previews: boolean;
  /** The page origin serves a blit server's HTTP routes — the `fonts`
   *  listing among them. Embedded on a static site there is no such route,
   *  and asking for it only produces a 404 in the console. */
  serverRoutes: boolean;
}

let caps: ShellCapabilities = {
  remotes: true,
  previews: true,
  serverRoutes: true,
};

export function setShellCapabilities(next: Partial<ShellCapabilities>): void {
  caps = { ...caps, ...next };
}

export function shellCapabilities(): ShellCapabilities {
  return caps;
}
