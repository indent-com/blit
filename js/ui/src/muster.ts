/**
 * Client mirror for the `blit.muster.v1` channel served by the muster
 * supervisor extension (`extensions/muster`).
 *
 * The channel is JSON, one object per message: a `hello`, then `state` frames
 * and `events` batches. A `state` frame carries whole units — never a
 * field-level patch — so applying one is a replace, and a reader that missed
 * the previous frame is still correct after the next. What it does *not* carry
 * is the ninety-nine units that did not change; `full` marks the frames that
 * do, and those are the only ones that redefine the instance tree.
 *
 * Like `systemd.ts` this lives in the app rather than in `@blit-sh/core`: the
 * server knows nothing about the channel's bytes, only that it has a listener.
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

export const MUSTER_CHANNEL = "blit.muster.v1";

/** A window a unit's terminal opened, as the compositor stamped it. */
export interface MusterSurface {
  readonly id: number;
  readonly title: string;
  readonly width: number;
  readonly height: number;
}

/** A finished run whose terminal is kept for reading. */
export interface MusterRun {
  readonly pty: number;
  readonly exitCode: number | null;
  readonly seq: number;
}

export type MusterPhase =
  | "stopped"
  | "waiting"
  | "activating"
  | "running"
  | "exited"
  | "backoff"
  | "failed"
  | "held";

export interface MusterUnit {
  /** `name` for a plain unit, `instance/template` for one from a stack. */
  readonly name: string;
  /** The instance this belongs to, or null for a top-level unit. */
  readonly instance: string | null;
  readonly description: string;
  readonly phase: MusterPhase;
  /** The live terminal, or null when nothing is running. */
  readonly pty: number | null;
  /** Consecutive failures, which is what the backoff is derived from. */
  readonly restarts: number;
  readonly lastExit: number | null;
  readonly requires: readonly string[];
  readonly autostart: boolean;
  /** The unit file changed under the running process. */
  readonly stale: boolean;
  readonly type: "simple" | "oneshot";
  readonly surfaces: readonly MusterSurface[];
  /** Retained terminals of previous runs, newest first. */
  readonly runs: readonly MusterRun[];
}

/**
 * The panel's primary action for a unit.
 *
 * A successful oneshot stays `exited` and ready, so `start` intentionally does
 * nothing to it. Re-running it is a restart, just like replacing a live unit.
 */
export function unitStartVerb(
  unit: Pick<MusterUnit, "phase">,
): "start" | "restart" {
  return unit.phase === "running" ||
    unit.phase === "activating" ||
    unit.phase === "exited"
    ? "restart"
    : "start";
}

/** A completed oneshot has no process left for Stop to act on. */
export function unitCanStop(
  unit: Pick<MusterUnit, "phase" | "type">,
): boolean {
  return unit.type !== "oneshot" || unit.phase !== "exited";
}

/** A stack instantiated under a name, and the units it expanded to. */
export interface MusterInstance {
  readonly name: string;
  /** The stack it came from: a subdirectory name, or a path. */
  readonly stack: string;
  readonly members: readonly string[];
}

/** One journal record. Free-form beyond these: the extension adds fields. */
export interface MusterEvent {
  readonly seq: number;
  readonly ts: number;
  readonly unit: string;
  readonly event: string;
  readonly phase: string;
  readonly instance?: string;
  readonly cause?: string;
  readonly pty?: number;
  readonly exitCode?: number;
  readonly detail?: string;
}

export interface MusterOptions {
  onEvents?(events: readonly MusterEvent[]): void;
  onClosed?(reason: number, detail: string): void;
}

export interface FollowMusterOptions {
  /** A fresh handle, or null while the channel is being reopened. */
  onHandle(handle: MusterHandle | null): void;
  onRetry?(): void;
  retryDelayMs?: number;
}

