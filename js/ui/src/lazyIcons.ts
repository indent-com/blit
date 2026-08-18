/**
 * Asking for artwork only for the rows a viewer can actually see.
 *
 * Both application lists — the Manage panel's catalog and the switcher's
 * Applications section — are the whole of what a machine has installed, which
 * on one with a games library is nine hundred rows. Their icons are tens of
 * megabytes; the dozen on screen are a few hundred kilobytes. So a row asks for
 * its own only when it comes near the viewport, and the observer's own batching
 * turns a scroll into one request rather than one per row.
 *
 * Shared because two things about it are easy to get wrong, and were:
 *
 *  - **The root must be the scrolling element**, not the page. `rootMargin`
 *    grows the root's rectangle and nothing else, so an observer rooted at the
 *    viewport is still clipped by the list's own overflow — the margin buys no
 *    lookahead at all, and the artwork never catches up with the scroll.
 *  - **A child's `ref` runs before its parent's.** A row registering straight
 *    from its ref finds no observer, because the scroller that roots it does
 *    not exist yet. Registration is deferred to `onMount` for that reason.
 */

import { createMemo, createSignal, onCleanup, onMount } from "solid-js";

/** How far past the list's edge a row is considered worth asking about.
 *
 *  Several screens: a request costs a child process on the far end whatever it
 *  asks for, so reaching well past the fold is what lets one of them cover a
 *  whole flick of the wheel. */
const LOOKAHEAD = "1500px";

/** The attribute the row's token is carried on, read back in the callback. */
const TOKEN = "lazyIconToken";

export interface LazyIcons {
  /** `ref` for the scrolling element the rows live in. */
  setRoot: (element: HTMLElement) => void;
  /** `ref` for one row. `token` is whatever identifies it to the caller — an
   *  application id, or a connection and an id together. */
  watch: (element: HTMLElement, token: string) => void;
}

/**
 * Call `request` with the tokens of rows that have come into view.
 *
 * Rows stay observed after asking rather than being released once they have:
 * the caller is expected to drop tokens it already holds, so re-entering the
 * list costs nothing — and it is the only thing that ever asks again for a row
 * whose answer was lost on the way back.
 */
export function createLazyIcons(
  request: (tokens: string[]) => void,
): LazyIcons {
  const [root, setRoot] = createSignal<HTMLElement>();

  const observer = createMemo<IntersectionObserver | undefined>(() => {
    const element = root();
    // Absent under jsdom, and there is nothing to observe before the list
    // exists. Rows fall back to asking outright, so a client without it still
    // shows artwork — it just asks for more of it.
    if (!element || typeof IntersectionObserver === "undefined") {
      return undefined;
    }
    const watcher = new IntersectionObserver(
      (entries) => {
        const tokens = entries
          .filter((entry) => entry.isIntersecting)
          .map((entry) => (entry.target as HTMLElement).dataset[TOKEN])
          .filter((token): token is string => token !== undefined);
        if (tokens.length > 0) request(tokens);
      },
      { root: element, rootMargin: LOOKAHEAD },
    );
    onCleanup(() => watcher.disconnect());
    return watcher;
  });

  return {
    setRoot,
    watch: (element, token) => {
      element.dataset[TOKEN] = token;
      onMount(() => {
        const watcher = observer();
        if (!watcher) {
          request([token]);
          return;
        }
        watcher.observe(element);
        onCleanup(() => watcher.unobserve(element));
      });
    },
  };
}
