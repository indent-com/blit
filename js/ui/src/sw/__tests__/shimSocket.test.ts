import { afterEach, describe, expect, it, vi } from "vitest";
import type { PreviewTarget } from "@blit-sh/core";
import { shimTag } from "../inject";

const target: PreviewTarget = {
  dest: "local",
  scheme: "http",
  host: "localhost",
  port: 3000,
};

/**
 * Run the injected shim with `window`/`navigator`/`location` of our own.
 *
 * The shim installs itself onto those globals and takes `serviceWorker` off
 * `Navigator.prototype`; handing it stand-ins keeps jsdom's real globals
 * intact so each test gets a clean set of shims.
 */
function loadShim(opts: { controller?: unknown } = {}) {
  const html = new TextDecoder().decode(shimTag(target, "sid=abc"));
  const src = html.replace(/^<script>/, "").replace(/<\/script>$/, "");

  /** The port the worker would hold, so a test can play the relay. */
  let relay: MessagePort | null = null;
  const controller =
    "controller" in opts
      ? opts.controller
      : {
          postMessage(msg: { type: string }, transfer: MessagePort[]) {
            if (msg.type === "blit-ws-open") relay = transfer[0];
          },
        };

  const win: Record<string, unknown> = {
    WebSocket: class NativeWSStub {},
    open() {},
  };
  const fn = new Function(
    "window",
    "navigator",
    "location",
    "Document",
    "Navigator",
    src,
  );
  fn(
    win,
    { serviceWorker: { controller } },
    {
      href: "https://gateway.example/p/local",
      host: "gateway.example",
      origin: "https://gateway.example",
      protocol: "https:",
    },
    class {},
    class {},
  );

  return {
    WS: win.WebSocket as new (url: string, protocols?: string[]) => WebSocket,
    get relay() {
      return relay;
    },
  };
}

/** Subscribe the way an HMR client does: after the constructor returns. */
function watch(ws: WebSocket): string[] {
  const events: string[] = [];
  ws.onopen = () => events.push("open");
  ws.onerror = () => events.push("error");
  ws.onclose = () => events.push("close");
  return events;
}

const encoder = new TextEncoder();

/** A 101 response and nothing more — enough to reach OPEN. */
const handshake = () =>
  encoder.encode(
    "HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\n\r\n",
  ).buffer;

function serverFrame(opcode: number, payload = new Uint8Array(0)) {
  const out = new Uint8Array(2 + payload.length);
  out[0] = 0x80 | opcode;
  out[1] = payload.length;
  out.set(payload, 2);
  return out.buffer;
}

/**
 * A relayed socket must always end in a close event.
 *
 * Nothing else in the stack can speak for it: the worker's sentinel covers a
 * relay that dies, but a worker the browser force-terminates sends nothing,
 * and a page that never hears a close is a page whose HMR client stops
 * reconnecting for good — connectivity lost with no symptom.
 */
