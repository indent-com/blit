import { readFile } from "node:fs/promises";
import { fileURLToPath } from "node:url";

import init from "@blit-sh/browser";

import type { BlitWasmModule } from "./TerminalStore";

/**
 * Initialise the `@blit-sh/browser` WASM module in a non-browser runtime
 * (Node / Bun / Deno) and return the module namespace, ready to hand to
 * `new BlitWorkspace({ wasm })`.
 *
 * Why this exists: the `@blit-sh/browser` package published today is a
 * wasm-bindgen `--target web` build, so its default `init()` assumes a
 * browser that can `fetch(new URL("blit_browser_bg.wasm", import.meta.url))`.
 * Under Node/Bun there is no such fetch and `init()` rejects with an opaque,
 * stackless error. `loadBlitWasm()` instead resolves the `.wasm` that ships
 * alongside `@blit-sh/browser`, reads its bytes from disk and feeds them to
 * `init({ module_or_path })` — so consumers never touch raw wasm bytes and a
 * missing/incorrect asset fails with a real filesystem error.
 *
 * It is also forward-compatible with a self-initializing build (e.g. a
 * `--target nodejs` artifact resolved via the `node` export condition): such a
 * build has no `init` default export and instantiates itself on import, so we
 * detect that and return it as-is without any filesystem access.
 *
 * @param wasmPath Optional override for the `.wasm` location. Accepts a
 *   filesystem path or a `file:` URL string; defaults to the asset colocated
 *   with `@blit-sh/browser`.
 */
export async function loadBlitWasm(wasmPath?: string): Promise<BlitWasmModule> {
  const mod =
    (await import("@blit-sh/browser")) as unknown as BlitWasmModule & {
      default?: unknown;
    };

  // A self-initializing build (`--target nodejs`/`bundler`) has already
  // instantiated the module on import and exposes no `init` default export.
  if (typeof mod.default !== "function") {
    return mod;
  }

  const location =
    wasmPath ?? import.meta.resolve("@blit-sh/browser/blit_browser_bg.wasm");
  const path = location.startsWith("file:")
    ? fileURLToPath(location)
    : location;
  const bytes = await readFile(path);
  await init({
    module_or_path: bytes as unknown as Parameters<typeof init>[0],
  });
  return mod;
}
