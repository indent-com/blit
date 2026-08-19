/**
 * An `xdg_activation_v1` request is an app asking to come forward. Answering it
 * by actually giving it the view is how a talkative client ends up fighting the
 * user: an activation token is cheap and its delivery unacknowledged, so a
 * client repeats the request several times a second, and every repeat lands
 * *after* whatever the user just picked. What the user sees is their choice
 * flashing up and being dragged back off — and "insisting" only working when a
 * click happens to fall in a gap between requests.
 *
 * So an activation buys a mark, not the view: the surface that asked is marked
 * wherever it already is — its dock card, its pane, the surface count, the
 * switcher — and nothing moves, so nothing can be taken.
 *
 * A mark waits rather than expires. It has no clock and no animation anywhere in
 * it: a request that timed itself out would be missed by whoever was not looking
 * at that moment, which is most of the point of marking it at all. So the set
 * below is the whole lifecycle — an entry goes in when a window asks, and comes
 * out only when the window is looked at or goes away.
 *
 * That also disposes of the repeat problem without a debounce: adding an
 * assignment that is already in the set is a no-op, so a client asking ten times
 * a second changes nothing ten times a second. {@link settleAttention} returns
 * its input by identity when nothing changed for the same reason — the caller
 * holds it in a signal, and a fresh-but-equal Set would re-render for nothing.
 */

/**
 * Retire the marks that have been answered.
 *
 * Answered means looked at, or moot: the surface reached the front, or it is
 * gone. `onTop` is the one surface the viewer is actually looking at, which is a
 * different slot depending on whether a layout is up, so the caller resolves it.
 */
export function settleAttention(
  prev: ReadonlySet<string>,
  onTop: string | null,
  isLive: (assignment: string) => boolean,
): ReadonlySet<string> {
  let next: Set<string> | null = null;
  for (const assignment of prev) {
    if (assignment !== onTop && isLive(assignment)) continue;
    next ??= new Set(prev);
    next.delete(assignment);
  }
  return next ?? prev;
}
