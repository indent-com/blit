/**
 * ConnectionSession — the applications ONE connection starts and keeps running:
 * what is enabled, what is actually up, how many windows each has, and controls
 * to run one.
 *
 * Two pairs of verbs, because they answer different questions. Enable/Disable
 * is intent: what this session should be running the next time it starts.
 * Start/Stop is now: try an application without adopting it, or stop one
 * without forgetting it. Collapsing them into one button — which is what this
 * had — makes "stop this for a minute" indistinguishable from "I never want
 * this again".
 *
 * State comes from the `blit.session.v1` native channel served by the session
 * supervisor extension (`extensions/session`), not from a server packet family.
 * A connection whose server runs no supervisor simply shows nothing — the
 * channel connect fails and the section stays out of the way, which is why this
 * renders nothing at all rather than an error when it cannot attach.
 *
 * Like the clients section, the subscription lives here: it opens when the
 * remote's row is expanded and closes when it is collapsed, so a collapsed
 * remote costs no channel traffic.
 */

import { createEffect, createSignal, For, onCleanup, Show } from "solid-js";
import type {
  BlitWorkspace,
  ConnectionId,
  TerminalPalette,
} from "@blit-sh/core";
import { scrollbarStyle, themeFor, ui, uiScale } from "./theme";
import {
  AppIcon,
  PanelEmpty,
  PanelRow,
  panelButton,
  SectionHeading,
  StatusPill,
  type PanelTone,
} from "./panelKit";
import { openSession, type SessionApp, type SessionHandle } from "./session";

/** Phase → the tone and word the row shows. Backoff is a warning rather than
 *  an error: it is a supervisor working, not a supervisor stuck. */
function phaseTone(app: SessionApp): { tone: PanelTone; label: string } {
  if (!app.enabled) return { tone: "idle", label: "disabled" };
  switch (app.phase) {
    case "running":
      return { tone: "ok", label: "running" };
    case "backoff":
      return { tone: "warn", label: "restarting" };
    case "starting":
      return { tone: "warn", label: "starting" };
    case "stopped":
      return { tone: "idle", label: "stopped" };
  }
}

