import type { ServerWebSocket } from "bun";
import { Redis } from "ioredis";
import nacl from "tweetnacl";

const PORT = parseInt(process.env.PORT || "8000", 10);
const REDIS_URL = process.env.REDIS_URL || "redis://localhost:6379";
const CF_TURN_TOKEN_ID = process.env.CF_TURN_TOKEN_ID;
const CF_TURN_API_TOKEN = process.env.CF_TURN_API_TOKEN;
const MESSAGE_TEMPLATE =
  process.env.MESSAGE_TEMPLATE ||
  "Terminals at https://blit.sh/s#psk={secret}\nRead-only: https://blit.sh/s#psk={ro_secret}";
const ICE_TTL = 86400;
const SESSION_TTL = 600;
const SESSION_REFRESH_INTERVAL = SESSION_TTL * 500; // refresh at half-TTL (ms)
const MAX_PAYLOAD_BYTES = 65536;
/** Close code for "this hub cannot reach redis". 1013 (Try Again Later) is
 *  the honest one: the peer should reconnect, and may land on a healthy
 *  machine — as opposed to holding a socket that will never be served. */
const CLOSE_REDIS_UNAVAILABLE = 1013;

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

// Redis is required, not advisory: presence and cross-machine relaying both
// live in it, so a hub that cannot reach it cannot do its job and must say
// so rather than accept sockets it will never be able to serve.
//
// `commandTimeout` is the load-bearing option. Fly's `suspend` resumes this
// process with its sockets' peers long gone; with no timeout, every awaited
// command hangs forever, and — because registration awaits redis before
// announcing peers — every producer and consumer parks on a socket that
// never says `peer_joined`. That was a real multi-hour outage: `registered`
// arrived, nothing followed, and the hub looked healthy from outside.
const COMMAND_TIMEOUT_MS = 2_000;
const REDIS_OPTS = {
  maxRetriesPerRequest: 3,
  commandTimeout: COMMAND_TIMEOUT_MS,
  // Detect a dead peer rather than trusting a socket that looks open.
  keepAlive: 10_000,
  enableOfflineQueue: false,
} as const;
const redis = new Redis(REDIS_URL, REDIS_OPTS);
const pubRedis = new Redis(REDIS_URL, REDIS_OPTS);
// The subscriber must keep its offline queue: ioredis replays subscriptions
// on reconnect, and dropping them would silence cross-machine delivery for
// every channel this instance holds.
const subRedis = new Redis(REDIS_URL, {
  ...REDIS_OPTS,
  enableOfflineQueue: true,
});

/** Whether each client believes it is usable, for /health. ioredis reports
 *  "ready" only after a successful handshake, so this tracks the real thing
 *  rather than the existence of an object. */
const clients = { main: redis, pub: pubRedis, sub: subRedis };
for (const [name, client] of Object.entries(clients)) {
  // An unhandled ioredis 'error' is a process-level crash in Bun, and a
  // silent one in logs — neither is acceptable for the dependency the whole
  // hub rests on.
  client.on("error", (err: Error) =>
    log(`redis(${name}) error: ${err.message}`),
  );
  client.on("end", () => log(`redis(${name}) connection ended`));
  client.on("reconnecting", () => log(`redis(${name}) reconnecting`));
  client.on("ready", () => log(`redis(${name}) ready`));
}

function log(line: string): void {
  Bun.write(Bun.stdout, `${new Date().toISOString()} ${line}\n`);
}

/** True when every client is in a state that can serve a request. */
function redisReady(): boolean {
  return Object.values(clients).every(
    (c) => c.status === "ready" || c.status === "connect",
  );
}

type Role = "producer" | "consumer";
const OPPOSITE_ROLE: Record<Role, Role> = {
  producer: "consumer",
  consumer: "producer",
};

type ClientData = {
  channelId: string;
  role: Role;
  sessionId: string;
  refreshTimer?: ReturnType<typeof setInterval>;
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
  return `blit:${prefix}:${parts.join(":")}`;
}

