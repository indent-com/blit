import wasmBuffer from "virtual:blit-wasm";

let initPromise: Promise<typeof import("blit-browser")> | null = null;

export function initWasm(): Promise<typeof import("blit-browser")> {
  if (!initPromise) {
    initPromise = import("blit-browser").then(async (mod) => {
      await mod.default({ module_or_path: wasmBuffer });
      return mod;
    });
  }
  return initPromise;
}
