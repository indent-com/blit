/**
 * A registry of installable extensions: `manifest.json` and the modules
 * beside it (`https://install.blit.sh/ext` by default).
 *
 * The manifest is a list of names and BLAKE3 digests, and the digest is what
 * gets installed — the bytes are fetched only when the server says it lacks
 * that object, and the server re-hashes them and refuses a mismatch. So a
 * registry that serves the wrong bytes cannot install anything; it can only
 * fail. That is the whole trust story, and it is worth being explicit about
 * because a browser cannot compute BLAKE3 to check for itself.
 */

import {
  EXT_CONTROL_DISABLE,
  EXT_CONTROL_REMOVE,
  EXT_CONTROL_STATUS,
  EXT_FLAG_PERSIST,
  EXT_PHASE_BLOCKED,
  EXT_PHASE_STOPPED,
  EXT_RESTART_ALWAYS,
  formatExtensionId,
  parseModuleDigest,
  type BlitExtensionRecord,
  type BlitExtensionStatus,
} from "@blit-sh/core";

export const PUBLIC_REGISTRY = "https://install.blit.sh/ext";

/** Path the `vite dev` server proxies to the stack's own registry. */
const DEV_REGISTRY_PATH = "/ext";

/**
 * Where the panel looks first.
 *
 * Under `vite dev` that is the stack's own registry, reached through the page
 * itself: the dev server proxies `/ext` to the port the development stack
 * allocated for this instance, so a second stack's page still offers that
 * stack's modules.
 * Going through the origin rather than a derived port is what makes it work
 * behind a reverse proxy — a page served at https://host/ has no port to
 * offset from, and the registry listens on loopback only, so the offset only
 * ever resolved for a browser on the dev machine. Anywhere else this is the
 * published registry. Deciding here rather than in the panel keeps "which
 * registry" one answer instead of one per caller.
 */
export function defaultRegistry(): string {
  const dev = import.meta.env?.DEV === true;
  if (!dev || typeof location === "undefined") return PUBLIC_REGISTRY;
  return `${location.origin}${DEV_REGISTRY_PATH}`;
}

export interface RegistryEntry {
  readonly name: string;
  /** What the extension is for, from the crate's `package.description`. */
  readonly description: string;
  readonly file: string;
  readonly blake3: string;
  readonly bytes: number;
  readonly brotliBytes: number;
}

export interface Registry {
  readonly url: string;
  readonly version: string;
  readonly extensions: readonly RegistryEntry[];
}

/** What a client needs from a connection to manage extensions. */
export interface ExtensionHost {
  listExtensions(): Promise<BlitExtensionRecord[]>;
  installExtension(request: {
    hash: Uint8Array;
    name: string;
    module: () => Promise<Uint8Array>;
    args?: readonly string[];
    restart?: number;
    expectedExtensionId?: bigint;
    expectedDefinitionRevision?: bigint;
  }): Promise<BlitExtensionStatus>;
  controlExtension(
    extensionId: bigint,
    action: number,
  ): Promise<BlitExtensionStatus>;
}

/** One extension: installed, offered by the registry, or both. */
export interface ExtensionRow {
  /** Stable across a refresh, and distinct for two unnamed definitions. */
  readonly key: string;
  readonly label: string;
  readonly description: string;
  readonly installed?: BlitExtensionRecord;
  readonly offered?: RegistryEntry;
}

/**
 * The one list the panel shows: installed first, then what only the registry
 * has.
 *
 * Matched by the durable name of a persistent definition, which is the only
 * handle the two sides share — the digest cannot be it, since an update is
 * precisely the case where the digests differ. A transient name is only a
 * label, so it must not claim a registry entry or become an update target.
 */
export function mergeExtensions(
  installed: readonly BlitExtensionRecord[],
  offered: readonly RegistryEntry[],
): ExtensionRow[] {
  const byName = new Map(offered.map((entry) => [entry.name, entry]));
  const claimed = new Set(
    installed
      .filter((record) => (record.flags & EXT_FLAG_PERSIST) !== 0)
      .map((record) => record.name),
  );
  return [
    ...installed.map((record) => {
      const id = formatExtensionId(record.extensionId);
      const match =
        record.name && (record.flags & EXT_FLAG_PERSIST) !== 0
          ? byName.get(record.name)
          : undefined;
      return {
        key: `installed:${id}`,
        label: record.name || `id:${id}`,
        description: match?.description ?? "",
        installed: record,
        offered: match,
      };
    }),
    ...offered
      .filter((entry) => !claimed.has(entry.name))
      .map((entry) => ({
        key: `offered:${entry.name}`,
        label: entry.name,
        description: entry.description,
        offered: entry,
      })),
  ];
}

