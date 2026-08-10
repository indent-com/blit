import { describe, it, expect } from "vitest";
import { readdirSync, readFileSync } from "node:fs";
import { dirname, join, resolve } from "node:path";

// `@blit-sh/core` is developed against `src/*.ts` (package.json points there)
// but published as `dist/*.js`, and tsc copies a `new URL(...)` literal into the
// emitted file verbatim. A specifier naming the TypeScript file therefore
// resolves in this repo and fails for every consumer of the tarball, which is
// what shipped in 0.47.0. `.js` works on both sides: Vite maps a `.js`
// specifier back to its `.ts` source.
const srcRoot = resolve(__dirname, "..");

function tsFiles(dir: string): string[] {
  return readdirSync(dir, { withFileTypes: true }).flatMap((entry) => {
    const path = join(dir, entry.name);
    if (entry.isDirectory())
      return entry.name === "__tests__" ? [] : tsFiles(path);
    return entry.name.endsWith(".ts") ? [path] : [];
  });
}

describe("relative import.meta.url assets", () => {
  it("name the emitted file, not the source file", () => {
    const offenders: string[] = [];
    for (const file of tsFiles(srcRoot)) {
      const source = readFileSync(file, "utf8");
      for (const match of source.matchAll(
        /new URL\(\s*"(\.[^"]*)"\s*,\s*import\.meta\.url\s*\)/g,
      )) {
        const specifier = match[1]!;
        if (!specifier.endsWith(".js")) offenders.push(`${file}: ${specifier}`);
      }
    }
    expect(offenders).toEqual([]);
  });

  it("resolve to a file that exists in src", () => {
    const missing: string[] = [];
    for (const file of tsFiles(srcRoot)) {
      const source = readFileSync(file, "utf8");
      for (const match of source.matchAll(
        /new URL\(\s*"(\.[^"]*\.js)"\s*,\s*import\.meta\.url\s*\)/g,
      )) {
        const specifier = match[1]!;
        const target = resolve(dirname(file), specifier).replace(
          /\.js$/,
          ".ts",
        );
        try {
          readFileSync(target);
        } catch {
          missing.push(`${file}: ${specifier}`);
        }
      }
    }
    expect(missing).toEqual([]);
  });
});
