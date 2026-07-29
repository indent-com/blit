/**
 * FileViewSwitcher — the in-tile tab strip that flips one file between its
 * editor and its three git diffs (staged / unstaged / all), without leaving the
 * pane. Clicking a tab just opens the corresponding tile assignment, which
 * *replaces* the current tile (see Workspace.openTile / BSPContainer.moveToPane)
 * — so the switch is seamless and survives reload via the same hash-persisted
 * assignment.
 *
 * Shared by BlitEditor and BlitDiff so the strip is identical in both.
 */

import { For } from "solid-js";
import type { ConnectionId } from "@blit-sh/core";
import {
  editorAssignment,
  diffAssignment,
  previewAssignment,
} from "@blit-sh/core/bsp";
import { previewKindFor } from "./previewKind";
import type { DiffSide } from "@blit-sh/core/bsp";
import type { Theme } from "../theme";

/** The view a file tile is currently showing. */
export type FileView = "editor" | "preview" | DiffSide;

const TABS: { view: FileView; label: string; title: string }[] = [
  { view: "editor", label: "Edit", title: "Editor" },
  {
    view: "preview",
    label: "Preview",
    title: "Rendered preview — images, markdown, HTML",
  },
  { view: "staged", label: "Staged", title: "Staged diff — HEAD × index" },
  {
    view: "unstaged",
    label: "Unstaged",
    title: "Unstaged diff — index × worktree",
  },
  { view: "worktree", label: "All", title: "All changes — HEAD × worktree" },
];

export function FileViewSwitcher(props: {
  current: FileView;
  connectionId: ConnectionId;
  path: string;
  onOpenTile: (assignment: string) => void;
  theme: Theme;
  fontFamily: string;
  fontSize: number;
}) {
  // An untracked file (a brand-new path) is really an unstaged edit; highlight
  // the Unstaged tab for it so the strip never shows "nothing selected".
  const active = (): FileView =>
    props.current === "untracked" ? "unstaged" : props.current;
  const assignmentFor = (v: FileView) =>
    v === "editor"
      ? editorAssignment(props.connectionId, props.path)
      : v === "preview"
        ? previewAssignment(props.connectionId, props.path)
        : diffAssignment(props.connectionId, props.path, v);
  // Preview only appears for a file that has one, so the strip does not
  // offer a tab that can only say "no preview for this file type".
  const tabs = () =>
    TABS.filter((t) => t.view !== "preview" || previewKindFor(props.path));
  const px = Math.round(props.fontSize * 0.82);
  return (
    <div
      style={{
        display: "flex",
        "align-items": "center",
        gap: "1px",
        "flex-shrink": 0,
      }}
    >
      <For each={tabs()}>
        {(t) => {
          const on = () => active() === t.view;
          return (
            <button
              onClick={() => {
                if (!on()) props.onOpenTile(assignmentFor(t.view));
              }}
              title={t.title}
              style={{
                display: "inline-flex",
                "align-items": "center",
                background: on() ? props.theme.hoverBg : "transparent",
                border: "none",
                color: on() ? props.theme.fg : props.theme.dimFg,
                cursor: on() ? "default" : "pointer",
                padding: "2px 6px",
                "font-family": props.fontFamily,
                "font-size": `${px}px`,
              }}
            >
              {t.label}
            </button>
          );
        }}
      </For>
    </div>
  );
}
