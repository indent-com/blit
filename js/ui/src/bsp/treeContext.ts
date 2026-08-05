import type { WebPaneHostRegistrar } from "../WebPaneHost";
/**
 * The BSP tree's shared context, deliberately in its own module.
 *
 * `createContext` mints an identity: a Provider and a consumer only match
 * when both hold the *same* object. Defining it alongside the components
 * meant every hot reload of `BSPContainer.tsx` created a fresh context, so
 * consumers re-rendered against it while ancestor Providers still carried
 * the previous one — `useContext` then returned undefined and the first
 * field read threw. Keeping it here means the identity survives reloads of
 * the components that use it.
 */

import { createContext, useContext } from "solid-js";
import type { BlitTerminalSurface, TerminalPalette } from "@blit-sh/core";
import type { BSPSplit } from "@blit-sh/core/bsp";

/** Props that stay constant through the BSPPane recursion tree.  Hoisted
 *  into context so each level only passes the values that actually change. */
export interface BSPTreeCtx {
  connectionId: string;
  connectionLabels?: Map<string, string>;
  multiPane: boolean;
  /** Coarse pointer: the pane's ✕ has no hover to reveal it, so it stays up. */
  isMobileTouch?: boolean;
  onFocusPane: (paneId: string) => void;
  /** Close whatever the pane holds — terminal, surface, IDE tile or web pane.
   *  Same targets, and the same order, as Ctrl+Alt+Shift+Q. */
  onClosePane: (paneId: string) => void;
  onCreateInPane?: (
    paneId: string,
    command?: string,
    connectionId?: string,
  ) => void;
  onSwitcher?: () => void;
  onHelp?: () => void;
  onResize: (
    split: BSPSplit,
    indexA: number,
    indexB: number,
    fraction: number,
  ) => void;
  palette: TerminalPalette;
  fontFamily: string;
  fontSize: number;
  tabMemory: Record<string, number>;
  onRender?: (renderMs?: number) => void;
  /** Called with each terminal pane's surface as it mounts (and null as it
   *  unmounts), so the workspace can attach hyperlink hover and activation to
   *  every split rather than only the focused one — hovering follows the
   *  pointer, not focus. */
  onTerminalSurface?: (surface: BlitTerminalSurface | null) => void;
  /** Whether a session's connection is read-only (an `.ro` share): its
   *  terminals render without input affordances instead of silently
   *  swallowing keystrokes the server will refuse. */
  isSessionReadOnly?: (sessionId: string) => boolean;
  /** Open an IDE tile from within a tile (commit view → editor). */
  onOpenTile?: (assignment: string) => void;
  /** Drop a dragged IDE tile assignment into a specific pane. */
  onDropTile?: (
    assignment: string,
    paneId: string,
    sourcePaneId?: string,
  ) => void;
  /** Register the visual host for a Workspace-owned persistent web pane. */
  registerWebPaneHost?: WebPaneHostRegistrar;
  /** The pane currently soloed to fill the workspace, if any. Its siblings
   *  are hidden rather than unmounted, so nothing is torn down and unsolo is
   *  free (see `BSPContainer`'s `soloedPaneId`). */
  soloedPaneId: string | null;
  /** Solo `paneId`, or unsolo if it already is. */
  onToggleSolo: (paneId: string) => void;
}

export const BSPTreeContext = createContext<BSPTreeCtx>();

/** Read the tree context. Callers are always rendered under the Provider in
 *  `BSPContainer`; an undefined here means that invariant broke, and failing
 *  loudly beats every consumer throwing on its first field access. */
export function useBSPTree(): BSPTreeCtx {
  const ctx = useContext(BSPTreeContext);
  if (!ctx) throw new Error("BSP tree context used outside its Provider");
  return ctx;
}
