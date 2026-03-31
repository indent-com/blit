import type { ServerWebSocket } from "bun";
import { Redis } from "ioredis";
import nacl from "tweetnacl";

const PORT = parseInt(process.env.PORT || "8000", 10);
const REDIS_URL = process.env.REDIS_URL || "redis://localhost:6379";
const SESSION_TTL = 600;

const redis = new Redis(REDIS_URL, { maxRetriesPerRequest: 3 });
const pubRedis = new Redis(REDIS_URL, { maxRetriesPerRequest: 3 });
const subRedis = new Redis(REDIS_URL, { maxRetriesPerRequest: 3 });

type Role = "producer" | "consumer";

type ClientData = {
  channelId: string;
  role: Role;
  sessionId: string;
};

type Channel = {
  producers: Map<string, ServerWebSocket<ClientData>>;
  consumers: Map<string, ServerWebSocket<ClientData>>;
};

const channels = new Map<string, Channel>();
const subCounts = new Map<string, number>();

function getOrCreateChannel(channelId: string): Channel {
  let ch = channels.get(channelId);
  if (!ch) {
    ch = { producers: new Map(), consumers: new Map() };
    channels.set(channelId, ch);
  }
  return ch;
}

function redisKey(prefix: string, ...parts: string[]): string {
  return `rendezvous:${prefix}:${parts.join(":")}`;
}

function toSessionTopic(channelId: string, sessionId: string): string {
  return redisKey("to_session", channelId, sessionId);
}

function hexToBytes(hex: string): Uint8Array {
  const bytes = new Uint8Array(hex.length / 2);
  for (let i = 0; i < hex.length; i += 2) {
    bytes[i / 2] = parseInt(hex.substring(i, i + 2), 16);
  }
  return bytes;
}

function verifySignedMessage(
  signedBase64: string,
  publicKeyHex: string,
): Uint8Array | null {
  try {
    const signed = Uint8Array.from(atob(signedBase64), (c) => c.charCodeAt(0));
    const pk = hexToBytes(publicKeyHex);
    return nacl.sign.open(signed, pk);
  } catch {
    return null;
  }
}

async function subscribe(topic: string) {
  const count = (subCounts.get(topic) || 0) + 1;
  subCounts.set(topic, count);
  if (count === 1) {
    await subRedis.subscribe(topic);
  }
}

async function unsubscribe(topic: string) {
  const count = (subCounts.get(topic) || 1) - 1;
  subCounts.set(topic, count);
  if (count <= 0) {
    subCounts.delete(topic);
    await subRedis.unsubscribe(topic);
  }
}

subRedis.on("message", (_topic: string, message: string) => {
  try {
    const envelope = JSON.parse(message);
    const { channelId, targetSessionId, payload } = envelope;
    const ch = channels.get(channelId);
    if (!ch) {
      return;
    }

    const target =
      ch.producers.get(targetSessionId) || ch.consumers.get(targetSessionId);
    if (target) {
      target.send(payload);
    }
  } catch {
    // malformed redis message
  }
});

function relayToSession(channelId: string, sessionId: string, payload: string) {
  const envelope = JSON.stringify({
    channelId,
    targetSessionId: sessionId,
    payload,
  });
  pubRedis.publish(toSessionTopic(channelId, sessionId), envelope);
}

function isValidChannelId(channelId: string): boolean {
  return /^[0-9a-f]{64}$/i.test(channelId);
}

