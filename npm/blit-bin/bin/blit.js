#!/usr/bin/env node
"use strict";

const { spawnSync } = require("child_process");
const { binaryPath } = require("../resolve.js");

let bin;
try {
  bin = binaryPath();
} catch (err) {
  process.stderr.write(`${err.message}\n`);
  process.exit(1);
}

const result = spawnSync(bin, process.argv.slice(2), { stdio: "inherit" });

if (result.error) {
  process.stderr.write(
    `blit-bin: failed to launch ${bin}: ${result.error.message}\n`,
  );
  process.exit(1);
}

// Re-raise the child's terminating signal, otherwise forward its exit code.
if (result.signal) {
  process.kill(process.pid, result.signal);
  process.exit(1);
}

process.exit(result.status === null ? 1 : result.status);
