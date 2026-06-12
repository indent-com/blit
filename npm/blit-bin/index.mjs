// ESM entry point. Default export is the absolute path to the platform `blit`
// binary; named exports expose the resolution helpers.
//
//   import blit from "blit-bin";
//   import { spawn } from "node:child_process";
//   spawn(blit, ["open"], { stdio: "inherit" });
import resolve from "./resolve.js";

const blitPath = resolve.binaryPath();

export default blitPath;
export const binaryPath = resolve.binaryPath;
export const binaryName = resolve.binaryName;
export const candidatePackages = resolve.candidatePackages;
export const isMusl = resolve.isMusl;
