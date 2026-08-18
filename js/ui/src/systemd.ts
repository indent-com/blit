/**
 * Client mirror for the `blit.systemd.v1` channel served by the systemd
 * watcher extension (`extensions/systemd`).
 *
 * The channel is JSON, one object per message: a `hello`, chunked `snapshot`
 * messages per scope, then `change` deltas. A frontend wants neither the
 * chunking nor the deltas, so this folds both into one live map per scope and
 * exposes the {@link ReactiveStore} contract every other blit handle uses.
 *
 * This is an extension protocol, not a server packet family, which is why it
 * lives in the app rather than in `@blit-sh/core`: the server knows nothing
 * about it, and a different watcher may publish something else under another
 * channel name. Core supplies the channel; the meaning of its bytes is ours.
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

export const SYSTEMD_CHANNEL = "blit.systemd.v1";

export interface SystemdUnit {
  readonly name: string;
  /** `loaded`, `not-found`, `masked`, … */
  readonly load: string;
  /** `active`, `inactive`, `failed`, `activating`, … */
  readonly active: string;
  /** Type-specific substate: `running`, `dead`, `listening`, … */
  readonly sub: string;
  readonly description: string;
}

export interface SystemdScopeState {
  readonly scope: string;
  /** `gdbus` when D-Bus signals drive the watcher, `poll` when it polls. */
  readonly source: string;
  readonly units: ReadonlyMap<string, SystemdUnit>;
  /** False until the first complete snapshot has arrived. */
  readonly ready: boolean;
  /** Extension clock, milliseconds since the epoch, of the last message. */
  readonly updatedAt: number;
}

/** One applied delta, for callers that want events rather than a snapshot. */
export interface SystemdChange {
  readonly scope: string;
  readonly ts: number;
  readonly added: readonly SystemdUnit[];
  readonly changed: readonly {
    readonly unit: SystemdUnit;
    readonly previous: { load: string; active: string; sub: string };
  }[];
  readonly removed: readonly string[];
}

export interface SystemdUnitsOptions {
  /** Limit the stream to these scopes, e.g. `["system"]`. */
  scopes?: readonly string[];
  /** Limit the stream to unit names with this prefix. */
  prefix?: string;
  onChange?(change: SystemdChange): void;
  onClosed?(reason: number, detail: string): void;
}

/** One journal entry, as the watcher reduces it. */
export interface SystemdLogEntry {
  /** Opaque journald cursor; the anchor for the next page either way. */
  readonly cursor: string;
  /** Microseconds since the epoch, as a string — it does not fit a double. */
  readonly realtime: string;
  /** syslog priority, "0".."7". */
  readonly priority: string;
  readonly unit: string;
  readonly pid: string;
  readonly message: string;
}

export interface SystemdBoot {
  readonly boot: string;
  /** 0 is the running boot, -1 the one before it. */
  readonly index: string;
  readonly first: string;
  readonly last: string;
}

export interface SystemdLogQuery {
  /** `all` drops the system/user filter, which a copied journal needs. */
  scope?: "system" | "user" | "all";
  unit?: string;
  boot?: string;
  /** journalctl priority: a number, a name, or a `warning..emerg` range. */
  priority?: string;
  /** Server-side regex, so a search does not need the whole journal here. */
  grep?: string;
  cursor?: string;
  /** `backward` reads older than the cursor, `forward` newer. */
  direction?: "backward" | "forward";
  limit?: number;
}

export interface SystemdLogPage {
  /** Always oldest-first, whichever direction the page was read in. */
  readonly entries: readonly SystemdLogEntry[];
  /** The page filled its limit, so there is probably another one. */
  readonly more: boolean;
}

export interface SystemdUnitsHandle extends ReactiveStore {
  readonly scopes: ReadonlyMap<string, SystemdScopeState>;
  /** Look one unit up, optionally in one scope. */
  unit(name: string, scope?: string): SystemdUnit | undefined;
  /** Every unit across scopes, sorted by name, for a flat list view. */
  all(): { scope: string; unit: SystemdUnit }[];
  /** Ask for fresh snapshots (after a UI reset, or on suspicion of drift). */
  resync(): void;
  setPrefix(prefix: string): void;
  setScopes(scopes: readonly string[]): void;
  /** One page of the journal. Rejects with journalctl's own words. */
  logs(query?: SystemdLogQuery): Promise<SystemdLogPage>;
  /** Boots the journal still holds, oldest first. */
  boots(): Promise<readonly SystemdBoot[]>;
  close(): void;
}

