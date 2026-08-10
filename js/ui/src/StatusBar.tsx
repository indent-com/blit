import { WebPaneNav } from "./WebPaneNav";
import type { WebPaneHandle } from "./WebPane";
import { Show, For, createMemo, type JSX } from "solid-js";
import { onMount, onCleanup } from "solid-js";
import type {
  BlitSession,
  BlitSurface,
  BlitConnectionSnapshot,
  ConnectionStatus,
  TerminalPalette,
  SurfaceFrameHistory,
  NumberRing,
  LinkHover,
} from "@blit-sh/core";
import { formatBw } from "./createMetrics";
import type {
  Metrics,
  RenderSampleRing,
  NetSampleRing,
} from "./createMetrics";
import {
  sessionName,
  sessionPrefix,
  surfaceName,
  surfacePrefix,
  themeFor,
  ui,
  uiScale,
  z,
} from "./theme";
import type { Theme, UIScale } from "./theme";
import { t, tp } from "./i18n";
import {
  activeEditor,
  type CommitController,
  type DiffController,
  type EditorController,
  type PreviewController,
} from "./ide/activeEditor";
import { lineWrap, toggleLineWrap } from "./ide/editorPrefs";
import { FileViewSwitcher } from "./ide/FileViewSwitcher";

type SurfaceDebugInfo = {
  surfaceId: number;
  codec: string;
  encoder: string;
  width: number;
  height: number;
  frameSamples: SurfaceFrameHistory;
  outputSamples: NumberRing;
  dropped: number;
  errors: number;
  queueDepth: number;
  clockRttMs: number | null;
};

type DebugStats = {
  displayFps: number;
  rendererBackend: string;
  pendingApplied: number;
  ackAhead: number;
  applyMs: number;
  mouseMode: number;
  mouseEncoding: number;
  terminals: number;
  staleTerminals: number;
  subscribed: number;
  pendingFrameQueues: number;
  totalPendingFrames: number;
  surfaces?: SurfaceDebugInfo[];
} | null;

function rgba([r, g, b]: [number, number, number], alpha: number): string {
  return `rgba(${r}, ${g}, ${b}, ${alpha})`;
}

