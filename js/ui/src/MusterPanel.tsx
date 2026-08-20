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
 * A terminal is an assignment, just like a parked terminal or an editor tile.
 * Its chip opens it in the focused pane and carries that same assignment in the
 * app's tile drag payload, so it can be placed in a different pane instead.
 */

import {
  createEffect,
  createMemo,
  createSignal,
  For,
  onCleanup,
  Show,
} from "solid-js";
import { Dynamic } from "solid-js/web";
import type {
  BlitSession,
  BlitWorkspace,
  ConnectionId,
  TerminalPalette,
} from "@blit-sh/core";
import {
  followMuster,
  groupUnits,
  instanceCanStart,
  musterDiagram,
  type MusterEvent,
  type MusterHandle,
  type MusterInstance,
  type MusterPhase,
  type MusterUnit,
  unitCanStop,
  unitHasDetails,
  unitStartVerb,
} from "./muster";
import { MusterGraph } from "./MusterGraph";
import {
  PanelEmpty,
  panelButton,
  pillColor,
  SectionHeading,
  StatusPill,
  type PanelTone,
} from "./panelKit";
import { fillTileDrag, startTileDrag, startTouchDrag } from "./ide/tileDrag";
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
      // A service is still establishing readiness; a oneshot is already doing
      // its finite work and becomes ready only when that work exits 0.
      if (unit.type === "oneshot") return { tone: "ok", label: "running" };
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

/** Dependencies inside the same stack do not need the instance repeated: the
 * card already sits under that instance's heading. */
function shortDependency(unit: MusterUnit, dependency: string): string {
  if (!unit.instance) return dependency;
  const prefix = `${unit.instance}/`;
  return dependency.startsWith(prefix)
    ? dependency.slice(prefix.length)
    : dependency;
}

interface MusterUnitSection {
  readonly kind: "service" | "oneshot";
  readonly label: "Services" | "Oneshots";
  readonly units: readonly MusterUnit[];
}

/** Keep finite setup/build work out of the service scan without losing the
 * instance boundary each unit belongs to. Empty categories disappear. */
