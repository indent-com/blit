/**
 * SystemdOverlay — the live systemd unit table of one connection.
 *
 * The state comes from the `blit.systemd.v1` native channel served by the
 * systemd watcher extension (`extensions/systemd`), not
 * from a server packet family: a connection whose server runs no watcher
 * shows the empty state rather than a broken panel.
 *
 * Filtering is local. The channel can filter server-side too, but a viewer
 * typing in a search box wants the previous rows back when it deletes a
 * character, and the whole table is only ~1500 rows.
 */

import {
  createEffect,
  createMemo,
  createSignal,
  For,
  onCleanup,
  Show,
} from "solid-js";
import type {
  BlitWorkspace,
  ConnectionId,
  TerminalPalette,
} from "@blit-sh/core";
import {
  filterUnits,
  openSystemdUnits,
  unitStates,
  SYSTEMD_UNIT_TYPES,
  type SystemdUnitsHandle,
} from "./systemd";
import { SystemdLogs } from "./SystemdLogs";
import { mergeStyle, scrollbarStyle, themeFor, ui, uiScale } from "./theme";
import { t, tp } from "./i18n";

type Row = {
  scope: string;
  name: string;
  load: string;
  active: string;
  sub: string;
  description: string;
};

export function SystemdPanel(props: {
  workspace: BlitWorkspace;
  connectionId: ConnectionId;
  palette: TerminalPalette;
  fontSize: number;
}) {
  const theme = () => themeFor(props.palette);
  const scale = () => uiScale(props.fontSize);

  const [handle, setHandle] = createSignal<SystemdUnitsHandle | null>(null);
  const [error, setError] = createSignal<string | null>(null);
  const [revision, setRevision] = createSignal(0);
  const [query, setQuery] = createSignal("");
  const [unitScope, setUnitScope] = createSignal("");
  const [unitState, setUnitState] = createSignal("");
  const [unitType, setUnitType] = createSignal("");
  const [tab, setTab] = createSignal<"units" | "logs">("units");
  /// Set when a unit row opens the journal, so the filter arrives prefilled.
  const [logUnit, setLogUnit] = createSignal("");
  const [logScope, setLogScope] = createSignal("system");

  // One channel per open. The watcher publishes the whole table on connect,
  // so a re-open is a resync and needs no other recovery path.
  createEffect(() => {
    const connectionId = props.connectionId;
    let live = true;
    let opened: SystemdUnitsHandle | null = null;
    let unsubscribe: (() => void) | undefined;
    setHandle(null);
    setError(null);
    const connection = props.workspace.getConnection(connectionId);
    if (!connection) {
      setError(t("systemd.unavailable"));
      return;
    }
    void openSystemdUnits(connection, {
      onClosed: () => {
        if (live) setError(t("systemd.closed"));
      },
    })
      .then((next: SystemdUnitsHandle) => {
        if (!live) {
          next.close();
          return;
        }
        opened = next;
        setHandle(next);
        // Not `onCleanup` here: a `then` runs with no reactive owner, so one
        // registered inside it is never called — Solid says as much on the
        // console — and every panel that opened a watcher leaked its
        // subscription for the life of the page.
        unsubscribe = next.subscribe(() => setRevision((n) => n + 1));
      })
      .catch(() => {
        if (live) setError(t("systemd.unavailable"));
      });
    onCleanup(() => {
      live = false;
      unsubscribe?.();
      opened?.close();
    });
  });

  const rows = createMemo<Row[]>(() => {
    revision();
    const current = handle();
    if (!current) return [];
    return filterUnits(current.scopes, {
      scope: unitScope(),
      state: unitState(),
      type: unitType(),
      search: query(),
    });
  });

  /** Only the states this server actually has, so the list cannot lie. */
  const states = createMemo(() => {
    revision();
    const current = handle();
    return current ? unitStates(current.scopes) : [];
  });

  // The unit count is right below, and the source only matters when it is the
  // slow one: say so then, and say nothing when signals are driving.
  const summary = createMemo(() => {
    revision();
    const current = handle();
    if (!current) return "";
    // Only `poll` counts: a scope is `unknown` until its first line lands, and
    // announcing that as polling would flash the banner on every open.
    const polling = [...current.scopes.values()].some(
      (scope) => scope.source === "poll",
    );
    return polling ? t("systemd.polling") : "";
  });

  // `failed` is the one state a viewer scans for, so it gets the error colour;
  // anything mid-transition is dimmed rather than coloured.
  const activeColor = (active: string): string => {
    if (active === "failed") return theme().error;
    if (active === "active") return theme().success;
    return theme().dimFg;
  };

  return (
    <>
      {/* Units and logs are two views of the same channel, so the switch sits
          inside the panel rather than beside it. */}
      <div
        style={{
          display: "flex",
          gap: `${scale().xs}px`,
          "margin-bottom": `${scale().sm}px`,
        }}
      >
        <For each={["units", "logs"] as const}>
          {(name) => (
            <button
              type="button"
              data-systemd-tab={name}
              onClick={() => setTab(name)}
              style={mergeStyle(ui.btn, {
                "font-size": `${scale().sm}px`,
                opacity: tab() === name ? 1 : 0.55,
              })}
            >
              {name === "units" ? t("systemd.tabUnits") : t("systemd.tabLogs")}
            </button>
          )}
        </For>
      </div>
      <Show
        when={tab() === "units"}
        fallback={
          <SystemdLogs
            handle={handle()}
            palette={props.palette}
            fontSize={props.fontSize}
            initialUnit={logUnit()}
            initialScope={logScope()}
          />
        }
      >
        <div
          style={{
            display: "flex",
            gap: `${scale().sm}px`,
            "align-items": "center",
            "margin-bottom": `${scale().sm}px`,
          }}
        >
          <select
            data-unit-scope
            value={unitScope()}
            onChange={(event) => setUnitScope(event.currentTarget.value)}
            style={mergeStyle(ui.input, {
              "font-size": `${scale().sm}px`,
            })}
          >
            <option value="">{t("systemd.scopeBoth")}</option>
            <option value="system">{t("systemd.scopeSystem")}</option>
            <option value="user">{t("systemd.scopeUser")}</option>
          </select>
          <select
            data-unit-state
            value={unitState()}
            onChange={(event) => setUnitState(event.currentTarget.value)}
            style={mergeStyle(ui.input, {
              "font-size": `${scale().sm}px`,
            })}
          >
            <option value="">{t("systemd.stateAny")}</option>
            <For each={states()}>
              {(state) => <option value={state}>{state}</option>}
            </For>
          </select>
          <select
            data-unit-type
            value={unitType()}
            onChange={(event) => setUnitType(event.currentTarget.value)}
            style={mergeStyle(ui.input, {
              "font-size": `${scale().sm}px`,
            })}
          >
            <option value="">{t("systemd.typeAny")}</option>
            <For each={SYSTEMD_UNIT_TYPES}>
              {(type) => <option value={type}>{type}</option>}
            </For>
          </select>
          <input
            value={query()}
            placeholder={t("systemd.filter")}
            onInput={(event) => setQuery(event.currentTarget.value)}
            style={mergeStyle(ui.input, {
              flex: "1 1 auto",
              "font-size": `${scale().md}px`,
            })}
          />
          <button
            type="button"
            style={mergeStyle(ui.btn, { "font-size": `${scale().sm}px` })}
            onClick={() => handle()?.resync()}
          >
            {t("systemd.resync")}
          </button>
        </div>

        <Show
          when={!error()}
          fallback={
            <div
              style={{ color: theme().dimFg, "font-size": `${scale().sm}px` }}
            >
              {error()}
            </div>
          }
        >
          <Show when={summary()}>
            <div
              style={{
                color: theme().dimFg,
                "font-size": `${scale().sm}px`,
                "margin-bottom": `${scale().xs}px`,
              }}
            >
              {summary()}
            </div>
          </Show>
          <div
            style={mergeStyle(scrollbarStyle(theme()), {
              "overflow-y": "auto",
              "max-height": "60vh",
              "font-family": "inherit",
              "font-size": `${scale().sm}px`,
            })}
          >
            <For
              each={rows()}
              fallback={
                <div
                  style={{
                    color: theme().dimFg,
                    padding: `${scale().sm}px 0`,
                  }}
                >
                  {handle() ? t("systemd.noMatches") : t("systemd.loading")}
                </div>
              }
            >
              {(row) => (
                // A row is the question "what has this unit been doing?", so
                // clicking it opens the journal already filtered to that unit.
                <div
                  role="button"
                  tabindex={0}
                  title={tp("systemd.openLogs", { unit: row.name })}
                  onClick={() => {
                    setLogUnit(row.name);
                    setLogScope(row.scope);
                    setTab("logs");
                  }}
                  onKeyDown={(event) => {
                    if (event.key !== "Enter" && event.key !== " ") return;
                    event.preventDefault();
                    setLogUnit(row.name);
                    setLogScope(row.scope);
                    setTab("logs");
                  }}
                  style={{
                    display: "grid",
                    "grid-template-columns":
                      "4.5em minmax(0, 2fr) 6em 7em minmax(0, 3fr)",
                    gap: `${scale().sm}px`,
                    padding: `${scale().xs}px 0`,
                    "border-bottom": `1px solid ${theme().border}`,
                    cursor: "pointer",
                  }}
                >
                  <span style={{ color: theme().dimFg }}>{row.scope}</span>
                  <span
                    style={{
                      overflow: "hidden",
                      "text-overflow": "ellipsis",
                      "white-space": "nowrap",
                    }}
                    title={row.name}
                  >
                    {row.name}
                  </span>
                  <span style={{ color: activeColor(row.active) }}>
                    {row.active}
                  </span>
                  <span style={{ color: theme().dimFg }}>{row.sub}</span>
                  <span
                    style={{
                      color: theme().dimFg,
                      overflow: "hidden",
                      "text-overflow": "ellipsis",
                      "white-space": "nowrap",
                    }}
                    title={row.description}
                  >
                    {row.description}
                  </span>
                </div>
              )}
            </For>
          </div>
          <div
            style={{
              color: theme().dimFg,
              "font-size": `${scale().sm}px`,
              "margin-top": `${scale().xs}px`,
            }}
          >
            {tp("systemd.rowCount", { count: String(rows().length) })}
          </div>
        </Show>
      </Show>
    </>
  );
}