interface MutableScope {
  scope: string;
  source: string;
  units: Map<string, SystemdUnit>;
  ready: boolean;
  updatedAt: number;
  /** Snapshot chunks accumulate here until the one flagged `last`. */
  building: Map<string, SystemdUnit> | null;
}

function unitOf(value: unknown): SystemdUnit | null {
  if (typeof value !== "object" || value === null) return null;
  const record = value as Record<string, unknown>;
  if (typeof record.name !== "string" || record.name.length === 0) return null;
  return {
    name: record.name,
    load: typeof record.load === "string" ? record.load : "",
    active: typeof record.active === "string" ? record.active : "",
    sub: typeof record.sub === "string" ? record.sub : "",
    description:
      typeof record.description === "string" ? record.description : "",
  };
}

/**
 * The message reducer, with no transport attached.
 *
 * Kept separate from {@link openSystemdUnits} so a caller can drive it from a
 * recorded transcript, and so tests need no connection.
 */
export class SystemdUnitsMirror implements ReactiveStore {
  readonly #notifier = new Notifier();
  readonly #scopes = new Map<string, MutableScope>();
  #onChange: ((change: SystemdChange) => void) | undefined;

  constructor(onChange?: (change: SystemdChange) => void) {
    this.#onChange = onChange;
  }

  get revision(): number {
    return this.#notifier.revision;
  }

  subscribe = (listener: () => void): (() => void) =>
    this.#notifier.subscribe(listener);

  get scopes(): ReadonlyMap<string, SystemdScopeState> {
    return this.#scopes;
  }

  unit(name: string, scope?: string): SystemdUnit | undefined {
    if (scope !== undefined) return this.#scopes.get(scope)?.units.get(name);
    for (const state of this.#scopes.values()) {
      const unit = state.units.get(name);
      if (unit) return unit;
    }
    return undefined;
  }

  all(): { scope: string; unit: SystemdUnit }[] {
    const rows: { scope: string; unit: SystemdUnit }[] = [];
    for (const state of this.#scopes.values()) {
      for (const unit of state.units.values())
        rows.push({ scope: state.scope, unit });
    }
    rows.sort(
      (left, right) =>
        left.unit.name.localeCompare(right.unit.name) ||
        left.scope.localeCompare(right.scope),
    );
    return rows;
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
    const ts = typeof record.ts === "number" ? record.ts : 0;
    switch (record.type) {
      case "hello": {
        if (!Array.isArray(record.scopes)) return;
        for (const entry of record.scopes) {
          if (typeof entry !== "object" || entry === null) continue;
          const scopeRecord = entry as Record<string, unknown>;
          if (typeof scopeRecord.scope !== "string") continue;
          const state = this.#scope(scopeRecord.scope);
          if (typeof scopeRecord.source === "string") {
            state.source = scopeRecord.source;
          }
          state.updatedAt = ts;
        }
        this.#notifier.emit();
        return;
      }
      case "snapshot": {
        if (typeof record.scope !== "string" || !Array.isArray(record.units)) {
          return;
        }
        const state = this.#scope(record.scope);
        // Chunk 0 opens a rebuild; anything else without one is a stray from
        // a snapshot whose head was dropped, so ignore it rather than merge
        // a partial table into the live one.
        if (record.chunk === 0) state.building = new Map();
        if (!state.building) return;
        for (const value of record.units) {
          const unit = unitOf(value);
          if (unit) state.building.set(unit.name, unit);
        }
        state.updatedAt = ts;
        if (record.last === true) {
          state.units = state.building;
          state.building = null;
          state.ready = true;
        }
        this.#notifier.emit();
        return;
      }
      case "change": {
        if (typeof record.scope !== "string") return;
        const state = this.#scope(record.scope);
        const added: SystemdUnit[] = [];
        const changed: SystemdChange["changed"][number][] = [];
        const removed: string[] = [];
        for (const value of Array.isArray(record.added) ? record.added : []) {
          const unit = unitOf(value);
          if (!unit) continue;
          state.units.set(unit.name, unit);
          added.push(unit);
        }
        for (const value of Array.isArray(record.changed)
          ? record.changed
          : []) {
          const unit = unitOf(value);
          if (!unit) continue;
          state.units.set(unit.name, unit);
          const previous = (value as Record<string, unknown>).previous;
          const previousRecord =
            typeof previous === "object" && previous !== null
              ? (previous as Record<string, unknown>)
              : {};
          changed.push({
            unit,
            previous: {
              load:
                typeof previousRecord.load === "string"
                  ? previousRecord.load
                  : "",
              active:
                typeof previousRecord.active === "string"
                  ? previousRecord.active
                  : "",
              sub:
                typeof previousRecord.sub === "string"
                  ? previousRecord.sub
                  : "",
            },
          });
        }
        for (const value of Array.isArray(record.removed)
          ? record.removed
          : []) {
          if (typeof value !== "string") continue;
          state.units.delete(value);
          removed.push(value);
        }
        state.updatedAt = ts;
        if (added.length || changed.length || removed.length) {
          this.#onChange?.({ scope: state.scope, ts, added, changed, removed });
          this.#notifier.emit();
        }
        return;
      }
      default:
        return;
    }
  }

