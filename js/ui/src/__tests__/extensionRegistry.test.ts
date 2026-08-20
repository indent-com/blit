import { describe, expect, it, vi } from "vitest";
import {
  EXT_CONTROL_DISABLE,
  EXT_CONTROL_REMOVE,
  EXT_CONTROL_STATUS,
  EXT_PHASE_STOPPED,
  EXT_PHASE_STOPPING,
} from "@blit-sh/core";
import {
  PUBLIC_REGISTRY,
  defaultRegistry,
  disableAndRemoveExtension,
  fetchRegistry,
  installFromRegistry,
  isOutdated,
  mergeExtensions,
  type Registry,
  type RegistryEntry,
} from "../extensionRegistry";

const DIGEST =
  "2ce9c852e69a2931610d10221bb4855f93333aa1d64eef8bc07e0d3b9e2c804f";

const manifest = {
  version: "0.53.2",
  extensions: [
    {
      name: "systemd",
      description: "Live systemd system and user unit state",
      file: "systemd.wasm",
      blake3: DIGEST,
      bytes: 94263,
      brotli_bytes: 36950,
    },
    // No digest: an entry nobody could install is not shown at all.
    { name: "broken", file: "broken.wasm" },
    // No description: a registry that predates the field still installs.
    { name: "session", file: "session.wasm", blake3: DIGEST },
  ],
};

const jsonResponse = (body: unknown) =>
  ({ ok: true, status: 200, json: async () => body }) as unknown as Response;

describe("extension registry", () => {
  it("reads a manifest and drops entries without a digest", async () => {
    const fetcher = vi.fn(async () => jsonResponse(manifest));
    const registry = await fetchRegistry(PUBLIC_REGISTRY, fetcher as never);
    expect(fetcher).toHaveBeenCalledWith(
      "https://install.blit.sh/ext/manifest.json",
      { mode: "cors" },
    );
    expect(registry.extensions.map((entry) => entry.name)).toEqual([
      "systemd",
      "session",
    ]);
    expect(registry.extensions[0]!.brotliBytes).toBe(36950);
    // The sentence the panel shows under the name, and its absence.
    expect(registry.extensions[0]!.description).toBe(
      "Live systemd system and user unit state",
    );
    expect(registry.extensions[1]!.description).toBe("");
  });

  // A dev page is often reached over a tunnel (https://host/, no port) and
  // the stack's registry listens on loopback only. Deriving a port from the
  // page sent those sessions to the public registry instead; staying on the
  // origin lets the dev server proxy it.
  it("defaults to the page's own origin in dev, whatever the port", () => {
    for (const href of [
      "https://blitdev.example.com/",
      "http://127.0.0.1:10000/",
      "http://127.0.0.1:10010/",
    ]) {
      const url = new URL(href);
      vi.stubGlobal("location", { origin: url.origin });
      expect(defaultRegistry()).toBe(`${url.origin}/ext`);
    }
    vi.unstubAllGlobals();
  });

  it("reports an unreachable registry rather than showing nothing", async () => {
    const fetcher = vi.fn(async () => ({ ok: false, status: 404 }) as Response);
    await expect(
      fetchRegistry("https://example.test/ext", fetcher as never),
    ).rejects.toThrow(/HTTP 404/);
  });

  it("installs by digest and fetches the module only on demand", async () => {
    const registry: Registry = {
      url: "https://install.blit.sh/ext",
      version: "0.53.2",
      extensions: [
        {
          name: "systemd",
          description: "",
          file: "systemd.wasm",
          blake3: DIGEST,
          bytes: 1,
          brotliBytes: 1,
        },
      ],
    };
    const fetcher = vi.fn(
      async () =>
        ({
          ok: true,
          status: 200,
          arrayBuffer: async () => new Uint8Array([0, 97, 115, 109]).buffer,
        }) as unknown as Response,
    );
    const host = {
      listExtensions: vi.fn(async () => []),
      controlExtension: vi.fn(),
      installExtension: vi.fn(async (request: any) => {
        expect(Array.from(request.hash).length).toBe(32);
        // The server has it: the bytes are never fetched.
        return { phase: 4, status: 0 };
      }),
    };
    await installFromRegistry(
      host as never,
      registry,
      registry.extensions[0]!,
      fetcher as never,
    );
    expect(fetcher).not.toHaveBeenCalled();

    // And when it asks, the module comes from the registry's own base URL.
    host.installExtension = vi.fn(async (request: any) => {
      await request.module();
      return { phase: 4, status: 0 };
    }) as never;
    await installFromRegistry(
      host as never,
      registry,
      registry.extensions[0]!,
      fetcher as never,
    );
    expect(fetcher).toHaveBeenCalledWith(
      "https://install.blit.sh/ext/systemd.wasm",
      { mode: "cors" },
    );
  });

  it("carries the CAS token of the definition it replaces", async () => {
    const registry: Registry = {
      url: "https://r.test",
      version: "1",
      extensions: [
        {
          name: "systemd",
          description: "",
          file: "systemd.wasm",
          blake3: DIGEST,
          bytes: 1,
          brotliBytes: 1,
        },
      ],
    };
    const host = {
      // The pane may still have rendered this name as available. Installation
      // must use this fresh inventory, not that stale render-time snapshot.
      listExtensions: vi.fn(async () => [
        {
          name: "systemd",
          flags: 3,
          extensionId: 7n,
          definitionRevision: 3n,
        },
      ]),
      installExtension: vi.fn(async () => ({ phase: 4, status: 0 })),
    };
    await installFromRegistry(host as never, registry, registry.extensions[0]!);
    const [first] = host.installExtension.mock.calls[0] as unknown as [
      Record<string, unknown>,
    ];
    expect(first).toMatchObject({
      expectedExtensionId: 7n,
      expectedDefinitionRevision: 3n,
    });
  });
});

