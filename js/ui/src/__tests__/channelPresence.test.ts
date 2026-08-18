import { describe, expect, it, vi } from "vitest";
import type { ChannelNamesWatch } from "@blit-sh/core";
import {
  followChannelNames,
  probeChannelNames,
  type ChannelPresenceHost,
} from "../channelPresence";

const SESSION = "blit.session.v1";
const SYSTEMD = "blit.systemd.v1";

/** A server that watches, driven by hand. */
function watchingHost(initial: readonly string[]) {
  const present = new Set(initial);
  let onNames: ((present: ReadonlySet<string>) => void) | null = null;
  let stopped = 0;
  const host: ChannelPresenceHost = {
    watchChannelNames: (_names, callback) => {
      onNames = callback;
      return Promise.resolve({
        present,
        stop: () => {
          stopped += 1;
        },
      } satisfies ChannelNamesWatch);
    },
    connectChannel: () => Promise.reject(new Error("not probed")),
  };
  return {
    host,
    /** What the server does when an extension is installed or removed. */
    publish(names: readonly string[]) {
      present.clear();
      for (const name of names) present.add(name);
      onNames?.(present);
    },
    get stopped() {
      return stopped;
    },
  };
}

/** A server too old to watch, which can only be probed. */
function probeOnlyHost(serving: readonly string[]) {
  const closed: string[] = [];
  const host: ChannelPresenceHost = {
    watchChannelNames: () =>
      Promise.reject(new Error("Server does not support channel-name watches")),
    connectChannel: (name) =>
      serving.includes(name)
        ? Promise.resolve({
            close: () => {
              closed.push(name);
            },
          })
        : Promise.reject(new Error(`Channel ${name} refused`)),
  };
  return { host, closed };
}

describe("following channel presence", () => {
  it("reports what is served now and every later change", async () => {
    const server = watchingHost([SESSION]);
    const seen: string[][] = [];
    const stop = await followChannelNames(
      server.host,
      [SESSION, SYSTEMD],
      (present) => seen.push([...present]),
    );
    expect(seen).toEqual([[SESSION]]);

    server.publish([SESSION, SYSTEMD]);
    server.publish([SYSTEMD]);
    expect(seen).toEqual([[SESSION], [SESSION, SYSTEMD], [SYSTEMD]]);

    stop();
    expect(server.stopped).toBe(1);
  });

  it("probes once when the server cannot watch", async () => {
    const server = probeOnlyHost([SYSTEMD]);
    const seen: string[][] = [];
    const stop = await followChannelNames(
      server.host,
      [SESSION, SYSTEMD],
      (present) => seen.push([...present]),
    );
    expect(seen).toEqual([[SYSTEMD]]);
    // The probe asked a question; it must not leave a channel open.
    expect(server.closed).toEqual([SYSTEMD]);
    // Nothing to release, and calling it must still be safe.
    expect(stop).not.toThrow();
  });

  it("treats a refused connect as the answer, not a failure", async () => {
    const server = probeOnlyHost([]);
    await expect(
      probeChannelNames(server.host, [SESSION, SYSTEMD]),
    ).resolves.toEqual(new Set());
  });

  it("asks for both names in one watch", async () => {
    const server = watchingHost([]);
    const watch = vi.spyOn(server.host, "watchChannelNames");
    await followChannelNames(server.host, [SESSION, SYSTEMD], () => {});
    expect(watch).toHaveBeenCalledTimes(1);
    expect(watch.mock.calls[0]![0]).toEqual([SESSION, SYSTEMD]);
  });
});