export function StatusBar(props: {
  sessions: readonly BlitSession[];
  surfaceCount: number;
  /** Editor/diff/commit tiles on screen. */
  tileCount: number;
  /** Web panes on screen. */
  webCount: number;
  focusedSession: BlitSession | null;
  /** Live cwd of the focused terminal, when it is known to be that
   *  terminal's (the poll keeps its last reading when a pty can't
   *  answer). */
  focusedCwd?: string | null;
  focusedSurface: BlitSurface | null;
  /** Hyperlink under the pointer in the focused terminal, if any. Shown in
   *  place of the identity for as long as the pointer rests on it. */
  hoveredLink?: LinkHover | null;
  connectionLabels?: Map<string, string>;
  connections: readonly BlitConnectionSnapshot[];
  gatewayStatus: "connecting" | "connected" | "unavailable";
  status: ConnectionStatus;
  /** Opens the remotes overlay; absent when the shell has no remotes to
   *  manage (an embedded share), which hides the chip's click affordance. */
  onRemotes?: () => void;
  metrics: Metrics;
  palette: TerminalPalette;
  fontSize: number;
  fontFamily: string;
  fontLoading: boolean;
  debug: boolean;
  toggleDebug: () => void;
  previewPanelOpen: boolean;
  onPreviewPanel: () => void;
  leftDockOpen: boolean;
  onToggleLeftDock: () => void;
  /** Navigation for the focused web pane, or null when none is focused. */
  webPane: {
    handle: WebPaneHandle;
    url: string;
    /** Point the focused pane at a different origin. */
    retarget: (url: string) => void;
  } | null;
  debugStats: DebugStats;
  timeline: RenderSampleRing;
  net: NetSampleRing;
  onSwitcher: () => void;
  onPalette: () => void;
  onFont: () => void;
  audioMuted: boolean;
  audioAvailable: boolean;
  hasSurfaces: boolean;
  onMedia: () => void;
  isMobileTouch?: boolean;
  keyboardOpen?: boolean;
  onToggleKeyboard?: () => void;
}) {
  const theme = () => themeFor(props.palette);
  const scale = () => uiScale(props.fontSize);
  const visible = () =>
    props.sessions.filter((session) => session.state !== "closed");
  const exited = () =>
    visible().filter((session) => session.state === "exited").length;
  const buttonStyle = (): JSX.CSSProperties => ({
    ...ui.btn,
    "font-size": `${scale().md}px`,
    opacity: 1,
  });
  // Icon buttons get a doubled glyph on touch devices, where the pointer
  // is a finger and the status bar's usual density is untappable.
  const touch = () => props.isMobileTouch ?? false;
  const iconSize = () => scale().md * (touch() ? 2 : 1);
  const iconButtonStyle = (): JSX.CSSProperties => ({
    ...buttonStyle(),
    "font-size": `${iconSize()}px`,
  });
  const dotSize = () => (touch() ? 14 : 7);

  // Count connections by bucket: ok / busy / bad
  const connCounts = () => {
    let ok = 0,
      busy = 0,
      bad = 0;
    for (const c of props.connections) {
      if (c.status === "connected") ok++;
      else if (c.status === "connecting" || c.status === "authenticating")
        busy++;
      else bad++;
    }
    return { ok, busy, bad };
  };

  // Worst status across all connections (for aria)
  const worstStatus = () => props.status;

  return (
    <>
      <button
        onClick={props.onSwitcher}
        style={iconButtonStyle()}
        title={t("statusbar.menuTitle")}
      >
        {tp("statusbar.terminals", { count: visible().length })}
        <Show when={exited() > 0}>
          <span style={{ opacity: 0.65 }}>
            {tp("statusbar.exited", { count: exited() })}
          </span>
        </Show>
        <Show when={props.surfaceCount > 0}>
          <span>
            {"\u00B7"}
            {tp("statusbar.surfaces", { count: props.surfaceCount })}
          </span>
        </Show>
        <Show when={props.tileCount > 0}>
          <span>
            {"\u00B7"}
            {tp("statusbar.tiles", { count: props.tileCount })}
          </span>
        </Show>
        <Show when={props.webCount > 0}>
          <span>
            {"\u00B7"}
            {tp("statusbar.webPanes", { count: props.webCount })}
          </span>
        </Show>
      </button>
      <span
        style={{
          flex: 1,
          "min-width": 0,
          display: "flex",
          "align-items": "center",
          gap: `${scale().tightGap}px`,
          overflow: "hidden",
          "text-overflow": "ellipsis",
          "white-space": "nowrap",
        }}
      >
        {/* A hovered link takes over the elastic middle region, the same way a
            browser's status bubble pre-empts the page it sits on. It is
            transient, so nothing underneath needs to be preserved. */}
        <Show
          when={props.hoveredLink}
          fallback={
            <Show
              when={props.webPane}
              fallback={
                <Show
                  when={activeEditor()}
                  keyed
                  fallback={
                    <Show
                      when={props.focusedSession}
                      fallback={
                        <Show when={props.focusedSurface}>
                          {(surface) => {
                            const label = () =>
                              props.connectionLabels?.get(
                                surface().connectionId,
                              ) ?? null;
                            const prefix = () =>
                              surfacePrefix(surface(), label());
                            return (
                              <>
                                <span style={{ opacity: 0.5 }}>{prefix()}</span>
                                {" \u203A "}
                                {surfaceName(surface())}
                              </>
                            );
                          }}
                        </Show>
                      }
                    >
                      {(session) => {
                        const label = () =>
                          props.connectionLabels?.get(session().connectionId) ??
                          null;
                        const prefix = () => sessionPrefix(session(), label());
                        return (
                          <>
                            <span style={{ opacity: 0.5 }}>{prefix()}</span>
                            {" \u203A "}
                            <span style={{ "flex-shrink": 0 }}>
                              {sessionName(session())}
                            </span>
                            {/* The cwd is the first thing to give up when the bar
                        runs out of room, and it truncates from the left:
                        the tail of a path identifies it, the leading
                        /Users/... does not. */}
                            <Show when={props.focusedCwd}>
                              {(cwd) => (
                                <span
                                  title={cwd()}
                                  style={{
                                    "min-width": 0,
                                    overflow: "hidden",
                                    "white-space": "nowrap",
                                    direction: "rtl",
                                    "text-overflow": "ellipsis",
                                    opacity: 0.5,
                                  }}
                                >
                                  {/* Isolated so the RTL truncation above cannot
                              reorder the path's own punctuation. */}
                                  <bdi>{cwd()}</bdi>
                                </span>
                              )}
                            </Show>
                          </>
                        );
                      }}
                    </Show>
                  }
                >
                  {(ed) => {
                    const label = () =>
                      props.connectionLabels?.get(ed.connectionId) ?? null;
                    return ed.kind === "diff" ? (
                      <DiffIdentity
                        d={ed}
                        label={label()}
                        theme={theme()}
                        scale={scale()}
                        fontFamily={props.fontFamily}
                        fontSize={props.fontSize}
                      />
                    ) : ed.kind === "commit" ? (
                      <CommitIdentity c={ed} label={label()} theme={theme()} />
                    ) : ed.kind === "preview" ? (
                      <PreviewIdentity
                        p={ed}
                        label={label()}
                        theme={theme()}
                        fontFamily={props.fontFamily}
                        fontSize={props.fontSize}
                      />
                    ) : (
                      <EditorIdentity
                        ed={ed}
                        label={label()}
                        theme={theme()}
                        scale={scale()}
                        fontFamily={props.fontFamily}
                        fontSize={props.fontSize}
                      />
                    );
                  }}
                </Show>
              }
            >
              {(web) => (
                <WebPaneNav
                  handle={web().handle}
                  url={web().url}
                  onRetarget={web().retarget}
                  fontSize={props.fontSize}
                />
              )}
            </Show>
          }
        >
          {(hover) => (
            <LinkPreview hover={hover()} theme={theme()} scale={scale()} />
          )}
        </Show>
      </span>
      <Show when={activeEditor()} keyed>
        {(ed) =>
          ed.kind === "diff" ? (
            <DiffActions d={ed} scale={scale()} theme={theme()} />
          ) : ed.kind === "commit" ? (
            <ViewModeButton
              viewMode={ed.viewMode}
              toggle={ed.toggleViewMode}
              scale={scale()}
            />
          ) : ed.kind === "preview" ? (
            // Nothing to act on: no save, no diff mode, no LSP.
            <></>
          ) : (
            <EditorActions ed={ed} scale={scale()} theme={theme()} />
          )
        }
      </Show>
      <button
        onClick={props.toggleDebug}
        style={{ ...iconButtonStyle(), opacity: props.debug ? 1 : 0.3 }}
        title={t("statusbar.debugStats")}
      >
        {"\u25C6"}
      </button>
      <Show when={props.audioAvailable || props.hasSurfaces}>
        <button
          onClick={props.onMedia}
          style={{
            ...iconButtonStyle(),
            opacity: !props.audioAvailable || props.audioMuted ? 0.5 : 1,
          }}
          title="Media settings"
        >
          {"\u266A"}
        </button>
      </Show>
      <button
        onClick={props.onToggleLeftDock}
        style={{
          ...iconButtonStyle(),
          opacity: props.leftDockOpen ? 1 : 0.3,
        }}
        title="Toggle IDE dock (Ctrl+Shift+E)"
      >
        {"\u25E7"}
      </button>
      <button
        onClick={props.onPreviewPanel}
        style={{
          ...iconButtonStyle(),
          opacity: props.previewPanelOpen ? 1 : 0.3,
        }}
        title={t("statusbar.previewPanel")}
      >
        {"\u25E8"}
      </button>
      <button
        onClick={props.onPalette}
        style={iconButtonStyle()}
        title={tp("statusbar.paletteTitle", { name: props.palette.name })}
      >
        {props.palette.dark ? "\u25D1" : "\u25D0"}
      </button>
      <button
        onClick={props.onFont}
        style={iconButtonStyle()}
        title={t("statusbar.fontTitle")}
      >
        <Show
          when={!props.fontLoading}
          fallback={
            <span
              style={{
                opacity: 0.5,
                "font-size": `${scale().xs}px`,
              }}
            >
              {t("statusbar.loadingFont")}
            </span>
          }
        >
          Aa
        </Show>
      </button>

      {/* Keyboard toggle — mobile only */}
      <Show when={props.isMobileTouch}>
        <button
          onClick={props.onToggleKeyboard}
          style={{
            ...buttonStyle(),
            opacity: props.keyboardOpen ? 1 : 0.5,
          }}
          title={
            props.keyboardOpen
              ? t("statusbar.hideKeyboard")
              : t("statusbar.showKeyboard")
          }
        >
          <svg
            width="32"
            height="32"
            viewBox="0 0 16 16"
            fill="none"
            stroke="currentColor"
            stroke-width="1.2"
            stroke-linecap="round"
            stroke-linejoin="round"
            style={{ display: "block" }}
          >
            <rect x="1" y="3" width="14" height="10" rx="1.5" />
            <line x1="4" y1="6" x2="5" y2="6" />
            <line x1="7.5" y1="6" x2="8.5" y2="6" />
            <line x1="11" y1="6" x2="12" y2="6" />
            <line x1="4" y1="9" x2="5" y2="9" />
            <line x1="11" y1="9" x2="12" y2="9" />
            <line x1="7" y1="9" x2="9" y2="9" />
          </svg>
        </button>
      </Show>

      {/* Connection status indicator — opens remotes overlay when the
          shell has one to open */}
      <button
        role="status"
        aria-label={worstStatus()}
        onClick={props.onRemotes}
        disabled={!props.onRemotes}
        style={{
          ...iconButtonStyle(),
          display: "flex",
          "align-items": "center",
          gap: "3px",
          cursor: props.onRemotes ? "pointer" : "default",
        }}
      >
        <Show when={connCounts().ok > 0}>
          <ConnectionDot
            color={theme().success}
            count={connCounts().ok}
            total={props.connections.length}
            size={dotSize()}
          />
        </Show>
        <Show when={connCounts().busy > 0}>
          <ConnectionDot
            color={theme().warning}
            count={connCounts().busy}
            total={props.connections.length}
            size={dotSize()}
          />
        </Show>
        <Show when={connCounts().bad > 0}>
          <ConnectionDot
            color={theme().error}
            count={connCounts().bad}
            total={props.connections.length}
            size={dotSize()}
          />
        </Show>
        {/* Gateway dot — shown when no blit connections exist and the
            gateway itself is still connecting or unreachable. */}
        <Show
          when={
            props.connections.length === 0 &&
            props.gatewayStatus === "connecting"
          }
        >
          <ConnectionDot
            color={theme().warning}
            count={1}
            total={1}
            size={dotSize()}
          />
        </Show>
        <Show
          when={
            props.connections.length === 0 &&
            props.gatewayStatus === "unavailable"
          }
        >
          <ConnectionDot
            color={theme().error}
            count={1}
            total={1}
            size={dotSize()}
          />
        </Show>
      </button>

      <Show when={props.debug}>
        <DebugPanel
          metrics={props.metrics}
          debugStats={props.debugStats}
          palette={props.palette}
          fontSize={props.fontSize}
          timeline={props.timeline}
          net={props.net}
          focusedSurfaceId={props.focusedSurface?.surfaceId ?? null}
        />
      </Show>
    </>
  );
}

