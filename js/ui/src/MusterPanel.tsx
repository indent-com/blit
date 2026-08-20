/**
 * MusterPanel — the units one connection's muster supervisor is running, as a
 * tree: instance ▸ unit ▸ (terminal, windows).
 *
 * The nesting is the point. A unit is not a row with a status, it is a terminal
 * that may have opened windows, and those windows are attributed to it by the
 * compositor rather than guessed at — so this is the one place in the UI where
 * "which of these thirty processes owns that window" has an answer. Flattening
 * it into a unit table would throw that away and leave the surfaces to the
 * switcher, which knows only their titles.
 *
 * State comes from the `blit.muster.v1` channel (`extensions/muster`), whose
 * frames carry whole units. So a row is a replace, never a patch, and this
 * renders the mirror rather than accumulating from it.
 *
 * The rows name a terminal and its windows; they do not open them. Putting a
 * pane there means reaching the workspace's own switch-and-assign from inside a
 * BSP tile, which is a separate piece of plumbing — and a name is enough to
 * find it in the switcher meanwhile.
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
  followMuster,
  groupUnits,
  type MusterEvent,
  type MusterHandle,
  type MusterPhase,
  type MusterUnit,
  unitCanStop,
  unitStartVerb,
} from "./muster";
import {
  PanelEmpty,
  PanelRow,
  panelButton,
  SectionHeading,
  StatusPill,
  type PanelTone,
} from "./panelKit";
import { mergeStyle, scrollbarStyle, themeFor, ui, uiScale } from "./theme";

/** Phase → the tone and word a row shows.
 *
 *  Backoff is a warning rather than an error for the reason the session panel
 *  gives it: a supervisor retrying is one working, not one stuck. `failed` is
 *  where it gave up, and that is the only red. */
function phaseTone(unit: MusterUnit): { tone: PanelTone; label: string } {
  const phase: MusterPhase = unit.phase;
  switch (phase) {
    case "running":
      return { tone: "ok", label: "running" };
    case "exited":
      // A oneshot that finished 0 counts as ready, so it is not idle.
      return { tone: "ok", label: "done" };
    case "activating":
      return { tone: "warn", label: "starting" };
    case "waiting":
      return { tone: "warn", label: "waiting" };
    case "backoff":
      return { tone: "warn", label: "restarting" };
    case "failed":
      return { tone: "bad", label: "failed" };
    case "held":
      return { tone: "idle", label: "held" };
    case "stopped":
      return { tone: "idle", label: unit.autostart ? "stopped" : "manual" };
  }
}

/** The part of a name a row shows. A unit from a stack is `instance/template`,
 *  and its instance is already the heading above it. */
function shortName(unit: MusterUnit): string {
  if (!unit.instance) return unit.name;
  const prefix = `${unit.instance}/`;
  return unit.name.startsWith(prefix)
    ? unit.name.slice(prefix.length)
    : unit.name;
}

function eventLine(event: MusterEvent): string {
  const parts = [event.unit, event.event];
  if (event.cause) parts.push(`(${event.cause})`);
  if (event.exitCode !== undefined) parts.push(`exit ${event.exitCode}`);
  if (event.detail) parts.push(event.detail);
  return parts.join(" ");
}

