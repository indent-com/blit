/**
 * LogPanel — a pure view over {@link IdeSession}'s commit log, rendered as a
 * DAG. The session watches HEAD in topological order (all parents); this
 * component lays out lanes with {@link layoutGraph} and draws the rails +
 * node for each row in a small per-row SVG gutter, then the commit summary.
 *
 * Rows are fixed-height, so the list is windowed by hand: only the rows near
 * the viewport render, between two spacer divs that keep the scroll geometry
 * (and the frontier-pagination trigger) exact over the full commit list.
 */

import {
  createMemo,
  createSignal,
  onCleanup,
  onMount,
  untrack,
  For,
  Show,
} from "solid-js";
import { gitOidHex } from "@blit-sh/core";
import { commitAssignment } from "@blit-sh/core/bsp";
import type { Theme, UIScale } from "../theme";
import { scrollbarStyle } from "../theme";
import type { IdeSession, IdeCommitRow } from "./session";
import { layoutGraph, type GraphRow, type GraphLayoutState } from "./git-graph";
import { startTileDrag } from "./tileDrag";
import { collectRefPills, RefPills, type RefPill } from "./refPills";

const LANE_W = 6;
const NODE_R = 2;
const RAIL_W = 1.25;

/** Rows rendered beyond each edge of the viewport. */
const OVERSCAN = 10;

function initials(name: string): string {
  // Only words that start with a letter — skips "(NGI0)", "[bot]", emails.
  const words = name
    .trim()
    .split(/\s+/)
    .filter((w) => /^\p{L}/u.test(w));
  if (words.length === 0) {
    const c = name.trim()[0];
    return c ? c.toUpperCase() : "?";
  }
  if (words.length === 1) return words[0].slice(0, 2).toUpperCase();
  return (words[0][0] + words[words.length - 1][0]).toUpperCase();
}

function relTime(seconds: bigint, nowSec: number): string {
  let d = nowSec - Number(seconds);
  if (d < 0) d = 0;
  if (d < 60) return `${d}s`;
  if (d < 3600) return `${Math.floor(d / 60)}m`;
  if (d < 86400) return `${Math.floor(d / 3600)}h`;
  if (d < 86400 * 30) return `${Math.floor(d / 86400)}d`;
  return `${Math.floor(d / (86400 * 30))}mo`;
}

/** ISO-8601 to the second, e.g. 2026-07-24T03:59:02Z. */
function isoTime(seconds: bigint): string {
  return new Date(Number(seconds) * 1000)
    .toISOString()
    .replace(/\.\d{3}Z$/, "Z");
}

