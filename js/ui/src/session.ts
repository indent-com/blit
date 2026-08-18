/**
 * Client mirror for the `blit.session.v1` channel served by the session
 * supervisor extension (`extensions/session`).
 *
 * Outbound is JSON, one object per message; inbound is a bare text line
 * (`enable <id>`), because a Wasm guest has no JSON parser and the vocabulary
 * is three verbs. The extension sends complete state rather than deltas: the
 * managed set is what an operator typed, so it is small, and a panel that can
 * only ever be correct beats one that avoids resending a few hundred bytes.
 *
 * Icons are the exception to "complete state": they are asked for, one batch of
 * ids at a time, and answered one message per id. Artwork is three orders of
 * magnitude larger than everything else here — a catalog of a thousand entries
 * is a few tens of kilobytes of names and tens of megabytes of icons — so the
 * panel asks only for the rows it is about to draw.
 *
 * This is an extension protocol, not a server packet family, which is why it
 * lives in the app rather than in `@blit-sh/core` — the same split
 * {@link ./systemd.ts} makes.
 */

import type {
  ChannelHandle,
  ChannelOpenOptions,
  ReactiveStore,
} from "@blit-sh/core";
import { Notifier } from "@blit-sh/core";

/** The part of a connection this mirror needs: one named channel. */
export interface ChannelOpener {
  connectChannel(
    name: string,
    options?: ChannelOpenOptions,
  ): Promise<ChannelHandle>;
}

export const SESSION_CHANNEL = "blit.session.v1";

/** What the supervisor is doing about one application. */
export type SessionPhase = "running" | "backoff" | "starting" | "stopped";

/** One application the session manages. */
export interface SessionApp {
  /** Desktop-entry id — the name `@session enable <id>` takes. */
  readonly id: string;
  readonly name: string;
  readonly enabled: boolean;
  readonly phase: SessionPhase;
  /** Consecutive failed starts; reset by a run that stays up. */
  readonly failures: number;
  /**
   * Windows counted from the identity the compositor stamped on the app's
   * Wayland socket — not from the app's self-asserted `app_id`, which is why
   * this number can be trusted.
   */
  readonly windows: number;
  readonly lastExit?: number;
  /** `WAYLAND_DISPLAY` basename of the running instance, when there is one. */
  readonly socket?: string;
}

/** One installed application that could be enabled. */
export interface SessionCatalogEntry {
  readonly id: string;
  readonly name: string;
}

/**
 * How many ids ride one request; the extension refuses more than this.
 *
 * Deliberately several screens' worth. A request costs one child process on the
 * far end whatever it asks for, so the batch size is what buys throughput while
 * a list is being scrolled — twelve rows per round trip cannot keep up with a
 * wheel, and forty-eight can.
 */
const ICON_BATCH = 48;

/**
 * How long an id is considered asked before it may be asked again.
 *
 * Not a retry timer so much as a leak stopper: an answer can be lost — the
 * supervisor bounds what it will queue for a panel that is not keeping up — and
 * without this the row that lost it keeps its placeholder for the life of the
 * channel, because the id is marked asked and nothing ever asks again.
 */
const ICON_RETRY_MS = 8_000;

/**
 * How long an icon request waits for company.
 *
 * A scrolling list reveals rows a handful at a time, and each of those bursts
 * would otherwise be its own request — and each request is a child process on
 * the far end. Collecting them for a moment first turns a flick of the wheel
 * into one round trip instead of six, and is short enough that a list sitting
 * still still fills in immediately.
 */
const ICON_COALESCE_MS = 120;

export interface SessionOptions {
  onClosed?(reason: number, detail: string): void;
}

