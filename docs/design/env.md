# RFC: Server Environment

- **Status:** Implemented (`FEATURE_ENV`, protocol feature bit 24)
- **Date:** 2026-08-18
- **Companion to:** [processes.md](processes.md), [extensions.md](extensions.md),
  [kv.md](kv.md), [../protocol.md](../protocol.md)

## Summary

One request, one reply: a client asks for the blit server's environment and
receives every variable, sorted by key.

It exists because **a client has no other way to learn anything about the
session it is attached to.** Before this, `WAYLAND_DISPLAY` appeared exactly once
in the whole server — inside a dead function — and no message carried the
compositor socket, `XDG_RUNTIME_DIR`, the desktop bus address, or the audio
sockets. That knowledge belonged solely to the PTY spawn path.

The immediate consumer is a Wasm extension. An extension's host ABI is five
imports — `send`, `recv`, `wait`, `clock`, `random` — with no filesystem, no
process, and no environment access; everything else it does, it does by speaking
this protocol as an ordinary client. So an extension that wants to enumerate
installed applications cannot read `XDG_DATA_DIRS` to find them. It can already
_read_ `/usr/share/applications` through the fs family, which accepts an
arbitrary root. It just could not find out where to look.

Putting this in the protocol rather than adding a sixth wasm import is
deliberate: the ABI is kept minimal on purpose, and a protocol family gets
feature negotiation, a dispatch-level kill switch, and reach for every client
rather than extensions alone.

## Wire

Both directions use opcode `0x75`; the protocol is direction-local.

    C2S_ENV_GET   [0x75][nonce:2]
    S2C_ENV       [0x75][nonce:2][status:1][count:2]
                  then count × [key_len:2][key:N][value_len:4][value:N]

Records are ascending by key, so an unchanged environment encodes to identical
bytes. Keys and values are **raw bytes, not UTF-8**: a Unix environment carries
no such guarantee, and dropping an entry that failed to decode would be a worse
answer than handing it over as it is.

A NUL in either half is `INVALID` — it cannot survive `execve`, so the codec
refuses to claim it round-tripped. A duplicate key is `INVALID` rather than
merged, since either resolution silently discards a value. Limits: key ≤ 4 KiB,
value ≤ 1 MiB, 8192 variables, 4 MiB of key and value bytes combined; exceeding
any of them answers `TOO_LARGE` with no entries.

Every outcome is still one reply under the caller's nonce. A client waiting on
`S2C_ENV` is never left hanging, whatever went wrong.

## Security

**This hands the caller every credential the server was started with.**

That is the whole posture, stated plainly. If the server's environment holds a
`GITHUB_TOKEN`, an `ANTHROPIC_API_KEY`, or any other secret, then any client that
can reach this family reads it. There is no allowlist and no redaction.

Two things bound it, neither of which should be mistaken for a sandbox:

- The ceiling is the one the protocol already has. A client that can call
  `ENV_GET` can also open a PTY, and a shell prints nearly the same environment.
  This family does not widen who can read what; it removes the need to spawn a
  process to do it. The one difference is `BLIT_*`, which `pty/pty_unix.rs`
  strips from a child (`BLIT_HUB` excepted) and this family does not: those are
  the server's own knobs — budgets, gates, the socket a caller is already
  talking on — and no credential of the deployment is among them.
- **`BLIT_PASSPHRASE` is not one of them.** It belongs to whoever authenticates
  browsers — the gateway process, or `blit share` — and no server reads it. It
  is not in a server's environment to hand over, and it is kept that way on
  purpose: the CLI's autostart (`transport.rs`) removes it from the child's
  environment rather than trusting that the parent had no reason to hold it,
  because `blit share` reads it one line before autostarting a server.
- **`BLIT_ENV=0` refuses the family at dispatch** with `PERMISSION` and no
  entries. The feature bit stays advertised, following [kv.md](kv.md)'s
  precedent, so a refusal is legible as an operator's decision rather than as an
  old server, and a client can report the difference.

The asymmetry worth understanding is _who_ is reading. A PTY child prints the
environment because a person typed a command; an extension reads it unattended,
at session start, from code the operator installed once. Installing a persistent
extension is opting in to _running_ that code across restarts, not necessarily
to handing it their credentials. An operator who wants the session-shaped values without the
secrets should set `BLIT_ENV=0` and rely on
[processes.md](processes.md)'s `PROCESS_SPAWN_SESSION_ENV`, which applies the
session environment to a child **server-side** without ever naming it on the
wire.

## Non-goals

- **No writes.** Nothing sets a server variable. The server's environment is
  fixed at exec, and a family that mutated it would race every reader and every
  in-flight spawn.
- **No watch.** There is no subscription: the value cannot change under a
  running server, so a snapshot is complete by construction.
- **No per-client view.** Every caller sees the same environment. Scoping would
  imply an identity model the protocol does not have.
