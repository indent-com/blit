# Blit extensions

Wasm extensions that are meant to be run, as opposed to read. The teaching
examples — one API each, a few dozen lines — stay in
[`crates/guest/examples`](../crates/guest/examples); anything here is something
you would install on a server.

This is a separate cargo workspace on purpose. Every member only makes sense as
a `wasm32-unknown-unknown` module, so keeping them out of the root workspace
stops a plain `cargo build`/`clippy`/`test` at the root from trying to build a
Wasm guest for the host. The root manifest lists `extensions` in its `exclude`.

| extension            | what it does                                                                                                                                  |
| -------------------- | --------------------------------------------------------------------------------------------------------------------------------------------- |
| [`session`](session) | autostart and supervise GUI applications: `@session list\|enable\|disable\|start\|stop\|forget\|status`                                       |
| [`systemd`](systemd) | live system and user unit state on the `blit.systemd.v1` channel, plus a live/paged journal reader: `@systemd list\|get\|watch\|logs\|status` |

## Building

```bash
./bin/extensions
```

That builds every member for wasm, runs `wasm-opt -Oz`, and writes
`extensions/dist/` — one `.wasm` per extension, a brotli copy, and a
`manifest.json` naming each module's BLAKE3 digest:

```json
{
  "version": "0.53.2",
  "extensions": [
    {
      "name": "systemd",
      "file": "systemd.wasm",
      "blake3": "2672…",
      "bytes": 91370,
      "brotli_bytes": 35582
    }
  ]
}
```

The digest is not decoration. A module's identity in the protocol _is_ its
BLAKE3 digest, so a published URL is only pinnable if the digest is published
next to it.

## Where releases put them

The release workflow builds these once (wasm is architecture-independent) and
publishes them twice:

- **`https://install.blit.sh/ext/<name>.wasm`**, with
  `https://install.blit.sh/ext/manifest.json` beside it. Like `install.blit.sh/bin`,
  this is the _current_ release only — Pages publishes the tree wholesale, so
  the previous version's bytes stop resolving when the next release lands.
- **GitHub Release assets**, `…/releases/download/v<version>/<name>.wasm`. This
  is the durable home: a `#digest` pin outlives its version here and nowhere
  else.

```bash
# latest, trusting TLS and the host
blit ext run --persist --restart always systemd \
  https://install.blit.sh/ext/systemd.wasm

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
the dev stack's own registry: `bin/dev` builds `extensions/dist` and serves it
on the UI's port plus three, so what you install is what you just compiled.

## Installing one locally

```bash
./bin/extensions
blit ext run --persist --restart always systemd extensions/dist/systemd.wasm
blit @systemd status
```

`--persist` needs a server started with `--allow-persistent-extensions`, and it
is also what makes the `@systemd` command namespace available.
