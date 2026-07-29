import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { PASSPHRASE_KEY } from "../passphrase-storage";

// The config WebSocket is the app's de-facto session heartbeat: whatever it
// concludes about the passphrase is what the whole UI acts on. Getting the
// throttled case wrong here logs the user out of a working session.

class MockWebSocket {
  static instances: MockWebSocket[] = [];

  readonly url: string;
  sentData: string[] = [];
  closed = false;

  onopen: ((ev: Event) => void) | null = null;
  onmessage: ((ev: MessageEvent) => void) | null = null;
  onerror: ((ev: Event) => void) | null = null;
  onclose: ((ev: CloseEvent) => void) | null = null;

  constructor(url: string) {
    this.url = url;
    MockWebSocket.instances.push(this);
  }

  send(data: string) {
    this.sentData.push(data);
  }

  close() {
    this.closed = true;
    this.onclose?.({ code: 1000, wasClean: true } as CloseEvent);
  }

  open() {
    this.onopen?.({} as Event);
  }

  message(data: string) {
    this.onmessage?.({ data } as MessageEvent);
  }
}

function latest(): MockWebSocket {
  return MockWebSocket.instances[MockWebSocket.instances.length - 1];
}

/** storage.ts caches module-level connection state, so each test needs a
 *  freshly imported copy. */
async function freshStorage() {
  vi.resetModules();
  return await import("../storage");
}

describe("config WebSocket authentication", () => {
  beforeEach(() => {
    vi.useFakeTimers();
    MockWebSocket.instances = [];
    vi.stubGlobal("WebSocket", MockWebSocket);
    localStorage.setItem(PASSPHRASE_KEY, "correct-horse");
  });

  afterEach(() => {
    vi.useRealTimers();
    vi.unstubAllGlobals();
    localStorage.clear();
  });

  it("discards the passphrase when the server rejects it", async () => {
    const storage = await freshStorage();
    storage.connectConfigWs();
    latest().open();
    expect(latest().sentData).toEqual(["correct-horse"]);

    latest().message("auth");

    expect(localStorage.getItem(PASSPHRASE_KEY)).toBeNull();
  });

  // "busy" is the auth throttle refusing the handshake — a peer lockout or the
  // global concurrent-handshake cap — before the passphrase is examined.
  // Clearing the credential here was the bug that dropped a connected user at
  // the login screen out of the blue, where re-entering the same correct
  // passphrase then failed for the remaining duration of the lockout.
  it("keeps the passphrase and retries when the server is throttling", async () => {
    const storage = await freshStorage();
    storage.connectConfigWs();
    latest().open();
    latest().message("busy");

    expect(localStorage.getItem(PASSPHRASE_KEY)).toBe("correct-horse");

    const before = MockWebSocket.instances.length;
    await vi.advanceTimersByTimeAsync(5000);
    expect(MockWebSocket.instances.length).toBeGreaterThan(before);

    // The retry replays the same credential and can still succeed.
    latest().open();
    latest().message("ok");
    latest().message("ready");
    expect(storage.configWsStatus()).toBe("connected");
    expect(localStorage.getItem(PASSPHRASE_KEY)).toBe("correct-horse");
  });

  // A fixed retry interval is what turns one server-side lockout into a
  // sustained handshake load that keeps the throttle tripped. Math.random is
  // pinned so the jitter multiplier is exactly 1 and the gaps are exact.
  it("backs off between reconnect attempts", async () => {
    vi.spyOn(Math, "random").mockReturnValue(0);
    const storage = await freshStorage();
    storage.connectConfigWs();
    latest().open();
    latest().message("busy");

    const gaps: number[] = [];
    let elapsed = 0;
    let seen = MockWebSocket.instances.length;
    let lastAttemptAt = 0;

    // Step finely enough that each gap is measured exactly, and long enough to
    // cross the 2s → 4s → 8s → 16s ladder.
    while (elapsed < 60_000 && gaps.length < 4) {
      await vi.advanceTimersByTimeAsync(100);
      elapsed += 100;
      if (MockWebSocket.instances.length > seen) {
        seen = MockWebSocket.instances.length;
        gaps.push(elapsed - lastAttemptAt);
        lastAttemptAt = elapsed;
        // Keep getting throttled so the backoff keeps growing.
        latest().open();
        latest().message("busy");
      }
    }

    expect(gaps).toEqual([2000, 4000, 8000, 16000]);
  });

  it("resets the backoff once a connection authenticates", async () => {
    vi.spyOn(Math, "random").mockReturnValue(0);
    const storage = await freshStorage();
    storage.connectConfigWs();
    latest().open();
    latest().message("busy");

    // Escalate the delay: 2s then 4s.
    await vi.advanceTimersByTimeAsync(2000);
    latest().open();
    latest().message("busy");
    await vi.advanceTimersByTimeAsync(4000);

    // This attempt authenticates, so the ladder must start over.
    latest().open();
    latest().message("ok");
    latest().close();

    const before = MockWebSocket.instances.length;
    await vi.advanceTimersByTimeAsync(1999);
    expect(MockWebSocket.instances.length).toBe(before);
    await vi.advanceTimersByTimeAsync(1);
    expect(MockWebSocket.instances.length).toBe(before + 1);
  });
});