/** Focused diff's identity: filename + the view switcher (or a plain
 *  comparison label when the tile cannot switch views). */
/**
 * Where a hovered hyperlink actually goes.
 *
 * Renders `assessment.display`, never `assessment.raw` — the display form has
 * had invisible and text-reordering codepoints escaped to `<U+XXXX>`, and this
 * preview exists precisely to defeat a target that misrepresents itself.
 *
 * Two rules make the rendering itself trustworthy:
 *
 * - `direction: ltr` + `unicode-bidi: bidi-override` pins visual order to byte
 *   order. Escaping removes bidi *controls*, but RTL letters in a path would
 *   still be reordered by the bidi algorithm; overriding it means the host you
 *   read is the host you would visit.
 * - Overflow is truncated from the right, keeping the scheme and host — the
 *   parts that decide whether following the link is safe — always visible.
 */
function LinkPreview(props: {
  hover: LinkHover;
  theme: Theme;
  scale: UIScale;
}) {
  const verdict = () => props.hover.assessment.verdict;
  const color = () =>
    verdict() === "deny"
      ? props.theme.errorText
      : verdict() === "confirm"
        ? props.theme.warning
        : props.theme.dimFg;

  return (
    <>
      <span
        style={{
          color: color(),
          "font-size": `${props.scale.sm}px`,
          "flex-shrink": 0,
        }}
      >
        {verdict() === "deny" ? "✕" : "↗"}
      </span>
      <span
        title={props.hover.assessment.display}
        style={{
          color: color(),
          "font-size": `${props.scale.sm}px`,
          "font-family": "monospace",
          "min-width": 0,
          overflow: "hidden",
          "text-overflow": "ellipsis",
          "white-space": "nowrap",
          direction: "ltr",
          "unicode-bidi": "bidi-override",
        }}
      >
        {props.hover.assessment.display}
      </span>
    </>
  );
}

function DiffIdentity(props: {
  d: DiffController;
  label: string | null;
  theme: Theme;
  scale: UIScale;
  fontFamily: string;
  fontSize: number;
}) {
  return (
    <>
      <PathIdentity
        connectionId={props.d.connectionId}
        label={props.label}
        path={props.d.path}
        theme={props.theme}
      />
      <Show
        when={props.d.onOpenTile}
        fallback={
          <span style={{ color: props.theme.dimFg, "flex-shrink": 0 }}>
            ◇ {props.d.sideLabel}
          </span>
        }
      >
        {(open) => (
          <FileViewSwitcher
            current={props.d.side}
            connectionId={props.d.connectionId}
            path={props.d.path}
            onOpenTile={open()}
            theme={props.theme}
            fontFamily={props.fontFamily}
            fontSize={props.fontSize}
          />
        )}
      </Show>
    </>
  );
}

/**
 * A tile's full location: `remote › /abs/path`, directory dimmed and
 * basename bold.
 *
 * Truncates from the left — the tail of a path identifies it, the leading
 * `/Users/...` does not — via an RTL block whose content is `bdi`-isolated
 * so the path's own text still reads left to right.
 */