export function LogPanel(props: {
  session: IdeSession | null;
  theme: Theme;
  scale: UIScale;
  fontFamily: string;
  fontSize: number;
  onOpenTile: (assignment: string) => void;
}) {
  const commits = () => props.session?.commits() ?? [];

  // Tick so relative timestamps advance on their own. Coarse (the rendered
  // strings are minute-grained soon after a commit lands) and paused while
  // the document is hidden; becoming visible refreshes immediately.
  const [now, setNow] = createSignal(Math.floor(Date.now() / 1000));
  const TICK_MS = 30_000;
  let tickTimer: ReturnType<typeof setInterval> | null = null;
  const stopTick = () => {
    if (tickTimer != null) {
      clearInterval(tickTimer);
      tickTimer = null;
    }
  };
  const startTick = () => {
    if (tickTimer == null)
      tickTimer = setInterval(
        () => setNow(Math.floor(Date.now() / 1000)),
        TICK_MS,
      );
  };
  const onVisibility = () => {
    if (document.hidden) stopTick();
    else {
      setNow(Math.floor(Date.now() / 1000));
      startTick();
    }
  };
  if (!document.hidden) startTick();
  document.addEventListener("visibilitychange", onVisibility);
  onCleanup(() => {
    stopTick();
    document.removeEventListener("visibilitychange", onVisibility);
  });

  // Commits whose timestamp is shown as absolute ISO (clicked).
  const [absTimes, setAbsTimes] = createSignal<Set<string>>(new Set());
  const toggleAbs = (oid: string) =>
    setAbsTimes((cur) => {
      const next = new Set(cur);
      if (next.has(oid)) next.delete(oid);
      else next.add(oid);
      return next;
    });

  function commitTile(oid: string): string | null {
    // Prefer the persistent repo workdir: gitHandle drops to null while the
    // connection re-attaches (e.g. after sleep/wake), but the log still shows
    // cached commits — without this, clicking one would silently do nothing.
    const repo =
      props.session?.repoWorkdir() || props.session?.gitHandle()?.workdir;
    const conn = props.session?.connectionId;
    return repo && conn ? commitAssignment(conn, oid, repo) : null;
  }
  function openCommit(oid: string) {
    const a = commitTile(oid);
    if (a) props.onOpenTile(a);
  }
  // Resumable layout: appending a page lays out only the appended rows, and
  // a restart (head moved, spec changed) reuses row objects whose geometry
  // is unchanged — so the keyed graph cells below keep their DOM.
  let graphState: GraphLayoutState | null = null;
  const graph = createMemo(() => {
    graphState = layoutGraph(graphState, commits());
    return graphState;
  });

  // Every ref pointing at each commit (shared with the commit viewer).
  // gitState pushes on every worktree settle; the pill map only depends on
  // head + refs, so it's rebuilt behind a fingerprint of those (the same
  // pattern as the session's opRefTips) and keeps its identity otherwise.
  const refsKey = createMemo(() => {
    const gs = props.session?.gitState();
    if (!gs) return "";
    const fmt = props.session?.gitHandle()?.oidFormat;
    const parts: string[] = gs.head
      ? [`${gs.head.flags} ${gs.head.name} ${gitOidHex(gs.head.oid, fmt)}`]
      : [];
    for (const [name, ref] of gs.refs) {
      parts.push(
        `${name} ${ref.flags} ${gitOidHex(ref.oid, fmt)} ${gitOidHex(ref.peeled, fmt)}`,
      );
    }
    return parts.join("\n");
  });
  const refsByOid = createMemo<ReadonlyMap<string, RefPill[]>>(() => {
    refsKey();
    return untrack(() => {
      const gs = props.session?.gitState();
      if (!gs) return new Map<string, RefPill[]>();
      return collectRefPills(gs, props.session?.gitHandle()?.oidFormat);
    });
  });
  // Authors shown in full (clicking an initials chip reveals every commit by
  // that author).
  const [expanded, setExpanded] = createSignal<Set<string>>(new Set());
  const toggleAuthor = (name: string) =>
    setExpanded((cur) => {
      const next = new Set(cur);
      if (next.has(name)) next.delete(name);
      else next.add(name);
      return next;
    });

  const rowH = () => Math.round(props.fontSize * 1.35);
  // Columns this row actually uses, so its rail gutter is only as wide as its
  // own lanes — not the global maximum — letting the subject run right up to
  // this row's rails.
  const rowCols = (row: GraphRow) =>
    Math.max(row.nodeCol, ...row.through, ...row.inCols, ...row.outCols) + 1;

  // Lane colours: the theme's semantic accents, with mixed tiers for lanes
  // beyond the base set so deep graphs stay legible and on-theme.
  const laneColor = (i: number): string => {
    const base = [
      props.theme.accent,
      props.theme.success,
      props.theme.warning,
      props.theme.error,
    ];
    const c = base[i % base.length];
    const tier = Math.floor(i / base.length);
    return tier === 0
      ? c
      : `color-mix(in srgb, ${c} ${Math.max(35, 70 - tier * 15)}%, ${props.theme.fg})`;
  };

  // Smooth vertical-ish edge between two points (straight when aligned).
  const edge = (x1: number, y1: number, x2: number, y2: number) => {
    if (x1 === x2) return `M ${x1} ${y1} L ${x2} ${y2}`;
    const my = (y1 + y2) / 2;
    return `M ${x1} ${y1} C ${x1} ${my}, ${x2} ${my}, ${x2} ${y2}`;
  };

  const graphCell = (row: GraphRow) => {
    const h = rowH();
    const mid = h / 2;
    const w = rowCols(row) * LANE_W;
    // Right-aligned within its own width: lane 0 (the main line) hugs the
    // right edge. Since each row's SVG is the rightmost element, lane i sits a
    // constant distance from the panel's right edge, so rails still line up
    // across rows despite the per-row widths.
    const cx = (col: number) => w - (col + 0.5) * LANE_W;
    return (
      <svg width={w} height={h} style={{ "flex-shrink": 0, display: "block" }}>
        <For each={row.through}>
          {(col) => (
            <line
              x1={cx(col)}
              y1={0}
              x2={cx(col)}
              y2={h}
              stroke={laneColor(col)}
              stroke-width={RAIL_W}
            />
          )}
        </For>
        <For each={row.inCols}>
          {(col) => (
            <path
              d={edge(cx(col), 0, cx(row.nodeCol), mid)}
              fill="none"
              stroke={laneColor(col)}
              stroke-width={RAIL_W}
            />
          )}
        </For>
        <For each={row.outCols}>
          {(col) => (
            <path
              d={edge(cx(row.nodeCol), mid, cx(col), h)}
              fill="none"
              stroke={laneColor(col)}
              stroke-width={RAIL_W}
            />
          )}
        </For>
        {/* Collapsed long edges: dashed stub where a branch detaches (down)
            or rejoins (up) after a long idle stretch. */}
        <Show when={row.resumed}>
          <line
            x1={cx(row.nodeCol)}
            y1={0}
            x2={cx(row.nodeCol)}
            y2={mid}
            stroke={laneColor(row.nodeCol)}
            stroke-width={RAIL_W}
            stroke-dasharray="1.5 2"
            opacity={0.55}
          />
        </Show>
        <Show when={row.suspendedOut}>
          <line
            x1={cx(row.nodeCol)}
            y1={mid}
            x2={cx(row.nodeCol)}
            y2={h}
            stroke={laneColor(row.nodeCol)}
            stroke-width={RAIL_W}
            stroke-dasharray="1.5 2"
            opacity={0.55}
          />
        </Show>
        <circle
          cx={cx(row.nodeCol)}
          cy={mid}
          r={NODE_R}
          fill={laneColor(row.nodeCol)}
          stroke={props.theme.bg}
          stroke-width={1}
        />
      </svg>
    );
  };

  // ── Windowing: fixed-height rows between spacer divs. ──────────────────
  let scrollEl!: HTMLDivElement;
  let headerEl: HTMLDivElement | undefined;
  let resizeObs: ResizeObserver | undefined;
  const [scrollTop, setScrollTop] = createSignal(0);
  const [viewH, setViewH] = createSignal(0);
  // Height of the spec-input header above row 0 inside the scroll content.
  const [headerH, setHeaderH] = createSignal(0);
  const headerRef = (el: HTMLDivElement) => {
    headerEl = el;
    resizeObs?.observe(el);
  };
  onMount(() => {
    resizeObs = new ResizeObserver(() => {
      setViewH(scrollEl.clientHeight);
      if (headerEl) setHeaderH(headerEl.offsetHeight);
    });
    resizeObs.observe(scrollEl);
    if (headerEl) resizeObs.observe(headerEl);
    setViewH(scrollEl.clientHeight);
    if (headerEl) setHeaderH(headerEl.offsetHeight);
    onCleanup(() => resizeObs?.disconnect());
  });

  const window_ = createMemo(() => {
    const total = commits().length;
    const h = rowH();
    const top = Math.max(0, scrollTop() - headerH());
    const start = Math.min(total, Math.max(0, Math.floor(top / h) - OVERSCAN));
    const end = Math.min(
      total,
      Math.max(start, Math.ceil((top + viewH()) / h) + OVERSCAN),
    );
    return { start, end, total };
  });

  // Fetch older commits when scrolled near the bottom (frontier pagination) —
  // the spacers keep scrollHeight exact, so the trigger works unchanged.
  const onScroll = (e: Event) => {
    const el = e.currentTarget as HTMLElement;
    setScrollTop(el.scrollTop);
    if (!props.session?.hasMoreLog()) return;
    if (el.scrollHeight - el.scrollTop - el.clientHeight < 400) {
      props.session.loadMoreLog();
    }
  };

  const commitRow = (c: IdeCommitRow, rowIdx: () => number) => (
    <div
      onClick={() => openCommit(c.oid)}
      draggable={true}
      onDragStart={(e) => {
        const a = commitTile(c.oid);
        if (a) startTileDrag(e, a);
      }}
      style={{
        display: "flex",
        "align-items": "center",
        gap: `${props.scale.tightGap}px`,
        height: `${rowH()}px`,
        padding: `0 ${props.scale.tightGap}px`,
        "font-family": props.fontFamily,
        // Commit subjects are content: configured font size, like the
        // editor. The timestamp and refs hung off the row stay smaller.
        "font-size": `${props.scale.md}px`,
        "white-space": "nowrap",
        cursor: "pointer",
      }}
      title={c.subject}
    >
      <span
        onClick={(e) => {
          e.stopPropagation();
          toggleAbs(c.oid);
        }}
        title="Toggle absolute time"
        style={{
          color: props.theme.dimFg,
          "font-size": `${props.scale.xs}px`,
          "flex-shrink": 0,
          "min-width": absTimes().has(c.oid) ? undefined : "2.6em",
          "text-align": "right",
          "font-variant-numeric": "tabular-nums",
        }}
      >
        {absTimes().has(c.oid) ? isoTime(c.time) : relTime(c.time, now())}
      </span>
      <span
        style={{
          color: props.theme.warning,
          "flex-shrink": 0,
          "font-variant-numeric": "tabular-nums",
        }}
      >
        {c.short}
      </span>
      <button
        onClick={(e) => {
          e.stopPropagation();
          toggleAuthor(c.author);
        }}
        title={c.author}
        style={{
          "flex-shrink": 0,
          border: "none",
          background: "transparent",
          padding: 0,
          cursor: "pointer",
          color: props.theme.accent,
          "font-family": "inherit",
          "font-size": `${props.scale.xs}px`,
        }}
      >
        {expanded().has(c.author) ? c.author : initials(c.author)}
      </button>
      <span
        style={{
          flex: 1,
          "min-width": 0,
          color: props.theme.fg,
          overflow: "hidden",
          "text-overflow": "ellipsis",
        }}
      >
        {c.subject}
      </span>
      <Show when={refsByOid().get(c.oid)}>
        {(pills) => (
          <RefPills pills={pills()} theme={props.theme} scale={props.scale} />
        )}
      </Show>
      {/* Keyed on the GraphRow object: the layout reuses row objects whose
          geometry didn't change, so this rebuilds only genuinely moved
          rails (and tracks the object, not just its truthiness). */}
      <Show when={graph().rows[rowIdx()]} keyed>
        {(row) => graphCell(row)}
      </Show>
    </div>
  );

  return (
    <div
      ref={scrollEl}
      onScroll={onScroll}
      style={{
        flex: "1 1 0",
        "min-height": 0,
        "overflow-y": "auto",
        ...scrollbarStyle(props.theme),
      }}
    >
      {/* Revision spec: whitespace-separated expressions merged like
          `git rev-list` args (`base..tip …`), so the log can walk from a
          base to multiple heads. Empty = HEAD. */}
      <Show when={props.session}>
        <div
          ref={headerRef}
          style={{
            padding: `${props.scale.tightGap}px ${props.scale.tightGap}px 0`,
          }}
        >
          <input
            value={props.session?.logSpec() ?? ""}
            placeholder="HEAD (spec: base..tip …)"
            spellcheck={false}
            style={{
              width: "100%",
              "box-sizing": "border-box",
              background: "transparent",
              color: props.theme.fg,
              border: `1px solid ${props.theme.subtleBorder}`,
              "border-radius": "2px",
              outline: "none",
              padding: "1px 4px",
              "font-family": props.fontFamily,
              "font-size": `${props.scale.xs}px`,
            }}
            onKeyDown={(ev) => {
              ev.stopPropagation();
              if (ev.key === "Enter")
                props.session?.setLogSpec(ev.currentTarget.value);
              else if (ev.key === "Escape") ev.currentTarget.blur();
            }}
            onFocus={(ev) => ev.currentTarget.select()}
          />
          <Show when={props.session?.logSpecError()}>
            <div
              style={{
                "font-size": `${props.scale.xs}px`,
                color: props.theme.errorText,
                padding: "2px 2px 0",
              }}
            >
              {props.session?.logSpecError()}
            </div>
          </Show>
        </div>
      </Show>
      <Show
        when={commits().length > 0}
        fallback={
          <div
            style={{
              padding: `${props.scale.panelPadding}px`,
              "font-size": `${props.scale.sm}px`,
              color: props.theme.dimFg,
            }}
          >
            {!props.session
              ? "No root selected."
              : props.session.logLoaded()
                ? "No commits."
                : "Loading…"}
          </div>
        }
      >
        <div style={{ height: `${window_().start * rowH()}px` }} />
        <For each={commits().slice(window_().start, window_().end)}>
          {(c, i) => commitRow(c, () => window_().start + i())}
        </For>
        <div
          style={{
            height: `${(window_().total - window_().end) * rowH()}px`,
          }}
        />
        <Show when={props.session?.hasMoreLog()}>
          <div
            onClick={() => props.session?.loadMoreLog()}
            style={{
              padding: `${props.scale.tightGap}px ${props.scale.panelPadding}px`,
              "font-size": `${props.scale.xs}px`,
              color: props.theme.dimFg,
              cursor: "pointer",
              "text-align": "center",
            }}
          >
            Load older commits…
          </div>
        </Show>
      </Show>
    </div>
  );
}