  #scope(name: string): MutableScope {
    let state = this.#scopes.get(name);
    if (!state) {
      state = {
        scope: name,
        source: "unknown",
        units: new Map(),
        ready: false,
        updatedAt: 0,
        building: null,
      };
      this.#scopes.set(name, state);
    }
    return state;
  }
}

/**
 * Connect to the watcher and keep a live unit table.
 *
 * The handle stays valid until `close`, the extension goes away, or the
 * transport drops; `onClosed` reports all three, and a caller that wants to
 * survive a reconnect opens a new one.
 */
/**
 * Is a watcher serving this connection?
 *
 * Opening the channel is the only way to ask — the server has no listener
 * directory — but the watcher answers a fresh connection with `hello` alone,
 * so the question costs a round trip rather than a unit table.
 */
export async function systemdWatcherPresent(
  connection: ChannelOpener,
): Promise<boolean> {
  try {
    const channel = await connection.connectChannel(SYSTEMD_CHANNEL);
    channel.close();
    return true;
  } catch {
    return false;
  }
}

/** One row of the unit table: a unit and the manager it belongs to. */
export interface SystemdUnitRow extends SystemdUnit {
  readonly scope: string;
}

/** The unit suffixes systemd defines, for the type filter. */
export const SYSTEMD_UNIT_TYPES = [
  "service",
  "socket",
  "target",
  "timer",
  "mount",
  "automount",
  "path",
  "device",
  "scope",
  "slice",
  "swap",
] as const;

export interface SystemdUnitFilter {
  /** Empty means every scope the watcher reports. */
  scope?: string;
  /** Matched against `active` — `failed`, `activating`, and so on. */
  state?: string;
  /** Unit suffix without the dot: `service`, `timer`, … */
  type?: string;
  /** Substring of the name or the description, case-insensitive. */
  search?: string;
}

/**
 * Apply the unit filters to a mirror, newest state included.
 *
 * Filtering is local because the whole table is here: a viewer typing into a
 * search box wants the rows back when it deletes a character, and asking the
 * server again for each keystroke would spend a `systemctl` run to answer a
 * question already answered.
 */
export function filterUnits(
  scopes: ReadonlyMap<string, SystemdScopeState>,
  filter: SystemdUnitFilter = {},
): SystemdUnitRow[] {
  const needle = (filter.search ?? "").trim().toLowerCase();
  const suffix = filter.type ? `.${filter.type}` : "";
  const rows: SystemdUnitRow[] = [];
  for (const scope of scopes.values()) {
    if (filter.scope && scope.scope !== filter.scope) continue;
    for (const unit of scope.units.values()) {
      if (filter.state && unit.active !== filter.state) continue;
      if (suffix && !unit.name.endsWith(suffix)) continue;
      if (
        needle &&
        !unit.name.toLowerCase().includes(needle) &&
        !unit.description.toLowerCase().includes(needle)
      ) {
        continue;
      }
      rows.push({ scope: scope.scope, ...unit });
    }
  }
  rows.sort(
    (left, right) =>
      left.name.localeCompare(right.name) ||
      left.scope.localeCompare(right.scope),
  );
  return rows;
}

/** Active states present in a mirror, so the filter offers only real ones. */
export function unitStates(
  scopes: ReadonlyMap<string, SystemdScopeState>,
): string[] {
  const states = new Set<string>();
  for (const scope of scopes.values()) {
    for (const unit of scope.units.values()) states.add(unit.active);
  }
  return [...states].sort();
}

/** A journal query that never answers must not hold a promise forever. */
const QUERY_TIMEOUT_MS = 20_000;

function isLogEntry(value: unknown): value is SystemdLogEntry {
  return (
    typeof value === "object" &&
    value !== null &&
    typeof (value as SystemdLogEntry).cursor === "string"
  );
}

