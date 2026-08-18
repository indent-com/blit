/**
 * ConnectionClients — the clients attached to ONE connection: age, bandwidth,
 * subscriptions, the terminals and surfaces each is watching, and a kick
 * control.
 *
 * This used to be its own overlay listing every connection at once. It is now
 * a section the remotes dialog expands under a remote's row, which is where a
 * client list belongs: a client is connected *to a remote*, and the remote row
 * already carries the status dot that says whether asking is worth anything.
 *
 * One consequence worth keeping: the CLIENT_WATCH subscription lives here, so
 * it starts when a row is expanded and stops when it is collapsed. Collapsed
 * remotes cost no per-second catalog traffic.
 */

import { createSignal, Index, onCleanup, Show } from "solid-js";
import type {
  BlitClientInfo,
  BlitClientList,
  BlitSession,
  BlitSurface,
  BlitWorkspace,
  ConnectionId,
  TerminalPalette,
} from "@blit-sh/core";
import { KICK_REASON_MAX, kickReasonByteLength } from "@blit-sh/core";
import { themeFor, ui, uiScale } from "./theme";
import {
  formatClientAge,
  formatClientBandwidth,
  formatClientLabel,
  formatClientOriginTag,
  formatClientSubscription,
  formatExtensionAttempt,
  formatExtensionTitle,
  formatKickAction,
  formatSurfaceViewSize,
  formatTerminalViewSize,
} from "./clientDisplay";
import {
  PanelEmpty,
  PanelRow,
  panelButton,
  SectionHeading,
  StatusPill,
} from "./panelKit";

/**
 * Whether this connection can answer a client-catalog watch at all.
 *
 * Read-only connections are excluded, not just stripped of their Kick button:
 * the share forwarder drops the whole client-control family, and the server
 * still advertises FEATURE_CLIENT_CONTROL through it, so a CLIENT_WATCH sent
 * there would never be answered and the section would sit on "Loading
 * clients…" forever.
 */
export function connectionHasClientList(
  connection: {
    status: string;
    supportsClientControl: boolean;
    id: ConnectionId;
  },
  readOnlyConnections: ReadonlySet<ConnectionId>,
): boolean {
  return (
    connection.status === "connected" &&
    connection.supportsClientControl &&
    !readOnlyConnections.has(connection.id)
  );
}

