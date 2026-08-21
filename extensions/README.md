# Blit extensions

Extensions that are meant to be run, as opposed to read. Rust extensions are
compiled to Wasm; TypeScript extensions are bundled to one ECMAScript module
and run in native QuickJS. The teaching examples — one API each, a few dozen
lines — stay in
[`crates/guest/examples`](../crates/guest/examples); anything here is something
you would install on a server.

The Rust extensions are a separate cargo workspace on purpose. Every member
only makes sense as a `wasm32-unknown-unknown` module, so keeping them out of the
root workspace stops a plain `cargo build`/`clippy`/`test` at the root from
trying to build a Wasm guest for the host. The root manifest lists `extensions`
in its `exclude`.

| extension            | what it does                                                                                                                                                                                                                      |
| -------------------- | --------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| [`doctor`](doctor)   | check the server handshake, native QuickJS runtime, extension lifecycle, clocks, entropy, and advertised capabilities: `@doctor [--json]`                                                                                         |
| [`muster`](muster)   | supervise units that run in terminals, from `~/.config/blit/instances/NAME/muster`, with a unit ▸ terminal ▸ windows tree on the `blit.muster.v1` channel: `@muster list\|status\|start\|stop\|restart\|instantiate\|log\|doctor` |
| [`session`](session) | autostart and supervise GUI applications: `@session list\|enable\|disable\|start\|stop\|forget\|status`                                                                                                                           |
| [`systemd`](systemd) | live system and user unit state on the `blit.systemd.v1` channel, plus a live/paged journal reader: `@systemd list\|get\|watch\|logs\|status`                                                                                     |

## Building

```bash
./bin/extensions
```

That tests and bundles the TypeScript extensions, builds every Rust member for
Wasm, runs `wasm-opt -Oz`, and writes `extensions/dist/` — one `.js` or `.wasm`
object per extension, a brotli copy, and a `manifest.json` naming each object's
BLAKE3 digest:

```json
{
  "version": "0.53.2",
  "extensions": [
    {
      "name": "doctor",
      "description": "Check a Blit server, the extension runtime, and advertised capabilities",
      "file": "doctor.js",
      "blake3": "d41f…",
      "bytes": 13397,
      "brotli_bytes": 4479
    }
  ]
}
```

The digest is not decoration. A module's identity in the protocol _is_ its
BLAKE3 digest, so a published URL is only pinnable if the digest is published
next to it.

The description is copied from the Rust crate or TypeScript package, so the
browser has something to show under the name — a registry is otherwise a list
of words and hashes, and neither says what installing one would do. Rust
descriptions are keyed by the bin target's name because that is what the
published object is called: the crate is `blit-ext-systemd`, the object is
`systemd.wasm`.

TypeScript source imports the small host and command-provider library in
[`typescript`](typescript). Bun removes the types and bundles those imports;
QuickJS receives one dependency-free `.js` file and does no build work.

## Where releases put them

The release workflow builds these architecture-independent objects once and
publishes them twice:

- **`https://install.blit.sh/ext/<file>`**, with
  `https://install.blit.sh/ext/manifest.json` beside it. Like `install.blit.sh/bin`,
  this is the _current_ release only — Pages publishes the tree wholesale, so
  the previous version's bytes stop resolving when the next release lands.
- **GitHub Release assets**, `…/releases/download/v<version>/<file>`. This
  is the durable home: a `#digest` pin outlives its version here and nowhere
  else.

```bash
# latest, trusting TLS and the host
blit ext run --persist --restart always systemd \
  https://install.blit.sh/ext/systemd.wasm

blit ext run --persist --restart always doctor \
  https://install.blit.sh/ext/doctor.js
blit @doctor

# one exact object, forever
blit ext run --persist --restart always systemd \
  https://github.com/indent-com/blit/releases/download/v0.53.2/systemd.wasm#2672...
```

With a pin the client asks the server first and downloads only if the server
does not already have that object; without one it must fetch before it can name
anything.

## Installing one from the browser

The Extensions tab of an expanded remote installs from a registry — a
`manifest.json` and the modules beside it. It defaults to
`https://install.blit.sh/ext`, except under `vite dev`, where it defaults to
the dev stack's own registry: `.blit/muster/extensions.json` builds
`extensions/dist` and serves it on the UI's port plus three, so what you install
is what you just compiled.

Installed and offered are one list, named once. An extension the server already
runs shows _Update_ when the registry offers a different digest under the same
name — which replaces the definition in place, keeping its identity — and
_Current_ when the digests match.

## Installing one locally

```bash
./bin/extensions
blit ext run --persist --restart always doctor extensions/dist/doctor.js
blit @doctor
blit @doctor --json

blit ext run --persist --restart always systemd extensions/dist/systemd.wasm
blit @systemd status
```

`--persist` needs a server that permits persistent extensions, which is the
default (`--no-persistent-extensions` turns it off), and it is also what makes
the `@systemd` command namespace available.
