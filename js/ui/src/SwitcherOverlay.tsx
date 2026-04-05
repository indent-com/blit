import {
  createSignal,
  createEffect,
  createMemo,
  onMount,
  onCleanup,
  Show,
  For,
  type JSX,
} from "solid-js";
import {
  BlitTerminal,
  BlitSurfaceView,
  createBlitWorkspace,
} from "@blit-sh/solid";
import {
  SEARCH_SOURCE_SCROLLBACK,
  SEARCH_SOURCE_TITLE,
  SEARCH_SOURCE_VISIBLE,
} from "@blit-sh/core";
import type {
  BlitSearchResult,
  BlitSession,
  BlitSurface,
  SessionId,
  TerminalPalette,
} from "@blit-sh/core";
import { OverlayBackdrop, OverlayPanel } from "./Overlay";
import {
  overlayChromeStyles,
  sessionName,
  sidebarWidth,
  themeFor,
  ui,
  uiScale,
} from "./theme";
import { LayoutPreview } from "./bsp/LayoutPreview";
import {
  enumeratePanes,
  layoutFromDSL,
  type BSPAssignments,
  type BSPLayout,
} from "./bsp/layout";
import { leafCount } from "@blit-sh/core/bsp";
import { t, tp } from "./i18n";

const SOURCE_LABEL: Record<number, string> = {
  [SEARCH_SOURCE_TITLE]: t("switcher.sourceTitle"),
  [SEARCH_SOURCE_VISIBLE]: t("switcher.sourceTerminal"),
  [SEARCH_SOURCE_SCROLLBACK]: t("switcher.sourceBacklog"),
};

type LayoutItem = {
  type: "layout";
  key: string;
  title: string;
  subtitle: string;
  layout: BSPLayout;
};

type SessionItem = {
  type: "session";
  key: string;
  title: string;
  subtitle: string;
  sessionId: SessionId;
  exited: boolean;
  context?: string;
  source?: number;
  focused: boolean;
};

type ActionItem = {
  type: "action";
  key: string;
  title: string;
  subtitle: string;
  action:
    | "new-terminal"
    | "clear-layout"
    | "clear-local-storage"
    | "change-font"
    | "change-palette"
    | "change-layout";
};

type PaneItem = {
  type: "pane";
  key: string;
  title: string;
  subtitle: string;
  paneId: string;
  paneIndex: number;
  label: string | null;
  sessionId: SessionId | null;
  empty: boolean;
};

type SurfaceItem = {
  type: "surface";
  key: string;
  title: string;
  subtitle: string;
  surfaceId: number;
  connectionId: string;
  focused: boolean;
};

type SwitcherItem =
  | LayoutItem
  | SessionItem
  | PaneItem
  | ActionItem
  | SurfaceItem;
type SwitcherSection = {
  title: string;
  items: SwitcherItem[];
};

function itemKey(item: SwitcherItem): string {
  return item.key;
}

