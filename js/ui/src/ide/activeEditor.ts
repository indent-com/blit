/**
 * The focused tile's chrome (filename, navigation, state, and actions) lives
 * in the global StatusBar rather than a per-tile header. An editor publishes
 * itself here when it gains focus; a diff publishes on mount and click. The
 * StatusBar reads the current controller and renders identity + actions.
 *
 * The accessors close over the owning tile's signals, so the StatusBar stays
 * reactive without any prop threading. A controller is cleared when its tile
 * unmounts (see clearActiveEditor), so the StatusBar never reads a disposed
 * signal.
 */
import { createSignal } from "solid-js";

export type EditorBanner = { text: string; tone: "err" | "warn" } | null;

export type EditorController = {
  kind: "editor";
  connectionId: string;
  /** Absolute path; the bar renders it whole, remote prefix included. */
  path: string;
  /** Reactive state (each reads the owning editor's signals). */
  dirty: () => boolean;
  banner: () => EditorBanner;
  lspMsg: () => string | null;
  lspAvailable: () => boolean;
  readOnly: () => boolean;
  /** True while an on-disk conflict / external change is pending. */
  conflicted: () => boolean;
  /** Actions. */
  save: () => void;
  discard: () => void;
  reload: () => void;
  overwrite: () => void;
  goToDefinition: () => void;
  findReferences: () => void;
  /** Open the rename box on the symbol at the cursor (F2). */
  renameSymbol: () => void;
  /** Open the document outline picker (⌘⇧O). */
  showOutline: () => void;
  /** Switch this file's tile between editor and its diff views. */
  onOpenTile?: (assignment: string) => void;
};

export type DiffController = {
  kind: "diff";
  connectionId: string;
  /** Absolute path; the bar renders it whole, remote prefix included. */
  path: string;
  /** Which comparison this tile shows (drives the view switcher). */
  side: "unstaged" | "staged" | "untracked" | "worktree";
  /** Human label for the comparison (shown when there is no switcher). */
  sideLabel: string;
  viewMode: () => "unified" | "split";
  toggleViewMode: () => void;
  onOpenTile?: (assignment: string) => void;
};

/** A rendered-file preview. It owns no state the bar can act on — the bar
 *  shows the path and the view switcher, nothing else. */
export type PreviewController = {
  kind: "preview";
  connectionId: string;
  /** Absolute path; the bar renders it whole, remote prefix included. */
  path: string;
  onOpenTile?: (assignment: string) => void;
};

export type CommitController = {
  kind: "commit";
  connectionId: string;
  /** The repository this commit belongs to, for the bar's location. */
  repoPath: string;
  /** Abbreviated oid, the bar's identity for this tile. */
  short: string;
  /** First line of the commit message. */
  subject: string;
  /** A commit tile is a patch across many files, so it carries the same
   *  unified ⇄ side-by-side choice a diff does. */
  viewMode: () => "unified" | "split";
  toggleViewMode: () => void;
};

/** Any tile whose chrome the StatusBar renders. */
export type TileChrome =
  | EditorController
  | DiffController
  | CommitController
  | PreviewController;

const [activeEditor, setActiveEditor] = createSignal<TileChrome | null>(null);

export { activeEditor };

/** Publish `c` as the focused tile chrome. */
export function registerActiveEditor(c: TileChrome): void {
  setActiveEditor(c);
}

/** Clear `c` if it is still the active one (call on tile unmount). */
export function clearActiveEditor(c: TileChrome): void {
  setActiveEditor((cur) => (cur === c ? null : cur));
}

/**
 * Drop whatever chrome is registered, whoever owns it.
 *
 * A tile only ever clears itself on unmount, which is enough in the
 * single-tile layout (focusing a terminal dismisses the tile) but not
 * under BSP, where a tile and a terminal are on screen at once: focus
 * leaves the tile, nothing unmounts, and the bar keeps showing the
 * editor's identity and its Save/Def/Refs actions while the user types
 * in a terminal. Called when a terminal takes focus, which is the one
 * moment the tile chrome is definitely not what the bar should show.
 */
export function clearTileChrome(): void {
  setActiveEditor(null);
}
