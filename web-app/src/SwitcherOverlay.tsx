import {
  useState,
  useCallback,
  useEffect,
  useMemo,
  useRef,
  type KeyboardEvent,
} from "react";
import {
  BlitTerminal,
  SEARCH_SOURCE_SCROLLBACK,
  SEARCH_SOURCE_TITLE,
  SEARCH_SOURCE_VISIBLE,
  useBlitWorkspace,
} from "blit-react";
import type {
  BlitSearchResult,
  BlitSession,
  SessionId,
  TerminalPalette,
} from "blit-react";
import { OverlayBackdrop, OverlayPanel } from "./Overlay";
import { overlayChromeStyles, sessionName, themeFor, ui, uiScale } from "./theme";
import { LayoutPreview } from "./bsp/LayoutPreview";
import {
  enumeratePanes,
  layoutFromDSL,
  type BSPAssignments,
  type BSPLayout,
} from "./bsp/layout";
import { leafCount } from "./bsp/dsl";

const SOURCE_LABEL: Record<number, string> = {
  [SEARCH_SOURCE_TITLE]: "Title",
  [SEARCH_SOURCE_VISIBLE]: "Terminal",
  [SEARCH_SOURCE_SCROLLBACK]: "Backlog",
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
  action: "new-terminal" | "clear-layout" | "clear-local-storage" | "edit-layout";
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

type SwitcherItem = LayoutItem | SessionItem | PaneItem | ActionItem;
type SwitcherSection = {
  title: string;
  items: SwitcherItem[];
};

function itemKey(item: SwitcherItem): string {
  return item.key;
}

function isCustomLayoutQuery(query: string): boolean {
  return /^\s*(line|col|tabs)\s*\(/i.test(query);
}

function PaneGlyph({
  empty,
  fg,
  dimFg,
}: {
  empty: boolean;
  fg: string;
  dimFg: string;
}) {
  return (
    <svg viewBox="0 0 24 24" width="24" height="24" fill="none" aria-hidden="true">
      <rect x="4.5" y="4.5" width="15" height="15" stroke={dimFg} />
      <path d="M4.5 10.5h15" stroke={dimFg} />
      <path d="M10.5 4.5v15" stroke={dimFg} />
      {empty ? (
        <>
          <path d="M12 8v8" stroke={fg} />
          <path d="M8 12h8" stroke={fg} />
        </>
      ) : (
        <rect x="6.5" y="6.5" width="7" height="7" fill={fg} opacity="0.75" />
      )}
    </svg>
  );
}

function ActionGlyph({
  action,
  fg,
  dimFg,
}: {
  action: ActionItem["action"];
  fg: string;
  dimFg: string;
}) {
  if (action === "new-terminal") {
    return (
      <svg viewBox="0 0 24 24" width="24" height="24" fill="none" aria-hidden="true">
        <rect x="3.5" y="5.5" width="17" height="13" stroke={dimFg} />
        <path d="M7 10l2.5 2L7 14" stroke={fg} />
        <path d="M11.5 14h3.5" stroke={fg} />
        <path d="M18 7.5v5" stroke={fg} />
        <path d="M15.5 10h5" stroke={fg} />
      </svg>
    );
  }

  return (
    <svg viewBox="0 0 24 24" width="24" height="24" fill="none" aria-hidden="true">
      <rect x="4.5" y="4.5" width="15" height="15" stroke={dimFg} />
      <path d="M12 4.5v15" stroke={dimFg} />
      <path d="M4.5 12h15" stroke={dimFg} />
      <rect x="7" y="7" width="10" height="10" fill={fg} opacity="0.32" />
    </svg>
  );
}

export function SwitcherOverlay({
  sessions,
  focusedSessionId,
  lru,
  palette,
  fontFamily,
  fontSize,
  onSelect,
  onClose,
  onCreate,
  activeLayout,
  layoutAssignments,
  onApplyLayout,
  onClearLayout,
  onSelectPane,
  recentLayouts,
  presetLayouts,
}: {
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
  onSelectPane?: (paneId: string, sessionId: SessionId | null, command?: string) => void;
  recentLayouts?: BSPLayout[];
  presetLayouts?: BSPLayout[];
}) {
  const workspace = useBlitWorkspace();
  const notClosed = useMemo(
    () => sessions.filter((session) => session.state !== "closed"),
    [sessions],
  );
  const lruIndex = useMemo(
    () => new Map(lru.map((id, index) => [id, index])),
    [lru],
  );
  const visibleSessions = useMemo(() => {
    const isNamed = (session: BlitSession) => (
      session.tag.length > 0 && !/^[0-9a-f-]{8,}$/.test(session.tag)
    );
    return [...notClosed].sort((left, right) => {
      const leftNamed = isNamed(left) ? 0 : 1;
      const rightNamed = isNamed(right) ? 0 : 1;
      if (leftNamed !== rightNamed) return leftNamed - rightNamed;
      const leftIndex = lruIndex.get(left.id) ?? Infinity;
      const rightIndex = lruIndex.get(right.id) ?? Infinity;
      return leftIndex - rightIndex;
    });
  }, [lruIndex, notClosed]);

  const dark = palette.dark;
  const theme = themeFor(palette);
  const scale = uiScale(fontSize ?? 13);
  const chrome = overlayChromeStyles(theme, dark, scale);
  const [query, setQuery] = useState("");
  const [searchResults, setSearchResults] = useState<BlitSearchResult[] | null>(null);
  const [selectedIdx, setSelectedIdx] = useState(0);
  const searchRef = useRef<HTMLInputElement>(null);
  const itemRefs = useRef<(HTMLDivElement | null)[]>([]);

  const isCommand = query.startsWith(">");
  const commandText = isCommand ? query.slice(1).trim() : "";
  const inlineCmd = !isCommand && query.includes(">") ? query.slice(query.indexOf(">") + 1).trim() : "";
  const searchPart = !isCommand && query.includes(">") ? query.slice(0, query.indexOf(">")).trim() : query.trim();
  const searching = !isCommand && searchPart.length > 0;

  useEffect(() => {
    if (!searching) {
      setSearchResults(null);
      return;
    }

    let cancelled = false;
    workspace.search(searchPart).then((results) => {
      if (!cancelled) setSearchResults(results);
    }).catch(() => {
      if (!cancelled) setSearchResults([]);
    });

    return () => {
      cancelled = true;
    };
  }, [searchPart, searching, workspace]);

  useEffect(() => {
    searchRef.current?.focus();
  }, []);

  const sessionsById = useMemo(
    () => new Map(visibleSessions.map((session) => [session.id, session])),
    [visibleSessions],
  );

  const layoutChoices = useMemo(() => {
    const recent = recentLayouts ?? [];
    const presets = presetLayouts ?? [];
    const custom = searching && isCustomLayoutQuery(searchPart)
      ? (() => {
          try {
            return [layoutFromDSL(searchPart)];
          } catch {
            return [];
          }
        })()
      : [];

    if (!searching) {
      return {
        recent,
        presets,
        custom,
      };
    }

    const needle = searchPart.toLowerCase();
    const matches = (layouts: BSPLayout[]) => layouts.filter((layout) => (
      layout.name.toLowerCase().includes(needle) ||
      layout.dsl.toLowerCase().includes(needle)
    ));

    return {
      recent: matches(recent),
      presets: matches(presets),
      custom,
    };
  }, [presetLayouts, query, recentLayouts, searching]);

  const paneMatches = useMemo(() => {
    if (!activeLayout) return [] as PaneItem[];

    const needle = searchPart.toLowerCase();
    return enumeratePanes(activeLayout.root)
      .map((pane, index) => {
        const sessionId = layoutAssignments?.assignments[pane.id] ?? null;
        const session = sessionId ? sessionsById.get(sessionId) ?? null : null;
        const label = pane.leaf.tag || null;
        const paneName = label || `Pane ${index + 1}`;
        const assignedName = session ? sessionName(session) : null;
        const subtitle = session
          ? `Shows ${assignedName}`
          : `Empty \u2014 Tab+>cmd or Enter for shell`;
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
        if (!searching) return true;
        return [
          pane.title,
          pane.subtitle,
          pane.label ?? "",
        ].some((value) => value.toLowerCase().includes(needle));
      });
  }, [activeLayout, layoutAssignments?.assignments, query, searching, sessionsById]);

  const sessionMatches = useMemo(() => {
    if (!searching) {
      return visibleSessions.map<SessionItem>((session) => ({
        type: "session",
        key: `session:${session.id}`,
        title: sessionName(session),
        subtitle: session.state === "exited" ? "Exited terminal" : "Open terminal",
        sessionId: session.id,
        exited: session.state === "exited",
        focused: session.id === focusedSessionId,
      }));
    }

    const needle = searchPart.toLowerCase();
    const seen = new Set<SessionId>();
    const matches: SessionItem[] = [];

    for (const session of visibleSessions) {
      if (
        session.tag.toLowerCase().includes(needle) ||
        (session.title ?? "").toLowerCase().includes(needle)
      ) {
        seen.add(session.id);
        matches.push({
          type: "session",
          key: `session:${session.id}`,
          title: sessionName(session),
          subtitle: session.state === "exited" ? "Exited terminal" : "Open terminal",
          sessionId: session.id,
          exited: session.state === "exited",
          focused: session.id === focusedSessionId,
        });
      }
    }

    for (const result of searchResults ?? []) {
      if (!sessionsById.has(result.sessionId) || seen.has(result.sessionId)) continue;
      const session = sessionsById.get(result.sessionId)!;
      seen.add(result.sessionId);
      matches.push({
        type: "session",
        key: `session:${session.id}`,
        title: sessionName(session),
        subtitle: session.state === "exited" ? "Exited terminal" : "Open terminal",
        sessionId: session.id,
        exited: session.state === "exited",
        context: result.context,
        source: result.primarySource,
        focused: session.id === focusedSessionId,
      });
    }

    return matches;
  }, [focusedSessionId, query, searchResults, searching, sessionsById, visibleSessions]);

  const sections = useMemo<SwitcherSection[]>(() => {
    if (isCommand) {
      return [{
        title: "Action",
        items: [{
          type: "action",
          key: "action:new-terminal",
          title: commandText ? `Run "${commandText}"` : "New terminal",
          subtitle: commandText
            ? "Create a new terminal running this command"
            : "Create a new terminal in the current working directory",
          action: "new-terminal",
        }],
      }];
    }

    const next: SwitcherSection[] = [];
    const customLayouts = layoutChoices.custom.map<LayoutItem>((layout) => ({
      type: "layout",
      key: `layout:custom:${layout.dsl}`,
      title: "Use typed layout",
      subtitle: layout.dsl,
      layout,
    }));
    const recent = layoutChoices.recent.map<LayoutItem>((layout) => ({
      type: "layout",
      key: `layout:recent:${layout.dsl}`,
      title: layout.name,
      subtitle: layout.dsl,
      layout,
    }));
    const presets = layoutChoices.presets.map<LayoutItem>((layout) => ({
      type: "layout",
      key: `layout:preset:${layout.dsl}`,
      title: layout.name,
      subtitle: layout.dsl,
      layout,
    }));

    if (paneMatches.length > 0) {
      next.push({ title: "Panes", items: paneMatches });
    }
    if (customLayouts.length > 0) {
      next.push({ title: "Typed Layout", items: customLayouts });
    }
    if (recent.length > 0) {
      next.push({ title: "Recent Layouts", items: recent });
    }
    if (presets.length > 0) {
      next.push({ title: "Default Layouts", items: presets });
    }
    if (sessionMatches.length > 0) {
      next.push({ title: "Terminals", items: sessionMatches });
    }

    const actions: ActionItem[] = [{
      type: "action",
      key: "action:new-terminal",
      title: "New terminal",
      subtitle: "Create a new terminal in the current working directory",
      action: "new-terminal",
    }];
    if (activeLayout && onClearLayout) {
      actions.push({
        type: "action",
        key: "action:edit-layout",
        title: "Edit current layout",
        subtitle: `Edit "${activeLayout.name}" DSL in a terminal editor`,
        action: "edit-layout",
      });
      actions.push({
        type: "action",
        key: "action:clear-layout",
        title: "Clear current layout",
        subtitle: "Return to a single focused terminal view",
        action: "clear-layout",
      });
    }
    actions.push({
      type: "action",
      key: "action:clear-local-storage",
      title: "Clear local storage",
      subtitle: "Reset all saved settings and reload",
      action: "clear-local-storage",
    });
    if (!searching || actions.some((action) => action.title.toLowerCase().includes(searchPart.toLowerCase()))) {
      next.push({
        title: "Actions",
        items: searching
          ? actions.filter((action) => action.title.toLowerCase().includes(searchPart.toLowerCase()))
          : actions,
      });
    }

    return next.filter((section) => section.items.length > 0);
  }, [
    activeLayout,
    commandText,
    isCommand,
    layoutChoices.custom,
    layoutChoices.presets,
    layoutChoices.recent,
    onClearLayout,
    query,
    searching,
    paneMatches,
    sessionMatches,
  ]);

  const flatItems = useMemo(
    () => sections.flatMap((section) => section.items),
    [sections],
  );

  useEffect(() => {
    setSelectedIdx((current) => {
      if (flatItems.length === 0) return 0;
      return Math.min(current, flatItems.length - 1);
    });
  }, [flatItems.length]);

  useEffect(() => {
    setSelectedIdx(0);
  }, [query]);

  useEffect(() => {
    itemRefs.current[selectedIdx]?.scrollIntoView({ block: "nearest" });
  }, [selectedIdx]);

  const selectedItem = flatItems[selectedIdx] ?? null;
  const uiFont = fontFamily ?? "inherit";
  const compact = 0.75;
  const fsXs = Math.round(scale.xs * compact);
  const fsSm = Math.round(scale.sm * compact);
  const fsMd = Math.round(scale.md * compact);
  const fsLg = Math.round(scale.lg * compact);
  const fsXl = Math.round(scale.xl * compact);
  const cardBg = theme.solidPanelBg;
  const previewBg = theme.solidPanelBg;
  const railBg = theme.solidPanelBg;
  const ctaStyle = {
    ...ui.btn,
    opacity: 1,
    justifySelf: "start" as const,
    padding: `${scale.controlY + 1}px ${scale.controlX}px`,
    backgroundColor: theme.accent,
    color: "#fff",
    border: `1px solid ${theme.accent}`,
    borderRadius: 0,
    boxShadow: "none",
    fontSize: fsSm,
    fontWeight: 600,
    letterSpacing: 0,
  };
  const iconSize = Math.round(scale.icon * compact);

  const renderItemSquare = (item: SwitcherItem, selected: boolean) => (
    <div
      style={{
        width: iconSize,
        height: iconSize,
        flexShrink: 0,
        borderRadius: 0,
        border: `1px solid ${selected ? theme.accent : theme.subtleBorder}`,
        backgroundColor: item.type === "session" || (item.type === "pane" && item.sessionId)
          ? theme.panelBg
          : theme.solidPanelBg,
        display: "flex",
        alignItems: "center",
        justifyContent: "center",
        overflow: "hidden",
        position: "relative",
      }}
    >
      {item.type === "layout" ? (
        <LayoutPreview
          node={item.layout.root}
          width={iconSize}
          height={iconSize}
          color={theme.fg}
          opacity={selected ? 0.82 : 0.56}
        />
      ) : item.type === "session" ? (
        <BlitTerminal
          sessionId={item.sessionId}
          readOnly
          showCursor={false}
          style={{ width: "100%", height: "100%", pointerEvents: "none" }}
        />
      ) : item.type === "pane" ? (
        activeLayout ? (
          <LayoutPreview
            node={activeLayout.root}
            width={iconSize}
            height={iconSize}
            color={theme.fg}
            opacity={selected ? 0.32 : 0.2}
            highlightIndex={item.paneIndex}
            highlightOpacity={selected ? 0.82 : 0.56}
          />
        ) : (
          <PaneGlyph empty={item.empty} fg={theme.fg} dimFg={theme.dimFg} />
        )
      ) : (
        <ActionGlyph action={item.action} fg={theme.fg} dimFg={theme.dimFg} />
      )}
    </div>
  );

  const activateItem = useCallback((item: SwitcherItem | null) => {
    if (!item) return;
    if (item.type === "layout") {
      onApplyLayout?.(item.layout);
      return;
    }
    if (item.type === "pane") {
      const cmdMatch = query.match(/>(.*)/);
      const paneCommand = cmdMatch?.[1]?.trim() || undefined;
      onSelectPane?.(item.paneId, item.sessionId, paneCommand);
      return;
    }
    if (item.type === "session") {
      onSelect(item.sessionId);
      return;
    }
    if (item.action === "edit-layout") {
      if (activeLayout) {
        setQuery(activeLayout.dsl);
        searchRef.current?.focus();
        searchRef.current?.select();
      }
      return;
    }
    if (item.action === "clear-layout") {
      onClearLayout?.();
      return;
    }
    if (item.action === "clear-local-storage") {
      localStorage.clear();
      location.reload();
      return;
    }
    onCreate(commandText || undefined);
  }, [activeLayout, commandText, onApplyLayout, onClearLayout, onCreate, onSelect, onSelectPane, query, setQuery]);

  const handleKeyDown = useCallback((event: KeyboardEvent<HTMLInputElement>) => {
    if (event.key === "ArrowDown") {
      event.preventDefault();
      if (flatItems.length > 0) {
        setSelectedIdx((index) => (index + 1) % flatItems.length);
      }
      return;
    }
    if (event.key === "ArrowUp") {
      event.preventDefault();
      if (flatItems.length > 0) {
        setSelectedIdx((index) => (index - 1 + flatItems.length) % flatItems.length);
      }
      return;
    }
    if (event.key === "Enter") {
      event.preventDefault();
      activateItem(selectedItem);
      return;
    }
    if (event.key === "Tab" && selectedItem) {
      event.preventDefault();
      setQuery(selectedItem.title + ">");
      return;
    }
    if (
      (event.key === "w" || event.key === "W") &&
      (event.ctrlKey || event.metaKey) &&
      selectedItem?.type === "session"
    ) {
      event.preventDefault();
      void workspace.closeSession(selectedItem.sessionId);
    }
  }, [activateItem, flatItems.length, selectedItem, workspace]);

  return (
    <OverlayBackdrop
      palette={palette}
      label="Workspace Switcher"
      onClose={onClose}
      style={{
        background: dark ? "rgba(0,0,0,0.66)" : "rgba(240,240,240,0.7)",
        alignItems: "start",
        paddingTop: "10vh",
      }}
    >
      <OverlayPanel
        palette={palette}
        fontSize={fontSize}
        style={{
          width: "min(94vw, 960px)",
          maxWidth: 960,
          backgroundColor: theme.solidPanelBg,
          fontFamily: uiFont,
          borderRadius: 0,
          border: `1px solid ${theme.subtleBorder}`,
          boxShadow: dark
            ? "0 18px 60px rgba(0,0,0,0.45)"
            : "0 18px 60px rgba(0,0,0,0.12)",
          padding: scale.tightGap,
          overflow: "hidden",
        }}
      >
        <div
          style={{
            display: "flex",
            alignItems: "center",
            gap: scale.tightGap,
            marginBottom: scale.tightGap,
          }}
        >
          <input
            ref={searchRef}
            type="text"
            value={query}
            onChange={(event) => setQuery(event.target.value)}
            onKeyDown={handleKeyDown}
            placeholder="Search layouts, panes, terminals, or type >command"
            style={{
              ...ui.input,
              flex: 1,
              minWidth: 0,
              padding: `${scale.controlY + 3}px ${scale.controlX + 1}px`,
              fontSize: fsMd,
              borderRadius: 0,
              border: `1px solid ${theme.subtleBorder}`,
              backgroundColor: railBg,
              color: theme.fg,
              boxShadow: "none",
            }}
          />
          {activeLayout && (
            <span
              style={{
                padding: `${scale.controlY}px ${scale.controlX}px`,
                border: `1px solid ${theme.subtleBorder}`,
                backgroundColor: railBg,
                fontSize: fsSm,
                color: theme.dimFg,
                whiteSpace: "nowrap",
              }}
            >
              {activeLayout.name}
            </span>
          )}
          <button
            style={{
              ...chrome.closeButton,
              borderRadius: 0,
              padding: `${scale.controlY}px ${scale.controlX}px`,
              backgroundColor: railBg,
              fontSize: fsSm,
            }}
            onClick={onClose}
          >
            Esc
          </button>
        </div>

        <div
          style={{
            display: "grid",
            ...(!isCommand && {
              gridTemplateColumns: "minmax(0, 1.12fr) minmax(320px, 0.88fr)",
            }),
            gap: scale.tightGap,
          }}
        >
          <div
            style={{
              minWidth: 0,
              maxHeight: "60vh",
              overflow: "auto",
              display: "grid",
              gap: scale.tightGap,
              paddingRight: 2,
            }}
          >
            {sections.map((section) => (
              <section
                key={section.title}
                style={{
                  display: "grid",
                  gap: scale.tightGap,
                  padding: scale.tightGap,
                  borderRadius: 0,
                  border: `1px solid ${theme.subtleBorder}`,
                  backgroundColor: railBg,
                }}
              >
                <div
                  style={{
                    display: "flex",
                    justifyContent: "space-between",
                    alignItems: "center",
                    gap: scale.gap,
                  }}
                >
                  <div
                    style={{
                      fontSize: fsSm,
                      fontWeight: 700,
                      color: theme.dimFg,
                      textTransform: "uppercase",
                      letterSpacing: "0.08em",
                    }}
                  >
                    {section.title}
                  </div>
                  <div
                    style={{
                      fontSize: fsSm,
                      color: theme.dimFg,
                    }}
                  >
                    {section.items.length}
                  </div>
                </div>
                <div style={{ display: "grid", gap: scale.tightGap }}>
                  {section.items.map((item) => {
                    const index = flatItems.findIndex((candidate) => itemKey(candidate) === itemKey(item));
                    const selected = index === selectedIdx;
                    return (
                      <div
                        key={item.key}
                        ref={(element) => {
                          itemRefs.current[index] = element;
                        }}
                        onClick={() => activateItem(item)}
                        onMouseEnter={() => setSelectedIdx(index)}
                        style={{
                          display: "flex",
                          alignItems: "stretch",
                          gap: scale.tightGap,
                          padding: scale.tightGap,
                          borderRadius: 0,
                          border: `1px solid ${selected ? theme.accent : theme.subtleBorder}`,
                          backgroundColor: selected ? theme.selectedBg : cardBg,
                          color: "inherit",
                          textAlign: "left",
                          cursor: "pointer",
                          fontFamily: "inherit",
                          boxShadow: "none",
                          transform: "none",
                          transition: "border-color 120ms ease, background-color 120ms ease",
                        }}
                      >
                        {renderItemSquare(item, selected)}

                        <div style={{ minWidth: 0, flex: 1, display: "grid" }}>
                          <div
                            style={{
                              display: "flex",
                              alignItems: "center",
                              gap: scale.tightGap,
                              minWidth: 0,
                              flexWrap: "wrap",
                            }}
                          >
                            <span
                              style={{
                                overflow: "hidden",
                                textOverflow: "ellipsis",
                                whiteSpace: "nowrap",
                                fontSize: fsMd,
                                fontWeight: 600,
                              }}
                            >
                              {item.title}
                            </span>
                            {item.type === "session" && item.focused && (
                              <mark style={ui.badge}>Focused</mark>
                            )}
                            {item.type === "pane" && item.empty && (
                              <mark style={ui.badge}>Empty</mark>
                            )}
                            {item.type === "session" && item.exited && (
                              <mark style={{ ...ui.badge, backgroundColor: "rgba(255,100,100,0.3)" }}>
                                Exited
                              </mark>
                            )}
                            {item.type === "layout" && activeLayout?.dsl === item.layout.dsl && (
                              <mark style={ui.badge}>Current</mark>
                            )}
                          </div>
                          <div
                            style={{
                              fontSize: fsSm,
                              color: theme.dimFg,
                              overflow: "hidden",
                              textOverflow: "ellipsis",
                              whiteSpace: "nowrap",
                            }}
                          >
                            {item.subtitle}
                            {item.type === "session" && item.source != null && (
                              <> · {SOURCE_LABEL[item.source] ?? "Match"}</>
                            )}
                          </div>
                          {item.type === "session" && item.context && (
                            <div
                              style={{
                                fontSize: fsSm,
                                color: theme.dimFg,
                                overflow: "hidden",
                                textOverflow: "ellipsis",
                                whiteSpace: "nowrap",
                              }}
                            >
                              {item.context}
                            </div>
                          )}
                        </div>

                        {item.type === "session" && (
                          <button
                            type="button"
                            title="Close"
                            onClick={(event) => {
                              event.stopPropagation();
                              void workspace.closeSession(item.sessionId);
                            }}
                            style={{
                              background: railBg,
                              border: `1px solid ${theme.subtleBorder}`,
                              color: "inherit",
                              cursor: "pointer",
                              opacity: 0.75,
                              fontSize: fsSm,
                              padding: "1px 5px",
                              fontFamily: "inherit",
                              alignSelf: "center",
                              borderRadius: 0,
                            }}
                          >
                            x
                          </button>
                        )}
                      </div>
                    );
                  })}
                </div>
              </section>
            ))}

            {sections.length === 0 && (
              <div
                style={{
                  display: "grid",
                  gap: scale.tightGap,
                  placeItems: "center",
                  borderRadius: 0,
                  border: `1px dashed ${theme.subtleBorder}`,
                  backgroundColor: railBg,
                  textAlign: "center",
                  color: theme.dimFg,
                  padding: scale.panelPadding,
                }}
              >
                <div style={{ fontSize: fsXl, color: theme.fg }}>No matches</div>
                <div style={{ fontSize: fsSm, maxWidth: 280 }}>
                  Try a broader search, browse the default layouts, or type <code>{">command"}</code> to open a terminal.
                </div>
              </div>
            )}
          </div>

          {!isCommand && <div
            style={{
              minWidth: 0,
              maxHeight: "60vh",
              border: `1px solid ${theme.subtleBorder}`,
              backgroundColor: previewBg,
              padding: scale.tightGap,
              display: "grid",
              gap: scale.tightGap,
              alignContent: "start",
              borderRadius: 0,
              overflow: "auto",
              boxShadow: "none",
            }}
          >
            {selectedItem ? (
              selectedItem.type === "layout" ? (
                <>
                  <div style={{ display: "grid", gap: scale.tightGap }}>
                    <div
                      style={{
                        fontSize: fsXs,
                        textTransform: "uppercase",
                        letterSpacing: "0.08em",
                        color: theme.dimFg,
                      }}
                    >
                      Layout
                    </div>
                    <div style={{ fontSize: fsLg, fontWeight: 600 }}>{selectedItem.title}</div>
                    <div style={{ fontSize: fsSm, color: theme.dimFg, lineHeight: 1.4 }}>{selectedItem.layout.dsl}</div>
                  </div>
                  <div
                    style={{
                      display: "flex",
                      alignItems: "center",
                      justifyContent: "center",
                      border: `1px solid ${theme.subtleBorder}`,
                      backgroundColor: theme.panelBg,
                      borderRadius: 0,
                    }}
                  >
                    <LayoutPreview
                      node={selectedItem.layout.root}
                      width={160}
                      height={96}
                      color={theme.fg}
                      opacity={0.68}
                    />
                  </div>
                  <div style={{ display: "flex", gap: scale.tightGap, flexWrap: "wrap" }}>
                    <mark style={ui.badge}>{leafCount(selectedItem.layout.root)} panes</mark>
                    {activeLayout?.dsl === selectedItem.layout.dsl && <mark style={ui.badge}>Current layout</mark>}
                    {layoutChoices.recent.some((layout) => layout.dsl === selectedItem.layout.dsl) && <mark style={ui.badge}>Recent</mark>}
                    {layoutChoices.presets.some((layout) => layout.dsl === selectedItem.layout.dsl) && <mark style={ui.badge}>Default</mark>}
                  </div>
                  <button
                    type="button"
                    onClick={() => activateItem(selectedItem)}
                    style={ctaStyle}
                  >
                    Apply Layout
                  </button>
                </>
              ) : selectedItem.type === "pane" ? (
                <>
                  <div style={{ display: "grid", gap: scale.tightGap }}>
                    <div
                      style={{
                        fontSize: fsXs,
                        textTransform: "uppercase",
                        letterSpacing: "0.08em",
                        color: theme.dimFg,
                      }}
                    >
                      Pane
                    </div>
                    <div style={{ fontSize: fsLg, fontWeight: 600 }}>{selectedItem.title}</div>
                    <div style={{ fontSize: fsSm, color: theme.dimFg }}>
                      {inlineCmd
                        ? `Run "${inlineCmd}" in ${selectedItem.title}`
                        : selectedItem.empty
                          ? `${selectedItem.title} is empty \u2014 Tab+>cmd or Enter for shell`
                          : selectedItem.subtitle}
                    </div>
                  </div>
                  {activeLayout && (
                    <div
                      style={{
                        border: `1px solid ${theme.subtleBorder}`,
                        display: "flex",
                        alignItems: "center",
                        justifyContent: "center",
                        backgroundColor: theme.panelBg,
                        borderRadius: 0,
                        padding: scale.panelPadding,
                      }}
                    >
                      <LayoutPreview
                        node={activeLayout.root}
                        width={160}
                        height={96}
                        color={theme.fg}
                        opacity={0.2}
                        highlightIndex={selectedItem.paneIndex}
                        highlightOpacity={0.82}
                      />
                    </div>
                  )}
                  <button
                    type="button"
                    onClick={() => activateItem(selectedItem)}
                    style={ctaStyle}
                  >
                    {inlineCmd ? `Run "${inlineCmd}"` : selectedItem.sessionId ? "Focus Pane" : "Open Shell"}
                  </button>
                </>
              ) : selectedItem.type === "session" ? (
                <>
                  <div style={{ display: "grid", gap: scale.tightGap }}>
                    <div
                      style={{
                        fontSize: fsXs,
                        textTransform: "uppercase",
                        letterSpacing: "0.08em",
                        color: theme.dimFg,
                      }}
                    >
                      Terminal
                    </div>
                    <div style={{ fontSize: fsLg, fontWeight: 600 }}>{selectedItem.title}</div>
                    <div style={{ fontSize: fsSm, color: theme.dimFg, lineHeight: 1.4 }}>
                      {selectedItem.subtitle}
                      {selectedItem.source != null && (
                        <> · {SOURCE_LABEL[selectedItem.source] ?? "Match"}</>
                      )}
                    </div>
                  </div>
                  <div
                    style={{
                      border: `1px solid ${theme.subtleBorder}`,
                      overflow: "hidden",
                      borderRadius: 0,
                    }}
                  >
                    <BlitTerminal
                      sessionId={selectedItem.sessionId}
                      readOnly
                      showCursor={false}
                      style={{ width: "100%", height: "100%", pointerEvents: "none" }}
                    />
                  </div>
                  {selectedItem.context && (
                    <div style={{ fontSize: fsSm, color: theme.dimFg }}>{selectedItem.context}</div>
                  )}
                  <button
                    type="button"
                    onClick={() => activateItem(selectedItem)}
                    style={ctaStyle}
                  >
                    Focus Terminal
                  </button>
                </>
              ) : (
                <button
                  type="button"
                  onClick={() => activateItem(selectedItem)}
                  style={ctaStyle}
                >
                  {selectedItem.action === "edit-layout" ? "Edit Layout" : selectedItem.action === "clear-layout" ? "Clear Layout" : selectedItem.action === "clear-local-storage" ? "Clear Local Storage" : commandText ? `Run "${commandText}"` : "New Terminal"}
                </button>
              )
            ) : (
              <div
                style={{
                  display: "grid",
                  placeItems: "center",
                  textAlign: "center",
                  color: theme.dimFg,
                  borderRadius: 0,
                  border: `1px dashed ${theme.subtleBorder}`,
                  backgroundColor: railBg,
                  padding: scale.panelPadding,
                }}
              >
                Select a layout, pane, terminal, or action.
              </div>
            )}
          </div>}
        </div>
      </OverlayPanel>
    </OverlayBackdrop>
  );
}
