# @blit-sh/bin

The [blit](https://blit.sh) binary, distributed via npm. Installing `@blit-sh/bin`
pulls in exactly one prebuilt package for your platform
(`@blit-sh/bin-<os>-<cpu>[-musl]`) through optional dependencies — nothing else.

## CLI

```sh
npm i -g @blit-sh/bin
blit open
```

## Bundle the binary in your own tool

The default export is the absolute filesystem path to the `blit` executable, so
you can spawn it directly. Resolution happens on import and throws with an
actionable message if the matching prebuilt package was not installed.

### ESM

```js
import blit from "@blit-sh/bin";
import { spawn } from "node:child_process";

spawn(blit, ["open"], { stdio: "inherit" });
```

### CommonJS

```js
const blit = require("@blit-sh/bin");
const { spawn } = require("node:child_process");

spawn(blit, ["open"], { stdio: "inherit" });
```

### Helpers

Lower-level resolution helpers are available on the `@blit-sh/bin/resolve` subpath
(and as named exports of the main entry):

```js
import {
  binaryPath,
  binaryName,
  candidatePackages,
  isMusl,
} from "@blit-sh/bin";
// or: import { binaryPath } from "@blit-sh/bin/resolve";
```

| export                | description                                             |
| --------------------- | ------------------------------------------------------- |
| `default`             | absolute path to the `blit` binary (resolved at import) |
| `binaryPath()`        | same path, computed lazily; throws if unavailable       |
| `binaryName()`        | `"blit"` or `"blit.exe"`                                |
| `candidatePackages()` | platform package names, in resolution order             |
| `isMusl()`            | `true` on musl-libc Linux                               |

## Platforms

Linux x64/arm64 (glibc & musl), macOS arm64, Windows x64 — matching the
binaries the blit release pipeline builds.

## GPL flavor

[`@blit-sh/bin-gpl`](https://www.npmjs.com/package/@blit-sh/bin-gpl) ships the
same build with x264 (GPL-2.0-or-later) instead of openh264 for software H.264:
better compression, and 4:4:4 rather than 4:2:0. Linux only, same API, no `blit`
CLI shim so it installs alongside this package.

## License

MIT
