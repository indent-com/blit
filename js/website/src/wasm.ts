import wasmBuffer from "virtual:blit-wasm";

let initPromise: Promise<typeof import("@blit-sh/browser")> | null = null;

export function initWasm(): Promise<typeof import("@blit-sh/browser")> {
  if (!initPromise) {
    initPromise = import("@blit-sh/browser").then(async (mod) => {
      await mod.default({ module_or_path: wasmBuffer });
      return mod;
    });
  }
  return initPromise;
}