export function MusterPanel(props: {
  workspace: BlitWorkspace;
  connectionId: ConnectionId;
  palette: TerminalPalette;
  fontSize: number;
}) {
  const theme = () => themeFor(props.palette);
  const scale = () => uiScale(props.fontSize);

  const [handle, setHandle] = createSignal<MusterHandle | null>(null);
  const [error, setError] = createSignal<string | null>(null);
  const [revision, setRevision] = createSignal(0);
  const [filter, setFilter] = createSignal("");
  const [tab, setTab] = createSignal<"units" | "journal">("units");
  const [expanded, setExpanded] = createSignal<ReadonlySet<string>>(new Set());

  // A supervisor update closes its channels before publishing them again. A
  // fresh handle brings a fresh full table and journal backfill, so reconnect
  // instead of stranding this mounted panel on the old channel.
  createEffect(() => {
    const connectionId = props.connectionId;
    let unsubscribe: (() => void) | undefined;
    setHandle(null);
    setError(null);
    const stop = followMuster(
      () => props.workspace.getConnection(connectionId),
      {
        onHandle: (next) => {
          unsubscribe?.();
          unsubscribe = undefined;
          setHandle(next);
          if (!next) return;
          setError(null);
          unsubscribe = next.subscribe(() => setRevision((n) => n + 1));
        },
        onRetry: () => {
          setError("Reconnecting to supervisor…");
        },
      },
    );
    onCleanup(() => {
      unsubscribe?.();
      stop();
    });
  });

  const toggle = (name: string) =>
    setExpanded((prev) => {
      const next = new Set(prev);
      if (!next.delete(name)) next.add(name);
      return next;
    });

  const matches = (unit: MusterUnit): boolean => {
    const needle = filter().trim().toLowerCase();
    if (!needle) return true;
    return (
      unit.name.toLowerCase().includes(needle) ||
      unit.description.toLowerCase().includes(needle)
    );
  };

  const groups = createMemo(() => {
    revision();
    const current = handle();
    if (!current) return [];
    return (
      groupUnits(current.units, current.instances)
        .map((group) => ({ ...group, units: group.units.filter(matches) }))
        // A filter empties groups rather than hiding them; an instance with no
        // matching member is not what the viewer typed for.
        .filter((group) => group.units.length > 0)
    );
  });

  const total = createMemo(() => {
    revision();
    return handle()?.units.size ?? 0;
  });

  const events = createMemo(() => {
    revision();
    // Newest first: a journal is read from the end.
    return [...(handle()?.events ?? [])].reverse();
  });

  const ready = () => {
    revision();
    return handle()?.ready ?? false;
  };

  const control = (
    label: string,
    tone: PanelTone | undefined,
    run: () => void,
  ) => (
    <button
      type="button"
      style={panelButton(theme(), scale(), tone)}
      onClick={run}
    >
      {label}
    </button>
  );

  return (
    <>
      <div
        style={{
          display: "flex",
          gap: `${scale().xs}px`,
          "align-items": "center",
          "margin-bottom": `${scale().sm}px`,
        }}
      >
        <For each={["units", "journal"] as const}>
          {(name) => (
            <button
              type="button"
              data-muster-tab={name}
              onClick={() => setTab(name)}
              style={mergeStyle(ui.btn, {
                "font-size": `${scale().sm}px`,
                opacity: tab() === name ? 1 : 0.55,
              })}
            >
              {name === "units" ? "Units" : "Journal"}
            </button>
          )}
        </For>
        <span style={{ flex: "1 1 auto" }} />
        <span
          style={{
            color: theme().dimFg,
            "font-size": `${scale().sm}px`,
            overflow: "hidden",
            "text-overflow": "ellipsis",
            "white-space": "nowrap",
          }}
          title={handle()?.dir ?? ""}
        >
          {handle()?.dir ?? ""}
        </span>
      </div>

      <Show
        when={!error()}
        fallback={
          <PanelEmpty theme={theme()} scale={scale()}>
            {error()}
          </PanelEmpty>
        }
      >
        <Show
          when={tab() === "units"}
          fallback={
            <div
              data-muster-journal
              style={mergeStyle(scrollbarStyle(theme()), {
                "overflow-y": "auto",
                flex: "1 1 0",
                "min-height": "6em",
                "font-size": `${scale().sm}px`,
              })}
            >
              <For
                each={events()}
                fallback={
                  <PanelEmpty theme={theme()} scale={scale()}>
                    Nothing has happened yet.
                  </PanelEmpty>
                }
              >
                {(event) => (
                  <div
                    style={{
                      display: "grid",
                      "grid-template-columns": "4em minmax(0, 1fr)",
                      gap: `${scale().sm}px`,
                      padding: `${scale().xs}px 0`,
                      "border-bottom": `1px solid ${theme().border}`,
                    }}
                  >
                    <span
                      style={{
                        color: theme().dimFg,
                        "font-variant-numeric": "tabular-nums",
                      }}
                    >
                      {event.seq}
                    </span>
                    <span
                      style={{
                        overflow: "hidden",
                        "text-overflow": "ellipsis",
                        "white-space": "nowrap",
                      }}
                      title={eventLine(event)}
                    >
                      {eventLine(event)}
                    </span>
                  </div>
                )}
              </For>
            </div>
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
            <input
              value={filter()}
              placeholder="Filter units…"
              onInput={(event) => setFilter(event.currentTarget.value)}
              style={mergeStyle(ui.input, {
                flex: "1 1 auto",
                "font-size": `${scale().md}px`,
              })}
            />
            {/* Not a "Reload": the supervisor watches its directory, so an
                edit is here before a button could be pressed. A watch the
                server refused is the one thing that cannot arrive on its own,
                and it is also the only reason to have a button here. */}
            {control("Retry watches", undefined, () => handle()?.rewatch())}
          </div>

          <div
            style={mergeStyle(scrollbarStyle(theme()), {
              "overflow-y": "auto",
              flex: "1 1 0",
              "min-height": "6em",
            })}
          >
            <For
              each={groups()}
              fallback={
                <PanelEmpty theme={theme()} scale={scale()}>
                  {!ready()
                    ? "Reading the configuration…"
                    : total() === 0
                      ? "No units are defined."
                      : "No unit matches that."}
                </PanelEmpty>
              }
            >
              {(group) => (
                <>
                  <Show when={group.instance}>
                    {(instance) => (
                      <SectionHeading
                        theme={theme()}
                        scale={scale()}
                        label={instance().name}
                        count={group.units.length}
                      >
                        <span
                          style={{
                            display: "flex",
                            gap: `${scale().tightGap}px`,
                            "align-items": "center",
                          }}
                        >
                          <span
                            style={{
                              color: theme().dimFg,
                              "font-size": `${scale().sm}px`,
                            }}
                            title={`stack: ${instance().stack}`}
                          >
                            {instance().stack}
                          </span>
                          {control("Start", "ok", () =>
                            handle()?.start(instance().name),
                          )}
                          {control("Restart", undefined, () =>
                            handle()?.restart(instance().name),
                          )}
                          {control("Stop", "warn", () =>
                            handle()?.stop(instance().name),
                          )}
                        </span>
                      </SectionHeading>
                    )}
                  </Show>
                  <Show when={!group.instance}>
                    <SectionHeading
                      theme={theme()}
                      scale={scale()}
                      label="Units"
                      count={group.units.length}
                    />
                  </Show>
                  <For each={group.units}>
                    {(unit) => (
                      <PanelRow theme={theme()} scale={scale()}>
                        <div
                          style={{
                            display: "flex",
                            "align-items": "center",
                            gap: `${scale().gap}px`,
                            "min-width": "0",
                          }}
                        >
                          <button
                            type="button"
                            data-muster-unit={unit.name}
                            aria-expanded={expanded().has(unit.name)}
                            onClick={() => toggle(unit.name)}
                            style={{
                              ...ui.btn,
                              border: "none",
                              background: "transparent",
                              color: "inherit",
                              padding: "0",
                              cursor: "pointer",
                              display: "flex",
                              "align-items": "baseline",
                              gap: `${scale().tightGap}px`,
                              "min-width": "0",
                              flex: "1 1 auto",
                              "text-align": "left",
                            }}
                          >
                            <span
                              aria-hidden="true"
                              style={{
                                color: theme().dimFg,
                                width: "1em",
                                "flex-shrink": "0",
                              }}
                            >
                              {expanded().has(unit.name) ? "▾" : "▸"}
                            </span>
                            <span
                              style={{
                                overflow: "hidden",
                                "text-overflow": "ellipsis",
                                "white-space": "nowrap",
                              }}
                              title={unit.description || unit.name}
                            >
                              {shortName(unit)}
                            </span>
                            <Show when={unit.type === "oneshot"}>
                              <span
                                style={{
                                  color: theme().dimFg,
                                  "font-size": `${scale().sm}px`,
                                }}
                              >
                                oneshot
                              </span>
                            </Show>
                            <Show when={unit.stale}>
                              <span
                                title="The unit file changed under the running process."
                                style={{
                                  color: theme().errorText,
                                  "font-size": `${scale().sm}px`,
                                }}
                              >
                                stale
                              </span>
                            </Show>
                            <Show when={unit.surfaces.length > 0}>
                              <span
                                style={{
                                  color: theme().dimFg,
                                  "font-size": `${scale().sm}px`,
                                }}
                              >
                                {unit.surfaces.length === 1
                                  ? "1 window"
                                  : `${unit.surfaces.length} windows`}
                              </span>
                            </Show>
                          </button>
                          <StatusPill
                            theme={theme()}
                            scale={scale()}
                            {...phaseTone(unit)}
                            title={
                              unit.lastExit === null
                                ? undefined
                                : `last exit ${unit.lastExit}`
                            }
                          />
                          {control(
                            unitStartVerb(unit) === "restart"
                              ? "Restart"
                              : "Start",
                            undefined,
                            () =>
                              unitStartVerb(unit) === "restart"
                                ? handle()?.restart(unit.name)
                                : handle()?.start(unit.name),
                          )}
                          <Show when={unitCanStop(unit)}>
                            {control("Stop", "warn", () =>
                              handle()?.stop(unit.name),
                            )}
                          </Show>
                        </div>

                        <Show when={expanded().has(unit.name)}>
                          <div
                            style={{
                              display: "grid",
                              gap: `${scale().tightGap}px`,
                              "padding-left": `${scale().controlX}px`,
                              "font-size": `${scale().sm}px`,
                              color: theme().dimFg,
                            }}
                          >
                            <Show when={unit.description}>
                              <span>{unit.description}</span>
                            </Show>
                            <span>
                              {unit.pty === null
                                ? "no terminal"
                                : `terminal #${unit.pty}`}
                              <Show when={unit.restarts > 0}>
                                {` · ${unit.restarts} failure${
                                  unit.restarts === 1 ? "" : "s"
                                }`}
                              </Show>
                              <Show when={unit.requires.length > 0}>
                                {` · requires ${unit.requires.join(", ")}`}
                              </Show>
                            </span>
                            <For each={unit.surfaces}>
                              {(surface) => (
                                <span data-muster-surface={surface.id}>
                                  {`window #${surface.id} · ${surface.width}×${surface.height}`}
                                  <Show when={surface.title}>
                                    {` · ${surface.title}`}
                                  </Show>
                                </span>
                              )}
                            </For>
                            <For each={unit.runs}>
                              {(run) => (
                                <span>
                                  {`kept terminal #${run.pty} · exit ${
                                    run.exitCode ?? "?"
                                  }`}
                                </span>
                              )}
                            </For>
                          </div>
                        </Show>
                      </PanelRow>
                    )}
                  </For>
                </>
              )}
            </For>
          </div>
        </Show>
      </Show>
    </>
  );
}
