/**
 * Replays a .blitrec (blit terminal record) through the real WASM terminal
 * and prints the final screen — the hero's pipeline, minus the renderer.
 *
 *   bun scripts/validate-blitrec.ts public/demo/hero-tests.blitrec
 */
import { readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
// Straight at the wasm-pack output: loadBlitWasm exists for consumers of the
// published package, and both bun and node trip over resolving it from a
// workspace script (bun picks the .d.ts, node refuses extensionless ESM).
import init, { Terminal } from "../../../crates/browser/pkg/blit_browser.js";

const wasmPath = fileURLToPath(
  new URL("../../../crates/browser/pkg/blit_browser_bg.wasm", import.meta.url),
);
await init({ module_or_path: readFileSync(wasmPath) });

const buf = readFileSync(process.argv[2]);
if (buf.subarray(0, 8).toString() !== "BLITREC\n") throw new Error("bad magic");
const frames: { t: number; data: Uint8Array }[] = [];
let off = 8;
while (off + 12 <= buf.length) {
  const t = Number(buf.readBigUInt64LE(off));
  const len = buf.readUInt32LE(off + 8);
  frames.push({
    t,
    data: new Uint8Array(buf.subarray(off + 12, off + 12 + len)),
  });
  off += 12 + len;
}
console.log(
  `${frames.length} frames, ${(frames.at(-1)!.t / 1e6).toFixed(1)}s, ${buf.length} bytes`,
);

const term = new Terminal(24, 80, 8, 16);
for (const f of frames) term.feed_compressed(f.data);
console.log("--- final screen ---");
console.log(term.get_text(0, 0, 23, 79).trimEnd());
