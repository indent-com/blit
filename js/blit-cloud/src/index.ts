import type { ServerWebSocket } from "bun";
import { Redis } from "ioredis";
import nacl from "tweetnacl";

const PORT = parseInt(process.env.PORT || "8000", 10);
const REDIS_URL = process.env.REDIS_URL || "redis://localhost:6379";
const CF_TURN_TOKEN_ID = process.env.CF_TURN_TOKEN_ID;
const CF_TURN_API_TOKEN = process.env.CF_TURN_API_TOKEN;
const MESSAGE_TEMPLATE =
  process.env.MESSAGE_TEMPLATE ||
  "Welcome to blitz! Terminals are now available at https://blitz.sh/#{secret}";
const ICE_TTL = 86400;
const SESSION_TTL = 600;
const MAX_PAYLOAD_BYTES = 65536;

const DEFAULT_ICE_SERVERS = [
  { urls: "stun:stun.l.google.com:19302" },
  { urls: "stun:stun1.l.google.com:19302" },
];

let cachedIce: { data: unknown; expiresAt: number } | null = null;
const ICE_CACHE_TTL = ICE_TTL / 2;

async function getIceServers() {
  if (!CF_TURN_TOKEN_ID || !CF_TURN_API_TOKEN) {
    return { iceServers: DEFAULT_ICE_SERVERS };
  }

  if (cachedIce && Date.now() < cachedIce.expiresAt) {
    return cachedIce.data;
  }

  const res = await fetch(
    `https://rtc.live.cloudflare.com/v1/turn/keys/${CF_TURN_TOKEN_ID}/credentials/generate-ice-servers`,
    {
      method: "POST",
      headers: {
        Authorization: `Bearer ${CF_TURN_API_TOKEN}`,
        "Content-Type": "application/json",
      },
      body: JSON.stringify({ ttl: ICE_TTL }),
    },
  );

  if (!res.ok) {
    throw new Error(`Cloudflare TURN API returned ${res.status}`);
  }

  const data = await res.json();
  cachedIce = { data, expiresAt: Date.now() + ICE_CACHE_TTL * 1000 };
  return data;
}

const redis = new Redis(REDIS_URL, { maxRetriesPerRequest: 3 });
const pubRedis = new Redis(REDIS_URL, { maxRetriesPerRequest: 3 });
const subRedis = new Redis(REDIS_URL, { maxRetriesPerRequest: 3 });

type Role = "producer" | "consumer";
const OPPOSITE_ROLE: Record<Role, Role> = {
  producer: "consumer",
  consumer: "producer",
};

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
  return `blitz:${prefix}:${parts.join(":")}`;
}

function channelPresenceTopic(channelId: string): string {
  return redisKey("presence", channelId);
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

function broadcastToLocalPeers(
  channelId: string,
  excludeSessionId: string,
  message: string,
) {
  const ch = channels.get(channelId);
  if (!ch) return;
  for (const map of [ch.producers, ch.consumers]) {
    for (const [sid, ws] of map) {
      if (sid !== excludeSessionId) ws.send(message);
    }
  }
}

subRedis.on("message", (topic: string, message: string) => {
  try {
    const envelope = JSON.parse(message);

    if (topic.startsWith("blitz:presence:")) {
      const channelId = topic.slice("blitz:presence:".length);
      broadcastToLocalPeers(channelId, envelope.sessionId, message);
      return;
    }

    const { channelId, targetSessionId, payload } = envelope;
    const ch = channels.get(channelId);
    if (!ch) return;

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

function publishPresence(
  channelId: string,
  type: "peer_joined" | "peer_left",
  role: Role,
  sessionId: string,
) {
  const msg = JSON.stringify({ type, role, sessionId });
  pubRedis.publish(channelPresenceTopic(channelId), msg);
}

const server = Bun.serve<ClientData>({
  port: PORT,

  async fetch(req) {
    const url = new URL(req.url);
    const cors = { "Access-Control-Allow-Origin": "*" };

    if (req.method === "OPTIONS") {
      return new Response(null, {
        status: 204,
        headers: {
          ...cors,
          "Access-Control-Allow-Methods": "GET, OPTIONS",
          "Access-Control-Allow-Headers": "Content-Type",
        },
      });
    }

    if (url.pathname === "/health") {
      try {
        await redis.ping();
        return new Response("ok", { status: 200, headers: cors });
      } catch {
        return new Response("redis unreachable", { status: 503, headers: cors });
      }
    }

    if (url.pathname === "/message") {
      return Response.json({ template: MESSAGE_TEMPLATE }, { headers: cors });
    }

    if (url.pathname === "/ice") {
      try {
        const config = await getIceServers();
        return Response.json(config, { headers: cors });
      } catch {
        return Response.json({ iceServers: DEFAULT_ICE_SERVERS }, { headers: cors });
      }
    }

    const match = url.pathname.match(
      /^\/channel\/([0-9a-fA-F]{64})\/(producer|consumer)$/,
    );
    if (!match) {
      return new Response("Not Found", { status: 404, headers: cors });
    }

    const channelId = match[1].toLowerCase();
    const role = match[2] as Role;

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
    maxPayloadLength: MAX_PAYLOAD_BYTES,

    async open(ws) {
      const { channelId, role, sessionId } = ws.data;
      const ch = getOrCreateChannel(channelId);
      const peers = role === "producer" ? ch.producers : ch.consumers;

      peers.set(sessionId, ws);
      await subscribe(toSessionTopic(channelId, sessionId));
      await subscribe(channelPresenceTopic(channelId));

      const memberKey = redisKey(role, channelId);
      await redis.sadd(memberKey, sessionId);
      await redis.expire(memberKey, SESSION_TTL);

      ws.send(
        JSON.stringify({ type: "registered", channelId, role, sessionId }),
      );

      const otherRole = OPPOSITE_ROLE[role];
      const remoteMembers = await redis.smembers(redisKey(otherRole, channelId));
      for (const peerId of remoteMembers) {
        ws.send(
          JSON.stringify({
            type: "peer_joined",
            role: otherRole,
            sessionId: peerId,
          }),
        );
      }

      publishPresence(channelId, "peer_joined", role, sessionId);
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
      if (!ch) return;

      const peers = role === "producer" ? ch.producers : ch.consumers;

      peers.delete(sessionId);
      await unsubscribe(toSessionTopic(channelId, sessionId));
      await redis.srem(redisKey(role, channelId), sessionId);

      publishPresence(channelId, "peer_left", role, sessionId);

      if (ch.producers.size === 0 && ch.consumers.size === 0) {
        await unsubscribe(channelPresenceTopic(channelId));
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

Bun.write(Bun.stdout, `Blitz signaling service listening on port ${PORT}\n`);