export interface SessionHandle extends ReactiveStore {
  /** Managed applications, sorted by display name. */
  readonly apps: readonly SessionApp[];
  /** Everything installed, sorted by display name. Empty until it arrives. */
  readonly catalog: readonly SessionCatalogEntry[];
  /** False until the first state message lands. */
  readonly ready: boolean;
  /**
   * Artwork for one application: a data URL, `null` for "there is none", and
   * `undefined` for "nobody has asked yet".
   *
   * The three-way answer is what lets a row show a placeholder without either
   * flickering through it on the way to an icon or re-asking forever for an
   * application that has none.
   */
  icon(id: string): string | null | undefined;
  /** Ask for the icons of these applications, skipping any already known or
   *  already in flight. Safe to call on every render. */
  requestIcons(ids: readonly string[]): void;
  /** Run it now, and on every session start. */
  enable(id: string): void;
  /** Stop it now, and on every session start. */
  disable(id: string): void;
  /** Run it now without changing what the next session start does. */
  start(id: string): void;
  /** Stop it now without changing what the next session start does. */
  stop(id: string): void;
  /** Stop it and drop it from the managed list entirely. */
  forget(id: string): void;
  /** Ask for fresh state and a fresh catalog. */
  resync(): void;
  close(): void;
}

function phaseOf(value: unknown): SessionPhase {
  return value === "running" ||
    value === "backoff" ||
    value === "starting" ||
    value === "stopped"
    ? value
    : "stopped";
}

function appOf(value: unknown): SessionApp | null {
  if (typeof value !== "object" || value === null) return null;
  const record = value as Record<string, unknown>;
  if (typeof record.id !== "string" || record.id.length === 0) return null;
  return {
    id: record.id,
    name: typeof record.name === "string" ? record.name : record.id,
    enabled: record.enabled === true,
    phase: phaseOf(record.phase),
    failures: typeof record.failures === "number" ? record.failures : 0,
    windows: typeof record.windows === "number" ? record.windows : 0,
    lastExit: typeof record.lastExit === "number" ? record.lastExit : undefined,
    socket: typeof record.socket === "string" ? record.socket : undefined,
  };
}

function entryOf(value: unknown): SessionCatalogEntry | null {
  if (typeof value !== "object" || value === null) return null;
  const record = value as Record<string, unknown>;
  if (typeof record.id !== "string" || record.id.length === 0) return null;
  return {
    id: record.id,
    name: typeof record.name === "string" ? record.name : record.id,
  };
}

const byName = (
  left: { name: string; id: string },
  right: { name: string; id: string },
): number =>
  left.name.localeCompare(right.name) || left.id.localeCompare(right.id);

/**
 * The message reducer, with no transport attached.
 *
 * Separate from {@link openSession} so it can be driven from a recorded
 * transcript and tested without a connection.
 */
export class SessionMirror implements ReactiveStore {
  readonly #notifier = new Notifier();
  #apps: SessionApp[] = [];
  #catalog: SessionCatalogEntry[] = [];
  #icons = new Map<string, string | null>();
  #ready = false;

  get revision(): number {
    return this.#notifier.revision;
  }

  subscribe = (listener: () => void): (() => void) =>
    this.#notifier.subscribe(listener);

  get apps(): readonly SessionApp[] {
    return this.#apps;
  }

  get catalog(): readonly SessionCatalogEntry[] {
    return this.#catalog;
  }

  get ready(): boolean {
    return this.#ready;
  }

  /** A data URL, `null` once the answer "no artwork" has arrived, `undefined`
   *  while nobody has asked. */
  icon(id: string): string | null | undefined {
    return this.#icons.get(id);
  }

  /** Apply one channel payload. Malformed messages are dropped, not thrown:
   *  a panel is not the place to surface a parser disagreement. */
  apply(payload: Uint8Array): void {
    let message: unknown;
    try {
      message = JSON.parse(new TextDecoder().decode(payload));
    } catch {
      return;
    }
    if (typeof message !== "object" || message === null) return;
    const record = message as Record<string, unknown>;

    // One id per message, and a missing `icon` is the answer "there is none" —
    // which has to be recorded, or the panel asks again on the next render.
    if (record.type === "icon") {
      if (typeof record.id !== "string" || record.id.length === 0) return;
      const icon = record.icon;
      this.#icons.set(
        record.id,
        typeof icon === "string" && icon.startsWith("data:") ? icon : null,
      );
      this.#notifier.emit();
      return;
    }
    if (record.type !== "state") return;