/** Per-session liveness key — exists only while the session is alive. */
function sessionLivenessKey(channelId: string, sessionId: string): string {
  return redisKey("alive", channelId, sessionId);
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

    if (topic.startsWith("blit:presence:")) {
      const channelId = topic.slice("blit:presence:".length);
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

/**
 * Sessions of `role` in `channelId` that are actually alive.
 *
 * A member set outlives the sockets in it: a forwarder that crashed, or a
 * WebSocket dropped without a clean close, leaves its id behind. Each
 * session also holds a short-TTL liveness key, so absence of that key is
 * the proof of death — and the stale ids are removed here so later peers do
 * not target them.
 */
async function livePeers(channelId: string, role: Role): Promise<string[]> {
  const memberKey = redisKey(role, channelId);
  const rawMembers = await redis.smembers(memberKey);
  if (rawMembers.length === 0) return [];
  const liveness = await redis.mget(
    ...rawMembers.map((sid) => sessionLivenessKey(channelId, sid)),
  );
  const live: string[] = [];
  const stale: string[] = [];
  rawMembers.forEach((sid, i) => (liveness[i] ? live : stale).push(sid));
  if (stale.length > 0) {
    await redis.srem(memberKey, ...stale);
  }
  return live;
}

async function relayToSession(
  channelId: string,
  sessionId: string,
  payload: string,
): Promise<void> {
  const envelope = JSON.stringify({
    channelId,
    targetSessionId: sessionId,
    payload,
  });
  // Awaited, not fired and forgotten: an SDP offer or ICE candidate that
  // silently fails to publish is a peer connection that never completes,
  // and the sender is the only party in a position to retry.
  await pubRedis.publish(toSessionTopic(channelId, sessionId), envelope);
}

function publishPresence(
  channelId: string,
  type: "peer_joined" | "peer_left",
  role: Role,
  sessionId: string,
): void {
  const msg = JSON.stringify({ type, role, sessionId });
  // Not awaited by its callers — a presence announcement that fails is a
  // missed `peer_joined` for peers on *other* machines, which their own
  // registration re-derives from the member set. Logged, never silent.
  pubRedis
    .publish(channelPresenceTopic(channelId), msg)
    .catch((err: Error) =>
      log(`presence ${type} ${sessionId}: ${err.message}`),
    );
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
      // Both halves matter: a PING proves the request path, and the
      // subscriber's state proves the delivery path. A hub whose subscriber
      // is wedged still answers PING while relaying nothing, which is the
      // shape of failure that kept traffic pointed at a dead machine.
      if (!redisReady()) {
        return new Response("redis not ready", { status: 503, headers: cors });
      }
      try {
        await redis.ping();
        return new Response("ok", { status: 200, headers: cors });
      } catch (err) {
        log(`health: redis ping failed: ${(err as Error).message}`);
        return new Response("redis unreachable", {
          status: 503,
          headers: cors,
        });
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
        return Response.json(
          { iceServers: DEFAULT_ICE_SERVERS },
          { headers: cors },
        );
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
      const otherRole = OPPOSITE_ROLE[role];
      const memberKey = redisKey(role, channelId);
      const livenessKey = sessionLivenessKey(channelId, sessionId);

      peers.set(sessionId, ws);

      // Everything redis owns happens before the session is acknowledged.
      // `registered` used to be sent in the middle of this, so a redis that
      // accepted connections but never answered produced a registered
      // session with no peers and no error — a socket that could only ever
      // time out. Ordered this way a redis failure is a refused connection,
      // which the peer's reconnect loop can act on.
      let peerIds: string[];
      try {
        await subscribe(toSessionTopic(channelId, sessionId));
        await subscribe(channelPresenceTopic(channelId));
        await Promise.all([
          redis.sadd(memberKey, sessionId),
          redis.expire(memberKey, SESSION_TTL),
          redis.set(livenessKey, "1", "EX", SESSION_TTL),
        ]);
        peerIds = await livePeers(channelId, otherRole);
      } catch (err) {
        log(
          `register ${role} ${sessionId} in ${channelId}: ${(err as Error).message}`,
        );
        peers.delete(sessionId);
        // Best-effort tidy-up; the TTLs above expire on their own if these
        // cannot run either.
        void Promise.allSettled([
          unsubscribe(toSessionTopic(channelId, sessionId)),
          unsubscribe(channelPresenceTopic(channelId)),
        ]);
        if (ch.producers.size === 0 && ch.consumers.size === 0) {
          channels.delete(channelId);
        }
        ws.send(
          JSON.stringify({
            type: "error",
            message: "hub cannot reach its presence store; retry",
          }),
        );
        ws.close(CLOSE_REDIS_UNAVAILABLE, "redis unavailable");
        return;
      }

      ws.data.refreshTimer = setInterval(() => {
        redis
          .expire(memberKey, SESSION_TTL)
          .catch((err) => log(`refresh member: ${err.message}`));
        redis
          .expire(livenessKey, SESSION_TTL)
          .catch((err) => log(`refresh liveness: ${err.message}`));
      }, SESSION_REFRESH_INTERVAL);

      ws.send(
        JSON.stringify({ type: "registered", channelId, role, sessionId }),
      );

      // Local peers are added on top of the redis view: this instance knows
      // its own sockets first-hand, and a member whose liveness key is mid
      // refresh should not read as absent to a peer sharing its machine.
      const localPeers = otherRole === "producer" ? ch.producers : ch.consumers;
      for (const peerId of new Set([...peerIds, ...localPeers.keys()])) {
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

      try {
        await relayToSession(
          channelId,
          outer.target,
          JSON.stringify({
            type: "signal",
            from: ws.data.sessionId,
            data: innerData,
          }),
        );
      } catch (err) {
        log(`relay to ${outer.target}: ${(err as Error).message}`);
        ws.send(
          JSON.stringify({
            type: "error",
            message: "hub could not relay the signal; retry",
          }),
        );
      }
    },

    async close(ws) {
      const { channelId, role, sessionId, refreshTimer } = ws.data;
      if (refreshTimer) clearInterval(refreshTimer);
      const ch = channels.get(channelId);
      if (!ch) return;

      const peers = role === "producer" ? ch.producers : ch.consumers;

      peers.delete(sessionId);

      // A teardown cannot be retried by anyone, so every step is attempted
      // and failures are logged: leftovers expire via SESSION_TTL, and the
      // liveness check treats them as dead in the meantime.
      const results = await Promise.allSettled([
        unsubscribe(toSessionTopic(channelId, sessionId)),
        redis.srem(redisKey(role, channelId), sessionId),
        redis.del(sessionLivenessKey(channelId, sessionId)),
      ]);
      for (const r of results) {
        if (r.status === "rejected") {
          log(
            `close ${sessionId}: ${(r.reason as Error)?.message ?? r.reason}`,
          );
        }
      }

      publishPresence(channelId, "peer_left", role, sessionId);

      if (ch.producers.size === 0 && ch.consumers.size === 0) {
        await unsubscribe(channelPresenceTopic(channelId)).catch((err: Error) =>
          log(`close unsubscribe presence: ${err.message}`),
        );
        channels.delete(channelId);
      }
    },
  },
});

async function shutdown() {
  log("shutting down");
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

log(`blit-hub listening on port ${PORT}`);
