/** A user-visible operation that may take long enough to need status UI. */
export interface BlitActivity {
  id: number;
  kind: "upload" | "download" | "sync" | "search" | "operation";
  /** The item being acted on: normally a file name or path. */
  label: string;
  /** Optional destination/source, such as a surface title or terminal cwd. */
  target?: string;
  /** Completed and total work in producer-defined units (bytes for uploads). */
  completed?: number;
  total?: number;
  startedAt: number;
}

export interface BlitActivityUpdate {
  label?: string;
  target?: string;
  completed?: number;
  total?: number;
}

export interface BlitActivityHandle {
  readonly id: number;
  update(update: BlitActivityUpdate): void;
  finish(): void;
}

/** Workspace-scoped registry consumed by status bars and other shell chrome.
 * Producers own handles, update them as work advances, and always finish them
 * on success, failure, or cancellation. */
export class BlitActivityStore {
  private readonly records = new Map<number, BlitActivity>();
  private readonly listeners = new Set<() => void>();
  private snapshot: readonly BlitActivity[] = [];
  private nextId = 1;

  subscribe = (listener: () => void): (() => void) => {
    this.listeners.add(listener);
    return () => this.listeners.delete(listener);
  };

  getSnapshot = (): readonly BlitActivity[] => this.snapshot;

  begin(activity: Omit<BlitActivity, "id" | "startedAt">): BlitActivityHandle {
    const id = this.nextId++;
    this.records.set(id, {
      ...activity,
      id,
      startedAt: Date.now(),
    });
    this.emit();
    let finished = false;
    return {
      id,
      update: (update) => {
        if (finished) return;
        const current = this.records.get(id);
        if (!current) return;
        this.records.set(id, { ...current, ...update });
        this.emit();
      },
      finish: () => {
        if (finished) return;
        finished = true;
        if (this.records.delete(id)) this.emit();
      },
    };
  }

  clear(): void {
    if (this.records.size === 0) return;
    this.records.clear();
    this.emit();
  }

  private emit(): void {
    this.snapshot = [...this.records.values()];
    for (const listener of this.listeners) listener();
  }
}
