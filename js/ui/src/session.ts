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
    subscribe: mirror.subscribe,
    enable: (id: string) => request(`enable ${id}`),
    disable: (id: string) => request(`disable ${id}`),
    start: (id: string) => request(`start ${id}`),
    stop: (id: string) => request(`stop ${id}`),
    forget: (id: string) => request(`forget ${id}`),
    resync: () => request("resync"),
    close: () => {
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
