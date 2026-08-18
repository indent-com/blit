import { describe, expect, it, vi } from "vitest";
import {
  PUBLIC_REGISTRY,
  defaultRegistry,
  fetchRegistry,
  installFromRegistry,
  type Registry,
} from "../extensionRegistry";

const DIGEST =
  "2ce9c852e69a2931610d10221bb4855f93333aa1d64eef8bc07e0d3b9e2c804f";

const manifest = {
  version: "0.53.2",
  extensions: [
    {
      name: "systemd",
      file: "systemd.wasm",
      blake3: DIGEST,
      bytes: 94263,
      brotli_bytes: 36950,
    },
    // No digest: an entry nobody could install is not shown at all.
    { name: "broken", file: "broken.wasm" },
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
    expect(registry.extensions.map((entry) => entry.name)).toEqual(["systemd"]);
    expect(registry.extensions[0]!.brotliBytes).toBe(36950);
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
      listExtensions: vi.fn(),
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
      undefined,
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
      undefined,
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
          file: "systemd.wasm",
          blake3: DIGEST,
          bytes: 1,
          brotliBytes: 1,
        },
      ],
    };
    const host = {
      installExtension: vi.fn(async () => ({ phase: 4, status: 0 })),
    };
    await installFromRegistry(
      host as never,
      registry,
      registry.extensions[0]!,
      { extensionId: 7n, definitionRevision: 3n } as never,
    );
    const [first] = host.installExtension.mock.calls[0] as unknown as [
      Record<string, unknown>,
    ];
    expect(first).toMatchObject({
      expectedExtensionId: 7n,
      expectedDefinitionRevision: 3n,
    });
  });
});
