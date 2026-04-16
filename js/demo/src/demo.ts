import type { Redis } from "ioredis";
import { ModalClient } from "modal";

const SANDBOX_TIMEOUT_MS = 5 * 60 * 1000;
const REDIS_TTL_SECONDS = 30 * 60;
const MAX_SANDBOXES_PER_IP = 5;
const RATE_LIMIT_WINDOW_SECONDS = 60;

const REDIS_NONCE_PREFIX = "blit_demo:nonce";
const REDIS_SANDBOX_PREFIX = "blit_demo:sandbox";
const REDIS_RATE_LIMIT_PREFIX = "blit_demo:rate";
const REDIS_LOCK_PREFIX = "blit_demo:lock";

const DEMO_APP_NAME = "blit-demo";

// Pre-built Modal image containing blit, opencode, htop, mpv, git, and the parrot gif.
// Rebuild with: uv run python js/demo/scripts/build-image.py
const DEMO_IMAGE_ID = "im-VydV0eeimTFG2pXKhnQWB1";

function isAllowedOrigin(origin: string): boolean {
  try {
    const url = new URL(origin);
    if (url.hostname === "localhost" || url.hostname === "127.0.0.1")
      return true;
    if (url.hostname === "indent.com" || url.hostname.endsWith(".indent.com"))
      return true;
    if (url.hostname === "blit.sh" || url.hostname.endsWith(".blit.sh"))
      return true;
    return false;
  } catch {
    return false;
  }
}

export function corsHeaders(origin: string | null): Record<string, string> {
  if (origin && isAllowedOrigin(origin)) {
    return { "Access-Control-Allow-Origin": origin, Vary: "Origin" };
  }
  return {};
}

type SandboxInfo = {
  sandbox_id: string;
  nonce: string;
};

function rateLimitKey(clientIp: string): string {
  if (clientIp.includes(":")) {
    const parts = clientIp.split(":");
    const prefix = parts.slice(0, 4).join(":");
    return `${REDIS_RATE_LIMIT_PREFIX}:${prefix}::/64`;
  }
  return `${REDIS_RATE_LIMIT_PREFIX}:${clientIp}`;
}

async function acquireRateLimit(
  redis: Redis,
  clientIp: string,
): Promise<boolean> {
  const key = rateLimitKey(clientIp);
  const count = (await redis.eval(
    "local c = redis.call('INCR', KEYS[1]); if c == 1 then redis.call('EXPIRE', KEYS[1], ARGV[1]) end; return c",
    1,
    key,
    RATE_LIMIT_WINDOW_SECONDS,
  )) as number;
  if (count > MAX_SANDBOXES_PER_IP) {
    await redis.decr(key);
    return false;
  }
  return true;
}

async function releaseRateLimit(redis: Redis, clientIp: string): Promise<void> {
  const key = rateLimitKey(clientIp);
  const exists = await redis.exists(key);
  if (exists) {
    await redis.decr(key);
  }
}

async function storeSandbox(redis: Redis, info: SandboxInfo): Promise<void> {
  const value = JSON.stringify(info);
  await redis
    .pipeline()
    .set(
      `${REDIS_SANDBOX_PREFIX}:${info.sandbox_id}`,
      value,
      "EX",
      REDIS_TTL_SECONDS,
    )
    .set(`${REDIS_NONCE_PREFIX}:${info.nonce}`, value, "EX", REDIS_TTL_SECONDS)
    .exec();
}

async function getSandboxByNonce(
  redis: Redis,
  nonce: string,
): Promise<SandboxInfo | null> {
  const value = await redis.get(`${REDIS_NONCE_PREFIX}:${nonce}`);
  if (!value) return null;
  return JSON.parse(value) as SandboxInfo;
}

async function removeSandbox(redis: Redis, info: SandboxInfo): Promise<void> {
  await redis
    .pipeline()
    .del(`${REDIS_NONCE_PREFIX}:${info.nonce}`)
    .del(`${REDIS_SANDBOX_PREFIX}:${info.sandbox_id}`)
    .exec();
}

export async function handleDemoRequest(
  redis: Redis,
  body: { nonce?: string },
  clientIp: string,
  origin: string | null,
): Promise<Response> {
  const cors = corsHeaders(origin);

  if (
    !body.nonce ||
    typeof body.nonce !== "string" ||
    body.nonce.length > 256
  ) {
    return Response.json(
      { error: "missing or invalid nonce" },
      { status: 400, headers: cors },
    );
  }

  const nonce = body.nonce;

  if (!(await acquireRateLimit(redis, clientIp))) {
    return Response.json(
      { error: "too many demo instances requested" },
      { status: 429, headers: cors },
    );
  }

  try {
    const cached = await getSandboxByNonce(redis, nonce);
    if (cached) {
      try {
        const modal = new ModalClient();
        const sb = await modal.sandboxes.fromId(cached.sandbox_id);
        const exitCode = await sb.poll();
        if (exitCode === null) {
          await releaseRateLimit(redis, clientIp);
          return new Response(null, { status: 204, headers: cors });
        }
      } catch {
        // sandbox gone
      }
      await removeSandbox(redis, cached);
    }

    const lockKey = `${REDIS_LOCK_PREFIX}:${nonce}`;
    const acquired = await redis.set(lockKey, "1", "EX", 60, "NX");
    if (!acquired) {
      await releaseRateLimit(redis, clientIp);
      return Response.json(
        { error: "sandbox creation in progress" },
        { status: 409, headers: cors },
      );
    }

    try {
      const modal = new ModalClient();
      const app = await modal.apps.fromName(DEMO_APP_NAME, {
        createIfMissing: true,
      });
      const image = await modal.images.fromId(DEMO_IMAGE_ID);

      const startupScript = [
        'BLIT_PASSPHRASE="$BLIT_NONCE" blit share &',
        "sleep 2",
        "blit terminal start -t bash --cols 280 --rows 60 -- bash",
        "blit terminal start -t parrot --cols 280 --rows 60 -- mpv --vo=tct --no-osd-bar --osd-level=0 --no-terminal --loop=inf /home/blit/parrot.gif",
        "wait",
      ].join("\n");

      const sb = await modal.sandboxes.create(app, image, {
        command: ["/bin/bash", "-c", startupScript],
        timeoutMs: SANDBOX_TIMEOUT_MS,
        cpu: 0.5,
        memoryMiB: 1024,
        workdir: "/home/blit",
        env: {
          BLIT_NONCE: nonce,
          SHELL: "/bin/bash",
          COLORFGBG: "15;0",
        },
      });

      await sb.setTags({ client_ip: clientIp });

      const info: SandboxInfo = { sandbox_id: sb.sandboxId, nonce };
      await storeSandbox(redis, info);
      await redis.del(lockKey);

      return new Response(null, { status: 204, headers: cors });
    } catch (e) {
      await redis.del(lockKey);
      throw e;
    }
  } catch (e) {
    await releaseRateLimit(redis, clientIp);
    const message = e instanceof Error ? e.message : "unknown error";
    console.error("Failed to create blit demo:", message);
    return Response.json(
      { error: "failed to create demo instance" },
      { status: 500, headers: cors },
    );
  }
}
