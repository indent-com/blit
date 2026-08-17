/**
 * The raise stack behind xdg_activation_v1 in the non-BSP main view.
 *
 * BSP tiles: a client that asks to be activated is either already in a pane --
 * where it is simply focused -- or takes the focused pane, whose occupant stays
 * visible in the dock. Either way nothing needs remembering. The non-BSP
 * main view is a single slot -- honouring an activation there means covering
 * whatever the user was looking at. A bare `focusSurface()` therefore loses the
 * previous occupant: the surface that ends up on screen is the one that spoke
 * last, and when it closes the slot falls back to whatever the *core* happens
 * to consider focused, which is rarely where the user came from.
 *
 * So activations push. The displaced occupant is remembered here, and when the
 * activated surface goes away the slot restores the entry beneath it, the way a
 * stacking WM lowers a window and reveals the one behind it.
 *
 * Entries are assignment strings -- the namespace `focusedAssignment()` returns
 * and `focusAssignment()` accepts, covering all four things the main view can
 * show (surface, tile, web pane, bare session id). Keeping them opaque is what
 * lets an activation restore a *terminal* it covered, not just a surface.
 */

/** Depth cap. An app in a notification storm must not grow this without
 *  bound; sixteen is far past what anyone navigates back through, and the
 *  entries it drops are the oldest, which are also the least likely to still
 *  be restorable. */
export const ACTIVATION_STACK_LIMIT = 16;

/**
 * Record `displaced` as the thing `activated` is covering.
 *
 * Both are removed before the push, which is what makes the stack MRU rather
 * than an append log: re-activating a surface that is already buried does not
 * leave a second copy behind to be restored twice, and a slot the user
 * returned to on their own moves back to the top instead of aliasing an older
 * position.
 */
export function pushActivation(
  stack: readonly string[],
  displaced: string | null,
  activated: string,
): string[] {
  const next = stack.filter((e) => e !== activated && e !== displaced);
  // Activating what is already on screen displaces nothing.
  if (displaced && displaced !== activated) next.push(displaced);
  return next.length > ACTIVATION_STACK_LIMIT
    ? next.slice(next.length - ACTIVATION_STACK_LIMIT)
    : next;
}

/**
 * Take the newest entry that can still be shown, discarding any that died
 * while they were buried (the surface closed, the tab was closed).
 *
 * `restore` is null when nothing is left, and the caller then does what it did
 * before there was a stack -- clear the slot.
 */
export function popActivation(
  stack: readonly string[],
  isRestorable: (assignment: string) => boolean,
): { restore: string | null; stack: string[] } {
  const next = stack.slice();
  while (next.length > 0) {
    const candidate = next.pop()!;
    if (isRestorable(candidate)) return { restore: candidate, stack: next };
  }
  return { restore: null, stack: next };
}
