"use strict";

// Platform binary resolution for @blit-sh/bin. Mirrors the release artifact matrix
// (see bin/build-npm-bin-packages). Kept separate from the public entry points
// (index.js / index.mjs) so the CLI and both module systems share one copy.

const fs = require("fs");

// This same file ships in @blit-sh/bin and in @blit-sh/bin-gpl, whose platform
// packages are named after their own launcher, so read the prefix off our own
// manifest rather than hardcoding one flavor.
const SELF = require("./package.json").name;

// Map process.platform/process.arch to the npm package that ships the binary.
function candidatePackages() {
  const platform = process.platform;
  const arch = process.arch;
  if (platform === "win32") return [`${SELF}-win32-${arch}`];
  if (platform === "darwin") return [`${SELF}-darwin-${arch}`];
  if (platform === "linux") {
    const base = `${SELF}-linux-${arch}`;
    // Prefer the libc variant we detect, but fall back to the other.
    return isMusl() ? [`${base}-musl`, base] : [base, `${base}-musl`];
  }
  return [];
}

// Detect musl libc on Linux. Uses the same signal as detect-libc: glibc
// runtimes expose glibcVersionRuntime in the Node process report header.
function isMusl() {
  if (process.platform !== "linux") return false;
  try {
    const header = process.report.getReport().header;
    return !header.glibcVersionRuntime;
  } catch {
    return false;
  }
}

function binaryName() {
  return process.platform === "win32" ? "blit.exe" : "blit";
}

// Resolve the absolute path to the platform binary, or throw a helpful error.
function binaryPath() {
  const exe = binaryName();
  const tried = [];
  for (const pkg of candidatePackages()) {
    tried.push(pkg);
    let resolved;
    try {
      resolved = require.resolve(`${pkg}/bin/${exe}`);
    } catch {
      continue;
    }
    if (process.platform !== "win32") {
      try {
        fs.chmodSync(resolved, 0o755);
      } catch {
        // read-only install; ignore.
      }
    }
    return resolved;
  }
  throw new Error(
    [
      `${SELF}: no prebuilt binary found for ${process.platform} ${process.arch}.`,
      tried.length ? `Tried optional packages: ${tried.join(", ")}.` : "",
      SELF === "@blit-sh/bin-gpl"
        ? "The GPL flavor is Linux-only: x64/arm64, glibc & musl."
        : "Supported: linux x64/arm64 (glibc & musl), darwin arm64, win32 x64.",
      "If your platform is supported, ensure optional dependencies were not",
      "skipped (e.g. npm install without --no-optional / --omit=optional).",
    ]
      .filter(Boolean)
      .join("\n"),
  );
}

module.exports = { binaryPath, binaryName, candidatePackages, isMusl };
