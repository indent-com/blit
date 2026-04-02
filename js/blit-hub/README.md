# blit-hub

WebRTC signaling relay for blit terminal sharing. Routes WebRTC signaling
messages (offers, answers, ICE candidates) between peers over WebSocket.

Channels are identified by ed25519 public keys. The server verifies NaCl
`crypto_sign` envelopes against the channel's public key before relaying —
anyone who holds the corresponding signing key can participate, without any
server-side authentication or user accounts.

## Deploy

Install [flyctl](https://fly.io/docs/flyctl/install/), provision Redis, then
run the setup script:

```bash
# Create a Redis instance (interactive — flyctl will prompt for options)
flyctl redis create

# Deploy (REDIS_URL is required on first run)
REDIS_URL=redis://... ./bin/setup-blit-hub
```

The script is idempotent — re-running it skips secrets that are already set
and redeploys.

Optional env vars for setup:

| Variable            | Description                               |
| ------------------- | ----------------------------------------- |
| `REDIS_URL`         | Redis connection URL (required first run) |
| `CF_TURN_TOKEN_ID`  | Cloudflare TURN key ID                    |
| `CF_TURN_API_TOKEN` | Cloudflare TURN API token                 |
| `FLY_ORG`           | Fly.io org (default: `personal`)          |

To enable continuous deployment from GitHub Actions:

```bash
flyctl tokens create deploy -a blit-hub
gh secret set FLY_API_TOKEN --repo <owner>/<repo>
```

## Running locally

```bash
docker run -d -p 6379:6379 redis:7
cd js/blit-hub
bun install
bun run dev
```

## Protocol

```
wss://hub.blit.sh/channel/<64-char-hex-pubkey>/<producer|consumer>
```

On connect, the server assigns a session ID and sends presence notifications:

```jsonc
<- {"type":"registered","channelId":"...","role":"producer","sessionId":"..."}
<- {"type":"peer_joined","role":"consumer","sessionId":"abc-123"}   // for each existing peer
```

All signaling messages are NaCl-signed and addressed to a specific session:

```jsonc
-> {"signed":"<base64 crypto_sign(json_payload)>","target":"abc-123"}
<- {"type":"signal","from":"def-456","data":<verified json_payload>}
```

Peers receive `peer_left` when the other side disconnects.

## ICE server configuration

`GET /ice` returns STUN/TURN server configuration for clients to use when
establishing WebRTC peer connections.

By default it returns Google's public STUN servers. If `CF_TURN_TOKEN_ID` and
`CF_TURN_API_TOKEN` are set, it fetches short-lived TURN credentials from
[Cloudflare Calls](https://developers.cloudflare.com/calls/turn/) instead:

```jsonc
// Without Cloudflare TURN
{"iceServers": [{"urls": "stun:stun.l.google.com:19302"}, ...]}

// With Cloudflare TURN
{"iceServers": [{"urls": "turn:...", "username": "...", "credential": "..."}, ...]}
```

## Configuration

| Variable            | Default                  | Description                         |
| ------------------- | ------------------------ | ----------------------------------- |
| `PORT`              | `8000`                   | HTTP/WebSocket port                 |
| `REDIS_URL`         | `redis://localhost:6379` | Redis connection URL                |
| `CF_TURN_TOKEN_ID`  | _(unset)_                | Cloudflare TURN key ID              |
| `CF_TURN_API_TOKEN` | _(unset)_                | Cloudflare TURN API bearer token    |
| `MESSAGE_TEMPLATE`  | _(see below)_            | Template returned by `GET /message` |

## Message template

`GET /message` returns a message template that clients can display when a
terminal session is shared:

```jsonc
{
  "template": "Session available at https://blit.sh/s#{secret}\nor blit --share {secret}",
}
```

The `{secret}` placeholder is intended to be replaced client-side with the
actual session secret. Override the default via the `MESSAGE_TEMPLATE` env var.

## Architecture

- **Bun** runtime with `Bun.serve()` for HTTP/WebSocket
- **Redis** for cross-instance message relay (pub/sub) and session tracking
  (sets with TTL)
- **tweetnacl** for ed25519 signature verification
- Stateless — all session state lives in Redis, so instances can scale
  horizontally