function isCustomLayoutQuery(query: string): boolean {
  return /^\s*([^:]*:\s*)?(line|col|tabs)\s*\(/i.test(query);
}

function parseLayoutQuery(query: string): { name: string | null; dsl: string } {
  const match = query.match(/^\s*([^:(]+?)\s*:\s*((line|col|tabs)\s*\(.*)/i);
  if (match) return { name: match[1].trim(), dsl: match[2].trim() };
  return { name: null, dsl: query.trim() };
}

function PaneGlyph(props: { empty: boolean; fg: string; dimFg: string }) {
  return (
    <svg
      viewBox="0 0 24 24"
      width="24"
      height="24"
      fill="none"
      aria-hidden="true"
    >
      <rect x="4.5" y="4.5" width="15" height="15" stroke={props.dimFg} />
      <path d="M4.5 10.5h15" stroke={props.dimFg} />
      <path d="M10.5 4.5v15" stroke={props.dimFg} />
      <Show
        when={props.empty}
        fallback={
          <rect
            x="6.5"
            y="6.5"
            width="7"
            height="7"
            fill={props.fg}
            opacity="0.75"
          />
        }
      >
        <path d="M12 8v8" stroke={props.fg} />
        <path d="M8 12h8" stroke={props.fg} />
      </Show>
    </svg>
  );
}

function ActionGlyph(props: {
  action: ActionItem["action"];
  fg: string;
  dimFg: string;
}) {
  const icon = (): JSX.Element => {
    switch (props.action) {
      case "new-terminal":
        return (
          <svg
            viewBox="0 0 24 24"
            width="24"
            height="24"
            fill="none"
            aria-hidden="true"
          >
            <rect x="3.5" y="5.5" width="17" height="13" stroke={props.dimFg} />
            <path d="M7 10l2.5 2L7 14" stroke={props.fg} />
            <path d="M11.5 14h3.5" stroke={props.fg} />
            <path d="M18 7.5v5" stroke={props.fg} />
            <path d="M15.5 10h5" stroke={props.fg} />
          </svg>
        );
      case "change-layout":
        return (
          <svg
            viewBox="0 0 24 24"
            width="24"
            height="24"
            fill="none"
            aria-hidden="true"
          >
            <rect x="4.5" y="4.5" width="15" height="15" stroke={props.dimFg} />
            <path d="M11 4.5v15" stroke={props.dimFg} />
            <path d="M4.5 12h15" stroke={props.dimFg} />
          </svg>
        );
      case "clear-layout":
        return (
          <svg
            viewBox="0 0 24 24"
            width="24"
            height="24"
            fill="none"
            aria-hidden="true"
          >
            <rect x="4.5" y="4.5" width="15" height="15" stroke={props.dimFg} />
            <path d="M9 9l6 6M15 9l-6 6" stroke={props.fg} />
          </svg>
        );
      case "change-palette":
        return (
          <svg
            viewBox="0 0 24 24"
            width="24"
            height="24"
            fill="none"
            aria-hidden="true"
          >
            <circle cx="12" cy="12" r="7.5" stroke={props.dimFg} />
            <path
              d="M12 4.5A7.5 7.5 0 0 1 12 19.5z"
              fill={props.fg}
              opacity="0.6"
            />
          </svg>
        );
      case "change-font":
        return (
          <svg
            viewBox="0 0 24 24"
            width="24"
            height="24"
            fill="none"
            aria-hidden="true"
          >
            <text
              x="4"
              y="18"
              font-size="14"
              font-weight="bold"
              fill={props.fg}
              font-family="sans-serif"
            >
              A
            </text>
            <text
              x="14"
              y="18"
              font-size="10"
              fill={props.dimFg}
              font-family="sans-serif"
            >
              a
            </text>
          </svg>
        );
      case "clear-local-storage":
        return (
          <svg
            viewBox="0 0 24 24"
            width="24"
            height="24"
            fill="none"
            aria-hidden="true"
          >
            <path d="M7 7h10l-1 12H8L7 7z" stroke={props.dimFg} />
            <path d="M5 7h14" stroke={props.fg} />
            <path d="M10 5h4" stroke={props.fg} />
          </svg>
        );
      default:
        return (
          <svg
            viewBox="0 0 24 24"
            width="24"
            height="24"
            fill="none"
            aria-hidden="true"
          >
            <circle cx="12" cy="12" r="3" fill={props.fg} opacity="0.5" />
          </svg>
        );
    }
  };

  return <>{icon()}</>;
}

function PreviewTerminal(props: {
  sessionId: SessionId;
  palette: TerminalPalette;
}) {
  let containerRef!: HTMLDivElement;
  const [termSize, setTermSize] = createSignal<{
    w: number;
    h: number;
  } | null>(null);

  createEffect(() => {
    // Track sessionId so the effect re-runs when it changes.
    void props.sessionId;

    const container = containerRef;
    if (!container) return;

    const update = () => {
      const canvas = container.querySelector("canvas");
      if (!canvas || canvas.width === 0 || canvas.height === 0) return;
      const cw = container.clientWidth;
      const ch = container.clientHeight;
      const scale = Math.min(cw / canvas.width, ch / canvas.height, 1);
      const w = Math.floor(canvas.width * scale);
      const h = Math.floor(canvas.height * scale);
      setTermSize((prev) =>
        prev && prev.w === w && prev.h === h ? prev : { w, h },
      );
    };

    const obs = new ResizeObserver(update);
    obs.observe(container);
    const mo = new MutationObserver(update);
    mo.observe(container, {
      subtree: true,
      attributes: true,
      attributeFilter: ["width", "height"],
    });
    update();

    onCleanup(() => {
      obs.disconnect();
      mo.disconnect();
    });
  });

  const ts = () => termSize();

  return (
    <div
      ref={containerRef}
      style={{
        flex: 1,
        "min-height": 0,
        overflow: "hidden",
        display: "flex",
        "align-items": "center",
        "justify-content": "center",
        "pointer-events": "none",
        "background-color": `rgb(${props.palette.bg[0]},${props.palette.bg[1]},${props.palette.bg[2]})`,
      }}
    >
      <BlitTerminal
        sessionId={props.sessionId}
        readOnly
        showCursor={false}
        style={
          ts()
            ? { width: `${ts()!.w}px`, height: `${ts()!.h}px` }
            : { width: "100%", height: "100%" }
        }
      />
    </div>
  );
}

function PreviewSurface(props: {
  connectionId: string;
  surfaceId: number;
  theme: ReturnType<typeof themeFor>;
  scale: ReturnType<typeof uiScale>;
}) {
  let containerRef!: HTMLDivElement;
  const [size, setSize] = createSignal<{ w: number; h: number } | null>(null);

  createEffect(() => {
    // Re-run when surfaceId changes so we re-measure.
    void props.surfaceId;

    const container = containerRef;
    if (!container) return;

    const update = () => {
      const canvas = container.querySelector("canvas");
      if (!canvas || canvas.width === 0 || canvas.height === 0) return;
      const cw = container.clientWidth;
      const ch = container.clientHeight;
      const scale = Math.min(cw / canvas.width, ch / canvas.height, 1);
      const w = Math.floor(canvas.width * scale);
      const h = Math.floor(canvas.height * scale);
      setSize((prev) =>
        prev && prev.w === w && prev.h === h ? prev : { w, h },
      );
    };

    const obs = new ResizeObserver(update);
    obs.observe(container);
    const mo = new MutationObserver(update);
    mo.observe(container, {
      subtree: true,
      attributes: true,
      attributeFilter: ["width", "height"],
    });
    update();

    onCleanup(() => {
      obs.disconnect();
      mo.disconnect();
    });
  });

  const s = () => size();

  return (
    <div
      ref={containerRef}
      style={{
        flex: 1,
        "min-height": 0,
        overflow: "hidden",
        display: "flex",
        "align-items": "center",
        "justify-content": "center",
        "pointer-events": "none",
        border: `1px solid ${props.theme.subtleBorder}`,
        "background-color": props.theme.panelBg,
      }}
    >
      <BlitSurfaceView
        connectionId={props.connectionId}
        surfaceId={props.surfaceId}
        style={
          s()
            ? { width: `${s()!.w}px`, height: `${s()!.h}px` }
            : { width: "100%", height: "100%" }
        }
      />
    </div>
  );
}

export function SwitcherOverlay(props: {
  sessions: readonly BlitSession[];
  focusedSessionId: SessionId | null;
  lru: SessionId[];
  palette: TerminalPalette;
  fontFamily?: string;
  fontSize?: number;
  onSelect: (sessionId: SessionId) => void;
  onClose: () => void;
  onCreate: (command?: string) => void;
  activeLayout?: BSPLayout | null;
  layoutAssignments?: BSPAssignments | null;
  onApplyLayout?: (layout: BSPLayout) => void;
  onClearLayout?: () => void;
  onSelectPane?: (
    paneId: string,
    sessionId: SessionId | null,
    command?: string,
  ) => void;
  focusedPaneId?: string | null;
  onMoveToPane?: (sessionId: SessionId, targetPaneId: string) => void;
  recentLayouts?: BSPLayout[];
  presetLayouts?: BSPLayout[];
  onChangeFont?: () => void;
  onChangePalette?: () => void;
  surfaces?: readonly BlitSurface[];
  connectionId?: string;
  connectionLabels?: Map<string, string>;
  multiConnection?: boolean;
  focusedSurfaceId?: number | null;
  onFocusSurface?: (surfaceId: number) => void;
  onMoveSurfaceToPane?: (surfaceId: number, targetPaneId: string) => void;
}) {
  const workspace = createBlitWorkspace();
  const notClosed = createMemo(() =>
    props.sessions.filter((session) => session.state !== "closed"),
  );
  const lruIndex = createMemo(
    () => new Map(props.lru.map((id, index) => [id, index])),
  );
  const visibleSessions = createMemo(() => {
    const isNamed = (session: BlitSession) =>
      session.tag.length > 0 && !/^[0-9a-f-]{8,}$/.test(session.tag);
    return [...notClosed()].sort((left, right) => {
      const leftNamed = isNamed(left) ? 0 : 1;
      const rightNamed = isNamed(right) ? 0 : 1;
      if (leftNamed !== rightNamed) return leftNamed - rightNamed;
      const leftIndex = lruIndex().get(left.id) ?? Infinity;
      const rightIndex = lruIndex().get(right.id) ?? Infinity;
      return leftIndex - rightIndex;
    });
  });

  const dark = () => props.palette.dark;
  const theme = () => themeFor(props.palette);
  const scale = () => uiScale(props.fontSize ?? 13);
  const chrome = () => overlayChromeStyles(theme(), dark(), scale());
  const [query, setQuery] = createSignal("");
  const [searchResults, setSearchResults] = createSignal<
    BlitSearchResult[] | null
  >(null);
  const [selectedIdx, setSelectedIdx] = createSignal(0);
  const [layoutMode, setLayoutMode] = createSignal(false);
  const [killPickerSessionId, setKillPickerSessionId] =
    createSignal<SessionId | null>(null);
  let searchRef!: HTMLInputElement;
  let itemRefs: (HTMLDivElement | null)[] = [];
  let wrapperRef!: HTMLDivElement;
  let previewRef!: HTMLDivElement;
  const [previewTop, setPreviewTop] = createSignal(0);

  const isCommand = () => query().startsWith(">");
  const commandText = () => (isCommand() ? query().slice(1).trim() : "");
  const inlineCmd = () =>
    !isCommand() && query().includes(">")
      ? query()
          .slice(query().indexOf(">") + 1)
          .trim()
      : "";
  const searchPart = () =>
    !isCommand() && query().includes(">")
      ? query().slice(0, query().indexOf(">")).trim()
      : query().trim();
  const searching = () => !isCommand() && searchPart().length > 0;

  createEffect(() => {
    if (!searching()) {
      setSearchResults(null);
      return;
    }

    const part = searchPart();
    let cancelled = false;
    workspace
      .search(part)
      .then((results) => {
        if (!cancelled) setSearchResults(results);
      })
      .catch(() => {
        if (!cancelled) setSearchResults([]);
      });

    onCleanup(() => {
      cancelled = true;
    });
  });

  onMount(() => {
    searchRef?.focus();
  });

  const sessionsById = createMemo(
    () => new Map(visibleSessions().map((session) => [session.id, session])),
  );

  const layoutChoices = createMemo(() => {
    const recent = props.recentLayouts ?? [];
    const presets = props.presetLayouts ?? [];
    const custom =
      searching() && isCustomLayoutQuery(searchPart())
        ? (() => {
            try {
              const { name, dsl } = parseLayoutQuery(searchPart());
              const layout = layoutFromDSL(dsl);
              if (name) layout.name = name;
              return [layout];
            } catch {
              return [];
            }
          })()
        : [];

    if (!searching()) {
      return { recent, presets, custom };
    }

    const needle = searchPart().toLowerCase();
    const matches = (layouts: BSPLayout[]) =>
      layouts.filter(
        (layout) =>
          layout.name.toLowerCase().includes(needle) ||
          layout.dsl.toLowerCase().includes(needle),
      );

    return {
      recent: matches(recent),
      presets: matches(presets),
      custom,
    };
  });

  const paneMatches = createMemo(() => {
    if (!props.activeLayout) return [] as PaneItem[];

    const needle = searchPart().toLowerCase();
    return enumeratePanes(props.activeLayout.root)
      .map((pane, index) => {
        const sessionId = props.layoutAssignments?.assignments[pane.id] ?? null;
        const session = sessionId
          ? (sessionsById().get(sessionId) ?? null)
          : null;
        const label = pane.leaf.tag || null;
        const paneName = label || `Pane ${index + 1}`;
        const assignedName = session ? sessionName(session) : null;
        const subtitle = session
          ? tp("switcher.showsPane", { name: assignedName ?? "" })
          : t("switcher.emptyPane");
        return {
          type: "pane" as const,
          key: `pane:${pane.id}`,
          title: paneName,
          subtitle,
          paneId: pane.id,
          paneIndex: index,
          label,
          sessionId,
          empty: sessionId == null,
        };
      })
      .filter((pane) => {
        if (!searching()) return true;
        return [pane.title, pane.label ?? ""].some((value) =>
          value.toLowerCase().includes(needle),
        );
      });
  });

  const sessionMatches = createMemo(() => {
    if (!searching()) {
      return visibleSessions().map<SessionItem>((session) => ({
        type: "session",
        key: `session:${session.id}`,
        title: sessionName(session),
        subtitle:
          session.command ??
          (session.state === "exited"
            ? t("switcher.exitedTerminal")
            : t("switcher.openTerminal")),
        sessionId: session.id,
        exited: session.state === "exited",
        focused: session.id === props.focusedSessionId,
      }));
    }

    const needle = searchPart().toLowerCase();
    const seen = new Set<SessionId>();
    const matches: SessionItem[] = [];

    for (const session of visibleSessions()) {
      if (
        session.tag.toLowerCase().includes(needle) ||
        (session.title ?? "").toLowerCase().includes(needle) ||
        (session.command ?? "").toLowerCase().includes(needle)
      ) {
        seen.add(session.id);
        matches.push({
          type: "session",
          key: `session:${session.id}`,
          title: sessionName(session),
          subtitle:
            session.command ??
            (session.state === "exited"
              ? t("switcher.exitedTerminal")
              : t("switcher.openTerminal")),
          sessionId: session.id,
          exited: session.state === "exited",
          focused: session.id === props.focusedSessionId,
        });
      }
    }

    for (const result of searchResults() ?? []) {
      if (!sessionsById().has(result.sessionId) || seen.has(result.sessionId))
        continue;
      const session = sessionsById().get(result.sessionId)!;
      seen.add(result.sessionId);
      matches.push({
        type: "session",
        key: `session:${session.id}`,
        title: sessionName(session),
        subtitle:
          session.command ??
          (session.state === "exited"
            ? t("switcher.exitedTerminal")
            : t("switcher.openTerminal")),
        sessionId: session.id,
        exited: session.state === "exited",
        context: result.context,
        source: result.primarySource,
        focused: session.id === props.focusedSessionId,
      });
    }

    return matches;
  });

  const surfaceMatches = createMemo(() => {
    const surfs = props.surfaces ?? [];
    if (surfs.length === 0) return [] as SurfaceItem[];

    const connId = props.connectionId ?? "";
    const needle = searching() ? searchPart().toLowerCase() : "";

    return surfs
      .filter((s) => {
        if (!searching()) return true;
        const name = s.title || s.appId || `Surface ${s.surfaceId}`;
        return name.toLowerCase().includes(needle);
      })
      .map<SurfaceItem>((s) => ({
        type: "surface",
        key: `surface:${s.surfaceId}`,
        title: s.title || s.appId || `Surface ${s.surfaceId}`,
        subtitle: `${s.width}\u00D7${s.height}`,
        surfaceId: s.surfaceId,
        connectionId: connId,
        focused: s.surfaceId === props.focusedSurfaceId,
      }));
  });

  const sections = createMemo<SwitcherSection[]>(() => {
    if (isCommand()) {
      return [
        {
          title: t("switcher.sectionAction"),
          items: [
            {
              type: "action",
              key: "action:new-terminal",
              title: commandText()
                ? tp("switcher.runCommand", { command: commandText() })
                : t("switcher.newTerminal"),
              subtitle: commandText()
                ? t("switcher.createRunning")
                : t("switcher.createInCwd"),
              action: "new-terminal",
            },
          ],
        },
      ];
    }

    const next: SwitcherSection[] = [];

    const customLayouts = layoutChoices().custom.map<LayoutItem>((layout) => ({
      type: "layout",
      key: `layout:custom:${layout.dsl}`,
      title: t("switcher.useTypedLayout"),
      subtitle: layout.dsl,
      layout,
    }));
    const recent = layoutChoices().recent.map<LayoutItem>((layout) => ({
      type: "layout",
      key: `layout:recent:${layout.dsl}`,
      title: layout.name,
      subtitle: layout.dsl,
      layout,
    }));
    const presets = layoutChoices().presets.map<LayoutItem>((layout) => ({
      type: "layout",
      key: `layout:preset:${layout.dsl}`,
      title: layout.name,
      subtitle: layout.dsl,
      layout,
    }));
    if (!layoutMode() && paneMatches().length > 0) {
      next.push({ title: t("switcher.sectionPanes"), items: paneMatches() });
    }
    if (!layoutMode() && sessionMatches().length > 0) {
      if (props.multiConnection && props.connectionLabels) {
        // Group sessions by connection.
        const groups = new Map<string, SessionItem[]>();
        for (const item of sessionMatches()) {
          const session = props.sessions.find((s) => s.id === item.sessionId);
          const connId = session?.connectionId ?? "unknown";
          if (!groups.has(connId)) groups.set(connId, []);
          groups.get(connId)!.push(item);
        }
        for (const [connId, items] of groups) {
          const label = props.connectionLabels.get(connId) ?? connId;
          next.push({
            title: label,
            items,
          });
        }
      } else {
        next.push({
          title: t("switcher.sectionTerminals"),
          items: sessionMatches(),
        });
      }
    }
    if (!layoutMode() && surfaceMatches().length > 0) {
      next.push({
        title: t("switcher.sectionSurfaces"),
        items: surfaceMatches(),
      });
    }

    if (customLayouts.length > 0) {
      next.push({
        title: t("switcher.sectionTypedLayout"),
        items: customLayouts,
      });
    }
    if ((searching() || layoutMode()) && recent.length > 0) {
      next.push({ title: t("switcher.sectionRecentLayouts"), items: recent });
    }
    if ((searching() || layoutMode()) && presets.length > 0) {
      next.push({ title: t("switcher.sectionLayouts"), items: presets });
    }

    const actions: ActionItem[] = [];
    if (props.multiConnection && props.connectionLabels) {
      // One "new terminal" action per connection.
      for (const [connId, label] of props.connectionLabels) {
        actions.push({
          type: "action",
          key: `action:new-terminal:${connId}`,
          title: `${t("switcher.newTerminal")} (${label})`,
          subtitle: t("switcher.createInCwd"),
          action: "new-terminal",
        });
      }
    } else {
      actions.push({
        type: "action",
        key: "action:new-terminal",
        title: t("switcher.newTerminal"),
        subtitle: t("switcher.createInCwd"),
        action: "new-terminal",
      });
    }
    if (props.onChangePalette) {
      actions.push({
        type: "action",
        key: "action:change-palette",
        title: t("switcher.palette"),
        subtitle: t("switcher.switchColorScheme"),
        action: "change-palette",
      });
    }
    if (props.onChangeFont) {
      actions.push({
        type: "action",
        key: "action:change-font",
        title: t("switcher.font"),
        subtitle: t("switcher.switchFont"),
        action: "change-font",
      });
    }
    actions.push({
      type: "action",
      key: "action:change-layout",
      title: t("switcher.layout"),
      subtitle: props.activeLayout
        ? props.activeLayout.dsl
        : t("switcher.chooseLayout"),
      action: "change-layout",
    });
    if (props.activeLayout && props.onClearLayout) {
      actions.push({
        type: "action",
        key: "action:clear-layout",
        title: t("switcher.exitLayout"),
        subtitle: t("switcher.exitLayoutDesc"),
        action: "clear-layout",
      });
    }
    actions.push({
      type: "action",
      key: "action:clear-local-storage",
      title: t("switcher.clearLocalStorage"),
      subtitle: t("switcher.clearLocalStorageDesc"),
      action: "clear-local-storage",
    });
    if (
      !layoutMode() &&
      (!searching() ||
        actions.some((action) =>
          action.title.toLowerCase().includes(searchPart().toLowerCase()),
        ))
    ) {
      next.push({
        title: t("switcher.sectionActions"),
        items: searching()
          ? actions.filter((action) =>
              action.title.toLowerCase().includes(searchPart().toLowerCase()),
            )
          : actions,
      });
    }

    return next.filter((section) => section.items.length > 0);
  });

  const flatItems = createMemo(() =>
    sections().flatMap((section) => section.items),
  );

  // Clamp selected index when flatItems changes.
  createEffect(() => {
    const len = flatItems().length;
    setSelectedIdx((current) => {
      if (len === 0) return 0;
      return Math.min(current, len - 1);
    });
  });

  // Reset selection when query changes.
  createEffect(() => {
    void query();
    setSelectedIdx(0);
    setKillPickerSessionId(null);
  });

  // Scroll selected item into view and position preview panel.
  createEffect(() => {
    const idx = selectedIdx();
    const el = itemRefs[idx];
    el?.scrollIntoView({ block: "nearest" });
    requestAnimationFrame(() => {
      if (!el || !wrapperRef) return;
      const wrapperRect = wrapperRef.getBoundingClientRect();
      const itemRect = el.getBoundingClientRect();
      const previewH = previewRef?.offsetHeight ?? 0;
      const itemCenter = itemRect.top + itemRect.height / 2 - wrapperRect.top;
      const unclamped = itemCenter - previewH / 2;
      setPreviewTop(
        Math.max(0, Math.min(unclamped, wrapperRect.height - previewH)),
      );
    });
  });

  const selectedItem = () => flatItems()[selectedIdx()] ?? null;
  const showPreview = () => {
    const sel = selectedItem();
    return !isCommand() && sel != null && sel.type !== "action";
  };
  const uiFont = () => props.fontFamily ?? "inherit";
  const compact = 0.75;
  const fsXs = () => Math.round(scale().xs * compact);
  const fsSm = () => Math.round(scale().sm * compact);
  const fsMd = () => Math.round(scale().md * compact);
  const fsLg = () => Math.round(scale().lg * compact);
  const fsXl = () => Math.round(scale().xl * compact);
  const cardBg = () => theme().solidPanelBg;
  const railBg = () => theme().solidPanelBg;
  const ctaStyle = (): JSX.CSSProperties => ({
    ...ui.btn,
    opacity: 1,
    "justify-self": "start",
    padding: `${scale().controlY + 1}px ${scale().controlX}px`,
    "background-color": theme().accent,
    color: "#fff",
    border: `1px solid ${theme().accent}`,
    "border-radius": "0",
    "box-shadow": "none",
    "font-size": `${fsSm()}px`,
    "font-weight": 600,
    "letter-spacing": "0",
  });
  const iconSize = () => Math.round(scale().icon * compact);

  function renderItemSquare(item: SwitcherItem, selected: boolean) {
    return (
      <div
        style={{
          width: `${iconSize()}px`,
          height: `${iconSize()}px`,
          "flex-shrink": 0,
          "border-radius": "0",
          border: `1px solid ${selected ? theme().accent : theme().subtleBorder}`,
          "background-color": theme().solidPanelBg,
          display: "flex",
          "align-items": "center",
          "justify-content": "center",
          overflow: "hidden",
          position: "relative",
        }}
      >
        {item.type === "layout" ? (
          <LayoutPreview
            node={item.layout.root}
            width={iconSize()}
            height={iconSize()}
            color={theme().fg}
            bg={theme().bg}
          />
        ) : item.type === "session" ? (
          <BlitTerminal
            sessionId={item.sessionId}
            readOnly
            showCursor={false}
            style={{
              width: `${iconSize()}px`,
              height: `${iconSize()}px`,
              "pointer-events": "none",
              "object-fit": "contain",
              "object-position": "center",
            }}
          />
        ) : item.type === "surface" ? (
          <BlitSurfaceView
            connectionId={(item as SurfaceItem).connectionId}
            surfaceId={(item as SurfaceItem).surfaceId}
            style={{
              width: `${iconSize()}px`,
              height: `${iconSize()}px`,
              "pointer-events": "none",
              overflow: "hidden",
            }}
          />
        ) : item.type === "pane" ? (
          props.activeLayout ? (
            <LayoutPreview
              node={props.activeLayout.root}
              width={iconSize()}
              height={iconSize()}
              color={theme().fg}
              bg={theme().bg}
              highlightIndex={item.paneIndex}
            />
          ) : (
            <PaneGlyph
              empty={item.empty}
              fg={theme().fg}
              dimFg={theme().dimFg}
            />
          )
        ) : (
          <ActionGlyph
            action={item.action}
            fg={theme().fg}
            dimFg={theme().dimFg}
          />
        )}
      </div>
    );
  }

  function activateItem(item: SwitcherItem | null) {
    if (!item) return;
    if (item.type === "layout") {
      const layoutStr =
        item.layout.name !== item.layout.dsl
          ? `${item.layout.name}:${item.layout.dsl}`
          : item.layout.dsl;
      if (query().trim() === layoutStr) {
        props.onApplyLayout?.(item.layout);
      } else {
        setQuery(layoutStr);
        searchRef?.focus();
        searchRef?.select();
      }
      return;
    }
    if (item.type === "pane") {
      const cmdMatch = query().match(/>(.*)/);
      const paneCommand = cmdMatch?.[1]?.trim() || undefined;
      props.onSelectPane?.(item.paneId, item.sessionId, paneCommand);
      return;
    }
    if (item.type === "session") {
      if (props.focusedPaneId && props.onMoveToPane) {
        props.onMoveToPane(item.sessionId, props.focusedPaneId);
      } else {
        props.onSelect(item.sessionId);
      }
      return;
    }
    if (item.type === "surface") {
      if (props.focusedPaneId && props.onMoveSurfaceToPane) {
        props.onMoveSurfaceToPane(item.surfaceId, props.focusedPaneId);
      } else {
        props.onFocusSurface?.(item.surfaceId);
      }
      return;
    }
    if (item.action === "change-layout") {
      setLayoutMode(true);
      if (props.activeLayout) {
        setQuery(
          props.activeLayout.name !== props.activeLayout.dsl
            ? `${props.activeLayout.name}:${props.activeLayout.dsl}`
            : props.activeLayout.dsl,
        );
      } else {
        setQuery("");
      }
      searchRef?.focus();
      searchRef?.select();
      return;
    }
    if (item.action === "clear-layout") {
      props.onClearLayout?.();
      return;
    }
    if (item.action === "change-font") {
      props.onChangeFont?.();
      return;
    }
    if (item.action === "change-palette") {
      props.onChangePalette?.();
      return;
    }
    if (item.action === "clear-local-storage") {
      localStorage.clear();
      location.reload();
      return;
    }
    if (props.focusedPaneId && props.onSelectPane) {
      props.onSelectPane(props.focusedPaneId, null, commandText() || undefined);
    } else {
      props.onCreate(commandText() || undefined);
    }
  }

  function handleKeyDown(event: KeyboardEvent) {
    if (event.key === "ArrowDown") {
      event.preventDefault();
      if (flatItems().length > 0) {
        setSelectedIdx((index) => (index + 1) % flatItems().length);
      }
      return;
    }
    if (event.key === "ArrowUp") {
      event.preventDefault();
      if (flatItems().length > 0) {
        setSelectedIdx(
          (index) => (index - 1 + flatItems().length) % flatItems().length,
        );
      }
      return;
    }
    if (event.key === "Escape" && layoutMode()) {
      event.preventDefault();
      event.stopPropagation();
      setLayoutMode(false);
      setQuery("");
      return;
    }
    if (event.key === "Enter") {
      event.preventDefault();
      activateItem(selectedItem());
      return;
    }
    if (event.key === "Tab" && selectedItem()) {
      event.preventDefault();
      setQuery(selectedItem()!.title + ">");
      return;
    }
    if (
      (event.key === "w" || event.key === "W") &&
      (event.ctrlKey || event.metaKey) &&
      selectedItem()?.type === "session"
    ) {
      event.preventDefault();
      void workspace.closeSession((selectedItem() as SessionItem).sessionId);
    }
  }

  return (
    <OverlayBackdrop
      palette={props.palette}
      label={t("switcher.label")}
      onClose={props.onClose}
      style={{
        background: dark() ? "rgba(0,0,0,0.66)" : "rgba(240,240,240,0.7)",
      }}
    >
      <div
        ref={wrapperRef}
        style={{ position: "relative", "margin-right": sidebarWidth }}
      >
        <OverlayPanel
          palette={props.palette}
          fontSize={props.fontSize}
          style={{
            "background-color": theme().solidPanelBg,
            "font-family": uiFont(),
            "border-radius": "0",
            border: `1px solid ${theme().subtleBorder}`,
            "box-shadow": dark()
              ? "0 18px 60px rgba(0,0,0,0.45)"
              : "0 18px 60px rgba(0,0,0,0.12)",
            padding: `${scale().tightGap}px`,
            overflow: "hidden",
          }}
        >
          {/* Search bar */}
          <div
            style={{
              display: "flex",
              "align-items": "center",
              gap: `${scale().tightGap}px`,
              "margin-bottom": `${scale().tightGap}px`,
            }}
          >
            <input
              ref={searchRef}
              type="text"
              value={query()}
              onInput={(e) => setQuery(e.currentTarget.value)}
              onKeyDown={handleKeyDown}
              placeholder={t("switcher.placeholder")}
              style={{
                ...ui.input,
                flex: 1,
                "min-width": "0",
                padding: `${scale().controlY + 3}px ${scale().controlX + 1}px`,
                "font-size": `${fsMd()}px`,
                "border-radius": "0",
                border: `1px solid ${theme().subtleBorder}`,
                "background-color": railBg(),
                color: theme().fg,
                "box-shadow": "none",
              }}
            />
            <Show when={props.activeLayout}>
              {(al) => (
                <span
                  style={{
                    padding: `${scale().controlY}px ${scale().controlX}px`,
                    border: `1px solid ${theme().subtleBorder}`,
                    "background-color": railBg(),
                    "font-size": `${fsSm()}px`,
                    color: theme().dimFg,
                    "white-space": "nowrap",
                    overflow: "hidden",
                    "text-overflow": "ellipsis",
                    "max-width": "8em",
                  }}
                  title={al().dsl}
                >
                  {al().name === al().dsl
                    ? tp("switcher.paneCount", {
                        count: leafCount(al().root),
                      })
                    : al().name}
                </span>
              )}
            </Show>
            <button
              style={{
                ...chrome().closeButton,
                "border-radius": "0",
                padding: `${scale().controlY}px ${scale().controlX}px`,
                "background-color": railBg(),
                "font-size": `${fsSm()}px`,
              }}
              onClick={props.onClose}
            >
              {t("overlay.close")}
            </button>
          </div>

          {/* Sections list */}
          <div style={{ display: "grid", gap: `${scale().tightGap}px` }}>
            <div
              style={{
                "min-width": "0",
                "max-height": "80vh",
                overflow: "auto",
                display: "grid",
                "align-content": "start",
                gap: `${scale().tightGap}px`,
                "padding-right": "2px",
              }}
            >
              <For each={sections()}>
                {(section) => (
                  <section
                    style={{
                      display: "grid",
                      gap: `${scale().tightGap}px`,
                    }}
                  >
                    <div
                      style={{
                        display: "flex",
                        "justify-content": "space-between",
                        "align-items": "center",
                        gap: `${scale().gap}px`,
                      }}
                    >
                      <div
                        style={{
                          "font-size": `${fsSm()}px`,
                          "font-weight": 700,
                          color: theme().dimFg,
                          "text-transform": "uppercase",
                          "letter-spacing": "0.08em",
                        }}
                      >
                        {section.title}
                      </div>
                      <div
                        style={{
                          "font-size": `${fsSm()}px`,
                          color: theme().dimFg,
                        }}
                      >
                        {section.items.length}
                      </div>
                    </div>
                    <div
                      style={{
                        display: "grid",
                        gap: `${scale().tightGap}px`,
                      }}
                    >
                      <For each={section.items}>
                        {(item) => {
                          const index = () =>
                            flatItems().findIndex(
                              (candidate) =>
                                itemKey(candidate) === itemKey(item),
                            );
                          const selected = () => index() === selectedIdx();
                          return (
                            <div
                              ref={(el) => {
                                itemRefs[index()] = el;
                              }}
                              onClick={() => activateItem(item)}
                              onMouseEnter={() => setSelectedIdx(index())}
                              style={{
                                display: "flex",
                                "align-items": "stretch",
                                gap: `${scale().tightGap}px`,
                                padding: `${scale().tightGap}px`,
                                "border-radius": "0",
                                border: `1px solid ${selected() ? theme().accent : theme().subtleBorder}`,
                                "background-color": selected()
                                  ? theme().selectedBg
                                  : cardBg(),
                                color: "inherit",
                                "text-align": "left",
                                cursor: "pointer",
                                "font-family": "inherit",
                                "box-shadow": "none",
                                transform: "none",
                                transition:
                                  "border-color 120ms ease, background-color 120ms ease",
                              }}
                            >
                              {renderItemSquare(item, selected())}

                              <div
                                style={{
                                  "min-width": "0",
                                  flex: 1,
                                  display: "grid",
                                }}
                              >
                                <div
                                  style={{
                                    display: "flex",
                                    "align-items": "center",
                                    gap: `${scale().tightGap}px`,
                                    "min-width": "0",
                                    "flex-wrap": "wrap",
                                  }}
                                >
                                  <span
                                    style={{
                                      overflow: "hidden",
                                      "text-overflow": "ellipsis",
                                      "white-space": "nowrap",
                                      "font-size": `${fsMd()}px`,
                                      "font-weight": 600,
                                    }}
                                  >
                                    {item.title}
                                  </span>
                                  <Show
                                    when={
                                      (item.type === "session" &&
                                        (item as SessionItem).focused) ||
                                      (item.type === "surface" &&
                                        (item as SurfaceItem).focused)
                                    }
                                  >
                                    <mark style={ui.badge}>
                                      {t("switcher.badgeFocused")}
                                    </mark>
                                  </Show>
                                  <Show
                                    when={
                                      item.type === "pane" &&
                                      (item as PaneItem).empty
                                    }
                                  >
                                    <mark style={ui.badge}>
                                      {t("switcher.badgeEmpty")}
                                    </mark>
                                  </Show>
                                  <Show
                                    when={
                                      item.type === "session" &&
                                      (item as SessionItem).exited
                                    }
                                  >
                                    <mark
                                      style={{
                                        ...ui.badge,
                                        "background-color":
                                          "rgba(255,100,100,0.3)",
                                      }}
                                    >
                                      {t("switcher.badgeExited")}
                                    </mark>
                                  </Show>
                                  <Show
                                    when={
                                      item.type === "layout" &&
                                      props.activeLayout?.dsl ===
                                        (item as LayoutItem).layout.dsl
                                    }
                                  >
                                    <mark style={ui.badge}>
                                      {t("switcher.badgeCurrent")}
                                    </mark>
                                  </Show>
                                </div>
                                <div
                                  style={{
                                    "font-size": `${fsSm()}px`,
                                    color: theme().dimFg,
                                    overflow: "hidden",
                                    "text-overflow": "ellipsis",
                                    "white-space": "nowrap",
                                  }}
                                >
                                  {item.subtitle}
                                  <Show
                                    when={
                                      item.type === "session" &&
                                      (item as SessionItem).source != null
                                    }
                                  >
                                    {" "}
                                    &middot;{" "}
                                    {SOURCE_LABEL[
                                      (item as SessionItem).source!
                                    ] ?? t("switcher.sourceMatch")}
                                  </Show>
                                </div>
                                <Show
                                  when={
                                    item.type === "session" &&
                                    (item as SessionItem).context
                                  }
                                >
                                  <div
                                    style={{
                                      "font-size": `${fsSm()}px`,
                                      color: theme().dimFg,
                                      overflow: "hidden",
                                      "text-overflow": "ellipsis",
                                      "white-space": "nowrap",
                                    }}
                                  >
                                    {(item as SessionItem).context}
                                  </div>
                                </Show>
                              </div>

                              {/* Kill picker / kill button / close button */}
                              <Show
                                when={
                                  item.type === "session" &&
                                  !(item as SessionItem).exited
                                }
                              >
                                <Show
                                  when={
                                    killPickerSessionId() ===
                                    (item as SessionItem).sessionId
                                  }
                                  fallback={
                                    <button
                                      type="button"
                                      title={t("switcher.kill")}
                                      onClick={(e) => {
                                        e.stopPropagation();
                                        setKillPickerSessionId(
                                          (item as SessionItem).sessionId,
                                        );
                                      }}
                                      style={{
                                        background: railBg(),
                                        border: `1px solid ${theme().subtleBorder}`,
                                        color: "inherit",
                                        cursor: "pointer",
                                        opacity: 0.75,
                                        "font-size": `${fsSm()}px`,
                                        padding: "1px 5px",
                                        "font-family": "inherit",
                                        "align-self": "center",
                                        "border-radius": "0",
                                      }}
                                    >
                                      k
                                    </button>
                                  }
                                >
                                  <div
                                    style={{
                                      display: "flex",
                                      gap: "2px",
                                      "align-self": "center",
                                    }}
                                  >
                                    <For
                                      each={
                                        [
                                          ["TERM", 15],
                                          ["KILL", 9],
                                          ["INT", 2],
                                          ["HUP", 1],
                                          ["USR1", 10],
                                          ["USR2", 12],
                                        ] as const
                                      }
                                    >
                                      {([name, sig]) => (
                                        <button
                                          type="button"
                                          title={name}
                                          onClick={(e) => {
                                            e.stopPropagation();
                                            workspace.killSession(
                                              (item as SessionItem).sessionId,
                                              sig,
                                            );
                                            setKillPickerSessionId(null);
                                          }}
                                          style={{
                                            background: railBg(),
                                            border: `1px solid ${theme().subtleBorder}`,
                                            color: "inherit",
                                            cursor: "pointer",
                                            opacity: 0.75,
                                            "font-size": `${fsSm()}px`,
                                            padding: "1px 4px",
                                            "font-family": "inherit",
                                            "border-radius": "0",
                                          }}
                                        >
                                          {name}
                                        </button>
                                      )}
                                    </For>
                                  </div>
                                </Show>
                              </Show>
                              <Show when={item.type === "session"}>
                                <button
                                  type="button"
                                  title={t("switcher.close")}
                                  onClick={(e) => {
                                    e.stopPropagation();
                                    void workspace.closeSession(
                                      (item as SessionItem).sessionId,
                                    );
                                  }}
                                  style={{
                                    background: railBg(),
                                    border: `1px solid ${theme().subtleBorder}`,
                                    color: "inherit",
                                    cursor: "pointer",
                                    opacity: 0.75,
                                    "font-size": `${fsSm()}px`,
                                    padding: "1px 5px",
                                    "font-family": "inherit",
                                    "align-self": "center",
                                    "border-radius": "0",
                                  }}
                                >
                                  x
                                </button>
                              </Show>
                            </div>
                          );
                        }}
                      </For>
                    </div>
                  </section>
                )}
              </For>

              {/* Empty state */}
              <Show when={sections().length === 0}>
                <div
                  style={{
                    display: "grid",
                    gap: `${scale().tightGap}px`,
                    "place-items": "center",
                    "border-radius": "0",
                    border: `1px dashed ${theme().subtleBorder}`,
                    "background-color": railBg(),
                    "text-align": "center",
                    color: theme().dimFg,
                    padding: `${scale().panelPadding}px`,
                  }}
                >
                  <div
                    style={{
                      "font-size": `${fsXl()}px`,
                      color: theme().fg,
                    }}
                  >
                    {t("switcher.noMatches")}
                  </div>
                  <div
                    style={{
                      "font-size": `${fsSm()}px`,
                      "max-width": sidebarWidth,
                    }}
                  >
                    {t("switcher.noMatchesHint")}
                  </div>
                </div>
              </Show>
            </div>
          </div>
        </OverlayPanel>

        {/* Preview panel */}
        <Show when={showPreview() && selectedItem()}>
          {(sel) => (
            <div
              ref={previewRef}
              onClick={(e) => e.stopPropagation()}
              style={{
                position: "absolute",
                left: "100%",
                top: `${previewTop()}px`,
                width: sidebarWidth,
                "max-height": "20em",
                "background-color": theme().solidPanelBg,
                border: `1px solid ${theme().subtleBorder}`,
                "border-left": "none",
                padding: `${scale().tightGap}px`,
                display: "flex",
                "flex-direction": "column",
                gap: `${scale().tightGap}px`,
                "border-radius": "0",
                overflow: "hidden",
              }}
            >
              <Show when={sel().type === "layout"}>
                <div style={{ display: "grid", gap: `${scale().tightGap}px` }}>
                  <div
                    style={{
                      "font-size": `${fsXs()}px`,
                      "text-transform": "uppercase",
                      "letter-spacing": "0.08em",
                      color: theme().dimFg,
                    }}
                  >
                    {t("switcher.previewLayout")}
                  </div>
                  <div
                    style={{
                      "font-size": `${fsLg()}px`,
                      "font-weight": 600,
                    }}
                  >
                    {sel().title}
                  </div>
                  <div
                    style={{
                      "font-size": `${fsSm()}px`,
                      color: theme().dimFg,
                      "line-height": "1.4",
                    }}
                  >
                    {(sel() as LayoutItem).layout.dsl}
                  </div>
                </div>
                <div
                  style={{
                    display: "flex",
                    "align-items": "center",
                    "justify-content": "center",
                    border: `1px solid ${theme().subtleBorder}`,
                    "background-color": theme().panelBg,
                    "border-radius": "0",
                  }}
                >
                  <LayoutPreview
                    node={(sel() as LayoutItem).layout.root}
                    width={160}
                    height={96}
                    color={theme().fg}
                    bg={theme().bg}
                  />
                </div>
                <div
                  style={{
                    display: "flex",
                    gap: `${scale().tightGap}px`,
                    "flex-wrap": "wrap",
                  }}
                >
                  <mark style={ui.badge}>
                    {tp("switcher.paneCount", {
                      count: leafCount((sel() as LayoutItem).layout.root),
                    })}
                  </mark>
                  <Show
                    when={
                      props.activeLayout?.dsl ===
                      (sel() as LayoutItem).layout.dsl
                    }
                  >
                    <mark style={ui.badge}>
                      {t("switcher.badgeCurrentLayout")}
                    </mark>
                  </Show>
                  <Show
                    when={layoutChoices().recent.some(
                      (layout) =>
                        layout.dsl === (sel() as LayoutItem).layout.dsl,
                    )}
                  >
                    <mark style={ui.badge}>{t("switcher.badgeRecent")}</mark>
                  </Show>
                  <Show
                    when={layoutChoices().presets.some(
                      (layout) =>
                        layout.dsl === (sel() as LayoutItem).layout.dsl,
                    )}
                  >
                    <mark style={ui.badge}>{t("switcher.badgeDefault")}</mark>
                  </Show>
                </div>
                <button
                  type="button"
                  onClick={() => activateItem(sel())}
                  style={ctaStyle()}
                >
                  {t("switcher.applyLayout")}
                </button>
              </Show>

              <Show when={sel().type === "pane"}>
                <div style={{ display: "grid", gap: `${scale().tightGap}px` }}>
                  <div
                    style={{
                      "font-size": `${fsXs()}px`,
                      "text-transform": "uppercase",
                      "letter-spacing": "0.08em",
                      color: theme().dimFg,
                    }}
                  >
                    {t("switcher.previewPane")}
                  </div>
                  <div
                    style={{
                      "font-size": `${fsLg()}px`,
                      "font-weight": 600,
                    }}
                  >
                    {sel().title}
                  </div>
                  <div
                    style={{
                      "font-size": `${fsSm()}px`,
                      color: theme().dimFg,
                    }}
                  >
                    {inlineCmd()
                      ? tp("switcher.runInPane", {
                          command: inlineCmd(),
                          pane: sel().title,
                        })
                      : (sel() as PaneItem).empty
                        ? tp("switcher.paneEmpty", {
                            pane: sel().title,
                          })
                        : sel().subtitle}
                  </div>
                </div>
                <Show when={props.activeLayout}>
                  {(al) => (
                    <div
                      style={{
                        border: `1px solid ${theme().subtleBorder}`,
                        display: "flex",
                        "align-items": "center",
                        "justify-content": "center",
                        "background-color": theme().panelBg,
                        "border-radius": "0",
                        padding: `${scale().panelPadding}px`,
                      }}
                    >
                      <LayoutPreview
                        node={al().root}
                        width={160}
                        height={96}
                        color={theme().fg}
                        bg={theme().bg}
                        highlightIndex={(sel() as PaneItem).paneIndex}
                      />
                    </div>
                  )}
                </Show>
                <button
                  type="button"
                  onClick={() => activateItem(sel())}
                  style={ctaStyle()}
                >
                  {inlineCmd()
                    ? tp("switcher.runInlineCmd", { command: inlineCmd() })
                    : t("switcher.selectPane")}
                </button>
              </Show>

              <Show when={sel().type === "session"}>
                <div style={{ display: "grid", gap: `${scale().tightGap}px` }}>
                  <div
                    style={{
                      "font-size": `${fsXs()}px`,
                      "text-transform": "uppercase",
                      "letter-spacing": "0.08em",
                      color: theme().dimFg,
                    }}
                  >
                    {t("switcher.previewTerminal")}
                  </div>
                  <div
                    style={{
                      "font-size": `${fsLg()}px`,
                      "font-weight": 600,
                    }}
                  >
                    {sel().title}
                  </div>
                  <div
                    style={{
                      "font-size": `${fsSm()}px`,
                      color: theme().dimFg,
                      "line-height": "1.4",
                    }}
                  >
                    {sel().subtitle}
                    <Show when={(sel() as SessionItem).source != null}>
                      {" "}
                      &middot;{" "}
                      {SOURCE_LABEL[(sel() as SessionItem).source!] ??
                        t("switcher.sourceMatch")}
                    </Show>
                  </div>
                </div>
                <PreviewTerminal
                  sessionId={(sel() as SessionItem).sessionId}
                  palette={props.palette}
                />
                <Show when={(sel() as SessionItem).context}>
                  <div
                    style={{
                      "font-size": `${fsSm()}px`,
                      color: theme().dimFg,
                    }}
                  >
                    {(sel() as SessionItem).context}
                  </div>
                </Show>
                <button
                  type="button"
                  onClick={() => activateItem(sel())}
                  style={ctaStyle()}
                >
                  {t("switcher.focusTerminal")}
                </button>
              </Show>

              <Show when={sel().type === "surface"}>
                <div style={{ display: "grid", gap: `${scale().tightGap}px` }}>
                  <div
                    style={{
                      "font-size": `${fsXs()}px`,
                      "text-transform": "uppercase",
                      "letter-spacing": "0.08em",
                      color: theme().dimFg,
                    }}
                  >
                    {t("switcher.previewSurface")}
                  </div>
                  <div
                    style={{
                      "font-size": `${fsLg()}px`,
                      "font-weight": 600,
                    }}
                  >
                    {sel().title}
                  </div>
                  <div
                    style={{
                      "font-size": `${fsSm()}px`,
                      color: theme().dimFg,
                      "line-height": "1.4",
                    }}
                  >
                    {sel().subtitle}
                  </div>
                </div>
                <PreviewSurface
                  connectionId={(sel() as SurfaceItem).connectionId}
                  surfaceId={(sel() as SurfaceItem).surfaceId}
                  theme={theme()}
                  scale={scale()}
                />
                <button
                  type="button"
                  onClick={() => activateItem(sel())}
                  style={ctaStyle()}
                >
                  {t("switcher.focusSurface")}
                </button>
              </Show>

              <Show
                when={
                  sel().type !== "layout" &&
                  sel().type !== "pane" &&
                  sel().type !== "session" &&
                  sel().type !== "surface"
                }
              >
                <div
                  style={{
                    display: "grid",
                    "place-items": "center",
                    "text-align": "center",
                    color: theme().dimFg,
                    "border-radius": "0",
                    border: `1px dashed ${theme().subtleBorder}`,
                    "background-color": railBg(),
                    padding: `${scale().panelPadding}px`,
                  }}
                >
                  {t("switcher.selectHint")}
                </div>
              </Show>
            </div>
          )}
        </Show>
      </div>
    </OverlayBackdrop>
  );
}