function PathIdentity(props: {
  connectionId: string;
  label: string | null;
  path: string;
  theme: Theme;
}) {
  const cut = () => {
    const i = props.path.lastIndexOf("/");
    return i < 0
      ? { dir: "", base: props.path }
      : { dir: props.path.slice(0, i + 1), base: props.path.slice(i + 1) };
  };
  return (
    <>
      <span style={{ opacity: 0.5, "flex-shrink": 0 }}>
        {props.label ?? props.connectionId}
      </span>
      {" › "}
      <span
        title={props.path}
        style={{
          "min-width": 0,
          overflow: "hidden",
          "white-space": "nowrap",
          direction: "rtl",
          "text-overflow": "ellipsis",
        }}
      >
        <bdi>
          <span style={{ opacity: 0.6 }}>{cut().dir}</span>
          <b style={{ color: props.theme.fg }}>{cut().base}</b>
        </bdi>
      </span>
    </>
  );
}

/** Focused commit's identity: repo location, abbreviated oid, subject. */
function CommitIdentity(props: {
  c: CommitController;
  label: string | null;
  theme: Theme;
}) {
  return (
    <>
      <PathIdentity
        connectionId={props.c.connectionId}
        label={props.label}
        path={props.c.repoPath}
        theme={props.theme}
      />
      <b style={{ color: props.theme.warning, "flex-shrink": 0 }}>
        {props.c.short}
      </b>
      <span
        style={{
          overflow: "hidden",
          "text-overflow": "ellipsis",
          "white-space": "nowrap",
        }}
      >
        {props.c.subject}
      </span>
    </>
  );
}

/** The unified ⇄ side-by-side toggle, shared by diff and commit tiles. */
function ViewModeButton(props: {
  viewMode: () => "unified" | "split";
  toggle: () => void;
  scale: UIScale;
}) {
  return (
    <button
      style={{ ...ui.btn, "font-size": `${props.scale.md}px` }}
      onClick={() => props.toggle()}
      title={
        props.viewMode() === "unified"
          ? "Switch to side-by-side"
          : "Switch to unified"
      }
    >
      {props.viewMode() === "unified" ? "⊟ Unified" : "⊠ Split"}
    </button>
  );
}

/** Focused diff's actions: the unified ⇄ side-by-side toggle. */
function DiffActions(props: {
  d: DiffController;
  scale: UIScale;
  theme: Theme;
}) {
  return (
    <ViewModeButton
      viewMode={props.d.viewMode}
      toggle={props.d.toggleViewMode}
      scale={props.scale}
    />
  );
}

/** Focused editor's identity in the status bar: filename, view switcher, and
 *  dirty / conflict / lsp state. Reads the controller's accessors reactively. */
/** A preview's identity: the path and the view switcher, nothing else.
 *  There is no dirty flag or banner because a preview cannot be edited. */
function PreviewIdentity(props: {
  p: PreviewController;
  label: string | null;
  theme: Theme;
  fontFamily: string;
  fontSize: number;
}) {
  return (
    <>
      <PathIdentity
        connectionId={props.p.connectionId}
        label={props.label}
        path={props.p.path}
        theme={props.theme}
      />
      <Show when={props.p.onOpenTile}>
        {(open) => (
          <FileViewSwitcher
            current="preview"
            connectionId={props.p.connectionId}
            path={props.p.path}
            onOpenTile={open()}
            theme={props.theme}
            fontFamily={props.fontFamily}
            fontSize={props.fontSize}
          />
        )}
      </Show>
    </>
  );
}

function EditorIdentity(props: {
  ed: EditorController;
  label: string | null;
  theme: Theme;
  scale: UIScale;
  fontFamily: string;
  fontSize: number;
}) {
  return (
    <>
      <PathIdentity
        connectionId={props.ed.connectionId}
        label={props.label}
        path={props.ed.path}
        theme={props.theme}
      />
      <Show when={props.ed.onOpenTile}>
        {(open) => (
          <FileViewSwitcher
            current="editor"
            connectionId={props.ed.connectionId}
            path={props.ed.path}
            onOpenTile={open()}
            theme={props.theme}
            fontFamily={props.fontFamily}
            fontSize={props.fontSize}
          />
        )}
      </Show>
      <Show when={props.ed.dirty()}>
        <span
          style={{ color: props.theme.warning, "flex-shrink": 0 }}
          title="unsaved"
        >
          ●
        </span>
      </Show>
      <Show when={props.ed.banner()}>
        {(b) => (
          <span
            style={{
              "flex-shrink": 0,
              color:
                b().tone === "err"
                  ? props.theme.errorText
                  : props.theme.warning,
            }}
          >
            {b().text}
          </span>
        )}
      </Show>
      <Show when={props.ed.lspMsg()}>
        <span style={{ color: props.theme.dimFg, "flex-shrink": 0 }}>
          {props.ed.lspMsg()}
        </span>
      </Show>
    </>
  );
}

