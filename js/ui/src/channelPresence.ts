/**
 * Which extension channels a server is serving, kept current.
 *
 * The panels above this ask one question — is anything answering
 * `blit.session.v1`, `blit.systemd.v1` — and used to answer it with a
 * connect-and-close probe per expansion. A probe is a photograph: install an
 * extension and the tab it should add never appears, remove one and its tab
 * outlives it until the row is collapsed and re-opened. So this follows the
 * server's own registry instead (`CHANNEL_WATCH`), and falls back to the probe
 * only for a server too old to have it, where a photograph is all there is.
 */

import type { ChannelNamesWatch } from "@blit-sh/core";

/** What following a name set needs from a connection. Opening a channel is
 *  only ever to hang up again, so the probe asks for nothing more. */
export interface ChannelPresenceHost {
  watchChannelNames(
    names: readonly string[],
    onNames: (present: ReadonlySet<string>) => void,
  ): Promise<ChannelNamesWatch>;
  connectChannel(name: string): Promise<{ close(): void }>;
}

/**
 * Report which of `names` have a listener, now and on every later change.
 *
 * `onPresent` is called with the first answer before this resolves, and again
 * whenever the set changes. The returned function stops the following; calling
 * it is required, because a watch holds a channel ID on the server until it is
 * released.
 *
 * A watch the server cannot offer is not an error: presence is probed once and
 * the caller gets a no-op stop, so the panels behave as they did before rather
 * than showing nothing at all.
 */
export async function followChannelNames(
  host: ChannelPresenceHost,
  names: readonly string[],
  onPresent: (present: ReadonlySet<string>) => void,
): Promise<() => void> {
  try {
    const watch = await host.watchChannelNames(names, onPresent);
    onPresent(watch.present);
    return () => watch.stop();
  } catch {
    onPresent(await probeChannelNames(host, names));
    return () => {};
  }
}

/**
 * Ask once whether each name has a listener, by connecting and hanging up.
 *
 * A refused connect is the answer rather than a failure: nothing serves that
 * name here. The channel that does open is closed immediately — this is a
 * question about the registry, not a session.
 */
export async function probeChannelNames(
  host: ChannelPresenceHost,
  names: readonly string[],
): Promise<ReadonlySet<string>> {
  const answers = await Promise.all(
    names.map(async (name) => {
      try {
        (await host.connectChannel(name)).close();
        return name;
      } catch {
        return null;
      }
    }),
  );
  return new Set(answers.filter((name): name is string => name !== null));
}
