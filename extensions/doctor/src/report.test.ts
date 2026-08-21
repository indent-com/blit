import { describe, expect, test } from "bun:test";
import type { BlitHost } from "../../typescript/blit";
import { inspectDoctor, renderDoctor } from "./report";

function healthyHost(): Pick<
  BlitHost,
  "context" | "monotonicNow" | "random" | "realtimeNow" | "sleep"
> {
  let monotonic = 4_000_000n;
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
      features: 0x7fffffff,
      bootGeneration: 17n,
      serverVersion: "0.55.1",
    },
    monotonicNow: () => monotonic,
    realtimeNow: () => 1_800_000_000_000_000_000n,
    random: (length) => new Uint8Array(length).fill(7),
    sleep: () => {
      monotonic += 1_250_000n;
    },
  };
}

describe("@doctor report", () => {
  test("renders a healthy report for humans", () => {
    const report = inspectDoctor(healthyHost());
    expect(report.status).toBe("healthy");
    expect(report.summary.failed).toBe(0);
    expect(report.summary.unavailableCapabilities).toBe(0);

    const text = renderDoctor(report);
    expect(text).toContain("Blit doctor");
    expect(text).toContain("native QuickJS");
    expect(text).toContain("1.250 ms monotonic sleep");
    expect(text).toContain("healthy — 7 passed, 0 warnings, 0 failed");
  });

  test("degrades on incompatible context while keeping JSON serializable", () => {
    const host = healthyHost();
    const report = inspectDoctor({
      ...host,
      context: {
        ...host.context,
        protocolVersion: 2,
        enabled: false,
        serverVersion: null,
        bootGeneration: null,
      },
    });
    expect(report.status).toBe("degraded");
    expect(report.summary.failed).toBe(2);
    expect(report.summary.warnings).toBe(1);
    expect(() => JSON.stringify(report)).not.toThrow();
  });
});
