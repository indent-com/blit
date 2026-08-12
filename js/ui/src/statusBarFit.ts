/**
 * Does the status bar still have room for the icon cluster in its right end?
 *
 * The bar's middle region — the identity: session/app title, path, cwd — is the
 * only elastic child (`flex: 1` over a zero basis), so it is exactly the space
 * the fixed-width chrome leaves behind. When that space drops under a floor the
 * title is no longer readable, and the icons are worth more folded into a menu
 * than spelled out.
 *
 * Collapsing is measured, not guessed from a viewport breakpoint: how much
 * chrome the bar carries depends on what is focused (an editor adds Save, Def,
 * Refs…), and a touch device draws every glyph at double size.
 */

/** Extra px the identity must clear before the cluster unfolds again. Without
 *  it a width sitting on the boundary would collapse, gain the icons' width
 *  back, expand, lose it again — a resize loop the user sees as a flicker. */
export const FIT_HYSTERESIS_PX = 12;

export type FitSample = {
  /** Width the identity region currently occupies. */
  identity: number;
  /** Width the right-end cluster currently occupies — the full icon row, or
   *  just the menu button when already collapsed. */
  icons: number;
  /** Width the cluster occupied the last time it was drawn expanded, or null
   *  before it has ever been measured that way. */
  expandedIcons: number | null;
  /** Floor under which the identity stops being worth showing. */
  minIdentity: number;
};

export function nextCompact(compact: boolean, s: FitSample): boolean {
  if (!compact) return s.identity < s.minIdentity;
  // Unfolding hands the difference between the two cluster widths back to the
  // identity; only do it when what remains still clears the floor.
  if (s.expandedIcons === null) return true;
  const unfolded = s.identity - (s.expandedIcons - s.icons);
  return unfolded < s.minIdentity + FIT_HYSTERESIS_PX;
}