const OTHER_DIGEST =
  "aa11c852e69a2931610d10221bb4855f93333aa1d64eef8bc07e0d3b9e2c804f";

const offer = (name: string, blake3: string): RegistryEntry => ({
  name,
  description: `what ${name} does`,
  file: `${name}.wasm`,
  blake3,
  bytes: 1,
  brotliBytes: 1,
});

const record = (name: string, hash: string, extensionId: bigint, flags = 3) =>
  ({ name, hash, extensionId, phase: 4, flags }) as never;

describe("merging installed with the registry", () => {
  // The panel used to show installed and offered as two tables, so an
  // installed extension the registry also offers was named twice and its
  // update read as a fresh install.
  it("names an extension once, whichever side has it", () => {
    const rows = mergeExtensions(
      [record("systemd", DIGEST, 7n), record("local", OTHER_DIGEST, 8n)],
      [offer("systemd", DIGEST), offer("session", DIGEST)],
    );
    expect(rows.map((row) => row.label)).toEqual([
      "systemd",
      "local",
      "session",
    ]);
    // Installed and offered are one row; the description comes from the offer.
    expect(rows[0]!.installed?.extensionId).toBe(7n);
    expect(rows[0]!.offered?.blake3).toBe(DIGEST);
    expect(rows[0]!.description).toBe("what systemd does");
    // Installed with nothing offering it: no description, nothing to update to.
    expect(rows[1]!.offered).toBeUndefined();
    expect(rows[2]!.installed).toBeUndefined();
    expect(new Set(rows.map((row) => row.key)).size).toBe(3);
  });

  // Two anonymous `ext run` definitions share the empty name; keying on it
  // would fold them into one row and let one claim a registry entry.
  it("keeps unnamed definitions apart and lets them claim nothing", () => {
    const rows = mergeExtensions(
      [record("", DIGEST, 9n), record("", DIGEST, 10n)],
      [offer("systemd", DIGEST)],
    );
    expect(rows).toHaveLength(3);
    expect(new Set(rows.map((row) => row.key)).size).toBe(3);
    expect(rows[0]!.label).toBe("id:0000000000000009");
    expect(rows[0]!.offered).toBeUndefined();
    expect(rows[2]!.label).toBe("systemd");
  });

  it("does not let a transient label claim a durable registry name", () => {
    const rows = mergeExtensions(
      [record("systemd", DIGEST, 9n, 1)],
      [offer("systemd", DIGEST)],
    );
    expect(rows).toHaveLength(2);
    expect(rows[0]!.installed?.extensionId).toBe(9n);
    expect(rows[0]!.offered).toBeUndefined();
    expect(rows[1]!.installed).toBeUndefined();
    expect(rows[1]!.offered?.name).toBe("systemd");
  });

  // The digest is the identity: same name, different bytes is the update.
  it("calls a row outdated only when the digests differ", () => {
    const [current] = mergeExtensions(
      [record("systemd", DIGEST, 7n)],
      [offer("systemd", DIGEST)],
    );
    const [stale] = mergeExtensions(
      [record("systemd", OTHER_DIGEST, 7n)],
      [offer("systemd", DIGEST)],
    );
    const [uninstalled] = mergeExtensions([], [offer("systemd", DIGEST)]);
    const [unoffered] = mergeExtensions([record("x", DIGEST, 7n)], []);
    expect(isOutdated(current!)).toBe(false);
    expect(isOutdated(stale!)).toBe(true);
    expect(isOutdated(uninstalled!)).toBe(false);
    expect(isOutdated(unoffered!)).toBe(false);
  });
});

describe("extension removal", () => {
  it("waits for disable to become quiescent before removing", async () => {
    const actions: number[] = [];
    const host = {
      controlExtension: vi.fn(async (_extensionId: bigint, action: number) => {
        actions.push(action);
        return {
          status: 0,
          phase:
            action === EXT_CONTROL_DISABLE
              ? EXT_PHASE_STOPPING
              : EXT_PHASE_STOPPED,
        };
      }),
    };
    const wait = vi.fn(async () => {});
    await disableAndRemoveExtension(
      host as never,
      record("systemd", DIGEST, 7n),
      wait,
    );
    expect(actions).toEqual([
      EXT_CONTROL_DISABLE,
      EXT_CONTROL_STATUS,
      EXT_CONTROL_REMOVE,
    ]);
    expect(wait).toHaveBeenCalledWith(50);
  });
});
