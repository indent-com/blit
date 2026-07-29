/**
 * A framework-agnostic reactive surface for the live handles.
 *
 * The fs/git/lsp mirrors are plain data, replaced wholesale on each server
 * push. Rather than have every client wire the push callbacks by hand, the
 * handles expose a {@link ReactiveStore}: `subscribe` for change
 * notifications and a monotonic `revision` to detect staleness. This is the
 * `useSyncExternalStore` contract (React) and consumes directly into Solid's
 * `from`, so any UI gets reactivity for free.
 */

/** Something whose data changes over time and can be observed generically. */
export interface ReactiveStore {
  /** Register a listener fired after every applied change; returns an
   *  unsubscribe. Adding the same function twice registers it once. */
  subscribe(listener: () => void): () => void;
  /** Bumped on every applied change. Read it, do work, and compare on the
   *  next notification to know whether the snapshot moved. */
  readonly revision: number;
}

/**
 * The producer half of a {@link ReactiveStore}: hold one per live handle,
 * expose its `subscribe`/`revision`, and call {@link emit} once after each
 * applied mirror change.
 */
export class Notifier implements ReactiveStore {
  #listeners = new Set<() => void>();
  #revision = 0;

  get revision(): number {
    return this.#revision;
  }

  subscribe = (listener: () => void): (() => void) => {
    this.#listeners.add(listener);
    return () => {
      this.#listeners.delete(listener);
    };
  };

  /** Advance the revision and notify every current listener. Snapshot the
   *  set first so a listener that unsubscribes mid-notify is well-defined. */
  emit(): void {
    this.#revision++;
    for (const listener of [...this.#listeners]) listener();
  }
}