/** Focused editor's action buttons in the status bar. */
function EditorActions(props: {
  ed: EditorController;
  scale: UIScale;
  theme: Theme;
}) {
  const btn = (): JSX.CSSProperties => ({
    ...ui.btn,
    "font-size": `${props.scale.md}px`,
  });
  // Keep the editor focused when a button is clicked: the mousedown would
  // otherwise blur the editor, which autosaves — turning Discard into a no-op
  // (it would revert to what autosave just wrote) and dropping the cursor Def/
  // Refs act on.
  const noBlur = (e: MouseEvent) => e.preventDefault();
  return (
    <>
      <Show when={props.ed.lspAvailable()}>
        <button
          style={btn()}
          onMouseDown={noBlur}
          onClick={() => props.ed.goToDefinition()}
          title="Go to definition (F12 or ⌘-click)"
        >
          Def
        </button>
        <button
          style={btn()}
          onMouseDown={noBlur}
          onClick={() => props.ed.findReferences()}
          title="Find references (⇧F12)"
        >
          Refs
        </button>
        <button
          style={btn()}
          onMouseDown={noBlur}
          onClick={() => props.ed.showOutline()}
          title="Document outline (⌘⇧O)"
        >
          Outline
        </button>
      </Show>
      <button
        style={{ ...btn(), opacity: lineWrap() ? 1 : 0.5 }}
        onMouseDown={noBlur}
        onClick={() => toggleLineWrap()}
        title={lineWrap() ? "Soft wrap on (⌥Z)" : "Soft wrap off (⌥Z)"}
      >
        ⏎
      </button>
      <Show when={props.ed.lspAvailable()}>
        <Show when={!props.ed.readOnly()}>
          <button
            style={btn()}
            onMouseDown={noBlur}
            onClick={() => props.ed.renameSymbol()}
            title="Rename symbol (F2)"
          >
            Rename
          </button>
        </Show>
      </Show>
      <Show when={props.ed.conflicted()}>
        <button
          style={btn()}
          onMouseDown={noBlur}
          onClick={() => props.ed.reload()}
        >
          Reload
        </button>
        <button
          style={btn()}
          onMouseDown={noBlur}
          onClick={() => props.ed.overwrite()}
        >
          Overwrite
        </button>
      </Show>
      <Show when={!props.ed.readOnly()}>
        <Show when={props.ed.dirty() && !props.ed.conflicted()}>
          <button
            style={btn()}
            onMouseDown={noBlur}
            onClick={() => props.ed.discard()}
            title="Discard changes (revert to saved)"
          >
            Discard
          </button>
        </Show>
        <button
          style={{ ...btn(), opacity: props.ed.dirty() ? 1 : 0.5 }}
          onMouseDown={noBlur}
          onClick={() => props.ed.save()}
          title="Save (⌘S)"
        >
          Save
        </button>
      </Show>
    </>
  );
}

function ConnectionDot(props: {
  color: string;
  count: number;
  total: number;
  size: number;
}) {
  return (
    <span
      style={{
        display: "inline-flex",
        "align-items": "center",
        gap: "2px",
      }}
    >
      <span
        style={{
          width: `${props.size}px`,
          height: `${props.size}px`,
          "border-radius": "50%",
          background: props.color,
          display: "inline-block",
          "flex-shrink": 0,
        }}
      />
      <Show when={props.total > 1}>
        <span style={{ "font-variant-numeric": "tabular-nums" }}>
          {props.count}
        </span>
      </Show>
    </span>
  );
}