    if (Array.isArray(record.apps)) {
      this.#apps = record.apps
        .map(appOf)
        .filter((app): app is SessionApp => app !== null)
        .sort(byName);
      this.#ready = true;
    }
    // Absent on an update; only a greeting or a resync carries it, because it
    // is the larger half and changes only when packages do.
    if (Array.isArray(record.catalog)) {
      this.#catalog = record.catalog
        .map(entryOf)
        .filter((entry): entry is SessionCatalogEntry => entry !== null)
        .sort(byName);
    }
    this.#notifier.emit();
  }
}

export async function openSession(
  connection: ChannelOpener,
  options: SessionOptions = {},
): Promise<SessionHandle> {
  const mirror = new SessionMirror();
  let channel: ChannelHandle | null = null;
  const channelHandle = await connection.connectChannel(SESSION_CHANNEL, {
    onData: (payload: Uint8Array) => mirror.apply(payload),
    onClosed: (reason: number, detail: string) => {
      channel = null;
      options.onClosed?.(reason, detail);
    },
  });
  channel = channelHandle;

  // Unlike the systemd watcher, this one greets with full state and the
  // catalog, so no opening request is needed.
  const request = (line: string): void => {
    channel?.send(line);
  };

  // When each id was last asked about. Separate from what the mirror holds
  // because a request is outstanding for a round trip, and a panel re-rendering
  // in that window would otherwise ask again for every row on screen — but it
  // expires, so an answer that never came is asked for again rather than
  // leaving one row a placeholder forever.
  const asked = new Map<string, number>();
  const worthAsking = (id: string, now: number): boolean => {
    if (id.length === 0 || id.includes("\n")) return false;
    if (mirror.icon(id) !== undefined) return false;
    const at = asked.get(id);
    return at === undefined || now - at >= ICON_RETRY_MS;
  };
  // Ids waiting to be asked about, and the timer that will ask.
  let queued: string[] = [];
  let coalescing: ReturnType<typeof setTimeout> | undefined;
  const flushIcons = () => {
    coalescing = undefined;
    const wanted = queued;
    queued = [];
    // Newline-separated, not space: a desktop-entry id is a filename, and Steam
    // alone installs hundreds with spaces in them ("3DMark Demo.desktop").
    for (let at = 0; at < wanted.length; at += ICON_BATCH) {
      request(`icons ${wanted.slice(at, at + ICON_BATCH).join("\n")}`);
    }
  };

  return {
    get apps() {
      return mirror.apps;
    },
    get catalog() {
      return mirror.catalog;
    },
    get ready() {
      return mirror.ready;
    },
    get revision() {
      return mirror.revision;
    },
    icon: (id: string) => mirror.icon(id),
    requestIcons: (ids: readonly string[]) => {
      const now = Date.now();
      const wanted = ids.filter((id) => worthAsking(id, now));
      if (wanted.length === 0) return;
      for (const id of wanted) asked.set(id, now);
      queued.push(...wanted);
      coalescing ??= setTimeout(flushIcons, ICON_COALESCE_MS);
    },
    subscribe: mirror.subscribe,
    enable: (id: string) => request(`enable ${id}`),
    disable: (id: string) => request(`disable ${id}`),
    start: (id: string) => request(`start ${id}`),
    stop: (id: string) => request(`stop ${id}`),
    forget: (id: string) => request(`forget ${id}`),
    resync: () => request("resync"),
    close: () => {
      if (coalescing !== undefined) clearTimeout(coalescing);
      coalescing = undefined;
      queued = [];
      channelHandle.close();
      channel = null;
    },
  };
}

/**
 * Whether a supervisor serves this connection.
 *
 * A connect that is refused is the answer "no supervisor here", not an error:
 * the extension is optional, and a server without it is not broken.
 */
export async function sessionSupervisorPresent(
  connection: ChannelOpener,
): Promise<boolean> {
  try {
    const channel = await connection.connectChannel(SESSION_CHANNEL);
    channel.close();
    return true;
  } catch {
    return false;
  }
}