function unitSections(units: readonly MusterUnit[]): MusterUnitSection[] {
  const services = units.filter((unit) => unit.type !== "oneshot");
  const oneshots = units.filter((unit) => unit.type === "oneshot");
  const sections: MusterUnitSection[] = [];
  if (services.length > 0) {
    sections.push({ kind: "service", label: "Services", units: services });
  }
  if (oneshots.length > 0) {
    sections.push({ kind: "oneshot", label: "Oneshots", units: oneshots });
  }
  return sections;
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
  sessions?: readonly BlitSession[];
  /** Show a terminal in the workspace's focused pane. */
  onOpenAssignment?: (assignment: string) => void;
}) {
  const theme = () => themeFor(props.palette);
  const scale = () => uiScale(props.fontSize);

  const [handle, setHandle] = createSignal<MusterHandle | null>(null);
  const [error, setError] = createSignal<string | null>(null);
  const [revision, setRevision] = createSignal(0);
  const [filter, setFilter] = createSignal("");
  const [tab, setTab] = createSignal<"units" | "graph" | "journal">("units");
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

  const summary = createMemo(() => {
    revision();
    const units = [...(handle()?.units.values() ?? [])];
    return {
      running: units.filter((unit) => unit.phase === "running").length,
      attention: units.filter(
        (unit) => unit.phase === "failed" || unit.phase === "backoff",
      ).length,
    };
  });

  // The supervisor speaks in server-local PTY numbers; panes speak in
  // workspace session IDs. Closed historical sessions can share a PTY number
  // after reconnect, so the newest non-closed one wins.
  const sessionsByPty = createMemo(() => {
    const byPty = new Map<number, BlitSession>();
    const sessions = props.sessions ?? props.workspace.getSnapshot().sessions;
    for (const session of sessions) {
      if (
        session.connectionId === props.connectionId &&
        session.state !== "closed"
      ) {
        byPty.set(session.ptyId, session);
      }
    }
    return byPty;
  });

  const events = createMemo(() => {
    revision();
    // Newest first: a journal is read from the end.
    return [...(handle()?.events ?? [])].reverse();
  });

  const diagram = createMemo(() => {
    revision();
    const current = handle();
    return musterDiagram(
      current?.units ?? new Map(),
      current?.instances ?? new Map(),
    );
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
      style={mergeStyle(
        panelButton(theme(), scale(), tone),
        tone === "bad"
          ? {
              color: theme().errorText,
              border: `1px solid ${theme().errorText}`,
            }
          : {},
      )}
      onClick={run}
    >
      {label}
    </button>
  );

  const openTerminal = (sessionId: string) => {
    if (props.onOpenAssignment) props.onOpenAssignment(sessionId);
    else props.workspace.focusSession(sessionId);
  };

  const canStartInstance = (instance: MusterInstance) => {
    revision();
    const current = handle();
    return current ? instanceCanStart(instance, current.units) : false;
  };

  const terminal = (
    pty: number,
    options?: { retained?: boolean; exitCode?: number | null },
  ) => {
    const session = () => sessionsByPty().get(pty) ?? null;
    const label = options?.retained ? `#${pty}` : `terminal #${pty}`;
    const descriptiveLabel = options?.retained
      ? `retained terminal #${pty}, exit ${options.exitCode ?? "unknown"}`
      : label;
    const title = () =>
      session()
        ? `Open ${descriptiveLabel}. Drag it to place it in a pane.`
        : `${descriptiveLabel} is no longer available in this workspace.`;
    return (
      <button
        type="button"
        data-muster-terminal={pty}
        data-muster-session={session()?.id}
        aria-label={`Open ${descriptiveLabel}`}
        title={title()}
        disabled={!session()}
        draggable={!!session()}
        onDragStart={(event) => {
          const current = session();
          if (current) startTileDrag(event, current.id);
          else event.preventDefault();
        }}
        onPointerDown={(event) => {
          const current = session();
          if (!current) return;
          startTouchDrag(
            event,
            (data) => fillTileDrag(data, current.id),
            "long-press",
          );
        }}
        onClick={() => {
          const current = session();
          if (current) openTerminal(current.id);
        }}
        style={mergeStyle(ui.btn, {
          display: "inline-flex",
          "align-items": "center",
          gap: `${scale().tightGap}px`,
          padding: options?.retained
            ? "0"
            : `${scale().controlY}px ${scale().controlX}px`,
          border: options?.retained
            ? "none"
            : `1px solid ${session() ? theme().accent : theme().subtleBorder}`,
          "background-color":
            options?.retained || !session() ? "transparent" : theme().inputBg,
          color: session()
            ? options?.retained
              ? theme().accent
              : theme().fg
            : theme().dimFg,
          opacity: session() ? 1 : 0.55,
          cursor: session() ? "grab" : "not-allowed",
          "font-size": `${scale().sm}px`,
          "white-space": "nowrap",
          "touch-action": "pan-y",
        })}
      >
        <span aria-hidden="true" style={{ color: theme().accent }}>
          ▣
        </span>
        {label}
        <Show when={options?.retained}>
          <span style={{ color: theme().dimFg }}>
            {`· exit ${options?.exitCode ?? "?"}`}
          </span>
        </Show>
      </button>
    );
  };

  return (
    <div
      data-muster-panel=""
      style={{
        display: "flex",
        "flex-direction": "column",
        flex: "1 1 auto",
        "min-height": "0",
        gap: `${scale().sm}px`,
      }}
    >
      <header
        style={{
          display: "flex",
          "align-items": "center",
          gap: `${scale().gap}px`,
          "min-width": "0",
          "border-bottom": `1px solid ${theme().subtleBorder}`,
          "padding-bottom": `${scale().sm}px`,
        }}
      >
        <div
          role="tablist"
          aria-label="Muster views"
          style={{
            display: "inline-flex",
            padding: "2px",
            border: `1px solid ${theme().subtleBorder}`,
            "background-color": theme().inputBg,
          }}
        >
          <For each={["units", "graph", "journal"] as const}>
            {(name) => (
              <button
                type="button"
                role="tab"
                data-muster-tab={name}
                aria-selected={tab() === name}
                onClick={() => setTab(name)}
                style={mergeStyle(ui.btn, {
                  padding: `${scale().controlY}px ${scale().controlX}px`,
                  "font-size": `${scale().sm}px`,
                  "background-color":
                    tab() === name ? theme().selectedBg : "transparent",
                  opacity: tab() === name ? 1 : 0.65,
                })}
              >
                {name === "units"
                  ? "Units"
                  : name === "graph"
                    ? "Graph"
                    : "Journal"}
              </button>
            )}
          </For>
        </div>
        <span
          style={{
            flex: "1 1 auto",
            color: theme().dimFg,
            "font-size": `${scale().sm}px`,
            overflow: "hidden",
            "text-overflow": "ellipsis",
            "white-space": "nowrap",
            "text-align": "right",
          }}
          title={handle()?.dir ?? ""}
        >
          {handle()?.dir ?? ""}
        </span>
      </header>

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
            <Show
              when={tab() === "graph"}
              fallback={
                <div
                  data-muster-journal
                  style={mergeStyle(scrollbarStyle(theme()), {
                    "overflow-y": "auto",
                    flex: "1 1 0",
                    "min-height": "6em",
                    display: "grid",
                    "align-content": "start",
                    gap: "1px",
                    "background-color": theme().subtleBorder,
                    border: `1px solid ${theme().subtleBorder}`,
                    "font-size": `${scale().sm}px`,
                  })}
                >
                  <For
                    each={events()}
                    fallback={
                      <div style={{ "background-color": theme().panelBg }}>
                        <PanelEmpty theme={theme()} scale={scale()}>
                          Nothing has happened yet.
                        </PanelEmpty>
                      </div>
                    }
                  >
                    {(event) => (
                      <div
                        style={{
                          display: "grid",
                          "grid-template-columns": "4.5em minmax(0, 1fr)",
                          gap: `${scale().sm}px`,
                          padding: `${scale().controlY + 1}px ${scale().controlX}px`,
                          "background-color": theme().panelBg,
                        }}
                      >
                        <span
                          style={{
                            color: theme().dimFg,
                            "font-variant-numeric": "tabular-nums",
                          }}
                        >
                          #{event.seq}
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
              <MusterGraph
                diagram={diagram()}
                theme={theme()}
                scale={scale()}
              />
            </Show>
          }
        >
          <div
            style={{
              display: "flex",
              "flex-wrap": "wrap",
              gap: `${scale().sm}px`,
              "align-items": "center",
            }}
          >
            <input
              value={filter()}
              placeholder="Filter units…"
              aria-label="Filter units"
              onInput={(event) => setFilter(event.currentTarget.value)}
              style={mergeStyle(ui.input, {
                flex: "1 1 18em",
                "min-width": "12em",
                "box-sizing": "border-box",
                color: theme().fg,
                "background-color": theme().inputBg,
                "font-size": `${scale().sm}px`,
                padding: `${scale().controlY + 1}px ${scale().controlX}px`,
              })}
            />
            <div
              aria-label="Unit summary"
              style={{
                display: "flex",
                "align-items": "center",
                gap: `${scale().gap}px`,
                "font-size": `${scale().sm}px`,
                color: theme().dimFg,
                "white-space": "nowrap",
              }}
            >
              <span>{`${total()} unit${total() === 1 ? "" : "s"}`}</span>
              <Show when={summary().running > 0}>
                <span style={{ color: theme().success }}>
                  {`${summary().running} running`}
                </span>
              </Show>
              <Show when={summary().attention > 0}>
                <span style={{ color: theme().errorText }}>
                  {`${summary().attention} need attention`}
                </span>
              </Show>
            </div>
            {/* Not a reload: successful watches already deliver edits. */}
            {control("Retry watches", undefined, () => handle()?.rewatch())}
          </div>

          <div
            style={mergeStyle(scrollbarStyle(theme()), {
              "overflow-y": "auto",
              flex: "1 1 0",
              "min-height": "6em",
              "padding-right": `${scale().tightGap}px`,
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
                <section
                  data-muster-group={group.instance?.name ?? "standalone"}
                  style={{ "margin-bottom": `${scale().gap * 2}px` }}
                >
                  <Show
                    when={group.instance}
                    fallback={
                      <SectionHeading
                        theme={theme()}
                        scale={scale()}
                        label="Standalone"
                        count={group.units.length}
                      />
                    }
                  >
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
                            "flex-wrap": "wrap",
                            gap: `${scale().tightGap}px`,
                            "align-items": "center",
                            "justify-content": "flex-end",
                            "min-width": "0",
                          }}
                        >
                          <span
                            style={{
                              color: theme().dimFg,
                              "font-size": `${scale().sm}px`,
                              overflow: "hidden",
                              "text-overflow": "ellipsis",
                              "white-space": "nowrap",
                              "max-width": "32em",
                            }}
                            title={`stack: ${instance().stack}`}
                          >
                            {instance().stack}
                          </span>
                          <Show when={canStartInstance(instance())}>
                            {control("Start", "ok", () =>
                              handle()?.start(instance().name),
                            )}
                          </Show>
                          {control("Restart", undefined, () =>
                            handle()?.restart(instance().name),
                          )}
                          {control("Stop", "bad", () =>
                            handle()?.stop(instance().name),
                          )}
                        </span>
                      </SectionHeading>
                    )}
                  </Show>

                  <For each={unitSections(group.units)}>
                    {(section) => (
                      <div
                        data-muster-unit-section={section.kind}
                        style={{ "padding-top": `${scale().gap}px` }}
                      >
                        <SectionHeading
                          theme={theme()}
                          scale={scale()}
                          label={section.label}
                          count={section.units.length}
                        />
                        <div
                          style={{
                            display: "grid",
                            "grid-template-columns":
                              "repeat(auto-fit, minmax(min(30em, 100%), 1fr))",
                            gap: `${scale().gap}px`,
                            padding: `${scale().gap}px 0 0`,
                          }}
                        >
                          <For each={section.units}>
                            {(unit) => {
                              const phase = phaseTone(unit);
                              const hasDetails = unitHasDetails(unit);
                              const showLastExit =
                                unit.lastExit !== null &&
                                unit.runs[0]?.exitCode !== unit.lastExit;
                              return (
                                <article
                                  style={{
                                    display: "grid",
                                    "align-content": "start",
                                    gap: `${scale().gap}px`,
                                    padding: `${scale().controlX}px`,
                                    border: `1px solid ${theme().subtleBorder}`,
                                    "border-left": `3px solid ${pillColor(
                                      theme(),
                                      phase.tone,
                                    )}`,
                                    "background-color": theme().panelBg,
                                    "min-width": "0",
                                  }}
                                >
                                  <div
                                    style={{
                                      display: "grid",
                                      "grid-template-columns":
                                        "minmax(0, 1fr) auto",
                                      "align-items": "start",
                                      "column-gap": `${scale().gap}px`,
                                      "row-gap": `${scale().tightGap}px`,
                                      "min-width": "0",
                                    }}
                                  >
                                    <Dynamic
                                      component={hasDetails ? "button" : "div"}
                                      type={hasDetails ? "button" : undefined}
                                      data-muster-unit={unit.name}
                                      aria-expanded={
                                        hasDetails
                                          ? expanded().has(unit.name)
                                          : undefined
                                      }
                                      onClick={
                                        hasDetails
                                          ? () => toggle(unit.name)
                                          : undefined
                                      }
                                      style={mergeStyle(ui.btn, {
                                        border: "none",
                                        background: "transparent",
                                        color: "inherit",
                                        padding: "0",
                                        cursor: hasDetails
                                          ? "pointer"
                                          : "default",
                                        display: "grid",
                                        "grid-template-columns": hasDetails
                                          ? "1em minmax(0, 1fr)"
                                          : "minmax(0, 1fr)",
                                        "column-gap": `${scale().tightGap}px`,
                                        "min-width": "0",
                                        "text-align": "left",
                                        opacity: 1,
                                      })}
                                    >
                                      <Show when={hasDetails}>
                                        <span
                                          aria-hidden="true"
                                          style={{ color: theme().dimFg }}
                                        >
                                          {expanded().has(unit.name)
                                            ? "▾"
                                            : "▸"}
                                        </span>
                                      </Show>
                                      <span
                                        style={{
                                          display: "grid",
                                          gap: "2px",
                                          "min-width": "0",
                                        }}
                                      >
                                        <span
                                          style={{
                                            display: "flex",
                                            "align-items": "baseline",
                                            "flex-wrap": "wrap",
                                            gap: `${scale().tightGap}px`,
                                            "font-weight": 600,
                                          }}
                                        >
                                          <span>{shortName(unit)}</span>
                                          <Show when={unit.type === "oneshot"}>
                                            <span
                                              style={{
                                                color: theme().dimFg,
                                                "font-size": `${scale().xs}px`,
                                                "font-weight": 400,
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
                                                "font-size": `${scale().xs}px`,
                                                "font-weight": 400,
                                              }}
                                            >
                                              stale
                                            </span>
                                          </Show>
                                        </span>
                                        <Show when={unit.description}>
                                          <span
                                            style={{
                                              color: theme().dimFg,
                                              "font-size": `${scale().sm}px`,
                                              overflow: "hidden",
                                              "text-overflow": "ellipsis",
                                              "white-space": "nowrap",
                                            }}
                                            title={unit.description}
                                          >
                                            {unit.description}
                                          </span>
                                        </Show>
                                      </span>
                                    </Dynamic>
                                    <div style={{ "justify-self": "end" }}>
                                      <StatusPill
                                        theme={theme()}
                                        scale={scale()}
                                        {...phase}
                                        title={
                                          unit.lastExit === null
                                            ? undefined
                                            : `last exit ${unit.lastExit}`
                                        }
                                      />
                                    </div>

                                    <div
                                      style={{
                                        display: "flex",
                                        "align-items": "center",
                                        "flex-wrap": "wrap",
                                        gap: `${scale().tightGap}px`,
                                        "min-width": "0",
                                      }}
                                    >
                                      <Show
                                        when={unit.pty !== null}
                                        fallback={
                                          <Show when={unit.type !== "oneshot"}>
                                            <span
                                              style={{
                                                color: theme().dimFg,
                                                "font-size": `${scale().sm}px`,
                                                "white-space": "nowrap",
                                              }}
                                            >
                                              no terminal
                                            </span>
                                          </Show>
                                        }
                                      >
                                        {terminal(unit.pty!)}
                                      </Show>
                                      <Show when={unit.surfaces.length > 0}>
                                        <span
                                          style={{
                                            color: theme().dimFg,
                                            "font-size": `${scale().xs}px`,
                                            "white-space": "nowrap",
                                          }}
                                        >
                                          {unit.surfaces.length === 1
                                            ? "1 window"
                                            : `${unit.surfaces.length} windows`}
                                        </span>
                                      </Show>
                                    </div>
                                    <div
                                      style={{
                                        display: "flex",
                                        gap: `${scale().tightGap}px`,
                                        "justify-content": "flex-end",
                                      }}
                                    >
                                      {control(
                                        unitStartVerb(unit) === "restart"
                                          ? "Restart"
                                          : "Start",
                                        unitStartVerb(unit) === "start"
                                          ? "ok"
                                          : undefined,
                                        () =>
                                          unitStartVerb(unit) === "restart"
                                            ? handle()?.restart(unit.name)
                                            : handle()?.start(unit.name),
                                      )}
                                      <Show when={unitCanStop(unit)}>
                                        {control("Stop", "bad", () =>
                                          handle()?.stop(unit.name),
                                        )}
                                      </Show>
                                    </div>
                                  </div>

                                  <Show
                                    when={
                                      hasDetails && expanded().has(unit.name)
                                    }
                                  >
                                    <div
                                      style={{
                                        display: "grid",
                                        "grid-template-columns":
                                          "max-content minmax(0, 1fr)",
                                        "column-gap": `${scale().gap}px`,
                                        "row-gap": `${scale().tightGap}px`,
                                        "align-items": "baseline",
                                        padding: `${scale().gap}px 0 0`,
                                        "border-top": `1px solid ${theme().subtleBorder}`,
                                        "font-size": `${scale().sm}px`,
                                        color: theme().dimFg,
                                      }}
                                    >
                                      <Show when={unit.restarts > 0}>
                                        <span
                                          style={{
                                            color: theme().dimFg,
                                            "font-size": `${scale().xs}px`,
                                          }}
                                        >
                                          Failures
                                        </span>
                                        <span>{unit.restarts}</span>
                                      </Show>

                                      <Show when={showLastExit}>
                                        <span
                                          style={{
                                            color: theme().dimFg,
                                            "font-size": `${scale().xs}px`,
                                          }}
                                        >
                                          Last exit
                                        </span>
                                        <span>{unit.lastExit}</span>
                                      </Show>

                                      <Show when={unit.requires.length > 0}>
                                        <span
                                          style={{
                                            color: theme().dimFg,
                                            "font-size": `${scale().xs}px`,
                                          }}
                                        >
                                          Requires
                                        </span>
                                        <span>
                                          {unit.requires
                                            .map((dependency) =>
                                              shortDependency(unit, dependency),
                                            )
                                            .join(" · ")}
                                        </span>
                                      </Show>

                                      <Show when={unit.surfaces.length > 0}>
                                        <span
                                          style={{
                                            color: theme().dimFg,
                                            "font-size": `${scale().xs}px`,
                                          }}
                                        >
                                          {unit.surfaces.length === 1
                                            ? "Window"
                                            : "Windows"}
                                        </span>
                                        <span
                                          style={{
                                            display: "grid",
                                            gap: "2px",
                                            "min-width": "0",
                                          }}
                                        >
                                          <For each={unit.surfaces}>
                                            {(surface) => (
                                              <span
                                                data-muster-surface={surface.id}
                                              >
                                                {`#${surface.id} · ${surface.width}×${surface.height}`}
                                                <Show when={surface.title}>
                                                  {` · ${surface.title}`}
                                                </Show>
                                              </span>
                                            )}
                                          </For>
                                        </span>
                                      </Show>

                                      <Show when={unit.runs.length > 0}>
                                        <span
                                          style={{
                                            color: theme().dimFg,
                                            "font-size": `${scale().xs}px`,
                                          }}
                                        >
                                          Retained
                                        </span>
                                        <span
                                          style={{
                                            display: "flex",
                                            "flex-wrap": "wrap",
                                            gap: `${scale().gap}px`,
                                            "min-width": "0",
                                          }}
                                        >
                                          <For each={unit.runs}>
                                            {(run) =>
                                              terminal(run.pty, {
                                                retained: true,
                                                exitCode: run.exitCode,
                                              })
                                            }
                                          </For>
                                        </span>
                                      </Show>
                                    </div>
                                  </Show>
                                </article>
                              );
                            }}
                          </For>
                        </div>
                      </div>
                    )}
                  </For>
                </section>
              )}
            </For>
          </div>
        </Show>
      </Show>
    </div>
  );
}