function DebugPanel(props: {
  metrics: Metrics;
  debugStats: DebugStats;
  palette: TerminalPalette;
  fontSize: number;
  timeline: RenderSampleRing;
  net: NetSampleRing;
  focusedSurfaceId: number | null;
}) {
  const stats = () =>
    props.debugStats ?? {
      displayFps: 0,
      rendererBackend: "none",
      pendingApplied: 0,
      ackAhead: 0,
      applyMs: 0,
      mouseMode: 0,
      mouseEncoding: 0,
      terminals: 0,
      staleTerminals: 0,
      subscribed: 0,
      pendingFrameQueues: 0,
      totalPendingFrames: 0,
    };
  const theme = () => themeFor(props.palette);
  const dark = () => props.palette.dark;
  const scale = () => uiScale(props.fontSize);

  /** The focused surface's debug entry (if any). */
  const focusedSurf = (): SurfaceDebugInfo | undefined => {
    const id = props.focusedSurfaceId;
    if (id == null) return undefined;
    return stats().surfaces?.find((s) => s.surfaceId === id);
  };

  /** Count samples whose timestamp falls within the last `windowMs`. */
  const countRecent = (
    samples: { length: number; time(index: number): number },
    windowMs: number,
  ): number => {
    const cutoff = performance.now() - windowMs;
    let n = 0;
    for (let i = samples.length - 1; i >= 0; i--) {
      if (samples.time(i) < cutoff) break;
      n++;
    }
    return n;
  };

  /** Capture cadence derived from source PTS, independent of bursty recv. */
  const sourceFps = (samples: SurfaceFrameHistory): number => {
    if (samples.length < 2) return 0;
    const last = samples.length - 1;
    if (performance.now() - samples.time(last) > 1000) return 0;
    let first = samples.length - 1;
    while (first > 0 && samples.sourceDelta(last, first - 1) <= 1000) {
      first--;
    }
    const span = samples.sourceDelta(last, first);
    if (span <= 0) return 0;
    return Math.round(((samples.length - first - 1) * 1000) / span);
  };

  const recentMaxGaps = (
    samples: SurfaceFrameHistory,
    windowMs: number,
  ): { src: number; recv: number } => {
    const cutoff = performance.now() - windowMs;
    let first = samples.length - 1;
    while (first > 0 && samples.time(first - 1) >= cutoff) first--;
    let src = 0;
    let recv = 0;
    for (let i = Math.max(1, first); i < samples.length; i++) {
      recv = Math.max(recv, samples.time(i) - samples.time(i - 1));
      const sourceGap = samples.sourceDelta(i, i - 1);
      if (sourceGap >= 0) src = Math.max(src, sourceGap);
    }
    return { src, recv };
  };

  let latencyScratch = new Float64Array(512);
  const percentile = (
    samples: SurfaceFrameHistory,
    cutoff: number,
    metric: "source" | "decode" | "present" | "e2e",
    p: number,
  ): number => {
    if (samples.length > latencyScratch.length)
      latencyScratch = new Float64Array(samples.length);
    let count = 0;
    for (let i = 0; i < samples.length; i++) {
      if (samples.time(i) < cutoff || !Number.isFinite(samples.e2eMs(i)))
        continue;
      let value: number;
      switch (metric) {
        case "source":
          value = samples.sourceToRecvMs(i);
          break;
        case "decode":
          value = samples.decodeMs(i);
          break;
        case "present":
          value = samples.presentMs(i);
          break;
        case "e2e":
          value = samples.e2eMs(i);
          break;
      }
      latencyScratch[count++] = Number.isFinite(value) ? value : 0;
    }
    if (count === 0) return 0;
    latencyScratch.fill(Infinity, count);
    latencyScratch.sort();
    return latencyScratch[Math.min(count - 1, Math.floor(p * count))];
  };

  const surfaceLatency = (surf: SurfaceDebugInfo, displayFps: number) => {
    const cutoff = performance.now() - 2000;
    const samples = surf.frameSamples;
    let completed = 0;
    for (let i = samples.length - 1; i >= 0; i--) {
      if (samples.time(i) < cutoff) break;
      if (Number.isFinite(samples.e2eMs(i))) completed++;
    }
    if (completed === 0) return null;
    const srcRecv = percentile(samples, cutoff, "source", 0.5);
    const net = Math.min(srcRecv, (surf.clockRttMs ?? 0) / 2);
    // rAF/paint-to-photons is not observable for a canvas. Half a refresh
    // is the expected wait to scanout; expose it as an estimate rather than
    // silently calling canvas submission end-to-end.
    const display = displayFps > 0 ? 500 / displayFps : 0;
    return {
      p50: percentile(samples, cutoff, "e2e", 0.5) + display,
      p95: percentile(samples, cutoff, "e2e", 0.95) + display,
      host: Math.max(0, srcRecv - net),
      net,
      decode: percentile(samples, cutoff, "decode", 0.5),
      present: percentile(samples, cutoff, "present", 0.5),
      display,
    };
  };

  const graphSeparator = (): JSX.CSSProperties => ({
    "border-top": `1px solid ${theme().subtleBorder}`,
    "margin-top": "4px",
    "padding-top": "2px",
  });

  return (
    <div
      style={{
        position: "fixed",
        top: 0,
        right: 0,
        "background-color": rgba(props.palette.bg, dark() ? 0.94 : 0.96),
        color: theme().fg,
        "border-left": `1px solid ${theme().subtleBorder}`,
        "border-bottom": `1px solid ${theme().subtleBorder}`,
        padding: "0.4em 0.7em",
        "font-size": `${scale().sm}px`,
        "font-family": "ui-monospace, monospace",
        "line-height": 1.6,
        "z-index": z.debugPanel,
        "white-space": "pre",
        "pointer-events": "none",
      }}
    >
      {/* ── Common rows (always visible) ── */}
      <Row
        label="Terminal renders"
        value={`${props.metrics.fps}/s (${props.metrics.ups} updates/s)`}
      />
      <Row label="Bandwidth" value={formatBw(props.metrics.bw)} />
      <Row
        label="Render"
        value={`${props.metrics.renderMs.toFixed(1)} ms avg, ${props.metrics.maxRenderMs.toFixed(1)} ms max`}
      />
      <Row label="Display Hz" value={stats().displayFps} />
      <Row label="Renderer" value={stats().rendererBackend} />
      <Row label="Backlog" value={stats().pendingApplied} />
      <Row label="Ack ahead" value={stats().ackAhead} />

      {/* ── Surface-focused section ── */}
      <Show when={focusedSurf()} keyed>
        {(surf) => {
          const srcFps = () => sourceFps(surf.frameSamples);
          const recvFps = () => countRecent(surf.frameSamples, 1000);
          const outFps = () => countRecent(surf.outputSamples, 1000);
          const gaps = createMemo(() => recentMaxGaps(surf.frameSamples, 2000));
          const latency = createMemo(() =>
            surfaceLatency(surf, stats().displayFps),
          );
          return (
            <>
              <div
                style={{
                  "border-top": `1px solid ${theme().subtleBorder}`,
                  "margin-top": "4px",
                  "padding-top": "2px",
                  opacity: 0.6,
                  "font-size": `${scale().xs}px`,
                }}
              >
                {`Surface ${surf.surfaceId}`}
              </div>
              <Row
                label="Codec"
                value={surf.encoder || surf.codec || "unknown"}
              />
              <Row
                label="Resolution"
                value={`${surf.width}\u00d7${surf.height}`}
              />
              <Row
                label="Frames"
                value={`${srcFps()} src/s, ${recvFps()} recv/s, ${outFps()} out/s`}
              />
              <Row
                label="Max gap"
                value={`${gaps().src.toFixed(1)} ms src, ${gaps().recv.toFixed(1)} ms recv`}
              />
              <Show
                when={latency()}
                fallback={<Row label="E2E latency" value="syncing…" />}
              >
                {(value) => (
                  <>
                    <Row
                      label="E2E latency"
                      value={`${value().p50.toFixed(1)} ms p50 est, ${value().p95.toFixed(1)} ms p95`}
                    />
                    <Row
                      label="Breakdown"
                      value={`${value().host.toFixed(1)} host + ~${value().net.toFixed(1)} net + ${value().decode.toFixed(1)} decode + ${value().present.toFixed(1)} present + ~${value().display.toFixed(1)} display ms`}
                    />
                  </>
                )}
              </Show>
              <Show when={surf.clockRttMs !== null}>
                <Row
                  label="Clock"
                  value={`${surf.clockRttMs?.toFixed(1)} ms RTT, ±${((surf.clockRttMs ?? 0) / 2).toFixed(1)} ms est`}
                />
              </Show>
              <Row label="Dropped" value={surf.dropped} />
              <Row label="Errors" value={surf.errors} />
              <Row label="Queue" value={`${surf.queueDepth} decode`} />
              <div style={graphSeparator()}>
                <span style={{ opacity: 0.6, "font-size": `${scale().xs}px` }}>
                  Surface frames
                </span>
                <SurfaceTimeline
                  samples={surf.frameSamples}
                  palette={props.palette}
                  fontSize={scale().xs}
                />
              </div>
            </>
          );
        }}
      </Show>

      {/* ── Terminal-focused section (hidden when a surface is focused) ── */}
      <Show when={props.focusedSurfaceId == null}>
        <Row label="Apply" value={`${stats().applyMs.toFixed(1)} ms`} />
        <Row
          label="Mouse"
          value={`mode=${stats().mouseMode} enc=${stats().mouseEncoding}`}
        />
        <Row
          label="Queued"
          value={`${stats().totalPendingFrames} frames in ${stats().pendingFrameQueues} queues`}
        />
        <Row
          label="Terminals"
          value={`${stats().terminals} live, ${stats().staleTerminals} stale`}
        />
        <Show when={(stats().surfaces?.length ?? 0) > 0}>
          <For each={stats().surfaces}>
            {(s) => (
              <Row
                label={`Surface ${s.surfaceId}`}
                value={`${s.encoder || s.codec} ${s.width}x${s.height}`}
              />
            )}
          </For>
        </Show>
      </Show>

      {/* ── Graphs (always visible) ── */}
      <div style={graphSeparator()}>
        <span style={{ opacity: 0.6, "font-size": `${scale().xs}px` }}>
          Render
        </span>
        <RenderTimeline
          timeline={props.timeline}
          palette={props.palette}
          displayFps={stats().displayFps}
          fontSize={scale().xs}
        />
      </div>
      <div style={graphSeparator()}>
        <span style={{ opacity: 0.6, "font-size": `${scale().xs}px` }}>
          Network
        </span>
        <NetTimeline
          net={props.net}
          palette={props.palette}
          fontSize={scale().xs}
        />
      </div>
    </div>
  );
}