function isBoot(value: unknown): value is SystemdBoot {
  return (
    typeof value === "object" &&
    value !== null &&
    typeof (value as SystemdBoot).boot === "string"
  );
}

export async function openSystemdUnits(
  connection: ChannelOpener,
  options: SystemdUnitsOptions = {},
): Promise<SystemdUnitsHandle> {
  const mirror = new SystemdUnitsMirror(options.onChange);
  let channel: ChannelHandle | null = null;

  // Queries are correlated by id and answered in chunks; state messages carry
  // no id and belong to the mirror. Keeping the two apart here leaves the
  // mirror a pure reducer.
  interface Pending {
    entries: unknown[];
    resolve(page: { entries: unknown[]; more: boolean }): void;
    reject(error: Error): void;
    timer: ReturnType<typeof setTimeout>;
  }
  const pending = new Map<string, Pending>();
  let nextRequestId = 1;

  const settle = (message: Record<string, unknown>): boolean => {
    const id = typeof message.id === "string" ? message.id : "";
    if (!id) return false;
    const waiting = pending.get(id);
    if (!waiting) return true;
    if (message.type === "error") {
      pending.delete(id);
      clearTimeout(waiting.timer);
      waiting.reject(
        new Error(
          typeof message.message === "string"
            ? message.message
            : "query failed",
        ),
      );
      return true;
    }
    if (Array.isArray(message.entries))
      waiting.entries.push(...message.entries);
    if (message.last === true) {
      pending.delete(id);
      clearTimeout(waiting.timer);
      waiting.resolve({
        entries: waiting.entries,
        more: message.more === true,
      });
    }
    return true;
  };

  const channelHandle = await connection.connectChannel(SYSTEMD_CHANNEL, {
    onData: (payload: Uint8Array) => {
      let message: unknown;
      try {
        message = JSON.parse(new TextDecoder().decode(payload));
      } catch {
        return;
      }
      if (
        typeof message === "object" &&
        message !== null &&
        settle(message as Record<string, unknown>)
      ) {
        return;
      }
      mirror.apply(payload);
    },
    onClosed: (reason: number, detail: string) => {
      channel = null;
      for (const [id, waiting] of pending) {
        clearTimeout(waiting.timer);
        waiting.reject(new Error(detail || `channel closed (${reason})`));
        pending.delete(id);
      }
      options.onClosed?.(reason, detail);
    },
  });
  channel = channelHandle;

  /** Send one correlated request and wait for its final chunk. */
  const query = (
    body: Record<string, unknown>,
  ): Promise<{ entries: unknown[]; more: boolean }> =>
    new Promise((resolve, reject) => {
      if (!channel) {
        reject(new Error("channel is closed"));
        return;
      }
      const id = String(nextRequestId++);
      const timer = setTimeout(() => {
        pending.delete(id);
        channel?.send(JSON.stringify({ type: "cancel", id }));
        reject(new Error("journal query timed out"));
      }, QUERY_TIMEOUT_MS);
      pending.set(id, { entries: [], resolve, reject, timer });
      if (!channel.send(JSON.stringify({ ...body, id }))) {
        pending.delete(id);
        clearTimeout(timer);
        reject(new Error("channel is not accepting requests"));
      }
    });

  // Requests are bare text lines; the extension answers each with fresh
  // snapshots, so a filter change needs no separate resync. The watcher sends
  // only `hello` until asked, so one request is always required.
  const request = (line: string): void => {
    channel?.send(line);
  };
  if (options.scopes?.length) request(`scopes ${options.scopes.join(",")}`);
  if (options.prefix) request(`filter ${options.prefix}`);
  if (!options.scopes?.length && !options.prefix) request("resync");

  return {
    get scopes() {
      return mirror.scopes;
    },
    get revision() {
      return mirror.revision;
    },
    subscribe: mirror.subscribe,
    unit: (name, scope) => mirror.unit(name, scope),
    all: () => mirror.all(),
    resync: () => request("resync"),
    setPrefix: (prefix) => request(`filter ${prefix}`),
    setScopes: (scopes) => request(`scopes ${scopes.join(",")}`),
    logs: async (request: SystemdLogQuery = {}) => {
      const page = await query({ type: "logs", ...request });
      return {
        entries: page.entries.filter(isLogEntry),
        more: page.more,
      };
    },
    boots: async () => {
      const page = await query({ type: "boots" });
      return page.entries.filter(isBoot);
    },
    close: () => {
      channelHandle.close();
      channel = null;
    },
  };
}
