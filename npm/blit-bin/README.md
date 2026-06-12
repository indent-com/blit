# blit-bin

The [blit](https://blit.sh) binary, distributed via npm. Installing `blit-bin`
pulls in exactly one prebuilt package for your platform
(`blit-bin-<os>-<cpu>[-musl]`) through optional dependencies — nothing else.

## CLI

```sh
npm i -g blit-bin
blit open
```

## Bundle the binary in your own tool

The default export is the absolute filesystem path to the `blit` executable, so
you can spawn it directly. Resolution happens on import and throws with an
actionable message if the matching prebuilt package was not installed.

### ESM

```js
import blit from "blit-bin";
import { spawn } from "node:child_process";

spawn(blit, ["open"], { stdio: "inherit" });
```

### CommonJS

```js
const blit = require("blit-bin");
const { spawn } = require("node:child_process");

spawn(blit, ["open"], { stdio: "inherit" });
```

### Helpers

Lower-level resolution helpers are available on the `blit-bin/resolve` subpath
(and as named exports of the main entry):

```js
import { binaryPath, binaryName, candidatePackages, isMusl } from "blit-bin";
// or: import { binaryPath } from "blit-bin/resolve";
```

| export | description |
| --- | --- |
| `default` | absolute path to the `blit` binary (resolved at import) |
| `binaryPath()` | same path, computed lazily; throws if unavailable |
| `binaryName()` | `"blit"` or `"blit.exe"` |
| `candidatePackages()` | platform package names, in resolution order |
| `isMusl()` | `true` on musl-libc Linux |

## Platforms

Linux x64/arm64 (glibc & musl), macOS arm64, Windows x64 — matching the
binaries the blit release pipeline builds.

## License

MIT
