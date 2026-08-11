/**
 * TerminalDropTarget — drop OS files onto a terminal to upload them into
 * that session's server-side cwd, then paste the (shell-quoted) names into
 * the terminal so the user can act on them immediately.
 *
 * The upload goes through an fs sync rooted at the pty's live cwd
 * (`syncFs("", { fromSessionId })`, docs/ide.md Decision 3), opened lazily
 * on the first drop and cached per session; handles are stopped when the
 * pane switches session or unmounts, mirroring how ide/session.ts scopes
 * its syncs to a reactive root.
 *
 * Gating is on the "Files" drag type only: the internal pane/tile drags
 * (ide/tileDrag.ts) carry MIME-namespaced payloads and never offer "Files",
 * so the two never compete for the same gesture.
 */

import {
  createEffect,
  createSignal,
  onCleanup,
  Show,
  type Accessor,
  type JSX,
} from "solid-js";
import type {
  BlitTerminalSurface,
  BlitWorkspace,
  ConnectionId,
  FsSyncHandle,
  SessionId,
} from "@blit-sh/core";
import { t, tp } from "./i18n";
import { isSourceTerminalUnavailableError } from "./ide/followTerminal";
import { ui, z, type Theme, type UIScale } from "./theme";

/** True while the drag offers OS files (as opposed to an internal pane/tile
 *  payload — those carry only the custom MIMEs from ide/tileDrag.ts).
 *  macOS file promises (the screenshot's floating thumbnail) can arrive
 *  with no "Files" type but a file-kind item — those count too. */
export function isFileDrag(e: DragEvent): boolean {
  const dt = e.dataTransfer;
  if (!dt) return false;
  if (dt.types.includes("Files")) return true;
  for (const item of Array.from(dt.items ?? [])) {
    if (item.kind === "file") return true;
  }
  return false;
}

/** POSIX single-quote escaping: `'` → `'\''`. Always quotes — harmless for
 *  plain names, safe for spaces/metacharacters. */
