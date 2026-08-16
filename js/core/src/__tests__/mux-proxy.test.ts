/**
 * The main-thread face of the transport worker.
 *
 * What matters here is that callers cannot tell the difference: a channel
 * still delivers `Uint8Array` views to `message` listeners, still reports
 * status, and still allocates the channel ids that prefix every frame on the
 * wire. Those ids are allocated on this side precisely because
 * `createChannel` is synchronous and cannot wait for the worker.
 */

import { describe, it, expect, beforeEach } from "vitest";
import { WorkerMuxTransport } from "../transports/mux-proxy";
import type {
  MuxWorkerEvent,
  MuxWorkerRequest,
} from "../transports/mux-worker-protocol";

/** A worker that records what it was sent and can post events back. */
class FakeWorker {
  sent: MuxWorkerRequest[] = [];
  transfers: Transferable[][] = [];
  terminated = false;
  onmessage: ((event: MessageEvent<MuxWorkerEvent>) => void) | null = null;

  postMessage(message: MuxWorkerRequest, transfer: Transferable[] = []): void {
    this.sent.push(message);
    this.transfers.push(transfer);
  }

  terminate(): void {
    this.terminated = true;
  }

  emit(event: MuxWorkerEvent): void {
    this.onmessage?.({ data: event } as MessageEvent<MuxWorkerEvent>);
  }
}

describe("WorkerMuxTransport", () => {
  let fake: FakeWorker;
  let mux: WorkerMuxTransport;

  beforeEach(() => {
    fake = new FakeWorker();
    mux = new WorkerMuxTransport("ws://host/mux", "secret", undefined, () =>
      fake as unknown as Worker,
    );
  });

  it("initialises the worker with the connection details", () => {
    expect(fake.sent[0]).toMatchObject({
      t: "init",
      wsUrl: "ws://host/mux",
      passphrase: "secret",
    });
  });

  it("allocates channel ids on this side and tells the worker which to use", () => {
    const first = mux.createChannel("alpha");
    const second = mux.createChannel("beta");

    expect(first.channelId).toBe(0);
    expect(second.channelId).toBe(1);
    expect(fake.sent).toContainEqual({
      t: "channelCreate",
      id: 0,
      destName: "alpha",
    });
    expect(fake.sent).toContainEqual({
      t: "channelCreate",
      id: 1,
      destName: "beta",
    });
  });

  it("delivers frames to the right channel as a Uint8Array view", () => {
    const alpha = mux.createChannel("alpha");
    const beta = mux.createChannel("beta");
    const seenByAlpha: Uint8Array[] = [];
    const seenByBeta: Uint8Array[] = [];
    alpha.addEventListener("message", (d) => seenByAlpha.push(d as Uint8Array));
    beta.addEventListener("message", (d) => seenByBeta.push(d as Uint8Array));

    fake.emit({
      t: "channelMessage",
      id: 1,
      data: Uint8Array.from([7, 8, 9]).buffer,
    });

    expect(seenByAlpha).toHaveLength(0);
    expect(seenByBeta).toHaveLength(1);
    // A bare ArrayBuffer would change the meaning of every byte offset the
    // callers parse with.
    expect(seenByBeta[0]).toBeInstanceOf(Uint8Array);
    expect(Array.from(seenByBeta[0])).toEqual([7, 8, 9]);
  });

  it("reports status, auth rejection and the last error", () => {
    const channel = mux.createChannel("alpha");
    const seen: string[] = [];
    channel.addEventListener("statuschange", (s) => seen.push(s));

    fake.emit({
      t: "channelStatus",
      id: 0,
      status: "connected",
      authRejected: false,
      lastError: null,
    });
    expect(seen).toEqual(["connected"]);
    expect(channel.status).toBe("connected");

    fake.emit({
      t: "channelStatus",
      id: 0,
      status: "disconnected",
      authRejected: true,
      lastError: "bad passphrase",
    });
    expect(channel.authRejected).toBe(true);
    expect(channel.lastError).toBe("bad passphrase");
  });

  it("copies outbound data, because callers reuse their buffers", () => {
    const channel = mux.createChannel("alpha");
    const scratch = Uint8Array.from([1, 2, 3]);
    channel.send(scratch);

    const request = fake.sent.at(-1) as Extract<
      MuxWorkerRequest,
      { t: "channelSend" }
    >;
    expect(Array.from(new Uint8Array(request.data))).toEqual([1, 2, 3]);
    // The caller's buffer must survive the transfer intact.
    expect(scratch.byteLength).toBe(3);
    expect(Array.from(scratch)).toEqual([1, 2, 3]);
  });

  it("stops delivering to a channel once it is closed", () => {
    const channel = mux.createChannel("alpha");
    const seen: unknown[] = [];
    channel.addEventListener("message", (d) => seen.push(d));

    channel.close();
    fake.emit({
      t: "channelMessage",
      id: 0,
      data: Uint8Array.from([1]).buffer,
    });

    expect(seen).toHaveLength(0);
    expect(fake.sent).toContainEqual({ t: "channelClose", id: 0 });
  });

  it("terminates the worker on close, so the socket goes with it", () => {
    mux.close();
    expect(fake.terminated).toBe(true);
    expect(fake.sent).toContainEqual({ t: "close" });
  });
});

