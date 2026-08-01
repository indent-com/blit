/** Navigation for the focused web pane, for the status bar (docs/design/net.md § Clean paths inside an iframe). */

import { Show, createSignal, type JSX } from "solid-js";
import type { WebPaneHandle } from "./WebPane";
import { normalizeLocation, splitLocation, webLocationLabel } from "./preview";

export interface WebPaneNavProps {
  /** The focused pane's handle, or null when the focus is not a web pane. */
  handle: WebPaneHandle | null;
  /** The pane's target origin, e.g. `https://localhost:3000`. */
  url: string;
  /**
   * Point the pane at a different origin. Absent leaves the location bar
   * able to move within the current target only.
   */
  onRetarget?: (url: string) => void;
  /** Chrome font size, matching the rest of the status bar. */
  fontSize?: number;
}

export function WebPaneNav(props: WebPaneNavProps): JSX.Element {
  const [editing, setEditing] = createSignal(false);
  const [draft, setDraft] = createSignal("");

  const size = () => props.fontSize ?? 11;
  const state = () => props.handle?.state() ?? null;

  const button = (
    label: string,
    title: string,
    enabled: boolean,
    action: () => void,
  ): JSX.Element => (
    <button
      title={title}
      disabled={!enabled}
      onClick={action}
      style={{
        background: "transparent",
        border: "none",
        color: "inherit",
        cursor: enabled ? "pointer" : "default",
        opacity: enabled ? 0.85 : 0.3,
        padding: "0 0.3em",
        "font-size": `${size()}px`,
        "line-height": "1",
      }}
    >
      {label}
    </button>
  );

  /** The full location, which is what the field edits. The plain-iframe
   *  marker stays out of it: it is not typeable, and origin comparison in
   *  `commit` runs on the URL either way. */
  const location = () =>
    `${webLocationLabel(props.url)}${state()?.path ?? "/"}`;

  const commit = () => {
    const text = draft().trim();
    setEditing(false);
    if (!text || text === location()) return;
    const origin = normalizeLocation(webLocationLabel(props.url));
    const split = splitLocation(text, origin);
    if (!split) return;
    // A different origin is a different relayed target, which the pane
    // cannot reach by navigating — the workspace has to re-point it.
    if (split.origin !== origin) {
      props.onRetarget?.(
        split.path === "/" ? split.origin : `${split.origin}${split.path}`,
      );
      return;
    }
    props.handle?.go(split.path);
  };

  return (
    <Show when={props.handle}>
      <span
        style={{
          display: "inline-flex",
          "align-items": "center",
          gap: "0.55em",
          "font-size": `${size()}px`,
          "min-width": "0",
        }}
      >
        <span
          style={{
            display: "inline-flex",
            "align-items": "center",
            gap: "0.15em",
          }}
        >
          {button("◀", "Back", state()?.canGoBack ?? false, () =>
            props.handle?.back(),
          )}
          {button("▶", "Forward", state()?.canGoForward ?? false, () =>
            props.handle?.forward(),
          )}
          {button("⟳", "Reload", true, () => props.handle?.reload())}
        </span>
        <Show
          when={editing()}
          fallback={
            <span
              title={`${location()} — click to edit`}
              onClick={() => {
                // The whole location, not just the path: retyping the host is
                // how you point a pane somewhere else, and a field that
                // showed only `/foo` made the origin look unchangeable.
                setDraft(location());
                setEditing(true);
              }}
              style={{
                cursor: "text",
                opacity: state()?.loading ? 0.5 : 0.85,
                "white-space": "nowrap",
                overflow: "hidden",
                "text-overflow": "ellipsis",
                "max-width": "28em",
              }}
            >
              <span style={{ opacity: 0.55 }}>
                {webLocationLabel(props.url)}
              </span>
              {state()?.path ?? "/"}
            </span>
          }
        >
          <input
            autofocus
            value={draft()}
            // Selected on focus: the common edit replaces the whole
            // location rather than appending to it.
            ref={(el) => queueMicrotask(() => el.select())}
            onInput={(e) => setDraft(e.currentTarget.value)}
            onBlur={commit}
            onKeyDown={(e) => {
              if (e.key === "Enter") {
                e.preventDefault();
                commit();
              } else if (e.key === "Escape") {
                e.preventDefault();
                setEditing(false);
              }
              // Everything else is the input's business, not the workspace's: a keystroke here must not reach the terminal keymap.
              e.stopPropagation();
            }}
            style={{
              font: "inherit",
              "font-size": `${size()}px`,
              background: "rgba(255,255,255,0.08)",
              border: "none",
              color: "inherit",
              padding: "0 0.3em",
              width: "28em",
            }}
          />
        </Show>
        <Show when={state()?.title}>
          <span
            title={state()?.title}
            style={{
              opacity: 0.6,
              "white-space": "nowrap",
              overflow: "hidden",
              "text-overflow": "ellipsis",
              "max-width": "16em",
            }}
          >
            {state()?.title}
          </span>
        </Show>
      </span>
    </Show>
  );
}
