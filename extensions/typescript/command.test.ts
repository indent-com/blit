import { describe, expect, test } from "bun:test";
import { ByteWriter, type BlitHost, decodeUtf8, encodeUtf8 } from "./blit";
import { decodeInvocation, serveCommands } from "./command";

const CHANNEL = 0x95;

function opened(): Uint8Array {
  return new ByteWriter()
    .u8(CHANNEL)
    .u8(1)
    .u32(2)
    .u8(0)
    .u64(1024n * 1024n)
    .u16(0)
    .u32(0)
    .finish();
}

function registered(): Uint8Array {
  return new ByteWriter().u8(0x92).u8(4).u16(1).u8(0).u64(7n).u64(9n).finish();
}

function accepted(): Uint8Array {
  return new ByteWriter()
    .u8(CHANNEL)
    .u8(2)
    .u32(3)
    .u32(2)
    .u64(1024n * 1024n)
    .u16(0)
    .u32(0)
    .finish();
}

function invocation(args: readonly string[]): Uint8Array {
  const payload = new ByteWriter().u8(1).u8(0).u16(args.length);
  for (const argument of args) {
    const encoded = encodeUtf8(argument);
    payload.u32(encoded.length).bytes(encoded);
  }
  return new ByteWriter()
    .u8(CHANNEL)
    .u8(3)
    .u32(3)
    .bytes(payload.finish())
    .finish();
}

function fakeHost(incoming: Uint8Array[]): BlitHost & { sent: Uint8Array[] } {
  const sent: Uint8Array[] = [];
  return {
    context: {
      extensionId: 7n,
      definitionRevision: 9n,
      attempt: 11n,
      taskId: 13,
      moduleHash: "42".repeat(32),
      name: "doctor",
      args: [],
      detached: true,
      persistent: true,
      enabled: true,
      desiredRunning: true,
      protocolVersion: 1,
      features: (1 << 11) | (1 << 12),
      bootGeneration: 17n,
      serverVersion: "test",
    },
    sent,
    send(packet) {
      sent.push(packet);
    },
    recv() {
      return incoming.shift();
    },
    wait: () => 2,
    waitUntil: () => 2,
    realtimeNow: () => 1n,
    monotonicNow: () => 1n,
    random: (length) => new Uint8Array(length),
    sleep() {},
    log() {},
  };
}

describe("QuickJS TypeScript support", () => {
  test("UTF-8 round-trips without web globals", () => {
    const text = "plain · café · 🚀";
    expect(decodeUtf8(encodeUtf8(text))).toBe(text);
    expect(() => decodeUtf8(Uint8Array.of(0xc0, 0x80))).toThrow(
      "invalid UTF-8",
    );
  });

  test("decodes invocation arguments", () => {
    const packet = invocation(["--json", "café"]);
    expect(decodeInvocation(packet.subarray(6))).toEqual({
      args: ["--json", "café"],
      streamsStdin: false,
    });
  });

  test("registers, serves, acknowledges, exits, and closes", () => {
    const host = fakeHost([
      opened(),
      registered(),
      accepted(),
      invocation(["--json"]),
    ]);
    const code = serveCommands(
      {
        protocol: "blit.cli.v1",
        summary: "test",
        commands: [{ path: [] }],
      },
      ({ args }) => ({
        stdout: `${args.length} argument\n`,
        result: { contentType: "application/json", data: '{"ok":true}\n' },
      }),
      host,
    );

    expect(code).toBe(0);
    expect(host.sent).toHaveLength(7);
    expect(host.sent[0].slice(0, 6)).toEqual(
      Uint8Array.of(CHANNEL, 1, 2, 0, 0, 0),
    );
    expect(host.sent[1].slice(0, 8)).toEqual(
      Uint8Array.of(0x94, 1, 1, 0, 2, 0, 0, 0),
    );
    expect(host.sent[2].slice(0, 6)).toEqual(
      Uint8Array.of(CHANNEL, 4, 3, 0, 0, 0),
    );
    expect(decodeUtf8(host.sent[3].subarray(7))).toBe("1 argument\n");
    expect(host.sent[4][6]).toBe(4);
    expect(host.sent[5][6]).toBe(5);
    expect(host.sent[6]).toEqual(Uint8Array.of(CHANNEL, 5, 3, 0, 0, 0, 0));
  });
});
