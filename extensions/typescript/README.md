# TypeScript extension support

These are source libraries, not runtime modules. A TypeScript extension imports
them normally and the build publishes one bundled ECMAScript module. QuickJS
has no package resolver and does not transpile TypeScript.

- [`blit.ts`](blit.ts) types the frozen `blit.context` and native host calls,
  and supplies UTF-8 and little-endian codecs without browser globals.
- [`command.ts`](command.ts) registers and serves a small synchronous
  `blit.cli.v1` command provider.

The command helper is intentionally for bounded diagnostic and configuration
responses that fit the native channel's one-MiB credit window. An extension
that streams output, multiplexes channels, or supervises other activity should
own its receive loop and flow-control state, as the larger Rust extensions do.

[`@doctor`](../doctor) is the complete example: a typed report, a human
renderer, a structured JSON result, protocol tests, and a default export that
stays small.
