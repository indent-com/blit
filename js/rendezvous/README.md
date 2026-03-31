# blit-rendezvous

WebRTC signaling relay for blit terminal sharing. Routes WebRTC signaling
messages (offers, answers, ICE candidates) between peers over WebSocket.

Channels are identified by ed25519 public keys. The server verifies NaCl
`crypto_sign` envelopes against the channel's public key before relaying —
anyone who holds the corresponding signing key can participate, without any
server-side authentication or user accounts.

## Protocol

```
ws://<host>/channel/<64-char-hex-pubkey>/<producer|consumer>
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

## Running locally

```bash
# Start Redis
docker run -d -p 6379:6379 redis:7

# Start the service
cd js/rendezvous
bun install
bun run dev
```

The service listens on port 8000 by default (`PORT` env var). Set `REDIS_URL`
to point at a Redis instance.

## Configuration

| Variable    | Default                  | Description            |
| ----------- | ------------------------ | ---------------------- |
| `PORT`      | `8000`                   | HTTP/WebSocket port    |
| `REDIS_URL` | `redis://localhost:6379` | Redis connection URL   |

## Deployment

The Dockerfile builds a minimal image suitable for any container platform:

```bash
cd js/rendezvous
docker build -t blit-rendezvous .
docker run -p 8000:8000 -e REDIS_URL=redis://your-redis:6379 blit-rendezvous
```

### Fly.io (easiest)

[Fly.io](https://fly.io) can deploy the Dockerfile directly with built-in
Redis via Upstash:

```bash
cd js/rendezvous

# Launch the app (creates fly.toml automatically)
fly launch --no-deploy

# Provision Redis
fly redis create --name blit-rendezvous-redis

# Set the connection string (printed by the previous command)
fly secrets set REDIS_URL=redis://...

# Deploy
fly deploy
```

### Other platforms

Any platform that runs Docker containers works. You need:

1. A Redis instance (managed Redis from AWS ElastiCache, GCP Memorystore,
   Railway, Upstash, etc.)
2. A container runtime with the `REDIS_URL` env var set
3. A health check pointing at `GET /health` (returns 200 when Redis is
   reachable, 503 otherwise)

For horizontal scaling, multiple instances share state through Redis pub/sub —
no sticky sessions required.

## Architecture

- **Bun** runtime with `Bun.serve()` for HTTP/WebSocket
- **Redis** for cross-instance message relay (pub/sub) and session tracking
  (sets with TTL)
- **tweetnacl** for ed25519 signature verification
- Stateless — all session state lives in Redis, so instances can scale
  horizontally
