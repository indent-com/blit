"use strict";

// Default (CommonJS) export: the absolute filesystem path to the prebuilt
// `blit` executable for the current platform.
//
//   const blit = require("@blit-sh/bin");
//   require("child_process").spawn(blit, ["open"], { stdio: "inherit" });
//
// Throws at require time with an actionable message if no matching prebuilt
// package is installed. Named helpers live on `@blit-sh/bin/resolve`.
module.exports = require("./resolve.js").binaryPath();
