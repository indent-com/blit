/**
 * An `xdg_activation_v1` request is an app asking to come forward. Answering it
 * by actually giving it the view is how a talkative client ends up fighting the
 * user: an activation token is cheap and its delivery unacknowledged, so a
 * client repeats the request several times a second, and every repeat lands
 * *after* whatever the user just picked. What the user sees is their choice
 * flashing up and being dragged back off — and "insisting" only working when a
 * click happens to fall in a gap between requests.
 *
 * So an activation buys a highlight, not the view. The surface that asked is lit
 * for {@link ATTENTION_MS} wherever it already is — its dock card, its pane —
 * and nothing moves, so nothing can be taken.
 *
 * The bookkeeping is here, and pure, because the interesting part of it is the
 * debounce: a repeat arriving inside an open window must leave the window
 * alone. Re-arming it would restart the animation from the top on every
 * request, so a chatty client would strobe its card rather than pulse it once,
 * and the window would never close while it kept asking.
 *
 * Both functions return the map they were given, by identity, when nothing
 * changed. Callers hold it in a signal, so a fresh-but-equal Map would re-run
 * the render and restart the very animation the debounce exists to protect.
 */

/** How long one activation stays lit. Matches the CSS animation's duration. */
export const ATTENTION_MS = 1400;

/** Assignments currently demanding attention, mapped to when they stop. */
export type Attention = ReadonlyMap<string, number>;

/**
 * Light `assignment` until `now + windowMs`, unless it is already lit — a
 * repeat inside the window is the retransmission this whole module exists to
 * absorb, and the answer to it is the highlight that is already on screen.
 */
export function armAttention(
  prev: Attention,
  assignment: string,
  now: number,
  windowMs: number = ATTENTION_MS,
): Attention {
  const until = prev.get(assignment);
  if (until != null && until > now) return prev;
  const next = new Map(prev);
  next.set(assignment, now + windowMs);
  return next;
}

/** Drop every assignment whose window has closed. */
export function expireAttention(prev: Attention, now: number): Attention {
  let next: Map<string, number> | null = null;
  for (const [assignment, until] of prev) {
    if (until > now) continue;
    next ??= new Map(prev);
    next.delete(assignment);
  }
  return next ?? prev;
}
