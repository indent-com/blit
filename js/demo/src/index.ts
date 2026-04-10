import { Redis } from "ioredis";
import { handleDemoRequest } from "./demo";

const PORT = parseInt(process.env.PORT || "8001", 10);
const REDIS_URL = process.env.REDIS_URL || "redis://localhost:6379";

const redis = new Redis(REDIS_URL, { maxRetriesPerRequest: 3 });

const server = Bun.serve({
  port: PORT,

  async fetch(req) {
    const url = new URL(req.url);
    const cors = { "Access-Control-Allow-Origin": "*" };

    if (req.method === "OPTIONS") {
      return new Response(null, {
        status: 204,
        headers: {
          ...cors,
          "Access-Control-Allow-Methods": "GET, POST, OPTIONS",
          "Access-Control-Allow-Headers": "Content-Type",
        },
      });
    }

    if (url.pathname === "/demo" && req.method === "POST") {
      let body: unknown;
      try {
        body = await req.json();
      } catch {
        return Response.json(
          { error: "invalid json" },
          { status: 400, headers: cors },
        );
      }
      const clientIp =
        req.headers.get("fly-client-ip") ||
        req.headers.get("x-forwarded-for")?.split(",")[0]?.trim() ||
        server.requestIP(req)?.address ||
        "unknown";
      return handleDemoRequest(redis, body as { nonce?: string }, clientIp);
    }

    if (url.pathname === "/health") {
      try {
        await redis.ping();
        return new Response("ok", { status: 200, headers: cors });
      } catch {
        return new Response("redis unreachable", {
          status: 503,
          headers: cors,
        });
      }
    }

    return new Response("Not Found", { status: 404, headers: cors });
  },
});

async function shutdown() {
  Bun.write(Bun.stdout, "Shutting down...\n");
  server.stop();
  redis.disconnect();
}

process.on("SIGTERM", async () => {
  await shutdown();
  process.exit(0);
});
process.on("SIGINT", async () => {
  await shutdown();
  process.exit(0);
});

Bun.write(Bun.stdout, `blit-demo listening on port ${PORT}\n`);