export function ConnectionSession(props: {
  workspace: BlitWorkspace;
  connectionId: ConnectionId;
  palette: TerminalPalette;
  fontSize: number;
}) {
  const theme = () => themeFor(props.palette);
  const scale = () => uiScale(props.fontSize);
  const [handle, setHandle] = createSignal<SessionHandle | null>(null);
  // Bumped from the mirror's subscribe, since the handle's getters are plain
  // properties rather than signals.
  const [revision, setRevision] = createSignal(0);
  const [filter, setFilter] = createSignal("");

  const connection = props.workspace.getConnection(props.connectionId);
  if (connection) {
    let live = true;
    void openSession(connection, {
      onClosed: () => {
        setHandle(null);
      },
    })
      .then((opened) => {
        // The row can collapse while the channel is still opening.
        if (!live) {
          opened.close();
          return;
        }
        setHandle(opened);
        const stop = opened.subscribe(() => setRevision((n) => n + 1));
        onCleanup(stop);
      })
      // No supervisor on this server: the section renders nothing rather than
      // an error, because "this server does not run one" is not a fault.
      .catch(() => setHandle(null));
    onCleanup(() => {
      live = false;
      handle()?.close();
    });
  }

  const apps = () => {
    revision();
    return handle()?.apps ?? [];
  };
  const catalog = () => {
    revision();
    return handle()?.catalog ?? [];
  };
  /** Installed applications that are not already managed, matched against the
   *  filter box. A managed app is offered by its own row, not this list. */
  const addable = () => {
    const managed = new Set(apps().map((app) => app.id));
    const needle = filter().trim().toLowerCase();
    return catalog()
      .filter((entry) => !managed.has(entry.id))
      .filter(
        (entry) =>
          needle.length === 0 ||
          entry.name.toLowerCase().includes(needle) ||
          entry.id.toLowerCase().includes(needle),
      );
  };
  /** Artwork for one row. Reads the revision like every other accessor here:
   *  an icon arrives long after the row that wants it was drawn, and the
   *  handle's getters are plain properties rather than signals, so without
   *  this the reply lands in the mirror and nothing re-renders. */
  const iconOf = (id: string) => {
    revision();
    return handle()?.icon(id);
  };

  // Artwork is asked for, never pushed: the catalog is names, and its icons are
  // three orders of magnitude larger. The managed set is small and always on
  // screen, so it is asked for outright.
  createEffect(() => {
    const session = handle();
    if (!session) return;
    session.requestIcons(apps().map((app) => app.id));
  });

  // The catalog is not. Every installed application is a row, which on a
  // machine with a games library is nine hundred of them — asking for all that
  // artwork would be tens of megabytes to draw a dozen tiles. So a row asks
  // only once it is near the viewport, and the observer's own batching is what
  // turns a scroll into one request rather than one per row.
  //
  // Rooted at the viewport rather than at the list's own scroller, which would
  // read better and cannot be done: a child's `ref` can run before its
  // parent's, so the rows would find no observer and would each ask outright —
  // the storm this exists to prevent. The viewport is the correct root anyway,
  // because an intersection is clipped by every scrolling ancestor on the way
  // up, this list's included.
  const iconWatcher = (() => {
    // Absent under jsdom. Rows fall back to asking outright, so a client
    // without it still shows artwork — it just asks for more of it.
    if (typeof IntersectionObserver === "undefined") return undefined;
    const observer = new IntersectionObserver(
      (entries) => {
        const ids = entries
          .filter((entry) => entry.isIntersecting)
          .map((entry) => {
            // Once asked, a row has nothing more to say: the answer is cached
            // by the handle and by the mirror, either way.
            observer.unobserve(entry.target);
            return (entry.target as HTMLElement).dataset.appId;
          })
          .filter((id): id is string => id !== undefined);
        if (ids.length > 0) handle()?.requestIcons(ids);
      },
      // A screen ahead, so scrolling meets artwork already there rather than a
      // wave of monograms filling in behind it.
      { rootMargin: "300px" },
    );
    onCleanup(() => observer.disconnect());
    return observer;
  })();
  /** Attach one catalog row to the watcher, or ask outright without one. */
  const watchForIcon = (element: HTMLElement, id: string) => {
    element.dataset.appId = id;
    if (!iconWatcher) {
      handle()?.requestIcons([id]);
      return;
    }
    iconWatcher.observe(element);
    onCleanup(() => iconWatcher.unobserve(element));
  };

  return (
    <Show when={handle()}>
      {(session) => (
        <div
          style={{
            display: "flex",
            "flex-direction": "column",
            "background-color": theme().panelBg,
          }}
        >
          <SectionHeading
            theme={theme()}
            scale={scale()}
            label="Applications"
            count={apps().length}
          />

          <Show
            when={apps().length > 0}
            fallback={
              <PanelEmpty theme={theme()} scale={scale()}>
                Nothing is managed yet. Enable an application below and it will
                start with this session.
              </PanelEmpty>
            }
          >
            <For each={apps()}>
              {(app) => (
                <PanelRow theme={theme()} scale={scale()}>
                  <div
                    style={{
                      display: "flex",
                      "align-items": "center",
                      "justify-content": "space-between",
                      gap: `${scale().gap}px`,
                    }}
                  >
                    <span
                      style={{
                        display: "flex",
                        // The icon is the tallest thing in the row, so the text
                        // centres against it rather than sitting on a baseline
                        // the tile does not share.
                        "align-items": "center",
                        gap: `${scale().gap}px`,
                        "min-width": "0",
                      }}
                    >
                      <AppIcon
                        theme={theme()}
                        scale={scale()}
                        name={app.name}
                        src={iconOf(app.id)}
                      />
                      <span
                        style={{
                          display: "flex",
                          "align-items": "baseline",
                          gap: `${scale().tightGap}px`,
                          "min-width": "0",
                        }}
                      >
                        <strong
                          style={{
                            overflow: "hidden",
                            "text-overflow": "ellipsis",
                            "white-space": "nowrap",
                          }}
                        >
                          {app.name}
                        </strong>
                        <Show when={app.name !== app.id}>
                          <span
                            style={{
                              color: theme().dimFg,
                              "font-size": `${scale().sm}px`,
                            }}
                          >
                            {app.id}
                          </span>
                        </Show>
                      </span>
                    </span>

                    <span
                      style={{
                        display: "flex",
                        "align-items": "center",
                        gap: `${scale().gap}px`,
                        "flex-shrink": "0",
                      }}
                    >
                      <StatusPill
                        theme={theme()}
                        scale={scale()}
                        {...phaseTone(app)}
                        title={
                          app.socket
                            ? `Wayland socket ${app.socket}`
                            : undefined
                        }
                      />
                      {/* Counted from the identity the compositor stamped on
                          the app's own socket, not from a self-asserted
                          app_id — which is what makes it worth showing. */}
                      <span
                        title="Windows, counted from the application's stamped Wayland socket"
                        style={{
                          color: theme().dimFg,
                          "font-size": `${scale().sm}px`,
                          "font-variant-numeric": "tabular-nums",
                        }}
                      >
                        {app.windows} {app.windows === 1 ? "window" : "windows"}
                      </span>
                      {/* Now. Running covers backoff too: a supervisor about
                          to retry is something a viewer wants to be able to
                          call off. */}
                      <button
                        type="button"
                        title={
                          app.phase === "stopped"
                            ? "Run it now, without changing what the next session start does"
                            : "Stop it now, without changing what the next session start does"
                        }
                        style={panelButton(theme(), scale())}
                        onClick={() =>
                          app.phase === "stopped"
                            ? session().start(app.id)
                            : session().stop(app.id)
                        }
                      >
                        {app.phase === "stopped" ? "Start" : "Stop"}
                      </button>
                      {/* Intent, and the way out of the list. Disabling keeps
                          the row -- an application that just failed is worth
                          looking at -- so there has to be something that
                          removes it, or a one-off experiment stays forever. */}
                      <button
                        type="button"
                        title={
                          app.enabled
                            ? "Stop it and do not start it with this session again"
                            : "Start it now and with every session"
                        }
                        style={panelButton(
                          theme(),
                          scale(),
                          app.enabled ? "bad" : undefined,
                        )}
                        onClick={() =>
                          app.enabled
                            ? session().disable(app.id)
                            : session().enable(app.id)
                        }
                      >
                        {app.enabled ? "Disable" : "Enable"}
                      </button>
                      <button
                        type="button"
                        title="Stop it and remove it from this list"
                        style={panelButton(theme(), scale(), "bad")}
                        onClick={() => session().forget(app.id)}
                      >
                        Discard
                      </button>
                    </span>
                  </div>

                  {/* Only worth a line when something went wrong: a healthy row
                      stays one line tall. */}
                  <Show when={app.failures > 0 || app.lastExit !== undefined}>
                    <div
                      style={{
                        color: theme().dimFg,
                        "font-size": `${scale().sm}px`,
                        "font-variant-numeric": "tabular-nums",
                      }}
                    >
                      <Show when={app.failures > 0}>
                        {app.failures} failed{" "}
                        {app.failures === 1 ? "start" : "starts"}
                      </Show>
                      <Show
                        when={app.failures > 0 && app.lastExit !== undefined}
                      >
                        {" · "}
                      </Show>
                      <Show when={app.lastExit !== undefined}>
                        last exit {app.lastExit}
                      </Show>
                    </div>
                  </Show>
                </PanelRow>
              )}
            </For>
          </Show>

          {/* Adding. The whole catalog, scrolling, with the filter narrowing it
              rather than summoning it: this list used to be hidden behind
              typing, which asked a viewer to name what they wanted before
              being shown that it existed. A launcher shows its shelf. */}
          <SectionHeading
            theme={theme()}
            scale={scale()}
            label="Add an application"
          >
            <input
              type="text"
              value={filter()}
              onInput={(event) => setFilter(event.currentTarget.value)}
              placeholder="Search installed…"
              aria-label="Search installed applications"
              autocomplete="off"
              spellcheck={false}
              style={{
                ...ui.input,
                "background-color": theme().inputBg,
                color: "inherit",
                border: `1px solid ${theme().border}`,
                "font-size": `${scale().sm}px`,
                padding: `${scale().controlY}px ${scale().controlX}px`,
                "min-width": "0",
                flex: "1 1 12em",
              }}
            />
          </SectionHeading>

          <Show
            when={addable().length > 0}
            fallback={
              <PanelEmpty theme={theme()} scale={scale()}>
                {catalog().length === 0
                  ? "No installed applications found."
                  : `Nothing installed matches “${filter().trim()}”.`}
              </PanelEmpty>
            }
          >
            {/* The catalog gets a scroller of its own rather than riding the
                overlay's: it is the only unbounded thing here, and letting it
                lengthen the panel would scroll the search box — the one
                control for a nine-hundred-row list — off the top. */}
            <div
              style={{
                "max-height": "42vh",
                "overflow-y": "auto",
                "min-width": "0",
                ...scrollbarStyle(theme()),
              }}
            >
              <For each={addable()}>
                {(entry) => (
                  <PanelRow theme={theme()} scale={scale()}>
                    <div
                      ref={(element) => watchForIcon(element, entry.id)}
                      style={{
                        display: "flex",
                        "align-items": "center",
                        "justify-content": "space-between",
                        gap: `${scale().gap}px`,
                      }}
                    >
                      <span
                        style={{
                          display: "flex",
                          "align-items": "center",
                          gap: `${scale().gap}px`,
                          "min-width": "0",
                        }}
                      >
                        <AppIcon
                          theme={theme()}
                          scale={scale()}
                          name={entry.name}
                          src={iconOf(entry.id)}
                        />
                        <span
                          style={{
                            "min-width": "0",
                            overflow: "hidden",
                            "text-overflow": "ellipsis",
                            "white-space": "nowrap",
                          }}
                        >
                          {entry.name}
                          <span
                            style={{
                              color: theme().dimFg,
                              "font-size": `${scale().sm}px`,
                            }}
                          >
                            {` ${entry.id}`}
                          </span>
                        </span>
                      </span>
                      <button
                        type="button"
                        style={panelButton(theme(), scale())}
                        onClick={() => {
                          session().enable(entry.id);
                          setFilter("");
                        }}
                      >
                        Enable
                      </button>
                    </div>
                  </PanelRow>
                )}
              </For>
            </div>
          </Show>
        </div>
      )}
    </Show>
  );
}
