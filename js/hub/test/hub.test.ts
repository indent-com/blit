/**
 * Hub integration tests against a real redis (valkey) and real WebSockets.
 *
 * The bug these exist for was invisible to unit tests: the hub answered
 * HTTP, accepted sockets, and sent `registered`, while every peer waited
 * forever for a `peer_joined` that came after an await on a redis whose
 * socket was dead. So these drive the whole thing — server process, redis,
 * two peers — and the redis is killed mid-test to prove the hub now refuses
 * connections it cannot serve instead of parking them.
 */
import { afterAll, beforeAll, describe, expect, test } from "bun:test";
import { spawn } from "node:child_process";
import nacl from "tweetnacl";

const REDIS_PORT = 7699;
const HUB_PORT = 7698;
const HUB_URL = `ws://127.0.0.1:${HUB_PORT}`;

let redisProc: ReturnType<typeof spawn> | null = null;
let hubProc: ReturnType<typeof spawn> | null = null;

/** A channel id is an ed25519 public key; any keypair will do here. */
function channelId(): string {
  const kp = nacl.sign.keyPair();
  return Array.from(kp.publicKey, (b) => b.toString(16).padStart(2, "0")).join(
    "",
  );
}

async function until(
  what: string,
  predicate: () => boolean | Promise<boolean>,
  timeoutMs = 10_000,
): Promise<void> {
  const deadline = Date.now() + timeoutMs;
  while (Date.now() < deadline) {
    if (await predicate()) return;
    await Bun.sleep(50);
  }
  throw new Error(`timed out waiting for ${what}`);
}

/** The redis server binary: valkey and redis are interchangeable here, and
 *  the env var lets a sandbox point at a nix store path directly. */
const REDIS_BIN = process.env.REDIS_SERVER_BIN ?? "valkey-server";

function startRedis(): void {
  redisProc = spawn(
    REDIS_BIN,
    ["--port", String(REDIS_PORT), "--save", "", "--appendonly", "no"],
    { stdio: "ignore" },
  );
  redisProc.on("error", (err) => {
    // A missing server would otherwise surface as an opaque hook timeout.
    console.error(
      `cannot start ${REDIS_BIN}: ${err.message}\n` +
        "set REDIS_SERVER_BIN, or install valkey/redis, to run the hub tests",
    );
  });
}

async function httpOk(path: string): Promise<number> {
  try {
    const r = await fetch(`http://127.0.0.1:${HUB_PORT}${path}`);
    return r.status;
  } catch {
    return 0;
  }
}

/** Collects everything a peer socket receives, in order. */
function peer(id: string, role: "producer" | "consumer") {
  const ws = new WebSocket(`${HUB_URL}/channel/${id}/${role}`);
  const seen: Record<string, unknown>[] = [];
  let closeCode: number | null = null;
  ws.onmessage = (e) => seen.push(JSON.parse(String(e.data)));
  ws.onclose = (e) => (closeCode = e.code);
  return {
    ws,
    seen,
    types: () => seen.map((m) => m.type as string),
    closeCode: () => closeCode,
    close: () => ws.close(),
  };
}

beforeAll(async () => {
  startRedis();
  hubProc = spawn("bun", ["run", "src/index.ts"], {
    cwd: import.meta.dir + "/..",
    env: {
      ...process.env,
      PORT: String(HUB_PORT),
      REDIS_URL: `redis://127.0.0.1:${REDIS_PORT}`,
    },
    stdio: "ignore",
  });
  await until("hub healthy", async () => (await httpOk("/health")) === 200);
});

afterAll(() => {
  hubProc?.kill("SIGKILL");
  redisProc?.kill("SIGKILL");
});

describe("hub with redis available", () => {
  test("a consumer learns about a producer already in the channel", async () => {
    const id = channelId();
    const producer = peer(id, "producer");
    await until("producer registered", () =>
      producer.types().includes("registered"),
    );

    const consumer = peer(id, "consumer");
    await until("consumer registered", () =>
      consumer.types().includes("registered"),
    );
    // The whole point of the hub: the consumer is told who to offer to.
    await until("peer_joined", () => consumer.types().includes("peer_joined"));
    const joined = consumer.seen.find((m) => m.type === "peer_joined")!;
    expect(joined.role).toBe("producer");

    // `registered` comes first — a peer that sees a join before its own
    // registration has no session id to sign with.
    expect(consumer.types()[0]).toBe("registered");

    // And the producer hears about the consumer, cross-published.
    await until(
      "producer sees the consumer",
      () => producer.types().filter((t) => t === "peer_joined").length >= 1,
    );

    producer.close();
    consumer.close();
  });

  test("a departing peer is announced, and stops being announced", async () => {
    const id = channelId();
    const producer = peer(id, "producer");
    await until("registered", () => producer.types().includes("registered"));
    producer.close();

    const consumer = peer(id, "consumer");
    await until("registered", () => consumer.types().includes("registered"));
    await Bun.sleep(500);
    // The producer left cleanly, so it must not be offered as a target.
    expect(consumer.types()).not.toContain("peer_joined");
    consumer.close();
  });
});

describe("hub with redis gone", () => {
  test("refuses sockets it cannot serve, instead of parking them", async () => {
    // This is the outage: with redis unreachable the hub used to send
    // `registered` and then hang forever on the very await that produces
    // `peer_joined`, leaving the peer with a live socket and no signal.
    redisProc?.kill("SIGKILL");
    await until(
      "health degraded",
      async () => {
        const s = await httpOk("/health");
        return s === 503;
      },
      15_000,
    );

    const id = channelId();
    const p = peer(id, "producer");
    await until(
      "socket refused",
      () => p.closeCode() !== null || p.types().includes("error"),
      15_000,
    );
    // Either an explicit error or a close — never a silent registration.
    expect(p.types()).not.toContain("registered");

    // Recovery: a fresh redis and the hub serves again, no restart.
    startRedis();
    await until(
      "health restored",
      async () => (await httpOk("/health")) === 200,
      20_000,
    );
    const again = peer(channelId(), "producer");
    await until("registered after recovery", () =>
      again.types().includes("registered"),
    );
    again.close();
  }, 60_000);
});