function Row(props: { label: string; value: string | number }) {
  return (
    <div
      data-debug-label={props.label}
      style={{
        display: "flex",
        "justify-content": "space-between",
        gap: "1em",
      }}
    >
      <span style={{ opacity: 0.6 }}>{props.label}</span>
      <span>{props.value}</span>
    </div>
  );
}

// Debug canvases are observability tools, not part of presentation. Drawing
// them at display refresh (240–480 Hz on the machines they diagnose) made the
// profiler itself the largest source of missed frames. 20 Hz keeps a 2-second
// timeline readable while leaving the video rAF path alone.
const DEBUG_GRAPH_INTERVAL_MS = 50;

function RenderTimeline(props: {
  timeline: RenderSampleRing;
  palette: TerminalPalette;
  displayFps: number;
  fontSize: number;
}) {
  let canvas!: HTMLCanvasElement;
  let timer: ReturnType<typeof setInterval> | undefined;
  const W = 300;
  const H = 80;
  const dpr = typeof devicePixelRatio !== "undefined" ? devicePixelRatio : 1;
  const pixelW = Math.max(1, Math.ceil(W * dpr));
  const columnMaxMs = new Float32Array(pixelW);

  onMount(() => {
    const ctx = canvas.getContext("2d");
    if (!ctx) return;
    const draw = () => {
      ctx.clearRect(0, 0, W * dpr, H * dpr);

      const samples = props.timeline;
      if (!samples || samples.length < 2) return;

      const fg = props.palette.fg;
      const success = props.palette.ansi[2] ?? props.palette.fg;
      const warning = props.palette.ansi[3] ?? props.palette.fg;
      const error = props.palette.ansi[1] ?? props.palette.fg;
      const successFill = rgba(success, 0.82);
      const warningFill = rgba(warning, 0.82);
      const errorFill = rgba(error, 0.82);

      const now = performance.now();
      const windowMs = 2000;
      const maxMs = 20;
      const budgetMs = props.displayFps > 0 ? 1000 / props.displayFps : 16.67;
      columnMaxMs.fill(0);

      ctx.strokeStyle = rgba(error, 0.45);
      ctx.lineWidth = dpr;
      const budgetY = (1 - budgetMs / maxMs) * H * dpr;
      ctx.beginPath();
      ctx.moveTo(0, budgetY);
      ctx.lineTo(W * dpr, budgetY);
      ctx.stroke();

      for (let i = 0; i < samples.length; i++) {
        const sampleT = samples.time(i);
        const sampleMs = samples.duration(i);
        const age = now - sampleT;
        if (age > windowMs || age < 0) continue;
        const x = Math.min(
          pixelW - 1,
          Math.floor(((windowMs - age) / windowMs) * pixelW),
        );
        columnMaxMs[x] = Math.max(columnMaxMs[x], sampleMs);
      }
      // Hundreds of high-rate samples land on the same 300 CSS pixels.
      // Collapse them before touching Canvas2D; drawing every sample made the
      // diagnostics overlay create the stalls it was meant to measure.
      for (let x = 0; x < pixelW; x++) {
        const sampleMs = columnMaxMs[x];
        if (sampleMs <= 0) continue;
        const barH = Math.min(sampleMs / maxMs, 1) * H * dpr;
        const y = H * dpr - barH;
        if (sampleMs < budgetMs) ctx.fillStyle = successFill;
        else if (sampleMs < budgetMs * 2) ctx.fillStyle = warningFill;
        else ctx.fillStyle = errorFill;
        ctx.fillRect(x, y, 1, barH);
      }

      ctx.fillStyle = rgba(fg, 0.45);
      ctx.font = `${props.fontSize * dpr}px ui-monospace, monospace`;
      ctx.textBaseline = "top";
      ctx.fillText(`${maxMs}ms`, 2 * dpr, 2 * dpr);
      ctx.textAlign = "right";
      ctx.fillText(
        `budget ${budgetMs.toFixed(1)}ms`,
        (W - 2) * dpr,
        budgetY - 10 * dpr,
      );
      ctx.textAlign = "left";
      ctx.textBaseline = "bottom";
      ctx.fillText("0ms", 2 * dpr, H * dpr - 2 * dpr);
    };
    draw();
    timer = setInterval(draw, DEBUG_GRAPH_INTERVAL_MS);
    onCleanup(() => {
      if (timer !== undefined) clearInterval(timer);
    });
  });

  return (
    <canvas
      ref={canvas}
      width={pixelW}
      height={H * dpr}
      style={{ width: `${W}px`, height: `${H}px`, "margin-top": "2px" }}
    />
  );
}