export function ConnectionClients(props: {
  workspace: BlitWorkspace;
  connectionId: ConnectionId;
  sessions: readonly BlitSession[];
  surfaces: readonly BlitSurface[];
  palette: TerminalPalette;
  fontSize: number;
}) {
  const theme = () => themeFor(props.palette);
  const scale = () => uiScale(props.fontSize);
  const [catalog, setCatalog] = createSignal<BlitClientList | null>(null);
  const [error, setError] = createSignal<string | null>(null);
  const [confirming, setConfirming] = createSignal<bigint | null>(null);
  const [kicking, setKicking] = createSignal<bigint | null>(null);
  const [reason, setReason] = createSignal("");

  // The server's cap is UTF-8 bytes; an input maxLength counts UTF-16 units,
  // so 1024 accented characters would pass the widget and be refused on send.
  // Measure what the server measures and block Confirm instead.
  const reasonBytes = () => kickReasonByteLength(reason());
  const reasonTooLong = () => reasonBytes() > KICK_REASON_MAX;

  /** Disarm a pending confirmation, so an armed destructive button cannot sit
   *  waiting for a stray click. Cancel lands here; so does collapsing the row,
   *  which unmounts this state entirely. */
  function disarm() {
    setConfirming(null);
    setReason("");
  }

  // Mounted only while the row is expanded, so the watch is scoped to the
  // component's own lifetime — no id bookkeeping, no stale catalogs.
  const connection = props.workspace.getConnection(props.connectionId);
  if (connection) {
    const stop = connection.subscribeClients(
      (next) => {
        setCatalog(next);
        setError(null);
      },
      (failure) => setError(failure.message),
    );
    onCleanup(stop);
  }

  async function kick(client: BlitClientInfo) {
    if (!connection) return;
    if (confirming() !== client.id) {
      setConfirming(client.id);
      setReason("");
      return;
    }
    setKicking(client.id);
    setError(null);
    try {
      await connection.kickClient(
        client.id,
        reason().trim() || "Kicked from the Blit remotes manager",
      );
      const current = catalog();
      if (current) {
        setCatalog({
          ...current,
          clients: current.clients.filter((entry) => entry.id !== client.id),
        });
      }
      disarm();
    } catch (failure) {
      setError(failure instanceof Error ? failure.message : String(failure));
    } finally {
      setKicking(null);
    }
  }

  function terminalName(ptyId: number): string {
    const session = props.sessions.find(
      (candidate) =>
        candidate.connectionId === props.connectionId &&
        candidate.ptyId === ptyId,
    );
    return session?.title?.trim() || session?.tag.trim() || `Terminal ${ptyId}`;
  }

  function surfaceName(surfaceId: number): string {
    const surface = props.surfaces.find(
      (candidate) =>
        candidate.connectionId === props.connectionId &&
        candidate.surfaceId === surfaceId,
    );
    return (
      surface?.title.trim() || surface?.appId.trim() || `Surface ${surfaceId}`
    );
  }

  const buttonStyle = () => panelButton(theme(), scale());

  const groupLabel = () => ({
    color: theme().dimFg,
    "font-size": `${scale().sm}px`,
  });

  return (
    <div
      style={{
        display: "flex",
        "flex-direction": "column",
        "background-color": theme().panelBg,
      }}
    >
      <Show when={error()}>
        {(message) => (
          <p
            role="alert"
            style={{
              margin: "0",
              padding: `${scale().controlY}px ${scale().controlX}px`,
              color: theme().error,
            }}
          >
            {message()}
          </p>
        )}
      </Show>

      <SectionHeading
        theme={theme()}
        scale={scale()}
        label="Clients"
        count={catalog()?.clients.length}
      />

      <Show
        when={catalog()}
        fallback={
          <Show when={!error()}>
            <PanelEmpty theme={theme()} scale={scale()}>
              Loading clients…
            </PanelEmpty>
          </Show>
        }
      >
        {(list) => (
          <Show
            when={list().clients.length > 0}
            fallback={
              <PanelEmpty theme={theme()} scale={scale()}>
                No clients connected.
              </PanelEmpty>
            }
          >
            {/* Index, not For: every catalog push allocates fresh objects, so a
                reference-keyed For would dispose and rebuild each row once a
                second and drop keyboard focus mid-confirmation. Rows are sorted
                by client id, so position is stable. */}
            <Index each={list().clients}>
              {(client) => (
                <PanelRow theme={theme()} scale={scale()}>
                  <div
                    style={{
                      display: "flex",
                      "align-items": "center",
                      "justify-content": "space-between",
                      gap: `${scale().gap}px`,
                    }}
                  >
                    <span
                      style={{
                        display: "flex",
                        "align-items": "center",
                        gap: `${scale().tightGap}px`,
                      }}
                    >
                      {/* An extension is named by its definition rather than
                          by its connection id: the id says nothing, and this
                          is the one row a viewer did not open themselves. */}
                      <strong
                        style={{ "font-variant-numeric": "tabular-nums" }}
                      >
                        {formatClientLabel(client())}
                      </strong>
                      <Show when={formatClientOriginTag(client())}>
                        {(label) => (
                          <StatusPill
                            theme={theme()}
                            scale={scale()}
                            tone="idle"
                            label={label()}
                          />
                        )}
                      </Show>
                      {/* The one row the viewer must not mistake for someone
                          else's — it is the only one whose Kick is absent. */}
                      <Show when={client().id === list().selfId}>
                        <StatusPill
                          theme={theme()}
                          scale={scale()}
                          tone="ok"
                          label="this client"
                        />
                      </Show>
                    </span>
                    {/* Arrows are from the listed client's point of view, the
                        same convention as the status bar's own transport row,
                        so a CLI's ↑ is what that CLI is sending. Both figures
                        are the server's measurement of the socket — a client
                        of any kind reports nothing about itself. */}
                    <span
                      title={[
                        formatExtensionTitle(client()),
                        "↓ server → client · ↑ client → server",
                      ]
                        .filter(Boolean)
                        .join("\n")}
                      style={{
                        color: theme().dimFg,
                        "font-size": `${scale().sm}px`,
                        "font-variant-numeric": "tabular-nums",
                      }}
                    >
                      {/* Attempt and age together are what says "crash loop":
                          a climbing attempt number on a row whose age keeps
                          resetting. Either figure alone reads as a new
                          connection. */}
                      <Show when={formatExtensionAttempt(client())}>
                        {(attempt) => <>{attempt()} · </>}
                      </Show>
                      Age {formatClientAge(client().ageSeconds)} · ↓{" "}
                      {formatClientBandwidth(client().outboundBytesPerSecond)} ·
                      ↑ {formatClientBandwidth(client().inboundBytesPerSecond)}
                    </span>
                    <Show when={client().id !== list().selfId}>
                      <span
                        style={{
                          display: "flex",
                          "align-items": "center",
                          gap: `${scale().gap}px`,
                        }}
                      >
                        <button
                          type="button"
                          style={{
                            ...buttonStyle(),
                            color:
                              confirming() === client().id
                                ? theme().error
                                : "inherit",
                          }}
                          disabled={
                            kicking() === client().id ||
                            (confirming() === client().id && reasonTooLong())
                          }
                          onClick={() => void kick(client())}
                        >
                          {kicking() === client().id
                            ? formatKickAction(client()).busy
                            : confirming() === client().id
                              ? formatKickAction(client()).confirm
                              : formatKickAction(client()).idle}
                        </button>
                        <Show when={confirming() === client().id}>
                          <button
                            type="button"
                            style={buttonStyle()}
                            disabled={kicking() === client().id}
                            onClick={disarm}
                          >
                            Cancel
                          </button>
                        </Show>
                      </span>
                    </Show>
                  </div>

                  {/* The reason reaches the kicked peer, so it is worth asking
                      for — but only once the action is armed, to keep the row
                      quiet. Escape is not handled here: the global shortcut
                      handler takes it on the capture phase and closes the
                      overlay, which disarms anyway. Cancel is the in-place
                      affordance. */}
                  <Show when={confirming() === client().id}>
                    <input
                      type="text"
                      value={reason()}
                      // A coarse paste guard only — maxLength counts UTF-16
                      // units, so reasonTooLong() is what actually enforces
                      // the cap.
                      maxLength={KICK_REASON_MAX}
                      placeholder="Reason (optional), shown to the kicked client"
                      aria-label="Kick reason"
                      // autofocus is only honoured at parse time, and this
                      // input is inserted long after the overlay mounts.
                      ref={(element: HTMLInputElement) =>
                        queueMicrotask(() => element.focus())
                      }
                      disabled={kicking() === client().id}
                      onInput={(event) => setReason(event.currentTarget.value)}
                      onKeyDown={(event) => {
                        if (event.key === "Enter" && !reasonTooLong()) {
                          void kick(client());
                        }
                      }}
                      style={{
                        ...ui.input,
                        "background-color": theme().inputBg,
                        color: reasonTooLong() ? theme().error : "inherit",
                        border: `1px solid ${
                          reasonTooLong() ? theme().error : theme().border
                        }`,
                        "font-size": `${scale().sm}px`,
                        padding: `${scale().controlY}px ${scale().controlX}px`,
                        width: "100%",
                      }}
                    />
                    <Show when={reasonTooLong()}>
                      <div
                        role="alert"
                        style={{
                          color: theme().error,
                          "font-size": `${scale().sm}px`,
                        }}
                      >
                        Reason is {reasonBytes()} bytes; the server accepts{" "}
                        {KICK_REASON_MAX}.
                      </div>
                    </Show>
                  </Show>

                  <div>
                    <div style={groupLabel()}>
                      Other subscriptions ({client().subscriptions.length})
                    </div>
                    <Show
                      when={client().subscriptions.length > 0}
                      fallback={
                        <div style={{ color: theme().dimFg }}>None</div>
                      }
                    >
                      <Index each={client().subscriptions}>
                        {(subscription) => (
                          <div>
                            {formatClientSubscription(
                              subscription().kind,
                              subscription().id,
                            )}
                          </div>
                        )}
                      </Index>
                    </Show>
                  </div>

                  <div>
                    <div style={groupLabel()}>
                      Terminals ({client().terminals.length})
                    </div>
                    <Show
                      when={client().terminals.length > 0}
                      fallback={
                        <div style={{ color: theme().dimFg }}>None</div>
                      }
                    >
                      <Index each={client().terminals}>
                        {(terminal) => (
                          <div
                            style={{
                              display: "flex",
                              "justify-content": "space-between",
                              gap: `${scale().gap}px`,
                            }}
                          >
                            <span>
                              {terminalName(terminal().ptyId)}
                              <span style={{ color: theme().dimFg }}>
                                {` (#${terminal().ptyId})`}
                              </span>
                            </span>
                            <code>
                              {formatTerminalViewSize(
                                terminal().cols,
                                terminal().rows,
                              )}
                            </code>
                          </div>
                        )}
                      </Index>
                    </Show>
                  </div>

                  <div>
                    <div style={groupLabel()}>
                      Surfaces ({client().surfaces.length})
                    </div>
                    <Show
                      when={client().surfaces.length > 0}
                      fallback={
                        <div style={{ color: theme().dimFg }}>None</div>
                      }
                    >
                      <Index each={client().surfaces}>
                        {(surface) => (
                          <div
                            style={{
                              display: "flex",
                              "justify-content": "space-between",
                              gap: `${scale().gap}px`,
                            }}
                          >
                            <span>
                              {surfaceName(surface().surfaceId)}
                              <span style={{ color: theme().dimFg }}>
                                {` (#${surface().surfaceId})`}
                              </span>
                            </span>
                            <code>
                              {formatSurfaceViewSize(
                                surface().width,
                                surface().height,
                                surface().scale120,
                              )}
                            </code>
                          </div>
                        )}
                      </Index>
                    </Show>
                  </div>
                </PanelRow>
              )}
            </Index>
          </Show>
        )}
      </Show>
    </div>
  );
}
