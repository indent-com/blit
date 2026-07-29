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
import type { TerminalPalette } from "@blit-sh/core";
import type { BSPSplit } from "@blit-sh/core/bsp";

/** Props that stay constant through the BSPPane recursion tree.  Hoisted
 *  into context so each level only passes the values that actually change. */
export interface BSPTreeCtx {
  connectionId: string;
  connectionLabels?: Map<string, string>;
  multiPane: boolean;
  onFocusPane: (paneId: string) => void;
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
  /** Open an IDE tile from within a tile (commit view → editor). */
  onOpenTile?: (assignment: string) => void;
  /** Drop a dragged IDE tile assignment into a specific pane. */
  onDropTile?: (assignment: string, paneId: string) => void;
  /** Register the visual host for a Workspace-owned persistent web pane. */
  registerWebPaneHost?: WebPaneHostRegistrar;
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
