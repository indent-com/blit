import { WebPaneNav } from "./WebPaneNav";
import type { WebPaneHandle } from "./WebPane";
import { Show, For, type JSX } from "solid-js";
import { onMount, onCleanup } from "solid-js";
import type {
  BlitSession,
  BlitSurface,
  BlitConnectionSnapshot,
  ConnectionStatus,
  TerminalPalette,
  SurfaceFrameSample,
  LinkHover,
} from "@blit-sh/core";
import { formatBw } from "./createMetrics";
import type { Metrics, RenderSample, NetSample } from "./createMetrics";
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
  frameSamples: SurfaceFrameSample[];
  outputSamples: readonly number[];
  dropped: number;
  errors: number;
  queueDepth: number;
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
  timeline: RenderSample[];
  net: NetSample[];
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
        style={buttonStyle()}
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
      <Show when={props.audioAvailable || props.hasSurfaces}>
        <button
          onClick={props.onMedia}
          style={{
            ...buttonStyle(),
            opacity: !props.audioAvailable || props.audioMuted ? 0.5 : 1,
          }}
          title="Media settings"
        >
          {"\u266A"}
        </button>
      </Show>
      <button
        onClick={props.toggleDebug}
        style={{ ...buttonStyle(), opacity: props.debug ? 1 : 0.3 }}
        title={t("statusbar.debugStats")}
      >
        {"\u25C6"}
      </button>
      <button
        onClick={props.onToggleLeftDock}
        style={{
          ...buttonStyle(),
          opacity: props.leftDockOpen ? 1 : 0.3,
        }}
        title="Toggle IDE dock (Ctrl+Shift+E)"
      >
        {"\u25E7"}
      </button>
      <button
        onClick={props.onPreviewPanel}
        style={{
          ...buttonStyle(),
          opacity: props.previewPanelOpen ? 1 : 0.3,
        }}
        title={t("statusbar.previewPanel")}
      >
        {"\u25E8"}
      </button>
      <button
        onClick={props.onPalette}
        style={buttonStyle()}
        title={tp("statusbar.paletteTitle", { name: props.palette.name })}
      >
        {props.palette.dark ? "\u25D1" : "\u25D0"}
      </button>
      <button
        onClick={props.onFont}
        style={buttonStyle()}
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
          title={props.keyboardOpen ? "Hide keyboard" : "Show keyboard"}
        >
          <svg
            width="16"
            height="16"
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
          ...buttonStyle(),
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
          />
        </Show>
        <Show when={connCounts().busy > 0}>
          <ConnectionDot
            color={theme().warning}
            count={connCounts().busy}
            total={props.connections.length}
          />
        </Show>
        <Show when={connCounts().bad > 0}>
          <ConnectionDot
            color={theme().error}
            count={connCounts().bad}
            total={props.connections.length}
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
          <ConnectionDot color={theme().warning} count={1} total={1} />
        </Show>
        <Show
          when={
            props.connections.length === 0 &&
            props.gatewayStatus === "unavailable"
          }
        >
          <ConnectionDot color={theme().error} count={1} total={1} />
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