export interface MusterHandle extends ReactiveStore {
  readonly units: ReadonlyMap<string, MusterUnit>;
  readonly instances: ReadonlyMap<string, MusterInstance>;
  /** The configuration directory the supervisor is watching. */
  readonly dir: string;
  /** False until the first `full` state frame has landed. */
  readonly ready: boolean;
  /** The journal tail, oldest first, capped at {@link EVENT_CAP}. */
  readonly events: readonly MusterEvent[];
  /** A unit name, or an instance name standing for all of its members. */
  start(name: string): void;
  stop(name: string): void;
  restart(name: string): void;
  /**
   * Retry the directories whose watch the server refused.
   *
   * Not "re-read the configuration": the supervisor watches it, so an edit is
   * already here. A refused watch is the one thing that cannot arrive on its
   * own — nothing watches a directory that is not being watched — and it also
   * retries by itself on a climbing timer, so this is only impatience.
   */
  rewatch(): void;
  /** Ask for a full frame — for a viewer who suspects drift. */
  resync(): void;
  close(): void;
}

/**
 * How much journal the mirror keeps.
 *
 * The supervisor backfills 200 on connect and the panel shows a tail, so this
 * only has to outlast a burst: a stack of thirty units restarting emits a few
 * records each.
 */
export const EVENT_CAP = 500;

const PHASES = new Set<string>([
  "stopped",
  "waiting",
  "activating",
  "running",
  "exited",
  "backoff",
  "failed",
  "held",
]);

function str(value: unknown, fallback = ""): string {
  return typeof value === "string" ? value : fallback;
}

function num(value: unknown): number | null {
  return typeof value === "number" && Number.isFinite(value) ? value : null;
}

function strings(value: unknown): string[] {
  return Array.isArray(value) ? value.filter((v) => typeof v === "string") : [];
}

function surfaceOf(value: unknown): MusterSurface | null {
  if (typeof value !== "object" || value === null) return null;
  const record = value as Record<string, unknown>;
  const id = num(record.id);
  if (id === null) return null;
  return {
    id,
    title: str(record.title),
    width: num(record.width) ?? 0,
    height: num(record.height) ?? 0,
  };
}

function runOf(value: unknown): MusterRun | null {
  if (typeof value !== "object" || value === null) return null;
  const record = value as Record<string, unknown>;
  const pty = num(record.pty);
  if (pty === null) return null;
  return { pty, exitCode: num(record.exitCode), seq: num(record.seq) ?? 0 };
}

function unitOf(value: unknown): MusterUnit | null {
  if (typeof value !== "object" || value === null) return null;
  const record = value as Record<string, unknown>;
  const name = str(record.name);
  if (!name) return null;
  const phase = str(record.phase);
  return {
    name,
    instance: typeof record.instance === "string" ? record.instance : null,
    description: str(record.description),
    phase: (PHASES.has(phase) ? phase : "stopped") as MusterPhase,
    pty: num(record.pty),
    restarts: num(record.restarts) ?? 0,
    lastExit: num(record.lastExit),
    requires: strings(record.requires),
    autostart: record.autostart !== false,
    stale: record.stale === true,
    type: record.type === "oneshot" ? "oneshot" : "simple",
    surfaces: Array.isArray(record.surfaces)
      ? record.surfaces
          .map(surfaceOf)
          .filter((s): s is MusterSurface => s !== null)
      : [],
    runs: Array.isArray(record.runs)
      ? record.runs.map(runOf).filter((r): r is MusterRun => r !== null)
      : [],
  };
}

function instanceOf(value: unknown): MusterInstance | null {
  if (typeof value !== "object" || value === null) return null;
  const record = value as Record<string, unknown>;
  const name = str(record.name);
  if (!name) return null;
  return { name, stack: str(record.stack), members: strings(record.members) };
}

