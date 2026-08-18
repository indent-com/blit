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
  EXT_RESTART_ALWAYS,
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
 * itself: the dev server proxies `/ext` to the port `bin/dev` allocated for
 * this instance, so a second stack's page still offers that stack's modules.
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

function entryOf(value: unknown): RegistryEntry | null {
  if (typeof value !== "object" || value === null) return null;
  const record = value as Record<string, unknown>;
  const name = typeof record.name === "string" ? record.name : "";
  const blake3 = typeof record.blake3 === "string" ? record.blake3 : "";
  if (!name || !parseModuleDigest(blake3)) return null;
  return {
    name,
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
 * `installed` is what the server currently has, so an update carries the CAS
 * token of the definition it means to replace and cannot adopt a different
 * one that appeared in between.
 */
export async function installFromRegistry(
  host: ExtensionHost,
  registry: Registry,
  entry: RegistryEntry,
  installed?: BlitExtensionRecord,
  fetcher: typeof fetch = fetch,
): Promise<BlitExtensionStatus> {
  const hash = parseModuleDigest(entry.blake3);
  if (!hash) throw new Error(`${entry.name}: manifest digest is not BLAKE3`);
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
