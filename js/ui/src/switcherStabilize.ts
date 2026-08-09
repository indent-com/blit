/**
 * Identity stabilization for the switcher overlay's section list.
 *
 * The switcher rebuilds its section/item objects on every workspace snapshot
 * emit (terminal titles, usedRows, output-driven title sync). Solid's <For>
 * keys rows by object identity, so a rebuild of identical data would still
 * dispose and recreate every row — including the live terminal/surface
 * thumbnails, whose remount is expensive (new canvas, new video subscription,
 * forced server keyframe) and visible as flashing. Reusing the previous
 * objects when their content is unchanged keeps those rows mounted.
 */

export interface KeyedItem {
  key: string;
}

export interface KeyedSection<T extends KeyedItem> {
  title: string;
  items: T[];
}

/** Shallow own-enumerable-field equality. Item payloads are primitives;
 *  the few object fields (layouts, search hits) fail === when they churn,
 *  which simply misses the reuse — correctness never depends on it. */
function sameItem(a: KeyedItem, b: KeyedItem): boolean {
  const aKeys = Object.keys(a) as (keyof KeyedItem)[];
  const bKeys = Object.keys(b);
  if (aKeys.length !== bKeys.length) return false;
  return aKeys.every((k) => a[k] === b[k]);
}

/**
 * Reuse previous section/item objects whose visible content is unchanged.
 * Sections match by position and title; items match by `key`, so a reorder
 * reuses item objects inside a fresh section (Solid moves the rows instead of
 * remounting them).
 */
export function stabilizeSections<T extends KeyedItem>(
  prev: KeyedSection<T>[] | undefined,
  next: KeyedSection<T>[],
): KeyedSection<T>[] {
  if (!prev) return next;
  return next.map((section, i) => {
    const old = prev[i];
    if (!old || old.title !== section.title) return section;
    const oldByKey = new Map(old.items.map((item) => [item.key, item]));
    const items = section.items.map((item) => {
      const oldItem = oldByKey.get(item.key);
      return oldItem && sameItem(oldItem, item) ? oldItem : item;
    });
    const unchanged =
      items.length === old.items.length &&
      items.every((item, j) => item === old.items[j]);
    return unchanged ? old : { ...section, items };
  });
}
