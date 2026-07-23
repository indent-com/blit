# uplink — exposing a blit server through a relay

`blit uplink <control-url>` makes the local blit server reachable from
outside NAT by holding an outbound WebTransport session to a relay and
bridging relay-initiated streams to the local server socket. This document
specifies the protocol between the uplink and its control endpoint and
relay. It deliberately leaves the relay side abstract: any off-the-shelf
WebTransport server that opens one bidirectional stream per consumer can
act as a relay. [upsidedown.md](upsidedown.md) is the hosted, multi-tenant
implementation.

## Roles

| Role             | Meaning                                                                 |
| ---------------- | ----------------------------------------------------------------------- |
| uplink           | `blit uplink` — connects out, bridges streams to the local blit server  |
| control endpoint | HTTPS URL that authenticates the uplink and allocates it a relay        |
| relay            | WebTransport server the uplink stays connected to                       |
| consumer         | A blit client reaching the server through the relay (see upsidedown.md) |

## Control endpoint

The uplink authenticates to the control endpoint with the
`BLIT_UPLINK_TOKEN` environment variable:

```
GET <control-url>
Authorization: Bearer <BLIT_UPLINK_TOKEN>
Accept: application/json
```

A success response is the **relay pool**:

```json
{ "relays": ["https://relay-1.example.com:4443/t/kfV3aB#sha256=<base64url>"] }
```

- `relays` is a non-empty array of `https` URLs. Any other scheme is an
  error.
- **A relay URL is a credential.** Whatever authenticates the uplink to
  the relay (a token in the path, a capability URL) is embedded in it.
  Implementations MUST NOT log relay URLs; log `host:port` instead.
- An optional URL fragment `#sha256=<base64url SHA-256>` pins the relay's
  TLS certificate (32 bytes, DER hash of the end-entity certificate).
  With a pin, chain and expiry are not checked — the hash is the trust
  anchor, exactly like the browser's `serverCertificateHashes`. Without
  one, system roots verify as usual. A malformed pin is an error, never a
  silent fall-back to system roots. The fragment is client-side only and
  is stripped before connecting.
- Unknown fields in the response are ignored.

Error handling:

- `401`/`403` — the token is bad; fatal, the uplink exits.
- Any other failure (unreachable, non-2xx, malformed body) — retried
  with exponential backoff, 1s doubling to a 60s cap, with 0.75×–1.25×
  jitter. A `Retry-After` header (seconds) overrides the backoff delay.

## Relay session

The uplink shuffles the pool and tries each relay in order: a
WebTransport (HTTP/3 CONNECT) session to the relay URL. Liveness settings
are a **10s keepalive** and a **30s idle timeout**, so a dead relay is
noticed within 30 seconds without any application-level pings.

The uplink never opens streams. The relay opens **one bidirectional
stream per consumer**; the uplink bridges each to a fresh connection to
the local blit server socket. Stream payload is exactly the bytes of a
local client connection — the blit wire protocol
([protocol.md](protocol.md)) with its length-prefixed frames — unparsed
and un-reframed in both directions. FIN propagates: a stream FIN shuts
down the socket's write side, socket EOF finishes the stream.

### Stream error codes

The uplink resets streams with these application error codes:

| Code | Meaning                                            |
| ---- | -------------------------------------------------- |
| 1    | local blit server unavailable (connect failed)     |
| 2    | uplink shutting down (also the session close code) |
| 3    | local I/O error after the bridge was established   |

A relay should close the consumer's connection when it sees any of them.

## Reconnection

- A relay that never accepted the session (handshake or CONNECT failed):
  try the next relay in the shuffled pool.
- A session that was actually established and later died: re-query the
  control endpoint immediately for a fresh pool — allocation is
  re-balanced on every reconnect — and reset the backoff.
- Pool exhausted with no session established: back off (same schedule as
  the control endpoint) and re-query.
- On SIGINT the uplink closes the active session with code 2 instead of
  letting it idle out on the relay.

Relay URLs stay valid for as long as their embedded credential does; the
uplink treats each pool response as single-use and re-queries rather than
caching it.