function ConnectionDot(props: { color: string; count: number; total: number }) {
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
          width: "7px",
          height: "7px",
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
  timeline: RenderSample[];
  net: NetSample[];
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
    samples: readonly number[] | SurfaceFrameSample[],
    windowMs: number,
  ): number => {
    const cutoff = performance.now() - windowMs;
    let n = 0;
    for (let i = samples.length - 1; i >= 0; i--) {
      const t =
        typeof samples[i] === "number"
          ? (samples[i] as number)
          : (samples[i] as SurfaceFrameSample).t;
      if (t < cutoff) break;
      n++;
    }
    return n;
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
        "background-color": rgba(props.palette.bg, dark() ? 0.78 : 0.84),
        "backdrop-filter": "blur(6px)",
        "-webkit-backdrop-filter": "blur(6px)",
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
        label="FPS / UPS"
        value={`${props.metrics.fps} / ${props.metrics.ups}`}
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
          const recvFps = () => countRecent(surf.frameSamples, 1000);
          const outFps = () => countRecent(surf.outputSamples, 1000);
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
                value={`${recvFps()} recv/s, ${outFps()} out/s`}
              />
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

function RenderTimeline(props: {
  timeline: RenderSample[];
  palette: TerminalPalette;
  displayFps: number;
  fontSize: number;
}) {
  let canvas!: HTMLCanvasElement;
  let raf = 0;
  const W = 300;
  const H = 80;
  const dpr = typeof devicePixelRatio !== "undefined" ? devicePixelRatio : 1;

  onMount(() => {
    const draw = () => {
      const ctx = canvas.getContext("2d");
      if (!ctx) return;
      ctx.clearRect(0, 0, W * dpr, H * dpr);

      const samples = props.timeline;
      if (!samples || samples.length < 2) {
        raf = requestAnimationFrame(draw);
        return;
      }

      const fg = props.palette.fg;
      const success = props.palette.ansi[2] ?? props.palette.fg;
      const warning = props.palette.ansi[3] ?? props.palette.fg;
      const error = props.palette.ansi[1] ?? props.palette.fg;

      const now = performance.now();
      const windowMs = 2000;
      const maxMs = 20;
      const budgetMs = props.displayFps > 0 ? 1000 / props.displayFps : 16.67;

      ctx.strokeStyle = rgba(error, 0.45);
      ctx.lineWidth = dpr;
      const budgetY = (1 - budgetMs / maxMs) * H * dpr;
      ctx.beginPath();
      ctx.moveTo(0, budgetY);
      ctx.lineTo(W * dpr, budgetY);
      ctx.stroke();

      for (const sample of samples) {
        const age = now - sample.t;
        if (age > windowMs || age < 0) continue;
        const x = ((windowMs - age) / windowMs) * W * dpr;
        const barH = Math.min(sample.ms / maxMs, 1) * H * dpr;
        const y = H * dpr - barH;

        if (sample.ms < budgetMs) ctx.fillStyle = rgba(success, 0.82);
        else if (sample.ms < budgetMs * 2) ctx.fillStyle = rgba(warning, 0.82);
        else ctx.fillStyle = rgba(error, 0.82);

        ctx.fillRect(x, y, Math.max(1, dpr), barH);
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

      raf = requestAnimationFrame(draw);
    };
    raf = requestAnimationFrame(draw);
    onCleanup(() => cancelAnimationFrame(raf));
  });

  return (
    <canvas
      ref={canvas}
      width={W * dpr}
      height={H * dpr}
      style={{ width: `${W}px`, height: `${H}px`, "margin-top": "2px" }}
    />
  );
}

function NetTimeline(props: {
  net: NetSample[];
  palette: TerminalPalette;
  fontSize: number;
}) {
  let canvas!: HTMLCanvasElement;
  let raf = 0;
  const W = 300;
  const H = 50;
  const dpr = typeof devicePixelRatio !== "undefined" ? devicePixelRatio : 1;

  onMount(() => {
    const draw = () => {
      const ctx = canvas.getContext("2d");
      if (!ctx) return;
      ctx.clearRect(0, 0, W * dpr, H * dpr);

      const samples = props.net;
      if (!samples || samples.length === 0) {
        raf = requestAnimationFrame(draw);
        return;
      }

      const fg = props.palette.fg;
      const rx =
        props.palette.ansi[12] ?? props.palette.ansi[6] ?? props.palette.fg;
      const tx =
        props.palette.ansi[11] ?? props.palette.ansi[3] ?? props.palette.fg;

      const now = performance.now();
      const windowMs = 2000;
      let maxBytes = 256;
      for (const sample of samples) {
        if (now - sample.t <= windowMs)
          maxBytes = Math.max(maxBytes, sample.bytes);
      }

      const midY = (H * dpr) / 2;

      ctx.strokeStyle = rgba(fg, 0.12);
      ctx.lineWidth = dpr;
      ctx.beginPath();
      ctx.moveTo(0, midY);
      ctx.lineTo(W * dpr, midY);
      ctx.stroke();

      for (const sample of samples) {
        const age = now - sample.t;
        if (age > windowMs || age < 0) continue;
        const x = ((windowMs - age) / windowMs) * W * dpr;
        const barH = Math.min(sample.bytes / maxBytes, 1) * (H * dpr * 0.45);
        const y = sample.dir === "rx" ? midY - barH : midY;
        ctx.fillStyle = sample.dir === "rx" ? rgba(rx, 0.82) : rgba(tx, 0.82);
        ctx.fillRect(x, y, Math.max(1, dpr), barH);
      }

      ctx.fillStyle = rgba(fg, 0.45);
      ctx.font = `${props.fontSize * dpr}px ui-monospace, monospace`;
      ctx.textBaseline = "top";
      ctx.fillText(formatBw(maxBytes).replace("/s", ""), 2 * dpr, 2 * dpr);
      ctx.textBaseline = "bottom";
      ctx.fillText("rx", 2 * dpr, midY - 2 * dpr);
      ctx.fillText("tx", 2 * dpr, H * dpr - 2 * dpr);

      raf = requestAnimationFrame(draw);
    };
    raf = requestAnimationFrame(draw);
    onCleanup(() => cancelAnimationFrame(raf));
  });

  return (
    <canvas
      ref={canvas}
      width={W * dpr}
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
  samples: SurfaceFrameSample[];
  palette: TerminalPalette;
  fontSize: number;
}) {
  let canvas!: HTMLCanvasElement;
  let raf = 0;
  const W = 300;
  const H = 60;
  const dpr = typeof devicePixelRatio !== "undefined" ? devicePixelRatio : 1;

  onMount(() => {
    const draw = () => {
      const ctx = canvas.getContext("2d");
      if (!ctx) return;
      ctx.clearRect(0, 0, W * dpr, H * dpr);

      const samples = props.samples;
      if (!samples || samples.length === 0) {
        raf = requestAnimationFrame(draw);
        return;
      }

      const fg = props.palette.fg;
      // Delta frames: green (ansi 2); keyframes: blue/cyan (ansi 4 / 12).
      const deltaColor = props.palette.ansi[2] ?? props.palette.fg;
      const keyColor =
        props.palette.ansi[4] ?? props.palette.ansi[12] ?? props.palette.fg;

      const now = performance.now();
      const windowMs = 2000;
      let maxBytes = 1024; // minimum scale: 1 KB
      for (const sample of samples) {
        if (now - sample.t <= windowMs)
          maxBytes = Math.max(maxBytes, sample.bytes);
      }

      for (const sample of samples) {
        const age = now - sample.t;
        if (age > windowMs || age < 0) continue;
        const x = ((windowMs - age) / windowMs) * W * dpr;
        const barH = Math.min(sample.bytes / maxBytes, 1) * H * dpr * 0.9;
        const y = H * dpr - barH;

        ctx.fillStyle = sample.key
          ? rgba(keyColor, 0.9)
          : rgba(deltaColor, 0.7);
        ctx.fillRect(x, y, Math.max(1, dpr), barH);
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
      ctx.fillStyle = rgba(keyColor, 0.9);
      ctx.fillText("key", (W - 2) * dpr, 2 * dpr);
      ctx.fillStyle = rgba(deltaColor, 0.7);
      ctx.fillText("delta", (W - 30) * dpr, 2 * dpr);
      ctx.textAlign = "left";

      raf = requestAnimationFrame(draw);
    };
    raf = requestAnimationFrame(draw);
    onCleanup(() => cancelAnimationFrame(raf));
  });

  return (
    <canvas
      ref={canvas}
      width={W * dpr}
      height={H * dpr}
      style={{ width: `${W}px`, height: `${H}px`, "margin-top": "2px" }}
    />
  );
}
