import {
  createEffect,
  createMemo,
  createSignal,
  Index,
  on,
  onCleanup,
  Show,
} from "solid-js";
import type {
  BlitClientInfo,
  BlitClientList,
  BlitConnectionSnapshot,
  BlitSession,
  BlitSurface,
  BlitWorkspace,
  ConnectionId,
  TerminalPalette,
} from "@blit-sh/core";
import { KICK_REASON_MAX, kickReasonByteLength } from "@blit-sh/core";
import { OverlayBackdrop, OverlayHeader, OverlayPanel } from "./Overlay";
import { themeFor, ui, uiScale } from "./theme";
import {
  formatClientAge,
  formatClientBandwidth,
  formatClientSubscription,
  formatSurfaceViewSize,
  formatTerminalViewSize,
} from "./clientDisplay";

type CatalogState = {
  loading: boolean;
  catalog: BlitClientList | null;
  error: string | null;
};

const EMPTY_STATE: CatalogState = {
  loading: false,
  catalog: null,
  error: null,
};

export function ClientsOverlay(props: {
  workspace: BlitWorkspace;
  connections: readonly BlitConnectionSnapshot[];
  sessions: readonly BlitSession[];
  surfaces: readonly BlitSurface[];
  connectionLabels: ReadonlyMap<ConnectionId, string>;
  readOnlyConnections: ReadonlySet<ConnectionId>;
  palette: TerminalPalette;
  fontSize: number;
  onClose: () => void;
}) {
  const theme = () => themeFor(props.palette);
  const scale = () => uiScale(props.fontSize);
  const [states, setStates] = createSignal<Record<string, CatalogState>>({});
  const [confirming, setConfirming] = createSignal<string | null>(null);
  const [kicking, setKicking] = createSignal<string | null>(null);
  const [reason, setReason] = createSignal("");

  // The server's cap is UTF-8 bytes; an input maxLength counts UTF-16 units,
  // so 1024 accented characters would pass the widget and be refused on send.
  // Measure what the server measures and block Confirm instead.
  const reasonBytes = () => kickReasonByteLength(reason());
  const reasonTooLong = () => reasonBytes() > KICK_REASON_MAX;

  /** Disarm a pending confirmation, so an armed destructive button cannot sit
   *  waiting for a stray click. Cancel lands here; so does closing the
   *  overlay, which unmounts this state entirely. */
  function disarm() {
    setConfirming(null);
    setReason("");
  }

  // Read-only connections are excluded, not just stripped of their Kick
  // button: the share forwarder drops the whole client-control family, and
  // the server still advertises FEATURE_CLIENT_CONTROL through it, so a
  // CLIENT_WATCH sent here would never be answered and the section would sit
  // on "Loading clients…" forever.
  const supportedConnections = () =>
    props.connections.filter(
      (connection) =>
        connection.status === "connected" &&
        connection.supportsClientControl &&
        !props.readOnlyConnections.has(connection.id),
    );

  const stateFor = (connectionId: string): CatalogState =>
    states()[connectionId] ?? EMPTY_STATE;

  function updateState(connectionId: string, update: Partial<CatalogState>) {
    setStates((previous) => ({
      ...previous,
      [connectionId]: { ...(previous[connectionId] ?? EMPTY_STATE), ...update },
    }));
  }

  async function kick(connectionId: string, client: BlitClientInfo) {
    const connection = props.workspace.getConnection(connectionId);
    if (!connection) return;
    const key = `${connectionId}:${client.id}`;
    if (confirming() !== key) {
      setConfirming(key);
      setReason("");
      return;
    }
    setKicking(key);
    updateState(connectionId, { error: null });
    try {
      await connection.kickClient(
        client.id,
        reason().trim() || "Kicked from the Blit client manager",
      );
      const state = stateFor(connectionId);
      if (state.catalog) {
        updateState(connectionId, {
          catalog: {
            ...state.catalog,
            clients: state.catalog.clients.filter(
              (entry) => entry.id !== client.id,
            ),
          },
        });
      }
      disarm();
    } catch (error) {
      updateState(connectionId, {
        error: error instanceof Error ? error.message : String(error),
      });
    } finally {
      setKicking(null);
    }
  }

  function terminalName(connectionId: string, ptyId: number): string {
    const session = props.sessions.find(
      (candidate) =>
        candidate.connectionId === connectionId && candidate.ptyId === ptyId,
    );
    return session?.title?.trim() || session?.tag.trim() || `Terminal ${ptyId}`;
  }

  function surfaceName(connectionId: string, surfaceId: number): string {
    const surface = props.surfaces.find(
      (candidate) =>
        candidate.connectionId === connectionId &&
        candidate.surfaceId === surfaceId,
    );
    return (
      surface?.title.trim() || surface?.appId.trim() || `Surface ${surfaceId}`
    );
  }

  const buttonStyle = () => ({
    ...ui.btn,
    color: "inherit",
    "background-color": "transparent",
    border: `1px solid ${theme().border}`,
    "border-radius": "0",
    "font-size": `${scale().sm}px`,
    padding: `${scale().controlY}px ${scale().controlX}px`,
    cursor: "pointer",
  });

  // Keyed on the id list, not on `props.connections`: those snapshots change
  // identity on unrelated events, and re-running this effect tears down every
  // live watch and re-establishes it (UNWATCH, WATCH, full snapshot) for
  // nothing. Only a connection appearing or disappearing should resubscribe.
  //
  // Returns the previous array when the ids match, so `on` sees no change.
  // Joining into a delimited string instead would assume ids never contain
  // the delimiter, and a connection id is a user-typed remote name.
  const supportedIds = createMemo<readonly string[]>((previous) => {
    const ids = supportedConnections().map((connection) => connection.id);
    return previous.length === ids.length &&
      previous.every((id, index) => id === ids[index])
      ? previous
      : ids;
  }, []);

  createEffect(
    on(supportedIds, (live) => {
      const stops: Array<() => void> = [];
      for (const connectionId of live) {
        const connection = props.workspace.getConnection(connectionId);
        if (!connection) continue;
        updateState(connectionId, { loading: true, error: null });
        stops.push(
          connection.subscribeClients(
            (catalog) =>
              updateState(connectionId, {
                loading: false,
                catalog,
                error: null,
              }),
            (error) =>
              updateState(connectionId, {
                loading: false,
                error: error.message,
              }),
          ),
        );
      }
      // Drop state for connections that went away, so a server that comes and
      // goes does not leave its last catalog and error behind forever.
      setStates((previous) =>
        Object.fromEntries(
          Object.entries(previous).filter(([id]) => live.includes(id)),
        ),
      );
      onCleanup(() => {
        for (const stop of stops) stop();
      });
    }),
  );

  return (
    <OverlayBackdrop
      palette={props.palette}
      label="Connected clients"
      onClose={props.onClose}
    >
      <OverlayPanel
        palette={props.palette}
        fontSize={props.fontSize}
        style={{
          display: "flex",
          "flex-direction": "column",
          gap: `${scale().gap}px`,
          width: "min(760px, 94vw)",
          "max-height": "var(--overlay-panel-cap)",
        }}
      >
        <OverlayHeader
          palette={props.palette}
          fontSize={props.fontSize}
          title="Connected clients"
          subtitle="Live age, bandwidth, and subscriptions for every client, including this one"
          onClose={props.onClose}
        />

        <Show
          when={supportedConnections().length > 0}
          fallback={
            <p style={{ margin: "0", color: theme().dimFg }}>
              No connected server supports client management.
            </p>
          }
        >
          {/* Index, not For: every catalog push allocates fresh objects, so a
              reference-keyed For would dispose and rebuild each row once a
              second and drop keyboard focus mid-confirmation. Rows are sorted
              by client id, so position is stable. */}
          <Index each={supportedConnections()}>
            {(connection) => {
              const connectionId = () => connection().id;
              const state = () => stateFor(connectionId());
              return (
                <section
                  style={{
                    border: `1px solid ${theme().border}`,
                    display: "flex",
                    "flex-direction": "column",
                  }}
                >
                  <div
                    style={{
                      display: "flex",
                      "align-items": "baseline",
                      gap: `${scale().gap}px`,
                      padding: `${scale().controlY + 2}px ${scale().controlX}px`,
                      "background-color": theme().inputBg,
                    }}
                  >
                    <strong>
                      {props.connectionLabels.get(connectionId()) ??
                        connectionId()}
                    </strong>
                  </div>

                  <Show when={state().error}>
                    {(error) => (
                      <p
                        role="alert"
                        style={{
                          margin: "0",
                          padding: `${scale().controlY}px ${scale().controlX}px`,
                          color: theme().error,
                        }}
                      >
                        {error()}
                      </p>
                    )}
                  </Show>

                  <Show when={state().loading && !state().catalog}>
                    <p
                      style={{ margin: "0", padding: `${scale().controlX}px` }}
                    >
                      Loading clients…
                    </p>
                  </Show>

                  <Show when={state().catalog}>
                    {(catalog) => (
                      <Show
                        when={catalog().clients.length > 0}
                        fallback={
                          <p
                            style={{
                              margin: "0",
                              padding: `${scale().controlX}px`,
                              color: theme().dimFg,
                            }}
                          >
                            No clients connected.
                          </p>
                        }
                      >
                        <Index each={catalog().clients}>
                          {(client) => {
                            const key = () =>
                              `${connectionId()}:${client().id}`;
                            return (
                              <article
                                style={{
                                  padding: `${scale().controlX}px`,
                                  "border-top": `1px solid ${theme().border}`,
                                  display: "grid",
                                  gap: `${scale().gap}px`,
                                }}
                              >
                                <div
                                  style={{
                                    display: "flex",
                                    "align-items": "center",
                                    "justify-content": "space-between",
                                    gap: `${scale().gap}px`,
                                  }}
                                >
                                  <strong
                                    style={{
                                      "font-variant-numeric": "tabular-nums",
                                    }}
                                  >
                                    Client {client().id.toString()}
                                    <Show
                                      when={client().id === catalog().selfId}
                                    >
                                      <> (this client)</>
                                    </Show>
                                  </strong>
                                  <span
                                    style={{
                                      color: theme().dimFg,
                                      "font-size": `${scale().sm}px`,
                                      "font-variant-numeric": "tabular-nums",
                                    }}
                                  >
                                    Age {formatClientAge(client().ageSeconds)} ·
                                    server → client{" "}
                                    {formatClientBandwidth(
                                      client().outboundBytesPerSecond,
                                    )}
                                  </span>
                                  <Show
                                    when={
                                      client().id !== catalog().selfId &&
                                      !props.readOnlyConnections.has(
                                        connectionId(),
                                      )
                                    }
                                  >
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
                                            confirming() === key()
                                              ? theme().error
                                              : "inherit",
                                        }}
                                        disabled={
                                          kicking() === key() ||
                                          (confirming() === key() &&
                                            reasonTooLong())
                                        }
                                        onClick={() =>
                                          void kick(connectionId(), client())
                                        }
                                      >
                                        {kicking() === key()
                                          ? "Kicking…"
                                          : confirming() === key()
                                            ? "Confirm kick"
                                            : "Kick"}
                                      </button>
                                      <Show when={confirming() === key()}>
                                        <button
                                          type="button"
                                          style={buttonStyle()}
                                          disabled={kicking() === key()}
                                          onClick={disarm}
                                        >
                                          Cancel
                                        </button>
                                      </Show>
                                    </span>
                                  </Show>
                                </div>

                                {/* The reason reaches the kicked peer, so it
                                    is worth asking for — but only once the
                                    action is armed, to keep the row quiet.
                                    Escape is not handled here: the global
                                    shortcut handler takes it on the capture
                                    phase and closes the overlay, which
                                    disarms anyway. Cancel is the in-place
                                    affordance. */}
                                <Show when={confirming() === key()}>
                                  <input
                                    type="text"
                                    value={reason()}
                                    // A coarse paste guard only — maxLength
                                    // counts UTF-16 units, so reasonTooLong()
                                    // is what actually enforces the cap.
                                    maxLength={KICK_REASON_MAX}
                                    placeholder="Reason (optional), shown to the kicked client"
                                    aria-label="Kick reason"
                                    // autofocus is only honoured at parse
                                    // time, and this input is inserted long
                                    // after the overlay mounts.
                                    ref={(element: HTMLInputElement) =>
                                      queueMicrotask(() => element.focus())
                                    }
                                    disabled={kicking() === key()}
                                    onInput={(event) =>
                                      setReason(event.currentTarget.value)
                                    }
                                    onKeyDown={(event) => {
                                      if (
                                        event.key === "Enter" &&
                                        !reasonTooLong()
                                      ) {
                                        void kick(connectionId(), client());
                                      }
                                    }}
                                    style={{
                                      ...ui.input,
                                      "background-color": theme().inputBg,
                                      color: reasonTooLong()
                                        ? theme().error
                                        : "inherit",
                                      border: `1px solid ${
                                        reasonTooLong()
                                          ? theme().error
                                          : theme().border
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
                                      Reason is {reasonBytes()} bytes; the
                                      server accepts {KICK_REASON_MAX}.
                                    </div>
                                  </Show>
                                </Show>

                                <div>
                                  <div
                                    style={{
                                      color: theme().dimFg,
                                      "font-size": `${scale().sm}px`,
                                    }}
                                  >
                                    Other subscriptions (
                                    {client().subscriptions.length})
                                  </div>
                                  <Show
                                    when={client().subscriptions.length > 0}
                                    fallback={
                                      <div style={{ color: theme().dimFg }}>
                                        None
                                      </div>
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
                                  <div
                                    style={{
                                      color: theme().dimFg,
                                      "font-size": `${scale().sm}px`,
                                    }}
                                  >
                                    Terminals ({client().terminals.length})
                                  </div>
                                  <Show
                                    when={client().terminals.length > 0}
                                    fallback={
                                      <div style={{ color: theme().dimFg }}>
                                        None
                                      </div>
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
                                            {terminalName(
                                              connectionId(),
                                              terminal().ptyId,
                                            )}
                                            <span
                                              style={{ color: theme().dimFg }}
                                            >
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
                                  <div
                                    style={{
                                      color: theme().dimFg,
                                      "font-size": `${scale().sm}px`,
                                    }}
                                  >
                                    Surfaces ({client().surfaces.length})
                                  </div>
                                  <Show
                                    when={client().surfaces.length > 0}
                                    fallback={
                                      <div style={{ color: theme().dimFg }}>
                                        None
                                      </div>
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
                                            {surfaceName(
                                              connectionId(),
                                              surface().surfaceId,
                                            )}
                                            <span
                                              style={{ color: theme().dimFg }}
                                            >
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
                              </article>
                            );
                          }}
                        </Index>
                      </Show>
                    )}
                  </Show>
                </section>
              );
            }}
          </Index>
        </Show>
      </OverlayPanel>
    </OverlayBackdrop>
  );
}