export function shellQuote(name: string): string {
  return `'${name.replace(/'/g, `'\\''`)}'`;
}

/** A dropped file's name as a single safe path component under the cwd. */
function baseName(name: string): string | null {
  const parts = name.split(/[\\/]/).filter((p) => p && p !== "." && p !== "..");
  return parts.length > 0 ? parts[parts.length - 1] : null;
}

/** A name for a dropped file that brought none (file promises can arrive
 *  nameless): derived from its MIME type. */
function fallbackName(mime: string, index: number): string {
  const ext =
    mime === "image/png"
      ? "png"
      : mime === "image/jpeg"
        ? "jpg"
        : mime === "image/webp"
          ? "webp"
          : mime === "image/gif"
            ? "gif"
            : "bin";
  return `drop-${index}.${ext}`;
}

// A file dropped anywhere without a drop handler makes the browser navigate
// to it — losing every session in the tab. One shared guard, ref-counted
// across the mounted drop targets, swallows exactly that case (file drags
// only; internal drags keep their own handlers).
let guardCount = 0;
const navigationGuard = (e: DragEvent) => {
  if (isFileDrag(e)) e.preventDefault();
};

export function TerminalDropTarget(props: {
  workspace: BlitWorkspace;
  sessionId: SessionId;
  connectionId: ConnectionId;
  /** The pane's live surface, for the post-upload paste. */
  surface: Accessor<BlitTerminalSurface | null>;
  theme: Theme;
  scale: UIScale;
  children: JSX.Element;
}) {
  const [dragOver, setDragOver] = createSignal(false);
  const [error, setError] = createSignal<string | null>(null);

  let enterDepth = 0;
  let errorTimer: ReturnType<typeof setTimeout> | null = null;
  let disposed = false;
  // One cached cwd sync per session this pane has uploaded to.
  const handles = new Map<SessionId, FsSyncHandle>();
  // Drops chain onto this so concurrent drops upload sequentially.
  let queue: Promise<void> = Promise.resolve();

  // The pane remapped to another session: drop the old session's sync
  // rather than letting one sync accumulate per visited session (the
  // server caps syncs per connection).
  createEffect(() => {
    const sid = props.sessionId;
    for (const [key, h] of handles) {
      if (key !== sid) {
        h.stop();
        handles.delete(key);
      }
    }
  });

  onCleanup(() => {
    disposed = true;
    if (errorTimer) clearTimeout(errorTimer);
    for (const h of handles.values()) h.stop();
    handles.clear();
  });

  if (guardCount++ === 0) {
    window.addEventListener("dragover", navigationGuard);
    window.addEventListener("drop", navigationGuard);
  }
  // `drop` anywhere ends the gesture, whatever the enter/leave count says
  // (same pattern as the pane-drag tracking in Workspace).
  const clearDrag = () => {
    enterDepth = 0;
    setDragOver(false);
  };
  window.addEventListener("drop", clearDrag);
  window.addEventListener("dragend", clearDrag);
  onCleanup(() => {
    window.removeEventListener("drop", clearDrag);
    window.removeEventListener("dragend", clearDrag);
    if (--guardCount === 0) {
      window.removeEventListener("dragover", navigationGuard);
      window.removeEventListener("drop", navigationGuard);
    }
  });

  function showError(msg: string) {
    setError(msg);
    if (errorTimer) clearTimeout(errorTimer);
    errorTimer = setTimeout(() => setError(null), 8000);
  }

  async function handleFor(sid: SessionId): Promise<FsSyncHandle | null> {
    const cached = handles.get(sid);
    if (cached) return cached;
    const h = await props.workspace.syncFs(props.connectionId, "", {
      fromSessionId: sid,
      recursive: false,
      content: false,
      onClosed: () => {
        if (handles.get(sid) === h) handles.delete(sid);
      },
    });
    // Unmounted (or remapped to another session) while the open was in
    // flight — abort quietly, nobody is left to report an error to.
    if (disposed || props.sessionId !== sid) {
      h.stop();
      return null;
    }
    handles.set(sid, h);
    return h;
  }

  async function uploadFiles(files: File[], dirsSkipped: boolean) {
    const sid = props.sessionId;
    const pasted: string[] = [];
    const totalBytes = files.reduce((total, file) => total + file.size, 0);
    const activity = props.workspace.activities.begin({
      kind: "upload",
      label:
        files.length === 1
          ? (baseName(files[0].name) ?? fallbackName(files[0].type, 0))
          : `${files.length} files`,
      completed: 0,
      total: totalBytes,
    });
    let completedBytes = 0;
    try {
      const handle = await handleFor(sid);
      if (!handle) return;
      activity.update({ target: handle.root });
      for (let i = 0; i < files.length; i++) {
        const file = files[i];
        // File promises (the screenshot thumbnail) can arrive nameless —
        // fall back to a MIME-derived name rather than skipping the file.
        const name = baseName(file.name) ?? fallbackName(file.type, i);
        activity.update({ label: name, completed: completedBytes });
        await handle.upload(name, file, {
          onProgress: (uploaded, total) =>
            activity.update({
              completed: completedBytes + uploaded,
              total: totalBytes || total,
            }),
        });
        completedBytes += file.size;
        pasted.push(name);
      }
      if (dirsSkipped) showError(t("terminalDrop.noDirs"));
      const surface = props.surface();
      if (surface && pasted.length > 0)
        surface.pasteText(pasted.map(shellQuote).join(" "));
    } catch (err) {
      // The failure may have killed the cached sync (connection lost,
      // server closed it) — drop it so the next drop reopens fresh.
      const h = handles.get(sid);
      if (h) {
        h.stop();
        handles.delete(sid);
      }
      showError(
        isSourceTerminalUnavailableError(err)
          ? t("terminalDrop.terminalClosed")
          : err instanceof Error
            ? err.message
            : String(err),
      );
    } finally {
      activity.finish();
    }
  }

  function onDragEnter(e: DragEvent) {
    if (!isFileDrag(e)) return;
    e.preventDefault();
    enterDepth++;
    setDragOver(true);
  }

  function onDragOver(e: DragEvent) {
    if (!isFileDrag(e)) return;
    e.preventDefault(); // required for the drop to be accepted
    e.dataTransfer!.dropEffect = "copy";
    setDragOver(true);
  }

  function onDragLeave(e: DragEvent) {
    if (!isFileDrag(e)) return;
    if (--enterDepth <= 0) clearDrag();
  }

  function onDrop(e: DragEvent) {
    if (!isFileDrag(e)) return;
    e.preventDefault();
    clearDrag();
    const dt = e.dataTransfer!;
    // The files a drop carries — from `files`, or from file-kind items
    // when `files` is empty (macOS file promises).
    const dropped: File[] =
      dt.files.length > 0
        ? Array.from(dt.files)
        : Array.from(dt.items ?? [])
            .filter((it) => it.kind === "file")
            .map((it) => it.getAsFile())
            .filter((f): f is File => f !== null);
    // Folders come through `items` as directory entries (and as unreadable
    // zero-byte "files" without this filter) — skip them and say so.
    let dirsSkipped = false;
    const files: File[] = [];
    for (let i = 0; i < dropped.length; i++) {
      const entry = dt.items[i]?.webkitGetAsEntry?.();
      if (entry?.isDirectory) {
        dirsSkipped = true;
        continue;
      }
      files.push(dropped[i]);
    }
    if (files.length === 0) {
      if (dirsSkipped) showError(t("terminalDrop.noDirs"));
      return;
    }
    queue = queue.then(() => uploadFiles(files, dirsSkipped));
  }

  const banner = (): JSX.CSSProperties => ({
    "background-color": props.theme.solidPanelBg,
    border: `1px solid ${props.theme.border}`,
    padding: `${props.scale.controlY}px ${props.scale.controlX}px`,
    "font-size": `${props.scale.sm}px`,
    display: "flex",
    "align-items": "center",
    gap: `${props.scale.gap}px`,
  });

  const hoverLabel = (): string => {
    const root = handles.get(props.sessionId)?.root;
    return root
      ? tp("terminalDrop.hoverCwd", { cwd: root })
      : t("terminalDrop.hover");
  };

  return (
    <div
      style={{ position: "relative", width: "100%", height: "100%" }}
      onDragEnter={onDragEnter}
      onDragOver={onDragOver}
      onDragLeave={onDragLeave}
      onDrop={onDrop}
    >
      {props.children}
      {/* Hover affordance. pointer-events:none so the overlay itself never
          re-targets the drag (which would churn enter/leave). */}
      <Show when={dragOver()}>
        <div
          style={{
            position: "absolute",
            inset: 0,
            "pointer-events": "none",
            display: "flex",
            "align-items": "center",
            "justify-content": "center",
            border: `2px dashed ${props.theme.accent}`,
            "box-sizing": "border-box",
            "background-color": "rgba(0,0,0,0.25)",
            "z-index": z.exitedBanner,
          }}
        >
          <div style={banner()}>{hoverLabel()}</div>
        </div>
      </Show>
      <Show when={error()}>
        <div
          style={{
            ...banner(),
            position: "absolute",
            bottom: "8px",
            left: "50%",
            transform: "translateX(-50%)",
            "max-width": "90%",
            "z-index": z.exitedBanner,
          }}
        >
          <mark
            style={{ ...ui.badge, "background-color": "rgba(255,100,100,0.3)" }}
          >
            {t("terminalDrop.failed")}
          </mark>
          <span style={{ overflow: "hidden", "text-overflow": "ellipsis" }}>
            {error()}
          </span>
        </div>
      </Show>
    </div>
  );
}