function NetTimeline(props: {
  net: NetSampleRing;
  palette: TerminalPalette;
  fontSize: number;
}) {
  let canvas!: HTMLCanvasElement;
  let timer: ReturnType<typeof setInterval> | undefined;
  const W = 300;
  const H = 50;
  const dpr = typeof devicePixelRatio !== "undefined" ? devicePixelRatio : 1;
  const pixelW = Math.max(1, Math.ceil(W * dpr));
  const rxColumns = new Uint32Array(pixelW);
  const txColumns = new Uint32Array(pixelW);

  onMount(() => {
    const ctx = canvas.getContext("2d");
    if (!ctx) return;
    const draw = () => {
      ctx.clearRect(0, 0, W * dpr, H * dpr);

      const samples = props.net;
      if (!samples || samples.length === 0) return;

      const fg = props.palette.fg;
      const rx =
        props.palette.ansi[12] ?? props.palette.ansi[6] ?? props.palette.fg;
      const tx =
        props.palette.ansi[11] ?? props.palette.ansi[3] ?? props.palette.fg;
      const rxFill = rgba(rx, 0.82);
      const txFill = rgba(tx, 0.82);

      const now = performance.now();
      const windowMs = 2000;
      let maxBytes = 256;
      rxColumns.fill(0);
      txColumns.fill(0);
      for (let i = 0; i < samples.length; i++) {
        const age = now - samples.time(i);
        if (age > windowMs || age < 0) continue;
        const bytes = samples.bytes(i);
        maxBytes = Math.max(maxBytes, bytes);
        const x = Math.min(
          pixelW - 1,
          Math.floor(((windowMs - age) / windowMs) * pixelW),
        );
        const columns = samples.isRx(i) ? rxColumns : txColumns;
        columns[x] = Math.max(columns[x], bytes);
      }

      const midY = (H * dpr) / 2;

      ctx.strokeStyle = rgba(fg, 0.12);
      ctx.lineWidth = dpr;
      ctx.beginPath();
      ctx.moveTo(0, midY);
      ctx.lineTo(W * dpr, midY);
      ctx.stroke();

      for (let x = 0; x < pixelW; x++) {
        const rxBytes = rxColumns[x];
        if (rxBytes > 0) {
          const barH = Math.min(rxBytes / maxBytes, 1) * (H * dpr * 0.45);
          ctx.fillStyle = rxFill;
          ctx.fillRect(x, midY - barH, 1, barH);
        }
        const txBytes = txColumns[x];
        if (txBytes > 0) {
          const barH = Math.min(txBytes / maxBytes, 1) * (H * dpr * 0.45);
          ctx.fillStyle = txFill;
          ctx.fillRect(x, midY, 1, barH);
        }
      }

      ctx.fillStyle = rgba(fg, 0.45);
      ctx.font = `${props.fontSize * dpr}px ui-monospace, monospace`;
      ctx.textBaseline = "top";
      ctx.fillText(formatBw(maxBytes).replace("/s", ""), 2 * dpr, 2 * dpr);
      ctx.textBaseline = "bottom";
      ctx.fillText("rx", 2 * dpr, midY - 2 * dpr);
      ctx.fillText("tx", 2 * dpr, H * dpr - 2 * dpr);
    };
    draw();
    timer = setInterval(draw, DEBUG_GRAPH_INTERVAL_MS);
    onCleanup(() => {
      if (timer !== undefined) clearInterval(timer);
    });
  });

  return (
    <canvas
      ref={canvas}
      width={pixelW}
      height={H * dpr}
      style={{ width: `${W}px`, height: `${H}px`, "margin-top": "2px" }}
    />
  );
}

/**
 * Canvas graph showing per-surface video frame arrivals over a 2-second
 * sliding window.  Bar height represents encoded frame size; keyframes
 * are drawn in a distinct accent colour.
 */
function SurfaceTimeline(props: {
  samples: SurfaceFrameHistory;
  palette: TerminalPalette;
  fontSize: number;
}) {
  let canvas!: HTMLCanvasElement;
  let timer: ReturnType<typeof setInterval> | undefined;
  const W = 300;
  const H = 60;
  const dpr = typeof devicePixelRatio !== "undefined" ? devicePixelRatio : 1;
  const pixelW = Math.max(1, Math.ceil(W * dpr));
  const byteColumns = new Uint32Array(pixelW);
  const keyColumns = new Uint8Array(pixelW);

  onMount(() => {
    const ctx = canvas.getContext("2d");
    if (!ctx) return;
    const draw = () => {
      ctx.clearRect(0, 0, W * dpr, H * dpr);

      const samples = props.samples;
      if (!samples || samples.length === 0) return;

      const fg = props.palette.fg;
      // Delta frames: green (ansi 2); keyframes: blue/cyan (ansi 4 / 12).
      const deltaColor = props.palette.ansi[2] ?? props.palette.fg;
      const keyColor =
        props.palette.ansi[4] ?? props.palette.ansi[12] ?? props.palette.fg;
      const deltaFill = rgba(deltaColor, 0.7);
      const keyFill = rgba(keyColor, 0.9);

      const now = performance.now();
      const windowMs = 2000;
      let maxBytes = 1024; // minimum scale: 1 KB
      byteColumns.fill(0);
      keyColumns.fill(0);
      for (let i = 0; i < samples.length; i++) {
        const age = now - samples.time(i);
        if (age > windowMs || age < 0) continue;
        const bytes = samples.bytes(i);
        maxBytes = Math.max(maxBytes, bytes);
        const x = Math.min(
          pixelW - 1,
          Math.floor(((windowMs - age) / windowMs) * pixelW),
        );
        if (bytes >= byteColumns[x]) {
          byteColumns[x] = bytes;
          keyColumns[x] = samples.isKey(i) ? 1 : 0;
        }
      }

      for (let x = 0; x < pixelW; x++) {
        const bytes = byteColumns[x];
        if (bytes === 0) continue;
        const barH = Math.min(bytes / maxBytes, 1) * H * dpr * 0.9;
        const y = H * dpr - barH;
        ctx.fillStyle = keyColumns[x] ? keyFill : deltaFill;
        ctx.fillRect(x, y, 1, barH);
      }

      // Labels
      ctx.fillStyle = rgba(fg, 0.45);
      ctx.font = `${props.fontSize * dpr}px ui-monospace, monospace`;
      ctx.textBaseline = "top";
      ctx.fillText(formatBw(maxBytes).replace("/s", ""), 2 * dpr, 2 * dpr);
      ctx.textBaseline = "bottom";
      ctx.fillText("0", 2 * dpr, H * dpr - 2 * dpr);

      // Legend (right-aligned)
      ctx.textAlign = "right";
      ctx.textBaseline = "top";
      ctx.fillStyle = keyFill;
      ctx.fillText("key", (W - 2) * dpr, 2 * dpr);
      ctx.fillStyle = deltaFill;
      ctx.fillText("delta", (W - 30) * dpr, 2 * dpr);
      ctx.textAlign = "left";
    };
    draw();
    timer = setInterval(draw, DEBUG_GRAPH_INTERVAL_MS);
    onCleanup(() => {
      if (timer !== undefined) clearInterval(timer);
    });
  });

  return (
    <canvas
      ref={canvas}
      width={pixelW}
      height={H * dpr}
      style={{ width: `${W}px`, height: `${H}px`, "margin-top": "2px" }}
    />
  );
}