function eventOf(value: unknown): MusterEvent | null {
  if (typeof value !== "object" || value === null) return null;
  const record = value as Record<string, unknown>;
  const seq = num(record.seq);
  if (seq === null) return null;
  return {
    ...(record as object),
    seq,
    ts: num(record.ts) ?? 0,
    unit: str(record.unit),
    event: str(record.event),
    phase: str(record.phase),
  } as MusterEvent;
}

/**
 * The message reducer, with no transport attached.
 *
 * Separate from {@link openMuster} so tests can drive it from a transcript,
 * the same split `systemd.ts` makes.
 */
export class MusterMirror implements ReactiveStore {
  readonly #notifier = new Notifier();
  readonly #units = new Map<string, MusterUnit>();
  readonly #instances = new Map<string, MusterInstance>();
  #events: MusterEvent[] = [];
  #dir = "";
  #ready = false;
  #onEvents: ((events: readonly MusterEvent[]) => void) | undefined;

  constructor(onEvents?: (events: readonly MusterEvent[]) => void) {
    this.#onEvents = onEvents;
  }

  get revision(): number {
    return this.#notifier.revision;
  }

  subscribe = (listener: () => void): (() => void) =>
    this.#notifier.subscribe(listener);

  get units(): ReadonlyMap<string, MusterUnit> {
    return this.#units;
  }

  get instances(): ReadonlyMap<string, MusterInstance> {
    return this.#instances;
  }

  get dir(): string {
    return this.#dir;
  }

  get ready(): boolean {
    return this.#ready;
  }

  get events(): readonly MusterEvent[] {
    return this.#events;
  }

