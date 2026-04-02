import wasmBuffer from "virtual:blit-wasm";
import init from "@blit-sh/browser";

let initPromise: Promise<typeof import("@blit-sh/browser")> | null = null;

export function initWasm(): Promise<typeof import("@blit-sh/browser")> {
  if (!initPromise) {
    initPromise = init({ module_or_path: wasmBuffer }).then(
      () => import("@blit-sh/browser"),
    );
  }
  return initPromise;
}