/**
 * The audio fast path.
 *
 * Audio is the reason the transport moved at all, so the routing that skips
 * the main thread deserves its own cover: a header still arrives here (the
 * AudioContext lifecycle runs on it) while the payload does not.
 */
describe("WorkerMuxTransport audio port", () => {
  it("hands the worker a port addressed to one channel", () => {
    const fake = new FakeWorker();
    const mux = new WorkerMuxTransport(
      "ws://host/mux",
      "secret",
      undefined,
      () => fake as unknown as Worker,
    );
    const channel = mux.createChannel("alpha");
    const { port2 } = new MessageChannel();

    mux.attachAudioPort(channel.channelId, port2);

    expect(fake.sent).toContainEqual({ t: "audioPort", id: 0 });
    // The port itself must be transferred, not structured-cloned.
    expect(fake.transfers.at(-1)).toEqual([port2]);
  });

  it("sends options a real worker can actually receive", () => {
    // The fake worker above accepts anything; a real one runs every message
    // through structured clone, which rejects functions. `debug` is a logger
    // with methods, so passing options through verbatim throws
    // DataCloneError inside the constructor — and the caller's try/catch
    // turns that into a silent downgrade to the main-thread transport.
    class CloningWorker extends FakeWorker {
      postMessage(message: MuxWorkerRequest, transfer: Transferable[] = []) {
        structuredClone(message);
        super.postMessage(message, transfer);
      }
    }
    const fake = new CloningWorker();

    expect(
      () =>
        new WorkerMuxTransport(
          "ws://host/mux",
          "secret",
          { wtUrl: "https://host/mux", debug: { log() {}, warn() {}, error() {} } },
          () => fake as unknown as Worker,
        ),
    ).not.toThrow();

    const init = fake.sent[0] as Extract<MuxWorkerRequest, { t: "init" }>;
    expect(init.options?.wtUrl).toBe("https://host/mux");
    expect(init.options).not.toHaveProperty("debug");
  });

  it("relays the worker's diagnostics to the logger it was given", () => {
    const lines: string[] = [];
    const fake = new FakeWorker();
    new WorkerMuxTransport(
      "ws://host/mux",
      "secret",
      {
        debug: {
          log: (msg, ...args) => lines.push([msg, ...args].join(" ")),
          warn() {},
          error() {},
        },
      },
      () => fake as unknown as Worker,
    );

    fake.emit({ t: "debug", level: "log", msg: "[mux] connected", args: [] });

    // The logger stays on this thread, so without the relay the WebTransport
    // upgrade, the backoff and the auth outcome all go unreported.
    expect(lines).toEqual(["[mux] connected"]);
  });

  it("can revoke the shortcut, so a dead decoder is not a dead channel", () => {
    const fake = new FakeWorker();
    const mux = new WorkerMuxTransport(
      "ws://host/mux",
      "secret",
      undefined,
      () => fake as unknown as Worker,
    );
    const channel = mux.createChannel("alpha");
    mux.attachAudioPort(channel.channelId, new MessageChannel().port2);

    channel.detachAudioPort();

    // Without this the worker keeps posting frames into a port whose far end
    // is gone, and the main thread — which receives only headers on this
    // route — has nothing left to decode: silence with no way back.
    expect(fake.sent).toContainEqual({ t: "audioPortDetach", id: 0 });
  });

  it("does not hand out ports once closed", () => {
    const fake = new FakeWorker();
    const mux = new WorkerMuxTransport(
      "ws://host/mux",
      "secret",
      undefined,
      () => fake as unknown as Worker,
    );
    mux.close();
    const before = fake.sent.length;
    const { port2 } = new MessageChannel();
    mux.attachAudioPort(0, port2);
    expect(fake.sent.length).toBe(before);
  });
});