  /** Apply one channel message. Malformed JSON is ignored, not thrown. */
  apply(payload: Uint8Array | string): void {
    let message: unknown;
    try {
      message =
        typeof payload === "string"
          ? JSON.parse(payload)
          : JSON.parse(new TextDecoder().decode(payload));
    } catch {
      return;
    }
    if (typeof message !== "object" || message === null) return;
    const record = message as Record<string, unknown>;
    switch (record.type) {
      case "hello":
        this.#dir = str(record.dir, this.#dir);
        this.#notifier.emit();
        return;
      case "state": {
        // A full frame redefines the whole table: units the supervisor no
        // longer has are absent rather than listed in `gone`, so anything not
        // named in it is gone by omission.
        if (record.full === true) {
          const kept = new Set<string>();
          for (const value of Array.isArray(record.units) ? record.units : []) {
            const unit = unitOf(value);
            if (!unit) continue;
            this.#units.set(unit.name, unit);
            kept.add(unit.name);
          }
          for (const name of [...this.#units.keys()]) {
            if (!kept.has(name)) this.#units.delete(name);
          }
          this.#instances.clear();
          for (const value of Array.isArray(record.instances)
            ? record.instances
            : []) {
            const instance = instanceOf(value);
            if (instance) this.#instances.set(instance.name, instance);
          }
          this.#dir = str(record.dir, this.#dir);
          this.#ready = true;
        } else {
          for (const value of Array.isArray(record.units) ? record.units : []) {
            const unit = unitOf(value);
            if (unit) this.#units.set(unit.name, unit);
          }
          for (const name of strings(record.gone)) this.#units.delete(name);
        }
        this.#notifier.emit();
        return;
      }
      case "events": {
        const batch = (Array.isArray(record.records) ? record.records : [])
          .map(eventOf)
          .filter((e): e is MusterEvent => e !== null);
        if (batch.length === 0) return;
        this.#events = this.#events.concat(batch);
        if (this.#events.length > EVENT_CAP) {
          this.#events = this.#events.slice(-EVENT_CAP);
        }
        this.#onEvents?.(batch);
        this.#notifier.emit();
        return;
      }
      default:
        return;
    }
  }
}

/** Units in display order, grouped: an instance's members under its name,
 *  then the units that belong to no instance. */
export interface MusterGroup {
  /** Null for the units that came from a file of their own. */
  readonly instance: MusterInstance | null;
  readonly units: readonly MusterUnit[];
}

export function groupUnits(
  units: ReadonlyMap<string, MusterUnit>,
  instances: ReadonlyMap<string, MusterInstance>,
): MusterGroup[] {
  const groups: MusterGroup[] = [];
  const claimed = new Set<string>();
  for (const instance of [...instances.values()].sort((a, b) =>
    a.name.localeCompare(b.name),
  )) {
    const members: MusterUnit[] = [];
    for (const name of instance.members) {
      const unit = units.get(name);
      if (!unit) continue;
      claimed.add(name);
      members.push(unit);
    }
    // An instance whose expansion failed has no members and no rows, but it is
    // still declared — dropping it would make a broken stack look absent.
    groups.push({ instance, units: members });
  }
  const loose = [...units.values()]
    .filter((unit) => !claimed.has(unit.name))
    .sort((a, b) => a.name.localeCompare(b.name));
  if (loose.length > 0) groups.push({ instance: null, units: loose });
  return groups;
}

/** Connect to the supervisor and keep a live view of its units. */
export async function openMuster(
  connection: ChannelOpener,
  options: MusterOptions = {},
): Promise<MusterHandle> {
  const mirror = new MusterMirror(options.onEvents);
  let channel: ChannelHandle | null = null;

  const handle = await connection.connectChannel(MUSTER_CHANNEL, {
    onData: (payload: Uint8Array) => mirror.apply(payload),
    onClosed: (reason: number, detail: string) => {
      channel = null;
      options.onClosed?.(reason, detail);
    },
  });
  channel = handle;

  /** Commands are bare text lines, as the CLI's verbs. */
  const send = (line: string): void => {
    channel?.send(line);
  };

  return {
    get units() {
      return mirror.units;
    },
    get instances() {
      return mirror.instances;
    },
    get dir() {
      return mirror.dir;
    },
    get ready() {
      return mirror.ready;
    },
    get events() {
      return mirror.events;
    },
    get revision() {
      return mirror.revision;
    },
    subscribe: mirror.subscribe,
    start: (name) => send(`start ${name}`),
    stop: (name) => send(`stop ${name}`),
    restart: (name) => send(`restart ${name}`),
    rewatch: () => send("rewatch"),
    resync: () => send("resync"),
    close: () => {
      handle.close();
      channel = null;
    },
  };
}

/**
 * Keep a Muster handle open across extension updates and connection flaps.
 *
 * Each reopen gets a fresh mirror. Reusing the old one would append the
 * supervisor's journal backfill a second time, and an extension replacement
 * may restart journal sequence numbers from one.
 */
export function followMuster(
  connection: () => ChannelOpener | null,
  options: FollowMusterOptions,
): () => void {
  let live = true;
  let serial = 0;
  let handle: MusterHandle | null = null;
  let retry: ReturnType<typeof setTimeout> | null = null;
  const retryDelayMs = options.retryDelayMs ?? 250;

  const schedule = () => {
    if (!live || retry !== null) return;
    options.onRetry?.();
    retry = setTimeout(() => {
      retry = null;
      void connect();
    }, retryDelayMs);
  };

  const connect = async () => {
    if (!live) return;
    const opener = connection();
    if (!opener) {
      schedule();
      return;
    }
    const mine = ++serial;
    try {
      const next = await openMuster(opener, {
        onClosed: () => {
          if (!live || mine !== serial) return;
          // Invalidate the in-flight/open handle before scheduling another;
          // a late resolution from this generation must not win the race.
          serial += 1;
          handle = null;
          options.onHandle(null);
          schedule();
        },
      });
      if (!live || mine !== serial) {
        next.close();
        return;
      }
      handle = next;
      options.onHandle(next);
    } catch {
      if (live && mine === serial) schedule();
    }
  };

  void connect();
  return () => {
    live = false;
    serial += 1;
    if (retry !== null) clearTimeout(retry);
    retry = null;
    handle?.close();
    handle = null;
  };
}
