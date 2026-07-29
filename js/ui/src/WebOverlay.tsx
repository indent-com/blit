/**
 * Open a web pane, in two autocompleted steps (docs/design/net.md).
 *
 * First the server: type to filter the remotes, Enter to pick. Then the
 * location: type to filter what that server remembers, Enter to open.
 * Escape backs out of the second step rather than closing, so a wrong
 * server costs one keystroke instead of a reopen.
 *
 * A single remote skips step one — there is nothing to choose — and
 * `remote>url` still works in either step for anyone who knows what they
 * want, which is also what a pasted entry from the terminal command line
 * looks like.
 */

import { For, Show, createSignal, onMount, type JSX } from "solid-js";
import {
  normalizeLocation,
  sortLocations,
  withoutLocation,
  type WebLocation,
} from "./preview";

export interface WebOverlayProps {
  /** Locations this server remembers, newest first once sorted. */
  locations: readonly WebLocation[];
  /** Servers the URL could resolve against. */
  remotes: readonly { id: string; label: string }[];
  /** Currently selected server. */
  dest: string;
  /** Select a different server; its remembered locations replace the list. */
  onDest: (id: string) => void;
  palette: {
    bg: string;
    fg: string;
    accent: string;
    dim: string;
    /** Fill behind the highlighted row. */
    selectedBg: string;
    /** Resting row border, so the box does not appear only on selection. */
    subtleBorder: string;
  };
  fontSize: number;
  /** Non-null when previews cannot work here at all. */
  unavailable: string | null;
  onOpen: (url: string, dest: string) => void;
  onForget: (locations: WebLocation[]) => void;
  onClose: () => void;
}