describe("relayed WebSocket liveness", () => {
  afterEach(() => vi.useRealTimers());

  // clearTimeout/clearInterval must be faked too, or a cancelled timer stays
  // on the fake clock and every leak assertion below passes vacuously.
  const fake = () =>
    vi.useFakeTimers({
      toFake: [
        "setTimeout",
        "clearTimeout",
        "setInterval",
        "clearInterval",
        "Date",
      ],
    });

  it("reports a close when no service worker controls the frame", async () => {
    const shim = loadShim({ controller: null });
    const ws = new shim.WS("ws://gateway.example/hmr");
    // Constructing must not dispatch: the app has not subscribed yet.
    const events = watch(ws);
    await new Promise((r) => setTimeout(r, 0));
    expect(events).toEqual(["error", "close"]);
  });

  it("reports a close when the pipe goes quiet while open", async () => {
    fake();
    const shim = loadShim();
    const ws = new shim.WS("ws://gateway.example/hmr");
    const events = watch(ws);
    await vi.advanceTimersByTimeAsync(0);
    shim.relay!.postMessage(handshake());
    await vi.advanceTimersByTimeAsync(0);
    expect(ws.readyState).toBe(1);
    await vi.advanceTimersByTimeAsync(40_000);
    expect(events).toContain("close");
  });

  it("reports a close on the worker's relay sentinel", async () => {
    const shim = loadShim();
    const ws = new shim.WS("ws://gateway.example/hmr");
    const events = watch(ws);
    await new Promise((r) => setTimeout(r, 0));
    shim.relay!.postMessage({ blitClosed: true });
    await new Promise((r) => setTimeout(r, 0));
    expect(events).toContain("close");
  });

  it("reports a close when the app closes and no reply comes", async () => {
    fake();
    const shim = loadShim();
    const ws = new shim.WS("ws://gateway.example/hmr");
    const events = watch(ws);
    await vi.advanceTimersByTimeAsync(0);
    shim.relay!.postMessage(handshake());
    await vi.advanceTimersByTimeAsync(0);

    ws.close();
    expect(ws.readyState).toBe(2);
    // The target is gone and will never echo the close frame.
    await vi.advanceTimersByTimeAsync(30_000);
    expect(events).toContain("close");
    expect(ws.readyState).toBe(3);
  });

  it("reports a close when the app closes before the upgrade lands", async () => {
    fake();
    const shim = loadShim();
    const ws = new shim.WS("ws://gateway.example/hmr");
    const events = watch(ws);
    await vi.advanceTimersByTimeAsync(0);

    ws.close();
    await vi.advanceTimersByTimeAsync(30_000);
    expect(events).toContain("close");
    expect(ws.readyState).toBe(3);
  });

  it("reports a close when the handshake stalls", async () => {
    fake();
    const shim = loadShim();
    const ws = new shim.WS("ws://gateway.example/hmr");
    const events = watch(ws);
    await vi.advanceTimersByTimeAsync(20_000);
    expect(events).toContain("close");
  });

  // A leaked interval keeps a frame's timers alive for every socket an app has
  // ever opened, and an HMR client reconnects for as long as the pane is open.
  it("leaves no timer running once the target closes cleanly", async () => {
    fake();
    const shim = loadShim();
    const ws = new shim.WS("ws://gateway.example/hmr");
    const events = watch(ws);
    await vi.advanceTimersByTimeAsync(0);
    shim.relay!.postMessage(handshake());
    await vi.advanceTimersByTimeAsync(0);

    shim.relay!.postMessage(serverFrame(0x8, new Uint8Array([0x03, 0xe8])));
    await vi.advanceTimersByTimeAsync(0);
    expect(ws.readyState).toBe(3);
    expect(events).toContain("close");
    await vi.advanceTimersByTimeAsync(60_000);
    expect(vi.getTimerCount()).toBe(0);
  });

  it("leaves no timer running once the socket dies", async () => {
    fake();
    const shim = loadShim();
    new shim.WS("ws://gateway.example/hmr");
    await vi.advanceTimersByTimeAsync(0);
    shim.relay!.postMessage(handshake());
    await vi.advanceTimersByTimeAsync(0);
    shim.relay!.postMessage({ blitClosed: true });
    await vi.advanceTimersByTimeAsync(60_000);
    expect(vi.getTimerCount()).toBe(0);
  });

  it("still delivers messages and pongs a server ping", async () => {
    fake();
    const shim = loadShim();
    const ws = new shim.WS("ws://gateway.example/hmr");
    const seen: string[] = [];
    ws.binaryType = "arraybuffer";
    ws.onmessage = (e) => seen.push(e.data as string);
    await vi.advanceTimersByTimeAsync(0);

    // Read from the start, so the shim's queued upgrade request is consumed
    // here rather than landing in the assertion below.
    const written: Uint8Array[] = [];
    shim.relay!.onmessage = (e) =>
      written.push(new Uint8Array(e.data as ArrayBuffer));
    shim.relay!.postMessage(handshake());
    await vi.advanceTimersByTimeAsync(0);
    expect(written).toHaveLength(1);
    expect(new TextDecoder().decode(written[0])).toContain(
      "Upgrade: websocket",
    );
    written.length = 0;

    shim.relay!.postMessage(serverFrame(0x1, encoder.encode("hello")));
    shim.relay!.postMessage(serverFrame(0x9));
    await vi.advanceTimersByTimeAsync(0);

    expect(seen).toEqual(["hello"]);
    // One pong, masked, with an empty payload.
    expect(written).toHaveLength(1);
    expect(written[0][0]).toBe(0x8a);
    expect(ws.readyState).toBe(1);
  });
});
