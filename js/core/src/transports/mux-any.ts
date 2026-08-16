/**
 * Pick a mux transport: worker-backed where that is possible, direct where it
 * is not.
 *
 * The worker version is what keeps audio flowing while the main thread is
 * busy, so it is the default wherever a `Worker` exists. It is not always
 * available — a `Worker` needs a real document origin, and there are hosts
 * (tests, embeddings, older runtimes) with none — and a session that cannot
 * spawn one must still connect rather than fail. The direct transport is the
 * same code, just running on the calling thread.
 */

import { MuxTransport } from "./mux";
import { WorkerMuxTransport } from "./mux-proxy";
import type { MuxWorkerOptions } from "./mux-worker-protocol";
import type { BlitDebug, BlitTransport } from "../types";

/**
 * What callers use, satisfied by both. Deliberately narrow: only the four
 * methods the app actually calls, so neither implementation is obliged to
 * mirror internals nobody exercises.
 */
export type AnyMuxTransport = {
  connect(): void;
  close(): void;
  updateWtCertHash(hexHash: string, wtUrl?: string): void;
  createChannel(destName: string): BlitTransport & { channelId: number };
};

export interface CreateMuxTransportOptions extends MuxWorkerOptions {
  debug?: BlitDebug;
  /** Force the direct transport. The worker is otherwise preferred. */
  inline?: boolean;
}

export function createMuxTransport(
  wsUrl: string,
  passphrase: string,
  options?: CreateMuxTransportOptions,
): AnyMuxTransport {
  let fallback: string | null = null;
  if (options?.inline) {
    fallback = "asked for";
  } else if (typeof Worker === "undefined") {
    fallback = "no Worker in this context";
  } else {
    try {
      return new WorkerMuxTransport(
        wsUrl,
        passphrase,
        options,
      ) as unknown as AnyMuxTransport;
    } catch (err) {
      // Falling through rather than failing: a session on the direct
      // transport works, and one that could not start does not.
      fallback =
        err instanceof Error ? `${err.name}: ${err.message}` : `${err}`;
    }
  }
  const direct = new MuxTransport(
    wsUrl,
    passphrase,
    options,
  ) as AnyMuxTransport;
  (direct as { workerFallback?: string }).workerFallback = fallback;
  // Stamped on the channels too, not just the transport: consumers hold a
  // channel and never see the transport that made it, and the consumer that
  // cares about this is the one that just lost its short path to the decoder.
  const create = direct.createChannel.bind(direct);
  direct.createChannel = (destName: string) => {
    const channel = create(destName);
    (channel as { workerFallback?: string }).workerFallback = fallback;
    return channel;
  };
  return direct;
}
