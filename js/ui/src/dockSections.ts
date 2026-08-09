/**
 * Which left-dock sections are folded, and what a click on a header means.
 *
 * Two independent reasons a section can be closed: the user collapsed it (a
 * persisted preference) or it does not apply to the current root — a commit
 * log over a directory that is not a repository has nothing to say, so it
 * folds itself away instead of standing there explaining. The distinction
 * matters at both ends: an auto-fold must not overwrite the preference (the
 * next root probably is a repo), and clicking an auto-folded header must open
 * it rather than record a collapse it already looks like it has.
 */

export type LeftPanel = "explorer" | "log" | "problems";

/** Fixed section order in the accordion. Defined here (not in LeftDock.tsx)
 *  so non-JSX modules like panelHash can read it without pulling in a
 *  component file. */
export const LEFT_PANELS: LeftPanel[] = ["explorer", "log", "problems"];


/**
 * The set LeftDock renders as collapsed: everything the user collapsed, plus
 * the inapplicable sections they have not overridden.
 */
export function foldedSections(
  userCollapsed: ReadonlySet<LeftPanel>,
  inapplicable: ReadonlySet<LeftPanel>,
  overridden: ReadonlySet<LeftPanel>,
): Set<LeftPanel> {
  const out = new Set(userCollapsed);
  for (const panel of inapplicable) if (!overridden.has(panel)) out.add(panel);
  return out;
}

/**
 * What toggling `panel`'s header changes. Auto-folded sections toggle the
 * override (a one-off "show it anyway", not a preference); everything else
 * toggles the persisted collapse.
 */
export function toggleSection(
  panel: LeftPanel,
  userCollapsed: ReadonlySet<LeftPanel>,
  inapplicable: ReadonlySet<LeftPanel>,
  overridden: ReadonlySet<LeftPanel>,
): { userCollapsed: Set<LeftPanel>; overridden: Set<LeftPanel> } {
  const nextCollapsed = new Set(userCollapsed);
  const nextOverridden = new Set(overridden);
  if (inapplicable.has(panel) && !userCollapsed.has(panel)) {
    if (nextOverridden.has(panel)) nextOverridden.delete(panel);
    else nextOverridden.add(panel);
  } else if (nextCollapsed.has(panel)) nextCollapsed.delete(panel);
  else nextCollapsed.add(panel);
  return { userCollapsed: nextCollapsed, overridden: nextOverridden };
}

/**
 * Overrides worth keeping: one lapses as soon as its section applies again, so
 * the next root without a repository folds the log afresh rather than
 * inheriting a decision made about a different root.
 */
export function liveOverrides(
  overridden: ReadonlySet<LeftPanel>,
  inapplicable: ReadonlySet<LeftPanel>,
): Set<LeftPanel> {
  return new Set([...overridden].filter((panel) => inapplicable.has(panel)));
}
