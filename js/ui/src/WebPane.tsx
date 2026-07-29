/** A web pane: an iframe onto something the server can reach, served through the preview service worker (docs/design/net.md). */

import { createEffect, createSignal, onCleanup, type JSX } from "solid-js";
import { previewIframeUrl } from "./preview";
import { forwardWebPaneCloseShortcut } from "./webPaneShortcuts";

export interface WebPaneState {
  /** Where the frame currently is, on the target's own terms (`/dashboard`). */
  path: string;
  /** The page's `<title>`, when it has one and we can read it. */
  title: string;
  loading: boolean;
  /** Set when the frame could not be pointed at the target at all. */
  error: string | null;
  canGoBack: boolean;
  canGoForward: boolean;
}

/** Handle the status bar drives. */
export interface WebPaneHandle {
  state: () => WebPaneState;
  back: () => void;
  forward: () => void;
  reload: () => void;
  /** Go to a path on the same target. */
  go: (path: string) => void;
}

export interface WebPaneProps {
  /** Gateway destination — which server's network the URL is on. */
  dest: string;
  /** Target origin as remembered, e.g. `https://localhost:3000`. */
  url: string;
  /** Path within the target; the pane starts here. */
  path?: string;
  focus?: boolean;
  style?: JSX.CSSProperties;
  /** Receives the handle once the frame exists, for the status bar. */
  onHandle?: (handle: WebPaneHandle) => void;
  /** Fires when the frame's location or title changes, so the workspace can update the pane's label and remember where it got to. */
  onNavigate?: (state: WebPaneState) => void;
  /** Fires when the user interacts inside the frame; the pane should take focus. */
  onFocusRequest?: () => void;
}

