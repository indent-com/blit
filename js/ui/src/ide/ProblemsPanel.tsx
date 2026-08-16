/**
 * ProblemsPanel — a pure view over {@link IdeSession}'s LSP diagnostics.
 *
 * Requests the session's lazy lsp attachment on mount, then groups
 * `lspHandle.diags.files` by file. An absent path means *unknown, not clean*:
 * an empty set reads as "analyzing…" while any backend indexes, and "no
 * problems" only once every backend is ready.
 */

import {
  createEffect,
  createMemo,
  createSignal,
  For,
  onCleanup,
  Show,
} from "solid-js";
import type { LspDiagnostic } from "@blit-sh/core";
import {
  LSP_SEVERITY_ERROR,
  LSP_SEVERITY_WARNING,
  LSP_SEVERITY_INFO,
  LSP_PHASE_SPAWNING,
  LSP_PHASE_INITIALIZING,
  LSP_PHASE_INDEXING,
  LSP_PHASE_READY,
  LSP_PHASE_FAILED,
} from "@blit-sh/core";
import { editorAssignment } from "@blit-sh/core/bsp";
import type { Theme, UIScale } from "../theme";
import { scrollbarStyle } from "../theme";
import type { IdeSession } from "./session";
import { setReveal } from "./reveal";
import { fillTileDrag, startTileDrag, startTouchDrag } from "./tileDrag";

function phaseLabel(phase: number): string {
  switch (phase) {
    case LSP_PHASE_SPAWNING:
      return "starting";
    case LSP_PHASE_INITIALIZING:
      return "initializing";
    case LSP_PHASE_INDEXING:
      return "indexing";
    case LSP_PHASE_FAILED:
      return "failed";
    default:
      return "";
  }
}

function sevGlyph(sev: number): string {
  if (sev === LSP_SEVERITY_ERROR) return "✖";
  if (sev === LSP_SEVERITY_WARNING) return "⚠";
  if (sev === LSP_SEVERITY_INFO) return "ℹ";
  return "·";
}

function sevColor(theme: Theme, sev: number): string {
  if (sev === LSP_SEVERITY_ERROR) return theme.errorText;
  if (sev === LSP_SEVERITY_WARNING) return theme.warning;
  if (sev === LSP_SEVERITY_INFO) return theme.accent;
  return theme.dimFg;
}

