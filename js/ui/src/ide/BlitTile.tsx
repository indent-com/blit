/**
 * BlitTile — renders an IDE tile assignment string as the appropriate view.
 *
 * A tile assignment is one of `editor:<conn>:<path>`, `diff:<conn>:<path>`
 * (optionally `diff:<conn>:staged:<path>`), or `commit:<conn>:<oid>:<repoPath>`
 * (see js/core/src/bsp/layout.ts). This component parses the assignment and
 * renders BlitEditor / BlitDiff / BlitCommit accordingly.
 *
 * It is the single render path shared by BSP leaf panes (BSPContainer) and the
 * non-BSP "focused tile" view (Workspace), so the two never drift.
 *
 * Keyed on the assignment string: replacing an editor tile with a *different*
 * file (or an editor with a diff) must rebuild the view, not reuse the old one.
 * BlitEditor/BlitDiff capture their path at construction, so without this a
 * pane swapped to another file would keep showing the old one. Theme/size props
 * stay reactive (SolidJS tracks them in the JSX), so re-theming never rebuilds.
 *
 * Also the app's error boundary. A throw anywhere inside a tile — a bad
 * assignment, a codec surprise, a grammar that dislikes a file — would
 * otherwise unwind past every pane and blank the whole window, because a
 * Solid error propagates to the nearest boundary and there is none above
 * this one. Containing it here costs one pane and keeps the rest of the
 * layout, and every terminal in it, alive.
 */

import { ErrorBoundary, Show } from "solid-js";
import type { BlitWorkspace, TerminalPalette } from "@blit-sh/core";
import { parseTileAssignment, parseDiffArg } from "@blit-sh/core/bsp";
import type { Theme, UIScale } from "../theme";
import { ui } from "../theme";
import { BlitDiff } from "./BlitDiff";
import { BlitEditor } from "./BlitEditor";
import { BlitCommit } from "./BlitCommit";
import { BlitPreview } from "./BlitPreview";

export function BlitTile(props: {
  workspace: BlitWorkspace;
  /** The tile assignment string (editor:/diff:/commit:). */
  assignment: string;
  theme: Theme;
  palette: TerminalPalette;
  scale: UIScale;
  fontFamily: string;
  fontSize: number;
  /** Open a further tile (e.g. from a commit's file rows). */
  onOpenTile: (assignment: string) => void;
  /** Read-only preview (the background dock): no editing, no LSP, no
   *  buffer parking — a zoomed-out always-on view, like a terminal
   *  thumbnail. */
  preview?: boolean;
}) {
  const view = (assignment: string) => {
    const t = parseTileAssignment(assignment);
    if (!t) return null;
    if (t.kind === "diff") {
      const { path, side } = parseDiffArg(t.arg);
      return (
        <BlitDiff
          workspace={props.workspace}
          connectionId={t.connectionId}
          path={path}
          side={side}
          theme={props.theme}
          palette={props.palette}
          scale={props.scale}
          fontFamily={props.fontFamily}
          fontSize={props.fontSize}
          onOpenTile={props.onOpenTile}
          preview={props.preview}
        />
      );
    }
    if (t.kind === "preview") {
      return (
        <BlitPreview
          workspace={props.workspace}
          connectionId={t.connectionId}
          path={t.arg}
          theme={props.theme}
          scale={props.scale}
          fontFamily={props.fontFamily}
          fontSize={props.fontSize}
          onOpenTile={props.onOpenTile}
          preview={props.preview}
        />
      );
    }
    if (t.kind === "commit") {
      const colon = t.arg.indexOf(":");
      return (
        <BlitCommit
          workspace={props.workspace}
          connectionId={t.connectionId}
          oid={t.arg.slice(0, colon)}
          repoPath={t.arg.slice(colon + 1)}
          theme={props.theme}
          palette={props.palette}
          scale={props.scale}
          fontFamily={props.fontFamily}
          fontSize={props.fontSize}
          onOpenTile={props.onOpenTile}
          preview={props.preview}
        />
      );
    }
    return (
      <BlitEditor
        workspace={props.workspace}
        connectionId={t.connectionId}
        path={t.arg}
        theme={props.theme}
        palette={props.palette}
        fontFamily={props.fontFamily}
        fontSize={props.fontSize}
        onOpenTile={props.onOpenTile}
        preview={props.preview}
      />
    );
  };

  return (
    <Show when={props.assignment} keyed>
      {(assignment) => (
        <ErrorBoundary
          fallback={(err: unknown, reset: () => void) => (
            <TileError
              assignment={assignment}
              err={err}
              reset={reset}
              theme={props.theme}
              scale={props.scale}
              fontFamily={props.fontFamily}
              preview={props.preview}
            />
          )}
        >
          {view(assignment)}
        </ErrorBoundary>
      )}
    </Show>
  );
}

/** What a pane shows when its tile threw: what broke, where, and a way back. */
function TileError(props: {
  assignment: string;
  err: unknown;
  reset: () => void;
  theme: Theme;
  scale: UIScale;
  fontFamily: string;
  preview?: boolean;
}) {
  const message = () =>
    props.err instanceof Error
      ? props.err.message || props.err.name
      : String(props.err);
  return (
    <div
      style={{
        width: "100%",
        height: "100%",
        display: "flex",
        "flex-direction": "column",
        gap: `${props.scale.tightGap}px`,
        padding: `${props.scale.panelPadding}px`,
        overflow: "auto",
        background: props.theme.bg,
        color: props.theme.fg,
        "font-family": props.fontFamily,
        "font-size": `${props.scale.md}px`,
      }}
    >
      <b style={{ color: props.theme.errorText }}>This pane failed to render</b>
      <div style={{ color: props.theme.dimFg, "word-break": "break-all" }}>
        {props.assignment}
      </div>
      <div style={{ "white-space": "pre-wrap", "word-break": "break-word" }}>
        {message()}
      </div>
      {/* A preview thumbnail has no keyboard path to act on this, so the
          button would be decoration. */}
      <Show when={!props.preview}>
        <div>
          <button style={ui.btn} onClick={() => props.reset()}>
            Retry
          </button>
        </div>
      </Show>
    </div>
  );
}