export function WebOverlay(props: WebOverlayProps): JSX.Element {
  const [draft, setDraft] = createSignal("");
  const [selected, setSelected] = createSignal(0);
  // Derived, not captured at mount: remotes arrive over the config socket,
  // so reading the count once could skip the server step forever on an
  // overlay opened early. The override holds an explicit choice; without
  // one, a lone server means there is nothing to pick.
  const [stageOverride, setStageOverride] = createSignal<
    "remote" | "url" | null
  >(null);
  const stage = (): "remote" | "url" =>
    stageOverride() ?? (props.remotes.length > 1 ? "remote" : "url");
  let input: HTMLInputElement | undefined;

  onMount(() => input?.focus());

  /** Filtered remotes for step one, matching id or label. */
  const remoteMatches = () => {
    const q = draft().trim().toLowerCase();
    if (!q) return props.remotes;
    return props.remotes.filter(
      (r) =>
        r.id.toLowerCase().includes(q) || r.label.toLowerCase().includes(q),
    );
  };

  /** Commit a server and move on to the location. */
  const pickRemote = (id: string) => {
    props.onDest(id);
    setDraft("");
    setSelected(0);
    setStageOverride("url");
    input?.focus();
  };

  /** Back to the server list, keeping the overlay open. */
  const backToRemote = () => {
    if (props.remotes.length < 2) {
      props.onClose();
      return;
    }
    setDraft("");
    setSelected(0);
    setStageOverride("remote");
    input?.focus();
  };

  const listed = () => sortLocations(props.locations);
  // Typing filters; an unmatched entry is still openable, which is how a new
  // location gets added without a separate "add" step.
  const matches = () => {
    const q = draft().trim().toLowerCase();
    if (!q) return listed();
    return listed().filter(
      (l) =>
        l.url.toLowerCase().includes(q) ||
        (l.title ?? "").toLowerCase().includes(q),
    );
  };

  const label = (id: string) =>
    props.remotes.find((r) => r.id === id)?.label ?? id;

  /** `remote>url` picks the server inline, the same shape as the terminal
   *  command entry. Anything before ">" that names a remote wins over the
   *  selection; anything else is part of the URL. */
  const split = (entry: string): { dest: string; url: string } => {
    const arrow = entry.indexOf(">");
    if (arrow > 0) {
      const name = entry.slice(0, arrow).trim();
      const match = props.remotes.find(
        (r) => r.id === name || r.label === name,
      );
      if (match) return { dest: match.id, url: entry.slice(arrow + 1).trim() };
    }
    return { dest: props.dest, url: entry.trim() };
  };

  const open = (entry: string) => {
    const { dest, url } = split(entry);
    if (!url) return;
    props.onOpen(normalizeLocation(url), dest);
    props.onClose();
  };

  /** Cycle servers with Tab — the list follows, so picking a remote shows what
   *  that remote remembers. */
  const cycleDest = (delta: number) => {
    if (props.remotes.length < 2) return;
    const at = props.remotes.findIndex((r) => r.id === props.dest);
    const next = (at + delta + props.remotes.length) % props.remotes.length;
    props.onDest(props.remotes[next].id);
    setSelected(0);
  };

  const onKeyDown = (e: KeyboardEvent) => {
    e.stopPropagation();

    if (stage() === "remote") {
      const list = remoteMatches();
      if (e.key === "Escape") {
        e.preventDefault();
        props.onClose();
      } else if (e.key === "ArrowDown" || e.key === "Tab") {
        e.preventDefault();
        setSelected((i) => Math.min(i + 1, Math.max(list.length - 1, 0)));
      } else if (e.key === "ArrowUp") {
        e.preventDefault();
        setSelected((i) => Math.max(i - 1, 0));
      } else if (e.key === "Enter") {
        e.preventDefault();
        // `remote>url` typed at step one jumps both steps at once.
        const entry = draft();
        if (entry.includes(">")) {
          const { dest, url } = split(entry);
          if (url) {
            props.onDest(dest);
            open(entry);
            return;
          }
        }
        const pick = list[selected()] ?? list[0];
        if (pick) pickRemote(pick.id);
      }
      return;
    }

    const list = matches();
    if (e.key === "Tab") {
      e.preventDefault();
      cycleDest(e.shiftKey ? -1 : 1);
      return;
    }
    if (e.key === "Escape") {
      e.preventDefault();
      // Back a step rather than out: a wrong server should not cost a reopen.
      backToRemote();
    } else if (
      e.key === "Backspace" &&
      draft() === "" &&
      props.remotes.length > 1
    ) {
      // Deleting past the start of an empty location is the same gesture.
      e.preventDefault();
      backToRemote();
    } else if (e.key === "ArrowDown") {
      e.preventDefault();
      setSelected((i) => Math.min(i + 1, list.length));
    } else if (e.key === "ArrowUp") {
      e.preventDefault();
      setSelected((i) => Math.max(i - 1, 0));
    } else if (e.key === "Enter") {
      e.preventDefault();
      const pick = list[selected()];
      open(pick && !draft().trim() ? pick.url : draft() || pick?.url || "");
    }
  };

  /**
   * A row, selected or not, styled as the Cmd+K switcher styles its own:
   * the box is always drawn and its border turns accent when active, over
   * a subtle fill. The previous inverted block (accent background,
   * background-coloured text) read as a different control in the same
   * product, and it hid the row's own text colour.
   */
  const row = (
    label: string,
    hint: string,
    active: boolean,
  ): JSX.CSSProperties => ({
    display: "flex",
    "justify-content": "space-between",
    gap: "1em",
    padding: "0.3em 0.6em",
    cursor: "pointer",
    border: `1px solid ${
      active ? props.palette.accent : props.palette.subtleBorder
    }`,
    background: active ? props.palette.selectedBg : "transparent",
    color: props.palette.fg,
    "font-size": `${props.fontSize}px`,
    // Same easing as the switcher, so moving through a list feels alike.
    transition: "border-color 120ms ease, background-color 120ms ease",
  });

  return (
    <div
      onKeyDown={onKeyDown}
      style={{
        position: "absolute",
        inset: "0",
        display: "flex",
        "align-items": "flex-start",
        "justify-content": "center",
        "padding-top": "12vh",
        background: "rgba(0,0,0,0.5)",
        "z-index": 40,
      }}
      onClick={(e) => {
        if (e.target === e.currentTarget) props.onClose();
      }}
    >
      <div
        style={{
          width: "min(46em, 90vw)",
          background: props.palette.bg,
          color: props.palette.fg,
          border: `1px solid ${props.palette.dim}`,
          font: `${props.fontSize}px ui-monospace, monospace`,
          "max-height": "70vh",
          display: "flex",
          "flex-direction": "column",
        }}
      >
        <div
          style={{
            padding: "0.5em 0.6em",
            "border-bottom": `1px solid ${props.palette.dim}`,
            display: "flex",
            "align-items": "center",
            gap: "0.5em",
          }}
        >
          <Show
            when={stage() === "remote"}
            fallback={
              <span
                title={
                  props.remotes.length > 1
                    ? "Esc to pick a different server"
                    : undefined
                }
                onClick={backToRemote}
                style={{
                  opacity: 0.6,
                  cursor: props.remotes.length > 1 ? "pointer" : "default",
                  "white-space": "nowrap",
                }}
              >
                web on {label(props.dest)}
                {props.remotes.length > 1 ? " ⌫" : ""}
              </span>
            }
          >
            <span style={{ opacity: 0.6, "white-space": "nowrap" }}>
              web on
            </span>
          </Show>
          <input
            ref={input}
            value={draft()}
            placeholder={
              stage() === "remote" ? "which server…" : "localhost:3000"
            }
            onInput={(e) => {
              setDraft(e.currentTarget.value);
              setSelected(0);
            }}
            style={{
              flex: 1,
              font: "inherit",
              background: "transparent",
              border: "none",
              color: "inherit",
              outline: "none",
            }}
          />
        </div>
        <Show when={props.unavailable}>
          <div style={{ padding: "0.5em 0.6em", color: "#e88" }}>
            {props.unavailable}
          </div>
        </Show>
        <div
          style={{
            overflow: "auto",
            flex: 1,
            // Gapped column, as the switcher lays its cards out: now that
            // every row draws its own border, adjacent rows would otherwise
            // meet as a 2px line.
            display: "flex",
            "flex-direction": "column",
            gap: "2px",
            padding: "2px",
          }}
        >
          {/* Step one: the servers, filtered as you type. */}
          <Show when={stage() === "remote"}>
            <For each={remoteMatches()}>
              {(r, index) => (
                <div
                  style={row(r.label, "", index() === selected())}
                  onMouseEnter={() => setSelected(index())}
                  onClick={() => pickRemote(r.id)}
                >
                  <span>{r.label}</span>
                  {/* The id is what `remote>url` expects, so show it when it
                      differs from the label. */}
                  <Show when={r.id !== r.label}>
                    <span style={{ opacity: 0.5 }}>{r.id}</span>
                  </Show>
                </div>
              )}
            </For>
            <Show when={remoteMatches().length === 0}>
              <div style={{ padding: "0.6em", opacity: 0.6 }}>
                No server matches that.
              </div>
            </Show>
          </Show>
          <Show when={stage() === "url"}>
            <For each={matches()}>
              {(entry, index) => (
                <div
                  style={row(entry.url, "", index() === selected())}
                  onMouseEnter={() => setSelected(index())}
                  onClick={() => open(entry.url)}
                >
                  <span>
                    {entry.url}
                    <Show when={entry.title}>
                      <span style={{ opacity: 0.5 }}> — {entry.title}</span>
                    </Show>
                  </span>
                  <button
                    title="Forget this location"
                    onClick={(e) => {
                      e.stopPropagation();
                      props.onForget(
                        withoutLocation(props.locations, entry.url),
                      );
                    }}
                    style={{
                      background: "transparent",
                      border: "none",
                      color: "inherit",
                      opacity: 0.5,
                      cursor: "pointer",
                    }}
                  >
                    ✕
                  </button>
                </div>
              )}
            </For>
            <Show
              when={
                draft().trim() &&
                !matches().some((m) => m.url === normalizeLocation(draft()))
              }
            >
              <div
                style={row("", "", selected() >= matches().length)}
                onMouseEnter={() => setSelected(matches().length)}
                onClick={() => open(draft())}
              >
                <span>
                  open <strong>{normalizeLocation(draft())}</strong>
                </span>
                <span style={{ opacity: 0.5 }}>Enter</span>
              </div>
            </Show>
            <Show when={!draft().trim() && props.locations.length === 0}>
              <div style={{ padding: "0.6em", opacity: 0.6 }}>
                Nothing remembered yet — type a host and port.
              </div>
            </Show>
          </Show>
        </div>
      </div>
    </div>
  );
}
