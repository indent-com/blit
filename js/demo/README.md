# blit-demo

Ephemeral demo sandbox provisioning via [Modal](https://modal.com/). Spins up
short-lived containers running `blit share` so visitors can try blit without
installing anything.

## Running locally

```bash
docker run -d -p 6379:6379 redis:7
cd js/demo
bun install
MODAL_TOKEN_ID=... MODAL_TOKEN_SECRET=... bun run dev
```

## API

`POST /demo` provisions a sandbox or returns 204 if one already exists for the
given nonce.

```jsonc
// Request
POST /demo
{"nonce": "my-secret-passphrase"}

// Response: 204 No Content (sandbox created or already running)
```

The nonce doubles as the `blit share` passphrase and the idempotency key.
Sandboxes auto-terminate after 1 minute.

Rate limited to 5 sandboxes per IP per 60 seconds.

## Configuration

| Variable             | Default                  | Description            |
| -------------------- | ------------------------ | ---------------------- |
| `PORT`               | `8001`                   | HTTP port              |
| `REDIS_URL`          | `redis://localhost:6379` | Redis connection URL   |
| `MODAL_TOKEN_ID`     | _(required)_             | Modal API token ID     |
| `MODAL_TOKEN_SECRET` | _(required)_             | Modal API token secret |