/** Installed, and the registry offers different bytes under the same name. */
export function isOutdated(row: ExtensionRow): boolean {
  return (
    row.installed !== undefined &&
    row.offered !== undefined &&
    row.installed.hash !== row.offered.blake3
  );
}

function entryOf(value: unknown): RegistryEntry | null {
  if (typeof value !== "object" || value === null) return null;
  const record = value as Record<string, unknown>;
  const name = typeof record.name === "string" ? record.name : "";
  const blake3 = typeof record.blake3 === "string" ? record.blake3 : "";
  if (!name || !parseModuleDigest(blake3)) return null;
  return {
    name,
    // Older registries have none; a missing sentence is not a broken entry.
    description:
      typeof record.description === "string" ? record.description : "",
    file: typeof record.file === "string" ? record.file : `${name}.wasm`,
    blake3: blake3.toLowerCase(),
    bytes: typeof record.bytes === "number" ? record.bytes : 0,
    brotliBytes:
      typeof record.brotli_bytes === "number" ? record.brotli_bytes : 0,
  };
}

/**
 * Read a registry's manifest.
 *
 * Entries without a usable digest are dropped rather than shown: an entry
 * nobody can install is worse than an entry nobody can see.
 */
export async function fetchRegistry(
  url = defaultRegistry(),
  fetcher: typeof fetch = fetch,
): Promise<Registry> {
  const base = url.replace(/\/+$/, "");
  const response = await fetcher(`${base}/manifest.json`, { mode: "cors" });
  if (!response.ok) {
    throw new Error(`${base}/manifest.json: HTTP ${response.status}`);
  }
  const body: unknown = await response.json();
  const record =
    typeof body === "object" && body !== null
      ? (body as Record<string, unknown>)
      : {};
  const listed = Array.isArray(record.extensions) ? record.extensions : [];
  return {
    url: base,
    version: typeof record.version === "string" ? record.version : "",
    extensions: listed
      .map(entryOf)
      .filter((entry): entry is RegistryEntry => entry !== null),
  };
}

/**
 * Install a registry entry, or replace the definition of the same name.
 *
 * The inventory is read again at click time. The panel's rendered row is only
 * a snapshot and may have been produced while the first list was still in
 * flight, or another client may have changed the definition since. A fresh
 * persistent-name match makes installation idempotent and gives updates the
 * current CAS token.
 */
export async function installFromRegistry(
  host: ExtensionHost,
  registry: Registry,
  entry: RegistryEntry,
  fetcher: typeof fetch = fetch,
): Promise<BlitExtensionStatus> {
  const hash = parseModuleDigest(entry.blake3);
  if (!hash) throw new Error(`${entry.name}: manifest digest is not BLAKE3`);
  const installed = (await host.listExtensions()).find(
    (record) =>
      (record.flags & EXT_FLAG_PERSIST) !== 0 && record.name === entry.name,
  );
  return host.installExtension({
    hash,
    name: entry.name,
    restart: EXT_RESTART_ALWAYS,
    expectedExtensionId: installed?.extensionId,
    expectedDefinitionRevision: installed?.definitionRevision,
    module: async () => {
      const response = await fetcher(`${registry.url}/${entry.file}`, {
        mode: "cors",
      });
      if (!response.ok) {
        throw new Error(`${entry.file}: HTTP ${response.status}`);
      }
      return new Uint8Array(await response.arrayBuffer());
    },
  });
}

const pause = (milliseconds: number) =>
  new Promise<void>((resolve) => setTimeout(resolve, milliseconds));

/** Disable a persistent definition, wait for its attempt to stop, then remove it. */
export async function disableAndRemoveExtension(
  host: ExtensionHost,
  record: BlitExtensionRecord,
  wait: (milliseconds: number) => Promise<void> = pause,
): Promise<void> {
  let status = await host.controlExtension(
    record.extensionId,
    EXT_CONTROL_DISABLE,
  );
  for (let attempt = 0; ; attempt++) {
    if (
      status.phase === EXT_PHASE_STOPPED ||
      status.phase === EXT_PHASE_BLOCKED
    ) {
      break;
    }
    if (attempt === 99) {
      throw new Error(`${record.name}: extension did not stop before removal`);
    }
    await wait(50);
    status = await host.controlExtension(
      record.extensionId,
      EXT_CONTROL_STATUS,
    );
  }
  await host.controlExtension(record.extensionId, EXT_CONTROL_REMOVE);
}