export function WebPane(props: WebPaneProps): JSX.Element {
  let frame: HTMLIFrameElement | undefined;
  const [state, setState] = createSignal<WebPaneState>({
    path: props.path ?? "/",
    title: "",
    loading: true,
    error: null,
    canGoBack: false,
    canGoForward: false,
  });

  const src = () => {
    try {
      return previewIframeUrl(props.dest, props.url, props.path ?? "/");
    } catch (err) {
      // A URL that cannot be parsed is a configuration mistake, not a load failure: say so in the pane rather than pointing the frame at nothing.
      setState((s) => ({
        ...s,
        loading: false,
        error: err instanceof Error ? err.message : String(err),
      }));
      return "about:blank";
    }
  };

  /** Read what we can from the frame. */
  const sample = () => {
    if (!frame) return;
    let path = state().path;
    let title = state().title;
    let canGoBack = false;
    let canGoForward = false;
    try {
      const win = frame.contentWindow;
      if (win) {
        // Inside the frame the app's own paths are clean, so its location is already the path on the target — except right after bootstrap, when it still carries the /x/ prefix.
        const raw = win.location.pathname + win.location.search;
        path = stripBootstrap(raw);
        title = frame.contentDocument?.title ?? title;
        // `history.length` is the only cross-browser signal available; there is no API for "can go back", so this is a lower bound: after one in-frame navigation there is somewhere to go back to.
        canGoBack = win.history.length > 1;
        canGoForward = false;
      }
    } catch {
      // A frame that has navigated somewhere unreadable (it should not, being same-origin) leaves the last known state in place.
    }
    const next = {
      ...state(),
      path,
      title,
      loading: false,
      canGoBack,
      canGoForward,
    };
    setState(next);
    props.onNavigate?.(next);
  };

  createEffect(() => {
    // Re-point the frame when the target changes, and re-publish the handle: a new URL is a new pane as far as the status bar is concerned.
    const url = src();
    if (frame && frame.src !== new URL(url, location.href).href) {
      setState((s) => ({ ...s, loading: true }));
      frame.src = url;
    }
  });

  createEffect(() => {
    if (!frame) return;
    const handle: WebPaneHandle = {
      state,
      back: () => {
        try {
          frame?.contentWindow?.history.back();
        } catch {
          // Nothing to do; the strip's buttons are advisory.
        }
      },
      forward: () => {
        try {
          frame?.contentWindow?.history.forward();
        } catch {
          // As above.
        }
      },
      reload: () => {
        if (!frame) return;
        setState((s) => ({ ...s, loading: true }));
        // Not the frame's own `location.reload()`: that re-requests whatever
        // URL it is currently on, and once the app has navigated to a clean
        // path that request arrives as a new client with no binding and no
        // referrer to adopt from — landing on the lost-target document. Point
        // it at a URL that names the target and the current path, so a reload
        // always resolves. `about:blank` first because assigning an identical
        // src is not a navigation.
        const path = state().path || "/";
        let next: string;
        try {
          next = previewIframeUrl(props.dest, props.url, path);
        } catch {
          next = src();
        }
        frame.src = "about:blank";
        const target = frame;
        requestAnimationFrame(() => {
          target.src = next;
        });
      },
      go: (path: string) => {
        if (!frame) return;
        setState((s) => ({ ...s, loading: true }));
        try {
          frame.contentWindow!.location.assign(path);
        } catch {
          frame.src = previewIframeUrl(props.dest, props.url, path);
        }
      },
    };
    props.onHandle?.(handle);
  });

  createEffect(() => {
    if (!frame) return;
    // Poll for in-app navigation: pushState fires no event the parent can see, and `hashchange`/`load` miss it.
    const timer = setInterval(sample, 1000);
    onCleanup(() => clearInterval(timer));
  });

  /** The document the listeners below are on, so a reload can take them off
   *  the previous one. */
  let bound: {
    doc: Document;
    claim: () => void;
    forwardWorkspaceShortcut: (event: KeyboardEvent) => void;
  } | null = null;

  const detachFrameListeners = () => {
    if (!bound) return;
    bound.doc.removeEventListener("pointerdown", bound.claim, true);
    bound.doc.removeEventListener("focusin", bound.claim, true);
    bound.doc.removeEventListener(
      "keydown",
      bound.forwardWorkspaceShortcut,
      true,
    );
    bound = null;
  };
  // The component's own scope, which is a real owner — so this actually runs.
  onCleanup(detachFrameListeners);

  /** Listen inside the frame — same-origin, so this works — for the two things
   *  the parent cannot otherwise learn: that the user is interacting with this
   *  pane, and that the worker lost the frame's target.
   *
   *  Called from the iframe's `load` event, which is outside any reactive
   *  owner: an `onCleanup` here is never run (Solid says so), so every reload
    *  of the previewed page left listeners on a document nobody
   *  would ever detach them from. Removal is explicit instead — the previous
   *  document's on each load, and the last one's on unmount. */
  const attachFrameListeners = () => {
    detachFrameListeners();
    const doc = frame?.contentDocument;
    if (!doc) return;
    const claim = () => props.onFocusRequest?.();
    // Keyboard events stop at an iframe document; they do not bubble into the
    // workspace window where createKeyboardShortcuts listens. Relay the
    // destructive close chord so a focused browser pane behaves like every
    // other focused tile. Keep this deliberately narrow: previewed apps retain
    // all of their ordinary keyboard input and shortcuts.
    const forwardWorkspaceShortcut = (event: KeyboardEvent) =>
      forwardWebPaneCloseShortcut(event, claim);
    doc.addEventListener("pointerdown", claim, true);
    doc.addEventListener("focusin", claim, true);
    doc.addEventListener("keydown", forwardWorkspaceShortcut, true);
    bound = { doc, claim, forwardWorkspaceShortcut };
    if (doc.body?.dataset.blitPreviewLost === "1") {
      // A binding was lost (worker restart, or a navigation we could not
      // attribute). Re-point once rather than leaving a dead pane.
      setState((s) => ({ ...s, loading: true }));
      if (frame) frame.src = src();
    }
  };

  return (
    <div
      style={{
        width: "100%",
        height: "100%",
        position: "relative",
        ...props.style,
      }}
    >
      <iframe
        ref={(el) => (frame = el)}
        // `window.top` is [LegacyUnforgeable] — it cannot be reassigned or
        // redefined, so a frame-busting app that checks it cannot be shimmed
        // from inside. Sandboxing without `allow-top-navigation` denies the
        // navigation instead, and keeping `allow-same-origin` is what lets the
        // service worker keep controlling this frame (an opaque origin would
        // not be controlled at all, and the preview would stop working).
        sandbox="allow-scripts allow-same-origin allow-forms allow-modals allow-popups allow-popups-to-escape-sandbox allow-downloads"
        src={src()}
        onLoad={() => {
          sample();
          attachFrameListeners();
        }}
        title={state().title || props.url}
        style={{
          width: "100%",
          height: "100%",
          border: "none",
          display: "block",
          background: "#fff",
        }}
      />
      {state().error ? (
        <div
          style={{
            position: "absolute",
            inset: "0",
            display: "flex",
            "align-items": "center",
            "justify-content": "center",
            padding: "1rem",
            "text-align": "center",
            font: "13px/1.5 ui-monospace, monospace",
            color: "#e66",
            background: "#1a1a1a",
          }}
        >
          {state().error}
        </div>
      ) : null}
    </div>
  );
}

/** Strip preview bookkeeping from a frame's location, leaving the target's own
 *  path — what the status bar should show and what the app itself sees. */
export function stripBootstrap(pathAndQuery: string): string {
  const [rawPath, rawQuery] = splitQuery(pathAndQuery);
  // The prefix form, still accepted for a directly-typed URL.
  const prefixed = /^\/x\/[^/]+\/(?:http|https)\/[^/]+(\/.*)?$/.exec(rawPath);
  const path = prefixed ? prefixed[1] || "/" : rawPath;
  if (!rawQuery) return path;
  const params = new URLSearchParams(rawQuery);
  const wanted = params.get("blit-path");
  params.delete("blit-preview");
  params.delete("blit-path");
  const rest = params.toString();
  // A frame still on its opening URL reports the path it was asked for.
  if (wanted) return wanted;
  return rest ? `${path}?${rest}` : path;
}

function splitQuery(value: string): [string, string] {
  const q = value.indexOf("?");
  return q < 0 ? [value, ""] : [value.slice(0, q), value.slice(q + 1)];
}
