/**
 * Per-file cursor + scroll memory. An editor tile is re-created when navigation
 * swaps a pane to another file (BlitTile is keyed on its assignment), so without
 * this, returning to a file lands at the top with the cursor reset. Each editor
 * saves its position on unmount and restores it on its first load, making
 * navigation feel like the file's editor was kept alive.
 *
 * Session-scoped (a plain module Map); not persisted across reloads.
 */
export type EditorPosition = { anchor: number; head: number; top: number };

const positions = new Map<string, EditorPosition>();

// NUL separator: can't appear in a connection id or a path, so prefix
// matching in `editorRecencySnapshot` is exact. Kept as an escape — a raw
// NUL byte makes git treat the file as binary.
const SEP = "\u0000";

const key = (connectionId: string, path: string): string =>
  `${connectionId}${SEP}${path}`;

export function rememberEditorPosition(
  connectionId: string,
  path: string,
  pos: EditorPosition,
): void {
  // Delete-then-set keeps the map ordered by last touch, which is what
  // `editorRecencySnapshot` reads off the iteration order.
  const k = key(connectionId, path);
  positions.delete(k);
  positions.set(k, pos);
}

export function recallEditorPosition(
  connectionId: string,
  path: string,
): EditorPosition | null {
  return positions.get(key(connectionId, path)) ?? null;
}

/** Absolute path → recency rank (0 = most recently touched) for one
 *  connection's remembered files. Feeds the @-search recency boost
 *  (ide/fileIndex.ts). */
export function editorRecencySnapshot(
  connectionId: string,
): Map<string, number> {
  const prefix = `${connectionId}${SEP}`;
  const ranked = new Map<string, number>();
  const keys = [...positions.keys()];
  for (let i = keys.length - 1; i >= 0; i--) {
    const k = keys[i];
    if (k.startsWith(prefix)) {
      ranked.set(k.slice(prefix.length), ranked.size);
    }
  }
  return ranked;
}