export function ProblemsPanel(props: {
  session: IdeSession | null;
  theme: Theme;
  scale: UIScale;
  fontFamily: string;
  fontSize: number;
  onOpenTile?: (assignment: string) => void;
}) {
  // Hold a lease on the language server while this panel is mounted. The dock
  // unmounts a collapsed section, so letting go here is what stops a folded
  // Problems panel from keeping a language server and its pushed diagnostics
  // stream alive for the rest of the session.
  createEffect(() => {
    const release = props.session?.ensureLsp();
    if (release) onCleanup(release);
  });

  // Resolve a diagnostic's LSP-root-relative path to absolute.
  function diagAbs(path: string): string | null {
    const s = props.session;
    if (!s) return null;
    const root = (s.lspHandle()?.root ?? s.root() ?? "").replace(/\/+$/, "");
    return path.startsWith("/")
      ? path
      : root
        ? `${root}/${path.replace(/^\/+/, "")}`
        : path;
  }
  // The editor tile assignment for a diagnostic, arming its reveal (0-based
  // line/col) so opening — by click or drop — lands on the diagnostic.
  function diagAssignment(
    path: string,
    line: number,
    col: number,
  ): string | null {
    const s = props.session;
    const abs = diagAbs(path);
    if (!s || !abs) return null;
    setReveal(s.connectionId, abs, { text: "", line: line + 1, col });
    return editorAssignment(s.connectionId, abs);
  }
  function openDiag(path: string, line: number, col: number) {
    const a = diagAssignment(path, line, col);
    if (a) props.onOpenTile?.(a);
  }

  const diagsHandle = () => {
    props.session?.lspVersion();
    return props.session?.lspHandle() ?? null;
  };

  // [path, diagnostics] for files that actually have any, sorted by path.
  const files = createMemo(() => {
    const h = diagsHandle();
    if (!h) return [];
    const out: [string, LspDiagnostic[]][] = [];
    for (const [path, fd] of h.diags.files) {
      if (fd.diags.length > 0) out.push([path, fd.diags]);
    }
    out.sort((a, b) => a[0].localeCompare(b[0]));
    return out;
  });

  const servers = createMemo(() => {
    props.session?.lspVersion();
    return [...(props.session?.lspHandle()?.state.servers.values() ?? [])];
  });
  const anyBackend = createMemo(() => servers().length > 0);
  // Servers still coming up (not ready, not failed) — drives the loading
  // banner so an initializing/indexing backend is visible even once some
  // diagnostics have already streamed in.
  const loadingServers = createMemo(() =>
    servers().filter(
      (s) => s.phase !== LSP_PHASE_READY && s.phase !== LSP_PHASE_FAILED,
    ),
  );
  const loadingLabel = createMemo(() =>
    loadingServers()
      .map((s) => {
        const l = phaseLabel(s.phase);
        return s.progressPct > 0 && s.progressPct < 100
          ? `${l} ${s.progressPct}%`
          : l;
      })
      .filter(Boolean)
      .join(" · "),
  );
  // Debounce the "Analyzing…" banner: a language server that briefly re-indexes
  // (e.g. on each edit) would otherwise flicker the banner on and off. Only show
  // it once a backend has been busy for a beat; hide it immediately when idle.
  const [showLoading, setShowLoading] = createSignal(false);
  let onTimer: ReturnType<typeof setTimeout> | undefined;
  createEffect(() => {
    const busy = loadingServers().length > 0;
    clearTimeout(onTimer);
    if (busy) onTimer = setTimeout(() => setShowLoading(true), 600);
    else setShowLoading(false);
  });
  onCleanup(() => clearTimeout(onTimer));

  const total = createMemo(() => files().reduce((n, [, d]) => n + d.length, 0));

  return (
    <div
      style={{
        flex: "1 1 0",
        "min-height": 0,
        "overflow-y": "auto",
        ...scrollbarStyle(props.theme),
      }}
    >
      {/* Loading banner: visible once a backend has been busy for a beat
          (debounced), even alongside diagnostics that have already streamed
          in — so a brief re-index doesn't flicker it on and off. */}
      <Show when={showLoading()}>
        <div
          style={{
            display: "flex",
            "align-items": "center",
            gap: `${props.scale.tightGap}px`,
            padding: `${props.scale.tightGap}px ${props.scale.panelPadding}px`,
            "font-size": `${props.scale.xs}px`,
            color: props.theme.dimFg,
            background: props.theme.hoverBg,
            "border-bottom": `1px solid ${props.theme.subtleBorder}`,
          }}
        >
          <span
            style={{
              display: "inline-block",
              animation: "blit-spin 0.9s linear infinite",
            }}
          >
            ◐
          </span>
          <span>Analyzing…</span>
          <Show when={loadingLabel()}>
            <span style={{ "margin-left": "auto" }}>{loadingLabel()}</span>
          </Show>
          <style>{"@keyframes blit-spin{to{transform:rotate(360deg)}}"}</style>
        </div>
      </Show>
      <Show
        when={total() > 0}
        fallback={
          <div
            style={{
              padding: `${props.scale.panelPadding}px`,
              "font-size": `${props.scale.sm}px`,
              color: props.theme.dimFg,
            }}
          >
            <Show when={props.session} fallback="No root selected.">
              {/* The dock folds this section away on a remote with no language
                  intelligence; this is what it says when opened anyway. Without
                  it the panel sat on "Opening…" for an attach that can never
                  happen. */}
              <Show
                when={!props.session!.noLsp()}
                fallback="No language support on this remote."
              >
                <Show when={props.session!.lspHandle()} fallback="Opening…">
                  <Show
                    when={!anyBackend()}
                    fallback={
                      // Sticky "No problems.": only blank while a backend is
                      // *sustained*-loading (debounced). A brief re-index blip
                      // no longer flips this on and off every second. If the
                      // reindex surfaces a problem it streams into `total()`
                      // and shows.
                      showLoading() ? "" : "No problems."
                    }
                  >
                    No language server for this project.
                  </Show>
                </Show>
              </Show>
            </Show>
          </div>
        }
      >
        <For each={files()}>
          {([path, diags]) => (
            <>
              <div
                style={{
                  display: "flex",
                  "align-items": "center",
                  gap: `${props.scale.tightGap}px`,
                  padding: `${props.scale.tightGap}px ${props.scale.panelPadding}px 2px`,
                  "font-family": props.fontFamily,
                  "font-size": `${props.scale.xs}px`,
                  color: props.theme.dimFg,
                }}
              >
                <span
                  style={{
                    overflow: "hidden",
                    "text-overflow": "ellipsis",
                    "white-space": "nowrap",
                  }}
                >
                  {path}
                </span>
                <span style={{ "margin-left": "auto" }}>{diags.length}</span>
              </div>
              <For each={diags}>
                {(d) => (
                  <div
                    style={{
                      display: "flex",
                      "align-items": "baseline",
                      gap: `${props.scale.tightGap}px`,
                      padding: `1px ${props.scale.panelPadding}px 1px ${props.scale.panelPadding + 6}px`,
                      "font-family": props.fontFamily,
                      // Diagnostic messages are content: configured font
                      // size. The code/source suffix and the line:col stay
                      // smaller.
                      "font-size": `${props.scale.md}px`,
                      cursor: props.onOpenTile ? "pointer" : "default",
                    }}
                    title={`${path}:${d.line + 1}:${d.col + 1}`}
                    onClick={() => openDiag(path, d.line, d.col)}
                    draggable={true}
                    onDragStart={(e) => {
                      const a = diagAssignment(path, d.line, d.col);
                      if (a) startTileDrag(e, a);
                    }}
                    // Touch never reaches onDragStart; a hold starts it, so
                    // the list still scrolls.
                    onPointerDown={(e) => {
                      const a = diagAssignment(path, d.line, d.col);
                      if (a)
                        startTouchDrag(
                          e,
                          (dt) => fillTileDrag(dt, a),
                          "long-press",
                        );
                    }}
                  >
                    <span
                      style={{
                        color: sevColor(props.theme, d.severity),
                        "flex-shrink": 0,
                      }}
                    >
                      {sevGlyph(d.severity)}
                    </span>
                    <span
                      style={{
                        color: props.theme.fg,
                        "white-space": "normal",
                        "line-height": 1.35,
                      }}
                    >
                      {d.msg}
                      <Show when={d.code || d.source}>
                        <span
                          style={{
                            color: props.theme.dimFg,
                            "font-size": `${props.scale.xs}px`,
                            "margin-left": "6px",
                          }}
                        >
                          {[d.code, d.source].filter(Boolean).join(" · ")}
                        </span>
                      </Show>
                    </span>
                    <span
                      style={{
                        "margin-left": "auto",
                        color: props.theme.dimFg,
                        "font-size": `${props.scale.xs}px`,
                        "flex-shrink": 0,
                        "font-variant-numeric": "tabular-nums",
                      }}
                    >
                      {d.line + 1}:{d.col + 1}
                    </span>
                  </div>
                )}
              </For>
            </>
          )}
        </For>
      </Show>
    </div>
  );
}
