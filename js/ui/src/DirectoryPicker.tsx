/**
 * DirectoryPicker — a compact directory browser for choosing a workspace-root
 * path on a given connection. Opens a non-recursive fs sync at the current
 * directory, lists sub-directories, and lets the user descend / go up / pick
 * the current directory. Metadata-only; directories only.
 */

import { createEffect, createSignal, onCleanup, For, Show } from "solid-js";
import type { BlitWorkspace, ConnectionId, FsSyncHandle } from "@blit-sh/core";
import {
  FS_ENTRY_TYPE_MASK,
  FS_ENTRY_DIR,
  FS_ENTRY_SYMLINK,
} from "@blit-sh/core";
import type { Theme, UIScale } from "./theme";
import { mergeStyle, scrollbarStyle, ui } from "./theme";

function parentOf(path: string): string {
  const s = path.replace(/\/+$/, "");
  const i = s.lastIndexOf("/");
  return i <= 0 ? "/" : s.slice(0, i);
}

function join(dir: string, name: string): string {
  return dir === "/" ? `/${name}` : `${dir}/${name}`;
}

export function DirectoryPicker(props: {
  workspace: BlitWorkspace;
  connectionId: ConnectionId;
  initialPath: string;
  theme: Theme;
  scale: UIScale;
  fontFamily: string;
  fontSize: number;
  onPick: (path: string) => void;
  onCancel: () => void;
}) {
  const [cwd, setCwd] = createSignal(
    props.initialPath && props.initialPath.startsWith("/")
      ? props.initialPath
      : "/",
  );
  const [dirs, setDirs] = createSignal<string[] | null>(null);
  const [error, setError] = createSignal<string | null>(null);

  const refresh = (h: FsSyncHandle) => {
    const out: string[] = [];
    for (const [name, node] of h.live) {
      if (name === "" || name === ".git") continue;
      const type = node.entryFlags & FS_ENTRY_TYPE_MASK;
      // Directories (and symlinks, which may point at directories).
      if (type === FS_ENTRY_DIR || type === FS_ENTRY_SYMLINK) out.push(name);
    }
    out.sort((a, b) => a.localeCompare(b));
    setDirs(out);
  };

  // Re-open a non-recursive sync whenever the directory changes.
  createEffect(() => {
    const dir = cwd();
    let opened: FsSyncHandle | null = null;
    let disposed = false;
    setDirs(null);
    setError(null);
    props.workspace
      .syncFs(props.connectionId, dir, {
        recursive: false,
        content: false,
        onSync: () => opened && refresh(opened),
        onUpdate: () => opened && refresh(opened),
      })
      .then((h) => {
        if (disposed) {
          h.stop();
          return;
        }
        opened = h;
        refresh(h);
      })
      .catch((e: unknown) =>
        setError(e instanceof Error ? e.message : String(e)),
      );
    onCleanup(() => {
      disposed = true;
      opened?.stop();
    });
  });

  const rowStyle = {
    display: "flex",
    "align-items": "center",
    gap: `${props.scale.tightGap}px`,
    height: `${Math.round(props.fontSize * 1.6)}px`,
    padding: `0 ${props.scale.panelPadding}px`,
    "font-family": props.fontFamily,
    "font-size": `${props.scale.sm}px`,
    color: props.theme.fg,
    cursor: "pointer",
    "white-space": "nowrap" as const,
  };

  return (
    <div
      style={{
        display: "flex",
        "flex-direction": "column",
        border: `1px solid ${props.theme.subtleBorder}`,
        "background-color": props.theme.solidPanelBg,
        "min-height": "0",
      }}
    >
      {/* Current path + actions */}
      <div
        style={{
          display: "flex",
          "align-items": "center",
          gap: `${props.scale.tightGap}px`,
          padding: `${props.scale.controlY}px ${props.scale.panelPadding}px`,
          "border-bottom": `1px solid ${props.theme.subtleBorder}`,
        }}
      >
        <button
          style={{ ...ui.btn, opacity: cwd() === "/" ? 0.3 : 0.8 }}
          disabled={cwd() === "/"}
          onClick={() => setCwd(parentOf(cwd()))}
          title="Parent directory"
        >
          {"↑"}
        </button>
        <span
          style={{
            flex: 1,
            "min-width": 0,
            overflow: "hidden",
            "text-overflow": "ellipsis",
            "font-family": "monospace, inherit",
            "font-size": `${props.scale.sm}px`,
            color: props.theme.dimFg,
            direction: "rtl",
            "text-align": "left",
          }}
          title={cwd()}
        >
          {cwd()}
        </span>
      </div>

      {/* Directory list */}
      <div
        style={{
          flex: "1 1 auto",
          "max-height": "40vh",
          "overflow-y": "auto",
          ...scrollbarStyle(props.theme),
        }}
      >
        <Show
          when={!error()}
          fallback={
            <div
              style={{
                padding: `${props.scale.panelPadding}px`,
                color: props.theme.errorText,
                "font-size": `${props.scale.sm}px`,
              }}
            >
              {error()}
            </div>
          }
        >
          <Show
            when={dirs()}
            fallback={
              <div
                style={{
                  padding: `${props.scale.panelPadding}px`,
                  color: props.theme.dimFg,
                  "font-size": `${props.scale.sm}px`,
                }}
              >
                Loading…
              </div>
            }
          >
            {(list) => (
              <Show
                when={list().length > 0}
                fallback={
                  <div
                    style={{
                      padding: `${props.scale.panelPadding}px`,
                      color: props.theme.dimFg,
                      "font-size": `${props.scale.sm}px`,
                    }}
                  >
                    No sub-directories.
                  </div>
                }
              >
                <For each={list()}>
                  {(name) => (
                    <div
                      style={rowStyle}
                      onClick={() => setCwd(join(cwd(), name))}
                      title={join(cwd(), name)}
                    >
                      <span style={{ "flex-shrink": 0, opacity: 0.85 }}>
                        📁
                      </span>
                      <span
                        style={{
                          overflow: "hidden",
                          "text-overflow": "ellipsis",
                        }}
                      >
                        {name}
                      </span>
                    </div>
                  )}
                </For>
              </Show>
            )}
          </Show>
        </Show>
      </div>

      {/* Footer: pick / cancel */}
      <div
        style={{
          display: "flex",
          "justify-content": "flex-end",
          gap: `${props.scale.tightGap}px`,
          padding: `${props.scale.controlY}px ${props.scale.panelPadding}px`,
          "border-top": `1px solid ${props.theme.subtleBorder}`,
        }}
      >
        <button style={{ ...ui.btn }} onClick={props.onCancel}>
          Cancel
        </button>
        <button
          style={mergeStyle(ui.btn, {
            border: `1px solid ${props.theme.accent}`,
            "background-color": props.theme.accent,
            color: "#fff",
          })}
          onClick={() => props.onPick(cwd())}
        >
          Use this directory
        </button>
      </div>
    </div>
  );
}