const server = Bun.serve<ClientData>({
  port: PORT,

  async fetch(req) {
    const url = new URL(req.url);

    if (url.pathname === "/health") {
      try {
        await redis.ping();
        return new Response("ok", { status: 200 });
      } catch {
        return new Response("redis unreachable", { status: 503 });
      }
    }

    const match = url.pathname.match(
      /^\/channel\/([0-9a-fA-F]{64})\/(producer|consumer)$/,
    );
    if (!match) {
      return new Response("Not Found", { status: 404 });
    }

    const channelId = match[1].toLowerCase();
    const role = match[2] as Role;

    if (!isValidChannelId(channelId)) {
      return new Response("Invalid channel ID", { status: 400 });
    }

    const sessionId = crypto.randomUUID();
    const upgraded = server.upgrade(req, {
      data: { channelId, role, sessionId },
    });
    if (!upgraded) {
      return new Response("WebSocket upgrade failed", { status: 400 });
    }
    return undefined as unknown as Response;
  },

  websocket: {
    async open(ws) {
      const { channelId, role, sessionId } = ws.data;
      const ch = getOrCreateChannel(channelId);
      const peers = role === "producer" ? ch.producers : ch.consumers;
      const others = role === "producer" ? ch.consumers : ch.producers;

      peers.set(sessionId, ws);
      await subscribe(toSessionTopic(channelId, sessionId));
      await redis.sadd(redisKey(role, channelId), sessionId);
      await redis.expire(redisKey(role, channelId), SESSION_TTL);

      ws.send(
        JSON.stringify({ type: "registered", channelId, role, sessionId }),
      );

      for (const [peerId] of others) {
        ws.send(
          JSON.stringify({
            type: "peer_joined",
            role: role === "producer" ? "consumer" : "producer",
            sessionId: peerId,
          }),
        );
      }

      for (const [, other] of others) {
        other.send(JSON.stringify({ type: "peer_joined", role, sessionId }));
      }
    },

    async message(ws, raw) {
      const { channelId } = ws.data;
      const text =
        typeof raw === "string" ? raw : new TextDecoder().decode(raw);

      let outer: { signed: string; target?: string };
      try {
        outer = JSON.parse(text);
      } catch {
        ws.send(JSON.stringify({ type: "error", message: "invalid json" }));
        return;
      }

      if (!outer.signed) {
        ws.send(
          JSON.stringify({ type: "error", message: "missing signed field" }),
        );
        return;
      }

      const opened = verifySignedMessage(outer.signed, channelId);
      if (!opened) {
        ws.send(
          JSON.stringify({
            type: "error",
            message: "signature verification failed",
          }),
        );
        return;
      }

      if (!outer.target) {
        ws.send(JSON.stringify({ type: "error", message: "missing target" }));
        return;
      }

      const innerText = new TextDecoder().decode(opened);
      let innerData: unknown;
      try {
        innerData = JSON.parse(innerText);
      } catch {
        ws.send(
          JSON.stringify({
            type: "error",
            message: "signed payload is not valid json",
          }),
        );
        return;
      }

      relayToSession(
        channelId,
        outer.target,
        JSON.stringify({
          type: "signal",
          from: ws.data.sessionId,
          data: innerData,
        }),
      );
    },

    async close(ws) {
      const { channelId, role, sessionId } = ws.data;
      const ch = channels.get(channelId);
      if (!ch) {
        return;
      }

      const peers = role === "producer" ? ch.producers : ch.consumers;
      const others = role === "producer" ? ch.consumers : ch.producers;

      peers.delete(sessionId);
      await unsubscribe(toSessionTopic(channelId, sessionId));
      await redis.srem(redisKey(role, channelId), sessionId);

      for (const [, other] of others) {
        other.send(JSON.stringify({ type: "peer_left", role, sessionId }));
      }

      if (ch.producers.size === 0 && ch.consumers.size === 0) {
        channels.delete(channelId);
      }
    },
  },
});

async function shutdown() {
  Bun.write(Bun.stdout, "Shutting down...\n");
  server.stop();
  for (const [, ch] of channels) {
    for (const [, ws] of ch.producers) {
      ws.close(1001, "server shutting down");
    }
    for (const [, ws] of ch.consumers) {
      ws.close(1001, "server shutting down");
    }
  }
  channels.clear();
  redis.disconnect();
  pubRedis.disconnect();
  subRedis.disconnect();
}

process.on("SIGTERM", async () => {
  await shutdown();
  process.exit(0);
});
process.on("SIGINT", async () => {
  await shutdown();
  process.exit(0);
});

Bun.write(Bun.stdout, `Rendezvous service listening on port ${PORT}\n`);
